//! Modal popups: help, text/diff viewers, confirmations, inputs, pickers and
//! the silence form. Popups receive raw key events.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use similar::{ChangeTag, TextDiff};

use super::keymap::{Act, Keymap};
use super::widgets::{InputResult, ListNav, Scroll, TextInput, fuzzy_filter, truncate};
use crate::grafana::models::Matcher;
use crate::timerange::{Millis, PRESETS, TimeRange};

#[derive(Debug, Clone)]
pub enum InputPurpose {
    Command,
    TimeCustom,
    Annotate { dashboard_uid: Option<String>, panel_id: Option<i64>, range: (Millis, Millis) },
    GroupBy,
    VarText { name: String },
}

#[derive(Debug, Clone)]
pub enum SelectPurpose {
    Context,
    Time,
    Refresh,
    Variable { name: String },
    MoveFolder { uid: String },
    Datasource,
    History,
}

/// A mutating action waiting for confirmation.
#[derive(Debug, Clone)]
pub enum Pending {
    DeleteDashboard { uid: String, title: String },
    SaveDashboard { uid: String, folder_uid: Option<String>, json: serde_json::Value },
    RestoreVersion { uid: String, version: i64 },
    ExpireSilences { ids: Vec<String> },
    PauseRule { uid: String, pause: bool },
}

#[derive(Debug, Clone)]
pub struct SelectItem {
    pub label: String,
    pub value: String,
    pub hint: String,
}

pub const SILENCE_DURATIONS: [(&str, Millis); 8] = [
    ("30m", 1_800_000),
    ("1h", 3_600_000),
    ("2h", 7_200_000),
    ("4h", 14_400_000),
    ("8h", 28_800_000),
    ("1d", 86_400_000),
    ("3d", 259_200_000),
    ("1w", 604_800_000),
];

#[derive(Debug, Clone)]
pub struct SilenceForm {
    pub title: String,
    pub matchers: TextInput,
    pub duration: usize,
    pub comment: TextInput,
    pub field: usize,
    pub error: Option<String>,
}

pub enum Popup {
    Help {
        lines: Vec<Line<'static>>,
        scroll: Scroll,
    },
    Text {
        title: String,
        lines: Vec<Line<'static>>,
        scroll: Scroll,
    },
    Confirm {
        title: String,
        lines: Vec<Line<'static>>,
        scroll: Scroll,
        pending: Pending,
    },
    Input {
        title: String,
        prefix: String,
        input: TextInput,
        purpose: InputPurpose,
    },
    Select {
        title: String,
        items: Vec<SelectItem>,
        nav: ListNav,
        multi: bool,
        checked: BTreeSet<usize>,
        filter: TextInput,
        purpose: SelectPurpose,
    },
    Silence(SilenceForm),
}

pub enum Outcome {
    None,
    Close,
    Input { purpose: InputPurpose, text: String },
    Select { purpose: SelectPurpose, values: Vec<String> },
    Confirm(Pending),
    Silence { matchers: Vec<Matcher>, duration: Millis, comment: String },
}

impl Popup {
    pub fn help() -> Self {
        Popup::Help { lines: Vec::new(), scroll: Scroll::default() }
    }

