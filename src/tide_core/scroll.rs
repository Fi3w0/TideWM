//! Ordered scrolling geometry. Output/workspace membership stays in Layouts;
//! only viewport offsets and requested tile widths belong to this policy.
use std::collections::HashMap;

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
    pub fn replace_slot(&mut self, old: &WlSurface, new: WlSurface) {
        if let Some(proportion) = self.proportions.get(old).copied() {
            self.proportions.insert(new, proportion);
        } else {
            self.proportions.remove(&new);
        }
    }
    pub fn forget(&mut self, surface: &WlSurface) {
        self.proportions.remove(surface);
        self.fullscreen_extents.remove(surface);
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
    let gap = gap
        .max(0)
        .min(area.size.w.min(area.size.h).saturating_sub(1) / 2);
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
            let size = (
                (extent.round() as i32).max(1),
                (area.size.h - 2 * gap).max(1),
            );
            Rectangle::new(
                (bounded(x + start - offset, size.0), bounded(y, size.1)).into(),
                size.into(),
            )
        }
        Orientation::Vertical => {
            let size = (
                (area.size.w - 2 * gap).max(1),
                (extent.round() as i32).max(1),
            );
            Rectangle::new(
                (bounded(x, size.0), bounded(y + start - offset, size.1)).into(),
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
}
