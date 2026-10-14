//! [1] Contexts: Grafana instances/orgs from the config.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::keymap::Act;
use super::msg::Msg;
use super::widgets::ListNav;
use super::{App, Focus};
use crate::config::Config;
use crate::grafana::Client;

pub struct ContextsState {
    pub names: Vec<String>,
    pub nav: ListNav,
    /// name -> Ok(version) / Err(message)
    pub statuses: HashMap<String, Result<String, String>>,
}

impl ContextsState {
    pub fn new(config: &Config) -> Self {
        Self { names: config.context_names(), nav: ListNav::default(), statuses: HashMap::new() }
    }

    pub fn select_name(&mut self, name: &str) {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            self.nav.selected = i;
        }
    }

    pub fn selected(&self) -> Option<&String> {
        self.names.get(self.nav.selected)
    }
}

impl App {
    pub fn contexts_side_key(&mut self, acts: &[Act]) -> bool {
        let len = self.contexts.names.len();
        if self.contexts.nav.handle(acts, len, 10) {
            return true;
        }
        for a in acts {
            match a {
                Act::Enter => {
                    if let Some(name) = self.contexts.selected().cloned() {
                        if name != self.ctx.name {
                            self.switch_context(&name);
                        } else {
                            self.focus = Focus::Main;
                        }
                    }
                }
                Act::Test => self.check_all_contexts(),
                Act::OpenBrowser => {
                    if let Some(name) = self.contexts.selected().cloned()
                        && let Some(c) = self.config.contexts.get(&name)
                    {
                        let url = c.url.clone();
                        self.open_url(&url);
                    }
                }
                Act::YankUrl => {
                    if let Some(name) = self.contexts.selected().cloned()
                        && let Some(c) = self.config.contexts.get(&name)
                    {
                        let url = c.url.clone();
                        self.copy("URL", &url);
                    }
                }
                _ => continue,
            }
            return true;
        }
        false
    }

    pub fn contexts_main_key(&mut self, acts: &[Act]) -> bool {
        self.contexts_side_key(acts)
    }

    /// Health-checks every context (each with its own client).
    pub fn check_all_contexts(&mut self) {
        for name in self.contexts.names.clone() {
            let Ok(ctx) = self.config.resolve_context(&name) else { continue };
            let tx = self.tx.clone();
            self.contexts.statuses.remove(&name);
            tokio::spawn(async move {
                let result = match Client::new(&ctx) {
                    Ok(c) => match (c.health().await, c.user().await) {
                        (Ok(h), Ok(_)) => Ok(h.version),
                        (Ok(_), Err(e)) | (Err(e), _) => Err(e.to_string()),
                    },
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Msg::ContextHealth { ctx: name, result });
            });
        }
        self.toast("checking contexts…");
    }

    pub fn render_contexts_side(&mut self, f: &mut Frame, area: Rect) {
        let height = area.height as usize;
        self.contexts.nav.clamp(self.contexts.names.len(), height);
        for (row, i) in self.contexts.nav.visible(self.contexts.names.len(), height).enumerate() {
            let name = &self.contexts.names[i];
            let cfg = &self.config.contexts[name];
            let active = *name == self.ctx.name;
            let mut spans = vec![
                Span::styled(if active { "● " } else { "  " }, Style::default().fg(Color::Green)),
                Span::styled(name.clone(), if active { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() }),
            ];
            if cfg.readonly {
                spans.push(Span::styled(" ro", Style::default().fg(Color::Yellow)));
            }
            if cfg.cloud.unwrap_or_else(|| crate::config::is_cloud_url(&cfg.url)) {
                spans.push(Span::styled(" ☁", Style::default().fg(Color::Blue)));
            }
            match self.contexts.statuses.get(name) {
                Some(Ok(v)) => spans.push(Span::styled(format!("  v{v}"), Style::default().fg(Color::DarkGray))),
                Some(Err(_)) => spans.push(Span::styled("  ✖", Style::default().fg(Color::Red))),
                None => {}
            }
            self.list_row(f, area, row, i == self.contexts.nav.selected, Line::from(spans));
        }
    }

    pub fn render_contexts_main(&mut self, f: &mut Frame, area: Rect) {
        let Some(name) = self.contexts.selected().cloned() else { return };
        let Some(cfg) = self.config.contexts.get(&name).cloned() else { return };
        let key = |k: &str| Span::styled(format!("{k:<14}"), Style::default().fg(Color::Gray));
        let token = if cfg.token.is_some() {
            "inline (config)".to_string()
        } else if let Some(e) = &cfg.token_env {
            format!("env ${e}")
        } else if let Some(p) = &cfg.token_file {
            format!("file {}", p.display())
        } else if let Some(c) = &cfg.token_cmd {
            format!("command `{c}`")
        } else {
            "none (anonymous)".into()
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
                Span::raw(if name == self.ctx.name { "  (active)" } else { "" }),
            ]),
            Line::raw(""),
            Line::from(vec![key("URL"), Span::raw(cfg.url.clone())]),
            Line::from(vec![key("Org"), Span::raw(cfg.org.map(|o| o.to_string()).unwrap_or_else(|| "token default".into()))]),
            Line::from(vec![key("Token"), Span::raw(token)]),
            Line::from(vec![
                key("Read-only"),
                Span::styled(
                    if cfg.readonly { "yes" } else { "no" },
                    Style::default().fg(if cfg.readonly { Color::Yellow } else { Color::Gray }),
                ),
            ]),
            Line::from(vec![
                key("Grafana Cloud"),
                Span::raw(if cfg.cloud.unwrap_or_else(|| crate::config::is_cloud_url(&cfg.url)) { "yes" } else { "no" }),
            ]),
        ];
        match self.contexts.statuses.get(&name) {
            Some(Ok(v)) => lines
                .push(Line::from(vec![key("Status"), Span::styled(format!("healthy · Grafana v{v}"), Style::default().fg(Color::Green))])),
            Some(Err(e)) => lines.push(Line::from(vec![key("Status"), Span::styled(e.clone(), Style::default().fg(Color::Red))])),
            None => lines
                .push(Line::from(vec![key("Status"), Span::styled("unknown (press t to check)", Style::default().fg(Color::DarkGray))])),
        }
        if name == self.ctx.name {
            if let Some(i) = &self.instance {
                lines.push(Line::from(vec![key("Edition"), Span::raw(i.edition.clone())]));
                if let Some(u) = &i.user {
                    lines.push(Line::from(vec![
                        key("Signed in as"),
                        Span::raw(format!("{} (org {})", if u.login.is_empty() { &u.name } else { &u.login }, u.org_id)),
                    ]));
                }
            }
            if let Some(e) = &self.ctx.token_error {
                lines.push(Line::from(vec![key("Token error"), Span::styled(e.clone(), Style::default().fg(Color::Red))]));
            }
            lines.push(Line::from(vec![
                key("Graphics"),
                Span::raw(format!(
                    "{} ({}×{} px cells{})",
                    self.gfx.mode.name(),
                    self.gfx.cell.0,
                    self.gfx.cell.1,
                    if self.gfx.tmux { ", tmux" } else { "" }
                )),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("Enter switch · t check all · o open · yy copy URL", Style::default().fg(Color::DarkGray)));
        f.render_widget(Paragraph::new(lines), area);
        self.link(area, area.x + 14, area.y + 2, &cfg.url);
    }
}
