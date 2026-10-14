//! Value formatting for Grafana units, and Grafana's named colours.

use ratatui::style::Color;

/// Formats `v` according to a Grafana unit id (`percent`, `bytes`, `s`, `reqps`, …).
pub fn format(v: f64, unit: Option<&str>, decimals: Option<u32>) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "+Inf".into() } else { "-Inf".into() };
    }
    let unit = unit.unwrap_or("").trim();
    match unit {
        "" | "none" | "short" => format_short(v, decimals),
        "percent" => format!("{}%", fixed(v, decimals.unwrap_or(auto_decimals(v)))),
        "percentunit" => format!("{}%", fixed(v * 100.0, decimals.unwrap_or(auto_decimals(v * 100.0)))),
        "bytes" | "decbytes" => scaled(
            v,
            if unit == "bytes" { 1024.0 } else { 1000.0 },
            &["B", "KiB", "MiB", "GiB", "TiB", "PiB"],
            &["B", "kB", "MB", "GB", "TB", "PB"],
            unit == "bytes",
            decimals,
        ),
        "bits" | "decbits" => scaled(v, 1000.0, &[], &["b", "kb", "Mb", "Gb", "Tb", "Pb"], false, decimals),
        "Bps" | "binBps" => format!(
            "{}/s",
            scaled(
                v,
                if unit == "binBps" { 1024.0 } else { 1000.0 },
                &["B", "KiB", "MiB", "GiB", "TiB"],
                &["B", "kB", "MB", "GB", "TB"],
                unit == "binBps",
                decimals
            )
        ),
        "bps" => scaled(v, 1000.0, &[], &["bps", "kbps", "Mbps", "Gbps", "Tbps"], false, decimals),
        "s" => duration(v, decimals),
        "ms" => duration(v / 1000.0, decimals),
        "µs" | "us" => duration(v / 1_000_000.0, decimals),
        "ns" => duration(v / 1_000_000_000.0, decimals),
        "dtdurations" | "dtdurationms" => {
            let ms = if unit == "dtdurationms" { v } else { v * 1000.0 };
            crate::timerange::humanize_duration(ms as i64)
        }
        "reqps" => format!("{} req/s", format_short(v, decimals)),
        "rps" => format!("{} rd/s", format_short(v, decimals)),
        "wps" => format!("{} wr/s", format_short(v, decimals)),
        "ops" | "opm" => format!("{} {}", format_short(v, decimals), if unit == "ops" { "ops/s" } else { "ops/min" }),
        "iops" => format!("{} io/s", format_short(v, decimals)),
        "celsius" => format!("{}°C", fixed(v, decimals.unwrap_or(1))),
        "fahrenheit" => format!("{}°F", fixed(v, decimals.unwrap_or(1))),
        "currencyUSD" => format!("${}", format_short(v, decimals.or(Some(2)))),
        "currencyEUR" => format!("€{}", format_short(v, decimals.or(Some(2)))),
        "bool" => (if v != 0.0 { "true" } else { "false" }).into(),
        "dateTimeAsIso" | "dateTimeAsSystem" => crate::timerange::format_absolute(v as i64),
        u if u.starts_with("suffix:") => format!("{}{}", format_short(v, decimals), &u[7..]),
        u if u.starts_with("prefix:") => format!("{}{}", &u[7..], format_short(v, decimals)),
        // Unknown units: show the value with the unit id as a suffix, like Grafana's custom units.
        u => format!("{} {u}", format_short(v, decimals)),
    }
}

/// Grafana's "short" format: SI-ish suffixes (K, Mil, Bil) with ~3 significant digits.
pub fn format_short(v: f64, decimals: Option<u32>) -> String {
    let abs = v.abs();
    let (scaled, suffix) = if abs >= 1e15 {
        (v / 1e15, " Quadr")
    } else if abs >= 1e12 {
        (v / 1e12, " Tri")
    } else if abs >= 1e9 {
        (v / 1e9, " Bil")
    } else if abs >= 1e6 {
        (v / 1e6, " Mil")
    } else if abs >= 1e3 {
        (v / 1e3, " K")
    } else {
        (v, "")
    };
    format!("{}{suffix}", fixed(scaled, decimals.unwrap_or(auto_decimals(scaled))))
}

