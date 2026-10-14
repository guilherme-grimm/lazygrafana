//! nvim-flavoured key bindings with multi-key sequences (`gg`, `dd`, `yy`, …)
//! and per-action overrides from the config file.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Every bindable action. A key may map to several actions; the focused view
/// picks the first one it understands (e.g. `t` = health check or table toggle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Act {
    Up,
    Down,
    Left,
    Right,
    Top,
    Bottom,
    HalfDown,
    HalfUp,
    NextPanel,
    PrevPanel,
    Focus1,
    Focus2,
    Focus3,
    Focus4,
    Focus5,
    PrevTab,
    NextTab,
    Filter,
    NextMatch,
    PrevMatch,
    Command,
    Mark,
    Visual,
    YankUrl,
    YankUid,
    YankJson,
    YankRange,
    OpenBrowser,
    ContextSwitch,
    TimePicker,
    RefreshPicker,
    JumpBack,
    JumpForward,
    Help,
    Back,
    Quit,
    Enter,
    Edit,
    Star,
    Move,
    Export,
    Delete,
    ExactRender,
    Silence,
    Unsilence,
    Pause,
    GroupBy,
    StateFilter,
    Test,
    ToggleTable,
    Diff,
    Restore,
    Follow,
    Wrap,
    PrettyJson,
    History,
    WordForward,
    WordBack,
    LineStart,
    LineEnd,
    ZoomIn,
    ZoomOut,
    PanLeft,
    PanRight,
    LogsForSelection,
    Annotate,
    MaxOverTime,
    Maximize,
    Refresh,
    Insert,
    PickDatasource,
}

impl Act {
    pub fn name(self) -> &'static str {
        DEFAULTS.iter().find(|(a, _, _)| *a == self).map(|(_, n, _)| *n).unwrap_or("?")
    }
}

/// `(action, config name, default keys)`. Several entries may share keys.
pub const DEFAULTS: &[(Act, &str, &[&str])] = &[
    (Act::Up, "up", &["k", "Up"]),
    (Act::Down, "down", &["j", "Down"]),
    (Act::Left, "left", &["h", "Left"]),
    (Act::Right, "right", &["l", "Right"]),
    (Act::Top, "top", &["gg", "Home"]),
    (Act::Bottom, "bottom", &["G", "End"]),
    (Act::HalfDown, "half_page_down", &["C-d", "PageDown"]),
    (Act::HalfUp, "half_page_up", &["C-u", "PageUp"]),
    (Act::NextPanel, "next_panel", &["Tab"]),
    (Act::PrevPanel, "prev_panel", &["BackTab"]),
    (Act::Focus1, "focus_contexts", &["1"]),
    (Act::Focus2, "focus_dashboards", &["2"]),
    (Act::Focus3, "focus_alerts", &["3"]),
    (Act::Focus4, "focus_explore", &["4"]),
    (Act::Focus5, "focus_datasources", &["5"]),
    (Act::PrevTab, "prev_tab", &["["]),
    (Act::NextTab, "next_tab", &["]"]),
    (Act::Filter, "filter", &["/"]),
    (Act::NextMatch, "next_match", &["n"]),
    (Act::PrevMatch, "prev_match", &["N"]),
    (Act::Command, "command", &[":"]),
    (Act::Mark, "mark", &["Space"]),
    (Act::Visual, "visual", &["v"]),
    (Act::YankUrl, "yank_url", &["yy"]),
    (Act::YankUid, "yank_uid", &["yu"]),
    (Act::YankJson, "yank_json", &["yj"]),
    (Act::YankRange, "yank_range", &["yr"]),
    (Act::OpenBrowser, "open_browser", &["o"]),
    (Act::ContextSwitch, "context_switch", &["@"]),
    (Act::TimePicker, "time_picker", &["T"]),
    (Act::RefreshPicker, "refresh_picker", &["R"]),
    (Act::JumpBack, "jump_back", &["C-o"]),
    (Act::JumpForward, "jump_forward", &["C-i"]),
    (Act::Help, "help", &["?"]),
    (Act::Back, "back", &["Esc"]),
    (Act::Quit, "quit", &["q", "C-c"]),
    (Act::Enter, "select", &["Enter"]),
    (Act::Edit, "edit", &["e"]),
    (Act::Star, "star", &["s"]),
    (Act::Move, "move", &["m"]),
    (Act::Export, "export", &["x"]),
    (Act::Delete, "delete", &["dd"]),
    (Act::ExactRender, "exact_render", &["P"]),
    (Act::Silence, "silence", &["S"]),
    (Act::Unsilence, "unsilence", &["U"]),
    (Act::Pause, "pause", &["p"]),
    (Act::GroupBy, "group_by", &["gb"]),
    (Act::StateFilter, "state_filter", &["F"]),
    (Act::Test, "health_check", &["t"]),
    (Act::ToggleTable, "toggle_table", &["t"]),
    (Act::Diff, "diff", &["c"]),
    (Act::Restore, "restore", &["r"]),
    (Act::Follow, "follow", &["f"]),
    (Act::Wrap, "wrap", &["w"]),
    (Act::PrettyJson, "pretty_json", &["J"]),
    (Act::History, "history", &["C-r"]),
    (Act::WordForward, "word_forward", &["w"]),
    (Act::WordBack, "word_back", &["b"]),
    (Act::LineStart, "line_start", &["0"]),
    (Act::LineEnd, "line_end", &["$"]),
    (Act::ZoomIn, "zoom_in", &["+", "="]),
    (Act::ZoomOut, "zoom_out", &["-"]),
    (Act::PanLeft, "pan_left", &["H"]),
    (Act::PanRight, "pan_right", &["L"]),
    (Act::LogsForSelection, "logs_for_selection", &["gl"]),
    (Act::Annotate, "annotate", &["a"]),
    (Act::MaxOverTime, "max_over_time", &["M"]),
    (Act::Maximize, "maximize", &["z"]),
    (Act::Refresh, "refresh", &["C-l", "F5"]),
    (Act::Insert, "insert", &["i"]),
    (Act::PickDatasource, "pick_datasource", &["D"]),
];

