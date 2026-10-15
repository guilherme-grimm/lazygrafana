//! The interactive application: state, event loop, background tasks.

pub mod alerts;
pub mod chartnav;
pub mod contexts;
pub mod dashboards;
pub mod datasources;
pub mod explore;
pub mod keymap;
pub mod msg;
pub mod perf;
pub mod popup;
pub mod query;
pub mod ui;
pub mod widgets;

use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::eyre::Result;
use crossterm::event::{Event, EventStream, KeyEvent, KeyEventKind, MouseEvent};
use futures::StreamExt;
use ratatui::symbols::Marker;
use tokio::sync::{Semaphore, mpsc};

use crate::config::{Config, Context, UnicodeMarker};
use crate::grafana::models::Datasource;
use crate::grafana::{ApiError, Client};
use crate::render::image::{GraphicsState, ImageCache};
use crate::timerange::{Jumplist, Millis, TimeRange, now_ms, parse_duration};
use keymap::{Act, Keymap, Lookup};
use msg::{After, InstanceInfo, Msg};
use popup::Popup;

pub type Term = ratatui::Terminal<ratatui::backend::CrosstermBackend<perf::CountingStdout>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Contexts,
    Dashboards,
    Alerts,
    Explore,
    Datasources,
}

impl Side {
    pub const ALL: [Side; 5] = [Side::Contexts, Side::Dashboards, Side::Alerts, Side::Explore, Side::Datasources];

