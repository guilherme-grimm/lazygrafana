//! [5] Datasources: list, details, health checks.

use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::App;
use super::keymap::Act;
use super::msg::Msg;
use super::widgets::ListNav;

#[derive(Default)]
pub struct DatasourcesState {
    pub nav: ListNav,
    pub health: HashMap<String, Result<crate::grafana::models::DatasourceHealth, String>>,
    pub checking: HashSet<String>,
}

impl App {
    pub fn datasources_side_key(&mut self, acts: &[Act]) -> bool {
        if self.datasources.nav.handle(acts, self.ds_list.len(), 10) {
            return true;
        }
        let Some(ds) = self.ds_list.get(self.datasources.nav.selected).cloned() else { return false };
        for a in acts {
            match a {
                Act::Test => {
                    let uid = ds.uid.clone();
                    self.datasources.checking.insert(uid.clone());
                    self.spawn(move |c| async move { Msg::DsHealth { result: c.datasource_health(&uid).await, uid } });
                }
                Act::YankUid => self.copy("UID", &ds.uid),
                Act::OpenBrowser => {
                    let url = format!("{}/connections/datasources/edit/{}", self.base_url(), ds.uid);
                    self.open_url(&url);
                }
                Act::YankUrl => {
                    let url = format!("{}/connections/datasources/edit/{}", self.base_url(), ds.uid);
                    self.copy("URL", &url);
                }
                Act::YankJson => {
                    let json = serde_json::to_string_pretty(&ds.json_data).unwrap_or_default();
                    self.copy("jsonData", &json);
                }
                _ => continue,
            }
            return true;
        }
        false
    }

    pub fn datasources_main_key(&mut self, acts: &[Act]) -> bool {
        self.datasources_side_key(acts)
    }

    pub fn render_datasources_side(&mut self, f: &mut Frame, area: Rect) {
        let height = area.height as usize;
        self.datasources.nav.clamp(self.ds_list.len(), height);
        if self.ds_list.is_empty() {
            f.render_widget(Paragraph::new(Line::styled("loading…", Style::default().fg(Color::DarkGray))), area);
            return;
        }
        for (row, i) in self.datasources.nav.visible(self.ds_list.len(), height).enumerate() {
            let ds = &self.ds_list[i];
            let health = match self.datasources.health.get(&ds.uid) {
                Some(Ok(h)) if h.status == "OK" => Span::styled("● ", Style::default().fg(Color::Green)),
                Some(_) => Span::styled("● ", Style::default().fg(Color::Red)),
                None => Span::styled("○ ", Style::default().fg(Color::DarkGray)),
            };
            let line = Line::from(vec![
                health,
                Span::raw(ds.name.clone()),
                Span::styled(format!("  {}", ds.kind), Style::default().fg(Color::DarkGray)),
                Span::styled(if ds.is_default { " ★" } else { "" }, Style::default().fg(Color::Yellow)),
            ]);
            self.list_row(f, area, row, i == self.datasources.nav.selected, line);
        }
    }

    pub fn render_datasources_main(&mut self, f: &mut Frame, area: Rect) {
        let Some(ds) = self.ds_list.get(self.datasources.nav.selected) else { return };
        let key = |k: &str| Span::styled(format!("{k:<12}"), Style::default().fg(Color::Gray));
        let mut lines = vec![
            Line::styled(ds.name.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Line::raw(""),
            Line::from(vec![
                key("Type"),
                Span::raw(if ds.type_name.is_empty() { ds.kind.clone() } else { format!("{} ({})", ds.type_name, ds.kind) }),
            ]),
            Line::from(vec![key("UID"), Span::raw(ds.uid.clone())]),
            Line::from(vec![key("URL"), Span::raw(ds.url.clone())]),
            Line::from(vec![key("Access"), Span::raw(ds.access.clone())]),
            Line::from(vec![key("Default"), Span::raw(if ds.is_default { "yes" } else { "no" })]),
            Line::from(vec![key("Read-only"), Span::raw(if ds.read_only { "yes (provisioned)" } else { "no" })]),
        ];
        let health = if self.datasources.checking.contains(&ds.uid) {
            Span::styled("checking…", Style::default().fg(Color::DarkGray))
        } else {
            match self.datasources.health.get(&ds.uid) {
                Some(Ok(h)) => Span::styled(
                    format!("{} — {}", h.status, h.message),
                    Style::default().fg(if h.status == "OK" { Color::Green } else { Color::Red }),
                ),
                Some(Err(e)) => Span::styled(e.clone(), Style::default().fg(Color::Red)),
                None => Span::styled("not checked (press t)", Style::default().fg(Color::DarkGray)),
            }
        };
        lines.push(Line::from(vec![key("Health"), health]));
        if !ds.json_data.is_null() && ds.json_data.as_object().is_some_and(|o| !o.is_empty()) {
            lines.push(Line::raw(""));
            lines.push(Line::styled("jsonData", Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)));
            for l in serde_json::to_string_pretty(&ds.json_data).unwrap_or_default().lines() {
                lines.push(Line::styled(l.to_string(), Style::default().fg(Color::DarkGray)));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("t health check · o open · yu copy UID · yj copy jsonData", Style::default().fg(Color::DarkGray)));
        let url = ds.url.clone();
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        if url.starts_with("http") {
            self.link(area, area.x + 12, area.y + 4, &url);
        }
    }
}
