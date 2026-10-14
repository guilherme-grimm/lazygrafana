//! [3] Alerts: Grafana-managed rules, instances, silences, history and
//! desktop notifications for newly firing alerts.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::keymap::Act;
use super::msg::{After, Msg};
use super::popup::{InputPurpose, Pending, Popup};
use super::widgets::{InputResult, ListNav, Scroll, TextInput, fuzzy_filter, truncate};
use super::{App, Focus, Side, label_key};
use crate::grafana::dataframe::{Values, series_from_frames};
use crate::grafana::models::{Matcher, NewSilence, Rule, RuleGroup, Silence};
use crate::render::chart::Overlay;
use crate::render::panels::{PanelCtx, PanelData, render_chart};
use crate::timerange::{Millis, format_absolute, humanize_duration, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StateFilter {
    #[default]
    All,
    Firing,
    Pending,
    Normal,
    Silenced,
    Error,
}

impl StateFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Firing,
            Self::Firing => Self::Pending,
            Self::Pending => Self::Normal,
            Self::Normal => Self::Silenced,
            Self::Silenced => Self::Error,
            Self::Error => Self::All,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Firing => "firing",
            Self::Pending => "pending",
            Self::Normal => "normal",
            Self::Silenced => "silenced",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlertTab {
    #[default]
    Details,
    History,
    Silences,
}

impl AlertTab {
    const ALL: [AlertTab; 3] = [AlertTab::Details, AlertTab::History, AlertTab::Silences];
    fn title(self) -> &'static str {
        match self {
            AlertTab::Details => "Details",
            AlertTab::History => "History",
            AlertTab::Silences => "Silences",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ARow {
    Header(String),
    Rule { g: usize, r: usize },
}

#[derive(Debug, Clone)]
pub struct HistEntry {
    pub time: Millis,
    pub prev: String,
    pub next: String,
    pub text: String,
}

#[derive(Default)]
pub struct AlertsState {
    pub groups: Vec<RuleGroup>,
    pub silences: Vec<Silence>,
    pub am: Vec<crate::grafana::models::AmAlert>,
    pub loading: bool,
    pub error: Option<String>,
    pub nav: ListNav,
    pub rows: Vec<ARow>,
    pub marked: HashSet<String>,
    pub filter: String,
    pub filter_input: Option<TextInput>,
    pub state_filter: StateFilter,
    pub group_by: Option<String>,
    pub tab: AlertTab,
    pub history: HashMap<String, Result<Vec<HistEntry>, String>>,
    pub chart: HashMap<String, PanelData>,
    pub chart_loading: HashSet<String>,
    pub detail_scroll: Scroll,
    pub silence_nav: ListNav,
    pub last_poll: Option<Instant>,
    pub firing_seen: Option<HashSet<String>>,
}

/// Display state of a rule, accounting for pause, errors and silences.
pub fn rule_state(rule: &Rule, am: &[crate::grafana::models::AmAlert]) -> &'static str {
    if rule.is_paused {
        return "paused";
    }
    if rule.health == "error" || rule.health == "nodata" && rule.state != "firing" {
        return if rule.health == "error" { "error" } else { "nodata" };
    }
    match rule.state.as_str() {
        "firing" | "recovering" => {
            let instances: Vec<_> = am.iter().filter(|a| a.labels.get("__alert_rule_uid__") == Some(&rule.uid)).collect();
            if !instances.is_empty() && instances.iter().all(|a| !a.status.silenced_by.is_empty()) { "silenced" } else { "firing" }
        }
        "pending" => "pending",
        _ => "normal",
    }
}

pub fn state_style(state: &str) -> (&'static str, Color) {
    match state {
        "firing" => ("●", Color::LightRed),
        "pending" => ("◐", Color::Yellow),
        "silenced" => ("◌", Color::Blue),
        "paused" => ("⏸", Color::DarkGray),
        "error" => ("✖", Color::Magenta),
        "nodata" => ("?", Color::Gray),
        _ => ("○", Color::Green),
    }
}

impl AlertsState {
    pub fn rule(&self, g: usize, r: usize) -> Option<&Rule> {
        self.groups.get(g).and_then(|grp| grp.rules.get(r))
    }

    pub fn selected_rule(&self) -> Option<(&RuleGroup, &Rule)> {
        match self.rows.get(self.nav.selected)? {
            ARow::Rule { g, r } => Some((&self.groups[*g], &self.groups[*g].rules[*r])),
            ARow::Header(_) => None,
        }
    }

    pub fn firing_count(&self) -> usize {
        self.groups.iter().flat_map(|g| &g.rules).filter(|r| rule_state(r, &self.am) == "firing").count()
    }

    pub fn rebuild(&mut self) {
        let mut items: Vec<(String, usize, usize)> = Vec::new();
        for (gi, g) in self.groups.iter().enumerate() {
            for (ri, r) in g.rules.iter().enumerate() {
                let st = rule_state(r, &self.am);
                let keep = match self.state_filter {
                    StateFilter::All => true,
                    StateFilter::Firing => st == "firing",
                    StateFilter::Pending => st == "pending",
                    StateFilter::Normal => st == "normal",
                    StateFilter::Silenced => st == "silenced",
                    StateFilter::Error => st == "error" || st == "nodata",
                };
                if !keep {
                    continue;
                }
                let header = match &self.group_by {
                    Some(label) => format!(
                        "{label}={}",
                        r.labels
                            .get(label)
                            .or_else(|| r.alerts.iter().find_map(|a| a.labels.get(label)))
                            .cloned()
                            .unwrap_or_else(|| "—".into())
                    ),
                    None => format!("{} › {}", g.file, g.name),
                };
                items.push((header, gi, ri));
            }
        }
        if !self.filter.is_empty() {
            let hay: Vec<String> = items
                .iter()
                .map(|(h, g, r)| {
                    let rule = &self.groups[*g].rules[*r];
                    format!("{} {h} {}", rule.name, rule.labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" "))
                })
                .collect();
            let keep: HashSet<usize> = fuzzy_filter(&self.filter, &hay).into_iter().collect();
            items = items.into_iter().enumerate().filter(|(i, _)| keep.contains(i)).map(|(_, x)| x).collect();
        }
        items.sort_by(|a, b| a.0.cmp(&b.0));
        let mut rows = Vec::new();
        let mut last: Option<String> = None;
        for (h, g, r) in items {
            if last.as_ref() != Some(&h) {
                rows.push(ARow::Header(h.clone()));
                last = Some(h);
            }
            rows.push(ARow::Rule { g, r });
        }
        self.rows = rows;
        // Never leave the cursor on a header if a rule exists below it.
        if matches!(self.rows.get(self.nav.selected), Some(ARow::Header(_))) && self.rows.len() > self.nav.selected + 1 {
            self.nav.selected += 1;
        }
        self.nav.clamp(self.rows.len(), 1000);
    }

    /// Active silences affecting a rule's instances (or naming its uid).
    pub fn silences_for(&self, rule: &Rule) -> Vec<&Silence> {
        let mut label_sets: Vec<BTreeMap<String, String>> = rule
            .alerts
            .iter()
            .map(|a| {
                let mut l = a.labels.clone();
                l.insert("__alert_rule_uid__".into(), rule.uid.clone());
                l
            })
            .collect();
        let mut base = rule.labels.clone();
        base.insert("__alert_rule_uid__".into(), rule.uid.clone());
        base.insert("alertname".into(), rule.name.clone());
        label_sets.push(base);
        self.silences.iter().filter(|s| s.status.state != "expired" && label_sets.iter().any(|l| s.matches(l))).collect()
    }
}

impl App {
    pub fn load_alerts(&mut self) {
        let ctx_gen = self.ctx_gen;
        self.alerts.loading = true;
        self.alerts.last_poll = Some(Instant::now());
        self.spawn(move |c| async move { Msg::Rules { ctx_gen, result: c.rules().await } });
        self.spawn(move |c| async move { Msg::Silences { ctx_gen, result: c.silences().await } });
        self.spawn(move |c| async move { Msg::AmAlerts { ctx_gen, result: c.am_alerts().await } });
    }

    /// Background polling (every 30s) for the list and notifications.
    pub fn alerts_tick(&mut self) -> bool {
        if self.client.is_none() {
            return false;
        }
        let due = self.alerts.last_poll.is_none_or(|t| t.elapsed() >= Duration::from_secs(30));
        if due && (self.side == Side::Alerts || self.config.notifications) {
            self.load_alerts();
            return true;
        }
        false
    }

    pub fn ensure_rule_chart(&mut self) {
        if self.side != Side::Alerts {
            return;
        }
        let Some((_, rule)) = self.alerts.selected_rule() else { return };
        let uid = rule.uid.clone();
        if self.alerts.chart.contains_key(&uid) || self.alerts.chart_loading.contains(&uid) {
            return;
        }
        let range = self.range();
        let ds = rule.queried_datasource_uids.first().and_then(|u| self.ds_list.iter().find(|d| d.uid == *u)).cloned();
        let ds = match ds {
            Some(ds) if !rule.query.trim().is_empty() && matches!(ds.kind.as_str(), "prometheus" | "loki") => ds,
            other => {
                let why = match other {
                    None if self.ds_list.is_empty() => return, // datasources not loaded yet
                    None => "rule datasource not found".to_string(),
                    Some(ds) => format!("charts are only drawn for Prometheus/Loki rules ({} here)", ds.kind),
                };
                self.alerts.chart.insert(uid, PanelData { errors: vec![why], range, ..Default::default() });
                return;
            }
        };
        let (q, _) = super::query::adhoc_query(&ds, &rule.query, range, 600, 100);
        self.alerts.chart_loading.insert(uid.clone());
        self.spawn_query(move |c| async move {
            let req = crate::grafana::QueryRequest { from: range.0, to: range.1, queries: vec![q] };
            Msg::RuleChart { uid, range, result: c.query(&req).await }
        });
    }

    fn ensure_history(&mut self) {
        let Some((_, rule)) = self.alerts.selected_rule() else { return };
        let uid = rule.uid.clone();
        if self.alerts.history.contains_key(&uid) {
            return;
        }
        let (from, to) = (now_ms() - 7 * 86_400_000, now_ms());
        self.spawn(move |c| async move { Msg::RuleHistory { result: c.rule_history(&uid, from, to).await, uid } });
    }

    pub fn on_alerts_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Rules { ctx_gen, result } => {
                if ctx_gen != self.ctx_gen {
                    return;
                }
                self.alerts.loading = false;
                match result {
                    Ok(groups) => {
                        self.notify_new_firing(&groups);
                        self.alerts.groups = groups;
                        self.alerts.error = None;
                        self.alerts.rebuild();
                        self.ensure_rule_chart();
                    }
                    Err(e) => self.alerts.error = Some(e.to_string()),
                }
            }
            Msg::Silences { ctx_gen, result } => {
                if ctx_gen == self.ctx_gen {
                    match result {
                        Ok(mut s) => {
                            s.sort_by(|a, b| a.status.state.cmp(&b.status.state).then(b.ends_at.cmp(&a.ends_at)));
                            self.alerts.silences = s;
                        }
                        Err(e) => tracing::warn!("silences: {e}"),
                    }
                }
            }
            Msg::AmAlerts { ctx_gen, result } => {
                if ctx_gen == self.ctx_gen
                    && let Ok(a) = result
                {
                    self.alerts.am = a;
                    self.alerts.rebuild();
                }
            }
            Msg::RuleHistory { uid, result } => {
                let parsed = result.map_err(|e| e.to_string()).map(|frame| {
                    let col = |name: &str| frame.field(name).map(|f| f.values.clone());
                    let times = frame.field("time").and_then(|f| f.times().map(|t| t.to_vec())).unwrap_or_default();
                    let get = |v: &Option<Values>, i: usize| match v {
                        Some(Values::String(s)) => s.get(i).cloned().flatten().unwrap_or_default(),
                        _ => String::new(),
                    };
                    let (text, prev, next) = (col("text"), col("prev"), col("next"));
                    let mut entries: Vec<HistEntry> = (0..times.len())
                        .map(|i| HistEntry {
                            time: times[i].unwrap_or_default(),
                            prev: get(&prev, i),
                            next: get(&next, i),
                            text: get(&text, i),
                        })
                        .collect();
                    entries.sort_by_key(|e| std::cmp::Reverse(e.time));
                    entries
                });
                self.alerts.history.insert(uid, parsed);
            }
            Msg::RuleChart { uid, range, result } => {
                self.alerts.chart_loading.remove(&uid);
                let data = match result {
                    Ok(r) => {
                        let frames = r.frames();
                        PanelData { series: series_from_frames(&frames), frames, errors: r.errors(), range, ..Default::default() }
                    }
                    Err(e) => PanelData { errors: vec![e.to_string()], range, ..Default::default() },
                };
                self.alerts.chart.insert(uid, data);
            }
            _ => {}
        }
    }

    fn notify_new_firing(&mut self, groups: &[RuleGroup]) {
        let mut current = HashSet::new();
        let mut details: HashMap<String, (String, String)> = HashMap::new();
        for g in groups {
            for r in &g.rules {
                for a in r.alerts.iter().filter(|a| a.state == "Alerting") {
                    let key = label_key(&r.uid, &a.labels);
                    let summary =
                        a.annotations.get("summary").or_else(|| r.annotations.get("summary")).cloned().unwrap_or_else(|| g.file.clone());
                    details.insert(key.clone(), (r.name.clone(), summary));
                    current.insert(key);
                }
            }
        }
        if let Some(prev) = &self.alerts.firing_seen
            && self.config.notifications
        {
            let new: Vec<&String> = current.difference(prev).collect();
            for key in new.iter().take(3) {
                if let Some((name, summary)) = details.get(*key) {
                    crate::term::notify(&format!("Firing: {name}"), summary);
                }
            }
            if !new.is_empty() {
                self.toast_err(format!("{} alert(s) started firing", new.len()));
            }
        }
        self.alerts.firing_seen = Some(current);
    }

    // ---- keys -------------------------------------------------------------------------

    pub fn alerts_filter_key(&mut self, k: &KeyEvent) {
        let Some(input) = &mut self.alerts.filter_input else { return };
        match input.handle(k) {
            InputResult::Submit => self.alerts.filter_input = None,
            InputResult::Cancel => {
                self.alerts.filter_input = None;
                self.alerts.filter.clear();
                self.alerts.rebuild();
            }
            InputResult::Changed => {
                self.alerts.filter = input.text.clone();
                self.alerts.nav.selected = 0;
                self.alerts.rebuild();
            }
            InputResult::Unchanged => {}
        }
    }

    fn selected_rules(&self) -> Vec<Rule> {
        let all: Vec<Rule> = self.alerts.groups.iter().flat_map(|g| g.rules.iter().cloned()).collect();
        if !self.alerts.marked.is_empty() {
            return all.into_iter().filter(|r| self.alerts.marked.contains(&r.uid)).collect();
        }
        self.alerts.selected_rule().map(|(_, r)| vec![r.clone()]).unwrap_or_default()
    }

    pub fn alerts_side_key(&mut self, acts: &[Act]) -> bool {
        let before = self.alerts.nav.selected;
        if self.alerts.nav.handle(acts, self.alerts.rows.len(), self.side_list_height()) {
            // Skip headers in the direction of travel.
            if matches!(self.alerts.rows.get(self.alerts.nav.selected), Some(ARow::Header(_))) {
                let down = self.alerts.nav.selected >= before;
                let n = self.alerts.rows.len();
                if down && self.alerts.nav.selected + 1 < n {
                    self.alerts.nav.selected += 1;
                } else if !down && self.alerts.nav.selected > 0 {
                    self.alerts.nav.selected -= 1;
                } else if self.alerts.nav.selected + 1 < n {
                    self.alerts.nav.selected += 1;
                }
            }
            self.alerts.detail_scroll = Scroll::default();
            self.ensure_rule_chart();
            if self.alerts.tab == AlertTab::History {
                self.ensure_history();
            }
            return true;
        }
        self.alert_action(acts)
    }

    fn alert_action(&mut self, acts: &[Act]) -> bool {
        for a in acts {
            match a {
                Act::Filter => {
                    self.alerts.filter.clear();
                    self.alerts.filter_input = Some(TextInput::default());
                    self.alerts.rebuild();
                }
                Act::Back if !self.alerts.filter.is_empty() || !self.alerts.marked.is_empty() => {
                    self.alerts.filter.clear();
                    self.alerts.marked.clear();
                    self.alerts.rebuild();
                }
                Act::StateFilter => {
                    self.alerts.state_filter = self.alerts.state_filter.next();
                    self.alerts.nav = ListNav::default();
                    self.alerts.rebuild();
                    self.toast(format!("showing {} alerts", self.alerts.state_filter.name()));
                }
                Act::GroupBy => {
                    let cur = self.alerts.group_by.clone().unwrap_or_default();
                    self.popup = Some(Popup::input("Group by label (empty = folder)", "› ", &cur, InputPurpose::GroupBy));
                }
                Act::Mark => {
                    if let Some((_, r)) = self.alerts.selected_rule() {
                        let uid = r.uid.clone();
                        if !self.alerts.marked.remove(&uid) {
                            self.alerts.marked.insert(uid);
                        }
                        let n = self.alerts.rows.len();
                        self.alerts.nav.selected = (self.alerts.nav.selected + 1).min(n.saturating_sub(1));
                    }
                }
                Act::NextTab | Act::PrevTab => {
                    let i = AlertTab::ALL.iter().position(|t| *t == self.alerts.tab).unwrap_or(0);
                    self.alerts.tab = AlertTab::ALL[if *a == Act::NextTab { (i + 1) % 3 } else { (i + 2) % 3 }];
                    if self.alerts.tab == AlertTab::History {
                        self.ensure_history();
                    }
                }
                Act::Silence => {
                    if self.readonly_guard() {
                        return true;
                    }
                    let rules = self.selected_rules();
                    if rules.is_empty() {
                        return true;
                    }
                    let matcher = if rules.len() == 1 {
                        Matcher::eq("__alert_rule_uid__", &rules[0].uid)
                    } else {
                        Matcher {
                            name: "__alert_rule_uid__".into(),
                            value: rules.iter().map(|r| regex::escape(&r.uid)).collect::<Vec<_>>().join("|"),
                            is_regex: true,
                            is_equal: true,
                        }
                    };
                    let title = if rules.len() == 1 { rules[0].name.clone() } else { format!("{} rules", rules.len()) };
                    self.popup = Some(Popup::silence(&title, &[matcher]));
                }
                Act::Unsilence => {
                    if self.readonly_guard() {
                        return true;
                    }
                    let ids: Vec<String> = if self.alerts.tab == AlertTab::Silences && self.focus == Focus::Main {
                        self.alerts
                            .silences
                            .iter()
                            .filter(|s| s.status.state != "expired")
                            .nth(self.alerts.silence_nav.selected)
                            .map(|s| vec![s.id.clone()])
                            .unwrap_or_default()
                    } else {
                        let rules = self.selected_rules();
                        let mut ids: Vec<String> =
                            rules.iter().flat_map(|r| self.alerts.silences_for(r).into_iter().map(|s| s.id.clone())).collect();
                        ids.sort();
                        ids.dedup();
                        ids
                    };
                    if ids.is_empty() {
                        self.toast("no active silences");
                        return true;
                    }
                    let lines = ids
                        .iter()
                        .filter_map(|id| self.alerts.silences.iter().find(|s| &s.id == id))
                        .map(|s| {
                            Line::raw(format!("{}  {}", s.matchers.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(", "), s.comment))
                        })
                        .collect();
                    self.popup = Some(Popup::confirm(&format!("Expire {} silence(s)?", ids.len()), lines, Pending::ExpireSilences { ids }));
                }
                Act::Pause => {
                    if self.readonly_guard() {
                        return true;
                    }
                    let Some((_, r)) = self.alerts.selected_rule() else { return true };
                    let pause = !r.is_paused;
                    let mut lines = vec![Line::raw(format!("{} rule {}?", if pause { "Pause" } else { "Resume" }, r.name))];
                    if r.provenance.as_deref().is_some_and(|p| !p.is_empty()) {
                        lines.push(Line::styled(
                            format!(
                                "This rule is provisioned ({}); Grafana may refuse the change.",
                                r.provenance.clone().unwrap_or_default()
                            ),
                            Style::default().fg(Color::Yellow),
                        ));
                    }
                    self.popup = Some(Popup::confirm(
                        if pause { "Pause rule?" } else { "Resume rule?" },
                        lines,
                        Pending::PauseRule { uid: r.uid.clone(), pause },
                    ));
                }
                Act::OpenBrowser | Act::YankUrl => {
                    let Some((_, r)) = self.alerts.selected_rule() else { return true };
                    let url = format!("{}/alerting/grafana/{}/view", self.base_url(), r.uid);
                    if *a == Act::OpenBrowser {
                        self.open_url(&url);
                    } else {
                        self.copy("rule URL", &url);
                    }
                }
                Act::YankUid => {
                    if let Some((_, r)) = self.alerts.selected_rule() {
                        let uid = r.uid.clone();
                        self.copy("rule UID", &uid);
                    }
                }
                _ => continue,
            }
            return true;
        }
        false
    }

    pub fn alerts_main_key(&mut self, acts: &[Act]) -> bool {
        match self.alerts.tab {
            AlertTab::Silences => {
                let n = self.alerts.silences.iter().filter(|s| s.status.state != "expired").count();
                if self.alerts.silence_nav.handle(acts, n, 20) {
                    return true;
                }
            }
            _ => {
                if acts.iter().any(|a| matches!(a, Act::Down | Act::Up | Act::HalfDown | Act::HalfUp | Act::Top | Act::Bottom)) {
                    return self.alerts.detail_scroll.handle(acts, 500, self.hit.main.height as usize);
                }
            }
        }
        self.alert_action(acts)
    }

    pub fn create_silence(&mut self, matchers: Vec<Matcher>, duration: Millis, comment: String) {
        let now = chrono::Utc::now();
        let s = NewSilence {
            matchers,
            starts_at: now.to_rfc3339(),
            ends_at: (now + chrono::Duration::milliseconds(duration)).to_rfc3339(),
            created_by: self
                .instance
                .as_ref()
                .and_then(|i| i.user.as_ref())
                .map(|u| if u.login.is_empty() { u.name.clone() } else { u.login.clone() })
                .unwrap_or_else(|| "lazygrafana".into()),
            comment,
        };
        self.alerts.marked.clear();
        self.mutate(format!("silenced for {}", humanize_duration(duration)), After::ReloadAlerts, move |c| async move {
            c.create_silence(&s).await.map(|_| ())
        });
    }

    pub fn expire_silences(&mut self, ids: Vec<String>) {
        let n = ids.len();
        self.mutate(format!("expired {n} silence(s)"), After::ReloadAlerts, move |c| async move {
            for id in ids {
                c.expire_silence(&id).await?;
            }
            Ok(())
        });
    }

    pub fn pause_rule(&mut self, uid: String, pause: bool) {
        self.mutate(if pause { "rule paused" } else { "rule resumed" }, After::ReloadAlerts, move |c| async move {
            let mut rule = c.provisioned_rule(&uid).await?;
            let provenance = rule.get("provenance").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
            if provenance == "file" {
                return Err(crate::grafana::ApiError {
                    status: None,
                    message: "rule is provisioned from a file; change isPaused in the provisioning file instead".into(),
                });
            }
            rule["isPaused"] = serde_json::json!(pause);
            c.update_provisioned_rule(&uid, &rule, provenance.is_empty()).await.map(|_| ())
        });
    }

    // ---- rendering -------------------------------------------------------------------------

    pub fn render_alerts_side(&mut self, f: &mut Frame, area: Rect) {
        let mut list_area = area;
        let show_bar = self.alerts.filter_input.is_some()
            || !self.alerts.filter.is_empty()
            || self.alerts.state_filter != StateFilter::All
            || self.alerts.group_by.is_some();
        if show_bar {
            let bar = Rect { height: 1, ..area };
            list_area = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
            let mut prefix = String::new();
            if self.alerts.state_filter != StateFilter::All {
                prefix.push_str(&format!("[{}] ", self.alerts.state_filter.name()));
            }
            if let Some(g) = &self.alerts.group_by {
                prefix.push_str(&format!("by {g} "));
            }
            match &self.alerts.filter_input {
                Some(i) => i.render(bar, f.buffer_mut(), &format!("{prefix}/"), Style::default().fg(Color::Yellow), true),
                None => f.buffer_mut().set_string(
                    bar.x,
                    bar.y,
                    truncate(
                        &format!(
                            "{prefix}{}",
                            if self.alerts.filter.is_empty() { String::new() } else { format!("/{}", self.alerts.filter) }
                        ),
                        bar.width as usize,
                    ),
                    Style::default().fg(Color::Yellow),
                ),
            }
        }
        if let Some(e) = &self.alerts.error {
            f.render_widget(
                Paragraph::new(Line::styled(format!("⚠ {e}"), Style::default().fg(Color::Red))).wrap(Wrap { trim: true }),
                list_area,
            );
            return;
        }
        if self.alerts.rows.is_empty() {
            let t = if self.alerts.loading { "loading…" } else { "no alert rules" };
            f.buffer_mut().set_string(list_area.x, list_area.y, t, Style::default().fg(Color::DarkGray));
            return;
        }
        let height = list_area.height as usize;
        self.alerts.nav.clamp(self.alerts.rows.len(), height);
        for (row, i) in self.alerts.nav.visible(self.alerts.rows.len(), height).enumerate() {
            let line = match &self.alerts.rows[i] {
                ARow::Header(h) => Line::styled(h.clone(), Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                ARow::Rule { g, r } => {
                    let rule = &self.alerts.groups[*g].rules[*r];
                    let st = rule_state(rule, &self.alerts.am);
                    let (icon, color) = state_style(st);
                    let firing = rule.alerts.iter().filter(|a| a.state == "Alerting").count();
                    let mut spans = vec![
                        Span::raw(if self.alerts.marked.contains(&rule.uid) { "▌" } else { " " }),
                        Span::styled(format!("{icon} "), Style::default().fg(color)),
                        Span::raw(rule.name.clone()),
                    ];
                    if firing > 1 {
                        spans.push(Span::styled(format!(" ×{firing}"), Style::default().fg(color)));
                    }
                    if let Some(since) = rule.active_at.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        && st != "normal"
                    {
                        spans.push(Span::styled(
                            format!("  {}", humanize_duration(now_ms() - since.timestamp_millis())),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    Line::from(spans)
                }
            };
            self.list_row(f, list_area, row, i == self.alerts.nav.selected, line);
        }
        self.hit.side_lists.retain(|(s, _, _)| *s != Side::Alerts);
        self.hit.side_lists.push((Side::Alerts, list_area, self.alerts.nav.offset));
    }

    pub fn alerts_main_title(&self) -> Vec<Span<'static>> {
        let mut spans = vec![Span::styled(
            format!(" {} ", self.alerts.selected_rule().map(|(_, r)| r.name.clone()).unwrap_or_else(|| "Alerts".into())),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        for t in AlertTab::ALL {
            let style = if t == self.alerts.tab {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            spans.push(Span::styled(format!(" {} ", t.title()), style));
        }
        spans
    }

    pub fn render_alerts_main(&mut self, f: &mut Frame, area: Rect) {
        match self.alerts.tab {
            AlertTab::Details => self.render_alert_details(f, area),
            AlertTab::History => self.render_alert_history(f, area),
            AlertTab::Silences => self.render_silences(f, area),
        }
    }

    fn render_alert_details(&mut self, f: &mut Frame, area: Rect) {
        let Some((group, rule)) = self.alerts.selected_rule() else {
            f.buffer_mut().set_string(area.x, area.y, "Select an alert rule.", Style::default().fg(Color::DarkGray));
            return;
        };
        let (group, rule) = (group.clone(), rule.clone());
        let st = rule_state(&rule, &self.alerts.am);
        let (icon, color) = state_style(st);
        let key = |k: &str| Span::styled(format!("{k:<12}"), Style::default().fg(Color::Gray));
        let mut lines = vec![
            Line::from(vec![
                key("State"),
                Span::styled(format!("{icon} {st}"), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(format!("   health: {}", rule.health), Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![key("Folder"), Span::raw(format!("{} › {} (every {}s)", group.file, group.name, group.interval))]),
        ];
        if let Some(t) = &rule.last_evaluation {
            lines.push(Line::from(vec![key("Evaluated"), Span::raw(t.clone())]));
        }
        if let Some(e) = rule.last_error.as_ref().filter(|e| !e.is_empty()) {
            lines.push(Line::from(vec![key("Error"), Span::styled(e.clone(), Style::default().fg(Color::Red))]));
        }
        lines.push(Line::from(vec![key("Query"), Span::styled(rule.query.clone(), Style::default().fg(Color::Yellow))]));
        if !rule.labels.is_empty() {
            lines.push(Line::from(vec![
                key("Labels"),
                Span::raw(rule.labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("  ")),
            ]));
        }
        for (k, v) in &rule.annotations {
            lines.push(Line::from(vec![key(k), Span::raw(v.clone())]));
        }
        let sil = self.alerts.silences_for(&rule);
        if !sil.is_empty() {
            lines.push(Line::from(vec![
                key("Silenced"),
                Span::styled(
                    format!("{} active silence(s), until {}", sil.len(), sil.iter().map(|s| s.ends_at.clone()).max().unwrap_or_default()),
                    Style::default().fg(Color::Blue),
                ),
            ]));
        }
        if !rule.alerts.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("Instances ({})", rule.alerts.len()),
                Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD),
            ));
            for a in &rule.alerts {
                let c = match a.state.as_str() {
                    "Alerting" => Color::LightRed,
                    "Pending" => Color::Yellow,
                    "Normal" => Color::Green,
                    _ => Color::Magenta,
                };
                let labels: Vec<String> = a
                    .labels
                    .iter()
                    .filter(|(k, _)| !k.starts_with("__") && k.as_str() != "alertname" && k.as_str() != "grafana_folder")
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:<9}", a.state), Style::default().fg(c)),
                    Span::styled(format!("{:<12}", a.value), Style::default().fg(Color::White)),
                    Span::styled(labels.join(" "), Style::default().fg(Color::DarkGray)),
                ]));
            }
        }
        let chart_h = if area.height >= 20 { (area.height / 2).min(16) } else { 0 };
        let [text_area, chart_area] = Layout::vertical([Constraint::Min(4), Constraint::Length(chart_h)]).areas(area);
        let max = lines.len().saturating_sub(text_area.height as usize);
        self.alerts.detail_scroll.y = self.alerts.detail_scroll.y.min(max);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((self.alerts.detail_scroll.y as u16, 0)), text_area);
        if chart_h > 0 {
            let title = Rect { height: 1, ..chart_area };
            f.buffer_mut().set_string(
                title.x,
                title.y,
                format!("── query over {} ", self.time.current()),
                Style::default().fg(Color::DarkGray),
            );
            let inner = Rect { y: chart_area.y + 1, height: chart_area.height.saturating_sub(1), ..chart_area };
            match self.alerts.chart.get(&rule.uid).cloned() {
                Some(data) if !data.series.is_empty() => {
                    let spec = crate::render::chart::ChartSpec {
                        series: data
                            .series
                            .iter()
                            .enumerate()
                            .map(|(i, s)| crate::render::chart::ChartSeries {
                                name: s.name.clone(),
                                color: crate::units::palette(i),
                                points: s.points.clone(),
                            })
                            .collect(),
                        range: data.range,
                        ..Default::default()
                    };
                    let images_ok = self.popup.is_none() || self.gfx.mode.supports_overlap();
                    let mut ctx = PanelCtx {
                        gfx: &self.gfx,
                        cache: &mut self.images,
                        marker: self.marker,
                        overlay: Overlay::default(),
                        annotations: &[],
                        images: images_ok,
                        key: format!("rule:{}:{}", rule.uid, inner.height),
                        wrap_logs: false,
                    };
                    render_chart(&spec, inner, f.buffer_mut(), &mut ctx);
                }
                Some(data) if !data.errors.is_empty() => f.buffer_mut().set_string(
                    inner.x,
                    inner.y,
                    truncate(&format!("⚠ {}", data.errors.join("; ")), inner.width as usize),
                    Style::default().fg(Color::Red),
                ),
                Some(_) => f.buffer_mut().set_string(inner.x, inner.y, "no data", Style::default().fg(Color::DarkGray)),
                None => f.buffer_mut().set_string(inner.x, inner.y, "loading…", Style::default().fg(Color::DarkGray)),
            }
        }
    }

    fn render_alert_history(&mut self, f: &mut Frame, area: Rect) {
        let Some((_, rule)) = self.alerts.selected_rule() else { return };
        match self.alerts.history.get(&rule.uid) {
            None => f.buffer_mut().set_string(area.x, area.y, "loading history…", Style::default().fg(Color::DarkGray)),
            Some(Err(e)) => f.render_widget(
                Paragraph::new(vec![
                    Line::styled(format!("⚠ {e}"), Style::default().fg(Color::Red)),
                    Line::styled("State history may be disabled on this instance.", Style::default().fg(Color::DarkGray)),
                ]),
                area,
            ),
            Some(Ok(entries)) if entries.is_empty() => {
                f.buffer_mut().set_string(area.x, area.y, "No state changes in the last 7 days.", Style::default().fg(Color::DarkGray))
            }
            Some(Ok(entries)) => {
                let color = |s: &str| match s.split(' ').next().unwrap_or("") {
                    "Alerting" => Color::LightRed,
                    "Pending" => Color::Yellow,
                    "Normal" => Color::Green,
                    _ => Color::Magenta,
                };
                let lines: Vec<Line> = entries
                    .iter()
                    .skip(self.alerts.detail_scroll.y)
                    .take(area.height as usize)
                    .map(|e| {
                        Line::from(vec![
                            Span::styled(format!("{}  ", format_absolute(e.time)), Style::default().fg(Color::Gray)),
                            Span::styled(format!("{:<10}", e.prev), Style::default().fg(color(&e.prev))),
                            Span::raw("→ "),
                            Span::styled(format!("{:<10}", e.next), Style::default().fg(color(&e.next)).add_modifier(Modifier::BOLD)),
                            Span::styled(e.text.split(" - ").nth(1).unwrap_or("").to_string(), Style::default().fg(Color::DarkGray)),
                        ])
                    })
                    .collect();
                f.render_widget(Paragraph::new(lines), area);
            }
        }
    }

    fn render_silences(&mut self, f: &mut Frame, area: Rect) {
        let rule = self.alerts.selected_rule().map(|(_, r)| r.clone());
        let affecting: HashSet<String> =
            rule.as_ref().map(|r| self.alerts.silences_for(r).into_iter().map(|s| s.id.clone()).collect()).unwrap_or_default();
        let list: Vec<&Silence> = self.alerts.silences.iter().filter(|s| s.status.state != "expired").collect();
        if list.is_empty() {
            f.buffer_mut().set_string(
                area.x,
                area.y,
                "No active or pending silences. Press S on a rule to create one.",
                Style::default().fg(Color::DarkGray),
            );
            return;
        }
        let height = area.height.saturating_sub(1) as usize / 2;
        self.alerts.silence_nav.clamp(list.len(), height);
        let mut y = area.y;
        for i in self.alerts.silence_nav.visible(list.len(), height) {
            let s = list[i];
            let sel = i == self.alerts.silence_nav.selected;
            let bg = if sel { Style::default().bg(Color::Rgb(0x2a, 0x2f, 0x3a)) } else { Style::default() };
            let ends = chrono::DateTime::parse_from_rfc3339(&s.ends_at)
                .map(|t| humanize_duration(t.timestamp_millis() - now_ms()))
                .unwrap_or_default();
            let l1 = Line::from(vec![
                Span::styled(if affecting.contains(&s.id) { "◌ " } else { "  " }, Style::default().fg(Color::Blue)),
                Span::styled(format!("{:<8}", s.status.state), Style::default().fg(Color::Blue)),
                Span::raw(s.matchers.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(", ")),
            ]);
            let l2 =
                Line::styled(format!("    ends in {ends} · by {} · {}", s.created_by, s.comment), Style::default().fg(Color::DarkGray));
            for l in [l1, l2] {
                if y >= area.y + area.height {
                    break;
                }
                f.buffer_mut().set_style(Rect { y, height: 1, ..area }, bg);
                f.buffer_mut().set_line(area.x, y, &l, area.width);
                y += 1;
            }
        }
        f.buffer_mut().set_string(
            area.x,
            area.y + area.height - 1,
            "◌ = affects the selected rule · U expire selected silence",
            Style::default().fg(Color::DarkGray),
        );
    }
}