    pub fn title(self) -> &'static str {
        match self {
            Side::Contexts => "Contexts",
            Side::Dashboards => "Dashboards",
            Side::Alerts => "Alerts",
            Side::Explore => "Explore",
            Side::Datasources => "Datasources",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Side,
    Main,
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

/// A drawn time series: its layout (for pixel ↔ time mapping) and data.
pub type DrawnChart = (crate::render::chart::ChartLayout, std::rc::Rc<crate::render::chart::ChartSpec>);

/// Screen regions from the last frame, for mouse hit-testing.
#[derive(Default)]
pub struct HitMap {
    pub side: Vec<(Side, ratatui::layout::Rect)>,
    pub side_lists: Vec<(Side, ratatui::layout::Rect, usize)>,
    pub main: ratatui::layout::Rect,
    /// (panel index, panel box, chart layout + spec for time series)
    pub panels: Vec<(usize, ratatui::layout::Rect, Option<DrawnChart>)>,
    pub explore_chart: Option<DrawnChart>,
    /// OSC 8 hyperlinks to emit after the frame: (x, y, visible text, url).
    pub links: Vec<(u16, u16, String, String)>,
}

pub struct EditorRequest {
    pub uid: String,
    pub folder_uid: Option<String>,
    pub json: serde_json::Value,
}

pub struct App {
    pub config: Config,
    pub keymap: Keymap,
    pub ctx: Context,
    pub client: Option<Client>,
    /// Bumped on context switch; results tagged with an older value are dropped.
    pub ctx_gen: u64,
    pub tx: mpsc::UnboundedSender<Msg>,
    pub query_limit: Arc<Semaphore>,
    pub side: Side,
    pub focus: Focus,
    pub time: Jumplist,
    pub refresh: Option<Duration>,
    pub last_refresh: Instant,
    pub pending_keys: Vec<String>,
    pub pending_at: Instant,
    pub popup: Option<Popup>,
    pub toast: Option<Toast>,
    pub quit: bool,
    pub gfx: GraphicsState,
    pub images: ImageCache,
    pub marker: Marker,
    pub instance: Option<InstanceInfo>,
    pub instance_error: Option<String>,
    pub ds_list: Vec<Datasource>,
    pub contexts: contexts::ContextsState,
    pub dashboards: dashboards::DashboardsState,
    pub dash: Option<dashboards::OpenDashboard>,
    pub alerts: alerts::AlertsState,
    pub explore: explore::ExploreState,
    pub datasources: datasources::DatasourcesState,
    pub hit: HitMap,
    pub editor: Option<EditorRequest>,
    pub state_dir: Option<PathBuf>,
    /// Debounced re-query (after zoom/pan/variable changes).
    pub requery_at: Option<Instant>,
    pub max_over_time: bool,
    pub drag_anchor: Option<Millis>,
    pub size: (u16, u16),
    pub perf: perf::Perf,
}

impl App {
    pub fn new(config: Config, ctx_name: &str, gfx: GraphicsState, tx: mpsc::UnboundedSender<Msg>) -> Result<Self> {
        let ctx = config.resolve_context(ctx_name)?;
        let keymap = Keymap::new(&config.keys);
        let time = TimeRange::parse_input(&config.default_time).unwrap_or_else(|_| TimeRange::last(6 * 3_600_000));
        let refresh = parse_refresh(&config.refresh);
        let marker = match config.unicode_marker {
            UnicodeMarker::Braille => Marker::Braille,
            UnicodeMarker::Octant => Marker::Octant,
            UnicodeMarker::Block => Marker::HalfBlock,
        };
        let state_dir = crate::config::state_dir();
        let mut app = Self {
            contexts: contexts::ContextsState::new(&config),
            dashboards: dashboards::DashboardsState::new(state_dir.as_deref()),
            explore: explore::ExploreState::new(state_dir.as_deref()),
            config,
            keymap,
            client: None,
            ctx,
            ctx_gen: 0,
            tx,
            query_limit: Arc::new(Semaphore::new(8)),
            side: Side::Dashboards,
            focus: Focus::Side,
            time: Jumplist::new(time),
            refresh,
            last_refresh: Instant::now(),
            pending_keys: Vec::new(),
            pending_at: Instant::now(),
            popup: None,
            toast: None,
            quit: false,
            gfx,
            images: ImageCache::default(),
            marker,
            instance: None,
            instance_error: None,
            ds_list: Vec::new(),
            dash: None,
            alerts: alerts::AlertsState::default(),
            datasources: datasources::DatasourcesState::default(),
            hit: HitMap::default(),
            editor: None,
            state_dir,
            requery_at: None,
            max_over_time: false,
            drag_anchor: None,
            size: (0, 0),
            perf: perf::Perf::default(),
        };
        app.contexts.select_name(ctx_name);
        for w in app.keymap.warnings.clone() {
            app.toast_err(w);
        }
        app.connect();
        Ok(app)
    }

    // ---- context ----------------------------------------------------------------

    /// (Re)creates the client for the current context and loads everything.
    pub fn connect(&mut self) {
        self.ctx_gen += 1;
        self.dash = None;
        self.dashboards.reset();
        self.alerts = alerts::AlertsState::default();
        self.explore.reset_results();
        self.datasources = datasources::DatasourcesState::default();
        self.ds_list.clear();
        self.instance = None;
        self.instance_error = None;
        self.images.clear();
        if let Some(e) = &self.ctx.token_error {
            self.toast_err(format!("context '{}': {e}", self.ctx.name));
        }
        match Client::new(&self.ctx) {
            Ok(c) => {
                self.client = Some(c);
                self.load_instance();
                self.load_search();
                self.load_datasources();
                self.load_alerts();
            }
            Err(e) => {
                self.client = None;
                self.instance_error = Some(e.to_string());
                self.toast_err(e.to_string());
            }
        }
    }

    pub fn switch_context(&mut self, name: &str) {
        match self.config.resolve_context(name) {
            Ok(ctx) => {
                self.ctx = ctx;
                self.contexts.select_name(name);
                self.connect();
                self.toast(format!("switched to {name}"));
            }
            Err(e) => self.toast_err(e.to_string()),
        }
    }

    fn load_instance(&mut self) {
        let ctx = self.ctx.name.clone();
        self.spawn(move |c| async move {
            let result = async {
                let health = c.health().await?;
                let settings = c.frontend_settings().await.ok();
                let user = c.user().await?;
                Ok(InstanceInfo {
                    version: if health.version.is_empty() {
                        settings.as_ref().map(|s| s.build_info.version.clone()).unwrap_or_default()
                    } else {
                        health.version
                    },
                    edition: settings.as_ref().map(|s| s.build_info.edition.clone()).unwrap_or_default(),
                    user: Some(user),
                    app_url: settings.map(|s| s.app_url).unwrap_or_default(),
                })
            }
            .await;
            Msg::Instance { ctx, result }
        });
    }

    pub fn load_datasources(&mut self) {
        let ctx_gen = self.ctx_gen;
        self.spawn(move |c| async move { Msg::Datasources { ctx_gen, result: c.datasources().await } });
    }

    // ---- tasks --------------------------------------------------------------------

    /// Runs `f` with a clone of the client in the background; its message is
    /// delivered to the UI loop. No-op without a client.
    pub fn spawn<F, Fut>(&self, f: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Msg> + Send + 'static,
    {
        let Some(client) = self.client.clone() else { return };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(f(client).await);
        });
    }

    /// Like `spawn`, but bounded by the query concurrency limit.
    pub fn spawn_query<F, Fut>(&self, f: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Msg> + Send + 'static,
    {
        let Some(client) = self.client.clone() else { return };
        let tx = self.tx.clone();
        let sem = self.query_limit.clone();
        let in_flight = self.perf.in_flight.clone();
        in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn(async move {
            let _permit = sem.acquire_owned().await;
            let started = Instant::now();
            let msg = f(client).await;
            in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            let _ = tx.send(Msg::QueryTimed(started.elapsed()));
            let _ = tx.send(msg);
        });
    }

    /// Runs a mutating call, then shows `ok` and performs `after`.
    pub fn mutate<F, Fut>(&mut self, ok: impl Into<String>, after: After, f: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), ApiError>> + Send + 'static,
    {
        if self.readonly_guard() {
            return;
        }
        let ok = ok.into();
        self.spawn(move |c| async move { Msg::Done { ok, after, result: f(c).await } });
    }

    pub fn readonly_guard(&mut self) -> bool {
        if self.ctx.readonly {
            self.toast_err(format!("context '{}' is read-only", self.ctx.name));
            return true;
        }
        false
    }

    // ---- feedback -----------------------------------------------------------------

    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), error: false, at: Instant::now() });
    }

    pub fn toast_err(&mut self, text: impl Into<String>) {
        let text = text.into();
        tracing::warn!("{text}");
        self.toast = Some(Toast { text, error: true, at: Instant::now() });
    }

    pub fn copy(&mut self, what: &str, text: &str) {
        if crate::term::copy(text) {
            self.toast(format!("copied {what}"));
        } else {
            self.toast_err(format!("could not copy {what}"));
        }
    }

    pub fn open_url(&mut self, url: &str) {
        match open::that_detached(url) {
            Ok(()) => self.toast(format!("opened {url}")),
            Err(e) => self.toast_err(format!("cannot open browser: {e}")),
        }
    }

    pub fn base_url(&self) -> String {
        // The configured URL is what the user can reach; Grafana's appUrl is often an internal address.
        self.ctx.url.clone()
    }

    // ---- time ---------------------------------------------------------------------

    pub fn range(&self) -> (Millis, Millis) {
        self.time.current().resolve(now_ms())
    }

    pub fn set_time(&mut self, r: TimeRange) {
        self.time.push(r);
        self.on_time_changed();
    }

    pub fn on_time_changed(&mut self) {
        if let Some(d) = &mut self.dash {
            d.cursor.clear();
        }
        self.explore.cursor.clear();
        self.requery_at = Some(Instant::now() + Duration::from_millis(150));
    }

    /// Runs debounced queries for whatever is visible.
    fn requery(&mut self) {
        self.requery_at = None;
        self.refresh_dashboard_vars_on_time();
        self.rerun_explore();
        self.alerts.chart.clear();
        self.ensure_rule_chart();
    }

    // ---- event loop -----------------------------------------------------------------

    pub async fn run(mut self, terminal: &mut Term, mut rx: mpsc::UnboundedReceiver<Msg>) -> Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut dirty = true;
        loop {
            if dirty {
                self.images.begin_frame();
                // Synchronized output: the terminal shows the frame atomically (no tearing).
                let started = Instant::now();
                crossterm::queue!(terminal.backend_mut(), crossterm::terminal::BeginSynchronizedUpdate)?;
                terminal.draw(|f| self.render(f))?;
                self.emit_links()?;
                crossterm::execute!(terminal.backend_mut(), crossterm::terminal::EndSynchronizedUpdate)?;
                self.perf.frame(started.elapsed());
                for d in self.images.timings.drain(..) {
                    self.perf.encode(d);
                }
                self.images.sweep();
                self.query_visible();
                dirty = false;
            }
            tokio::select! {
                ev = events.next() => {
                    match ev {
                        Some(Ok(ev)) => { self.on_event(ev); dirty = true; }
                        Some(Err(e)) => { tracing::warn!("event error: {e}"); }
                        None => break,
                    }
                }
                Some(msg) = rx.recv() => {
                    self.on_msg(msg);
                    // Drain whatever else is ready before redrawing.
                    while let Ok(m) = rx.try_recv() { self.on_msg(m); }
                    dirty = true;
                }
                _ = tick.tick() => { dirty |= self.on_tick(); }
            }
            if let Some(req) = self.editor.take() {
                self.run_editor(terminal, req)?;
                dirty = true;
            }
            if self.quit {
                break;
            }
        }
        tracing::info!("session stats:\n{}", self.perf.report().join("\n"));
        Ok(())
    }

    fn on_tick(&mut self) -> bool {
        let mut changed = false;
        if let Some(t) = &self.toast
            && t.at.elapsed() > Duration::from_secs(if t.error { 6 } else { 3 })
        {
            self.toast = None;
            changed = true;
        }
        if !self.pending_keys.is_empty() && self.pending_at.elapsed() > Duration::from_millis(1200) {
            self.pending_keys.clear();
            changed = true;
        }
        if self.requery_at.is_some_and(|t| Instant::now() >= t) {
            self.requery();
            changed = true;
        }
        if let Some(every) = self.refresh
            && self.last_refresh.elapsed() >= every
            && self.time.current().is_relative()
        {
            self.last_refresh = Instant::now();
            self.requery();
            changed = true;
        }
        changed |= self.explore_tick();
        changed |= self.alerts_tick();
        changed
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) if self.config.mouse => self.on_mouse(m),
            Event::Paste(s) => self.on_paste(&s),
            Event::Resize(w, h) => {
                self.size = (w, h);
                self.images.clear();
            }
            _ => {}
        }
    }

    fn on_paste(&mut self, s: &str) {
        if let Some(p) = &mut self.popup {
            p.paste(s);
        } else if self.explore.editing {
            self.explore.input.insert_str(s);
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        // Text inputs get raw keys first.
        if let Some(mut p) = self.popup.take() {
            let outcome = p.handle_raw(&k);
            self.popup = Some(p);
            self.on_popup_outcome(outcome);
            return;
        } else if self.raw_input_active() {
            self.handle_raw_input(&k);
            return;
        }
        let Some(tok) = keymap::token(&k) else { return };
        self.pending_keys.push(tok);
        self.pending_at = Instant::now();
        let acts = match self.keymap.lookup(&self.pending_keys) {
            Lookup::Pending => return,
            Lookup::Acts(a) => a,
            Lookup::None => {
                // A dead prefix: retry with just the last key.
                let last = self.pending_keys.pop().expect("pushed");
                self.pending_keys.clear();
                match self.keymap.lookup(std::slice::from_ref(&last)) {
                    Lookup::Acts(a) => a,
                    Lookup::Pending => {
                        self.pending_keys.push(last);
                        return;
                    }
                    Lookup::None => return,
                }
            }
        };
        self.pending_keys.clear();
        self.dispatch(&acts);
    }

    /// Keys for the focused view first, then global bindings.
    fn dispatch(&mut self, acts: &[Act]) {
        let handled = match (self.focus, self.side) {
            (Focus::Side, Side::Contexts) => self.contexts_side_key(acts),
            (Focus::Side, Side::Dashboards) => self.dash_side_key(acts),
            (Focus::Side, Side::Alerts) => self.alerts_side_key(acts),
            (Focus::Side, Side::Explore) => self.explore_side_key(acts),
            (Focus::Side, Side::Datasources) => self.datasources_side_key(acts),
            (Focus::Main, Side::Contexts) => self.contexts_main_key(acts),
            (Focus::Main, Side::Dashboards) => self.dash_main_key(acts),
            (Focus::Main, Side::Alerts) => self.alerts_main_key(acts),
            (Focus::Main, Side::Explore) => self.explore_main_key(acts),
            (Focus::Main, Side::Datasources) => self.datasources_main_key(acts),
        };
        if !handled {
            self.global_key(acts);
        }
    }

    fn global_key(&mut self, acts: &[Act]) {
        for a in acts {
            match a {
                Act::Quit => self.quit = true,
                Act::Help => self.popup = Some(Popup::help()),
                Act::Command => self.popup = Some(Popup::input("Command", ":", "", popup::InputPurpose::Command)),
                Act::Focus1 => self.focus_side(Side::Contexts),
                Act::Focus2 => self.focus_side(Side::Dashboards),
                Act::Focus3 => self.focus_side(Side::Alerts),
                Act::Focus4 => self.focus_side(Side::Explore),
                Act::Focus5 => self.focus_side(Side::Datasources),
                Act::NextPanel => {
                    if self.focus == Focus::Side {
                        self.focus = Focus::Main;
                    } else {
                        self.focus_side(Side::ALL[(self.side.index() + 1) % 5]);
                    }
                }
                Act::PrevPanel => {
                    if self.focus == Focus::Main {
                        self.focus = Focus::Side;
                    } else {
                        self.focus_side(Side::ALL[(self.side.index() + 4) % 5]);
                    }
                }
                Act::Right | Act::Enter if self.focus == Focus::Side => self.focus = Focus::Main,
                Act::Left | Act::Back if self.focus == Focus::Main => self.focus = Focus::Side,
                Act::ContextSwitch => self.popup = Some(Popup::context_picker(&self.config.context_names(), &self.ctx.name)),
                Act::TimePicker => self.popup = Some(Popup::time_picker(self.time.current())),
                Act::RefreshPicker => self.popup = Some(Popup::refresh_picker(self.refresh)),
                Act::JumpBack => {
                    if self.time.back() {
                        self.on_time_changed();
                        self.toast(format!("time {} ({}/{})", self.time.current(), self.time.position().0, self.time.position().1));
                    }
                }
                Act::JumpForward => {
                    if self.time.forward() {
                        self.on_time_changed();
                        self.toast(format!("time {} ({}/{})", self.time.current(), self.time.position().0, self.time.position().1));
                    }
                }
                Act::ZoomOut => {
                    let r = self.time.current().zoom(now_ms(), None, 2.0);
                    self.set_time(r);
                }
                Act::ZoomIn => {
                    let r = self.time.current().zoom(now_ms(), None, 0.5);
                    self.set_time(r);
                }
                Act::PanLeft => {
                    let r = self.time.current().pan(now_ms(), -0.5);
                    self.set_time(r);
                }
                Act::PanRight => {
                    let r = self.time.current().pan(now_ms(), 0.5);
                    self.set_time(r);
                }
                Act::YankRange => {
                    let (a, b) = self.time.current().url_params(now_ms());
                    self.copy("time range", &format!("from={a}&to={b}"));
                }
                Act::Refresh => {
                    self.requery();
                    self.load_search();
                    self.load_alerts();
                    self.toast("refreshed");
                }
                _ => continue,
            }
            return;
        }
    }

    pub fn focus_side(&mut self, side: Side) {
        self.side = side;
        self.focus = Focus::Side;
        if side == Side::Alerts {
            self.ensure_rule_chart();
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        let pos = ratatui::layout::Position { x: m.column, y: m.row };
        if self.popup.is_some() {
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((side, _)) = self.hit.side.iter().find(|(_, r)| r.contains(pos)).copied() {
                    self.side = side;
                    self.focus = Focus::Side;
                    if let Some((_, list, offset)) = self.hit.side_lists.iter().find(|(s, r, _)| *s == side && r.contains(pos)).copied() {
                        self.side_click(side, offset + (pos.y - list.y) as usize);
                    }
                    return;
                }
                if self.hit.main.contains(pos) {
                    self.focus = Focus::Main;
                    self.main_click(pos);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => self.main_drag(pos),
            MouseEventKind::Up(MouseButton::Left) => self.main_release(pos),
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = matches!(m.kind, MouseEventKind::ScrollDown);
                if self.hit.side.iter().any(|(_, r)| r.contains(pos)) {
                    self.dispatch(&[if down { Act::Down } else { Act::Up }]);
                } else if self.main_chart_at(pos).is_some() {
                    let center = self.main_chart_at(pos);
                    let r = self.time.current().zoom(now_ms(), center, if down { 1.5 } else { 1.0 / 1.5 });
                    self.set_time(r);
                } else if self.hit.main.contains(pos) {
                    let prev = self.focus;
                    self.focus = Focus::Main;
                    self.dispatch(&[if down { Act::Down } else { Act::Up }]);
                    self.focus = prev;
                }
            }
            _ => {}
        }
    }

    /// The time under the mouse, if it's over a chart in the main pane.
    fn main_chart_at(&self, pos: ratatui::layout::Position) -> Option<Millis> {
        let charts = self.hit.panels.iter().filter_map(|(_, _, c)| c.as_ref()).chain(self.hit.explore_chart.as_ref());
        for (layout, spec) in charts {
            if layout.plot.contains(pos) {
                return Some(layout.col_to_time(spec, pos.x));
            }
        }
        None
    }

    fn side_click(&mut self, side: Side, row: usize) {
        match side {
            Side::Contexts => self.contexts.nav.selected = row,
            Side::Dashboards => self.dashboards.nav.selected = row,
            Side::Alerts => {
                self.alerts.nav.selected = row;
                self.ensure_rule_chart();
            }
            Side::Explore => self.explore.hist_nav.selected = row,
            Side::Datasources => self.datasources.nav.selected = row,
        }
    }

    fn main_click(&mut self, pos: ratatui::layout::Position) {
        if self.side == Side::Dashboards
            && let Some((idx, _, _)) = self.hit.panels.iter().find(|(_, r, _)| r.contains(pos))
            && let Some(d) = &mut self.dash
        {
            d.selected = *idx;
        }
        if let Some(t) = self.main_chart_at(pos) {
            self.drag_anchor = Some(t);
            let cursor = self.active_cursor();
            if let Some(c) = cursor {
                c.crosshair = Some(t);
                c.selection = None;
                c.anchor = None;
            }
        }
    }

    fn main_drag(&mut self, pos: ratatui::layout::Position) {
        let Some(anchor) = self.drag_anchor else { return };
        let Some(t) = self.main_chart_at(pos) else { return };
        if let Some(c) = self.active_cursor() {
            c.crosshair = Some(t);
            c.selection = Some((anchor.min(t), anchor.max(t)));
        }
    }

    fn main_release(&mut self, _pos: ratatui::layout::Position) {
        if self.drag_anchor.take().is_none() {
            return;
        }
        let sel = self.active_cursor().and_then(|c| c.selection);
        if let Some((a, b)) = sel
            && b - a > 1000
        {
            if let Some(c) = self.active_cursor() {
                c.clear();
            }
            self.set_time(TimeRange::absolute(a, b));
        }
    }

    pub fn active_cursor(&mut self) -> Option<&mut chartnav::ChartCursor> {
        match self.side {
            Side::Dashboards => self.dash.as_mut().map(|d| &mut d.cursor),
            Side::Explore => Some(&mut self.explore.cursor),
            _ => None,
        }
    }

    fn raw_input_active(&self) -> bool {
        (self.side == Side::Explore && self.focus == Focus::Main && (self.explore.editing || self.explore.log_filter_input.is_some()))
            || (self.side == Side::Dashboards && self.dashboards.filter_input.is_some())
            || (self.side == Side::Alerts && self.alerts.filter_input.is_some())
    }

    fn handle_raw_input(&mut self, k: &KeyEvent) {
        match self.side {
            Side::Explore => self.explore_raw_key(k),
            Side::Dashboards => self.dash_filter_key(k),
            Side::Alerts => self.alerts_filter_key(k),
            _ => {}
        }
    }

    // ---- messages -------------------------------------------------------------------

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Instance { ctx, result } => {
                if ctx != self.ctx.name {
                    return;
                }
                match result {
                    Ok(i) => {
                        self.contexts.statuses.insert(ctx, Ok(i.version.clone()));
                        self.instance = Some(i);
                        self.instance_error = None;
                    }
                    Err(e) => {
                        self.contexts.statuses.insert(ctx, Err(e.to_string()));
                        self.instance_error = Some(e.to_string());
                        self.toast_err(format!("{}: {e}", self.ctx.name));
                    }
                }
            }
            Msg::ContextHealth { ctx, result } => {
                self.contexts.statuses.insert(ctx, result);
            }
            Msg::Datasources { ctx_gen, result } => {
                if ctx_gen != self.ctx_gen {
                    return;
                }
                match result {
                    Ok(list) => {
                        self.ds_list = list;
                        self.explore.ensure_datasource(&self.ds_list);
                    }
                    Err(e) => self.toast_err(format!("datasources: {e}")),
                }
            }
            Msg::QueryTimed(took) => self.perf.query(took),
            Msg::Done { ok, after, result } => match result {
                Ok(()) => {
                    self.toast(ok);
                    self.after(after);
                }
                Err(e) => self.toast_err(e.to_string()),
            },
            other => self.on_view_msg(other),
        }
    }

    fn on_view_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Search { .. }
            | Msg::Dashboard { .. }
            | Msg::VarOptions { .. }
            | Msg::PanelData { .. }
            | Msg::Annotations { .. }
            | Msg::Versions { .. } => self.on_dash_msg(msg),
            Msg::Diff { title, result } => match result {
                Ok((a, b)) => self.popup = Some(Popup::diff(&title, &a, &b)),
                Err(e) => self.toast_err(e.to_string()),
            },
            Msg::ExactRender { .. } | Msg::EditFetched { .. } => self.on_dash_msg(msg),
            Msg::Rules { .. } | Msg::Silences { .. } | Msg::AmAlerts { .. } | Msg::RuleHistory { .. } | Msg::RuleChart { .. } => {
                self.on_alerts_msg(msg)
            }
            Msg::Explore { .. } | Msg::LokiLabels { .. } => self.on_explore_msg(msg),
            Msg::DsHealth { uid, result } => {
                self.datasources.checking.remove(&uid);
                self.datasources.health.insert(uid, result.map_err(|e| e.to_string()));
            }
            _ => {}
        }
    }

    fn after(&mut self, after: After) {
        match after {
            After::Nothing => {}
            After::ReloadDashboards => self.load_search(),
            After::ReloadDashboard(uid) => {
                self.load_search();
                if self.dash.as_ref().is_some_and(|d| d.uid == uid) {
                    self.open_dashboard(&uid, false);
                }
            }
            After::CloseDashboard => {
                self.dash = None;
                self.load_search();
            }
            After::ReloadAlerts => self.load_alerts(),
            After::ReloadAnnotations => self.load_annotations(),
        }
    }

    /// Re-prints registered URLs wrapped in OSC 8 so they are clickable. The visible
    /// text is identical to what ratatui drew, so its buffer stays in sync.
    fn emit_links(&mut self) -> std::io::Result<()> {
        if self.popup.is_some() || self.hit.links.is_empty() {
            return Ok(());
        }
        let mut out = std::io::stdout();
        for (x, y, text, url) in &self.hit.links {
            let url: String = url.chars().filter(|c| !c.is_control()).collect();
            crossterm::queue!(out, crossterm::cursor::MoveTo(*x, *y))?;
            write!(out, "\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")?;
        }
        out.flush()
    }

    // ---- external editor ----------------------------------------------------------------

    fn run_editor(&mut self, terminal: &mut Term, req: EditorRequest) -> Result<()> {
        let original = serde_json::to_string_pretty(&req.json)?;
        let mut file = tempfile::Builder::new().prefix(&format!("lazygrafana-{}-", req.uid)).suffix(".json").tempfile()?;
        file.write_all(original.as_bytes())?;
        file.flush()?;
        let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
        crate::term::leave()?;
        let status = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(file.path()).status();
        crate::term::enter(self.config.mouse)?;
        terminal.clear()?;
        self.images.clear();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                self.toast_err(format!("editor exited with {s}"));
                return Ok(());
            }
            Err(e) => {
                self.toast_err(format!("cannot run editor '{editor}': {e}"));
                return Ok(());
            }
        }
        let edited = std::fs::read_to_string(file.path())?;
        let value: serde_json::Value = match serde_json::from_str(&edited) {
            Ok(v) => v,
            Err(e) => {
                self.toast_err(format!("invalid JSON, nothing saved: {e}"));
                return Ok(());
            }
        };
        let edited_pretty = serde_json::to_string_pretty(&value)?;
        if edited_pretty == original {
            self.toast("no changes");
            return Ok(());
        }
        self.popup = Some(Popup::confirm_diff(
            "Save dashboard?",
            &original,
            &edited_pretty,
            popup::Pending::SaveDashboard { uid: req.uid, folder_uid: req.folder_uid, json: value },
        ));
        Ok(())
    }
}

