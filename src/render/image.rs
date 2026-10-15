//! Terminal graphics: capability detection and a cache of encoded chart images.
//!
//! Each chart slot keeps a stable kitty image id, so re-transmitting replaces
//! the image in the terminal instead of leaking new ones. The base layer
//! (data) is cached; the overlay (crosshair/selection) is composited onto a
//! copy, which is cheap compared with re-rasterising.

use std::collections::HashMap;

use ratatui::layout::{Rect, Size};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::protocol::iterm2::Iterm2;
use ratatui_image::protocol::kitty::Kitty;
use ratatui_image::protocol::sixel::Sixel;
use tiny_skia::Pixmap;

use super::chart::{ChartLayout, ChartSpec, Overlay};
use super::raster;
use crate::config::GraphicsMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Graphics {
    Kitty,
    Sixel,
    Iterm,
    Unicode,
}

impl Graphics {
    pub fn is_image(self) -> bool {
        self != Self::Unicode
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::Sixel => "sixel",
            Self::Iterm => "iterm2",
            Self::Unicode => "unicode",
        }
    }

    /// Whether popups can be drawn over images without artefacts.
    pub fn supports_overlap(self) -> bool {
        matches!(self, Self::Kitty | Self::Unicode)
    }
}

pub struct GraphicsState {
    pub mode: Graphics,
    pub cell: (u16, u16),
    pub tmux: bool,
}

impl GraphicsState {
    /// Detects terminal graphics support. Must run after entering the alternate
    /// screen and before the event reader starts.
    pub fn detect(requested: GraphicsMode) -> Self {
        if requested == GraphicsMode::Unicode {
            return Self { mode: Graphics::Unicode, cell: (10, 20), tmux: false };
        }
        let picker = Picker::from_query_stdio().unwrap_or_else(|e| {
            tracing::warn!("graphics query failed: {e}");
            Picker::halfblocks()
        });
        let fs = picker.font_size();
        let detected = match picker.protocol_type() {
            ProtocolType::Kitty => Graphics::Kitty,
            ProtocolType::Sixel => Graphics::Sixel,
            ProtocolType::Iterm2 => Graphics::Iterm,
            ProtocolType::Halfblocks => Graphics::Unicode,
        };
        let mode = match requested {
            GraphicsMode::Auto => detected,
            GraphicsMode::Kitty => Graphics::Kitty,
            GraphicsMode::Sixel => Graphics::Sixel,
            GraphicsMode::Iterm => Graphics::Iterm,
            GraphicsMode::Unicode => Graphics::Unicode,
        };
        tracing::info!(?detected, ?mode, font = ?(fs.width, fs.height), tmux = picker.tmux_detected(), "graphics");
        Self { mode, cell: (fs.width.max(1), fs.height.max(1)), tmux: picker.tmux_detected() }
    }

    pub fn unicode() -> Self {
        Self { mode: Graphics::Unicode, cell: (10, 20), tmux: false }
    }
}

struct Slot {
    id: u32,
    base_key: u64,
    size: (u32, u32),
    base: Option<Pixmap>,
    overlay: Overlay,
    protocol: Option<Protocol>,
    used: bool,
}

#[derive(Default)]
pub struct ImageCache {
    slots: HashMap<String, Slot>,
    next_id: u32,
    /// Built chart specs (downsampled data), keyed like slots.
    specs: HashMap<String, (u64, std::rc::Rc<ChartSpec>)>,
    /// Rendered Unicode chart bases, keyed like slots.
    cells: HashMap<String, (u64, ratatui::buffer::Buffer)>,
    used: std::collections::HashSet<String>,
    /// Rasterise + encode durations since the last drain (for `:stats`).
    pub timings: Vec<std::time::Duration>,
}

impl ImageCache {
    /// Returns the cached spec for `key` if built from the same `version`, else builds it.
    pub fn spec(&mut self, key: &str, version: u64, make: impl FnOnce() -> ChartSpec) -> std::rc::Rc<ChartSpec> {
        self.used.insert(key.to_string());
        if version != 0
            && let Some((v, spec)) = self.specs.get(key)
            && *v == version
        {
            return spec.clone();
        }
        let spec = std::rc::Rc::new(make());
        if version != 0 {
            self.specs.insert(key.to_string(), (version, spec.clone()));
        }
        spec
    }

    /// Cached Unicode rendering of a chart's static part.
    pub fn cells(&mut self, key: &str, version: u64) -> Option<&ratatui::buffer::Buffer> {
        self.used.insert(key.to_string());
        self.cells.get(key).filter(|(v, _)| *v == version).map(|(_, b)| b)
    }

    pub fn put_cells(&mut self, key: &str, version: u64, buf: ratatui::buffer::Buffer) {
        self.cells.insert(key.to_string(), (version, buf));
    }

    fn slot(&mut self, key: &str) -> &mut Slot {
        self.used.insert(key.to_string());
        if !self.slots.contains_key(key) {
            // Kitty ids must be non-zero; keep them small and stable per slot.
            self.next_id = self.next_id.wrapping_add(1).max(1);
            let id = 0x4c47_0000 | (self.next_id & 0xffff);
            self.slots.insert(
                key.to_string(),
                Slot { id, base_key: 0, size: (0, 0), base: None, overlay: Overlay::default(), protocol: None, used: true },
            );
        }
        let s = self.slots.get_mut(key).expect("inserted");
        s.used = true;
        s
    }