    pub fn help_lines(keymap: &Keymap) -> Vec<Line<'static>> {
        let k = |a: Act| keymap.key_for(a);
        let section = |t: &str| Line::styled(format!("── {t} "), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        let row = |keys: String, desc: &str| {
            Line::from(vec![Span::styled(format!("  {keys:<16}"), Style::default().fg(Color::Yellow)), Span::raw(desc.to_string())])
        };
        let mut l = vec![
            section("Global"),
            row(format!("{} {} {} {}", k(Act::Left), k(Act::Down), k(Act::Up), k(Act::Right)), "move; h/l switch side ↔ main"),
            row(format!("{} / {}", k(Act::Top), k(Act::Bottom)), "top / bottom"),
            row(format!("{} / {}", k(Act::HalfDown), k(Act::HalfUp)), "half page down / up"),
            row("1-5, Tab, S-Tab".into(), "focus side panel / cycle"),
            row(format!("{} {}", k(Act::PrevTab), k(Act::NextTab)), "previous / next tab in the main pane"),
            row(k(Act::Filter), "fuzzy filter the current list"),
            row(k(Act::Command), "command line (:ctx, :time, :var, :refresh, :tag, :open, :q)"),
            row(k(Act::ContextSwitch), "switch context"),
            row(format!("{} / {}", k(Act::TimePicker), k(Act::RefreshPicker)), "time range / auto-refresh"),
            row(format!("{} / {}", k(Act::JumpBack), k(Act::JumpForward)), "time range history back / forward"),
            row(
                format!("{} {} {} {}", k(Act::ZoomIn), k(Act::ZoomOut), k(Act::PanLeft), k(Act::PanRight)),
                "zoom in / out, pan left / right",
            ),
            row(k(Act::Refresh), "refresh now"),
            row(
                format!("{} {} {} {}", k(Act::YankUrl), k(Act::YankUid), k(Act::YankJson), k(Act::YankRange)),
                "yank URL / UID / JSON / time range",
            ),
            row(k(Act::OpenBrowser), "open in browser"),
            row(format!("{} / {}", k(Act::Help), k(Act::Quit)), "help / quit"),
            Line::raw(""),
            section("Dashboards"),
            row(format!("{} / {}", k(Act::Enter), k(Act::Right)), "open dashboard / expand folder"),
            row(k(Act::StateFilter), "cycle view: tree → starred → recent"),
            row(
                format!("{} {} {} {} {}", k(Act::Edit), k(Act::Star), k(Act::Move), k(Act::Export), k(Act::Delete)),
                "edit JSON, star, move, export, delete",
            ),
            row(format!("{} {} {}", k(Act::Mark), k(Act::Diff), k(Act::Restore)), "versions: mark, diff (marked vs selected), restore"),
            Line::raw(""),
            section("Charts (Panels tab, Explore)"),
            row(format!("{} {}", k(Act::Down), k(Act::Up)), "select panel"),
            row(format!("{} {}", k(Act::Left), k(Act::Right)), "move crosshair by one data point"),
            row(
                format!("{} {} {} {}", k(Act::WordForward), k(Act::WordBack), k(Act::LineStart), k(Act::LineEnd)),
                "move crosshair 1/10 range, start, end",
            ),
            row(format!("{} … {}", k(Act::Visual), k(Act::Enter)), "visual-select a range, then zoom into it"),
            row(k(Act::LogsForSelection), "logs for the selection / visible range"),
            row(k(Act::Annotate), "annotate the selection or crosshair"),
            row(format!("{} {}", k(Act::Maximize), k(Act::ExactRender)), "maximize panel / exact render via Grafana"),
            row(k(Act::MaxOverTime), "toggle max_over_time (keep short spikes visible)"),
            row("mouse".into(), "click: crosshair, drag: zoom, wheel: zoom"),
            Line::raw(""),
            section("Alerts"),
            row(format!("{} {}", k(Act::StateFilter), k(Act::GroupBy)), "cycle state filter / group by label"),
            row(format!("{} {} {}", k(Act::Silence), k(Act::Unsilence), k(Act::Pause)), "silence / expire silences / pause rule"),
            row(k(Act::Mark), "mark for bulk actions"),
            Line::raw(""),
            section("Explore"),
            row(format!("{} / {}", k(Act::Insert), k(Act::Enter)), "edit query / run"),
            row(format!("{} {}", k(Act::PickDatasource), k(Act::History)), "pick datasource / search history"),
            row(
                format!("{} {} {} {}", k(Act::ToggleTable), k(Act::Follow), k(Act::Wrap), k(Act::PrettyJson)),
                "table view, follow (tail), wrap, pretty JSON",
            ),
            Line::raw(""),
            section("Popups"),
            row("Enter / Esc".into(), "confirm / cancel; ↑↓ or C-n/C-p move; type to filter; Space toggles"),
        ];
        if !keymap.warnings.is_empty() {
            l.push(Line::raw(""));
            l.extend(keymap.warnings.iter().map(|w| Line::styled(w.clone(), Style::default().fg(Color::Red))));
        }
        l
    }

    pub fn text(title: &str, lines: Vec<Line<'static>>) -> Self {
        Popup::Text { title: title.into(), lines, scroll: Scroll::default() }
    }

    pub fn diff(title: &str, a: &str, b: &str) -> Self {
        Popup::text(title, diff_lines(a, b))
    }

