//! [4] Explore: ad-hoc PromQL/LogQL queries, log tailing, query history.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::{Deserialize, Serialize};

use super::chartnav::{ChartCursor, ChartOutcome};
use super::keymap::Act;
use super::msg::{LogsJump, Msg};
use super::popup::{Popup, SelectItem, SelectPurpose};
use super::query::adhoc_query;
use super::widgets::{InputResult, ListNav, TextInput};
use super::{App, Focus, Side};
use crate::grafana::dashboard::Panel;
use crate::grafana::dataframe::{LogLine, logs_from_frames, series_from_frames};
use crate::grafana::models::Datasource;
use crate::render::chart::Overlay;
use crate::render::panels::{PanelCtx, PanelData, frames_table, render_logs, render_rows, render_timeseries};
use crate::timerange::{Millis, TimeRange, now_ms};

const MAX_LOG_LINES: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEntry {
    pub ds_uid: String,
    pub ds_name: String,
    pub query: String,
    pub at: i64,
}

pub struct ExploreState {
    pub ds_uid: Option<String>,
    pub input: TextInput,
    pub editing: bool,
    pub result: Option<PanelData>,
    pub loading: bool,
    pub generation: u64,
    pub table: bool,
    pub follow: bool,
    pub last_follow: Instant,
    pub log_filter: String,
    pub log_filter_input: Option<TextInput>,
    pub wrap: bool,
    pub log_nav: ListNav,
    pub pretty_json: bool,
    pub history: Vec<HistoryEntry>,
    pub hist_nav: ListNav,
    hist_path: Option<PathBuf>,
    pub cursor: ChartCursor,
    pub table_scroll: usize,
    pub last_run: Option<(String, String)>,
}

impl ExploreState {
    pub fn new(state_dir: Option<&Path>) -> Self {
        let hist_path = state_dir.map(|d| d.join("history.json"));
        let history = hist_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            ds_uid: None,
            input: TextInput::default(),
            editing: false,
            result: None,
            loading: false,
            generation: 0,
            table: false,
            follow: false,
            last_follow: Instant::now(),
            log_filter: String::new(),
            log_filter_input: None,
            wrap: false,
            log_nav: ListNav::default(),
            pretty_json: true,
            history,
            hist_nav: ListNav::default(),
            hist_path,
            cursor: ChartCursor::default(),
            table_scroll: 0,
            last_run: None,
        }
    }

    pub fn reset_results(&mut self) {
        self.result = None;
        self.loading = false;
        self.follow = false;
        self.cursor.clear();
        self.ds_uid = None;
        self.generation += 1;
    }

    /// Picks a datasource if none is selected (or the selected one vanished).
    pub fn ensure_datasource(&mut self, list: &[Datasource]) {
        if self.ds_uid.as_ref().is_some_and(|u| list.iter().any(|d| &d.uid == u)) {
            return;
        }
        let queryable = |d: &&Datasource| matches!(d.kind.as_str(), "prometheus" | "loki");
        self.ds_uid = list
            .iter()
            .filter(queryable)
            .find(|d| d.is_default)
            .or_else(|| list.iter().find(queryable))
            .or_else(|| list.first())
            .map(|d| d.uid.clone());
    }

    fn push_history(&mut self, ds: &Datasource, query: &str) {
        self.history.retain(|h| !(h.query == query && h.ds_uid == ds.uid));
        self.history.insert(0, HistoryEntry { ds_uid: ds.uid.clone(), ds_name: ds.name.clone(), query: query.to_string(), at: now_ms() });
        self.history.truncate(200);
        if let Some(p) = &self.hist_path {
            let _ = std::fs::create_dir_all(p.parent().unwrap_or(Path::new(".")));
            let _ = std::fs::write(p, serde_json::to_string(&self.history).unwrap_or_default());
        }
    }

    pub fn is_logs(&self) -> bool {
        self.result.as_ref().is_some_and(|r| !r.logs.is_empty() || r.frames.iter().any(|f| f.is_logs()))
    }

    pub fn filtered_logs(&self) -> Vec<LogLine> {
        let Some(r) = &self.result else { return Vec::new() };
        if self.log_filter.is_empty() {
            return r.logs.clone();
        }
        let needle = self.log_filter.to_lowercase();
        r.logs
            .iter()
            .filter(|l| l.line.to_lowercase().contains(&needle) || l.labels.values().any(|v| v.to_lowercase().contains(&needle)))
            .cloned()
            .collect()
    }
}

