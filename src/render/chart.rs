//! Backend-independent chart description (`ChartSpec`), axis layout, and the
//! Unicode renderer. The pixel renderer lives in `raster.rs`.

use std::hash::{Hash, Hasher};

use chrono::{Local, Offset, TimeZone};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::widgets::Widget;
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};

use crate::grafana::dashboard::DrawStyle;
use crate::timerange::{Millis, format_tick};
use crate::units;

pub type Rgb = (u8, u8, u8);

#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries {
    pub name: String,
    pub color: Rgb,
    pub points: Vec<(Millis, Option<f64>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartAnnotation {
    pub start: Millis,
    pub end: Millis,
    pub color: Rgb,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThresholdStyle {
    #[default]
    Off,
    Line,
    Dashed,
    Area,
}

impl ThresholdStyle {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("line") => Self::Line,
            Some("dashed") | Some("line+area") | Some("dashed+area") => Self::Dashed,
            Some("area") => Self::Area,
            _ => Self::Off,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartSpec {
    pub series: Vec<ChartSeries>,
    pub range: (Millis, Millis),
    pub y_min: Option<f64>,
    pub y_max: Option<f64>,
    pub unit: Option<String>,
    pub decimals: Option<u32>,
    pub thresholds: Vec<(f64, Rgb)>,
    pub threshold_style: ThresholdStyle,
    pub annotations: Vec<ChartAnnotation>,
    pub draw_style: DrawStyle,
    pub fill_opacity: f64,
    pub line_width: f64,
    pub stacking: bool,
}

impl Default for ChartSpec {
    fn default() -> Self {
        Self {
            series: Vec::new(),
            range: (0, 1),
            y_min: None,
            y_max: None,
            unit: None,
            decimals: None,
            thresholds: Vec::new(),
            threshold_style: ThresholdStyle::Off,
            annotations: Vec::new(),
            draw_style: DrawStyle::Line,
            fill_opacity: 0.0,
            line_width: 1.0,
            stacking: false,
        }
    }
}

/// Interaction state drawn on top of a chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Overlay {
    pub crosshair: Option<Millis>,
    pub selection: Option<(Millis, Millis)>,
    pub focused: bool,
}

impl ChartSpec {
    /// Series after stacking (cumulative sums at shared timestamps).
    pub fn plotted_series(&self) -> Vec<ChartSeries> {
        if !self.stacking {
            return self.series.clone();
        }
        let mut acc: std::collections::HashMap<Millis, f64> = std::collections::HashMap::new();
        self.series
            .iter()
            .map(|s| ChartSeries {
                name: s.name.clone(),
                color: s.color,
                points: s
                    .points
                    .iter()
                    .map(|(t, v)| {
                        let v = v.map(|v| {
                            let e = acc.entry(*t).or_insert(0.0);
                            *e += v;
                            *e
                        });
                        (*t, v)
                    })
                    .collect(),
            })
            .collect()
    }

    /// Y bounds: explicit min/max win; otherwise data extent with a little headroom.
    pub fn y_bounds(&self) -> (f64, f64) {
        let series = self.plotted_series();
        let vals = series.iter().flat_map(|s| s.points.iter().filter_map(|(_, v)| *v));
        let (mut lo, mut hi) = vals.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)));
        if !lo.is_finite() || !hi.is_finite() {
            lo = 0.0;
            hi = 1.0;
        }
        if self.draw_style == DrawStyle::Bars || self.fill_opacity > 0.0 || self.stacking {
            lo = lo.min(0.0);
        }
        if self.threshold_style != ThresholdStyle::Off {
            for (t, _) in &self.thresholds {
                if self.y_max.is_none() && *t > hi && *t < hi * 2.0 {
                    hi = *t;
                }
            }
        }
        let pad = if (hi - lo).abs() < f64::EPSILON { if hi == 0.0 { 1.0 } else { hi.abs() * 0.1 } } else { (hi - lo) * 0.05 };
        // Pad both ends, but don't push a non-negative series below zero.
        let lo = self.y_min.unwrap_or(if lo >= 0.0 { (lo - pad).max(0.0) } else { lo - pad });
        let hi = self.y_max.unwrap_or(hi + pad);
        if hi > lo { (lo, hi) } else { (lo, lo + 1.0) }
    }

    pub fn base_hash(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.range.hash(&mut h);
        for s in &self.series {
            s.name.hash(&mut h);
            s.color.hash(&mut h);
            s.points.len().hash(&mut h);
            for (t, v) in &s.points {
                t.hash(&mut h);
                v.map(f64::to_bits).hash(&mut h);
            }
        }
        format!(
            "{:?}{:?}{:?}{:?}{:?}{}{}{}",
            self.y_min,
            self.y_max,
            self.thresholds,
            self.threshold_style,
            self.annotations,
            self.fill_opacity,
            self.line_width,
            self.stacking
        )
        .hash(&mut h);
        h.finish()
    }

    /// The point time closest to `t` across all series (for snapping the crosshair).
    pub fn snap(&self, t: Millis) -> Option<Millis> {
        self.series.iter().flat_map(|s| s.points.iter().map(|(pt, _)| *pt)).min_by_key(|pt| (pt - t).abs())
    }

    /// Sorted, de-duplicated timestamps of all points.
    pub fn timestamps(&self) -> Vec<Millis> {
        let mut ts: Vec<Millis> = self.series.iter().flat_map(|s| s.points.iter().map(|(t, _)| *t)).collect();
        ts.sort_unstable();
        ts.dedup();
        ts
    }
}