    pub fn confirm(title: &str, lines: Vec<Line<'static>>, pending: Pending) -> Self {
        Popup::Confirm { title: title.into(), lines, scroll: Scroll::default(), pending }
    }

    pub fn confirm_diff(title: &str, a: &str, b: &str, pending: Pending) -> Self {
        Popup::confirm(title, diff_lines(a, b), pending)
    }

    pub fn input(title: &str, prefix: &str, initial: &str, purpose: InputPurpose) -> Self {
        Popup::Input { title: title.into(), prefix: prefix.into(), input: TextInput::new(initial), purpose }
    }

    pub fn select(
        title: &str,
        items: Vec<SelectItem>,
        multi: bool,
        checked: BTreeSet<usize>,
        selected: usize,
        purpose: SelectPurpose,
    ) -> Self {
        Popup::Select {
            title: title.into(),
            items,
            nav: ListNav { selected, offset: 0 },
            multi,
            checked,
            filter: TextInput::default(),
            purpose,
        }
    }

    pub fn context_picker(names: &[String], current: &str) -> Self {
        let items = names
            .iter()
            .map(|n| SelectItem { label: n.clone(), value: n.clone(), hint: if n == current { "current".into() } else { String::new() } })
            .collect();
        let sel = names.iter().position(|n| n == current).unwrap_or(0);
        Popup::select("Switch context", items, false, BTreeSet::new(), sel, SelectPurpose::Context)
    }

    pub fn time_picker(current: &TimeRange) -> Self {
        let mut items: Vec<SelectItem> =
            PRESETS.iter().map(|(v, l)| SelectItem { label: l.to_string(), value: v.to_string(), hint: v.to_string() }).collect();
        items.push(SelectItem {
            label: "Custom range…".into(),
            value: "__custom".into(),
            hint: "now-2d to now-1d, 2026-10-14 13:00".into(),
        });
        let cur = current.to_string();
        let sel = items.iter().position(|i| i.value == cur).unwrap_or(0);
        Popup::select(&format!("Time range (current: {cur})"), items, false, BTreeSet::new(), sel, SelectPurpose::Time)
    }

    pub fn refresh_picker(current: Option<std::time::Duration>) -> Self {
        let opts = ["off", "5s", "10s", "30s", "1m", "5m", "15m"];
        let items: Vec<SelectItem> =
            opts.iter().map(|o| SelectItem { label: o.to_string(), value: o.to_string(), hint: String::new() }).collect();
        let cur = current.map(|d| crate::timerange::format_duration(d.as_millis() as i64)).unwrap_or_else(|| "off".into());
        let sel = opts.iter().position(|o| *o == cur).unwrap_or(0);
        Popup::select("Auto refresh", items, false, BTreeSet::new(), sel, SelectPurpose::Refresh)
    }

    pub fn silence(title: &str, matchers: &[Matcher]) -> Self {
        let text = matchers.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(", ");
        Popup::Silence(SilenceForm {
            title: title.into(),
            matchers: TextInput::new(text),
            duration: 1,
            comment: TextInput::new("silenced from lazygrafana"),
            field: 1,
            error: None,
        })
    }

    pub fn paste(&mut self, s: &str) {
        match self {
            Popup::Input { input, .. } => input.insert_str(s),
            Popup::Select { filter, .. } => filter.insert_str(s),
            Popup::Silence(f) => match f.field {
                0 => f.matchers.insert_str(s),
                2 => f.comment.insert_str(s),
                _ => {}
            },
            _ => {}
        }
    }

