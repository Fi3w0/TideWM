//! Ordered scrolling geometry. Output/workspace membership stays in Layouts;
//! only viewport offsets, column widths and column stacking belong to this
//! policy.
//!
//! Columns are derived from the workspace's window order: a window marked
//! `stacked` joins the column of the window before it, so a column is a
//! contiguous run whose first window (its head) carries the column width.
//! Classic ignores this metadata, which keeps engine switches lossless.
use std::collections::{HashMap, HashSet};

use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Rectangle},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceMode {
    #[default]
    Separate,
    Sections,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Orientation {
    #[default]
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy)]
pub struct ScrollConfig {
    pub workspaces: WorkspaceMode,
    pub orientation: Orientation,
    pub tile_proportion: f64,
    pub center_focused: bool,
    pub gesture_distance: f64,
}
impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            workspaces: WorkspaceMode::Separate,
            orientation: Orientation::Horizontal,
            tile_proportion: 0.5,
            center_focused: false,
            gesture_distance: 1200.0,
        }
    }
}

#[derive(Default)]
pub struct ScrollState {
    pub config: Option<ScrollConfig>,
    /// Per-retile snapshot of full-output extents; FullscreenEntry remains authoritative.
    pub fullscreen_extents: HashMap<WlSurface, f64>,
    offsets: HashMap<(String, u32), f64>,
    proportions: HashMap<WlSurface, f64>,
    /// Windows that continue the column of the window before them.
    stacked: HashSet<WlSurface>,
    /// Relative height (or width, for vertical strips) inside a column.
    weights: HashMap<WlSurface, f64>,
}
impl ScrollState {
    pub fn offset(&self, output: &str, workspace: u32) -> f64 {
        self.offsets
            .get(&(output.to_owned(), workspace))
            .copied()
            .unwrap_or(0.0)
    }
    pub fn set_offset(&mut self, output: &str, workspace: u32, value: f64) {
        if value.is_finite() {
            let key = (output.to_owned(), workspace);
            if value == 0.0 {
                self.offsets.remove(&key);
            } else {
                self.offsets.insert(key, value);
            }
        }
    }
    pub fn proportion(&self, surface: &WlSurface) -> f64 {
        self.proportions
            .get(surface)
            .copied()
            .unwrap_or_else(|| self.config.unwrap_or_default().tile_proportion)
    }
    pub fn resize(&mut self, surface: WlSurface, proportion: f64) {
        if proportion.is_finite() {
            self.proportions.insert(surface, proportion.clamp(0.1, 1.0));
        }
    }
    pub fn has_proportion(&self, surface: &WlSurface) -> bool {
        self.proportions.contains_key(surface)
    }
    pub fn clear_proportion(&mut self, surface: &WlSurface) {
        self.proportions.remove(surface);
    }
    pub fn is_stacked(&self, surface: &WlSurface) -> bool {
        self.stacked.contains(surface)
    }
    pub fn set_stacked(&mut self, surface: &WlSurface, stacked: bool) {
        if stacked {
            self.stacked.insert(surface.clone());
        } else {
            self.stacked.remove(surface);
        }
    }
    pub fn weight(&self, surface: &WlSurface) -> f64 {
        self.weights.get(surface).copied().unwrap_or(1.0)
    }
    pub fn set_weight(&mut self, surface: &WlSurface, weight: f64) {
        if weight.is_finite() {
            let weight = weight.clamp(0.1, 10.0);
            if (weight - 1.0).abs() < f64::EPSILON {
                self.weights.remove(surface);
            } else {
                self.weights.insert(surface.clone(), weight);
            }
        }
    }
    pub fn replace_slot(&mut self, old: &WlSurface, new: WlSurface) {
        if let Some(proportion) = self.proportions.get(old).copied() {
            self.proportions.insert(new.clone(), proportion);
        } else {
            self.proportions.remove(&new);
        }
        let stacked = self.stacked.contains(old);
        self.set_stacked(&new, stacked);
        let weight = self.weight(old);
        self.weights.remove(&new);
        self.set_weight(&new, weight);
    }
    /// Two windows trade places: each takes the other's column slot
    /// (stacking, width, row share).
    pub fn swap_slots(&mut self, a: &WlSurface, b: &WlSurface) {
        let (stacked_a, stacked_b) = (self.is_stacked(a), self.is_stacked(b));
        self.set_stacked(a, stacked_b);
        self.set_stacked(b, stacked_a);
        let (width_a, width_b) = (self.proportions.remove(a), self.proportions.remove(b));
        if let Some(width) = width_b {
            self.proportions.insert(a.clone(), width);
        }
        if let Some(width) = width_a {
            self.proportions.insert(b.clone(), width);
        }
        let (weight_a, weight_b) = (self.weight(a), self.weight(b));
        self.weights.remove(a);
        self.weights.remove(b);
        self.set_weight(a, weight_b);
        self.set_weight(b, weight_a);
    }
    pub fn clear_columns(&mut self) {
        self.stacked.clear();
        self.weights.clear();
    }
    pub fn forget(&mut self, surface: &WlSurface) {
        self.proportions.remove(surface);
        self.fullscreen_extents.remove(surface);
        self.stacked.remove(surface);
        self.weights.remove(surface);
    }
    pub fn prune(&mut self, populated: &[(String, u32)]) {
        self.offsets.retain(|(output, workspace), _| {
            populated
                .iter()
                .any(|(name, number)| name == output && (*workspace == 0 || number == workspace))
        });
    }
}