impl App {
    fn explore_ds(&self) -> Option<Datasource> {
        self.explore.ds_uid.as_ref().and_then(|u| self.ds_list.iter().find(|d| &d.uid == u)).cloned()
    }

    fn explore_points(&self) -> u32 {
        let w = self.hit.main.width.max(60).saturating_sub(10) as u32;
        let per = if self.gfx.mode.is_image() { self.gfx.cell.0 as u32 } else { 2 };
        (w * per).clamp(100, 2000)
    }

    pub fn run_explore(&mut self) {
        let Some(ds) = self.explore_ds() else {
            self.toast_err("no datasource selected (press D)");
            return;
        };
        let expr = self.explore.input.text.trim().to_string();
        if expr.is_empty() {
            self.toast_err("empty query (press i to edit)");
            return;
        }
        self.explore.push_history(&ds, &expr);
        self.explore.last_run = Some((ds.uid.clone(), expr.clone()));
        self.explore.cursor.clear();
        self.explore.log_nav = ListNav::default();
        self.explore.table_scroll = 0;
        self.spawn_explore(&ds, &expr, self.range(), false);
    }

    fn spawn_explore(&mut self, ds: &Datasource, expr: &str, range: (Millis, Millis), append: bool) {
        let (q, step) = adhoc_query(ds, expr, range, self.explore_points(), 1000);
        self.explore.generation += 1;
        self.explore.loading = true;
        let generation = self.explore.generation;
        self.spawn_query(move |c| async move {
            let req = crate::grafana::QueryRequest { from: range.0, to: range.1, queries: vec![q] };
            Msg::Explore { generation, range, step, append, result: c.query(&req).await }
        });
    }

    /// Re-runs the last query after a time change.
    pub fn rerun_explore(&mut self) {
        let Some((uid, expr)) = self.explore.last_run.clone() else { return };
        let Some(ds) = self.ds_list.iter().find(|d| d.uid == uid).cloned() else { return };
        self.spawn_explore(&ds, &expr, self.range(), false);
    }

    /// Follow mode: poll for new log lines (or refresh metrics).
    pub fn explore_tick(&mut self) -> bool {
        if !self.explore.follow || self.explore.loading {
            return false;
        }
        let logs = self.explore.is_logs();
        let every = if logs { Duration::from_secs(2) } else { Duration::from_secs(10) };
        if self.explore.last_follow.elapsed() < every {
            return false;
        }
        self.explore.last_follow = Instant::now();
        let Some((uid, expr)) = self.explore.last_run.clone() else { return false };
        let Some(ds) = self.ds_list.iter().find(|d| d.uid == uid).cloned() else { return false };
        if logs {
            let newest = self.explore.result.as_ref().and_then(|r| r.logs.iter().map(|l| l.time).max()).unwrap_or(now_ms() - 60_000);
            self.spawn_explore(&ds, &expr, (newest, now_ms()), true);
        } else {
            self.spawn_explore(&ds, &expr, self.range(), false);
        }
        true
    }