pub fn parse_refresh(s: &str) -> Option<Duration> {
    match s.trim() {
        "" | "off" | "0" => None,
        s => parse_duration(s).ok().filter(|ms| *ms > 0).map(|ms| Duration::from_millis(ms as u64)),
    }
}

/// Entry point: sets up the terminal, runs the app, restores the terminal.
pub async fn run(config: Config, context: Option<String>, dashboard: Option<String>) -> Result<()> {
    let ctx_name = config.initial_context(context.as_deref())?;
    crate::term::install_panic_hook();
    crate::term::enter(config.mouse)?;
    let bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(perf::CountingStdout::new(bytes.clone())))?;
    terminal.clear()?;
    let gfx = GraphicsState::detect(config.graphics);
    let (tx, rx) = mpsc::unbounded_channel();
    let result = match App::new(config, &ctx_name, gfx, tx) {
        Ok(mut app) => {
            app.perf.bytes_total = bytes;
            if let Some(uid) = dashboard {
                app.side = Side::Dashboards;
                app.focus = Focus::Main;
                app.open_dashboard(&uid, true);
            }
            app.run(&mut terminal, rx).await
        }
        Err(e) => Err(e),
    };
    crate::term::leave()?;
    terminal.show_cursor()?;
    result
}

/// Labels as a `HashSet` of `k=v` strings (for diffing firing sets).
pub fn label_key(uid: &str, labels: &std::collections::BTreeMap<String, String>) -> String {
    let mut s = uid.to_string();
    for (k, v) in labels {
        s.push_str(&format!("|{k}={v}"));
    }
    s
}

