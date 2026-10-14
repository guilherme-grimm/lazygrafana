//! Terminal integrations outside ratatui's buffer: OSC 52 clipboard, desktop
//! notifications, and terminal setup/teardown.

use std::io::{self, Write};

use base64::Engine as _;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};

fn in_tmux() -> bool {
    std::env::var("TMUX").is_ok_and(|v| !v.is_empty())
}

/// Wraps an escape sequence for tmux passthrough when needed.
fn passthrough(seq: &str) -> String {
    if in_tmux() { format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b")) } else { seq.to_string() }
}

/// Copies text to the clipboard. OSC 52 reaches the *local* clipboard even over
/// SSH; locally we also try the system clipboard directly.
pub fn copy(text: &str) -> bool {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = passthrough(&format!("\x1b]52;c;{b64}\x07"));
    let osc_ok = io::stdout().write_all(seq.as_bytes()).and_then(|_| io::stdout().flush()).is_ok();
    let local_ok =
        std::env::var("SSH_CONNECTION").is_err() && arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())).is_ok();
    osc_ok || local_ok
}

/// Sends a desktop notification: OSC 99 on kitty, OSC 777 on Ghostty/foot/WezTerm, OSC 9 otherwise.
pub fn notify(title: &str, body: &str) {
    let clean = |s: &str| s.replace(['\x1b', '\x07', ';'], " ");
    let (title, body) = (clean(title), clean(body));
    let term = std::env::var("TERM").unwrap_or_default();
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let seq = if term.contains("kitty") || std::env::var("KITTY_WINDOW_ID").is_ok() {
        format!("\x1b]99;i=lg:d=0;{title}\x1b\\\x1b]99;i=lg:d=1:p=body;{body}\x1b\\")
    } else if program.contains("ghostty") || term.contains("ghostty") || term.starts_with("foot") || program.contains("wezterm") {
        format!("\x1b]777;notify;{title};{body}\x1b\\")
    } else {
        format!("\x1b]9;{title}: {body}\x07")
    };
    let _ = io::stdout().write_all(passthrough(&seq).as_bytes());
    let _ = io::stdout().flush();
}

pub fn enter(mouse: bool) -> io::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    // Kitty keyboard protocol: disambiguates Ctrl-i/Tab, Ctrl-m/Enter, etc. Ignored where unsupported.
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        queue!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
        out.flush()?;
    }
    Ok(())
}

pub fn leave() -> io::Result<()> {
    let mut out = io::stdout();
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste);
    // Delete all kitty images we may have placed.
    let _ = out.write_all(passthrough("\x1b_Ga=d,d=A\x1b\\").as_bytes());
    execute!(out, LeaveAlternateScreen)?;
    disable_raw_mode()
}

/// Restores the terminal if we panic mid-frame.
pub fn install_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave();
        hook(info);
    }));
}
