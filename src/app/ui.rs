//! Screen layout: side panels, main pane, status bar, popups.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

use super::dashboards::side_width;
use super::keymap::Act;
use super::popup::{InputPurpose, Outcome, Pending, Popup, SelectPurpose};
use super::widgets::{hints_line, truncate};
use super::{App, Focus, Side, parse_refresh};
use crate::timerange::{TimeRange, now_ms};

const SELECTED_BG: Color = Color::Rgb(0x2a, 0x3a, 0x55);
const SELECTED_BG_DIM: Color = Color::Rgb(0x26, 0x29, 0x31);

impl App {
    pub fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        self.size = (area.width, area.height);
        self.hit.side.clear();
        self.hit.side_lists.clear();
        self.hit.links.clear();
        let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        // Narrow terminals: show only the focused column.
        if area.width >= 100 {
            let [side, main] = Layout::horizontal([Constraint::Length(side_width(area.width)), Constraint::Min(20)]).areas(body);
            self.render_side(f, side);
            self.render_main(f, main);
        } else if self.focus == Focus::Side {
            self.render_side(f, body);
        } else {
            self.render_main(f, body);
        }
        self.render_status(f, status);
        if let Some(mut p) = self.popup.take() {
            p.render(f, &self.keymap);
            self.popup = Some(p);
        }
    }

    pub fn side_list_height(&self) -> usize {
        self.hit.side_lists.iter().find(|(s, _, _)| *s == self.side).map(|(_, r, _)| r.height as usize).unwrap_or(10)
    }

    fn side_counts(&self, s: Side) -> usize {
        match s {
            Side::Contexts => self.contexts.names.len(),
            Side::Dashboards => self.dashboards.rows.len(),
            Side::Alerts => self.alerts.rows.len(),
            Side::Explore => self.explore.history.len(),
            Side::Datasources => self.ds_list.len(),
        }
    }

    fn render_side(&mut self, f: &mut Frame, area: Rect) {
        // The selected panel expands; others get a few rows (like lazygit).
        let collapsed = |s: Side| -> u16 { (self.side_counts(s) as u16).clamp(1, 3) + 2 };
        let others: u16 = Side::ALL.iter().filter(|s| **s != self.side).map(|s| collapsed(*s)).sum();
        let constraints: Vec<Constraint> = Side::ALL
            .iter()
            .map(|s| {
                if *s == self.side { Constraint::Min(area.height.saturating_sub(others).max(5)) } else { Constraint::Length(collapsed(*s)) }
            })
            .collect();
        let rects = Layout::vertical(constraints).split(area);
        for (i, side) in Side::ALL.iter().enumerate() {
            let r = rects[i];
            self.hit.side.push((*side, r));
            let active = *side == self.side;
            let focused = active && self.focus == Focus::Side;
            let mut title = vec![Span::styled(
                format!("[{}] {}", i + 1, side.title()),
                Style::default().add_modifier(if active { Modifier::BOLD } else { Modifier::empty() }),
            )];
            match side {
                Side::Alerts => {
                    let n = self.alerts.firing_count();
                    if n > 0 {
                        title.push(Span::styled(format!(" {n}●"), Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD)));
                    }
                }
                Side::Contexts => title.push(Span::styled(format!(" {}", self.ctx.name), Style::default().fg(Color::Green))),
                _ => {}
            }
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if focused {
                    Color::Cyan
                } else if active {
                    Color::Gray
                } else {
                    Color::Rgb(0x44, 0x47, 0x50)
                }))
                .title(Line::from(title));
            let inner = block.inner(r);
            f.render_widget(block, r);
            if inner.height == 0 {
                continue;
            }
            match side {
                Side::Contexts => self.render_contexts_side(f, inner),
                Side::Dashboards => self.render_dash_side(f, inner),
                Side::Alerts => self.render_alerts_side(f, inner),
                Side::Explore => self.render_explore_side(f, inner),
                Side::Datasources => self.render_datasources_side(f, inner),
            }
            if !self.hit.side_lists.iter().any(|(s, _, _)| s == side) {
                let offset = match side {
                    Side::Contexts => self.contexts.nav.offset,
                    Side::Datasources => self.datasources.nav.offset,
                    _ => 0,
                };
                self.hit.side_lists.push((*side, inner, offset));
            }
        }
    }

    /// Registers a clickable URL drawn at (x, y), clipped to `area`.
    pub fn link(&mut self, area: Rect, x: u16, y: u16, url: &str) {
        if y >= area.y + area.height || x >= area.x + area.width {
            return;
        }
        let text = truncate(url, (area.x + area.width - x) as usize);
        if text == url {
            self.hit.links.push((x, y, text, url.to_string()));
        }
    }

    /// Draws one list row with selection highlighting.
    pub fn list_row(&self, f: &mut Frame, area: Rect, row: usize, selected: bool, line: Line) {
        let y = area.y + row as u16;
        if y >= area.y + area.height {
            return;
        }
        let r = Rect { y, height: 1, ..area };
        if selected {
            let bg = if self.focus == Focus::Side { SELECTED_BG } else { SELECTED_BG_DIM };
            f.buffer_mut().set_style(r, Style::default().bg(bg));
        }
        let line = Line::from(line.spans.into_iter().map(|s| Span::styled(s.content.into_owned(), s.style)).collect::<Vec<_>>());
        let w = line.width();
        if w > area.width as usize {
            let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            f.buffer_mut().set_string(
                area.x,
                y,
                truncate(&text, area.width as usize),
                line.spans.first().map(|s| s.style).unwrap_or_default(),
            );
            // Re-apply span styles for the visible part.
            let mut x = area.x;
            for s in &line.spans {
                for _ in s.content.chars() {
                    if x >= area.x + area.width {
                        break;
                    }
                    if let Some(c) = f.buffer_mut().cell_mut((x, y)) {
                        c.set_style(s.style);
                    }
                    x += 1;
                }
            }
        } else {
            f.buffer_mut().set_line(area.x, y, &line, area.width);
        }
        if selected {
            let bg = if self.focus == Focus::Side { SELECTED_BG } else { SELECTED_BG_DIM };
            for x in area.x..area.x + area.width {
                if let Some(c) = f.buffer_mut().cell_mut((x, y)) {
                    c.set_bg(bg);
                }
            }
        }
    }

    fn render_main(&mut self, f: &mut Frame, area: Rect) {
        let title: Vec<Span> = match self.side {
            Side::Contexts => vec![Span::styled(" Context ", Style::default().add_modifier(Modifier::BOLD))],
            Side::Dashboards => self.dash_main_title(),
            Side::Alerts => self.alerts_main_title(),
            Side::Explore => vec![Span::styled(" Explore ", Style::default().add_modifier(Modifier::BOLD))],
            Side::Datasources => vec![Span::styled(" Datasource ", Style::default().add_modifier(Modifier::BOLD))],
        };
        let focused = self.focus == Focus::Main;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { Color::Cyan } else { Color::Gray }))
            .title(Line::from(title));
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.hit.main = inner;
        if self.side != Side::Dashboards {
            self.hit.panels.clear();
        }
        match self.side {
            Side::Contexts => self.render_contexts_main(f, inner),
            Side::Dashboards => self.render_dash_main(f, inner),
            Side::Alerts => self.render_alerts_main(f, inner),
            Side::Explore => self.render_explore_main(f, inner),
            Side::Datasources => self.render_datasources_main(f, inner),
        }
    }

    fn hints(&self) -> Vec<(String, &'static str)> {
        let k = |a: Act| self.keymap.key_for(a);
        let mut h: Vec<(String, &'static str)> = match (self.focus, self.side) {
            (Focus::Side, Side::Contexts) => vec![(k(Act::Enter), "switch"), (k(Act::Test), "check all")],
            (Focus::Side, Side::Dashboards) => vec![
                (k(Act::Enter), "open"),
                (k(Act::Filter), "filter"),
                (k(Act::StateFilter), "starred/recent"),
                (k(Act::Edit), "edit"),
                (k(Act::OpenBrowser), "browser"),
            ],
            (Focus::Side, Side::Alerts) => vec![
                (k(Act::Silence), "silence"),
                (k(Act::Unsilence), "unsilence"),
                (k(Act::Pause), "pause"),
                (k(Act::StateFilter), "state"),
                (k(Act::Mark), "mark"),
            ],
            (Focus::Side, Side::Explore) => vec![(k(Act::Enter), "run"), (k(Act::Insert), "new query"), (k(Act::History), "history")],
            (Focus::Side, Side::Datasources) => vec![(k(Act::Test), "health check"), (k(Act::OpenBrowser), "browser")],
            (Focus::Main, Side::Dashboards) => match self.dash.as_ref().map(|d| d.tab) {
                Some(super::dashboards::DashTab::Panels) => vec![
                    (format!("{}{}", k(Act::Left), k(Act::Right)), "crosshair"),
                    (k(Act::Visual), "select"),
                    (k(Act::Enter), "zoom/max"),
                    (k(Act::LogsForSelection), "logs"),
                    (k(Act::ExactRender), "exact"),
                    (format!("{}{}", k(Act::PrevTab), k(Act::NextTab)), "tabs"),
                ],
                Some(super::dashboards::DashTab::Variables) => vec![(k(Act::Enter), "change")],
                Some(super::dashboards::DashTab::Versions) => {
                    vec![(k(Act::Mark), "mark"), (k(Act::Diff), "diff"), (k(Act::Restore), "restore")]
                }
                _ => vec![(k(Act::Edit), "edit")],
            },
            (Focus::Main, Side::Explore) => {
                if self.explore.editing {
                    vec![("Enter".into(), "run"), ("Esc".into(), "stop editing"), ("C-r".into(), "history")]
                } else if self.explore.is_logs() {
                    vec![
                        (k(Act::Follow), "follow"),
                        (k(Act::Filter), "filter"),
                        (k(Act::Wrap), "wrap"),
                        (k(Act::Enter), "details"),
                        (k(Act::Insert), "edit"),
                    ]
                } else {
                    vec![
                        (k(Act::Insert), "edit"),
                        (k(Act::PickDatasource), "datasource"),
                        (k(Act::ToggleTable), "table"),
                        (k(Act::Visual), "select"),
                        (k(Act::LogsForSelection), "logs"),
                    ]
                }
            }
            (Focus::Main, Side::Alerts) => vec![
                (format!("{}{}", k(Act::PrevTab), k(Act::NextTab)), "tabs"),
                (k(Act::Silence), "silence"),
                (k(Act::Unsilence), "unsilence"),
            ],
            _ => vec![],
        };
        h.push((k(Act::TimePicker), "time"));
        h.push((k(Act::Help), "help"));
        h
    }

    fn render_status(&mut self, f: &mut Frame, area: Rect) {
        let bg = Style::default().bg(Color::Rgb(0x16, 0x17, 0x1d));
        f.buffer_mut().set_style(area, bg);
        // Right side: context · time · refresh · graphics.
        let mut right = vec![Span::styled(
            format!(" {} ", self.ctx.name),
            Style::default().fg(Color::Black).bg(if self.ctx.readonly { Color::Yellow } else { Color::Green }),
        )];
        if self.ctx.readonly {
            right.push(Span::styled(" ro ", Style::default().fg(Color::Yellow)));
        }
        let (pos, total) = self.time.position();
        right.push(Span::styled(format!(" ⏱ {} ", self.time.current()), Style::default().fg(Color::White)));
        if total > 1 {
            right.push(Span::styled(format!("[{pos}/{total}] "), Style::default().fg(Color::DarkGray)));
        }
        if let Some(r) = self.refresh {
            right.push(Span::styled(
                format!("⟳ {} ", crate::timerange::format_duration(r.as_millis() as i64)),
                Style::default().fg(Color::Cyan),
            ));
        }
        right.push(Span::styled(format!(" {} ", self.gfx.mode.name()), Style::default().fg(Color::DarkGray)));
        let right = Line::from(right);
        let rw = right.width() as u16;
        let left_w = area.width.saturating_sub(rw + 1);
        let left: Line = if !self.pending_keys.is_empty() {
            Line::styled(format!(" {}…", self.pending_keys.join("")), Style::default().fg(Color::Yellow))
        } else if let Some(t) = &self.toast {
            Line::styled(
                format!(" {}", truncate(&t.text, left_w as usize - 1)),
                Style::default().fg(if t.error { Color::LightRed } else { Color::LightGreen }),
            )
        } else {
            let mut l = hints_line(&self.hints());
            l.spans.insert(0, Span::raw(" "));
            l
        };
        f.buffer_mut().set_line(area.x, area.y, &left, left_w);
        f.buffer_mut().set_line(area.x + area.width.saturating_sub(rw), area.y, &right, rw);
    }

    // ---- popup outcomes & commands ------------------------------------------------------

    pub fn on_popup_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::None => {}
            Outcome::Close => self.popup = None,
            Outcome::Input { purpose, text } => {
                self.popup = None;
                self.on_input(purpose, text);
            }
            Outcome::Select { purpose, values } => {
                self.popup = None;
                self.on_select(purpose, values);
            }
            Outcome::Confirm(p) => {
                self.popup = None;
                self.on_confirm(p);
            }
            Outcome::Silence { matchers, duration, comment } => {
                self.popup = None;
                self.create_silence(matchers, duration, comment);
            }
        }
    }

    fn on_input(&mut self, purpose: InputPurpose, text: String) {
        match purpose {
            InputPurpose::Command => self.run_command(&text),
            InputPurpose::TimeCustom => match TimeRange::parse_input(&text) {
                Ok(r) => self.set_time(r),
                Err(e) => self.toast_err(e),
            },
            InputPurpose::Annotate { dashboard_uid, panel_id, range } => {
                if text.trim().is_empty() {
                    self.toast_err("empty annotation, nothing added");
                } else {
                    self.create_annotation(dashboard_uid, panel_id, range, text);
                }
            }
            InputPurpose::GroupBy => {
                self.alerts.group_by = Some(text.trim().to_string()).filter(|s| !s.is_empty());
                self.alerts.rebuild();
            }
            InputPurpose::VarText { name } => self.set_variable(&name, vec![text]),
        }
    }

    fn on_select(&mut self, purpose: SelectPurpose, values: Vec<String>) {
        let first = values.first().cloned().unwrap_or_default();
        match purpose {
            SelectPurpose::Context => {
                if first != self.ctx.name {
                    self.switch_context(&first);
                }
            }
            SelectPurpose::Time => {
                if first == "__custom" {
                    let cur = self.time.current().to_string().replace(" → ", " to ");
                    self.popup = Some(Popup::input("Time range (e.g. now-2d to now-1d)", "› ", &cur, InputPurpose::TimeCustom));
                } else if let Ok(r) = TimeRange::parse_input(&first) {
                    self.set_time(r);
                }
            }
            SelectPurpose::Refresh => {
                self.refresh = parse_refresh(&first);
                self.last_refresh = std::time::Instant::now();
                self.toast(match self.refresh {
                    Some(d) => format!("auto refresh every {}", crate::timerange::format_duration(d.as_millis() as i64)),
                    None => "auto refresh off".into(),
                });
            }
            SelectPurpose::Variable { name } => {
                // Choosing "All" alongside other values means just All.
                let values = if values.iter().any(|v| v == crate::grafana::variables::ALL_VALUE) {
                    vec![crate::grafana::variables::ALL_VALUE.to_string()]
                } else {
                    values
                };
                self.set_variable(&name, values);
            }
            SelectPurpose::MoveFolder { uid } => {
                let folder_title = if first.is_empty() {
                    "General".to_string()
                } else {
                    self.dashboards.hits.iter().find(|h| h.uid == first).map(|h| h.title.clone()).unwrap_or_else(|| first.clone())
                };
                let folder_uid = Some(first).filter(|s| !s.is_empty());
                let purpose = super::msg::FetchPurpose::Move { folder_uid, folder_title };
                self.spawn(move |c| async move { super::msg::Msg::EditFetched { result: c.dashboard(&uid).await, uid, purpose } });
            }
            SelectPurpose::Datasource => {
                self.explore.ds_uid = Some(first);
                self.explore.result = None;
            }
            SelectPurpose::History => {
                if let Ok(i) = first.parse::<usize>() {
                    self.load_history_entry(i);
                }
            }
        }
    }

    fn on_confirm(&mut self, p: Pending) {
        use super::msg::After;
        match p {
            Pending::DeleteDashboard { uid, title } => {
                let after = if self.dash.as_ref().is_some_and(|d| d.uid == uid) { After::CloseDashboard } else { After::ReloadDashboards };
                self.mutate(format!("deleted {title}"), after, move |c| async move { c.delete_dashboard(&uid).await.map(|_| ()) });
            }
            Pending::SaveDashboard { uid, folder_uid, json } => {
                let reload = uid.clone();
                self.mutate("dashboard saved", After::ReloadDashboard(reload), move |c| async move {
                    c.save_dashboard(&json, folder_uid.as_deref(), "Edited with lazygrafana").await.map(|_| ())
                });
            }
            Pending::RestoreVersion { uid, version } => {
                let reload = uid.clone();
                self.mutate(format!("restored version {version}"), After::ReloadDashboard(reload), move |c| async move {
                    c.restore_version(&uid, version).await.map(|_| ())
                });
            }
            Pending::ExpireSilences { ids } => self.expire_silences(ids),
            Pending::PauseRule { uid, pause } => self.pause_rule(uid, pause),
        }
    }

    /// `:` commands.
    pub fn run_command(&mut self, cmd: &str) {
        let cmd = cmd.trim().trim_start_matches(':');
        let (name, arg) = cmd.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((cmd, ""));
        match name {
            "" => {}
            "q" | "quit" | "q!" | "qa" => self.quit = true,
            "ctx" | "context" => {
                if arg.is_empty() {
                    self.popup = Some(Popup::context_picker(&self.config.context_names(), &self.ctx.name));
                } else {
                    self.switch_context(arg);
                }
            }
            "time" | "t" => {
                if arg.is_empty() {
                    self.popup = Some(Popup::time_picker(self.time.current()));
                } else {
                    match TimeRange::parse_input(arg) {
                        Ok(r) => self.set_time(r),
                        Err(e) => self.toast_err(e),
                    }
                }
            }
            "refresh" => {
                self.refresh = parse_refresh(arg);
                self.last_refresh = std::time::Instant::now();
                self.toast(format!("auto refresh {}", if arg.is_empty() { "off" } else { arg }));
            }
            "var" => {
                let Some((n, v)) = arg.split_once('=') else {
                    self.toast_err("usage: :var name=value[,value2]");
                    return;
                };
                let values: Vec<String> = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                let values = if values.iter().any(|v| v.eq_ignore_ascii_case("all")) {
                    vec![crate::grafana::variables::ALL_VALUE.to_string()]
                } else {
                    values
                };
                self.set_variable(n.trim().trim_start_matches('$'), values);
            }
            "tag" => {
                self.dashboards.tag = Some(arg.to_string()).filter(|s| !s.is_empty());
                self.dashboards.nav = Default::default();
                self.dashboards.rebuild();
                self.focus_side(Side::Dashboards);
            }
            "open" | "o" => {
                if arg.is_empty() {
                    self.toast_err("usage: :open <dashboard uid>");
                } else {
                    self.side = Side::Dashboards;
                    self.focus = Focus::Main;
                    self.open_dashboard(arg, true);
                }
            }
            "explore" | "e" => {
                self.side = Side::Explore;
                self.focus = Focus::Main;
                if !arg.is_empty() {
                    self.explore.input = super::widgets::TextInput::new(arg);
                    self.run_explore();
                } else {
                    self.explore.editing = true;
                }
            }
            "help" | "h" => self.popup = Some(Popup::help()),
            "stats" => {
                let mut lines: Vec<Line<'static>> = self.perf.report().into_iter().map(Line::raw).collect();
                lines.push(Line::raw(""));
                lines.push(Line::raw(format!(
                    "graphics              {} ({}×{} px cells)",
                    self.gfx.mode.name(),
                    self.gfx.cell.0,
                    self.gfx.cell.1
                )));
                self.popup = Some(Popup::text("Stats", lines));
            }
            "zoom" => {
                let f: f64 = arg.parse().unwrap_or(2.0);
                let r = self.time.current().zoom(now_ms(), None, f);
                self.set_time(r);
            }
            other => self.toast_err(format!("unknown command :{other} (try :ctx :time :var :refresh :tag :open :explore :q)")),
        }
        let _ = Duration::ZERO;
    }
}
