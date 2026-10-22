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

/// When revealing a column centers it (niri's `center-focused-column`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CenterMode {
    /// Move only as far as needed.
    #[default]
    Never,
    /// Always center the revealed column.
    Always,
    /// Center only when the column is not already fully visible.
    OnOverflow,
}

/// Most preset widths a config can list.
pub const MAX_PRESET_WIDTHS: usize = 16;

/// Ascending column widths for `switch-preset-width`, stored inline so the
/// config stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresetWidths {
    values: [f64; MAX_PRESET_WIDTHS],
    len: usize,
}
impl PresetWidths {
    /// Sorted, deduplicated widths; `None` when empty or too many.
    pub fn new(widths: &[f64]) -> Option<Self> {
        let mut sorted = widths.to_vec();
        sorted.sort_by(f64::total_cmp);
        sorted.dedup();
        if sorted.is_empty() || sorted.len() > MAX_PRESET_WIDTHS {
            return None;
        }
        let mut values = [0.0; MAX_PRESET_WIDTHS];
        values[..sorted.len()].copy_from_slice(&sorted);
        Some(Self {
            values,
            len: sorted.len(),
        })
    }
    pub fn as_slice(&self) -> &[f64] {
        &self.values[..self.len]
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ScrollConfig {
    pub workspaces: WorkspaceMode,
    pub orientation: Orientation,
    pub tile_proportion: f64,
    pub center_focused: CenterMode,
    /// Center a workspace's only column regardless of `center_focused`.
    pub always_center_single: bool,
    /// Widths `switch-preset-width` cycles through.
    pub preset_widths: PresetWidths,
    pub gesture_distance: f64,
    /// Whether floating windows follow the camera or stay on the strip.
    pub float_anchor: FloatAnchor,
    /// With `float_anchor = strip`, move a floater the strip can no longer
    /// scroll to (its columns closed) back onto the screen.
    pub float_rescue: bool,
}

/// A column width or window height change: `50%`, `+10%`, `-10%`,
/// `800` or `+40` (logical pixels). Percentages are kept in hundredths so
/// the action stays `Eq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeChange {
    Percent { hundredths: i32, relative: bool },
    Pixels { pixels: i32, relative: bool },
}
impl SizeChange {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let relative = text.starts_with(['+', '-']);
        if let Some(number) = text.strip_suffix('%') {
            let value = number
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())?;
            if !relative && value <= 0.0 {
                return None;
            }
            let hundredths = (value * 100.0).round();
            (hundredths.abs() <= 100_000.0).then_some(Self::Percent {
                hundredths: hundredths as i32,
                relative,
            })
        } else {
            let pixels = text.parse::<i32>().ok()?;
            (relative || pixels > 0).then_some(Self::Pixels { pixels, relative })
        }
    }

    /// The new share of `viewport` (logical pixels) starting from
    /// `current`, bounded like every other column width.
    pub fn apply(self, current: f64, viewport: f64) -> f64 {
        let viewport = viewport.max(1.0);
        let next = match self {
            Self::Percent {
                hundredths,
                relative,
            } => {
                let share = f64::from(hundredths) / 10_000.0;
                if relative {
                    current + share
                } else {
                    share
                }
            }
            Self::Pixels { pixels, relative } => {
                let share = f64::from(pixels) / viewport;
                if relative {
                    current + share
                } else {
                    share
                }
            }
        };
        next.clamp(0.1, 1.0)
    }
}

/// What a floating window is fixed to while the strip scrolls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FloatAnchor {
    /// Floating windows stay put on screen; the strip slides under them.
    #[default]
    Screen,
    /// Floating windows keep their place on the strip and scroll with it.
    Strip,
}
impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            workspaces: WorkspaceMode::Separate,
            orientation: Orientation::Horizontal,
            tile_proportion: 0.5,
            center_focused: CenterMode::Never,
            always_center_single: false,
            preset_widths: PresetWidths::new(&[1.0 / 3.0, 0.5, 2.0 / 3.0])
                .expect("default presets are valid"),
            gesture_distance: 1200.0,
            float_anchor: FloatAnchor::Screen,
            float_rescue: true,
        }
    }
}

/// The preset after `current` (the first one wider by more than a pixel's
/// worth), wrapping to the narrowest.
pub fn next_preset(presets: &[f64], current: f64) -> Option<f64> {
    presets
        .iter()
        .copied()
        .find(|preset| *preset > current + 1e-3)
        .or_else(|| presets.first().copied())
}