    pub fn handle_raw(&mut self, k: &KeyEvent) -> Outcome {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match self {
            Popup::Help { lines, scroll } | Popup::Text { lines, scroll, .. } => match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => Outcome::Close,
                _ => {
                    scroll_keys(scroll, k, lines.len());
                    Outcome::None
                }
            },
            Popup::Confirm { lines, scroll, pending, .. } => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Outcome::Confirm(pending.clone()),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => Outcome::Close,
                _ => {
                    scroll_keys(scroll, k, lines.len());
                    Outcome::None
                }
            },
            Popup::Input { input, purpose, .. } => match input.handle(k) {
                InputResult::Submit => Outcome::Input { purpose: purpose.clone(), text: input.text.clone() },
                InputResult::Cancel => Outcome::Close,
                _ => Outcome::None,
            },
            Popup::Select { items, nav, multi, checked, filter, purpose, .. } => {
                let visible = select_visible(items, &filter.text);
                let len = visible.len();
                match k.code {
                    KeyCode::Esc => return Outcome::Close,
                    KeyCode::Enter => {
                        let values: Vec<String> = if *multi && !checked.is_empty() {
                            checked.iter().map(|i| items[*i].value.clone()).collect()
                        } else {
                            visible.get(nav.selected).map(|i| vec![items[*i].value.clone()]).unwrap_or_default()
                        };
                        if values.is_empty() {
                            return Outcome::Close;
                        }
                        return Outcome::Select { purpose: purpose.clone(), values };
                    }
                    KeyCode::Down | KeyCode::Tab => nav.selected = (nav.selected + 1).min(len.saturating_sub(1)),
                    KeyCode::Up | KeyCode::BackTab => nav.selected = nav.selected.saturating_sub(1),
                    KeyCode::Char('n') | KeyCode::Char('j') if ctrl => nav.selected = (nav.selected + 1).min(len.saturating_sub(1)),
                    KeyCode::Char('p') | KeyCode::Char('k') if ctrl => nav.selected = nav.selected.saturating_sub(1),
                    KeyCode::PageDown => nav.selected = (nav.selected + 10).min(len.saturating_sub(1)),
                    KeyCode::PageUp => nav.selected = nav.selected.saturating_sub(10),
                    KeyCode::Char(' ') if *multi => {
                        if let Some(i) = visible.get(nav.selected) {
                            if !checked.remove(i) {
                                checked.insert(*i);
                            }
                            nav.selected = (nav.selected + 1).min(len.saturating_sub(1));
                        }
                    }
                    _ => {
                        if let InputResult::Changed = filter.handle(k) {
                            nav.selected = 0;
                            nav.offset = 0;
                        }
                    }
                }
                Outcome::None
            }
            Popup::Silence(f) => match k.code {
                KeyCode::Esc => Outcome::Close,
                KeyCode::Tab | KeyCode::Down => {
                    f.field = (f.field + 1) % 3;
                    Outcome::None
                }
                KeyCode::BackTab | KeyCode::Up => {
                    f.field = (f.field + 2) % 3;
                    Outcome::None
                }
                KeyCode::Enter => match parse_matchers(&f.matchers.text) {
                    Ok(m) if !m.is_empty() => {
                        Outcome::Silence { matchers: m, duration: SILENCE_DURATIONS[f.duration].1, comment: f.comment.text.clone() }
                    }
                    Ok(_) => {
                        f.error = Some("at least one matcher is required".into());
                        Outcome::None
                    }
                    Err(e) => {
                        f.error = Some(e);
                        Outcome::None
                    }
                },
                _ if f.field == 1 => {
                    match k.code {
                        KeyCode::Left | KeyCode::Char('h') => f.duration = f.duration.saturating_sub(1),
                        KeyCode::Right | KeyCode::Char('l') => f.duration = (f.duration + 1).min(SILENCE_DURATIONS.len() - 1),
                        _ => {}
                    }
                    Outcome::None
                }
                _ => {
                    let input = if f.field == 0 { &mut f.matchers } else { &mut f.comment };
                    input.handle(k);
                    f.error = None;
                    Outcome::None
                }
            },
        }
    }

    pub fn render(&mut self, f: &mut Frame, keymap: &Keymap) {
        let area = f.area();
        match self {
            Popup::Help { lines, scroll } => {
                if lines.is_empty() {
                    *lines = Popup::help_lines(keymap);
                }
                let r = centered(area, 86, 90);
                render_scrolled(f, r, " Help — Esc to close ", lines, scroll);
            }
            Popup::Text { title, lines, scroll } => {
                let r = centered(area, 90, 90);
                render_scrolled(f, r, &format!(" {title} — Esc to close "), lines, scroll);
            }
            Popup::Confirm { title, lines, scroll, .. } => {
                let h = fit(lines.len() as u16 + 4, 6, area.height.saturating_sub(4));
                let w = fit(lines.iter().map(|l| l.width() as u16).max().unwrap_or(30), 40, area.width.saturating_sub(4));
                let r = centered_abs(area, w + 4, h + 1);
                // Body above, footer on the last inner row.
                let body = Rect { height: r.height.saturating_sub(1), ..r };
                render_scrolled(f, body, &format!(" {title} "), lines, scroll);
                if r.height < 3 {
                    return;
                }
                let footer = Rect { x: r.x + 1, y: r.y + r.height - 2, width: r.width.saturating_sub(2), height: 1 };
                let frame = Rect { y: r.y + r.height - 1, height: 1, ..r };
                f.render_widget(Clear, frame);
                f.render_widget(
                    Block::default()
                        .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::Cyan))
                        .style(Style::default().bg(Color::Rgb(0x16, 0x17, 0x1d))),
                    Rect { y: r.y + r.height - 2, height: 2, ..r },
                );
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(" y", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                        Span::raw("/Enter confirm   "),
                        Span::styled("n", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                        Span::raw("/Esc cancel"),
                    ]))
                    .style(Style::default().bg(Color::Rgb(0x1e, 0x20, 0x26))),
                    footer,
                );
                let _ = frame;
            }
            Popup::Input { title, prefix, input, .. } => {
                let r = centered_abs(area, (area.width * 2 / 3).max(40), 3);
                f.render_widget(Clear, r);
                let block = popup_block(title);
                let inner = block.inner(r);
                f.render_widget(block, r);
                input.render(inner, f.buffer_mut(), prefix, Style::default(), true);
            }
            Popup::Select { title, items, nav, multi, checked, filter, .. } => {
                let visible = select_visible(items, &filter.text);
                let h = fit(visible.len() as u16 + 4, 6, area.height.saturating_sub(4));
                let r = centered_abs(area, (area.width * 2 / 3).clamp(40, 100), h);
                f.render_widget(Clear, r);
                let block = popup_block(title);
                let inner = block.inner(r);
                f.render_widget(block, r);
                let [filter_area, list_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
                filter.render(filter_area, f.buffer_mut(), "› ", Style::default().fg(Color::White), true);
                nav.clamp(visible.len(), list_area.height as usize);
                for (row, vi) in nav.visible(visible.len(), list_area.height as usize).enumerate() {
                    let idx = visible[vi];
                    let item = &items[idx];
                    let sel = vi == nav.selected;
                    let mark = if *multi { if checked.contains(&idx) { "[x] " } else { "[ ] " } } else { "" };
                    let style =
                        if sel { Style::default().bg(Color::Rgb(0x2a, 0x3a, 0x55)).add_modifier(Modifier::BOLD) } else { Style::default() };
                    let y = list_area.y + row as u16;
                    let line = Line::from(vec![
                        Span::styled(format!("{mark}{}", truncate(&item.label, list_area.width as usize)), style),
                        Span::styled(format!("  {}", item.hint), Style::default().fg(Color::DarkGray)),
                    ]);
                    f.buffer_mut().set_style(Rect { x: list_area.x, y, width: list_area.width, height: 1 }, style);
                    f.buffer_mut().set_line(list_area.x, y, &line, list_area.width);
                }
            }
            Popup::Silence(form) => {
                let r = centered_abs(area, (area.width * 2 / 3).clamp(50, 110), 10);
                f.render_widget(Clear, r);
                let block = popup_block(&format!("Silence: {}", form.title));
                let inner = block.inner(r);
                f.render_widget(block, r);
                let rows = Layout::vertical([Constraint::Length(2), Constraint::Length(2), Constraint::Length(2), Constraint::Min(1)])
                    .split(inner);
                let label = |i: usize, t: &str| {
                    Span::styled(
                        format!("{t:<10}"),
                        Style::default().fg(if form.field == i { Color::Cyan } else { Color::Gray }).add_modifier(Modifier::BOLD),
                    )
                };
                f.buffer_mut().set_span(rows[0].x, rows[0].y, &label(0, "Matchers"), 10);
                form.matchers.render(
                    Rect { x: rows[0].x + 10, width: rows[0].width.saturating_sub(10), height: 1, ..rows[0] },
                    f.buffer_mut(),
                    "",
                    Style::default(),
                    form.field == 0,
                );
                f.buffer_mut().set_span(rows[1].x, rows[1].y, &label(1, "Duration"), 10);
                let durs: Vec<Span> = SILENCE_DURATIONS
                    .iter()
                    .enumerate()
                    .map(|(i, (n, _))| {
                        let s = if i == form.duration {
                            Style::default().bg(Color::Rgb(0x2a, 0x3a, 0x55)).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Gray)
                        };
                        Span::styled(format!(" {n} "), s)
                    })
                    .collect();
                f.buffer_mut().set_line(rows[1].x + 10, rows[1].y, &Line::from(durs), rows[1].width.saturating_sub(10));
                f.buffer_mut().set_span(rows[2].x, rows[2].y, &label(2, "Comment"), 10);
                form.comment.render(
                    Rect { x: rows[2].x + 10, width: rows[2].width.saturating_sub(10), height: 1, ..rows[2] },
                    f.buffer_mut(),
                    "",
                    Style::default(),
                    form.field == 2,
                );
                let help = match &form.error {
                    Some(e) => Line::styled(e.clone(), Style::default().fg(Color::Red)),
                    None => Line::styled("Tab next field · h/l duration · Enter create · Esc cancel", Style::default().fg(Color::DarkGray)),
                };
                f.render_widget(Paragraph::new(help), rows[3]);
            }
        }
    }
}

