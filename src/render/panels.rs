//! Panel widgets: turn a dashboard `Panel` plus its query results into
//! terminal output, using pixel images where available.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Sparkline, Table, Widget, Wrap};
use serde_json::Value;

use std::hash::{Hash, Hasher};
use std::rc::Rc;

use super::chart::{
    ChartAnnotation, ChartLayout, ChartSeries, ChartSpec, Overlay, Rgb, ThresholdStyle, render_overlay_cells, render_unicode,
    render_unicode_base,
};
use super::image::{GraphicsState, ImageCache};
use super::raster::{HeatmapSpec, heat_color};
use crate::grafana::dashboard::Panel;
use crate::grafana::dataframe::{FieldType, Frame, LogLevel, LogLine, Series, Values};
use crate::timerange::{Millis, format_absolute};
use crate::units::{self, named_rgb, palette, rgb, threshold_color};

#[derive(Debug, Clone, Default)]
pub struct PanelData {
    pub frames: Vec<Frame>,
    pub series: Vec<Series>,
    pub logs: Vec<LogLine>,
    pub errors: Vec<String>,
    pub loading: bool,
    pub range: (Millis, Millis),
    /// Step in ms actually used, for the aliasing hint.
    pub step: Option<Millis>,
    /// Identity of this result for render caches (0 = uncached).
    pub version: u64,
    /// Data points received (for `:stats`).
    pub points: usize,
}

impl PanelData {
    /// Turns a query response into ready-to-draw data. Runs in the background
    /// task, so the UI thread only swaps the result in. Raw frames are kept only
    /// when `keep_frames` (tables, Explore's table view).
    pub fn prepare(resp: crate::grafana::QueryResponse, range: (Millis, Millis), step: Millis, keep_frames: bool) -> Self {
        let errors = resp.errors();
        let frames = resp.into_frames();
        let points = frames.iter().map(|f| f.len() * f.fields.len().saturating_sub(1)).sum();
        let mut logs = crate::grafana::dataframe::logs_from_frames(&frames);
        logs.sort_by_key(|l| std::cmp::Reverse(l.time));
        Self {
            series: crate::grafana::dataframe::series_from_frames(&frames),
            logs,
            frames: if keep_frames { frames } else { Vec::new() },
            errors,
            loading: false,
            range,
            step: Some(step),
            version: next_version(),
            points,
        }
    }

    /// No frames, series or log lines.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.series.is_empty() && self.logs.is_empty()
    }
}

static VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A fresh, unique data version.
pub fn next_version() -> u64 {
    VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Sorted, de-duplicated timestamps of a result (crosshair stops).
pub fn timestamps(data: &PanelData) -> Vec<Millis> {
    let mut ts: Vec<Millis> = data.series.iter().flat_map(|s| s.points.iter().map(|(t, _)| *t)).collect();
    ts.sort_unstable();
    ts.dedup();
    ts
}

pub struct PanelCtx<'a> {
    pub gfx: &'a GraphicsState,
    pub cache: &'a mut ImageCache,
    pub marker: Marker,
    pub overlay: Overlay,
    pub annotations: &'a [ChartAnnotation],
    /// False while a popup overlaps images on terminals that can't layer them.
    pub images: bool,
    pub key: String,
    pub wrap_logs: bool,
}

const DIM: Color = Color::DarkGray;

fn series_color(panel: &Panel, name: &str, idx: usize) -> Rgb {
    if let Some(c) = panel.override_for(name, "color").and_then(|v| v.get("fixedColor")).and_then(Value::as_str).and_then(named_rgb) {
        return c;
    }
    if panel.defaults.color.is_some()
        && panel.overrides.is_empty()
        && idx == 0
        && let Some(c) = panel.defaults.color.as_deref().and_then(named_rgb)
    {
        return c;
    }
    palette(idx)
}

/// Builds the chart description for a time series panel.
/// Builds the chart for a panel, downsampled to `buckets` columns (min/max per column).
pub fn chart_spec(panel: &Panel, data: &PanelData, annotations: &[ChartAnnotation], buckets: usize) -> ChartSpec {
    let series = data
        .series
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let negate = panel.override_for(&s.name, "custom.transform").and_then(Value::as_str) == Some("negative-Y");
            ChartSeries {
                name: s.name.clone(),
                color: series_color(panel, &s.name, i),
                points: {
                    let mapped: Vec<(Millis, Option<f64>)> =
                        s.points.iter().map(|(t, v)| (*t, v.map(|v| if negate { -v } else { v }))).collect();
                    super::chart::downsample(&mapped, data.range, buckets)
                },
            }
        })
        .collect();
    let d = &panel.defaults;
    ChartSpec {
        series,
        range: data.range,
        y_min: d.min,
        y_max: d.max,
        unit: d.unit.clone(),
        decimals: d.decimals,
        thresholds: d
            .thresholds
            .iter()
            .filter_map(|(v, c)| Some((v.as_ref().copied()?, named_rgb(c).unwrap_or((0x80, 0x80, 0x80)))))
            .collect(),
        threshold_style: ThresholdStyle::parse(d.thresholds_style.as_deref()),
        annotations: annotations.to_vec(),
        draw_style: d.draw_style,
        fill_opacity: d.fill_opacity,
        line_width: d.line_width,
        stacking: d.stacking,
        version: data.version,
    }
}