/// Nice tick values for the y axis.
pub fn y_ticks(lo: f64, hi: f64, max_ticks: usize) -> Vec<f64> {
    let span = hi - lo;
    if span <= 0.0 || max_ticks == 0 {
        return vec![lo];
    }
    let raw = span / max_ticks.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag);
    let mut t = (lo / step).ceil() * step;
    let mut out = Vec::new();
    while t <= hi + step * 1e-9 {
        out.push(if t.abs() < step * 1e-9 { 0.0 } else { t });
        t += step;
    }
    out
}

/// Nice tick timestamps for the time axis, aligned to local time.
pub fn x_ticks(from: Millis, to: Millis, max_ticks: usize) -> Vec<Millis> {
    const STEPS: [Millis; 22] = [
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
        1_800_000,
        3_600_000,
        7_200_000,
        10_800_000,
        21_600_000,
        43_200_000,
        86_400_000,
        172_800_000,
        604_800_000,
        2_592_000_000,
        31_536_000_000,
    ];
    let span = to.saturating_sub(from);
    if span <= 0 || max_ticks == 0 {
        return Vec::new();
    }
    let raw = span / max_ticks as Millis;
    let step = STEPS.iter().copied().find(|s| *s >= raw).unwrap_or(raw.max(1));
    let offset = Local.timestamp_millis_opt(from).single().map(|d| d.offset().fix().local_minus_utc() as Millis * 1000).unwrap_or(0);
    let mut t = ((from + offset) / step + 1) * step - offset;
    let mut out = Vec::new();
    while t < to {
        out.push(t);
        t += step;
    }
    out
}

/// Computed geometry: where the plot goes and what the axes show.
#[derive(Debug, Clone)]
pub struct ChartLayout {
    pub plot: Rect,
    pub y_ticks: Vec<(f64, String)>,
    pub x_ticks: Vec<(Millis, String)>,
    pub y_bounds: (f64, f64),
}

