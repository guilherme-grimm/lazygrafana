//! [2] Dashboards: folder tree, dashboard loading (variables → queries →
//! annotations), and the Panels / Variables / Versions / JSON tabs.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use serde_json::Value;

use super::chartnav::{ChartCursor, ChartOutcome};
use super::keymap::Act;
use super::msg::{After, FetchPurpose, LogsJump, Msg};
use super::popup::{InputPurpose, Pending, Popup, SelectItem, SelectPurpose};
use super::query::build_panel_queries;
use super::widgets::{InputResult, ListNav, Scroll, TextInput, fuzzy_filter, truncate};
use super::{App, EditorRequest, Focus, Side};
use crate::grafana::dashboard::{Dashboard, Panel};
use crate::grafana::dataframe::series_from_frames;
use crate::grafana::models::{DashboardMeta, NewAnnotation, SearchHit, VersionInfo};
use crate::grafana::variables::{self, Scope, VarKind, VarSet, post_process, resolve_ds};
use crate::render::chart::{ChartAnnotation, Overlay};
use crate::render::panels::{PanelCtx, PanelData, render_panel};
use crate::timerange::{Millis, TimeRange, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMode {
    Tree,
    Starred,
    Recent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Folder { uid: String, title: String, depth: usize, expanded: bool, count: usize },
    Dash { hit: usize, depth: usize },
}

pub struct DashboardsState {
    pub hits: Vec<SearchHit>,
    pub loading: bool,
    pub error: Option<String>,
    pub expanded: HashSet<String>,
    pub mode: ListMode,
    pub filter: String,
    pub filter_input: Option<TextInput>,
    pub tag: Option<String>,
    pub nav: ListNav,
    pub rows: Vec<Row>,
    pub recent: Vec<String>,
    recent_path: Option<PathBuf>,
}

impl DashboardsState {
    pub fn new(state_dir: Option<&Path>) -> Self {
        let recent_path = state_dir.map(|d| d.join("recent.json"));
        let recent = recent_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            hits: Vec::new(),
            loading: false,
            error: None,
            expanded: HashSet::new(),
            mode: ListMode::Tree,
            filter: String::new(),
            filter_input: None,
            tag: None,
            nav: ListNav::default(),
            rows: Vec::new(),
            recent,
            recent_path,
        }
    }

    pub fn reset(&mut self) {
        self.hits.clear();
        self.rows.clear();
        self.error = None;
        self.nav = ListNav::default();
        self.filter.clear();
        self.filter_input = None;
        self.tag = None;
        self.expanded.clear();
    }

    fn push_recent(&mut self, uid: &str) {
        self.recent.retain(|u| u != uid);
        self.recent.insert(0, uid.to_string());
        self.recent.truncate(30);
        if let Some(p) = &self.recent_path {
            let _ = std::fs::create_dir_all(p.parent().unwrap_or(Path::new(".")));
            let _ = std::fs::write(p, serde_json::to_string(&self.recent).unwrap_or_default());
        }
    }

    fn folder_path(&self, folder_uid: Option<&str>) -> String {
        let mut parts = Vec::new();
        let mut cur = folder_uid.map(str::to_string);
        let mut guard = 0;
        while let Some(uid) = cur {
            let Some(f) = self.hits.iter().find(|h| h.is_folder() && h.uid == uid) else { break };
            parts.push(f.title.clone());
            cur = f.folder_uid.clone();
            guard += 1;
            if guard > 20 {
                break;
            }
        }
        parts.reverse();
        parts.join("/")
    }

    pub fn rebuild(&mut self) {
        let selected_key = self.rows.get(self.nav.selected).map(|r| self.row_key(r));
        let mut rows = Vec::new();
        let flat = !self.filter.is_empty() || self.mode != ListMode::Tree || self.tag.is_some();
        if flat {
            let mut idx: Vec<usize> = self
                .hits
                .iter()
                .enumerate()
                .filter(|(_, h)| !h.is_folder())
                .filter(|(_, h)| self.tag.as_ref().is_none_or(|t| h.tags.contains(t)))
                .filter(|(_, h)| self.mode != ListMode::Starred || h.is_starred)
                .filter(|(_, h)| self.mode != ListMode::Recent || self.recent.contains(&h.uid))
                .map(|(i, _)| i)
                .collect();
            if self.mode == ListMode::Recent {
                idx.sort_by_key(|i| self.recent.iter().position(|u| *u == self.hits[*i].uid).unwrap_or(usize::MAX));
            } else {
                idx.sort_by_key(|i| (self.folder_path(self.hits[*i].folder_uid.as_deref()), self.hits[*i].title.to_lowercase()));
            }
            if !self.filter.is_empty() {
                let hay: Vec<String> = idx
                    .iter()
                    .map(|i| {
                        let h = &self.hits[*i];
                        format!("{}/{} {}", self.folder_path(h.folder_uid.as_deref()), h.title, h.tags.join(" "))
                    })
                    .collect();
                idx = fuzzy_filter(&self.filter, &hay).into_iter().map(|j| idx[j]).collect();
            }
            rows = idx.into_iter().map(|hit| Row::Dash { hit, depth: 0 }).collect();
        } else {
            self.tree_rows(None, 0, &mut rows);
        }
        self.rows = rows;
        if let Some(k) = selected_key
            && let Some(i) = self.rows.iter().position(|r| self.row_key(r) == k)
        {
            self.nav.selected = i;
        }
        self.nav.clamp(self.rows.len(), 1000);
    }

    fn row_key(&self, r: &Row) -> String {
        match r {
            Row::Folder { uid, .. } => format!("f:{uid}"),
            Row::Dash { hit, .. } => format!("d:{}", self.hits.get(*hit).map(|h| h.uid.as_str()).unwrap_or("")),
        }
    }

    fn tree_rows(&self, parent: Option<&str>, depth: usize, out: &mut Vec<Row>) {
        let mut folders: Vec<&SearchHit> = self.hits.iter().filter(|h| h.is_folder() && h.folder_uid.as_deref() == parent).collect();
        folders.sort_by_key(|h| h.title.to_lowercase());
        for f in folders {
            let expanded = self.expanded.contains(&f.uid);
            let count = self.hits.iter().filter(|h| h.folder_uid.as_deref() == Some(&f.uid)).count();
            out.push(Row::Folder { uid: f.uid.clone(), title: f.title.clone(), depth, expanded, count });
            if expanded {
                self.tree_rows(Some(&f.uid), depth + 1, out);
            }
        }
        let mut dashes: Vec<usize> =
            self.hits.iter().enumerate().filter(|(_, h)| !h.is_folder() && h.folder_uid.as_deref() == parent).map(|(i, _)| i).collect();
        dashes.sort_by_key(|i| self.hits[*i].title.to_lowercase());
        out.extend(dashes.into_iter().map(|hit| Row::Dash { hit, depth }));
    }

    pub fn selected_hit(&self) -> Option<&SearchHit> {
        match self.rows.get(self.nav.selected)? {
            Row::Dash { hit, .. } => self.hits.get(*hit),
            Row::Folder { .. } => None,
        }
    }

    pub fn hit_by_uid(&self, uid: &str) -> Option<&SearchHit> {
        self.hits.iter().find(|h| h.uid == uid && !h.is_folder())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashTab {
    Panels,
    Variables,
    Versions,
    Json,
}

impl DashTab {
    const ALL: [DashTab; 4] = [DashTab::Panels, DashTab::Variables, DashTab::Versions, DashTab::Json];
    fn title(self) -> &'static str {
        match self {
            DashTab::Panels => "Panels",
            DashTab::Variables => "Variables",
            DashTab::Versions => "Versions",
            DashTab::Json => "JSON",
        }
    }
}

pub struct OpenDashboard {
    pub uid: String,
    pub generation: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub meta: DashboardMeta,
    pub dash: Option<Dashboard>,
    pub vars: VarSet,
    pub data: HashMap<i64, PanelData>,
    pub query_gen: u64,
    pub annotations: Vec<ChartAnnotation>,
    pub tab: DashTab,
    /// Index into `order`.
    pub selected: usize,
    pub order: Vec<usize>,
    pub scroll: u16,
    pub maximized: bool,
    pub cursor: ChartCursor,
    pub var_nav: ListNav,
    pub versions: Option<Result<Vec<VersionInfo>, String>>,
    pub ver_nav: ListNav,
    pub ver_mark: Option<usize>,
    pub json_scroll: Scroll,
    pub json_lines: Vec<Line<'static>>,
    pub exact: HashMap<i64, Result<Vec<u8>, String>>,
    pub exact_view: Option<i64>,
    pub vars_loading: bool,
    /// Panels whose data is out of date for the current query generation.
    pub stale: HashSet<i64>,
    /// Panels on screen or within a screen of it, from the last frame (queried first).
    pub near: Vec<i64>,
}

impl OpenDashboard {
    pub fn panel(&self, i: usize) -> Option<&Panel> {
        self.dash.as_ref().and_then(|d| self.order.get(i).and_then(|pi| d.panels.get(*pi)))
    }

    pub fn selected_panel(&self) -> Option<&Panel> {
        self.panel(self.selected)
    }
}

static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn next_gen() -> u64 {
    GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Grafana grid units → terminal rows: a Grafana screen is ~30 units tall.
pub fn row_scale(height: u16) -> f32 {
    (height as f32 / 30.0).clamp(0.8, 1.6)
}

impl App {
    // ---- loading ----------------------------------------------------------------------

    pub fn load_search(&mut self) {
        self.dashboards.loading = true;
        let ctx_gen = self.ctx_gen;
        self.spawn(move |c| async move { Msg::Search { ctx_gen, result: c.search("", false).await } });
    }

    pub fn open_dashboard(&mut self, uid: &str, apply_time: bool) {
        let keep = self.dash.as_ref().filter(|d| d.uid == uid);
        let (tab, selected, scroll) = keep.map(|d| (d.tab, d.selected, d.scroll)).unwrap_or((DashTab::Panels, 0, 0));
        let generation = next_gen();
        self.dash = Some(OpenDashboard {
            uid: uid.to_string(),
            generation,
            loading: true,
            error: None,
            meta: DashboardMeta::default(),
            dash: None,
            vars: VarSet::default(),
            data: HashMap::new(),
            query_gen: 0,
            annotations: Vec::new(),
            tab,
            selected,
            order: Vec::new(),
            scroll,
            maximized: false,
            cursor: ChartCursor::default(),
            var_nav: ListNav::default(),
            versions: None,
            ver_nav: ListNav::default(),
            ver_mark: None,
            json_scroll: Scroll::default(),
            json_lines: Vec::new(),
            exact: HashMap::new(),
            exact_view: None,
            vars_loading: false,
            stale: HashSet::new(),
            near: Vec::new(),
        });
        self.dashboards.push_recent(uid);
        self.images.clear();
        let uid = uid.to_string();
        let msg_gen = if apply_time { generation | (1 << 63) } else { generation };
        self.spawn(move |c| async move { Msg::Dashboard { generation: msg_gen, result: c.dashboard(&uid).await } });
    }

    /// Re-fetches variables that refresh on time change, then queries.
    pub fn refresh_dashboard_vars_on_time(&mut self) {
        let Some(d) = &self.dash else { return };
        if d.dash.is_none() {
            return;
        }
        let names: Vec<String> = d.vars.fetch_order().into_iter().filter(|n| d.vars.get(n).is_some_and(|v| v.def.refresh == 2)).collect();
        if names.is_empty() {
            self.query_dashboard();
        } else {
            self.resolve_vars(names, true);
        }
    }

    fn resolve_vars(&mut self, mut names: Vec<String>, then_query: bool) {
        let (from, to) = self.range();
        let Some(d) = &mut self.dash else { return };
        if names.is_empty() {
            d.vars_loading = false;
            if then_query {
                self.query_dashboard();
            }
            return;
        }
        d.vars_loading = true;
        let name = names.remove(0);
        let Some(var) = d.vars.get(&name).cloned() else {
            self.resolve_vars(names, then_query);
            return;
        };
        let scope = Scope { from, to };
        let ds = resolve_ds(var.def.datasource.as_ref(), &d.vars, &self.ds_list, scope).cloned();
        let query = d.vars.interpolate(&var.def.query, ds.as_ref().map(|d| d.kind.as_str()), scope);
        let ds_list = self.ds_list.clone();
        let generation = d.generation;
        let def = var.def.clone();
        self.spawn(move |c| async move {
            let result = variables::fetch_options(&c, &def, &query, ds.as_ref(), &ds_list, from, to).await.map(|raw| {
                if def.kind == VarKind::Datasource {
                    let opts = variables::datasource_options(&ds_list, raw);
                    let re_filtered = post_process(&def, opts.iter().map(|o| o.text.clone()).collect());
                    opts.into_iter().filter(|o| re_filtered.iter().any(|r| r.value == o.text)).collect()
                } else {
                    post_process(&def, raw)
                }
            });
            Msg::VarOptions { generation, name, rest: names, then_query, result }
        });
    }

    /// Estimated plot width (pixels) for a panel, used as maxDataPoints.
    fn panel_points(&self, panel: &Panel) -> u32 {
        let total_w = self.size.0.max(80);
        let main_w = if total_w >= 100 { total_w - side_width(total_w) } else { total_w } as u32;
        let cells = (panel.grid.w as u32 * main_w / 24).saturating_sub(12).max(10);
        let per_cell = if self.gfx.mode.is_image() { self.gfx.cell.0 as u32 } else { 2 };
        (cells * per_cell).clamp(50, 2000)
    }

    /// Marks every panel stale and queries the ones near the viewport; the rest
    /// are queried as they scroll into view (like Grafana's lazy loading).
    pub fn query_dashboard(&mut self) {
        let Some(d) = &mut self.dash else { return };
        let Some(dash) = &d.dash else { return };
        d.query_gen = next_gen();
        d.stale = d.order.iter().map(|&pi| &dash.panels[pi]).filter(|p| p.needs_query() && !p.targets.is_empty()).map(|p| p.id).collect();
        for id in &d.stale {
            if let Some(data) = d.data.get_mut(id) {
                data.loading = true;
            }
        }
        self.query_visible();
        self.load_annotations();
    }

    /// Queries stale panels that are on (or near) the screen.
    pub fn query_visible(&mut self) {
        let range = self.range();
        let max_ot = self.max_over_time;
        let Some(d) = &self.dash else { return };
        let Some(dash) = &d.dash else { return };
        if d.stale.is_empty() || d.vars_loading {
            return;
        }
        let ids: Vec<i64> = d.near.iter().copied().filter(|id| d.stale.contains(id)).collect();
        if ids.is_empty() {
            return;
        }
        let (generation, query_gen) = (d.generation, d.query_gen);
        let mut jobs = Vec::new();
        for id in &ids {
            let Some(panel) = dash.panels.iter().find(|p| p.id == *id) else { continue };
            let points = self.panel_points(panel);
            tracing::debug!(panel = id, points, size = ?self.size, "query panel");
            jobs.push((*id, panel.kind.starts_with("table"), build_panel_queries(panel, &d.vars, &self.ds_list, range, points, max_ot)));
        }
        let Some(d) = &mut self.dash else { return };
        for id in &ids {
            d.stale.remove(id);
        }
        let mut requests = Vec::new();
        for (panel_id, keep_frames, built) in jobs {
            let entry = d.data.entry(panel_id).or_default();
            match built {
                Ok(b) => {
                    entry.loading = true;
                    requests.push((
                        panel_id,
                        b.step,
                        keep_frames,
                        crate::grafana::QueryRequest { from: range.0, to: range.1, queries: b.queries },
                    ));
                }
                Err(e) if e == "no queries" => {}
                Err(e) => {
                    *entry = PanelData { errors: vec![e], range, ..Default::default() };
                }
            }
        }
        for (panel_id, step, keep_frames, req) in requests {
            self.spawn_query(move |c| async move {
                let result = c.query(&req).await.map(|r| PanelData::prepare(r, range, step, keep_frames));
                Msg::PanelData { generation, query_gen, panel_id, range, result }
            });
        }
    }

    pub fn load_annotations(&mut self) {
        let range = self.range();
        let Some(d) = &self.dash else { return };
        let Some(dash) = &d.dash else { return };
        let generation = d.generation;
        let uid = d.uid.clone();
        let scope = Scope { from: range.0, to: range.1 };
        let defs: Vec<_> = dash.annotations.iter().filter(|a| a.enabled).cloned().collect();
        let mut expr_queries = Vec::new();
        for a in defs.iter().filter(|a| !a.builtin) {
            let Some(expr) = &a.expr else { continue };
            let Some(ds) = resolve_ds(a.datasource.as_ref(), &d.vars, &self.ds_list, scope) else { continue };
            let expr = d.vars.interpolate(expr, Some(&ds.kind), scope);
            let (q, step) = super::query::adhoc_query(ds, &expr, range, 500, 100);
            expr_queries.push((
                q,
                step,
                crate::units::named_rgb(&a.color).unwrap_or((0xF2, 0x49, 0x5C)),
                a.title_format.clone().unwrap_or_else(|| a.name.clone()),
            ));
        }
        let builtin = defs.iter().find(|a| a.builtin).map(|a| crate::units::named_rgb(&a.color).unwrap_or((0x57, 0x94, 0xF2)));
        self.spawn(move |c| async move {
            let mut items = Vec::new();
            if let Some(color) = builtin
                && let Ok(list) = c.annotations(Some(&uid), range.0, range.1).await
            {
                for a in list {
                    let color = if a.new_state.as_deref() == Some("Alerting") { (0xF2, 0x49, 0x5C) } else { color };
                    items.push(ChartAnnotation { start: a.time, end: a.time_end.max(a.time), color, text: a.text });
                }
            }
            for (q, step, color, title) in expr_queries {
                let req = crate::grafana::QueryRequest { from: range.0, to: range.1, queries: vec![q] };
                let Ok(resp) = c.query(&req).await else { continue };
                for s in series_from_frames(&resp.frames()) {
                    for (start, end) in annotation_runs(&s.points, step) {
                        items.push(ChartAnnotation { start, end, color, text: title.clone() });
                    }
                }
            }
            Msg::Annotations { generation, items }
        });
    }

    fn load_versions(&mut self) {
        let Some(d) = &mut self.dash else { return };
        d.versions = None;
        let (generation, uid) = (d.generation, d.uid.clone());
        self.spawn(move |c| async move { Msg::Versions { generation, result: c.versions(&uid).await } });
    }

    // ---- messages -------------------------------------------------------------------------

    pub fn on_dash_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Search { ctx_gen, result } => {
                if ctx_gen != self.ctx_gen {
                    return;
                }
                self.dashboards.loading = false;
                match result {
                    Ok(hits) => {
                        let first_load = self.dashboards.hits.is_empty();
                        self.dashboards.hits = hits;
                        self.dashboards.error = None;
                        if first_load && self.dashboards.expanded.is_empty() {
                            // Expand top-level folders on first load so dashboards are visible.
                            let roots: Vec<String> = self
                                .dashboards
                                .hits
                                .iter()
                                .filter(|h| h.is_folder() && h.folder_uid.is_none())
                                .map(|h| h.uid.clone())
                                .collect();
                            if roots.len() <= 8 {
                                self.dashboards.expanded.extend(roots);
                            }
                        }
                        self.dashboards.rebuild();
                    }
                    Err(e) => {
                        self.dashboards.error = Some(e.to_string());
                    }
                }
            }
            Msg::Dashboard { generation, result } => {
                let apply_time = generation & (1 << 63) != 0;
                let generation = generation & !(1 << 63);
                let Some(d) = &mut self.dash else { return };
                if d.generation != generation {
                    return;
                }
                d.loading = false;
                match result {
                    Ok(resp) => {
                        let dash = Dashboard::from_json(&resp.dashboard);
                        d.order = {
                            let mut idx: Vec<usize> = (0..dash.panels.len()).filter(|i| !dash.panels[*i].is_row()).collect();
                            idx.sort_by_key(|i| {
                                let p = &dash.panels[*i];
                                // Panels from collapsed rows keep their relative order after the visible ones.
                                (p.grid.y, p.grid.x)
                            });
                            idx
                        };
                        d.selected = d.selected.min(d.order.len().saturating_sub(1));
                        d.vars = VarSet::new(&dash.variables);
                        d.json_lines = json_lines(&resp.dashboard);
                        d.meta = resp.meta;
                        let (from, to) = (dash.time_from.clone(), dash.time_to.clone());
                        d.dash = Some(dash);
                        if apply_time && let Ok(r) = TimeRange::parse(&from, &to) {
                            self.time.push(r);
                        }
                        let names = self.dash.as_ref().map(|d| d.vars.fetch_order()).unwrap_or_default();
                        self.resolve_vars(names, true);
                    }
                    Err(e) => d.error = Some(e.to_string()),
                }
            }
            Msg::VarOptions { generation, name, rest, then_query, result } => {
                let Some(d) = &mut self.dash else { return };
                if d.generation != generation {
                    return;
                }
                match result {
                    Ok(opts) => {
                        if let Some(v) = d.vars.get_mut(&name) {
                            v.set_options(opts);
                        }
                    }
                    Err(e) => {
                        if let Some(v) = d.vars.get_mut(&name) {
                            v.error = Some(e.to_string());
                            v.loaded = true;
                        }
                        self.toast_err(format!("${name}: {e}"));
                    }
                }
                self.resolve_vars(rest, then_query);
            }
            Msg::PanelData { generation, query_gen, panel_id, range, result } => {
                let Some(d) = &mut self.dash else { return };
                if d.generation != generation || d.query_gen != query_gen {
                    return;
                }
                let data = result.unwrap_or_else(|e| PanelData { errors: vec![e.to_string()], range, ..Default::default() });
                tracing::debug!(
                    panel = panel_id,
                    points = data.points,
                    series = data.series.len(),
                    per_series = data.series.first().map(|s| s.points.len()),
                    "panel data"
                );
                self.perf.points(data.points);
                d.data.insert(panel_id, data);
            }
            Msg::Annotations { generation, items } => {
                if let Some(d) = &mut self.dash
                    && d.generation == generation
                {
                    d.annotations = items;
                }
            }
            Msg::Versions { generation, result } => {
                if let Some(d) = &mut self.dash
                    && d.generation == generation
                {
                    d.versions = Some(result.map_err(|e| e.to_string()));
                }
            }
            Msg::ExactRender { generation, panel_id, result } => {
                let unicode = !self.gfx.mode.is_image();
                let Some(d) = &mut self.dash else { return };
                if d.generation != generation {
                    return;
                }
                match result {
                    Ok(png) if unicode => {
                        let path = std::env::temp_dir().join(format!("lazygrafana-{}-{panel_id}.png", d.uid));
                        let _ = std::fs::write(&path, &png);
                        self.toast(format!("terminal can't show images; saved {}", path.display()));
                    }
                    Ok(png) => {
                        d.exact.insert(panel_id, Ok(png));
                        d.exact_view = Some(panel_id);
                    }
                    Err(e) => self.toast_err(format!("exact render: {e}")),
                }
            }
            Msg::EditFetched { uid, result, purpose } => match result {
                Err(e) => self.toast_err(e.to_string()),
                Ok(resp) => match purpose {
                    FetchPurpose::Edit => {
                        self.editor = Some(EditorRequest { uid, folder_uid: resp.meta.folder_uid.clone(), json: resp.dashboard });
                    }
                    FetchPurpose::YankJson => {
                        let s = serde_json::to_string_pretty(&resp.dashboard).unwrap_or_default();
                        self.copy("dashboard JSON", &s);
                    }
                    FetchPurpose::Export => self.export_json(&uid, &resp.dashboard),
                    FetchPurpose::Move { folder_uid, folder_title } => {
                        let json = resp.dashboard;
                        self.mutate(format!("moved to {folder_title}"), After::ReloadDashboard(uid.clone()), move |c| async move {
                            c.save_dashboard(&json, Some(folder_uid.as_deref().unwrap_or("")), "Moved with lazygrafana").await.map(|_| ())
                        });
                    }
                },
            },
            _ => {}
        }
    }

    // ---- keys: side list ---------------------------------------------------------------

    pub fn dash_filter_key(&mut self, k: &KeyEvent) {
        let Some(input) = &mut self.dashboards.filter_input else { return };
        match input.handle(k) {
            InputResult::Submit => self.dashboards.filter_input = None,
            InputResult::Cancel => {
                self.dashboards.filter_input = None;
                self.dashboards.filter.clear();
                self.dashboards.rebuild();
            }
            InputResult::Changed => {
                self.dashboards.filter = input.text.clone();
                self.dashboards.nav.selected = 0;
                self.dashboards.rebuild();
            }
            InputResult::Unchanged => {}
        }
    }

    pub fn dash_side_key(&mut self, acts: &[Act]) -> bool {
        let len = self.dashboards.rows.len();
        if self.dashboards.nav.handle(acts, len, self.side_list_height()) {
            return true;
        }
        let row = self.dashboards.rows.get(self.dashboards.nav.selected).cloned();
        for a in acts {
            match (a, &row) {
                (Act::Filter, _) => {
                    // Like vim's `/`: a fresh search each time.
                    self.dashboards.filter.clear();
                    self.dashboards.filter_input = Some(TextInput::default());
                    self.dashboards.rebuild();
                }
                (Act::Back, _) if !self.dashboards.filter.is_empty() || self.dashboards.tag.is_some() => {
                    self.dashboards.filter.clear();
                    self.dashboards.tag = None;
                    self.dashboards.rebuild();
                }
                (Act::StateFilter, _) => {
                    self.dashboards.mode = match self.dashboards.mode {
                        ListMode::Tree => ListMode::Starred,
                        ListMode::Starred => ListMode::Recent,
                        ListMode::Recent => ListMode::Tree,
                    };
                    self.dashboards.nav = ListNav::default();
                    self.dashboards.rebuild();
                }
                (Act::Enter | Act::Right, Some(Row::Folder { uid, expanded, .. })) => {
                    if *expanded {
                        if *a == Act::Enter {
                            self.dashboards.expanded.remove(uid);
                        } else {
                            self.dashboards.nav.selected += 1;
                        }
                    } else {
                        self.dashboards.expanded.insert(uid.clone());
                    }
                    self.dashboards.rebuild();
                }
                (Act::Left, Some(Row::Folder { uid, expanded: true, .. })) => {
                    self.dashboards.expanded.remove(uid);
                    self.dashboards.rebuild();
                }
                (Act::Left, Some(r)) => {
                    // Jump to the parent folder.
                    let parent = match r {
                        Row::Dash { hit, .. } => self.dashboards.hits.get(*hit).and_then(|h| h.folder_uid.clone()),
                        Row::Folder { uid, .. } => self.dashboards.hits.iter().find(|h| h.uid == *uid).and_then(|h| h.folder_uid.clone()),
                    };
                    if let Some(p) = parent
                        && let Some(i) = self.dashboards.rows.iter().position(|r| matches!(r, Row::Folder { uid, .. } if *uid == p))
                    {
                        self.dashboards.nav.selected = i;
                    }
                }
                (Act::Enter | Act::Right, Some(Row::Dash { hit, .. })) => {
                    let uid = self.dashboards.hits[*hit].uid.clone();
                    if self.dash.as_ref().is_none_or(|d| d.uid != uid) {
                        self.open_dashboard(&uid, true);
                    }
                    self.focus = Focus::Main;
                }
                (_, Some(Row::Dash { hit, .. })) => {
                    let uid = self.dashboards.hits[*hit].uid.clone();
                    if !self.dashboard_action(*a, &uid) {
                        continue;
                    }
                }
                _ => continue,
            }
            return true;
        }
        false
    }

    /// Actions available on any dashboard (side list or main pane).
    fn dashboard_action(&mut self, a: Act, uid: &str) -> bool {
        let hit = self.dashboards.hit_by_uid(uid).cloned();
        match a {
            Act::OpenBrowser => {
                let url = self.dashboard_url(uid, None);
                self.open_url(&url);
            }
            Act::YankUrl => {
                let url = self.dashboard_url(uid, None);
                self.copy("URL", &url);
            }
            Act::YankUid => self.copy("UID", uid),
            Act::YankJson => self.with_dashboard_json(uid, FetchPurpose::YankJson),
            Act::Export => self.with_dashboard_json(uid, FetchPurpose::Export),
            Act::Edit => {
                if !self.readonly_guard() {
                    self.with_dashboard_json(uid, FetchPurpose::Edit);
                }
            }
            Act::Star => {
                let starred = hit.as_ref().is_some_and(|h| h.is_starred);
                let uid = uid.to_string();
                let msg = if starred { "unstarred" } else { "starred" };
                self.mutate(msg, After::ReloadDashboards, move |c| async move { c.set_star(&uid, !starred).await.map(|_| ()) });
            }
            Act::Move => {
                if self.readonly_guard() {
                    return true;
                }
                let mut items = vec![SelectItem { label: "General".into(), value: String::new(), hint: "(root)".into() }];
                let mut folders: Vec<(String, String)> = self
                    .dashboards
                    .hits
                    .iter()
                    .filter(|h| h.is_folder())
                    .map(|h| (self.dashboards.folder_path(Some(&h.uid)), h.uid.clone()))
                    .collect();
                folders.sort();
                items.extend(folders.into_iter().map(|(path, uid)| SelectItem { label: path, value: uid, hint: String::new() }));
                self.popup = Some(Popup::select(
                    "Move dashboard to folder",
                    items,
                    false,
                    BTreeSet::new(),
                    0,
                    SelectPurpose::MoveFolder { uid: uid.to_string() },
                ));
            }
            Act::Delete => {
                if self.readonly_guard() {
                    return true;
                }
                let title = hit.map(|h| h.title).unwrap_or_else(|| uid.to_string());
                self.popup = Some(Popup::confirm(
                    "Delete dashboard?",
                    vec![
                        Line::from(vec![
                            Span::raw("Delete "),
                            Span::styled(title.clone(), Style::default().add_modifier(Modifier::BOLD)),
                            Span::raw(format!(" ({uid})?")),
                        ]),
                        Line::styled(
                            "Deleted dashboards may be recoverable from Grafana's trash, depending on version.",
                            Style::default().fg(Color::DarkGray),
                        ),
                    ],
                    Pending::DeleteDashboard { uid: uid.to_string(), title },
                ));
            }
            _ => return false,
        }
        true
    }

    fn with_dashboard_json(&mut self, uid: &str, purpose: FetchPurpose) {
        let uid = uid.to_string();
        self.spawn(move |c| async move { Msg::EditFetched { result: c.dashboard(&uid).await, uid, purpose } });
    }

    fn export_json(&mut self, uid: &str, json: &Value) {
        let path = PathBuf::from(format!("{uid}.json"));
        match std::fs::write(&path, serde_json::to_string_pretty(json).unwrap_or_default()) {
            Ok(()) => self.toast(format!("exported to {}", std::fs::canonicalize(&path).unwrap_or(path).display())),
            Err(e) => self.toast_err(format!("export failed: {e}")),
        }
    }

    /// Dashboard URL with the current time range and variables when it's open.
    pub fn dashboard_url(&self, uid: &str, panel_id: Option<i64>) -> String {
        let path = self.dashboards.hit_by_uid(uid).map(|h| h.url.clone()).filter(|u| !u.is_empty()).unwrap_or_else(|| format!("/d/{uid}"));
        let mut url = format!("{}{}", self.base_url(), path);
        if let Some(d) = self.dash.as_ref().filter(|d| d.uid == uid) {
            let (from, to) = self.time.current().url_params(now_ms());
            url.push_str(&format!("?from={}&to={}", urlencoding::encode(&from), urlencoding::encode(&to)));
            let vars = d.vars.url_params();
            if !vars.is_empty() {
                url.push('&');
                url.push_str(&vars);
            }
            if let Some(p) = panel_id {
                url.push_str(&format!("&viewPanel={p}"));
            }
            if let Some(org) = self.ctx.org {
                url.push_str(&format!("&orgId={org}"));
            }
        }
        url
    }

    // ---- keys: main pane -----------------------------------------------------------------

    pub fn dash_main_key(&mut self, acts: &[Act]) -> bool {
        let Some(d) = &mut self.dash else { return false };
        for a in acts {
            match a {
                Act::NextTab | Act::PrevTab => {
                    let i = DashTab::ALL.iter().position(|t| *t == d.tab).unwrap_or(0);
                    let n = DashTab::ALL.len();
                    d.tab = DashTab::ALL[if *a == Act::NextTab { (i + 1) % n } else { (i + n - 1) % n }];
                    if d.tab == DashTab::Versions && d.versions.is_none() {
                        self.load_versions();
                    }
                    return true;
                }
                _ => {}
            }
        }
        let tab = d.tab;
        let handled = match tab {
            DashTab::Panels => self.panels_key(acts),
            DashTab::Variables => self.variables_key(acts),
            DashTab::Versions => self.versions_key(acts),
            DashTab::Json => {
                let d = self.dash.as_mut().expect("checked");
                let len = d.json_lines.len();
                d.json_scroll.handle(acts, len, self.hit.main.height as usize)
            }
        };
        if handled {
            return true;
        }
        let uid = self.dash.as_ref().map(|d| d.uid.clone()).unwrap_or_default();
        acts.iter().any(|a| self.dashboard_action(*a, &uid))
    }

    fn panels_key(&mut self, acts: &[Act]) -> bool {
        let range = self.range();
        let Some(d) = &mut self.dash else { return false };
        let n = d.order.len();
        // Exact view: Esc closes it.
        if d.exact_view.is_some() && acts.iter().any(|a| matches!(a, Act::Back | Act::ExactRender | Act::Maximize)) {
            d.exact_view = None;
            self.images.clear();
            return true;
        }
        let timestamps = d.selected_panel().and_then(|p| d.data.get(&p.id).map(crate::render::panels::timestamps)).unwrap_or_default();
        match d.cursor.handle(acts, &timestamps, range) {
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
        let d = self.dash.as_mut().expect("checked");
        for a in acts {
            match a {
                Act::Down => d.selected = (d.selected + 1).min(n.saturating_sub(1)),
                Act::Up => d.selected = d.selected.saturating_sub(1),
                Act::HalfDown => d.selected = (d.selected + 3).min(n.saturating_sub(1)),
                Act::HalfUp => d.selected = d.selected.saturating_sub(3),
                Act::Top => d.selected = 0,
                Act::Bottom => d.selected = n.saturating_sub(1),
                Act::Maximize | Act::Enter => {
                    d.maximized = !d.maximized;
                    self.images.clear();
                }
                Act::Back if d.maximized => {
                    d.maximized = false;
                    self.images.clear();
                }
                Act::ExactRender => {
                    let Some(p) = d.selected_panel() else { return true };
                    let (pid, uid, slug, generation) =
                        (p.id, d.uid.clone(), if d.meta.slug.is_empty() { "d".to_string() } else { d.meta.slug.clone() }, d.generation);
                    let vars = d.vars.url_params();
                    let (from, to) = self.time.current().url_params(now_ms());
                    let area = self.hit.main;
                    let (w, h) = (
                        (area.width as u32 * self.gfx.cell.0 as u32).max(400),
                        (area.height.saturating_sub(2) as u32 * self.gfx.cell.1 as u32).max(200),
                    );
                    self.toast("rendering panel in Grafana…");
                    self.spawn(move |c| async move {
                        Msg::ExactRender {
                            generation,
                            panel_id: pid,
                            result: c.render_panel(&uid, &slug, pid, (&from, &to), (w, h), &vars).await,
                        }
                    });
                }
                Act::MaxOverTime => {
                    self.max_over_time = !self.max_over_time;
                    self.toast(if self.max_over_time { "max_over_time on: short spikes stay visible" } else { "max_over_time off" });
                    self.query_dashboard();
                }
                Act::LogsForSelection => self.logs_for_selection(),
                Act::Annotate => {
                    let Some(p) = d.selected_panel() else { return true };
                    let range = d.cursor.selection.or(d.cursor.crosshair.map(|t| (t, t)));
                    let Some(range) = range else {
                        self.toast_err("place the crosshair (h/l) or select a range (v) first");
                        return true;
                    };
                    let purpose = InputPurpose::Annotate { dashboard_uid: Some(d.uid.clone()), panel_id: Some(p.id), range };
                    self.popup = Some(Popup::input("Annotation text", "› ", "", purpose));
                }
                Act::OpenBrowser => {
                    let (uid, pid) = (d.uid.clone(), d.selected_panel().map(|p| p.id));
                    let url = self.dashboard_url(&uid, pid);
                    self.open_url(&url);
                }
                Act::YankUrl => {
                    let (uid, pid) = (d.uid.clone(), d.selected_panel().map(|p| p.id));
                    let url = self.dashboard_url(&uid, pid);
                    self.copy("panel URL", &url);
                }
                _ => continue,
            }
            return true;
        }
        false
    }

    fn variables_key(&mut self, acts: &[Act]) -> bool {
        let Some(d) = &mut self.dash else { return false };
        let visible: Vec<String> = d.vars.vars.iter().filter(|v| v.def.hide < 2).map(|v| v.def.name.clone()).collect();
        if d.var_nav.handle(acts, visible.len(), 20) {
            return true;
        }
        if !acts.contains(&Act::Enter) {
            return false;
        }
        let Some(name) = visible.get(d.var_nav.selected).cloned() else { return true };
        let Some(v) = d.vars.get(&name) else { return true };
        match v.def.kind {
            VarKind::Textbox => {
                let cur = v.selected.first().cloned().unwrap_or_default();
                self.popup = Some(Popup::input(&format!("${name}"), "› ", &cur, InputPurpose::VarText { name }));
            }
            VarKind::Constant => self.toast("constant variables can't be changed"),
            _ => {
                let choices = v.choices();
                let checked: BTreeSet<usize> =
                    choices.iter().enumerate().filter(|(_, c)| v.selected.contains(&c.value)).map(|(i, _)| i).collect();
                let items = choices
                    .iter()
                    .map(|c| SelectItem {
                        label: c.text.clone(),
                        value: c.value.clone(),
                        hint: if c.text != c.value { c.value.clone() } else { String::new() },
                    })
                    .collect();
                let sel = checked.iter().next().copied().unwrap_or(0);
                self.popup = Some(Popup::select(
                    &format!("${name}"),
                    items,
                    v.def.multi,
                    if v.def.multi { checked } else { BTreeSet::new() },
                    sel,
                    SelectPurpose::Variable { name },
                ));
            }
        }
        true
    }

    /// Applies a new variable value and re-queries (refreshing dependent variables first).
    pub fn set_variable(&mut self, name: &str, values: Vec<String>) {
        let Some(d) = &mut self.dash else { return };
        let Some(v) = d.vars.get_mut(name) else {
            self.toast_err(format!("no variable ${name}"));
            return;
        };
        v.selected = values;
        let deps = d.vars.dependents(name);
        let ordered: Vec<String> = d.vars.fetch_order().into_iter().filter(|n| deps.contains(n)).collect();
        self.resolve_vars(ordered, true);
    }

    fn versions_key(&mut self, acts: &[Act]) -> bool {
        let Some(d) = &mut self.dash else { return false };
        let list = match &d.versions {
            Some(Ok(v)) => v.clone(),
            _ => return false,
        };
        if d.ver_nav.handle(acts, list.len(), 20) {
            return true;
        }
        let Some(sel) = list.get(d.ver_nav.selected).cloned() else { return false };
        let uid = d.uid.clone();
        for a in acts {
            match a {
                Act::Mark => {
                    d.ver_mark = if d.ver_mark == Some(d.ver_nav.selected) { None } else { Some(d.ver_nav.selected) };
                }
                Act::Diff | Act::Enter => {
                    let other = d.ver_mark.and_then(|i| list.get(i).cloned());
                    let current = d.dash.as_ref().map(|x| x.raw.clone());
                    let title = match &other {
                        Some(o) => format!("v{} → v{}", o.version.min(sel.version), o.version.max(sel.version)),
                        None => format!("v{} → current", sel.version),
                    };
                    let (sel_id, other_id) = (sel.id, other.as_ref().map(|o| (o.id, o.version)));
                    let sel_version = sel.version;
                    self.spawn(move |c| async move {
                        let result = async {
                            let a = c.version(&uid, sel_id).await?;
                            let (old, new) = match other_id {
                                Some((oid, over)) => {
                                    let b = c.version(&uid, oid).await?;
                                    if over < sel_version { (b.data, a.data) } else { (a.data, b.data) }
                                }
                                None => (a.data, current.unwrap_or(Value::Null)),
                            };
                            Ok((pretty(&strip_volatile(old)), pretty(&strip_volatile(new))))
                        }
                        .await;
                        Msg::Diff { title, result }
                    });
                }
                Act::Restore => {
                    if self.readonly_guard() {
                        return true;
                    }
                    self.popup = Some(Popup::confirm(
                        "Restore version?",
                        vec![
                            Line::raw(format!("Restore {uid} to version {} ({})?", sel.version, sel.message)),
                            Line::styled("This creates a new version.", Style::default().fg(Color::DarkGray)),
                        ],
                        Pending::RestoreVersion { uid, version: sel.version },
                    ));
                }
                _ => continue,
            }
            return true;
        }
        false
    }

    /// `gl`: jump to Loki logs for the selection (or visible range), carrying labels over.
    pub fn logs_for_selection(&mut self) {
        let range = self.range();
        let (sel, labels) = match self.side {
            Side::Dashboards => {
                let Some(d) = &self.dash else { return };
                let labels = d
                    .selected_panel()
                    .and_then(|p| d.data.get(&p.id))
                    .and_then(|data| match d.cursor.crosshair {
                        Some(t) => data
                            .series
                            .iter()
                            .filter(|s| s.value_at(t).is_some())
                            .max_by(|a, b| a.value_at(t).unwrap_or(0.0).total_cmp(&b.value_at(t).unwrap_or(0.0))),
                        None => data.series.first(),
                    })
                    .map(|s| {
                        s.labels
                            .iter()
                            .filter(|(k, _)| !k.starts_with("__") && k.as_str() != "le")
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                (d.cursor.selection, labels)
            }
            _ => (self.explore.cursor.selection, Vec::new()),
        };
        let Some(loki) = self.ds_list.iter().find(|d| d.kind == "loki").cloned() else {
            self.toast_err("no Loki datasource");
            return;
        };
        let jump = LogsJump { ds_uid: loki.uid.clone(), labels, range: sel.unwrap_or(range) };
        let uid = loki.uid.clone();
        let (from, to) = jump.range;
        self.spawn(move |c| async move {
            let result =
                c.ds_resource(&uid, &format!("labels?start={}&end={}", from as i128 * 1_000_000, to as i128 * 1_000_000)).await.map(|v| {
                    v.get("data")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                        .unwrap_or_default()
                });
            Msg::LokiLabels { result, then: jump }
        });
    }

    pub fn create_annotation(&mut self, dashboard_uid: Option<String>, panel_id: Option<i64>, range: (Millis, Millis), text: String) {
        let a = NewAnnotation { dashboard_uid, panel_id, time: range.0, time_end: range.1, tags: vec!["lazygrafana".into()], text };
        self.mutate("annotation added", After::ReloadAnnotations, move |c| async move { c.create_annotation(&a).await.map(|_| ()) });
    }

    // ---- rendering ---------------------------------------------------------------------------

    pub fn render_dash_side(&mut self, f: &mut Frame, area: Rect) {
        let (list_area, filter_area) =
            if self.dashboards.filter_input.is_some() || !self.dashboards.filter.is_empty() || self.dashboards.mode != ListMode::Tree {
                (Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area }, Some(Rect { height: 1, ..area }))
            } else {
                (area, None)
            };
        if let Some(fa) = filter_area {
            let mode = match self.dashboards.mode {
                ListMode::Tree => "",
                ListMode::Starred => "★ starred ",
                ListMode::Recent => "◷ recent ",
            };
            match &self.dashboards.filter_input {
                Some(input) => input.render(fa, f.buffer_mut(), &format!("{mode}/"), Style::default().fg(Color::Yellow), true),
                None => {
                    let text =
                        if self.dashboards.filter.is_empty() { mode.to_string() } else { format!("{mode}/{}", self.dashboards.filter) };
                    f.buffer_mut().set_string(fa.x, fa.y, truncate(&text, fa.width as usize), Style::default().fg(Color::Yellow));
                }
            }
        }
        if let Some(e) = &self.dashboards.error {
            f.render_widget(
                Paragraph::new(Line::styled(format!("⚠ {e}"), Style::default().fg(Color::Red))).wrap(ratatui::widgets::Wrap { trim: true }),
                list_area,
            );
            return;
        }
        if self.dashboards.rows.is_empty() {
            let t = if self.dashboards.loading { "loading…" } else { "no dashboards" };
            f.buffer_mut().set_string(list_area.x, list_area.y, t, Style::default().fg(Color::DarkGray));
            return;
        }
        let height = list_area.height as usize;
        let len = self.dashboards.rows.len();
        self.dashboards.nav.clamp(len, height);
        let flat = !self.dashboards.filter.is_empty() || self.dashboards.mode != ListMode::Tree || self.dashboards.tag.is_some();
        let open_uid = self.dash.as_ref().map(|d| d.uid.clone());
        for (row, i) in self.dashboards.nav.visible(len, height).enumerate() {
            let line = match &self.dashboards.rows[i] {
                Row::Folder { title, depth, expanded, count, .. } => Line::from(vec![
                    Span::raw("  ".repeat(*depth)),
                    Span::styled(if *expanded { "▾ " } else { "▸ " }, Style::default().fg(Color::Blue)),
                    Span::styled(title.clone(), Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {count}"), Style::default().fg(Color::DarkGray)),
                ]),
                Row::Dash { hit, depth } => {
                    let h = &self.dashboards.hits[*hit];
                    let open = open_uid.as_deref() == Some(h.uid.as_str());
                    let mut spans = vec![
                        Span::raw("  ".repeat(*depth)),
                        Span::styled(if open { "› " } else { "  " }, Style::default().fg(Color::Green)),
                        Span::styled(h.title.clone(), if open { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() }),
                    ];
                    if h.is_starred {
                        spans.push(Span::styled(" ★", Style::default().fg(Color::Yellow)));
                    }
                    if flat {
                        let path = self.dashboards.folder_path(h.folder_uid.as_deref());
                        if !path.is_empty() {
                            spans.push(Span::styled(format!("  {path}"), Style::default().fg(Color::DarkGray)));
                        }
                    }
                    Line::from(spans)
                }
            };
            self.list_row(f, list_area, row, i == self.dashboards.nav.selected, line);
        }
        self.hit.side_lists.retain(|(s, _, _)| *s != Side::Dashboards);
        self.hit.side_lists.push((Side::Dashboards, list_area, self.dashboards.nav.offset));
    }

    pub fn dash_main_title(&self) -> Vec<Span<'static>> {
        match &self.dash {
            None => vec![Span::raw(" Dashboard ")],
            Some(d) => {
                let title = d.dash.as_ref().map(|x| x.title.clone()).unwrap_or_else(|| d.uid.clone());
                let mut spans = vec![Span::styled(format!(" {title} "), Style::default().add_modifier(Modifier::BOLD))];
                for t in DashTab::ALL {
                    let style = if t == d.tab {
                        Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    spans.push(Span::styled(format!(" {} ", t.title()), style));
                }
                if d.meta.provisioned {
                    spans.push(Span::styled(" provisioned ", Style::default().fg(Color::DarkGray)));
                }
                spans
            }
        }
    }

    pub fn render_dash_main(&mut self, f: &mut Frame, area: Rect) {
        let Some(d) = &self.dash else {
            self.render_dash_preview(f, area);
            return;
        };
        if let Some(e) = &d.error {
            f.render_widget(Paragraph::new(Line::styled(format!("⚠ {e}"), Style::default().fg(Color::Red))), area);
            return;
        }
        if d.dash.is_none() {
            f.buffer_mut().set_string(area.x, area.y, "loading dashboard…", Style::default().fg(Color::DarkGray));
            return;
        }
        match d.tab {
            DashTab::Panels => self.render_panels(f, area),
            DashTab::Variables => self.render_variables(f, area),
            DashTab::Versions => self.render_versions(f, area),
            DashTab::Json => {
                let d = self.dash.as_mut().expect("checked");
                let max = d.json_lines.len().saturating_sub(area.height as usize);
                d.json_scroll.y = d.json_scroll.y.min(max);
                f.render_widget(Paragraph::new(d.json_lines.clone()).scroll((d.json_scroll.y as u16, d.json_scroll.x as u16)), area);
            }
        }
    }

    fn render_dash_preview(&mut self, f: &mut Frame, area: Rect) {
        let Some(h) = self.dashboards.selected_hit().cloned() else {
            let lines = vec![
                Line::styled("Select a dashboard and press Enter to open it.", Style::default().fg(Color::Gray)),
                Line::raw(""),
                Line::styled("/ filter · F starred/recent · o open in browser · ? help", Style::default().fg(Color::DarkGray)),
            ];
            f.render_widget(Paragraph::new(lines), area);
            return;
        };
        let key = |k: &str| Span::styled(format!("{k:<10}"), Style::default().fg(Color::Gray));
        let lines = vec![
            Line::styled(h.title.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Line::raw(""),
            Line::from(vec![key("Folder"), Span::raw(self.dashboards.folder_path(h.folder_uid.as_deref()))]),
            Line::from(vec![key("UID"), Span::raw(h.uid.clone())]),
            Line::from(vec![key("Tags"), Span::raw(h.tags.join(", "))]),
            Line::from(vec![key("URL"), Span::raw(format!("{}{}", self.base_url(), h.url))]),
            Line::from(vec![key("Starred"), Span::raw(if h.is_starred { "yes" } else { "no" })]),
            Line::raw(""),
            Line::styled(
                "Enter open · e edit · s star · m move · x export · dd delete · yy copy URL",
                Style::default().fg(Color::DarkGray),
            ),
        ];
        f.render_widget(Paragraph::new(lines), area);
        let url = format!("{}{}", self.base_url(), h.url);
        self.link(area, area.x + 10, area.y + 5, &url);
    }

    fn render_variables_bar(&self, d: &OpenDashboard, area: Rect, f: &mut Frame) -> u16 {
        let vars: Vec<_> = d.vars.vars.iter().filter(|v| v.def.hide < 2).collect();
        if vars.is_empty() && d.annotations.is_empty() {
            return 0;
        }
        let mut spans = Vec::new();
        for v in vars {
            if v.def.hide == 0 {
                spans.push(Span::styled(format!("{} ", v.def.display_label()), Style::default().fg(Color::Gray)));
            }
            let style = if v.error.is_some() {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            };
            spans.push(Span::styled(truncate(&v.display(), 30), style));
            spans.push(Span::raw("   "));
        }
        if d.vars_loading {
            spans.push(Span::styled("⟳ variables ", Style::default().fg(Color::DarkGray)));
        }
        if !d.annotations.is_empty() {
            spans.push(Span::styled(format!("◆ {} annotations", d.annotations.len()), Style::default().fg(Color::DarkGray)));
        }
        if self.max_over_time {
            spans.push(Span::styled("  max_over_time", Style::default().fg(Color::Magenta)));
        }
        f.buffer_mut().set_line(area.x, area.y, &Line::from(spans), area.width);
        1
    }

    fn render_panels(&mut self, f: &mut Frame, area: Rect) {
        self.hit.panels.clear();
        let focused_main = self.focus == Focus::Main;
        let images_ok = self.popup.is_none() || self.gfx.mode.supports_overlap();
        let Some(d) = self.dash.as_ref() else { return };
        let bar = self.render_variables_bar(d, area, f);
        let area = Rect { y: area.y + bar, height: area.height.saturating_sub(bar), ..area };
        let d = self.dash.as_mut().expect("checked");
        let Some(dash) = d.dash.as_ref() else { return };

        // Exact view: the Grafana-rendered PNG, full size.
        if let Some(pid) = d.exact_view
            && let Some(Ok(png)) = d.exact.get(&pid)
        {
            let title = dash.panels.iter().find(|p| p.id == pid).map(|p| p.title.clone()).unwrap_or_default();
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(format!(" {title} — exact render (Esc to close) "));
            let inner = block.inner(area);
            f.render_widget(block, area);
            if images_ok && let Some(p) = self.images.png(&format!("exact:{pid}"), &self.gfx, png, inner) {
                f.render_widget(ratatui_image::Image::new(p), inner);
            }
            return;
        }

        let boxes = panel_boxes(dash, &d.order, area.width, d.maximized.then_some(d.selected), area.height);
        // Keep the selected panel in view.
        if let Some((_, r)) = boxes.iter().find(|(i, _)| *i == d.selected) {
            let (top, bottom) = (r.y, r.y + r.height);
            if top < d.scroll {
                d.scroll = top;
            } else if bottom > d.scroll + area.height {
                d.scroll = if r.height > area.height { top } else { bottom - area.height };
            }
        }
        let scroll = d.scroll;
        // Panels within one screen of the viewport get queried first.
        d.near = boxes
            .iter()
            .filter(|(_, r)| {
                let top = r.y as i32 - scroll as i32;
                top + r.height as i32 > -(area.height as i32) && top < 2 * area.height as i32
            })
            .filter_map(|(i, _)| d.order.get(*i).and_then(|pi| dash.panels.get(*pi)).map(|p| p.id))
            .collect();
        let overlay_base = Overlay { crosshair: d.cursor.crosshair, selection: d.cursor.selection, focused: false };
        let range = self.time.current().resolve(now_ms());
        for (idx, vbox) in boxes {
            let top = vbox.y as i32 - scroll as i32;
            if top + vbox.height as i32 <= 0 || top >= area.height as i32 {
                continue;
            }
            let y0 = area.y as i32 + top.max(0);
            let y1 = (area.y as i32 + top + vbox.height as i32).min((area.y + area.height) as i32);
            let rect = Rect { x: area.x + vbox.x, y: y0 as u16, width: vbox.width, height: (y1 - y0).max(0) as u16 };
            if rect.height < 3 {
                continue;
            }
            let Some(panel) = d.panel(idx).cloned() else { continue };
            let selected = idx == d.selected;
            let placeholder;
            let data = match d.data.get(&panel.id) {
                Some(x) if x.range != (0, 0) => x,
                Some(x) => {
                    placeholder = PanelData { range, ..x.clone() };
                    &placeholder
                }
                None => {
                    placeholder = PanelData { loading: true, range, ..Default::default() };
                    &placeholder
                }
            };
            let mut title = vec![Span::styled(
                format!(" {} ", panel.title),
                Style::default().add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            )];
            if data.loading {
                title.push(Span::styled("⟳ ", Style::default().fg(Color::DarkGray)));
            }
            if !data.errors.is_empty() {
                title.push(Span::styled("⚠ ", Style::default().fg(Color::Red)));
            }
            if let (Some(step), Some(scrape)) = (data.step, Some(15_000))
                && step >= scrape * 20
                && !self.max_over_time
                && matches!(panel.kind.as_str(), "timeseries" | "graph")
            {
                title
                    .push(Span::styled(format!("step {} ", crate::timerange::format_duration(step)), Style::default().fg(Color::DarkGray)));
            }
            let border = if selected && focused_main {
                Color::Cyan
            } else if selected {
                Color::Gray
            } else {
                Color::Rgb(0x3a, 0x3d, 0x45)
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border))
                .title(Line::from(title));
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let mut ctx = PanelCtx {
                gfx: &self.gfx,
                cache: &mut self.images,
                marker: self.marker,
                overlay: Overlay { focused: selected && focused_main, ..overlay_base },
                annotations: &d.annotations,
                images: images_ok,
                key: format!("{}:{}:{}", d.uid, panel.id, inner.height),
                wrap_logs: false,
            };
            let chart = render_panel(&panel, data, inner, f.buffer_mut(), &mut ctx);
            self.hit.panels.push((idx, rect, chart));
        }
        // Scroll indicator.
        let total = panel_boxes(dash, &d.order, area.width, d.maximized.then_some(d.selected), area.height)
            .iter()
            .map(|(_, r)| r.y + r.height)
            .max()
            .unwrap_or(0);
        if total > area.height {
            let info = format!(" {}/{} ", d.selected + 1, d.order.len());
            f.buffer_mut().set_string(
                area.x + area.width.saturating_sub(info.len() as u16 + 1),
                area.y + area.height - 1,
                info,
                Style::default().fg(Color::DarkGray),
            );
        }
    }

    fn render_variables(&mut self, f: &mut Frame, area: Rect) {
        let Some(d) = &mut self.dash else { return };
        let vars: Vec<_> = d.vars.vars.iter().filter(|v| v.def.hide < 2).cloned().collect();
        if vars.is_empty() {
            f.buffer_mut().set_string(area.x, area.y, "This dashboard has no variables.", Style::default().fg(Color::DarkGray));
            return;
        }
        d.var_nav.clamp(vars.len(), area.height as usize);
        let name_w = vars.iter().map(|v| v.def.name.len()).max().unwrap_or(4) + 2;
        for (row, i) in d.var_nav.visible(vars.len(), area.height as usize).enumerate() {
            let v = &vars[i];
            let kind = match &v.def.kind {
                VarKind::Query => "query",
                VarKind::Custom => "custom",
                VarKind::Constant => "constant",
                VarKind::Textbox => "textbox",
                VarKind::Interval => "interval",
                VarKind::Datasource => "datasource",
                VarKind::AdHoc => "adhoc",
                VarKind::Other(s) => s.as_str(),
            };
            let mut spans = vec![
                Span::styled(format!("${:<w$}", v.def.name, w = name_w), Style::default().fg(Color::Cyan)),
                Span::styled(format!("{kind:<11}"), Style::default().fg(Color::DarkGray)),
                Span::styled(v.display(), Style::default().add_modifier(Modifier::BOLD)),
            ];
            if v.def.multi {
                spans.push(Span::styled("  multi", Style::default().fg(Color::DarkGray)));
            }
            if let Some(e) = &v.error {
                spans.push(Span::styled(format!("  ⚠ {e}"), Style::default().fg(Color::Red)));
            } else if v.def.needs_fetch() {
                spans.push(Span::styled(format!("  {} options", v.options.len()), Style::default().fg(Color::DarkGray)));
            }
            let sel = i == d.var_nav.selected;
            let y = area.y + row as u16;
            if sel {
                f.buffer_mut().set_style(Rect { y, height: 1, ..area }, Style::default().bg(Color::Rgb(0x2a, 0x2f, 0x3a)));
            }
            f.buffer_mut().set_line(area.x, y, &Line::from(spans), area.width);
        }
    }

    fn render_versions(&mut self, f: &mut Frame, area: Rect) {
        let Some(d) = &mut self.dash else { return };
        match &d.versions {
            None => f.buffer_mut().set_string(area.x, area.y, "loading versions…", Style::default().fg(Color::DarkGray)),
            Some(Err(e)) => f.buffer_mut().set_string(area.x, area.y, format!("⚠ {e}"), Style::default().fg(Color::Red)),
            Some(Ok(list)) => {
                let list = list.clone();
                let height = area.height.saturating_sub(2) as usize;
                d.ver_nav.clamp(list.len(), height);
                for (row, i) in d.ver_nav.visible(list.len(), height).enumerate() {
                    let v = &list[i];
                    let mark = if d.ver_mark == Some(i) { "● " } else { "  " };
                    let when = chrono::DateTime::parse_from_rfc3339(&v.created)
                        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or(v.created.clone());
                    let line = Line::from(vec![
                        Span::styled(mark, Style::default().fg(Color::Yellow)),
                        Span::styled(format!("v{:<4}", v.version), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                        Span::styled(format!("{when}  "), Style::default().fg(Color::Gray)),
                        Span::styled(format!("{:<14}", truncate(&v.created_by, 14)), Style::default().fg(Color::DarkGray)),
                        Span::raw(if v.restored_from > 0 {
                            format!("restored from v{}  {}", v.restored_from, v.message)
                        } else {
                            v.message.clone()
                        }),
                    ]);
                    let y = area.y + row as u16;
                    if i == d.ver_nav.selected {
                        f.buffer_mut().set_style(Rect { y, height: 1, ..area }, Style::default().bg(Color::Rgb(0x2a, 0x2f, 0x3a)));
                    }
                    f.buffer_mut().set_line(area.x, y, &line, area.width);
                }
                let help = "Space mark · c/Enter diff (marked ↔ selected, or selected → current) · r restore";
                f.buffer_mut().set_string(
                    area.x,
                    area.y + area.height - 1,
                    truncate(help, area.width as usize),
                    Style::default().fg(Color::DarkGray),
                );
            }
        }
    }
}

/// Contiguous runs of points become annotation regions. Prometheus omits
/// samples where a filter expression is false, so a gap longer than the step
/// also ends a run (as do explicit nulls).
pub fn annotation_runs(points: &[(Millis, Option<f64>)], step: Millis) -> Vec<(Millis, Millis)> {
    let mut out = Vec::new();
    let mut run: Option<(Millis, Millis)> = None;
    for (t, v) in points {
        match (v, run) {
            (Some(_), Some((st, last))) if *t - last <= step * 3 / 2 => run = Some((st, *t)),
            (Some(_), Some(r)) => {
                out.push(r);
                run = Some((*t, *t));
            }
            (Some(_), None) => run = Some((*t, *t)),
            (None, Some(r)) => {
                out.push(r);
                run = None;
            }
            (None, None) => {}
        }
    }
    out.extend(run);
    out
}

pub fn side_width(total: u16) -> u16 {
    (total * 28 / 100).clamp(28, 48)
}

/// Lays out panels on a virtual canvas `width` wide (y may exceed the screen).
pub fn panel_boxes(dash: &Dashboard, order: &[usize], width: u16, maximized: Option<usize>, height: u16) -> Vec<(usize, Rect)> {
    let scale = row_scale(height);
    if let Some(i) = maximized {
        return vec![(i, Rect { x: 0, y: 0, width, height })];
    }
    let narrow = width < 80;
    let mut out = Vec::new();
    let mut y_cursor = 0u16;
    for (i, &pi) in order.iter().enumerate() {
        let p = &dash.panels[pi];
        if narrow {
            let h = ((p.grid.h as f32 * scale) as u16).max(6);
            out.push((i, Rect { x: 0, y: y_cursor, width, height: h }));
            y_cursor += h;
            continue;
        }
        let x0 = (p.grid.x as u32 * width as u32 / 24) as u16;
        let x1 = (((p.grid.x + p.grid.w).min(24)) as u32 * width as u32 / 24) as u16;
        let y0 = (p.grid.y as f32 * scale).round() as u16;
        let y1 = ((p.grid.y + p.grid.h) as f32 * scale).round() as u16;
        out.push((i, Rect { x: x0, y: y0, width: x1.saturating_sub(x0).max(8), height: y1.saturating_sub(y0).max(4) }));
    }
    out
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Drops fields that change on every save so diffs show real changes.
fn strip_volatile(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("version");
        o.remove("id");
    }
    v
}

/// Syntax-highlighted pretty JSON.
pub fn json_lines(v: &Value) -> Vec<Line<'static>> {
    let key_re = regex::Regex::new(r#"^(\s*)("(?:[^"\\]|\\.)*")(:\s*)(.*)$"#).expect("valid regex");
    pretty(v)
        .lines()
        .map(|l| {
            let value_span = |s: &str| -> Span<'static> {
                let t = s.trim_end_matches(',');
                let color = if t.starts_with('"') {
                    Color::Green
                } else if t == "true" || t == "false" || t == "null" {
                    Color::Magenta
                } else if t.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-') {
                    Color::Yellow
                } else {
                    Color::Gray
                };
                Span::styled(s.to_string(), Style::default().fg(color))
            };
            match key_re.captures(l) {
                Some(c) => Line::from(vec![
                    Span::raw(c[1].to_string()),
                    Span::styled(c[2].to_string(), Style::default().fg(Color::Cyan)),
                    Span::raw(c[3].to_string()),
                    value_span(&c[4]),
                ]),
                None => Line::from(value_span(l)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits() -> Vec<SearchHit> {
        serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/search.json")).unwrap()).unwrap()
    }

    #[test]
    fn tree_and_filter() {
        let mut s = DashboardsState::new(None);
        s.hits = hits();
        s.rebuild();
        // Collapsed: only root folders + General dashboards.
        assert!(s.rows.iter().all(|r| matches!(r, Row::Folder { depth: 0, .. }) || matches!(r, Row::Dash { depth: 0, .. })));
        s.expanded.insert("lg-teams".into());
        s.expanded.insert("lg-team-checkout".into());
        s.expanded.insert("lg-team-checkout-oncall".into());
        s.rebuild();
        let scratch = s.rows.iter().find_map(|r| match r {
            Row::Dash { hit, depth } if s.hits[*hit].uid == "lg-scratch" => Some(*depth),
            _ => None,
        });
        assert_eq!(scratch, Some(3));
        assert_eq!(s.folder_path(Some("lg-team-checkout-oncall")), "Teams/Checkout/On-call");
        s.filter = "node".into();
        s.rebuild();
        assert_eq!(s.rows.len(), 1);
        assert_eq!(s.selected_hit().unwrap().uid, "lg-node-overview");
    }

    #[test]
    fn layout_boxes() {
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dashboard_demo.json")).unwrap(),
        )
        .unwrap();
        let d = Dashboard::from_json(&v["dashboard"]);
        let order: Vec<usize> = (0..d.panels.len()).collect();
        let boxes = panel_boxes(&d, &order, 120, None, 40);
        assert_eq!(boxes.len(), d.panels.len());
        assert!(boxes.iter().all(|(_, r)| r.x + r.width <= 120 + 8));
        let narrow = panel_boxes(&d, &order, 60, None, 40);
        assert!(narrow.windows(2).all(|w| w[1].1.y >= w[0].1.y + w[0].1.height));
        let max = panel_boxes(&d, &order, 120, Some(2), 40);
        assert_eq!(max, vec![(2, Rect { x: 0, y: 0, width: 120, height: 40 })]);
    }

    #[test]
    fn annotation_regions_break_on_gaps() {
        let pts = |ts: &[i64]| ts.iter().map(|t| (*t, Some(1.0))).collect::<Vec<_>>();
        assert_eq!(annotation_runs(&pts(&[0, 30, 60, 900, 930]), 30), vec![(0, 60), (900, 930)]);
        assert_eq!(annotation_runs(&[(0, Some(1.0)), (30, None), (60, Some(1.0))], 30), vec![(0, 0), (60, 60)]);
        assert!(annotation_runs(&[], 30).is_empty());
    }

    #[test]
    fn json_highlighting() {
        let lines = json_lines(&serde_json::json!({"a": 1, "b": "x"}));
        assert!(lines.len() >= 4);
    }
}
