//! Keyboard interaction with charts: crosshair movement, visual range
//! selection and the resulting zoom requests.

use super::keymap::Act;
use crate::timerange::Millis;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChartCursor {
    pub crosshair: Option<Millis>,
    /// Visual-mode anchor (`v`).
    pub anchor: Option<Millis>,
    pub selection: Option<(Millis, Millis)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChartOutcome {
    /// Cursor state changed; redraw.
    Moved,
    /// Zoom into this absolute range.
    Zoom(Millis, Millis),
    /// Zoom around a point by a factor.
    ZoomAround(Option<Millis>, f64),
    /// Leave the chart (e.g. `h` with no crosshair).
    Exit,
    Ignored,
}

impl ChartCursor {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn is_active(&self) -> bool {
        self.crosshair.is_some() || self.selection.is_some()
    }

    fn update_selection(&mut self) {
        if let (Some(a), Some(c)) = (self.anchor, self.crosshair) {
            self.selection = Some((a.min(c), a.max(c)));
        }
    }

    /// `timestamps` are the data points of the chart (sorted); `range` the visible range.
    pub fn handle(&mut self, acts: &[Act], timestamps: &[Millis], range: (Millis, Millis)) -> ChartOutcome {
        let (from, to) = range;
        let span = (to - from).max(1);
        let snap_step = |t: Millis, dir: i64| -> Millis {
            if timestamps.is_empty() {
                return (t + dir * span / 100).clamp(from, to);
            }
            let i = timestamps.partition_point(|x| *x < t);
            let idx = if dir > 0 { if i < timestamps.len() && timestamps[i] == t { i + 1 } else { i } } else { i.saturating_sub(1) };
            timestamps[idx.min(timestamps.len() - 1)].clamp(from, to)
        };
        let default_pos = || timestamps.last().copied().unwrap_or(to).clamp(from, to);
        for a in acts {
            match a {
                Act::Right | Act::Left => {
                    let dir = if *a == Act::Right { 1 } else { -1 };
                    match self.crosshair {
                        None if *a == Act::Left => return ChartOutcome::Exit,
                        None => self.crosshair = Some(default_pos()),
                        Some(t) => self.crosshair = Some(snap_step(t, dir)),
                    }
                }
                Act::WordForward | Act::WordBack => {
                    let dir = if *a == Act::WordForward { 1 } else { -1 };
                    let t = self.crosshair.unwrap_or_else(default_pos);
                    self.crosshair = Some((t + dir * span / 10).clamp(from, to));
                }
                Act::LineStart => self.crosshair = Some(timestamps.first().copied().unwrap_or(from).clamp(from, to)),
                Act::LineEnd => self.crosshair = Some(default_pos()),
                Act::Visual => {
                    if self.anchor.is_some() {
                        self.anchor = None;
                        self.selection = None;
                    } else {
                        let t = self.crosshair.unwrap_or_else(default_pos);
                        self.crosshair = Some(t);
                        self.anchor = Some(t);
                        self.selection = Some((t, t));
                    }
                }
                Act::Enter => match self.selection {
                    Some((a, b)) if b > a => {
                        self.clear();
                        return ChartOutcome::Zoom(a, b);
                    }
                    _ => return ChartOutcome::Ignored,
                },
                Act::Back => {
                    if self.selection.is_some() {
                        self.anchor = None;
                        self.selection = None;
                    } else if self.crosshair.is_some() {
                        self.crosshair = None;
                    } else {
                        return ChartOutcome::Ignored;
                    }
                }
                Act::ZoomIn => return ChartOutcome::ZoomAround(self.crosshair, 0.5),
                Act::ZoomOut => return ChartOutcome::ZoomAround(self.crosshair, 2.0),
                _ => continue,
            }
            self.update_selection();
            return ChartOutcome::Moved;
        }
        ChartOutcome::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crosshair_and_selection() {
        let ts: Vec<Millis> = (0..=10).map(|i| i * 100).collect();
        let mut c = ChartCursor::default();
        assert_eq!(c.handle(&[Act::Left], &ts, (0, 1000)), ChartOutcome::Exit);
        assert_eq!(c.handle(&[Act::Right], &ts, (0, 1000)), ChartOutcome::Moved);
        assert_eq!(c.crosshair, Some(1000));
        c.handle(&[Act::Left], &ts, (0, 1000));
        assert_eq!(c.crosshair, Some(900));
        c.handle(&[Act::Visual], &ts, (0, 1000));
        c.handle(&[Act::Left], &ts, (0, 1000));
        c.handle(&[Act::Left], &ts, (0, 1000));
        assert_eq!(c.selection, Some((700, 900)));
        assert_eq!(c.handle(&[Act::Enter], &ts, (0, 1000)), ChartOutcome::Zoom(700, 900));
        assert!(!c.is_active());
        c.handle(&[Act::LineStart], &ts, (0, 1000));
        assert_eq!(c.crosshair, Some(0));
        c.handle(&[Act::WordForward], &ts, (0, 1000));
        assert_eq!(c.crosshair, Some(100));
        assert_eq!(c.handle(&[Act::Back], &ts, (0, 1000)), ChartOutcome::Moved);
        assert_eq!(c.handle(&[Act::Back], &ts, (0, 1000)), ChartOutcome::Ignored);
    }
}