pub type Seen = HashSet<String>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grafana::models::{DashboardResponse, RuleGroup, SearchHit};
    use ratatui::backend::TestBackend;

    fn fixture<T: serde::de::DeserializeOwned>(name: &str) -> T {
        serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()
    }

    /// An app pointed at a dead address, filled with fixture data.
    fn app() -> App {
        let config =
            Config::parse("contexts:\n  t: {url: 'http://127.0.0.1:1', token: x}\n  ro: {url: 'http://127.0.0.1:1', readonly: true}")
                .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(config, "t", GraphicsState::unicode(), tx).unwrap();
        app.state_dir = None;
        app.on_msg(Msg::Datasources { ctx_gen: app.ctx_gen, result: Ok(fixture("datasources.json")) });
        let hits: Vec<SearchHit> = fixture("search.json");
        app.on_msg(Msg::Search { ctx_gen: app.ctx_gen, result: Ok(hits) });
        let groups: crate::grafana::models::RulesResponse = fixture("rules.json");
        let groups: Vec<RuleGroup> = groups.data.groups;
        app.on_msg(Msg::Rules { ctx_gen: app.ctx_gen, result: Ok(groups) });
        app.on_msg(Msg::AmAlerts { ctx_gen: app.ctx_gen, result: Ok(fixture("am_alerts.json")) });
        app
    }

    fn open_demo(app: &mut App) {
        app.open_dashboard("lg-demo-signals", true);
        let generation = app.dash.as_ref().unwrap().generation;
        let resp: DashboardResponse = fixture("dashboard_demo.json");
        app.on_msg(Msg::Dashboard { generation: generation | (1 << 63), result: Ok(resp) });
        // Override the dashboard's relative default with the fixture's range, so tests don't depend on the clock.
        app.time.push(crate::timerange::TimeRange::absolute(1_792_000_000_000, 1_792_003_600_000));
        // Feed every panel the range-query fixture.
        let ids: Vec<i64> = app.dash.as_ref().unwrap().dash.as_ref().unwrap().panels.iter().map(|p| p.id).collect();
        let query_gen = app.dash.as_ref().unwrap().query_gen;
        let body: serde_json::Value = fixture("query_range.json");
        for id in ids {
            let resp = crate::grafana::QueryResponse::parse(&body, &["A".to_string()]);
            let range = (1_792_000_000_000, 1_792_003_600_000);
            let data = crate::render::panels::PanelData::prepare(resp, range, 60_000, true);
            app.on_msg(Msg::PanelData { generation, query_gen, panel_id: id, range, result: Ok(data) });
        }
    }

    fn draw(app: &mut App, w: u16, h: u16) -> String {
        let mut t = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.render(f)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[tokio::test]
    async fn renders_every_view_at_every_size() {
        let mut app = app();
        open_demo(&mut app);
        for side in Side::ALL {
            for focus in [Focus::Side, Focus::Main] {
                app.side = side;
                app.focus = focus;
                for (w, h) in [(20, 5), (60, 15), (99, 30), (100, 30), (180, 50), (240, 70)] {
                    draw(&mut app, w, h);
                }
            }
        }
        app.side = Side::Dashboards;
        app.focus = Focus::Main;
        for tab in 0..4 {
            app.dispatch(&[Act::NextTab]);
            let _ = tab;
            draw(&mut app, 160, 45);
        }
        for p in [Popup::help(), Popup::time_picker(app.time.current()), Popup::context_picker(&app.config.context_names(), "t")] {
            app.popup = Some(p);
            for (w, h) in [(10, 4), (160, 45)] {
                draw(&mut app, w, h);
            }
        }
    }

    #[tokio::test]
    async fn dashboard_view_content() {
        let mut app = app();
        open_demo(&mut app);
        app.side = Side::Dashboards;
        app.focus = Focus::Main;
        let screen = draw(&mut app, 180, 50);
        assert!(screen.contains("Demo signals"), "{screen}");
        assert!(screen.contains("Requests per second"));
        assert!(screen.contains("Region"));
        // Crosshair + visual selection + zoom.
        app.dispatch(&[Act::Right]);
        assert!(app.dash.as_ref().unwrap().cursor.crosshair.is_some());
        app.dispatch(&[Act::Visual]);
        for _ in 0..5 {
            app.dispatch(&[Act::Left]);
        }
        let sel = app.dash.as_ref().unwrap().cursor.selection.unwrap();
        assert!(sel.1 > sel.0);
        app.dispatch(&[Act::Enter]);
        assert_eq!(app.time.current().resolve(0), sel, "zoomed into the selection");
        assert!(app.time.back(), "jumplist has the previous range");
    }

    #[tokio::test]
    async fn readonly_blocks_mutations() {
        let mut app = app();
        app.switch_context("ro");
        app.side = Side::Dashboards;
        app.dashboards.hits = fixture("search.json");
        app.dashboards.rebuild();
        app.dashboards.nav.selected = app.dashboards.rows.iter().position(|r| matches!(r, dashboards::Row::Dash { .. })).unwrap();
        app.dispatch(&[Act::Delete]);
        assert!(app.popup.is_none(), "no delete confirmation in read-only mode");
        assert!(app.toast.as_ref().is_some_and(|t| t.error && t.text.contains("read-only")));
    }

    #[tokio::test]
    async fn key_sequences_dispatch() {
        let mut app = app();
        app.side = Side::Dashboards;
        let key = |c| KeyEvent::new(crossterm::event::KeyCode::Char(c), crossterm::event::KeyModifiers::NONE);
        app.on_key(key('G'));
        let last = app.dashboards.rows.len() - 1;
        assert_eq!(app.dashboards.nav.selected, last);
        app.on_key(key('g'));
        assert_eq!(app.pending_keys, vec!["g"]);
        app.on_key(key('g'));
        assert_eq!(app.dashboards.nav.selected, 0);
        app.on_key(key('3'));
        assert_eq!(app.side, Side::Alerts);
        app.on_key(key('?'));
        assert!(matches!(app.popup, Some(Popup::Help { .. })));
        app.on_key(KeyEvent::new(crossterm::event::KeyCode::Esc, crossterm::event::KeyModifiers::NONE));
        assert!(app.popup.is_none());
    }

    #[tokio::test]
    async fn commands() {
        let mut app = app();
        app.run_command("time now-24h");
        assert_eq!(app.time.current().to_string(), "now-1d");
        app.run_command("refresh 30s");
        assert_eq!(app.refresh, Some(Duration::from_secs(30)));
        app.run_command("bogus");
        assert!(app.toast.as_ref().unwrap().error);
        app.run_command("q");
        assert!(app.quit);
    }
}
