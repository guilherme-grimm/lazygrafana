//! Grafana-style time ranges (`now-6h` → `now`, absolute timestamps), the
//! zoom/pan math used by chart interaction, and the time-range jumplist.

use std::fmt;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};

/// Milliseconds since the Unix epoch.
pub type Millis = i64;

pub fn now_ms() -> Millis {
    Utc::now().timestamp_millis()
}

/// One end of a time range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeExpr {
    /// `now` plus an offset in milliseconds (negative = past), optionally
    /// rounded down to a unit (`now/d`).
    Relative {
        offset: Millis,
        round: Option<char>,
    },
    Absolute(Millis),
}

impl TimeExpr {
    pub fn now() -> Self {
        Self::Relative { offset: 0, round: None }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("now") {
            let (rest, round) = match rest.rfind('/') {
                Some(i) => {
                    let unit = rest[i + 1..].chars().next().ok_or("missing unit after '/'")?;
                    (&rest[..i], Some(unit))
                }
                None => (rest, None),
            };
            let offset = if rest.is_empty() {
                0
            } else if let Some(d) = rest.strip_prefix('-') {
                -parse_duration(d)?
            } else if let Some(d) = rest.strip_prefix('+') {
                parse_duration(d)?
            } else {
                return Err(format!("invalid relative time: {s}"));
            };
            return Ok(Self::Relative { offset, round });
        }
        parse_absolute(s).map(Self::Absolute)
    }

    pub fn resolve(&self, now: Millis) -> Millis {
        match *self {
            Self::Absolute(ms) => ms,
            Self::Relative { offset, round } => {
                let t = now + offset;
                match round {
                    Some(unit) => round_down(t, unit),
                    None => t,
                }
            }
        }
    }

    pub fn is_relative(&self) -> bool {
        matches!(self, Self::Relative { .. })
    }
}

impl fmt::Display for TimeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Absolute(ms) => write!(f, "{}", format_absolute(ms)),
            Self::Relative { offset, round } => {
                write!(f, "now")?;
                if offset < 0 {
                    write!(f, "-{}", format_duration(-offset))?;
                } else if offset > 0 {
                    write!(f, "+{}", format_duration(offset))?;
                }
                if let Some(u) = round {
                    write!(f, "/{u}")?;
                }
                Ok(())
            }
        }
    }
}

fn round_down(t: Millis, unit: char) -> Millis {
    let dt = Local.timestamp_millis_opt(t).single().unwrap_or_else(Local::now);
    let naive = dt.naive_local();
    let date = naive.date();
    let rounded = match unit {
        's' => naive.with_nanosecond_floor(),
        'm' => date.and_hms_opt(naive.hour(), naive.minute(), 0),
        'h' => date.and_hms_opt(naive.hour(), 0, 0),
        'd' => date.and_hms_opt(0, 0, 0),
        'w' => {
            let back = date.weekday().num_days_from_monday() as i64;
            (date - chrono::Duration::days(back)).and_hms_opt(0, 0, 0)
        }
        'M' => date.with_day(1).and_then(|d| d.and_hms_opt(0, 0, 0)),
        'y' => date.with_day(1).and_then(|d| d.with_month(1)).and_then(|d| d.and_hms_opt(0, 0, 0)),
        _ => Some(naive),
    };
    rounded.and_then(|n| Local.from_local_datetime(&n).earliest()).map(|d| d.timestamp_millis()).unwrap_or(t)
}

use chrono::{Datelike, Timelike};

trait FloorSecond {
    fn with_nanosecond_floor(&self) -> Option<NaiveDateTime>;
}

impl FloorSecond for NaiveDateTime {
    fn with_nanosecond_floor(&self) -> Option<NaiveDateTime> {
        self.with_nanosecond(0)
    }
}

/// Parses Grafana/Prometheus style durations: `30s`, `5m`, `1h30m`, `7d`, `2w`, `1M`, `1y`, `500ms`.
pub fn parse_duration(s: &str) -> Result<Millis, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let mut total: Millis = 0;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if digits == 0 {
            return Err(format!("invalid duration: {s}"));
        }
        let n: Millis = rest[..digits].parse().map_err(|_| format!("invalid duration: {s}"))?;
        rest = &rest[digits..];
        let (unit_ms, len) = if rest.starts_with("ms") {
            (1, 2)
        } else {
            let unit = rest.chars().next().ok_or_else(|| format!("missing unit in duration: {s}"))?;
            let ms = match unit {
                's' => 1_000,
                'm' => 60_000,
                'h' => 3_600_000,
                'd' => 86_400_000,
                'w' => 7 * 86_400_000,
                'M' => 30 * 86_400_000,
                'y' => 365 * 86_400_000,
                _ => return Err(format!("unknown duration unit '{unit}' in {s}")),
            };
            (ms, 1)
        };
        total += n * unit_ms;
        rest = &rest[len..];
    }
    Ok(total)
}