impl ChartLayout {
    pub fn compute(spec: &ChartSpec, area: Rect) -> Self {
        let y_bounds = spec.y_bounds();
        let show_x_axis = area.height >= 4;
        let plot_h = area.height.saturating_sub(u16::from(show_x_axis)).max(1);
        let ticks = y_ticks(y_bounds.0, y_bounds.1, (plot_h as usize / 2).clamp(1, 6));
        let y_labels: Vec<(f64, String)> = ticks.iter().map(|v| (*v, units::format(*v, spec.unit.as_deref(), spec.decimals))).collect();
        let label_w = y_labels.iter().map(|(_, s)| s.chars().count()).max().unwrap_or(1).min(12) as u16;
        let label_w = if area.width < 20 { 0 } else { label_w + 1 };
        let plot = Rect { x: area.x + label_w, y: area.y, width: area.width.saturating_sub(label_w).max(1), height: plot_h };
        let span = spec.range.1.saturating_sub(spec.range.0).max(1);
        let sample = format_tick(spec.range.0, span).chars().count().max(5) as u16;
        let xt = if show_x_axis { x_ticks(spec.range.0, spec.range.1, (plot.width / (sample + 3)).max(1) as usize) } else { Vec::new() };
        let x_labels = xt.into_iter().map(|t| (t, format_tick(t, span))).collect();
        Self { plot, y_ticks: y_labels, x_ticks: x_labels, y_bounds }
    }

    pub fn time_to_col(&self, spec: &ChartSpec, t: Millis) -> Option<u16> {
        let (a, b) = spec.range;
        if t < a || t > b || b <= a {
            return None;
        }
        let frac = t.saturating_sub(a) as f64 / b.saturating_sub(a) as f64;
        Some(self.plot.x + ((frac * (self.plot.width.saturating_sub(1)) as f64).round() as u16))
    }

    pub fn col_to_time(&self, spec: &ChartSpec, col: u16) -> Millis {
        let (a, b) = spec.range;
        let rel = col.saturating_sub(self.plot.x).min(self.plot.width.saturating_sub(1)) as f64;
        let frac = if self.plot.width > 1 { rel / (self.plot.width - 1) as f64 } else { 0.0 };
        a.saturating_add((b.saturating_sub(a) as f64 * frac) as Millis)
    }

    /// Draws tick labels around the plot area (shared by both renderers).
    pub fn render_axes(&self, spec: &ChartSpec, area: Rect, buf: &mut Buffer) {
        let dim = Style::default().fg(Color::DarkGray);
        let (lo, hi) = self.y_bounds;
        let label_w = self.plot.x - area.x;
        if label_w > 0 {
            let mut last_row = None;
            for (v, label) in self.y_ticks.iter().rev() {
                let frac = (v - lo) / (hi - lo);
                let row = self.plot.y + ((1.0 - frac) * (self.plot.height.saturating_sub(1)) as f64).round() as u16;
                if last_row == Some(row) {
                    continue;
                }
                last_row = Some(row);
                let text: String = label.chars().take(label_w as usize - 1).collect();
                let x = area.x + label_w - 1 - text.chars().count() as u16;
                buf.set_string(x, row, &text, dim);
            }
        }
        if area.height > self.plot.height {
            let y = self.plot.y + self.plot.height;
            let mut next_free = self.plot.x;
            for (t, label) in &self.x_ticks {
                let Some(col) = self.time_to_col(spec, *t) else { continue };
                let w = label.chars().count() as u16;
                let x = col.saturating_sub(w / 2).max(self.plot.x);
                if x < next_free || x + w > self.plot.x + self.plot.width {
                    continue;
                }
                buf.set_string(x, y, label, dim);
                next_free = x + w + 2;
            }
        }
    }
}