/// Find the smallest viewport movement that reveals a tile, or center it.
/// Oversized tiles align their leading edge instead of oscillating sides.
pub fn reveal(offset: f64, start: f64, extent: f64, viewport: f64, center: bool) -> f64 {
    if viewport <= 0.0
        || ![offset, start, extent, viewport]
            .iter()
            .all(|v| v.is_finite())
    {
        return offset;
    }
    if extent >= viewport {
        return start.max(0.0);
    }
    if center {
        return start - (viewport - extent) / 2.0;
    }
    if start < offset {
        start.max(0.0)
    } else if start + extent > offset + viewport {
        (start + extent - viewport).max(0.0)
    } else {
        offset
    }
}

/// Convert strip coordinates only at the final logical-pixel boundary.
/// Saturation keeps distant, non-visible content inside Smithay's i32 domain.
pub fn tile_rect(
    area: Rectangle<i32, Logical>,
    start: f64,
    extent: f64,
    offset: f64,
    gap: i32,
    orientation: Orientation,
) -> Rectangle<i32, Logical> {
    column_tile_rect(area, start, extent, offset, gap, orientation, (0, &[1.0]))
}

/// The rect of row `row` in a column whose rows share the cross axis by
/// `weights` (`rows = (row, weights)`), with `gap` between rows. A one-row
/// column fills the cross axis.
pub fn column_tile_rect(
    area: Rectangle<i32, Logical>,
    start: f64,
    extent: f64,
    offset: f64,
    gap: i32,
    orientation: Orientation,
    (row, weights): (usize, &[f64]),
) -> Rectangle<i32, Logical> {
    let gap = gap
        .max(0)
        .min(area.size.w.min(area.size.h).saturating_sub(1) / 2);
    let full = match orientation {
        Orientation::Horizontal => area.size.h,
        Orientation::Vertical => area.size.w,
    };
    let rows = weights.len().max(1);
    let row = row.min(rows - 1);
    let available = (f64::from(full - 2 * gap) - f64::from(gap) * (rows - 1) as f64).max(1.0);
    let total: f64 = weights
        .iter()
        .map(|w| w.max(0.0))
        .sum::<f64>()
        .max(f64::EPSILON);
    let edge = |index: usize| {
        let before: f64 = weights[..index].iter().map(|w| w.max(0.0)).sum();
        (available * before / total).round() + f64::from(gap) * index as f64
    };
    let cross_start = edge(row);
    let cross_end = if row + 1 == rows {
        available + f64::from(gap) * (rows - 1) as f64
    } else {
        edge(row + 1) - f64::from(gap)
    };
    let cross = ((cross_end - cross_start).round() as i32).max(1);
    let x = f64::from(area.loc.x) + f64::from(gap);
    let y = f64::from(area.loc.y) + f64::from(gap);
    // Leave space for the size when clamping the position, so rect edges do not overflow.
    let bounded = |value: f64, size: i32| {
        value
            .round()
            .clamp(f64::from(i32::MIN), f64::from(i32::MAX - size.max(1))) as i32
    };
    match orientation {
        Orientation::Horizontal => {
            let size = ((extent.round() as i32).max(1), cross);
            Rectangle::new(
                (
                    bounded(x + start - offset, size.0),
                    bounded(y + cross_start, size.1),
                )
                    .into(),
                size.into(),
            )
        }
        Orientation::Vertical => {
            let size = (cross, (extent.round() as i32).max(1));
            Rectangle::new(
                (
                    bounded(x + cross_start, size.0),
                    bounded(y + start - offset, size.1),
                )
                    .into(),
                size.into(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reveal_preserves_visible_windows_and_handles_oversized_content() {
        assert_eq!(reveal(200., 250., 300., 800., false), 200.);
        assert_eq!(reveal(200., 100., 300., 800., false), 100.);
        assert_eq!(reveal(0., 700., 400., 800., false), 300.);
        assert_eq!(reveal(200., 100., 900., 800., false), 100.);
        assert_eq!(reveal(0., 900., 400., 800., true), 700.);
        assert_eq!(reveal(200., f64::NAN, 300., 800., false), 200.);
    }
    #[test]
    fn vertical_geometry_uses_live_dimensions_and_negative_output_origin() {
        let area = Rectangle::new((-900, 45).into(), (753, 1201).into());
        let rect = tile_rect(area, 800., 500., 700., 7, Orientation::Vertical);
        assert_eq!(rect, Rectangle::new((-893, 152).into(), (739, 500).into()));
        let tiny = tile_rect(
            Rectangle::new((0, 0).into(), (1, 1).into()),
            0.,
            1.,
            0.,
            100,
            Orientation::Horizontal,
        );
        assert_eq!(tiny.size, (1, 1).into());
        let distant = tile_rect(area, 1e30, 500., 0., 7, Orientation::Horizontal);
        assert!(distant.loc.x.checked_add(distant.size.w).is_some());
    }
    #[test]
    fn column_rows_split_the_cross_axis_by_weight_with_gaps() {
        let area = Rectangle::new((10, 20).into(), (1000, 616).into());
        let weights = [1.0, 2.0, 1.0];
        let rows: Vec<_> = (0..3)
            .map(|row| {
                column_tile_rect(
                    area,
                    0.,
                    400.,
                    0.,
                    8,
                    Orientation::Horizontal,
                    (row, &weights),
                )
            })
            .collect();
        // 616 - 2*8 outer - 2*8 between rows = 584 split 1:2:1.
        assert_eq!(rows[0], Rectangle::new((18, 28).into(), (400, 146).into()));
        assert_eq!(rows[1], Rectangle::new((18, 182).into(), (400, 292).into()));
        assert_eq!(rows[2], Rectangle::new((18, 482).into(), (400, 146).into()));
        assert_eq!(rows[2].loc.y + rows[2].size.h, area.loc.y + area.size.h - 8);
        let sole = column_tile_rect(area, 0., 400., 0., 8, Orientation::Horizontal, (0, &[3.0]));
        assert_eq!(
            sole,
            tile_rect(area, 0., 400., 0., 8, Orientation::Horizontal)
        );
        let vertical = column_tile_rect(
            area,
            0.,
            300.,
            0.,
            8,
            Orientation::Vertical,
            (1, &[1.0, 1.0]),
        );
        assert_eq!(
            vertical.loc.x + vertical.size.w,
            area.loc.x + area.size.w - 8
        );
    }
}