/// Formats a duration with the largest unit that divides it exactly, falling
/// back to compound form (`1h30m`).
pub fn format_duration(ms: Millis) -> String {
    const UNITS: [(Millis, &str); 7] =
        [(365 * 86_400_000, "y"), (7 * 86_400_000, "w"), (86_400_000, "d"), (3_600_000, "h"), (60_000, "m"), (1_000, "s"), (1, "ms")];
    if ms == 0 {
        return "0s".into();
    }
    let mut out = String::new();
    let mut rest = ms;
    for (size, name) in UNITS {
        // Weeks only when exact, otherwise "10d" reads better than "1w3d".
        if name == "w" && rest % size != 0 {
            continue;
        }
        if rest >= size {
            out.push_str(&format!("{}{name}", rest / size));
            rest %= size;
        }
        if rest == 0 {
            break;
        }
    }
    out
}

/// A short human duration for display, rounded to at most two units (`2h 5m`).
pub fn humanize_duration(ms: Millis) -> String {
    let secs = ms / 1000;
    let (d, h, m, s) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m {s}s"),
        (0, _, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

fn parse_absolute(s: &str) -> Result<Millis, String> {
    if let Ok(ms) = s.parse::<i64>() {
        // Grafana URLs use epoch milliseconds.
        return Ok(if ms < 100_000_000_000 { ms * 1000 } else { ms });
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return Local
                .from_local_datetime(&n)
                .earliest()
                .map(|d| d.timestamp_millis())
                .ok_or_else(|| format!("invalid local time: {s}"));
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let n = d.and_hms_opt(0, 0, 0).expect("midnight is valid");
        return Local.from_local_datetime(&n).earliest().map(|d| d.timestamp_millis()).ok_or_else(|| format!("invalid local time: {s}"));
    }
    Err(format!("unrecognised time: {s} (try now-6h, 2026-10-14 13:00, or RFC 3339)"))
}

pub fn format_absolute(ms: Millis) -> String {
    Local.timestamp_millis_opt(ms).single().map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_else(|| ms.to_string())
}

/// Formats a timestamp for an axis tick, choosing precision from the visible span.
pub fn format_tick(ms: Millis, span: Millis) -> String {
    let Some(d) = Local.timestamp_millis_opt(ms).single() else {
        return String::new();
    };
    if span <= 5 * 60_000 {
        d.format("%H:%M:%S").to_string()
    } else if span <= 2 * 86_400_000 {
        d.format("%H:%M").to_string()
    } else if span <= 60 * 86_400_000 {
        d.format("%m/%d %H:%M").to_string()
    } else {
        d.format("%Y-%m-%d").to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeRange {
    pub from: TimeExpr,
    pub to: TimeExpr,
}

impl TimeRange {
    pub fn parse(from: &str, to: &str) -> Result<Self, String> {
        Ok(Self { from: TimeExpr::parse(from)?, to: TimeExpr::parse(to)? })
    }

    /// `now-6h` shorthand: a range ending now.
    pub fn last(ms: Millis) -> Self {
        Self { from: TimeExpr::Relative { offset: -ms, round: None }, to: TimeExpr::now() }
    }

    pub fn absolute(from: Millis, to: Millis) -> Self {
        let (from, to) = if from <= to { (from, to) } else { (to, from) };
        Self { from: TimeExpr::Absolute(from), to: TimeExpr::Absolute(to.max(from + 1000)) }
    }

    /// Parses user input from the time picker / `:time`: either `<from>` (to = now),
    /// `<from> to <to>`, or `<from>,<to>`.
    pub fn parse_input(s: &str) -> Result<Self, String> {
        let s = s.trim();
        for sep in [" to ", ","] {
            if let Some((a, b)) = s.split_once(sep) {
                return Self::parse(a, b);
            }
        }
        let from = if s.starts_with("now") || s.chars().next().is_some_and(|c| c.is_ascii_digit()) && !looks_like_duration(s) {
            TimeExpr::parse(s)?
        } else {
            // Bare durations ("6h") mean "last 6h".
            TimeExpr::Relative { offset: -parse_duration(s)?, round: None }
        };
        Ok(Self { from, to: TimeExpr::now() })
    }

    pub fn resolve(&self, now: Millis) -> (Millis, Millis) {
        let (a, b) = (self.from.resolve(now), self.to.resolve(now));
        if a < b { (a, b) } else { (b, a.max(b + 1000)) }
    }

    pub fn span(&self, now: Millis) -> Millis {
        let (a, b) = self.resolve(now);
        b - a
    }

    pub fn is_relative(&self) -> bool {
        self.from.is_relative() && self.to.is_relative()
    }

    /// Zoom around `center` by `factor` (< 1 zooms in). Always yields an absolute range.
    pub fn zoom(&self, now: Millis, center: Option<Millis>, factor: f64) -> Self {
        let (a, b) = self.resolve(now);
        let c = center.unwrap_or((a + b) / 2).clamp(a, b);
        let left = ((c - a) as f64 * factor) as Millis;
        let right = ((b - c) as f64 * factor) as Millis;
        let (mut na, mut nb) = (c - left, c + right);
        // Never zoom out into the future: slide the window back instead.
        if nb > now && b <= now {
            na -= nb - now;
            nb = now;
        }
        Self::absolute(na, nb)
    }

    /// Shift by a fraction of the span (positive = forward in time).
    pub fn pan(&self, now: Millis, fraction: f64) -> Self {
        let (a, b) = self.resolve(now);
        let delta = ((b - a) as f64 * fraction) as Millis;
        Self::absolute(a + delta, b + delta)
    }

    /// Query parameters as Grafana URLs expect them.
    pub fn url_params(&self, now: Millis) -> (String, String) {
        let enc = |e: &TimeExpr| match e {
            TimeExpr::Absolute(ms) => ms.to_string(),
            rel => rel.to_string(),
        };
        let _ = now;
        (enc(&self.from), enc(&self.to))
    }
}

impl fmt::Display for TimeRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.from, &self.to) {
            (from, TimeExpr::Relative { offset: 0, round: None }) if from.is_relative() => write!(f, "{from}"),
            (from, to) => write!(f, "{from} → {to}"),
        }
    }
}

fn looks_like_duration(s: &str) -> bool {
    parse_duration(s).is_ok()
}

/// Grafana's time-picker presets.
pub const PRESETS: &[(&str, &str)] = &[
    ("now-5m", "Last 5 minutes"),
    ("now-15m", "Last 15 minutes"),
    ("now-30m", "Last 30 minutes"),
    ("now-1h", "Last 1 hour"),
    ("now-3h", "Last 3 hours"),
    ("now-6h", "Last 6 hours"),
    ("now-12h", "Last 12 hours"),
    ("now-24h", "Last 24 hours"),
    ("now-2d", "Last 2 days"),
    ("now-7d", "Last 7 days"),
    ("now-30d", "Last 30 days"),
    ("now/d", "Today so far"),
    ("now/w", "This week so far"),
];

/// nvim-style jumplist of time ranges (`Ctrl-o` / `Ctrl-i`).
#[derive(Debug, Clone)]
pub struct Jumplist {
    entries: Vec<TimeRange>,
    pos: usize,
}

impl Jumplist {
    pub fn new(initial: TimeRange) -> Self {
        Self { entries: vec![initial], pos: 0 }
    }

    pub fn current(&self) -> &TimeRange {
        &self.entries[self.pos]
    }

    /// Records a new range, discarding any "forward" history.
    pub fn push(&mut self, range: TimeRange) {
        if *self.current() == range {
            return;
        }
        self.entries.truncate(self.pos + 1);
        self.entries.push(range);
        if self.entries.len() > 100 {
            self.entries.remove(0);
        }
        self.pos = self.entries.len() - 1;
    }

    pub fn back(&mut self) -> bool {
        if self.pos > 0 {
            self.pos -= 1;
            true
        } else {
            false
        }
    }

    pub fn forward(&mut self) -> bool {
        if self.pos + 1 < self.entries.len() {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub fn position(&self) -> (usize, usize) {
        (self.pos + 1, self.entries.len())
    }
}

/// Picks a step (ms) close to `span / points`, rounded to a "nice" Prometheus-friendly value
/// and never below `min_step`.
pub fn nice_step(span: Millis, points: u32, min_step: Millis) -> Millis {
    const NICE: [Millis; 20] = [
        1_000,
        2_000,
        5_000,
        10_000,
        15_000,
        30_000,
        60_000,
        120_000,
        300_000,
        600_000,
        900_000,
        1_200_000,
        1_800_000,
        3_600_000,
        7_200_000,
        10_800_000,
        21_600_000,
        43_200_000,
        86_400_000,
        604_800_000,
    ];
    let raw = span / points.max(1) as Millis;
    let step = NICE.iter().copied().find(|&n| n >= raw).unwrap_or(raw);
    step.max(min_step)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Millis = 1_760_450_000_000;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("30s"), Ok(30_000));
        assert_eq!(parse_duration("1h30m"), Ok(5_400_000));
        assert_eq!(parse_duration("500ms"), Ok(500));
        assert_eq!(parse_duration("7d"), Ok(7 * 86_400_000));
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(5_400_000), "1h30m");
        assert_eq!(format_duration(7 * 86_400_000), "1w");
        assert_eq!(format_duration(10 * 86_400_000), "10d");
        assert_eq!(format_duration(21_600_000), "6h");
    }

    #[test]
    fn relative_expressions_roundtrip() {
        for s in ["now", "now-6h", "now-1h30m", "now+5m", "now/d", "now-1d/d"] {
            assert_eq!(TimeExpr::parse(s).unwrap().to_string(), s);
        }
        assert_eq!(TimeExpr::parse("now-6h").unwrap().resolve(NOW), NOW - 21_600_000);
    }

    #[test]
    fn absolute_expressions() {
        assert_eq!(TimeExpr::parse("1760450000000").unwrap(), TimeExpr::Absolute(NOW));
        assert_eq!(TimeExpr::parse("1760450000").unwrap(), TimeExpr::Absolute(NOW));
        assert_eq!(TimeExpr::parse("2025-10-14T13:53:20Z").unwrap(), TimeExpr::Absolute(NOW));
        assert!(TimeExpr::parse("2026-10-14 13:00").is_ok());
        assert!(TimeExpr::parse("yesterday").is_err());
    }

    #[test]
    fn parse_input_forms() {
        assert_eq!(TimeRange::parse_input("6h").unwrap(), TimeRange::last(21_600_000));
        assert_eq!(TimeRange::parse_input("now-6h").unwrap(), TimeRange::last(21_600_000));
        let r = TimeRange::parse_input("now-2d to now-1d").unwrap();
        assert_eq!(r.resolve(NOW), (NOW - 2 * 86_400_000, NOW - 86_400_000));
        assert!(TimeRange::parse_input("2026-10-14 13:00, 2026-10-14 14:00").is_ok());
    }

    #[test]
    fn zoom_and_pan() {
        let r = TimeRange::absolute(NOW - 4_000_000, NOW);
        let z = r.zoom(NOW, Some(NOW - 2_000_000), 0.5);
        assert_eq!(z.resolve(NOW), (NOW - 3_000_000, NOW - 1_000_000));
        let p = z.pan(NOW, -0.5);
        assert_eq!(p.resolve(NOW), (NOW - 4_000_000, NOW - 2_000_000));
        let out = z.zoom(NOW, None, 2.0);
        assert_eq!(out.span(NOW), 4_000_000);
    }

    #[test]
    fn jumplist_navigation() {
        let mut j = Jumplist::new(TimeRange::last(3_600_000));
        j.push(TimeRange::absolute(1, 2_000));
        j.push(TimeRange::absolute(1, 3_000));
        assert!(j.back());
        assert_eq!(j.current(), &TimeRange::absolute(1, 2_000));
        j.push(TimeRange::absolute(5, 9_000));
        assert!(!j.forward());
        assert_eq!(j.position(), (3, 3));
        assert!(j.back() && j.back());
        assert!(!j.back());
        assert_eq!(j.current(), &TimeRange::last(3_600_000));
    }

    #[test]
    fn nice_steps() {
        assert_eq!(nice_step(7 * 86_400_000, 1000, 15_000), 900_000);
        assert_eq!(nice_step(3_600_000, 1000, 15_000), 15_000);
    }

    #[test]
    fn display() {
        assert_eq!(TimeRange::last(21_600_000).to_string(), "now-6h");
        assert!(TimeRange::absolute(NOW - 1000, NOW).to_string().contains('→'));
    }
}