/// Draws the plot with sub-cell Unicode markers (braille/octant/half-block).
pub fn render_unicode(spec: &ChartSpec, layout: &ChartLayout, overlay: &Overlay, marker: Marker, buf: &mut Buffer) {
    let plot = layout.plot;
    let (lo, hi) = layout.y_bounds;
    let (from, to) = spec.range;
    let series = spec.plotted_series();
    let grid_color = Color::Rgb(0x2a, 0x2d, 0x33);

    // Grid rows behind the data.
    for (v, _) in &layout.y_ticks {
        let frac = (v - lo) / (hi - lo);
        let row = plot.y + ((1.0 - frac) * (plot.height.saturating_sub(1)) as f64).round() as u16;
        for x in plot.x..plot.x + plot.width {
            if let Some(cell) = buf.cell_mut((x, row))
                && cell.symbol() == " "
            {
                cell.set_symbol("┈").set_fg(grid_color);
            }
        }
    }
    // Threshold areas tint the background.
    if spec.threshold_style == ThresholdStyle::Area {
        for row in 0..plot.height {
            let v = hi - (hi - lo) * (row as f64 + 0.5) / plot.height as f64;
            let color = spec.thresholds.iter().rev().find(|(t, _)| v >= *t).map(|(_, c)| *c);
            if let Some(c) = color {
                let tint = Color::Rgb(c.0 / 6, c.1 / 6, c.2 / 6);
                for x in plot.x..plot.x + plot.width {
                    if let Some(cell) = buf.cell_mut((x, plot.y + row)) {
                        cell.set_bg(tint);
                    }
                }
            }
        }
    }

    let canvas = Canvas::default().marker(marker).x_bounds([from as f64, to as f64]).y_bounds([lo, hi]).paint(|ctx| {
        // Annotations: vertical markers at the start (and end for regions).
        for a in &spec.annotations {
            let c = Color::Rgb(a.color.0, a.color.1, a.color.2);
            for t in [a.start, a.end] {
                if t >= from && t <= to {
                    ctx.draw(&CanvasLine { x1: t as f64, y1: lo, x2: t as f64, y2: hi, color: c });
                }
                if a.end == a.start {
                    break;
                }
            }
        }
        if matches!(spec.threshold_style, ThresholdStyle::Line | ThresholdStyle::Dashed) {
            for (t, c) in &spec.thresholds {
                if *t < lo || *t > hi {
                    continue;
                }
                let color = Color::Rgb(c.0, c.1, c.2);
                let dashes = if spec.threshold_style == ThresholdStyle::Dashed { 60 } else { 1 };
                let w = to.saturating_sub(from) as f64 / dashes as f64;
                for i in (0..dashes).step_by(if dashes > 1 { 2 } else { 1 }) {
                    let x1 = from as f64 + w * i as f64;
                    ctx.draw(&CanvasLine { x1, y1: *t, x2: x1 + w, y2: *t, color });
                }
            }
        }
        ctx.layer();
        for s in &series {
            let color = Color::Rgb(s.color.0, s.color.1, s.color.2);
            match spec.draw_style {
                DrawStyle::Bars => {
                    let base = 0f64.clamp(lo, hi);
                    for (t, v) in &s.points {
                        if let Some(v) = v {
                            ctx.draw(&CanvasLine { x1: *t as f64, y1: base, x2: *t as f64, y2: v.clamp(lo, hi), color });
                        }
                    }
                }
                DrawStyle::Points => {
                    let pts: Vec<(f64, f64)> = s.points.iter().filter_map(|(t, v)| v.map(|v| (*t as f64, v))).collect();
                    ctx.draw(&ratatui::widgets::canvas::Points { coords: &pts, color });
                }
                DrawStyle::Line => {
                    let mut prev: Option<(f64, f64)> = None;
                    for (t, v) in &s.points {
                        match v {
                            Some(v) => {
                                let p = (*t as f64, v.clamp(lo, hi));
                                if let Some(q) = prev {
                                    ctx.draw(&CanvasLine { x1: q.0, y1: q.1, x2: p.0, y2: p.1, color });
                                } else if s.points.len() == 1 {
                                    ctx.draw(&ratatui::widgets::canvas::Points { coords: &[p], color });
                                }
                                prev = Some(p);
                            }
                            None => prev = None,
                        }
                    }
                }
            }
        }
    });
    canvas.render(plot, buf);
    render_overlay_cells(spec, layout, overlay, buf);
}