/// Renders `panel` into `area` (inside its border). Returns the chart layout for
/// time series panels, so mouse positions can be mapped to time.
pub fn render_panel(
    panel: &Panel,
    data: &PanelData,
    area: Rect,
    buf: &mut Buffer,
    ctx: &mut PanelCtx,
) -> Option<(ChartLayout, Rc<ChartSpec>)> {
    if area.width < 4 || area.height < 1 {
        return None;
    }
    if !data.errors.is_empty() && data.is_empty() {
        let text: Vec<Line> = data.errors.iter().map(|e| Line::styled(format!("⚠ {e}"), Style::default().fg(Color::LightRed))).collect();
        Paragraph::new(text).wrap(Wrap { trim: true }).render(area, buf);
        return None;
    }
    if panel.kind == "text" {
        render_text(panel, area, buf);
        return None;
    }
    if !panel.is_supported() {
        render_placeholder(panel, area, buf);
        return None;
    }
    if data.loading && data.is_empty() {
        buf.set_string(area.x, area.y, "loading…", Style::default().fg(DIM));
        return None;
    }
    if data.is_empty() {
        buf.set_string(area.x, area.y, "No data", Style::default().fg(DIM));
        return None;
    }
    match panel.kind.as_str() {
        "timeseries" | "graph" | "barchart" => return render_timeseries(panel, data, area, buf, ctx),
        "stat" | "singlestat" => render_stat(panel, data, area, buf),
        "gauge" => render_gauge(panel, data, area, buf),
        "bargauge" => render_bargauge(panel, data, area, buf),
        "table" | "table-old" => render_table(panel, &data.frames, area, buf),
        "heatmap" => render_heatmap(panel, data, area, buf, ctx),
        "logs" => render_logs(&data.logs, area, buf, ctx.wrap_logs, None),
        _ => render_placeholder(panel, area, buf),
    }
    None
}

pub fn render_timeseries(
    panel: &Panel,
    data: &PanelData,
    area: Rect,
    buf: &mut Buffer,
    ctx: &mut PanelCtx,
) -> Option<(ChartLayout, Rc<ChartSpec>)> {
    // Draw at most two points per pixel column, and reuse the built chart while the data is unchanged.
    let per_cell = if ctx.gfx.mode.is_image() && ctx.images { ctx.gfx.cell.0 as usize } else { 2 };
    let buckets = (area.width as usize * per_cell).max(16);
    let version = if data.version == 0 {
        0
    } else {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (data.version, buckets, panel.id, ctx.annotations.len()).hash(&mut h);
        for a in ctx.annotations {
            (a.start, a.end).hash(&mut h);
        }
        h.finish().max(1)
    };
    let spec = ctx.cache.spec(&format!("{}#spec", ctx.key), version, || chart_spec(panel, data, ctx.annotations, buckets));
    let legend_mode = panel.options.pointer("/legend/showLegend").and_then(Value::as_bool).unwrap_or(true)
        && panel.options.pointer("/legend/displayMode").and_then(Value::as_str) != Some("hidden");
    let legend_rows = if legend_mode && area.height >= 6 {
        (spec.series.len() as u16).min(area.height / 3).max(1) + u16::from(ctx.overlay.crosshair.is_some())
    } else {
        0
    };
    let chart_area = Rect { height: area.height - legend_rows, ..area };
    let legend_area = Rect { y: area.y + chart_area.height, height: legend_rows, ..area };
    let layout = render_chart(&spec, chart_area, buf, ctx);
    if legend_rows > 0 {
        let calcs: Vec<String> = panel
            .options
            .pointer("/legend/calcs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        render_legend(&spec, data, legend_area, buf, ctx.overlay.crosshair, &calcs);
    }
    Some((layout, spec))
}

/// Draws a chart (axes + plot) with the best available backend.
pub fn render_chart(spec: &ChartSpec, area: Rect, buf: &mut Buffer, ctx: &mut PanelCtx) -> ChartLayout {
    let layout = ChartLayout::compute(spec, area);
    let use_image = ctx.gfx.mode.is_image() && ctx.images && layout.plot.width >= 4 && layout.plot.height >= 2;
    let mut drawn = false;
    if use_image && let Some(proto) = ctx.cache.chart(&ctx.key, ctx.gfx, spec, &layout, &ctx.overlay) {
        ratatui_image::Image::new(proto).render(layout.plot, buf);
        drawn = true;
    }
    if !drawn {
        render_unicode_cached(spec, &layout, buf, ctx);
    }
    layout.render_axes(spec, area, buf);
    layout
}

/// Unicode chart: the static base is cached per chart (keyed by data version and
/// geometry), so moving the crosshair only repaints the overlay cells.
fn render_unicode_cached(spec: &ChartSpec, layout: &ChartLayout, buf: &mut Buffer, ctx: &mut PanelCtx) {
    let plot = layout.plot;
    if spec.version == 0 {
        render_unicode(spec, layout, &ctx.overlay, ctx.marker, buf);
        return;
    }
    let key = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (spec.base_hash(), plot.width, plot.height, format!("{:?}", ctx.marker)).hash(&mut h);
        h.finish()
    };
    let cell_key = format!("{}#cells", ctx.key);
    if ctx.cache.cells(&cell_key, key).is_none() {
        let mut tmp = Buffer::empty(plot);
        render_unicode_base(spec, layout, ctx.marker, &mut tmp);
        ctx.cache.put_cells(&cell_key, key, tmp);
    }
    if let Some(cached) = ctx.cache.cells(&cell_key, key) {
        let src = cached.area;
        for dy in 0..plot.height.min(src.height) {
            for dx in 0..plot.width.min(src.width) {
                if let (Some(from), Some(to)) = (cached.cell((src.x + dx, src.y + dy)), buf.cell_mut((plot.x + dx, plot.y + dy))) {
                    *to = from.clone();
                }
            }
        }
    }
    render_overlay_cells(spec, layout, &ctx.overlay, buf);
}