fn select_visible(items: &[SelectItem], filter: &str) -> Vec<usize> {
    let hay: Vec<String> = items.iter().map(|i| format!("{} {}", i.label, i.hint)).collect();
    fuzzy_filter(filter, &hay)
}

fn scroll_keys(scroll: &mut Scroll, k: &KeyEvent, len: usize) {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let act = match k.code {
        KeyCode::Char('j') | KeyCode::Down => Act::Down,
        KeyCode::Char('k') | KeyCode::Up => Act::Up,
        KeyCode::Char('d') if ctrl => Act::HalfDown,
        KeyCode::Char('u') if ctrl => Act::HalfUp,
        KeyCode::PageDown => Act::HalfDown,
        KeyCode::PageUp => Act::HalfUp,
        KeyCode::Char('g') | KeyCode::Home => Act::Top,
        KeyCode::Char('G') | KeyCode::End => Act::Bottom,
        KeyCode::Char('l') | KeyCode::Right => Act::Right,
        KeyCode::Char('h') | KeyCode::Left => Act::Left,
        _ => return,
    };
    scroll.handle(&[act], len, 20);
}

fn popup_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(format!(" {title} "), Style::default().add_modifier(Modifier::BOLD)))
        .style(Style::default().bg(Color::Rgb(0x16, 0x17, 0x1d)))
}

