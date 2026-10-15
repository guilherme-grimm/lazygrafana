//! Pixel renderer: rasterises a `ChartSpec` (and heatmaps) with tiny-skia.
//! Axis labels stay in terminal text; only the plot area is an image.
//! The background is transparent so charts sit on any terminal theme.

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, StrokeDash, Transform};

use super::chart::{ChartLayout, ChartSpec, Overlay, Rgb, ThresholdStyle};
use crate::grafana::dashboard::DrawStyle;
use crate::timerange::Millis;

fn paint(c: Rgb, alpha: f32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(Color::from_rgba8(c.0, c.1, c.2, (alpha.clamp(0.0, 1.0) * 255.0) as u8));
    p.anti_alias = true;
    p
}

pub struct Geometry {
    pub w: f32,
    pub h: f32,
    pub from: Millis,
    pub to: Millis,
    pub lo: f64,
    pub hi: f64,
}

impl Geometry {
    pub fn x(&self, t: Millis) -> f32 {
        (t.saturating_sub(self.from) as f64 / self.to.saturating_sub(self.from).max(1) as f64 * (self.w - 1.0) as f64) as f32
    }

    pub fn y(&self, v: f64) -> f32 {
        let frac = (v - self.lo) / (self.hi - self.lo);
        ((1.0 - frac.clamp(-0.05, 1.05)) * (self.h - 1.0) as f64) as f32
    }
}