fn render_legend(spec: &ChartSpec, data: &PanelData, area: Rect, buf: &mut Buffer, crosshair: Option<Millis>, calcs: &[String]) {
    let mut y = area.y;
    if let Some(t) = crosshair {
        buf.set_string(area.x, y, format!("⏱ {}", format_absolute(t)), Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD));
        y += 1;
    }
    let name_w = spec.series.iter().map(|s| s.name.chars().count()).max().unwrap_or(0).min(area.width as usize / 2).max(4);
    let show_calcs: Vec<&str> = if crosshair.is_none() && !calcs.is_empty() && area.width as usize > name_w + 12 * calcs.len() {
        calcs.iter().map(String::as_str).collect()
    } else {
        vec!["lastNotNull"]
    };
    for (i, s) in spec.series.iter().enumerate() {
        if y >= area.y + area.height {
            break;
        }
        let remaining = spec.series.len() - i;
        if y == area.y + area.height - 1 && remaining > 1 {
            buf.set_string(area.x, y, format!("… {remaining} more series"), Style::default().fg(DIM));
            break;
        }
        let src = data.series.get(i);
        let mut spans =
            vec![Span::styled("■ ", Style::default().fg(rgb(s.color))), Span::raw(crate::app::widgets::truncate(&s.name, name_w))];
        let pad = name_w.saturating_sub(s.name.chars().count().min(name_w));
        spans.push(Span::raw(" ".repeat(pad + 2)));
        for c in &show_calcs {
            let v = match crosshair {
                Some(t) => s.points.iter().min_by_key(|(pt, _)| (pt - t).abs()).and_then(|(_, v)| *v),
                None => src.and_then(|s| s.reduce(c)),
            };
            let text = v.map(|v| units::format(v, spec.unit.as_deref(), spec.decimals)).unwrap_or_else(|| "—".into());
            let label = if show_calcs.len() > 1 { format!("{}={text}  ", short_calc(c)) } else { format!("{text}  ") };
            spans.push(Span::styled(label, Style::default().fg(Color::White)));
        }
        buf.set_line(area.x, y, &Line::from(spans), area.width);
        y += 1;
    }
}

fn short_calc(c: &str) -> &str {
    match c {
        "lastNotNull" | "last" => "last",
        "mean" => "mean",
        other => other,
    }
}

fn values_for_reduce(panel: &Panel, data: &PanelData) -> Vec<(String, Option<f64>, Vec<u64>)> {
    let calc = panel.reduce_calc();
    data.series
        .iter()
        .map(|s| {
            let spark: Vec<u64> = {
                let vals: Vec<f64> = s.points.iter().filter_map(|(_, v)| *v).collect();
                let (lo, hi) = vals.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(*v), b.max(*v)));
                vals.iter().map(|v| if hi > lo { ((v - lo) / (hi - lo) * 100.0) as u64 + 1 } else { 1 }).collect()
            };
            (s.name.clone(), s.reduce(calc), spark)
        })
        .collect()
}

fn value_style(panel: &Panel, v: Option<f64>) -> (String, Rgb) {
    let d = &panel.defaults;
    match v {
        None => ("No data".into(), (0x80, 0x80, 0x80)),
        Some(v) => {
            if let Some((text, color)) = d.mapped(v) {
                let c = color.as_deref().and_then(named_rgb).or_else(|| threshold_color(v, &d.thresholds)).unwrap_or((0x73, 0xBF, 0x69));
                return (text, c);
            }
            let c = threshold_color(v, &d.thresholds).unwrap_or((0x73, 0xBF, 0x69));
            (units::format(v, d.unit.as_deref(), d.decimals), c)
        }
    }
}