fn auto_decimals(v: f64) -> u32 {
    let a = v.abs();
    if a == 0.0 || a >= 100.0 {
        0
    } else if a >= 10.0 {
        1
    } else if a >= 1.0 {
        2
    } else if a >= 0.01 {
        3
    } else {
        4
    }
}

/// Fixed decimals with trailing zeros removed (`1.50` → `1.5`, `2.00` → `2`).
fn fixed(v: f64, decimals: u32) -> String {
    let s = format!("{v:.*}", decimals as usize);
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

fn scaled(v: f64, base: f64, iec: &[&str], si: &[&str], use_iec: bool, decimals: Option<u32>) -> String {
    let names = if use_iec && !iec.is_empty() { iec } else { si };
    let mut x = v;
    let mut i = 0;
    while x.abs() >= base && i + 1 < names.len() {
        x /= base;
        i += 1;
    }
    format!("{} {}", fixed(x, decimals.unwrap_or(auto_decimals(x))), names[i])
}

fn duration(secs: f64, decimals: Option<u32>) -> String {
    let a = secs.abs();
    let (x, u) = if a == 0.0 {
        (0.0, "s")
    } else if a < 1e-6 {
        (secs * 1e9, "ns")
    } else if a < 1e-3 {
        (secs * 1e6, "µs")
    } else if a < 1.0 {
        (secs * 1e3, "ms")
    } else if a < 60.0 {
        (secs, "s")
    } else if a < 3600.0 {
        (secs / 60.0, "min")
    } else if a < 86_400.0 {
        (secs / 3600.0, "hour")
    } else {
        (secs / 86_400.0, "day")
    };
    format!("{} {u}", fixed(x, decimals.unwrap_or(auto_decimals(x))))
}

// ---- colours ---------------------------------------------------------------

/// Grafana's classic series palette.
pub const PALETTE: [(u8, u8, u8); 12] = [
    (0x73, 0xBF, 0x69), // green
    (0xF2, 0xCC, 0x0C), // yellow
    (0x8A, 0xB8, 0xFF), // blue
    (0xFF, 0x78, 0x0A), // orange
    (0xF2, 0x49, 0x5C), // red
    (0x57, 0x94, 0xF2), // dark blue
    (0xB8, 0x77, 0xD9), // purple
    (0x70, 0x5D, 0xA0), // violet
    (0x37, 0x87, 0x2D), // dark green
    (0xFA, 0xDE, 0x2A), // light yellow
    (0x6E, 0xD0, 0xE0), // cyan
    (0xEF, 0x84, 0x3C), // light orange
];

pub fn palette(i: usize) -> (u8, u8, u8) {
    PALETTE[i % PALETTE.len()]
}

/// Resolves Grafana colour names (`green`, `semi-dark-red`, `#ff0000`, `rgba(…)`) to RGB.
pub fn named_rgb(name: &str) -> Option<(u8, u8, u8)> {
    let n = name.trim().to_ascii_lowercase();
    if let Some(hex) = n.strip_prefix('#') {
        let hex = if hex.len() == 3 { hex.chars().flat_map(|c| [c, c]).collect::<String>() } else { hex.to_string() };
        if hex.len() >= 6 {
            let p = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
            return Some((p(0)?, p(2)?, p(4)?));
        }
        return None;
    }
    if let Some(inner) = n.strip_prefix("rgba(").or_else(|| n.strip_prefix("rgb(")) {
        let parts: Vec<u8> = inner.trim_end_matches(')').split(',').take(3).filter_map(|p| p.trim().parse().ok()).collect();
        return (parts.len() == 3).then(|| (parts[0], parts[1], parts[2]));
    }
    let (shade, base) = ["super-light-", "light-", "semi-dark-", "dark-"]
        .iter()
        .find_map(|p| n.strip_prefix(p).map(|b| (*p, b)))
        .unwrap_or(("", n.as_str()));
    let rgb = match (base, shade) {
        ("red", "dark-") => (0xC4, 0x16, 0x2A),
        ("red", "semi-dark-") => (0xE0, 0x2F, 0x44),
        ("red", "light-") => (0xFF, 0x73, 0x83),
        ("red", "super-light-") => (0xFF, 0xA6, 0xB0),
        ("red", _) => (0xF2, 0x49, 0x5C),
        ("orange", "dark-") => (0xFA, 0x64, 0x00),
        ("orange", "semi-dark-") => (0xFF, 0x78, 0x0A),
        ("orange", "light-") => (0xFF, 0xB3, 0x57),
        ("orange", _) => (0xFF, 0x98, 0x30),
        ("yellow", "dark-") => (0xE0, 0xB4, 0x00),
        ("yellow", "semi-dark-") => (0xF2, 0xCC, 0x0C),
        ("yellow", "light-") => (0xFF, 0xEE, 0x52),
        ("yellow", _) => (0xFA, 0xDE, 0x2A),
        ("green", "dark-") => (0x37, 0x87, 0x2D),
        ("green", "semi-dark-") => (0x56, 0xA6, 0x4B),
        ("green", "light-") => (0x96, 0xD9, 0x8D),
        ("green", _) => (0x73, 0xBF, 0x69),
        ("blue", "dark-") => (0x1F, 0x60, 0xC4),
        ("blue", "semi-dark-") => (0x32, 0x74, 0xD9),
        ("blue", "light-") => (0x8A, 0xB8, 0xFF),
        ("blue", _) => (0x57, 0x94, 0xF2),
        ("purple", "dark-") => (0x8F, 0x3B, 0xB8),
        ("purple", "semi-dark-") => (0xA3, 0x52, 0xCC),
        ("purple", "light-") => (0xCA, 0x95, 0xE5),
        ("purple", _) => (0xB8, 0x77, 0xD9),
        ("text", _) | ("white", _) => (0xD8, 0xD9, 0xDA),
        ("transparent", _) => (0x80, 0x80, 0x80),
        ("gray" | "grey", _) => (0x80, 0x80, 0x80),
        ("cyan", _) => (0x6E, 0xD0, 0xE0),
        _ => return None,
    };
    Some(rgb)
}

pub fn rgb(c: (u8, u8, u8)) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// Colour for a value given Grafana threshold steps (`[(None, "green"), (Some(80.0), "red")]`).
pub fn threshold_color(v: f64, steps: &[(Option<f64>, String)]) -> Option<(u8, u8, u8)> {
    let mut color = None;
    for (limit, c) in steps {
        match limit {
            None => color = named_rgb(c),
            Some(l) if v >= *l => color = named_rgb(c),
            _ => {}
        }
    }
    color
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_units() {
        assert_eq!(format(42.0, Some("percent"), None), "42%");
        assert_eq!(format(0.425, Some("percentunit"), None), "42.5%");
        assert_eq!(format(1536.0, Some("bytes"), None), "1.5 KiB");
        assert_eq!(format(1_500_000.0, Some("decbytes"), None), "1.5 MB");
        assert_eq!(format(0.25, Some("s"), None), "250 ms");
        assert_eq!(format(1200.0, Some("reqps"), None), "1.2 K req/s");
        assert_eq!(format(3.0, Some("widgets"), None), "3 widgets");
        assert_eq!(format(1234.0, None, None), "1.23 K");
        assert_eq!(format(5.0, None, Some(2)), "5");
        assert_eq!(format(f64::INFINITY, None, None), "+Inf");
    }

    #[test]
    fn colours() {
        assert_eq!(named_rgb("green"), Some((0x73, 0xBF, 0x69)));
        assert_eq!(named_rgb("#ff0000"), Some((255, 0, 0)));
        assert_eq!(named_rgb("rgba(0, 211, 255, 1)"), Some((0, 211, 255)));
        assert_eq!(named_rgb("semi-dark-red"), Some((0xE0, 0x2F, 0x44)));
        assert_eq!(named_rgb("nope"), None);
        let steps = vec![(None, "green".to_string()), (Some(80.0), "red".to_string())];
        assert_eq!(threshold_color(50.0, &steps), named_rgb("green"));
        assert_eq!(threshold_color(90.0, &steps), named_rgb("red"));
    }
}