    pub fn on_explore_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Explore { generation, range, step, append, result } => {
                if generation != self.explore.generation {
                    return;
                }
                self.explore.loading = false;
                match result {
                    Ok(resp) => {
                        let frames = resp.frames();
                        let mut logs = logs_from_frames(&frames);
                        logs.sort_by_key(|l| std::cmp::Reverse(l.time));
                        if append && let Some(prev) = &mut self.explore.result {
                            let seen: HashSet<String> = prev.logs.iter().map(LogLine::key).collect();
                            let new: Vec<LogLine> = logs.into_iter().filter(|l| !seen.contains(&l.key())).collect();
                            if !new.is_empty() {
                                // Keep the cursor on the same line while new lines arrive on top.
                                if self.explore.log_nav.selected > 0 {
                                    self.explore.log_nav.selected += new.len();
                                }
                                let mut merged = new;
                                merged.append(&mut prev.logs);
                                merged.truncate(MAX_LOG_LINES);
                                prev.logs = merged;
                                prev.range.1 = range.1;
                            }
                            return;
                        }
                        self.explore.result = Some(PanelData {
                            series: series_from_frames(&frames),
                            logs,
                            frames,
                            errors: resp.errors(),
                            loading: false,
                            range,
                            step: Some(step),
                        });
                    }
                    Err(e) => {
                        if append {
                            self.toast_err(format!("tail: {e}"));
                        } else {
                            self.explore.result = Some(PanelData { errors: vec![e.to_string()], range, ..Default::default() });
                        }
                    }
                }
            }
            Msg::LokiLabels { result, then } => self.finish_logs_jump(result.unwrap_or_default(), then),
            _ => {}
        }
    }

    fn finish_logs_jump(&mut self, loki_labels: Vec<String>, jump: LogsJump) {
        let known: BTreeSet<&str> = loki_labels.iter().map(String::as_str).collect();
        let matchers: Vec<String> = jump
            .labels
            .iter()
            .filter(|(k, _)| known.contains(k.as_str()))
            .map(|(k, v)| format!("{k}=\"{}\"", v.replace('"', "\\\"")))
            .collect();
        let selector = if matchers.is_empty() {
            let label = ["service_name", "service", "job", "app", "container", "namespace"]
                .iter()
                .find(|l| known.contains(**l))
                .copied()
                .or(loki_labels.first().map(String::as_str))
                .unwrap_or("job");
            format!("{{{label}=~\".+\"}}")
        } else {
            format!("{{{}}}", matchers.join(", "))
        };
        self.explore.ds_uid = Some(jump.ds_uid);
        self.explore.input = TextInput::new(selector);
        self.side = Side::Explore;
        self.focus = Focus::Main;
        self.explore.editing = false;
        self.time.push(TimeRange::absolute(jump.range.0, jump.range.1));
        if let Some(d) = &mut self.dash {
            d.cursor.clear();
        }
        self.run_explore();
        self.toast(if matchers.is_empty() { "no matching Loki labels; showing all streams" } else { "logs for selection" });
    }

    // ---- keys ---------------------------------------------------------------------------

    pub fn explore_raw_key(&mut self, k: &KeyEvent) {
        if let Some(input) = &mut self.explore.log_filter_input {
            match input.handle(k) {
                InputResult::Submit => self.explore.log_filter_input = None,
                InputResult::Cancel => {
                    self.explore.log_filter_input = None;
                    self.explore.log_filter.clear();
                }
                InputResult::Changed => {
                    self.explore.log_filter = input.text.clone();
                    self.explore.log_nav = ListNav::default();
                }
                InputResult::Unchanged => {}
            }
            return;
        }
        if k.code == KeyCode::Char('r') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.history_popup();
            return;
        }
        match self.explore.input.handle(k) {
            InputResult::Submit => {
                self.explore.editing = false;
                self.run_explore();
            }
            InputResult::Cancel => self.explore.editing = false,
            _ => {}
        }
    }

    fn history_popup(&mut self) {
        let items: Vec<SelectItem> = self
            .explore
            .history
            .iter()
            .enumerate()
            .map(|(i, h)| SelectItem { label: h.query.clone(), value: i.to_string(), hint: h.ds_name.clone() })
            .collect();
        if items.is_empty() {
            self.toast("no query history yet");
            return;
        }
        self.popup = Some(Popup::select("Query history", items, false, BTreeSet::new(), 0, SelectPurpose::History));
    }

    pub fn load_history_entry(&mut self, idx: usize) {
        let Some(h) = self.explore.history.get(idx).cloned() else { return };
        if self.ds_list.iter().any(|d| d.uid == h.ds_uid) {
            self.explore.ds_uid = Some(h.ds_uid);
        }
        self.explore.input = TextInput::new(h.query);
        self.side = Side::Explore;
        self.focus = Focus::Main;
        self.run_explore();
    }

    pub fn explore_side_key(&mut self, acts: &[Act]) -> bool {
        if self.explore.hist_nav.handle(acts, self.explore.history.len(), self.side_list_height()) {
            return true;
        }
        for a in acts {
            match a {
                Act::Enter => {
                    if self.explore.history.is_empty() {
                        self.focus = Focus::Main;
                        self.explore.editing = true;
                    } else {
                        self.load_history_entry(self.explore.hist_nav.selected);
                    }
                }
                Act::Insert => {
                    self.focus = Focus::Main;
                    self.explore.editing = true;
                }
                Act::History => self.history_popup(),
                Act::PickDatasource => self.datasource_popup(),
                _ => continue,
            }
            return true;
        }
        false
    }

    fn datasource_popup(&mut self) {
        let items: Vec<SelectItem> =
            self.ds_list.iter().map(|d| SelectItem { label: d.name.clone(), value: d.uid.clone(), hint: d.kind.clone() }).collect();
        let sel = self.explore.ds_uid.as_ref().and_then(|u| self.ds_list.iter().position(|d| &d.uid == u)).unwrap_or(0);
        self.popup = Some(Popup::select("Datasource", items, false, BTreeSet::new(), sel, SelectPurpose::Datasource));
    }

    pub fn explore_main_key(&mut self, acts: &[Act]) -> bool {
        // Chart interaction first (metrics view).
        if !self.explore.is_logs() && !self.explore.table && self.explore.result.as_ref().is_some_and(|r| !r.series.is_empty()) {
            let range = self.range();
            let ts = self.hit.explore_chart.as_ref().map(|(_, s)| s.timestamps()).unwrap_or_default();
            match self.explore.cursor.handle(acts, &ts, range) {
                ChartOutcome::Moved => return true,
                ChartOutcome::Zoom(a, b) => {
                    self.set_time(TimeRange::absolute(a, b));
                    return true;
                }
                ChartOutcome::ZoomAround(c, f) => {
                    let r = self.time.current().zoom(now_ms(), c, f);
                    self.set_time(r);
                    return true;
                }
                ChartOutcome::Exit => {
                    self.focus = Focus::Side;
                    return true;
                }
                ChartOutcome::Ignored => {}
            }
        }
        let logs = self.explore.is_logs();
        if logs {
            let n = self.explore.filtered_logs().len();
            if self.explore.log_nav.handle(acts, n, self.hit.main.height.saturating_sub(4) as usize) {
                return true;
            }
        } else if self.explore.table && acts.iter().any(|a| matches!(a, Act::Down | Act::Up | Act::HalfDown | Act::HalfUp | Act::Top)) {
            for a in acts {
                match a {
                    Act::Down => self.explore.table_scroll += 1,
                    Act::Up => self.explore.table_scroll = self.explore.table_scroll.saturating_sub(1),
                    Act::HalfDown => self.explore.table_scroll += 10,
                    Act::HalfUp => self.explore.table_scroll = self.explore.table_scroll.saturating_sub(10),
                    Act::Top => self.explore.table_scroll = 0,
                    _ => {}
                }
            }
            return true;
        }
        for a in acts {
            match a {
                Act::Insert => self.explore.editing = true,
                Act::Enter if logs => self.log_detail(),
                Act::Enter => self.explore.editing = true,
                Act::PickDatasource => self.datasource_popup(),
                Act::History => self.history_popup(),
                Act::ToggleTable if !logs => {
                    self.explore.table = !self.explore.table;
                    self.explore.table_scroll = 0;
                }
                Act::Follow => {
                    self.explore.follow = !self.explore.follow;
                    self.explore.last_follow = Instant::now();
                    self.toast(if self.explore.follow { "following" } else { "follow off" });
                }
                Act::Wrap if logs => self.explore.wrap = !self.explore.wrap,
                Act::PrettyJson => {
                    self.explore.pretty_json = !self.explore.pretty_json;
                    self.toast(if self.explore.pretty_json { "JSON pretty-printing on" } else { "JSON pretty-printing off" });
                }
                Act::Filter if logs => self.explore.log_filter_input = Some(TextInput::new(self.explore.log_filter.clone())),
                Act::LogsForSelection => self.logs_for_selection(),
                Act::YankUrl | Act::OpenBrowser => {
                    let Some(url) = self.explore_url() else { return true };
                    if *a == Act::OpenBrowser {
                        self.open_url(&url);
                    } else {
                        self.copy("Explore URL", &url);
                    }
                }
                Act::YankJson if logs => {
                    let logs = self.explore.filtered_logs();
                    if let Some(l) = logs.get(self.explore.log_nav.selected) {
                        let line = l.line.clone();
                        self.copy("log line", &line);
                    }
                }
                Act::Back if !self.explore.log_filter.is_empty() => self.explore.log_filter.clear(),
                _ => continue,
            }
            return true;
        }
        false
    }

    fn explore_url(&self) -> Option<String> {
        let ds = self.explore_ds()?;
        let (from, to) = self.time.current().url_params(now_ms());
        let panes = serde_json::json!({
            "lg": {
                "datasource": ds.uid,
                "queries": [{ "refId": "A", "expr": self.explore.input.text, "datasource": { "uid": ds.uid, "type": ds.kind } }],
                "range": { "from": from, "to": to }
            }
        });
        Some(format!("{}/explore?schemaVersion=1&panes={}", self.base_url(), urlencoding::encode(&panes.to_string())))
    }

    fn log_detail(&mut self) {
        let logs = self.explore.filtered_logs();
        let Some(l) = logs.get(self.explore.log_nav.selected) else { return };
        let mut lines = vec![
            Line::from(vec![
                Span::styled("time   ", Style::default().fg(Color::Gray)),
                Span::raw(crate::timerange::format_absolute(l.time)),
            ]),
            Line::from(vec![Span::styled("level  ", Style::default().fg(Color::Gray)), Span::raw(format!("{:?}", l.level()))]),
            Line::raw(""),
            Line::styled("Labels", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ];
        for (k, v) in &l.labels {
            lines.push(Line::from(vec![Span::styled(format!("  {k:<20}"), Style::default().fg(Color::Gray)), Span::raw(v.clone())]));
        }
        let fields = parse_fields(&l.line);
        if !fields.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled("Parsed fields", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
            for (k, v) in fields {
                lines.push(Line::from(vec![Span::styled(format!("  {k:<20}"), Style::default().fg(Color::Gray)), Span::raw(v)]));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("Line", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
        match serde_json::from_str::<serde_json::Value>(&l.line) {
            Ok(v) if self.explore.pretty_json && v.is_object() => lines.extend(super::dashboards::json_lines(&v)),
            _ => lines.extend(textwrap::wrap(&l.line, 120).into_iter().map(|s| Line::raw(s.into_owned()))),
        }
        self.popup = Some(Popup::text("Log line", lines));
    }

    // ---- rendering ---------------------------------------------------------------------------

    pub fn render_explore_side(&mut self, f: &mut Frame, area: Rect) {
        if self.explore.history.is_empty() {
            f.buffer_mut().set_string(area.x, area.y, "no queries yet — i to write one", Style::default().fg(Color::DarkGray));
            return;
        }
        let height = area.height as usize;
        let len = self.explore.history.len();
        self.explore.hist_nav.clamp(len, height);
        for (row, i) in self.explore.hist_nav.visible(len, height).enumerate() {
            let h = &self.explore.history[i];
            let line = Line::from(vec![
                Span::raw(h.query.clone()),
                Span::styled(format!("  {}", h.ds_name), Style::default().fg(Color::DarkGray)),
            ]);
            self.list_row(f, area, row, i == self.explore.hist_nav.selected, line);
        }
        self.hit.side_lists.retain(|(s, _, _)| *s != Side::Explore);
        self.hit.side_lists.push((Side::Explore, area, self.explore.hist_nav.offset));
    }

    pub fn render_explore_main(&mut self, f: &mut Frame, area: Rect) {
        self.hit.explore_chart = None;
        if area.height < 4 {
            return;
        }
        let ds = self.explore_ds();
        let ds_line = Line::from(vec![
            Span::styled("Datasource ", Style::default().fg(Color::Gray)),
            Span::styled(
                ds.as_ref().map(|d| d.name.clone()).unwrap_or_else(|| "none".into()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(ds.as_ref().map(|d| format!(" ({})", d.kind)).unwrap_or_default(), Style::default().fg(Color::DarkGray)),
            Span::styled("   D change · i edit · C-r history", Style::default().fg(Color::DarkGray)),
        ]);
        f.buffer_mut().set_line(area.x, area.y, &ds_line, area.width);
        let input_area = Rect { y: area.y + 1, height: 1, ..area };
        let style = if self.explore.editing { Style::default().fg(Color::White) } else { Style::default().fg(Color::Yellow) };
        f.buffer_mut().set_style(input_area, Style::default().bg(Color::Rgb(0x1c, 0x1e, 0x24)));
        self.explore.input.render(input_area, f.buffer_mut(), "› ", style, self.explore.editing && self.focus == Focus::Main);
        if self.explore.input.text.is_empty() && !self.explore.editing {
            f.buffer_mut().set_string(
                input_area.x + 2,
                input_area.y,
                "press i to write a PromQL / LogQL query",
                Style::default().fg(Color::DarkGray),
            );
        }
        let logs = self.explore.is_logs();
        let status = match &self.explore.result {
            _ if self.explore.loading && !self.explore.follow => Span::styled("running…", Style::default().fg(Color::DarkGray)),
            None => Span::raw(""),
            Some(r) if !r.errors.is_empty() => Span::styled(format!("⚠ {}", r.errors.join("; ")), Style::default().fg(Color::Red)),
            Some(r) if logs => Span::styled(
                format!(
                    "{} lines{}{}{}",
                    r.logs.len(),
                    if self.explore.log_filter.is_empty() { String::new() } else { format!(" · filter /{}", self.explore.log_filter) },
                    if self.explore.follow { " · following ●" } else { "" },
                    if self.explore.wrap { " · wrap" } else { "" }
                ),
                Style::default().fg(Color::Gray),
            ),
            Some(r) => Span::styled(
                format!(
                    "{} series · step {}{}",
                    r.series.len(),
                    crate::timerange::format_duration(r.step.unwrap_or(0)),
                    if self.explore.follow { " · auto-refresh ●" } else { "" }
                ),
                Style::default().fg(Color::Gray),
            ),
        };
        let status_area = Rect { y: area.y + 2, height: 1, ..area };
        match &self.explore.log_filter_input {
            Some(i) => i.render(status_area, f.buffer_mut(), "filter /", Style::default().fg(Color::Yellow), true),
            None => {
                f.buffer_mut().set_line(area.x, area.y + 2, &Line::from(status), area.width);
            }
        }
        let body = Rect { y: area.y + 3, height: area.height - 3, ..area };
        let Some(result) = self.explore.result.clone() else {
            let help = vec![
                Line::raw(""),
                Line::styled("Examples", Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)),
                Line::styled("  rate(prometheus_http_requests_total[$__rate_interval])", Style::default().fg(Color::DarkGray)),
                Line::styled("  {service=~\".+\"} |= \"error\"", Style::default().fg(Color::DarkGray)),
                Line::raw(""),
                Line::styled("Charts: h/l crosshair · v select · Enter zoom · gl logs for selection", Style::default().fg(Color::DarkGray)),
                Line::styled("Logs: f follow · / filter · w wrap · Enter details · J JSON", Style::default().fg(Color::DarkGray)),
            ];
            f.render_widget(ratatui::widgets::Paragraph::new(help), body);
            return;
        };
        if logs {
            let lines = self.explore.filtered_logs();
            let height = body.height as usize;
            self.explore.log_nav.clamp(lines.len(), height);
            let start = self.explore.log_nav.offset;
            let visible: Vec<LogLine> = lines.iter().skip(start).take(height).cloned().collect();
            let sel = self.explore.log_nav.selected.checked_sub(start);
            render_logs(&visible, body, f.buffer_mut(), self.explore.wrap, if self.focus == Focus::Main { sel } else { None });
            return;
        }
        if self.explore.table {
            let (cols, rows) = frames_table(&result.frames);
            render_rows((cols, rows), body, f.buffer_mut(), self.explore.table_scroll);
            return;
        }
        if result.series.is_empty() {
            if !result.frames.is_empty() {
                render_rows(frames_table(&result.frames), body, f.buffer_mut(), self.explore.table_scroll);
            } else if result.errors.is_empty() {
                f.buffer_mut().set_string(body.x, body.y, "No data", Style::default().fg(Color::DarkGray));
            }
            return;
        }
        let panel = Panel::synthetic("timeseries");
        let images_ok = self.popup.is_none() || self.gfx.mode.supports_overlap();
        let mut ctx = PanelCtx {
            gfx: &self.gfx,
            cache: &mut self.images,
            marker: self.marker,
            overlay: Overlay {
                crosshair: self.explore.cursor.crosshair,
                selection: self.explore.cursor.selection,
                focused: self.focus == Focus::Main,
            },
            annotations: &[],
            images: images_ok,
            key: format!("explore:{}", body.height),
            wrap_logs: false,
        };
        self.hit.explore_chart = render_timeseries(&panel, &result, body, f.buffer_mut(), &mut ctx);
    }
}

/// `key=value` (logfmt) or top-level JSON fields of a log line.
pub fn parse_fields(line: &str) -> Vec<(String, String)> {
    if let Ok(serde_json::Value::Object(o)) = serde_json::from_str::<serde_json::Value>(line) {
        return o.into_iter().map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect();
    }
    let re = regex::Regex::new(r#"([A-Za-z_][\w.\-]*)=("(?:[^"\\]|\\.)*"|\S+)"#).expect("valid regex");
    re.captures_iter(line).map(|c| (c[1].to_string(), c[2].trim_matches('"').to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        assert_eq!(
            parse_fields(r#"level=info msg="hello world" n=3"#),
            vec![("level".into(), "info".into()), ("msg".into(), "hello world".into()), ("n".into(), "3".into())]
        );
        assert_eq!(parse_fields(r#"{"a":1,"b":"x"}"#), vec![("a".into(), "1".into()), ("b".into(), "x".into())]);
        assert!(parse_fields("plain text").is_empty());
    }

    #[test]
    fn picks_default_datasource() {
        let list: Vec<Datasource> = serde_json::from_str(
            &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/datasources.json")).unwrap(),
        )
        .unwrap();
        let mut s = ExploreState::new(None);
        s.ensure_datasource(&list);
        assert_eq!(s.ds_uid.as_deref(), Some("prometheus"));
    }
}