pub fn render_stat(panel: &Panel, data: &PanelData, area: Rect, buf: &mut Buffer) {
    let vals = values_for_reduce(panel, data);
    if vals.is_empty() {
        return;
    }
    let background = panel.options.get("colorMode").and_then(Value::as_str) == Some("background");
    let graph = panel.options.get("graphMode").and_then(Value::as_str) != Some("none");
    // Grid of cells: as many columns as fit (≥ 8 wide), rows of ≥ 3 lines.
    let cols = (vals.len() as u16).min((area.width / 8).max(1));
    let rows = (vals.len() as u16).div_ceil(cols).min((area.height / 3).max(1));
    let (w, h) = (area.width / cols, area.height / rows);
    for (i, (name, v, spark)) in vals.iter().enumerate().take((cols * rows) as usize) {
        let (cx, cy) = (i as u16 % cols, i as u16 / cols);
        let cell = Rect {
            x: area.x + cx * w,
            y: area.y + cy * h,
            width: if cx == cols - 1 { area.width - cx * w } else { w },
            height: if cy == rows - 1 { area.height - cy * h } else { h },
        };
        let (text, color) = value_style(panel, *v);
        if background {
            let bg = rgb((color.0 / 2, color.1 / 2, color.2 / 2));
            for y in cell.y..cell.y + cell.height {
                for x in cell.x..cell.x + cell.width {
                    if let Some(c) = buf.cell_mut((x, y)) {
                        c.set_bg(bg);
                    }
                }
            }
        }
        let mid = cell.y + cell.height.saturating_sub(1) / 2;
        let fg = if background { Color::White } else { rgb(color) };
        let width = cell.width.saturating_sub(1) as usize;
        let centered = |s: &str| cell.x + cell.width.saturating_sub(s.chars().count().min(width) as u16) / 2;
        if vals.len() > 1 && mid > cell.y {
            let label = crate::app::widgets::truncate(name, width);
            buf.set_string(centered(&label), mid - 1, &label, Style::default().fg(if background { Color::Gray } else { DIM }));
        }
        let value = crate::app::widgets::truncate(&text, width);
        buf.set_string(centered(&value), mid, &value, Style::default().fg(fg).add_modifier(Modifier::BOLD));
        if graph && cell.height >= 3 && !spark.is_empty() {
            let rows = (cell.y + cell.height).saturating_sub(mid + 1).min(3);
            if rows > 0 {
                let sa = Rect { y: cell.y + cell.height - rows, height: rows, ..cell };
                let data: Vec<u64> = resample(spark, sa.width as usize);
                Sparkline::default()
                    .data(&data)
                    .style(Style::default().fg(rgb((color.0 / 3 * 2, color.1 / 3 * 2, color.2 / 3 * 2))))
                    .render(sa, buf);
            }
        }
    }
}

fn resample(v: &[u64], n: usize) -> Vec<u64> {
    if v.len() <= n || n == 0 {
        return v.to_vec();
    }
    (0..n).map(|i| v[i * v.len() / n]).collect()
}