/// Crosshair and selection as cell styling (Unicode tier, and on top of images where possible).
pub fn render_overlay_cells(spec: &ChartSpec, layout: &ChartLayout, overlay: &Overlay, buf: &mut Buffer) {
    let plot = layout.plot;
    if let Some((a, b)) = overlay.selection {
        let ca = layout.time_to_col(spec, a.max(spec.range.0)).unwrap_or(plot.x);
        let cb = layout.time_to_col(spec, b.min(spec.range.1)).unwrap_or(plot.x + plot.width - 1);
        for x in ca.min(cb)..=ca.max(cb) {
            for y in plot.y..plot.y + plot.height {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(Color::Rgb(0x1f, 0x3a, 0x5f));
                }
            }
        }
    }
    if let Some(t) = overlay.crosshair
        && let Some(col) = layout.time_to_col(spec, t)
    {
        for y in plot.y..plot.y + plot.height {
            if let Some(cell) = buf.cell_mut((col, y)) {
                if cell.symbol() == " " || cell.symbol() == "┈" {
                    cell.set_symbol("│");
                    cell.set_fg(if overlay.focused { Color::Gray } else { Color::DarkGray });
                } else {
                    cell.set_style(Style::default().add_modifier(Modifier::BOLD).bg(Color::Rgb(0x3a, 0x3d, 0x45)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ChartSpec {
        ChartSpec {
            series: vec![
                ChartSeries {
                    name: "a".into(),
                    color: (255, 0, 0),
                    points: (0..60).map(|i| (i * 60_000, Some((i as f64 / 5.0).sin() * 50.0 + 50.0))).collect(),
                },
                ChartSeries {
                    name: "b".into(),
                    color: (0, 255, 0),
                    points: (0..60).map(|i| (i * 60_000, if i == 30 { None } else { Some(10.0) })).collect(),
                },
            ],
            range: (0, 59 * 60_000),
            ..Default::default()
        }
    }

    #[test]
    fn ticks() {
        assert_eq!(y_ticks(0.0, 100.0, 5), vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
        assert_eq!(y_ticks(0.0, 1.0, 4), vec![0.0, 0.25, 0.5, 0.75, 1.0]);
        let xt = x_ticks(0, 3_600_000, 6);
        assert!(xt.len() >= 4 && xt.len() <= 6, "{xt:?}");
        assert!(xt.windows(2).all(|w| w[1] - w[0] == xt[1] - xt[0]));
    }

    #[test]
    fn bounds_and_stacking() {
        let mut s = spec();
        let (lo, hi) = s.y_bounds();
        assert!(lo <= 0.0 + 1.0 && hi >= 100.0 * 0.99);
        s.y_min = Some(0.0);
        s.y_max = Some(100.0);
        assert_eq!(s.y_bounds(), (0.0, 100.0));
        s.stacking = true;
        let st = s.plotted_series();
        assert_eq!(st[1].points[0].1, Some(st[0].points[0].1.unwrap() + 10.0));
        assert_eq!(st[1].points[30].1, None);
    }

    #[test]
    fn column_time_mapping_roundtrips() {
        let s = spec();
        let l = ChartLayout::compute(&s, Rect::new(0, 0, 80, 20));
        let col = l.time_to_col(&s, 30 * 60_000).unwrap();
        let t = l.col_to_time(&s, col);
        assert!((t - 30 * 60_000).abs() <= (59 * 60_000) / l.plot.width as i64 + 1);
        assert_eq!(s.snap(30 * 60_000 + 10_000), Some(30 * 60_000));
    }

    #[test]
    fn unicode_render_smoke() {
        let s = spec();
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        let l = ChartLayout::compute(&s, area);
        let ov = Overlay { crosshair: Some(30 * 60_000), selection: Some((10 * 60_000, 20 * 60_000)), focused: true };
        render_unicode(&s, &l, &ov, Marker::Braille, &mut buf);
        l.render_axes(&s, area, &mut buf);
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)), "braille drawn");
        assert!(text.contains('│') || buf.content().iter().any(|c| c.modifier.contains(Modifier::BOLD)), "crosshair drawn");
        assert!(text.contains("100") || text.contains("80"), "y labels: {text}");
    }
}
