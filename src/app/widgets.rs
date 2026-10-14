//! Small reusable UI pieces: list navigation, a single-line text input, and
//! fuzzy matching.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as MatcherConfig, Matcher, Utf32Str};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::keymap::Act;

/// Cursor + scroll offset for a list of `len` rows shown in `height` rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListNav {
    pub selected: usize,
    pub offset: usize,
}

impl ListNav {
    /// Applies a navigation action; returns true if it was one.
    pub fn handle(&mut self, acts: &[Act], len: usize, height: usize) -> bool {
        let half = (height / 2).max(1);
        for a in acts {
            match a {
                Act::Down => self.selected = (self.selected + 1).min(len.saturating_sub(1)),
                Act::Up => self.selected = self.selected.saturating_sub(1),
                Act::Top => self.selected = 0,
                Act::Bottom => self.selected = len.saturating_sub(1),
                Act::HalfDown => self.selected = (self.selected + half).min(len.saturating_sub(1)),
                Act::HalfUp => self.selected = self.selected.saturating_sub(half),
                _ => continue,
            }
            self.clamp(len, height);
            return true;
        }
        false
    }

    pub fn clamp(&mut self, len: usize, height: usize) {
        if len == 0 {
            self.selected = 0;
            self.offset = 0;
            return;
        }
        self.selected = self.selected.min(len - 1);
        let height = height.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        self.offset = self.offset.min(len.saturating_sub(height));
    }

    pub fn visible(&self, len: usize, height: usize) -> std::ops::Range<usize> {
        let end = (self.offset + height).min(len);
        self.offset.min(end)..end
    }
}

/// A plain scroll position for read-only text panes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Scroll {
    pub y: usize,
    pub x: usize,
}

impl Scroll {
    pub fn handle(&mut self, acts: &[Act], len: usize, height: usize) -> bool {
        let max = len.saturating_sub(height.max(1));
        let half = (height / 2).max(1);
        for a in acts {
            match a {
                Act::Down => self.y = (self.y + 1).min(max),
                Act::Up => self.y = self.y.saturating_sub(1),
                Act::Top => self.y = 0,
                Act::Bottom => self.y = max,
                Act::HalfDown => self.y = (self.y + half).min(max),
                Act::HalfUp => self.y = self.y.saturating_sub(half),
                Act::Right => self.x += 4,
                Act::Left => self.x = self.x.saturating_sub(4),
                Act::LineStart => self.x = 0,
                _ => continue,
            }
            return true;
        }
        false
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    pub text: String,
    /// Cursor position in chars.
    pub cursor: usize,
}

pub enum InputResult {
    Changed,
    Unchanged,
    Submit,
    Cancel,
}

impl TextInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        Self { text, cursor }
    }

    fn byte(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(self.text.len())
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte(self.cursor);
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(at, &clean);
        self.cursor += clean.chars().count();
    }

    pub fn handle(&mut self, k: &KeyEvent) -> InputResult {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.text.chars().count();
        match k.code {
            KeyCode::Enter => return InputResult::Submit,
            KeyCode::Esc => return InputResult::Cancel,
            KeyCode::Char('c') if ctrl => return InputResult::Cancel,
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.replace_range(..at, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.truncate(at);
            }
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut i = self.cursor;
                while i > 0 && chars[i - 1] == ' ' {
                    i -= 1;
                }
                while i > 0 && chars[i - 1] != ' ' {
                    i -= 1;
                }
                let (a, b) = (self.byte(i), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = i;
            }
            KeyCode::Char(c) if !ctrl => {
                let at = self.byte(self.cursor);
                self.text.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let (a, b) = (self.byte(self.cursor - 1), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let (a, b) = (self.byte(self.cursor), self.byte(self.cursor + 1));
                self.text.replace_range(a..b, "");
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            _ => return InputResult::Unchanged,
        }
        InputResult::Changed
    }

    /// Renders `prefix` + text with a block cursor, scrolled to keep the cursor visible.
    pub fn render(&self, area: Rect, buf: &mut Buffer, prefix: &str, style: Style, focused: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let prefix_w = prefix.width() as u16;
        buf.set_string(area.x, area.y, prefix, style.add_modifier(Modifier::BOLD));
        let avail = area.width.saturating_sub(prefix_w + 1) as usize;
        let chars: Vec<char> = self.text.chars().collect();
        let start = self.cursor.saturating_sub(avail);
        let visible: String = chars[start..].iter().take(avail).collect();
        buf.set_string(area.x + prefix_w, area.y, &visible, style);
        if focused {
            let cx = area.x + prefix_w + (self.cursor - start) as u16;
            if cx < area.x + area.width
                && let Some(cell) = buf.cell_mut((cx, area.y))
            {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                if cell.symbol().is_empty() {
                    cell.set_symbol(" ");
                }
            }
        }
    }
}

/// Fuzzy-scores `haystacks` against `query`, returning matching indices sorted by score.
pub fn fuzzy_filter<S: AsRef<str>>(query: &str, haystacks: &[S]) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..haystacks.len()).collect();
    }
    let mut matcher = Matcher::new(MatcherConfig::DEFAULT);
    // fzf syntax per space-separated atom: 'exact ^prefix suffix$ !negate
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = haystacks
        .iter()
        .enumerate()
        .filter_map(|(i, h)| pattern.score(Utf32Str::new(h.as_ref(), &mut buf), &mut matcher).map(|s| (s, i)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    // Drop scattered, weak matches far below the best one.
    let best = scored.first().map(|(s, _)| *s).unwrap_or(0);
    scored.into_iter().filter(|(s, _)| *s * 4 >= best * 3).map(|(_, i)| i).collect()
}

pub fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

pub fn key_hint(key: &str, label: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(key.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {label}  "), Style::default().fg(Color::Gray)),
    ]
}

pub fn hints_line(pairs: &[(String, &str)]) -> Line<'static> {
    Line::from(pairs.iter().flat_map(|(k, l)| key_hint(k, l)).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent { code, modifiers: m, kind: KeyEventKind::Press, state: KeyEventState::empty() }
    }

    #[test]
    fn list_nav() {
        let mut n = ListNav::default();
        n.handle(&[Act::Bottom], 50, 10);
        assert_eq!((n.selected, n.offset), (49, 40));
        n.handle(&[Act::HalfUp], 50, 10);
        assert_eq!(n.selected, 44);
        n.handle(&[Act::Top], 50, 10);
        assert_eq!((n.selected, n.offset), (0, 0));
        assert!(!n.handle(&[Act::Quit], 50, 10));
        n.clamp(0, 10);
        assert_eq!(n.selected, 0);
    }

    #[test]
    fn text_input_editing() {
        let mut t = TextInput::new("hello wörld");
        t.handle(&key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.text, "hello ");
        t.handle(&key(KeyCode::Char('x'), KeyModifiers::NONE));
        t.handle(&key(KeyCode::Home, KeyModifiers::NONE));
        t.handle(&key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(t.text, "ello x");
        t.handle(&key(KeyCode::End, KeyModifiers::NONE));
        t.handle(&key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(t.text, "ello ");
        assert!(matches!(t.handle(&key(KeyCode::Enter, KeyModifiers::NONE)), InputResult::Submit));
    }

    #[test]
    fn fuzzy() {
        let items = ["Demo Signals", "Node Overview", "Self Monitoring"];
        assert_eq!(fuzzy_filter("node", &items), vec![1]);
        assert_eq!(fuzzy_filter("dsig", &items)[0], 0);
        assert_eq!(fuzzy_filter("", &items).len(), 3);
        assert_eq!(truncate("abcdef", 4), "abc…");
    }
}