/// `next_preset` backwards: the last preset narrower than `current`,
/// wrapping to the widest.
pub fn previous_preset(presets: &[f64], current: f64) -> Option<f64> {
    presets
        .iter()
        .rev()
        .copied()
        .find(|preset| *preset < current - 1e-3)
        .or_else(|| presets.last().copied())
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
    /// Width a maximized column returns to, keyed by its head.
    restore_widths: HashMap<WlSurface, f64>,
    /// Strips an explicit `center-column` left with end margins.
    margins: HashSet<(String, u32)>,
    /// Offset, in whole pixels, floating windows were last placed against.
    float_offsets: HashMap<(String, u32), i32>,
}
impl ScrollState {
    /// Records `offset` as the one floating windows now sit against and
    /// returns how far they must move, or `None` the first time a strip is
    /// seen. Whole pixels so repeated small pans cannot drift.
    pub fn float_shift(&mut self, output: &str, workspace: u32, offset: f64) -> Option<i32> {
        let offset = offset
            .round()
            .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
        let previous = self
            .float_offsets
            .insert((output.to_string(), workspace), offset)?;
        Some(previous.saturating_sub(offset)).filter(|shift| *shift != 0)
    }

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
    pub fn has_margins(&self, output: &str, workspace: u32) -> bool {
        self.margins.contains(&(output.to_owned(), workspace))
    }
    pub fn set_margins(&mut self, output: &str, workspace: u32, on: bool) {
        if on {
            self.margins.insert((output.to_owned(), workspace));
        } else {
            self.margins.remove(&(output.to_owned(), workspace));
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
    /// Toggles `head`'s column between full width and its previous width.
    pub fn toggle_maximized(&mut self, head: &WlSurface) {
        match self.restore_widths.remove(head) {
            Some(width) if self.proportion(head) >= 1.0 - 1e-6 => self.resize(head.clone(), width),
            _ => {
                self.restore_widths
                    .insert(head.clone(), self.proportion(head));
                self.resize(head.clone(), 1.0);
            }
        }
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
        self.restore_widths.remove(surface);
    }
    pub fn prune(&mut self, populated: &[(String, u32)]) {
        self.margins.retain(|(output, workspace)| {
            populated
                .iter()
                .any(|(name, number)| name == output && (*workspace == 0 || number == workspace))
        });
        self.offsets.retain(|(output, workspace), _| {
            populated
                .iter()
                .any(|(name, number)| name == output && (*workspace == 0 || number == workspace))
        });
    }
}

/// Where a dragged tile lands relative to the tile under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    /// A new column before the target's column.
    ColumnBefore,
    /// A new column after the target's column.
    ColumnAfter,
    /// Into the target's column, before the target.
    Before,
    /// Into the target's column, after the target.
    After,
    /// Trade places with the target.
    Swap,
}

/// The zone of `rect` under `point`: the outer quarters along the strip
/// make new columns, the outer quarters across it join the column, and the
/// middle swaps.
pub fn drop_zone(
    rect: Rectangle<i32, Logical>,
    point: smithay::utils::Point<f64, Logical>,
    orientation: Orientation,
) -> DropZone {
    let fraction =
        |value: f64, start: i32, size: i32| (value - f64::from(start)) / f64::from(size.max(1));
    let (along, across) = match orientation {
        Orientation::Horizontal => (
            fraction(point.x, rect.loc.x, rect.size.w),
            fraction(point.y, rect.loc.y, rect.size.h),
        ),
        Orientation::Vertical => (
            fraction(point.y, rect.loc.y, rect.size.h),
            fraction(point.x, rect.loc.x, rect.size.w),
        ),
    };
    if along < 0.25 {
        DropZone::ColumnBefore
    } else if along > 0.75 {
        DropZone::ColumnAfter
    } else if across < 0.25 {
        DropZone::Before
    } else if across > 0.75 {
        DropZone::After
    } else {
        DropZone::Swap
    }
}

/// The preview a drop zone shows: a bar on the target's landing edge, or
/// the whole target for a swap.
pub fn drop_hint(
    rect: Rectangle<i32, Logical>,
    zone: DropZone,
    orientation: Orientation,
    thickness: i32,
) -> Rectangle<i32, Logical> {
    let t = thickness.max(2);
    let horizontal = orientation == Orientation::Horizontal;
    let (x, y, w, h) = (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);
    let (x, y, w, h) = match (zone, horizontal) {
        (DropZone::Swap, _) => (x, y, w, h),
        (DropZone::ColumnBefore, true) | (DropZone::Before, false) => (x, y, t, h),
        (DropZone::ColumnAfter, true) | (DropZone::After, false) => (x + w - t, y, t, h),
        (DropZone::Before, true) | (DropZone::ColumnBefore, false) => (x, y, w, t),
        (DropZone::After, true) | (DropZone::ColumnAfter, false) => (x, y + h - t, w, t),
    };
    Rectangle::new((x, y).into(), (w.max(1), h.max(1)).into())
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

    #[test]
    fn size_change_forms() {
        let parse = |t| SizeChange::parse(t).unwrap().apply(0.5, 1000.0);
        assert!((parse("+10%") - 0.6).abs() < 1e-9);
        assert!((parse("-10%") - 0.4).abs() < 1e-9);
        assert!((parse("33.3%") - 0.333).abs() < 1e-9);
        assert!((parse("800") - 0.8).abs() < 1e-9);
        assert!((parse("+100") - 0.6).abs() < 1e-9);
        assert_eq!(parse("-90%"), 0.1);
        assert_eq!(parse("5000"), 1.0);
        for bad in ["", "abc", "0", "0%", "%", "nan%", "50px"] {
            assert_eq!(SizeChange::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn float_shift_tracks_whole_pixels() {
        let mut state = ScrollState::default();
        assert_eq!(state.float_shift("out", 1, 0.0), None);
        assert_eq!(state.float_shift("out", 1, 0.4), None);
        assert_eq!(state.float_shift("out", 1, 300.6), Some(-301));
        assert_eq!(state.float_shift("out", 1, 100.0), Some(201));
        assert_eq!(state.float_shift("out", 2, 500.0), None);
    }

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
    fn drop_zones_split_tiles_into_edges_and_middle() {
        use smithay::utils::Point;
        let rect = Rectangle::new((100, 0).into(), (400, 800).into());
        let at = |x: f64, y: f64| drop_zone(rect, Point::from((x, y)), Orientation::Horizontal);
        assert_eq!(at(150.0, 400.0), DropZone::ColumnBefore);
        assert_eq!(at(450.0, 400.0), DropZone::ColumnAfter);
        assert_eq!(at(300.0, 100.0), DropZone::Before);
        assert_eq!(at(300.0, 700.0), DropZone::After);
        assert_eq!(at(300.0, 400.0), DropZone::Swap);
        // Vertical strips swap the axes.
        assert_eq!(
            drop_zone(rect, Point::from((300.0, 100.0)), Orientation::Vertical),
            DropZone::ColumnBefore
        );
        assert_eq!(
            drop_hint(rect, DropZone::ColumnAfter, Orientation::Horizontal, 6),
            Rectangle::new((494, 0).into(), (6, 800).into())
        );
        assert_eq!(
            drop_hint(rect, DropZone::Before, Orientation::Horizontal, 6),
            Rectangle::new((100, 0).into(), (400, 6).into())
        );
        assert_eq!(
            drop_hint(rect, DropZone::Swap, Orientation::Horizontal, 6),
            rect
        );
    }

    #[test]
    fn presets_cycle_upward_and_wrap() {
        let presets = PresetWidths::new(&[0.5, 1.0 / 3.0, 2.0 / 3.0, 0.5]).unwrap();
        assert_eq!(presets.as_slice().len(), 3);
        assert_eq!(next_preset(presets.as_slice(), 0.4), Some(0.5));
        assert_eq!(next_preset(presets.as_slice(), 0.5), Some(2.0 / 3.0));
        assert_eq!(next_preset(presets.as_slice(), 0.9), Some(1.0 / 3.0));
        assert_eq!(previous_preset(presets.as_slice(), 0.6), Some(0.5));
        assert_eq!(previous_preset(presets.as_slice(), 0.5), Some(1.0 / 3.0));
        assert_eq!(previous_preset(presets.as_slice(), 0.2), Some(2.0 / 3.0));
        assert!(PresetWidths::new(&[]).is_none());
        // Duplicates collapse before the size limit applies.
        assert!(PresetWidths::new(&[0.5; 17]).is_some());
        let too_many: Vec<f64> = (0..17).map(|i| 0.1 + f64::from(i) * 0.05).collect();
        assert!(PresetWidths::new(&too_many).is_none());
    }

    #[test]
    fn maximize_toggles_back_to_the_previous_width() {
        let mut fixture = crate::ocean::test_protocol::ProtocolFixture::new();
        let head = fixture.window().toplevel().unwrap().wl_surface().clone();
        let mut state = ScrollState::default();
        state.resize(head.clone(), 0.4);
        state.toggle_maximized(&head);
        assert_eq!(state.proportion(&head), 1.0);
        state.toggle_maximized(&head);
        assert_eq!(state.proportion(&head), 0.4);
        // A manual resize after maximizing starts a fresh maximize.
        state.toggle_maximized(&head);
        state.resize(head.clone(), 0.7);
        state.toggle_maximized(&head);
        assert_eq!(state.proportion(&head), 1.0);
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