pub fn render_gauge(panel: &Panel, data: &PanelData, area: Rect, buf: &mut Buffer) {
    let vals = values_for_reduce(panel, data);
    let min = panel.defaults.min.unwrap_or(0.0);
    let max = panel.defaults.max.unwrap_or(100.0);
    let rows_each = (area.height / vals.len().max(1) as u16).max(1);
    for (i, (name, v, _)) in vals.iter().enumerate() {
        let y = area.y + i as u16 * rows_each;
        if y >= area.y + area.height {
            break;
        }
        let (text, color) = value_style(panel, *v);
        let frac = v.map(|v| ((v - min) / (max - min)).clamp(0.0, 1.0)).unwrap_or(0.0);
        let mid = y + rows_each.saturating_sub(2) / 2;
        let header = if vals.len() > 1 { format!("{name}  {text}") } else { text.clone() };
        let x = area.x + area.width.saturating_sub(header.chars().count() as u16) / 2;
        buf.set_string(
            x,
            mid,
            crate::app::widgets::truncate(&header, area.width as usize),
            Style::default().fg(rgb(color)).add_modifier(Modifier::BOLD),
        );
        if mid + 1 < area.y + area.height {
            draw_bar(
                buf,
                Rect { x: area.x + 1, y: mid + 1, width: area.width.saturating_sub(2), height: 1 },
                frac,
                color,
                &panel.defaults.thresholds,
                min,
                max,
                false,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_bar(buf: &mut Buffer, r: Rect, frac: f64, color: Rgb, thresholds: &[(Option<f64>, String)], min: f64, max: f64, lcd: bool) {
    let total = r.width as f64 * 8.0;
    let filled = (frac * total).round() as usize;
    for i in 0..r.width as usize {
        let cell_fill = filled.saturating_sub(i * 8).min(8);
        let pos_value = min + (max - min) * (i as f64 + 0.5) / r.width as f64;
        let c = if lcd { threshold_color(pos_value, thresholds).unwrap_or(color) } else { color };
        let (sym, fg) = if lcd {
            (if cell_fill >= 4 { "▮" } else { "▯" }, if cell_fill >= 4 { rgb(c) } else { Color::Rgb(0x33, 0x33, 0x3a) })
        } else {
            let blocks = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
            (blocks[cell_fill], rgb(c))
        };
        if let Some(cell) = buf.cell_mut((r.x + i as u16, r.y)) {
            cell.set_symbol(sym).set_fg(fg).set_bg(Color::Rgb(0x1c, 0x1d, 0x22));
        }
    }
}

pub fn render_bargauge(panel: &Panel, data: &PanelData, area: Rect, buf: &mut Buffer) {
    let vals = values_for_reduce(panel, data);
    let lcd = panel.options.get("displayMode").and_then(Value::as_str) == Some("lcd");
    let min = panel.defaults.min.unwrap_or(0.0);
    let max = panel.defaults.max.unwrap_or_else(|| vals.iter().filter_map(|(_, v, _)| *v).fold(0.0, f64::max).max(1.0));
    let name_w = vals.iter().map(|(n, _, _)| n.chars().count()).max().unwrap_or(0).min(area.width as usize / 3) as u16;
    let rows_each = (area.height / vals.len().max(1) as u16).clamp(1, 2);
    // Reserve the last row for "… N more" when not everything fits.
    let fits = (area.height / rows_each) as usize;
    let shown = if vals.len() > fits { fits.saturating_sub(1) } else { vals.len() };
    if shown < vals.len() {
        buf.set_string(area.x, area.y + area.height - 1, format!("… {} more", vals.len() - shown), Style::default().fg(DIM));
    }
    for (i, (name, v, _)) in vals.iter().enumerate().take(shown) {
        let y = area.y + i as u16 * rows_each;
        let (text, color) = value_style(panel, *v);
        let val_w = 10.min(area.width / 4);
        buf.set_string(area.x, y, crate::app::widgets::truncate(name, name_w as usize), Style::default().fg(Color::Gray));
        let bar = Rect { x: area.x + name_w + 1, y, width: area.width.saturating_sub(name_w + val_w + 2), height: 1 };
        let frac = v.map(|v| ((v - min) / (max - min)).clamp(0.0, 1.0)).unwrap_or(0.0);
        draw_bar(buf, bar, frac, color, &panel.defaults.thresholds, min, max, lcd);
        buf.set_string(
            bar.x + bar.width + 1,
            y,
            crate::app::widgets::truncate(&text, val_w as usize),
            Style::default().fg(rgb(color)).add_modifier(Modifier::BOLD),
        );
    }
}

/// Builds rows/columns for a table: one row per instant series (labels as
/// columns), or the first frame's fields as-is.
pub fn table_model(frames: &[Frame], hidden: &dyn Fn(&str) -> bool) -> (Vec<String>, Vec<Vec<String>>) {
    // Instant series (one row each, labels on the value) read best as one row per series.
    let labelled = !frames.is_empty()
        && frames.iter().all(|f| f.fields.iter().filter(|x| x.kind == FieldType::Number).count() == 1 && f.len() <= 1)
        && frames.iter().any(|f| f.fields.iter().any(|x| !x.labels.is_empty()));
    if labelled {
        let mut keys: Vec<String> = frames
            .iter()
            .flat_map(|f| f.fields.iter().flat_map(|x| x.labels.keys().cloned()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        keys.retain(|k| !hidden(k));
        let mut cols: Vec<String> = Vec::new();
        let show_time = !hidden("Time");
        if show_time {
            cols.push("Time".into());
        }
        cols.extend(keys.iter().cloned());
        cols.push("Value".into());
        let rows = frames
            .iter()
            .map(|f| {
                let num = f.fields.iter().find(|x| x.kind == FieldType::Number).expect("checked");
                let mut row = Vec::new();
                if show_time {
                    row.push(f.time_field().map(|t| t.values.display(0)).unwrap_or_default());
                }
                row.extend(keys.iter().map(|k| num.labels.get(k).cloned().unwrap_or_default()));
                row.push(num.values.display(0));
                row
            })
            .collect();
        return (cols, rows);
    }
    let Some(frame) = frames.iter().find(|f| !f.fields.is_empty()) else { return (Vec::new(), Vec::new()) };
    let fields: Vec<_> = frame.fields.iter().filter(|f| !f.is_hidden() && !hidden(&f.name)).collect();
    let cols = fields.iter().map(|f| f.display_name()).collect();
    let rows = (0..frame.len()).map(|i| fields.iter().map(|f| f.values.display(i)).collect()).collect();
    (cols, rows)
}

pub fn render_table(panel: &Panel, frames: &[Frame], area: Rect, buf: &mut Buffer) {
    let hidden = |name: &str| panel.override_for(name, "custom.hidden").and_then(Value::as_bool).unwrap_or(false);
    render_rows(table_model(frames, &hidden), area, buf, 0);
}

pub fn render_rows((cols, rows): (Vec<String>, Vec<Vec<String>>), area: Rect, buf: &mut Buffer, scroll: usize) {
    if cols.is_empty() {
        return;
    }
    let widths: Vec<Constraint> = (0..cols.len())
        .map(|i| {
            let w = std::iter::once(cols[i].chars().count())
                .chain(rows.iter().map(|r| r.get(i).map(|c| c.chars().count()).unwrap_or(0)))
                .max()
                .unwrap_or(4);
            Constraint::Max(w.min(40) as u16 + 1)
        })
        .collect();
    let header = Row::new(cols.iter().map(|c| Cell::from(c.clone()))).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let body: Vec<Row> = rows.iter().skip(scroll).map(|r| Row::new(r.iter().map(|c| Cell::from(c.clone())))).collect();
    Widget::render(Table::new(body, widths).header(header).column_spacing(2), area, buf);
}

/// Converts `le` bucket series into a heatmap (de-cumulating Prometheus histograms).
pub fn heatmap_spec(data: &PanelData) -> Option<HeatmapSpec> {
    let mut buckets: Vec<(f64, &Series)> = data
        .series
        .iter()
        .map(|s| {
            let le = s.labels.get("le").cloned().unwrap_or_else(|| s.name.clone());
            let v = if le == "+Inf" { f64::INFINITY } else { le.parse::<f64>().unwrap_or(f64::NAN) };
            (v, s)
        })
        .filter(|(v, _)| !v.is_nan())
        .collect();
    if buckets.is_empty() {
        return None;
    }
    buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut times: Vec<Millis> = buckets.iter().flat_map(|(_, s)| s.points.iter().map(|(t, _)| *t)).collect();
    times.sort_unstable();
    times.dedup();
    let grid: Vec<Vec<f64>> = buckets.iter().map(|(_, s)| times.iter().map(|t| s.value_at(*t).unwrap_or(0.0)).collect()).collect();
    let cumulative = grid.windows(2).all(|w| w[1].iter().zip(&w[0]).all(|(b, a)| b + 1e-9 >= *a));
    let cells: Vec<Vec<f64>> = if cumulative {
        grid.iter()
            .enumerate()
            .map(|(i, row)| if i == 0 { row.clone() } else { row.iter().zip(&grid[i - 1]).map(|(b, a)| (b - a).max(0.0)).collect() })
            .collect()
    } else {
        grid
    };
    Some(HeatmapSpec {
        times,
        buckets: buckets.iter().map(|(v, _)| if v.is_infinite() { "+Inf".into() } else { units::format_short(*v, None) }).collect(),
        cells,
        range: data.range,
    })
}

pub fn render_heatmap(panel: &Panel, data: &PanelData, area: Rect, buf: &mut Buffer, ctx: &mut PanelCtx) {
    let Some(spec) = heatmap_spec(data) else {
        buf.set_string(area.x, area.y, "No histogram buckets (expects `le` series)", Style::default().fg(DIM));
        return;
    };
    let label_w = spec.buckets.iter().map(|b| b.chars().count()).max().unwrap_or(1).clamp(6, 10) as u16 + 1;
    let plot =
        Rect { x: area.x + label_w, y: area.y, width: area.width.saturating_sub(label_w), height: area.height.saturating_sub(1).max(1) };
    let unit = panel.options.pointer("/yAxis/unit").and_then(Value::as_str).map(str::to_string);
    // Bucket labels on the left (a few of them).
    let n = spec.buckets.len();
    let step = (n as f64 / plot.height.max(1) as f64).ceil().max(1.0) as usize;
    for (i, b) in spec.buckets.iter().enumerate().step_by(step) {
        let row =
            plot.y + plot.height - 1 - ((i as f64 + 0.5) / n as f64 * plot.height as f64).floor().min(plot.height as f64 - 1.0) as u16;
        let label = match (&unit, b.parse::<f64>()) {
            (Some(u), Ok(v)) => units::format(v, Some(u), None),
            _ => b.clone(),
        };
        buf.set_string(area.x, row, crate::app::widgets::truncate(&label, label_w as usize - 1), Style::default().fg(DIM));
    }
    let use_image = ctx.gfx.mode.is_image() && ctx.images;
    if use_image && let Some(p) = ctx.cache.heatmap(&ctx.key, ctx.gfx, &spec, plot) {
        ratatui_image::Image::new(p).render(plot, buf);
    } else {
        // Half-block cells: two buckets per row (upper = fg, lower = bg).
        let max = spec.max();
        let (from, to) = spec.range;
        for col in 0..plot.width {
            let t = from.saturating_add((to.saturating_sub(from) as f64 * (col as f64 + 0.5) / plot.width as f64) as Millis);
            let ti = spec.times.partition_point(|x| *x < t).min(spec.times.len().saturating_sub(1));
            for row in 0..plot.height {
                let sample = |sub: f64| {
                    let frac = 1.0 - (row as f64 * 2.0 + sub + 0.5) / (plot.height as f64 * 2.0);
                    let bi = ((frac * n as f64) as usize).min(n - 1);
                    spec.cells[bi].get(ti).copied().unwrap_or(0.0)
                };
                let (top, bottom) = (sample(0.0), sample(1.0));
                if let Some(cell) = buf.cell_mut((plot.x + col, plot.y + row)) {
                    cell.set_symbol("▀").set_fg(rgb(heat_color(top / max.max(1e-12)))).set_bg(rgb(heat_color(bottom / max.max(1e-12))));
                }
            }
        }
    }
    let span = spec.range.1.saturating_sub(spec.range.0).max(1);
    let y = plot.y + plot.height;
    if y < area.y + area.height {
        for t in super::chart::x_ticks(spec.range.0, spec.range.1, (plot.width / 12).max(1) as usize) {
            let col = plot.x + (t.saturating_sub(spec.range.0) as f64 / span as f64 * plot.width as f64) as u16;
            buf.set_string(
                col.min(plot.x + plot.width.saturating_sub(5)),
                y,
                crate::timerange::format_tick(t, span),
                Style::default().fg(DIM),
            );
        }
    }
}

pub fn level_style(l: LogLevel) -> (&'static str, Color) {
    match l {
        LogLevel::Critical => ("CRIT", Color::Magenta),
        LogLevel::Error => ("ERR ", Color::LightRed),
        LogLevel::Warn => ("WARN", Color::Yellow),
        LogLevel::Info => ("INFO", Color::Green),
        LogLevel::Debug => ("DBG ", Color::Blue),
        LogLevel::Trace => ("TRC ", Color::DarkGray),
        LogLevel::Unknown => ("    ", Color::DarkGray),
    }
}

/// Renders log lines newest-first. `selected` highlights one line.
pub fn render_logs(logs: &[LogLine], area: Rect, buf: &mut Buffer, wrap: bool, selected: Option<usize>) {
    let mut y = area.y;
    for (i, l) in logs.iter().enumerate() {
        if y >= area.y + area.height {
            break;
        }
        let (tag, color) = level_style(l.level());
        let time = chrono::DateTime::from_timestamp_millis(l.time)
            .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S%.3f").to_string())
            .unwrap_or_default();
        let sel = selected == Some(i);
        let base = if sel { Style::default().bg(Color::Rgb(0x2a, 0x2f, 0x3a)) } else { Style::default() };
        let prefix = vec![
            Span::styled(format!("{time} "), base.fg(DIM)),
            Span::styled(format!("{tag} "), base.fg(color).add_modifier(Modifier::BOLD)),
        ];
        let prefix_w = 13 + 5;
        let text_w = area.width.saturating_sub(prefix_w) as usize;
        if wrap && text_w > 0 {
            let lines = textwrap_simple(&l.line, text_w);
            for (j, chunk) in lines.iter().enumerate() {
                if y >= area.y + area.height {
                    break;
                }
                let mut spans = if j == 0 { prefix.clone() } else { vec![Span::styled(" ".repeat(prefix_w as usize), base)] };
                spans.push(Span::styled(chunk.clone(), base));
                buf.set_line(area.x, y, &Line::from(spans), area.width);
                y += 1;
            }
        } else {
            let mut spans = prefix;
            spans.push(Span::styled(crate::app::widgets::truncate(&l.line.replace('\t', "  "), text_w.max(1)), base));
            buf.set_line(area.x, y, &Line::from(spans), area.width);
            y += 1;
        }
    }
}

fn textwrap_simple(s: &str, w: usize) -> Vec<String> {
    let chars: Vec<char> = s.replace('\t', "  ").chars().collect();
    chars.chunks(w.max(1)).map(|c| c.iter().collect()).collect()
}

pub fn render_text(panel: &Panel, area: Rect, buf: &mut Buffer) {
    let content = panel.options.get("content").and_then(Value::as_str).unwrap_or_default();
    Paragraph::new(markdown_lines(content)).wrap(Wrap { trim: false }).render(area, buf);
}

/// Minimal markdown: headings, bullets, **bold**, `code`, [links](url).
pub fn markdown_lines(md: &str) -> Vec<Line<'static>> {
    md.lines()
        .map(|raw| {
            let trimmed = raw.trim_start();
            if let Some(h) = trimmed.strip_prefix('#') {
                let h = h.trim_start_matches('#').trim();
                return Line::styled(h.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
            }
            let (bullet, rest) = match trimmed.strip_prefix("* ").or_else(|| trimmed.strip_prefix("- ")) {
                Some(r) => ("  • ", r),
                None => ("", trimmed),
            };
            let mut spans = vec![Span::raw(bullet)];
            spans.extend(inline_md(rest));
            Line::from(spans)
        })
        .collect()
}

fn inline_md(s: &str) -> Vec<Span<'static>> {
    let re = regex::Regex::new(r"\*\*([^*]+)\*\*|\*([^*]+)\*|`([^`]+)`|\[([^\]]+)\]\(([^)]+)\)").expect("valid regex");
    let mut out = Vec::new();
    let mut last = 0;
    for c in re.captures_iter(s) {
        let m = c.get(0).expect("match");
        out.push(Span::raw(s[last..m.start()].to_string()));
        if let Some(b) = c.get(1) {
            out.push(Span::styled(b.as_str().to_string(), Style::default().add_modifier(Modifier::BOLD)));
        } else if let Some(i) = c.get(2) {
            out.push(Span::styled(i.as_str().to_string(), Style::default().add_modifier(Modifier::ITALIC)));
        } else if let Some(code) = c.get(3) {
            out.push(Span::styled(code.as_str().to_string(), Style::default().fg(Color::Yellow)));
        } else if let Some(t) = c.get(4) {
            out.push(Span::styled(t.as_str().to_string(), Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED)));
        }
        last = m.end();
    }
    out.push(Span::raw(s[last..].to_string()));
    out
}

pub fn render_placeholder(panel: &Panel, area: Rect, buf: &mut Buffer) {
    let lines = vec![
        Line::styled(format!("◇ {} panel", panel.kind), Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)),
        Line::styled("not rendered in the terminal", Style::default().fg(DIM)),
        Line::from(vec![
            Span::styled("o", Style::default().fg(Color::Cyan)),
            Span::styled(" open in browser  ", Style::default().fg(DIM)),
            Span::styled("P", Style::default().fg(Color::Cyan)),
            Span::styled(" exact render", Style::default().fg(DIM)),
        ]),
    ];
    let y = area.y + area.height.saturating_sub(3) / 2;
    Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center).render(Rect { y, height: area.height.min(3), ..area }, buf);
}

/// Tabular dump of any frames (Explore's table view).
pub fn frames_table(frames: &[Frame]) -> (Vec<String>, Vec<Vec<String>>) {
    if frames.len() > 1 && frames.iter().all(|f| f.time_field().is_some() && f.fields.iter().any(|x| x.kind == FieldType::Number)) {
        // Wide view of series: one column per series, rows by time (latest first).
        let series = crate::grafana::dataframe::series_from_frames(frames);
        let mut times: Vec<Millis> = series.iter().flat_map(|s| s.points.iter().map(|(t, _)| *t)).collect();
        times.sort_unstable();
        times.dedup();
        times.reverse();
        let mut cols = vec!["Time".to_string()];
        cols.extend(series.iter().map(|s| s.name.clone()));
        let rows = times
            .iter()
            .map(|t| {
                let mut r = vec![format_absolute(*t)];
                r.extend(series.iter().map(|s| {
                    s.points.iter().find(|(pt, _)| pt == t).and_then(|(_, v)| *v).map(|v| units::format_short(v, None)).unwrap_or_default()
                }));
                r
            })
            .collect();
        return (cols, rows);
    }
    table_model(frames, &|_| false)
}

/// Every string field value, for log frames without a `Line` field.
pub fn any_strings(frame: &Frame) -> Vec<String> {
    frame
        .fields
        .iter()
        .filter_map(|f| match &f.values {
            Values::String(v) => Some(v.iter().flatten().cloned().collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grafana::dashboard::Dashboard;
    use crate::grafana::dataframe::series_from_frames;

    fn fixture(name: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()
    }

    fn frames(name: &str) -> Vec<Frame> {
        fixture(name)["results"]["A"]["frames"].as_array().unwrap().iter().map(|f| Frame::from_json(f).unwrap()).collect()
    }

    fn dash() -> Dashboard {
        Dashboard::from_json(&fixture("dashboard_demo.json")["dashboard"])
    }

    fn data(name: &str) -> PanelData {
        let fs = frames(name);
        let series = series_from_frames(&fs);
        let range = series.iter().flat_map(|s| s.points.iter().map(|p| p.0)).fold((i64::MAX, i64::MIN), |(a, b), t| (a.min(t), b.max(t)));
        PanelData { logs: crate::grafana::dataframe::logs_from_frames(&fs), frames: fs, series, range, ..Default::default() }
    }

    fn render(panel: &Panel, d: &PanelData, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let gfx = GraphicsState::unicode();
        let mut cache = ImageCache::default();
        let mut ctx = PanelCtx {
            gfx: &gfx,
            cache: &mut cache,
            marker: Marker::Braille,
            overlay: Overlay::default(),
            annotations: &[],
            images: false,
            key: "t".into(),
            wrap_logs: false,
        };
        render_panel(panel, d, area, &mut buf, &mut ctx);
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn renders_each_panel_type() {
        let d = dash();
        let by = |t: &str| d.panels.iter().find(|p| p.kind == t).unwrap().clone();
        let ts = render(&by("timeseries"), &data("query_range.json"), 80, 16);
        assert!(ts.contains("sine eu-west-1"), "{ts}");
        let stat = render(&by("stat"), &data("query_range.json"), 40, 5);
        assert!(stat.chars().any(|c| c.is_ascii_digit()), "{stat}");
        let gauge = render(&by("gauge"), &data("query_range.json"), 40, 5);
        assert!(gauge.contains('█') || gauge.contains('▏') || gauge.chars().any(|c| ('▏'..='█').contains(&c)), "{gauge}");
        let table = render(&by("table"), &data("query_table.json"), 80, 8);
        assert!(table.contains("region") && table.contains("Value") && !table.contains("Time"), "{table}");
        let text =
            render(&d.panels.iter().find(|p| p.kind == "text").cloned().unwrap_or_else(|| by("timeseries")), &PanelData::default(), 40, 4);
        assert!(!text.is_empty());
    }

    /// Every panel type at every small size must render without panicking.
    #[test]
    fn no_panics_at_any_size() {
        let d = dash();
        let datasets =
            [data("query_range.json"), data("query_table.json"), data("query_heatmap.json"), data("query_logs.json"), PanelData::default()];
        let mut kinds: Vec<Panel> = d.panels.clone();
        for k in ["heatmap", "logs", "state-timeline", "text"] {
            kinds.push(Panel::synthetic(k));
        }
        for panel in &kinds {
            for data in &datasets {
                for w in (1..60).step_by(7) {
                    for h in 1..24 {
                        render(panel, data, w, h);
                    }
                }
            }
        }
        // Charts with crosshair/selection at the edges and outside the range.
        let spec = chart_spec(
            &kinds[0],
            &datasets[0],
            &[ChartAnnotation { start: 0, end: i64::MAX / 2, color: (1, 2, 3), text: String::new() }],
            10_000,
        );
        for (w, h) in [(1, 1), (5, 2), (20, 3), (80, 20)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            let layout = ChartLayout::compute(&spec, area);
            for ov in [
                Overlay { crosshair: Some(0), selection: Some((i64::MIN / 2, i64::MAX / 2)), focused: true },
                Overlay { crosshair: Some(spec.range.1), selection: None, focused: false },
            ] {
                render_unicode(&spec, &layout, &ov, Marker::Braille, &mut buf);
                layout.render_axes(&spec, area, &mut buf);
            }
        }
    }

    #[test]
    fn heatmap_decumulates() {
        let d = data("query_heatmap.json");
        let spec = heatmap_spec(&d).unwrap();
        assert_eq!(spec.buckets.last().unwrap(), "+Inf");
        assert!(spec.cells.iter().flatten().all(|v| *v >= 0.0));
    }

    #[test]
    fn logs_render() {
        let d = data("query_logs.json");
        let area = Rect::new(0, 0, 100, 5);
        let mut buf = Buffer::empty(area);
        render_logs(&d.logs, area, &mut buf, false, Some(0));
        let line: String = (0..100).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(line.contains(':'), "{line}");
    }

    #[test]
    fn markdown() {
        let lines = markdown_lines("# Title\n* **bold** and `code` [link](http://x)");
        assert_eq!(lines.len(), 2);
        assert!(lines[1].spans.iter().any(|s| s.content == "bold"));
    }
}