    /// Returns an encoded protocol for a chart, re-rasterising only what changed.
    pub fn chart(
        &mut self,
        key: &str,
        gfx: &GraphicsState,
        spec: &ChartSpec,
        layout: &ChartLayout,
        overlay: &Overlay,
    ) -> Option<&Protocol> {
        let area = layout.plot;
        let (w, h) = (area.width as u32 * gfx.cell.0 as u32, area.height as u32 * gfx.cell.1 as u32);
        let base_key = spec.base_hash() ^ ((w as u64) << 32 | h as u64) ^ hash_layout(layout);
        let started = std::time::Instant::now();
        let mut work = false;
        let slot = self.slot(key);
        let base_changed = slot.base_key != base_key || slot.base.is_none() || slot.size != (w, h);
        if base_changed {
            slot.base = raster::render_base(spec, layout, w, h, gfx.cell.1);
            slot.base_key = base_key;
            slot.size = (w, h);
        }
        if base_changed || slot.overlay != *overlay || slot.protocol.is_none() {
            let base = slot.base.as_ref()?;
            let composed = if overlay.crosshair.is_some() || overlay.selection.is_some() {
                raster::composite_overlay(base, spec, layout, overlay, gfx.cell.1)
            } else {
                base.clone()
            };
            slot.protocol = encode(gfx, slot.id, composed, area);
            slot.overlay = *overlay;
            work = true;
        }
        if work {
            self.timings.push(started.elapsed());
        }
        self.slots.get(key).and_then(|s| s.protocol.as_ref())
    }

    pub fn heatmap(&mut self, key: &str, gfx: &GraphicsState, spec: &raster::HeatmapSpec, area: Rect) -> Option<&Protocol> {
        let (w, h) = (area.width as u32 * gfx.cell.0 as u32, area.height as u32 * gfx.cell.1 as u32);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        format!("{:?}{:?}{:?}", spec.times, spec.buckets, spec.range).hash(&mut hasher);
        for row in &spec.cells {
            for v in row {
                v.to_bits().hash(&mut hasher);
            }
        }
        let key_hash = hasher.finish() ^ ((w as u64) << 32 | h as u64);
        let slot = self.slot(key);
        if slot.base_key != key_hash || slot.protocol.is_none() {
            let pm = raster::render_heatmap(spec, w, h)?;
            slot.protocol = encode(gfx, slot.id, pm, area);
            slot.base_key = key_hash;
        }
        slot.protocol.as_ref()
    }

    /// An arbitrary PNG (exact view from the Grafana renderer), fitted to `area`.
    pub fn png(&mut self, key: &str, gfx: &GraphicsState, png: &[u8], area: Rect) -> Option<&Protocol> {
        let key_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            png.len().hash(&mut h);
            png.iter().take(4096).for_each(|b| b.hash(&mut h));
            (area.width, area.height).hash(&mut h);
            h.finish()
        };
        let slot = self.slot(key);
        if slot.base_key != key_hash || slot.protocol.is_none() {
            let img = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?;
            let (w, h) = (area.width as u32 * gfx.cell.0 as u32, area.height as u32 * gfx.cell.1 as u32);
            let img = img.resize(w, h, image::imageops::FilterType::Triangle);
            let cells = Size::new(
                (img.width().div_ceil(gfx.cell.0 as u32) as u16).min(area.width),
                (img.height().div_ceil(gfx.cell.1 as u32) as u16).min(area.height),
            );
            slot.protocol = encode_dynamic(gfx, slot.id, img, cells);
            slot.base_key = key_hash;
        }
        slot.protocol.as_ref()
    }

    /// Marks all slots unused; call before a frame, then `sweep` after.
    pub fn begin_frame(&mut self) {
        for s in self.slots.values_mut() {
            s.used = false;
        }
        self.used.clear();
    }

    pub fn sweep(&mut self) {
        self.slots.retain(|_, s| s.used);
        let used = &self.used;
        self.specs.retain(|k, _| used.contains(k));
        self.cells.retain(|k, _| used.contains(k));
    }

    pub fn clear(&mut self) {
        self.slots.clear();
        self.specs.clear();
        self.cells.clear();
    }
}

fn hash_layout(l: &ChartLayout) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (l.plot.width, l.plot.height).hash(&mut h);
    for (v, _) in &l.y_ticks {
        v.to_bits().hash(&mut h);
    }
    for (t, _) in &l.x_ticks {
        t.hash(&mut h);
    }
    h.finish()
}

fn encode(gfx: &GraphicsState, id: u32, pm: Pixmap, area: Rect) -> Option<Protocol> {
    let img = image::DynamicImage::ImageRgba8(raster::to_rgba_image(pm));
    encode_dynamic(gfx, id, img, Size::new(area.width, area.height))
}

fn encode_dynamic(gfx: &GraphicsState, id: u32, img: image::DynamicImage, size: Size) -> Option<Protocol> {
    let result = match gfx.mode {
        Graphics::Kitty => Kitty::new(img, size, id, gfx.tmux, false).map(Protocol::Kitty),
        Graphics::Sixel => Sixel::new(img, size, gfx.tmux).map(Protocol::Sixel),
        Graphics::Iterm => Iterm2::new(img, size, gfx.tmux).map(Protocol::ITerm2),
        Graphics::Unicode => return None,
    };
    result.map_err(|e| tracing::warn!("image encoding failed: {e}")).ok()
}