fn render_scrolled(f: &mut Frame, r: Rect, title: &str, lines: &[Line<'static>], scroll: &mut Scroll) {
    f.render_widget(Clear, r);
    let block = popup_block(title.trim());
    let inner = block.inner(r);
    f.render_widget(block, r);
    let max = lines.len().saturating_sub(inner.height as usize);
    scroll.y = scroll.y.min(max);
    let p = Paragraph::new(lines.to_vec()).scroll((scroll.y as u16, scroll.x as u16));
    f.render_widget(p, inner);
    if lines.len() > inner.height as usize {
        let info = format!(" {}/{} ", (scroll.y + inner.height as usize).min(lines.len()), lines.len());
        let x = r.x + r.width.saturating_sub(info.len() as u16 + 2);
        f.buffer_mut().set_string(x, r.y + r.height - 1, info, Style::default().fg(Color::DarkGray));
    }
}

/// Like `clamp`, but never panics: the upper bound wins on tiny screens.
fn fit(v: u16, lo: u16, hi: u16) -> u16 {
    v.max(lo).min(hi)
}

pub fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = area.width * pct_w / 100;
    let h = area.height * pct_h / 100;
    centered_abs(area, w, h)
}

pub fn centered_abs(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// Unified diff with colours.
pub fn diff_lines(a: &str, b: &str) -> Vec<Line<'static>> {
    let diff = TextDiff::from_lines(a, b);
    let mut out = Vec::new();
    for group in diff.grouped_ops(3) {
        let first = &group[0];
        let last = &group[group.len() - 1];
        out.push(Line::styled(
            format!("@@ -{} +{} @@", first.old_range().start + 1, first.new_range().start + 1),
            Style::default().fg(Color::Cyan),
        ));
        let _ = last;
        for op in group {
            for change in diff.iter_changes(&op) {
                let text = change.value().trim_end_matches('\n').to_string();
                out.push(match change.tag() {
                    ChangeTag::Delete => Line::styled(format!("-{text}"), Style::default().fg(Color::LightRed)),
                    ChangeTag::Insert => Line::styled(format!("+{text}"), Style::default().fg(Color::LightGreen)),
                    ChangeTag::Equal => Line::styled(format!(" {text}"), Style::default().fg(Color::DarkGray)),
                });
            }
        }
    }
    if out.is_empty() {
        out.push(Line::styled("(no differences)", Style::default().fg(Color::DarkGray)));
    }
    out
}