/// Renders the static base layer: grid, thresholds, annotations, series.
pub fn render_base(spec: &ChartSpec, layout: &ChartLayout, w: u32, h: u32, cell_h: u16) -> Option<Pixmap> {
    let mut pm = Pixmap::new(w.max(1), h.max(1))?;
    let (lo, hi) = layout.y_bounds;
    let g = Geometry { w: w as f32, h: h as f32, from: spec.range.0, to: spec.range.1, lo, hi };
    let scale = (cell_h as f32 / 16.0).max(1.0);
    let id = Transform::identity();

    // Threshold areas.
    if spec.threshold_style == ThresholdStyle::Area {
        let mut bounds: Vec<(f64, Rgb)> = spec.thresholds.clone();
        bounds.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (i, (t, c)) in bounds.iter().enumerate() {
            let top = bounds.get(i + 1).map(|n| n.0).unwrap_or(hi).min(hi);
            let bottom = t.max(lo);
            if top <= bottom {
                continue;
            }
            if let Some(r) = Rect::from_ltrb(0.0, g.y(top), g.w, g.y(bottom)) {
                pm.fill_rect(r, &paint(*c, 0.12), id, None);
            }
        }
    }

    // Horizontal grid lines at the y ticks.
    let grid = paint((0x80, 0x80, 0x80), 0.18);
    let grid_stroke = Stroke { width: 1.0, ..Default::default() };
    for (v, _) in &layout.y_ticks {
        let y = g.y(*v).round() + 0.5;
        let mut pb = PathBuilder::new();
        pb.move_to(0.0, y);
        pb.line_to(g.w, y);
        if let Some(p) = pb.finish() {
            pm.stroke_path(&p, &grid, &grid_stroke, id, None);
        }
    }
    for (t, _) in &layout.x_ticks {
        let x = g.x(*t).round() + 0.5;
        let mut pb = PathBuilder::new();
        pb.move_to(x, 0.0);
        pb.line_to(x, g.h);
        if let Some(p) = pb.finish() {
            pm.stroke_path(&p, &paint((0x80, 0x80, 0x80), 0.10), &grid_stroke, id, None);
        }
    }

    // Annotations: region fill + dashed edge(s).
    for a in &spec.annotations {
        let (x1, x2) = (g.x(a.start.max(g.from)), g.x(a.end.min(g.to)));
        if a.end > a.start
            && let Some(r) = Rect::from_ltrb(x1, 0.0, x2.max(x1 + 1.0), g.h)
        {
            pm.fill_rect(r, &paint(a.color, 0.10), id, None);
        }
        let dash = StrokeDash::new(vec![4.0 * scale, 3.0 * scale], 0.0);
        let stroke = Stroke { width: 1.0 * scale, dash, ..Default::default() };
        for x in [x1, x2] {
            if !(0.0..=g.w).contains(&x) {
                continue;
            }
            let mut pb = PathBuilder::new();
            pb.move_to(x, 0.0);
            pb.line_to(x, g.h);
            if let Some(p) = pb.finish() {
                pm.stroke_path(&p, &paint(a.color, 0.8), &stroke, id, None);
            }
            if a.end == a.start {
                break;
            }
        }
    }

    // Series.
    let series = spec.plotted_series();
    let line_w = (spec.line_width as f32 * scale * 1.2).max(1.0);
    // Bar width from the smallest spacing between points (not their count).
    let min_dt = series
        .iter()
        .flat_map(|s| s.points.windows(2).map(|w| w[1].0 - w[0].0))
        .filter(|d| *d > 0)
        .min()
        .unwrap_or_else(|| (g.to - g.from).max(1) / 20);
    for (idx, s) in series.iter().enumerate() {
        match spec.draw_style {
            DrawStyle::Bars => {
                let slot = (min_dt as f64 / (g.to - g.from).max(1) as f64 * g.w as f64) as f32;
                let bar_w = (slot * 0.8).max(1.0);
                let base_y = g.y(0f64.clamp(lo, hi));
                // With stacking, draw each series' own segment from the previous stack top.
                let prev = if spec.stacking && idx > 0 { Some(&series[idx - 1]) } else { None };
                for (i, (t, v)) in s.points.iter().enumerate() {
                    let Some(v) = v else { continue };
                    let bottom = prev.and_then(|p| p.points.get(i).and_then(|(_, pv)| *pv)).map(|pv| g.y(pv)).unwrap_or(base_y);
                    let x = g.x(*t) - bar_w / 2.0;
                    let y = g.y(*v);
                    if let Some(r) = Rect::from_ltrb(x, y.min(bottom), x + bar_w, y.max(bottom).max(y.min(bottom) + 1.0)) {
                        pm.fill_rect(r, &paint(s.color, (spec.fill_opacity.max(60.0) / 100.0) as f32), id, None);
                    }
                }
            }
            DrawStyle::Points => {
                for (t, v) in &s.points {
                    if let Some(v) = v
                        && let Some(c) = PathBuilder::from_circle(g.x(*t), g.y(*v), line_w * 1.5)
                    {
                        pm.fill_path(&c, &paint(s.color, 1.0), FillRule::Winding, id, None);
                    }
                }
            }
            DrawStyle::Line => {
                // Split into contiguous segments at gaps.
                let mut segments: Vec<Vec<(f32, f32)>> = vec![Vec::new()];
                for (t, v) in &s.points {
                    match v {
                        Some(v) => segments.last_mut().expect("non-empty").push((g.x(*t), g.y(*v))),
                        None => {
                            if !segments.last().expect("non-empty").is_empty() {
                                segments.push(Vec::new());
                            }
                        }
                    }
                }
                for seg in segments.iter().filter(|s| !s.is_empty()) {
                    if spec.fill_opacity > 0.0 && seg.len() > 1 {
                        let prev_series = if spec.stacking && idx > 0 { Some(&series[idx - 1]) } else { None };
                        let mut pb = PathBuilder::new();
                        pb.move_to(seg[0].0, seg[0].1);
                        for p in &seg[1..] {
                            pb.line_to(p.0, p.1);
                        }
                        match prev_series {
                            Some(prev) => {
                                // Close along the previous stack's top, walking backwards.
                                let back: Vec<(f32, f32)> = prev
                                    .points
                                    .iter()
                                    .filter_map(|(t, v)| v.map(|v| (g.x(*t), g.y(v))))
                                    .filter(|(x, _)| *x >= seg[0].0 - 0.5 && *x <= seg[seg.len() - 1].0 + 0.5)
                                    .collect();
                                for p in back.iter().rev() {
                                    pb.line_to(p.0, p.1);
                                }
                                if back.is_empty() {
                                    let base_y = g.y(0f64.clamp(lo, hi));
                                    pb.line_to(seg[seg.len() - 1].0, base_y);
                                    pb.line_to(seg[0].0, base_y);
                                }
                            }
                            None => {
                                let base_y = g.y(0f64.clamp(lo, hi));
                                pb.line_to(seg[seg.len() - 1].0, base_y);
                                pb.line_to(seg[0].0, base_y);
                            }
                        }
                        pb.close();
                        if let Some(p) = pb.finish() {
                            pm.fill_path(&p, &paint(s.color, (spec.fill_opacity / 100.0) as f32 * 0.8), FillRule::Winding, id, None);
                        }
                    }
                    if seg.len() == 1 {
                        if let Some(c) = PathBuilder::from_circle(seg[0].0, seg[0].1, line_w) {
                            pm.fill_path(&c, &paint(s.color, 1.0), FillRule::Winding, id, None);
                        }
                        continue;
                    }
                    let mut pb = PathBuilder::new();
                    pb.move_to(seg[0].0, seg[0].1);
                    for p in &seg[1..] {
                        pb.line_to(p.0, p.1);
                    }
                    if let Some(p) = pb.finish() {
                        let stroke = Stroke { width: line_w, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
                        pm.stroke_path(&p, &paint(s.color, 1.0), &stroke, id, None);
                    }
                }
            }
        }
    }

    // Threshold lines on top of the data.
    if matches!(spec.threshold_style, ThresholdStyle::Line | ThresholdStyle::Dashed) {
        for (t, c) in &spec.thresholds {
            if *t < lo || *t > hi {
                continue;
            }
            let y = g.y(*t);
            let dash =
                if spec.threshold_style == ThresholdStyle::Dashed { StrokeDash::new(vec![6.0 * scale, 4.0 * scale], 0.0) } else { None };
            let stroke = Stroke { width: scale, dash, ..Default::default() };
            let mut pb = PathBuilder::new();
            pb.move_to(0.0, y);
            pb.line_to(g.w, y);
            if let Some(p) = pb.finish() {
                pm.stroke_path(&p, &paint(*c, 0.9), &stroke, id, None);
            }
        }
    }
    Some(pm)
}

/// Composites the interaction overlay (selection band, crosshair, value dots) onto a copy of the base.
pub fn composite_overlay(base: &Pixmap, spec: &ChartSpec, layout: &ChartLayout, overlay: &Overlay, cell_h: u16) -> Pixmap {
    let mut pm = base.clone();
    let (lo, hi) = layout.y_bounds;
    let g = Geometry { w: pm.width() as f32, h: pm.height() as f32, from: spec.range.0, to: spec.range.1, lo, hi };
    let id = Transform::identity();
    let scale = (cell_h as f32 / 16.0).max(1.0);
    if let Some((a, b)) = overlay.selection {
        let (x1, x2) = (g.x(a.max(g.from)), g.x(b.min(g.to)));
        if let Some(r) = Rect::from_ltrb(x1.min(x2), 0.0, x1.max(x2).max(x1.min(x2) + 1.0), g.h) {
            pm.fill_rect(r, &paint((0x57, 0x94, 0xF2), 0.22), id, None);
        }
        for x in [x1, x2] {
            if let Some(r) = Rect::from_xywh(x - scale / 2.0, 0.0, scale, g.h) {
                pm.fill_rect(r, &paint((0x57, 0x94, 0xF2), 0.8), id, None);
            }
        }
    }
    if let Some(t) = overlay.crosshair
        && t >= g.from
        && t <= g.to
    {
        let x = g.x(t);
        let dash = if overlay.focused { None } else { StrokeDash::new(vec![3.0 * scale, 3.0 * scale], 0.0) };
        let stroke = Stroke { width: scale, dash, ..Default::default() };
        let mut pb = PathBuilder::new();
        pb.move_to(x, 0.0);
        pb.line_to(x, g.h);
        if let Some(p) = pb.finish() {
            pm.stroke_path(&p, &paint((0xD8, 0xD9, 0xDA), if overlay.focused { 0.8 } else { 0.5 }), &stroke, id, None);
        }
        for s in spec.plotted_series() {
            let nearest = s.points.iter().min_by_key(|(pt, _)| (pt - t).abs());
            if let Some((pt, Some(v))) = nearest
                && let Some(c) = PathBuilder::from_circle(g.x(*pt), g.y(*v), 3.0 * scale)
            {
                pm.fill_path(&c, &paint(s.color, 1.0), FillRule::Winding, id, None);
                pm.stroke_path(&c, &paint((0x11, 0x12, 0x17), 1.0), &Stroke { width: scale, ..Default::default() }, id, None);
            }
        }
    }
    pm
}

/// A heatmap: buckets (rows) × time (columns) → colour intensity.
#[derive(Debug, Clone, PartialEq)]
pub struct HeatmapSpec {
    pub times: Vec<Millis>,
    /// Bucket labels from low to high.
    pub buckets: Vec<String>,
    /// `cells[bucket][time]`.
    pub cells: Vec<Vec<f64>>,
    pub range: (Millis, Millis),
}

impl HeatmapSpec {
    pub fn max(&self) -> f64 {
        self.cells.iter().flatten().copied().filter(|v| v.is_finite()).fold(0.0, f64::max)
    }
}

/// Grafana's "Oranges" scheme, approximated.
pub fn heat_color(frac: f64) -> Rgb {
    let f = frac.clamp(0.0, 1.0);
    let lerp = |a: f64, b: f64| (a + (b - a) * f) as u8;
    if f <= 0.0 { (0x16, 0x17, 0x1d) } else { (lerp(60.0, 255.0), lerp(30.0, 152.0), lerp(10.0, 48.0)) }
}

pub fn render_heatmap(spec: &HeatmapSpec, w: u32, h: u32) -> Option<Pixmap> {
    let mut pm = Pixmap::new(w.max(1), h.max(1))?;
    let max = spec.max();
    let rows = spec.buckets.len().max(1);
    let id = Transform::identity();
    let (from, to) = spec.range;
    let col_w = if spec.times.len() > 1 {
        spec.times[1].saturating_sub(spec.times[0]) as f64 / to.saturating_sub(from).max(1) as f64 * w as f64
    } else {
        w as f64
    };
    for (bi, row) in spec.cells.iter().enumerate() {
        let y1 = h as f32 - (bi + 1) as f32 * h as f32 / rows as f32;
        let row_h = h as f32 / rows as f32;
        for (ti, v) in row.iter().enumerate() {
            if !v.is_finite() || *v <= 0.0 || max <= 0.0 {
                continue;
            }
            let t = spec.times[ti];
            let x = (t.saturating_sub(from) as f64 / to.saturating_sub(from).max(1) as f64 * w as f64 - col_w) as f32;
            if let Some(r) = Rect::from_xywh(x + 0.5, y1 + 0.5, (col_w as f32 - 1.0).max(1.0), (row_h - 1.0).max(1.0)) {
                pm.fill_rect(r, &paint(heat_color(v / max), 1.0), id, None);
            }
        }
    }
    Some(pm)
}

pub fn to_rgba_image(pm: Pixmap) -> image::RgbaImage {
    let (w, h) = (pm.width(), pm.height());
    image::RgbaImage::from_raw(w, h, pm.take_demultiplied()).expect("buffer size matches")
}

#[cfg(test)]
mod tests {
    use super::super::chart::{ChartSeries, ChartSpec};
    use super::*;
    use ratatui::layout::Rect as TRect;

    #[test]
    fn renders_pixels() {
        let spec = ChartSpec {
            series: vec![ChartSeries {
                name: "a".into(),
                color: (255, 0, 0),
                points: (0..10).map(|i| (i * 1000, Some(i as f64))).collect(),
            }],
            range: (0, 9000),
            fill_opacity: 20.0,
            ..Default::default()
        };
        let layout = ChartLayout::compute(&spec, TRect::new(0, 0, 40, 10));
        let pm = render_base(&spec, &layout, 400, 200, 20).unwrap();
        let red = pm.pixels().iter().filter(|p| p.red() > 200 && p.alpha() > 200).count();
        assert!(red > 100, "line drawn ({red} red pixels)");
        let ov =
            composite_overlay(&pm, &spec, &layout, &Overlay { crosshair: Some(4500), selection: Some((1000, 3000)), focused: true }, 20);
        assert_ne!(ov.data(), pm.data());
        let img = to_rgba_image(ov);
        assert_eq!(img.dimensions(), (400, 200));
    }

    #[test]
    fn heatmap_pixels() {
        let spec = HeatmapSpec {
            times: vec![1000, 2000, 3000],
            buckets: vec!["0.1".into(), "1".into()],
            cells: vec![vec![1.0, 2.0, 3.0], vec![0.0, 5.0, 1.0]],
            range: (0, 3000),
        };
        let pm = render_heatmap(&spec, 90, 40).unwrap();
        assert!(pm.pixels().iter().any(|p| p.alpha() > 0));
    }
}

#[cfg(test)]
mod snapshot {
    use super::super::chart::{ChartAnnotation, ChartLayout, Overlay};
    use super::super::panels::{PanelData, chart_spec};
    use super::*;
    use crate::grafana::dashboard::Dashboard;
    use crate::grafana::dataframe::{Frame, series_from_frames};

    /// Writes a PNG of the demo timeseries panel to $LG_PNG_OUT for eyeballing.
    #[test]
    #[ignore]
    fn dump_png() {
        let load = |n: &str| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/{n}", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()
        };
        let d = Dashboard::from_json(&load("dashboard_demo.json")["dashboard"]);
        let panel = d.panels.iter().find(|p| p.title == "Demo signals").unwrap();
        let frames: Vec<Frame> =
            load("query_range.json")["results"]["A"]["frames"].as_array().unwrap().iter().map(|f| Frame::from_json(f).unwrap()).collect();
        let series = series_from_frames(&frames);
        let (a, b) = (series[0].points[0].0, series[0].points.last().unwrap().0);
        let data = PanelData { frames, series, range: (a, b), ..Default::default() };
        let ann = [ChartAnnotation { start: a + (b - a) / 3, end: a + (b - a) / 3, color: (0x57, 0x94, 0xF2), text: "deploy".into() }];
        let spec = chart_spec(panel, &data, &ann, 10_000);
        let layout = ChartLayout::compute(&spec, ratatui::layout::Rect::new(0, 0, 100, 20));
        let (w, h) = (layout.plot.width as u32 * 10, layout.plot.height as u32 * 20);
        let base = render_base(&spec, &layout, w, h, 20).unwrap();
        let ov = composite_overlay(
            &base,
            &spec,
            &layout,
            &Overlay { crosshair: Some(a + (b - a) * 2 / 3), selection: Some((a + (b - a) / 10, a + (b - a) / 4)), focused: true },
            20,
        );
        let mut img = to_rgba_image(ov);
        // Composite over a dark background so transparency is visible.
        for p in img.pixels_mut() {
            let al = p[3] as u32;
            for c in 0..3 {
                p[c] = ((p[c] as u32 * al + 0x18 * (255 - al)) / 255) as u8;
            }
            p[3] = 255;
        }
        img.save(std::env::var("LG_PNG_OUT").unwrap()).unwrap();
    }
}