pub type Seq = Vec<String>;

/// Converts a key event into a token (`j`, `G`, `C-o`, `Enter`, `Space`, …).
pub fn token(k: &KeyEvent) -> Option<String> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let base = match k.code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) if ctrl => format!("C-{}", c.to_ascii_lowercase()),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Tab if ctrl => "C-i".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "BackTab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => return None,
    };
    Some(if alt { format!("M-{base}") } else { base })
}

/// Parses a binding like `gg`, `C-o`, `Space`, `F5` into tokens.
pub fn parse_seq(s: &str) -> Seq {
    const NAMES: [&str; 17] = [
        "Space",
        "Enter",
        "Esc",
        "Tab",
        "BackTab",
        "Backspace",
        "Up",
        "Down",
        "Left",
        "Right",
        "PageUp",
        "PageDown",
        "Home",
        "End",
        "Delete",
        "C-",
        "F",
    ];
    if s.starts_with("C-")
        || s.starts_with("M-")
        || NAMES[..15].contains(&s)
        || (s.starts_with('F') && s.len() > 1 && s[1..].parse::<u8>().is_ok())
    {
        return vec![s.to_string()];
    }
    s.chars().map(|c| c.to_string()).collect()
}

#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: BTreeMap<Seq, Vec<Act>>,
    pub warnings: Vec<String>,
}

pub enum Lookup {
    /// Complete match.
    Acts(Vec<Act>),
    /// Prefix of a longer binding: wait for more keys.
    Pending,
    None,
}

impl Keymap {
    pub fn new(overrides: &BTreeMap<String, String>) -> Self {
        let mut warnings = Vec::new();
        let mut keys: Vec<(Act, Vec<String>)> = DEFAULTS.iter().map(|(a, _, k)| (*a, k.iter().map(|s| s.to_string()).collect())).collect();
        for (name, key) in overrides {
            match keys.iter_mut().find(|(a, _)| a.name() == name) {
                Some((_, ks)) => *ks = vec![key.clone()],
                None => warnings.push(format!("unknown key action '{name}' in config")),
            }
        }
        let mut bindings: BTreeMap<Seq, Vec<Act>> = BTreeMap::new();
        for (act, ks) in keys {
            for k in ks {
                bindings.entry(parse_seq(&k)).or_default().push(act);
            }
        }
        Self { bindings, warnings }
    }