/// Parses `a="b", c=~"d.*", e!="f"` into matchers.
pub fn parse_matchers(s: &str) -> Result<Vec<Matcher>, String> {
    let re = regex::Regex::new(r#"^\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*(=~|!~|!=|=)\s*"((?:[^"\\]|\\.)*)"\s*$"#).expect("valid regex");
    let mut out = Vec::new();
    let mut depth_split = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut prev = '\0';
    for c in s.chars() {
        if c == '"' && prev != '\\' {
            in_quotes = !in_quotes;
        }
        if c == ',' && !in_quotes {
            depth_split.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
        prev = c;
    }
    depth_split.push(cur);
    for part in depth_split.iter().filter(|p| !p.trim().is_empty()) {
        let c = re.captures(part).ok_or_else(|| format!("invalid matcher: {} (expected name=\"value\")", part.trim()))?;
        let (is_equal, is_regex) = match &c[2] {
            "=" => (true, false),
            "!=" => (false, false),
            "=~" => (true, true),
            _ => (false, true),
        };
        out.push(Matcher { name: c[1].to_string(), value: c[3].replace("\\\"", "\""), is_regex, is_equal });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent { code: c, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::empty() }
    }

    #[test]
    fn matchers_parse() {
        let m = parse_matchers(r#"alertname="X, Y", team=~"a|b", sev!="info""#).unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].value, "X, Y");
        assert!(m[1].is_regex && m[1].is_equal);
        assert!(!m[2].is_equal);
        assert!(parse_matchers("bogus").is_err());
        assert!(parse_matchers("").unwrap().is_empty());
    }

    #[test]
    fn select_filter_and_multi() {
        let items = vec![
            SelectItem { label: "eu-west-1".into(), value: "eu".into(), hint: String::new() },
            SelectItem { label: "us-east-1".into(), value: "us".into(), hint: String::new() },
        ];
        let mut p = Popup::select("x", items, true, BTreeSet::new(), 0, SelectPurpose::Variable { name: "r".into() });
        p.handle_raw(&key(KeyCode::Char(' ')));
        p.handle_raw(&key(KeyCode::Char(' ')));
        match p.handle_raw(&key(KeyCode::Enter)) {
            Outcome::Select { values, .. } => assert_eq!(values, vec!["eu", "us"]),
            _ => panic!("expected select"),
        }
        let items = vec![
            SelectItem { label: "eu-west-1".into(), value: "eu".into(), hint: String::new() },
            SelectItem { label: "us-east-1".into(), value: "us".into(), hint: String::new() },
        ];
        let mut p = Popup::select("x", items, false, BTreeSet::new(), 0, SelectPurpose::Datasource);
        for c in "us".chars() {
            p.handle_raw(&key(KeyCode::Char(c)));
        }
        match p.handle_raw(&key(KeyCode::Enter)) {
            Outcome::Select { values, .. } => assert_eq!(values, vec!["us"]),
            _ => panic!("expected select"),
        }
    }

    #[test]
    fn confirm_keys() {
        let mut p = Popup::confirm("x", vec![], Pending::ExpireSilences { ids: vec![] });
        assert!(matches!(p.handle_raw(&key(KeyCode::Char('j'))), Outcome::None));
        assert!(matches!(p.handle_raw(&key(KeyCode::Char('y'))), Outcome::Confirm(_)));
        assert!(matches!(p.handle_raw(&key(KeyCode::Esc)), Outcome::Close));
    }

    #[test]
    fn diffs() {
        let lines = diff_lines("a\nb\nc\n", "a\nB\nc\n");
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text.contains(&"-b".to_string()) && text.contains(&"+B".to_string()));
    }
}