    pub fn lookup(&self, seq: &[String]) -> Lookup {
        if let Some(acts) = self.bindings.get(seq) {
            // `g` alone is not bound, but `gg` is: an exact match wins only if nothing longer exists
            // or the exact match is not itself a prefix.
            let longer = self.bindings.keys().any(|k| k.len() > seq.len() && k.starts_with(seq));
            if !longer {
                return Lookup::Acts(acts.clone());
            }
        }
        if self.bindings.keys().any(|k| k.len() > seq.len() && k.starts_with(seq)) {
            return Lookup::Pending;
        }
        Lookup::None
    }

    /// The first key bound to `act`, for hints.
    pub fn key_for(&self, act: Act) -> String {
        self.bindings
            .iter()
            .filter(|(_, acts)| acts.contains(&act))
            .map(|(seq, _)| seq.join(""))
            .min_by_key(|s| s.len())
            .unwrap_or_else(|| "?".into())
    }

    /// All bindings for the help screen: (keys, action name).
    pub fn describe(&self) -> Vec<(String, &'static str)> {
        let mut out: Vec<(String, &'static str)> = Vec::new();
        for (act, name, _) in DEFAULTS {
            let keys: Vec<String> = self.bindings.iter().filter(|(_, a)| a.contains(act)).map(|(s, _)| s.join("")).collect();
            out.push((keys.join(" "), name));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(s: &str) -> Seq {
        parse_seq(s)
    }

    #[test]
    fn sequences() {
        let k = Keymap::new(&BTreeMap::new());
        assert!(matches!(k.lookup(&seq("g")), Lookup::Pending));
        assert!(matches!(k.lookup(&seq("gg")), Lookup::Acts(a) if a == vec![Act::Top]));
        assert!(matches!(k.lookup(&seq("d")), Lookup::Pending));
        assert!(matches!(k.lookup(&seq("dd")), Lookup::Acts(a) if a == vec![Act::Delete]));
        assert!(matches!(k.lookup(&seq("t")), Lookup::Acts(a) if a.contains(&Act::Test) && a.contains(&Act::ToggleTable)));
        assert!(matches!(k.lookup(&seq("Z")), Lookup::None));
        assert!(matches!(k.lookup(&["C-o".to_string()]), Lookup::Acts(a) if a == vec![Act::JumpBack]));
    }

    #[test]
    fn overrides() {
        let k = Keymap::new(&[("silence".to_string(), "X".to_string()), ("bogus".to_string(), "Q".to_string())].into());
        assert!(matches!(k.lookup(&seq("X")), Lookup::Acts(a) if a == vec![Act::Silence]));
        assert!(matches!(k.lookup(&seq("S")), Lookup::None));
        assert_eq!(k.warnings.len(), 1);
        assert_eq!(k.key_for(Act::Silence), "X");
    }

    #[test]
    fn tokens() {
        use crossterm::event::{KeyEventKind, KeyEventState};
        let ev = |code, modifiers| KeyEvent { code, modifiers, kind: KeyEventKind::Press, state: KeyEventState::empty() };
        assert_eq!(token(&ev(KeyCode::Char('o'), KeyModifiers::CONTROL)).unwrap(), "C-o");
        assert_eq!(token(&ev(KeyCode::Char('G'), KeyModifiers::SHIFT)).unwrap(), "G");
        assert_eq!(token(&ev(KeyCode::Char(' '), KeyModifiers::NONE)).unwrap(), "Space");
        assert_eq!(parse_seq("C-d"), vec!["C-d"]);
        assert_eq!(parse_seq("Space"), vec!["Space"]);
        assert_eq!(parse_seq("yy"), vec!["y", "y"]);
        assert_eq!(parse_seq("F5"), vec!["F5"]);
    }
}
