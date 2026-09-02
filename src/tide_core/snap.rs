//! Classic floating-window edge and corner snapping.
//!
//! Detection works in global logical coordinates. Output scale and transform
//! are already reflected in Smithay's logical output geometry, so the same
//! math serves nested, HiDPI, rotated, and multi-output sessions.

use smithay::utils::{Logical, Point, Rectangle};

use crate::config::SnapConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapPreset {
    Halves,
    Quarters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapZone {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// A drop dead-center along the top edge, clear of the corner zones.
    /// Unlike every other zone this isn't a rect resize -- it enters real
    /// compositor fullscreen (see `Smallvil::enter_fullscreen`).
    Fullscreen,
}

impl SnapZone {
    pub const ALL: [Self; 9] = [
        Self::Left,
        Self::Right,
        Self::Top,
        Self::Bottom,
        Self::TopLeft,
        Self::TopRight,
        Self::BottomLeft,
        Self::BottomRight,
        Self::Fullscreen,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "top" | "up" => Some(Self::Top),
            "bottom" | "down" => Some(Self::Bottom),
            "top-left" => Some(Self::TopLeft),
            "top-right" => Some(Self::TopRight),
            "bottom-left" => Some(Self::BottomLeft),
            "bottom-right" => Some(Self::BottomRight),
            "fullscreen" | "full" | "maximize" => Some(Self::Fullscreen),
            _ => None,
        }
    }

    fn is_corner(self) -> bool {
        matches!(
            self,
            Self::TopLeft | Self::TopRight | Self::BottomLeft | Self::BottomRight
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapTarget {
    pub output: String,
    pub workspace: u32,
    pub zone: SnapZone,
    pub rect: Rectangle<i32, Logical>,
}

pub fn zone_at(
    point: Point<f64, Logical>,
    output: Rectangle<i32, Logical>,
    config: &SnapConfig,
) -> Option<SnapZone> {
    if !config.enabled || config.distance == 0 || !output.to_f64().contains(point) {
        return None;
    }

    let distance = f64::from(config.distance);
    let left_distance = point.x - f64::from(output.loc.x);
    let right_distance = f64::from(output.loc.x) + f64::from(output.size.w) - point.x;
    let top_distance = point.y - f64::from(output.loc.y);
    let bottom_distance = f64::from(output.loc.y) + f64::from(output.size.h) - point.y;
    let horizontal = [
        (SnapZone::Left, left_distance),
        (SnapZone::Right, right_distance),
    ]
    .into_iter()
    .filter(|(_, d)| *d <= distance)
    .min_by(|(_, a), (_, b)| a.total_cmp(b));
    let vertical = [
        (SnapZone::Top, top_distance),
        (SnapZone::Bottom, bottom_distance),
    ]
    .into_iter()
    .filter(|(_, d)| *d <= distance)
    .min_by(|(_, a), (_, b)| a.total_cmp(b));

    if let (Some((horizontal, _)), Some((vertical, _))) = (horizontal, vertical) {
        let corner = match (horizontal, vertical) {
            (SnapZone::Left, SnapZone::Top) => SnapZone::TopLeft,
            (SnapZone::Right, SnapZone::Top) => SnapZone::TopRight,
            (SnapZone::Left, SnapZone::Bottom) => SnapZone::BottomLeft,
            (SnapZone::Right, SnapZone::Bottom) => SnapZone::BottomRight,
            _ => unreachable!(),
        };
        if zone_enabled(corner, config) {
            return Some(corner);
        }
    }

    // Dead-center along the top edge, clear of the corner zones: the same
    // "drag to the top of the screen" affordance other WMs use for
    // maximize, scoped to the middle third so it doesn't fight the ordinary
    // top-edge half-snap on either side of it.
    if horizontal.is_none() && matches!(vertical, Some((SnapZone::Top, _))) {
        let center_x = f64::from(output.loc.x) + f64::from(output.size.w) / 2.0;
        let half_band = f64::from(output.size.w) / 6.0;
        if (point.x - center_x).abs() <= half_band && zone_enabled(SnapZone::Fullscreen, config) {
            return Some(SnapZone::Fullscreen);
        }
    }

    horizontal
        .into_iter()
        .chain(vertical)
        .filter(|(zone, _)| zone_enabled(*zone, config))
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(zone, _)| zone)
}

pub fn target_rect(
    area: Rectangle<i32, Logical>,
    zone: SnapZone,
    gap: i32,
) -> Rectangle<i32, Logical> {
    // Fullscreen is edge-to-edge by definition and never actually resizes
    // the window through this rect (see `Smallvil::enter_fullscreen`) --
    // callers pass the raw output geometry here, used only to size the
    // drop preview.
    if zone == SnapZone::Fullscreen {
        return area;
    }
    let left_width = area.size.w / 2;
    let right_width = area.size.w - left_width;
    let top_height = area.size.h / 2;
    let bottom_height = area.size.h - top_height;
    let x_mid = area.loc.x.saturating_add(left_width);
    let y_mid = area.loc.y.saturating_add(top_height);
    let raw = match zone {
        SnapZone::Left => Rectangle::new(area.loc, (left_width, area.size.h).into()),
        SnapZone::Right => Rectangle::new(
            (x_mid, area.loc.y).into(),
            (right_width, area.size.h).into(),
        ),
        SnapZone::Top => Rectangle::new(area.loc, (area.size.w, top_height).into()),
        SnapZone::Bottom => Rectangle::new(
            (area.loc.x, y_mid).into(),
            (area.size.w, bottom_height).into(),
        ),
        SnapZone::TopLeft => Rectangle::new(area.loc, (left_width, top_height).into()),
        SnapZone::TopRight => {
            Rectangle::new((x_mid, area.loc.y).into(), (right_width, top_height).into())
        }
        SnapZone::BottomLeft => Rectangle::new(
            (area.loc.x, y_mid).into(),
            (left_width, bottom_height).into(),
        ),
        SnapZone::BottomRight => {
            Rectangle::new((x_mid, y_mid).into(), (right_width, bottom_height).into())
        }
        SnapZone::Fullscreen => unreachable!("handled by the early return above"),
    };
    crate::layout::inset(raw, gap.max(0))
}

pub fn zone_enabled(zone: SnapZone, config: &SnapConfig) -> bool {
    if let Some(zones) = &config.zones {
        return zones.contains(&zone);
    }
    if zone == SnapZone::Fullscreen {
        return config.fullscreen;
    }
    match config.preset {
        SnapPreset::Halves => !zone.is_corner(),
        SnapPreset::Quarters => true,
    }
}

/// Converts a global logical preview origin into output-local physical
/// coordinates. Keeping this pure makes fractional-scale behavior testable
/// without constructing renderer state.
pub fn preview_physical_location(
    preview_location: Point<i32, Logical>,
    output_location: Point<i32, Logical>,
    scale: f64,
) -> Point<i32, smithay::utils::Physical> {
    (preview_location - output_location).to_physical_precise_round(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SnapConfig {
        SnapConfig::default()
    }

    #[test]
    fn corners_win_over_edges_and_halves_fall_back_to_nearest_edge() {
        let output = Rectangle::new((100, -50).into(), (1200, 800).into());
        assert_eq!(
            zone_at((104.0, -46.0).into(), output, &config()),
            Some(SnapZone::TopLeft)
        );

        let halves = SnapConfig {
            preset: SnapPreset::Halves,
            ..config()
        };
        assert_eq!(
            zone_at((104.0, -42.0).into(), output, &halves),
            Some(SnapZone::Left)
        );
    }

    #[test]
    fn top_center_band_fullscreens_and_the_flanks_stay_a_half_snap() {
        let output = Rectangle::new((0, 0).into(), (1200, 800).into());
        assert_eq!(
            zone_at((600.0, 2.0).into(), output, &config()),
            Some(SnapZone::Fullscreen)
        );
        assert_eq!(
            zone_at((300.0, 2.0).into(), output, &config()),
            Some(SnapZone::Top)
        );

        let no_fullscreen = SnapConfig {
            fullscreen: false,
            ..config()
        };
        assert_eq!(
            zone_at((600.0, 2.0).into(), output, &no_fullscreen),
            Some(SnapZone::Top)
        );
    }

    #[test]
    fn fullscreen_target_rect_is_the_untouched_output_geometry() {
        let output = Rectangle::new((50, -20).into(), (1920, 1080).into());
        assert_eq!(target_rect(output, SnapZone::Fullscreen, 8), output);
    }

    #[test]
    fn explicit_zone_list_replaces_the_preset() {
        let config = SnapConfig {
            zones: Some(vec![SnapZone::BottomRight]),
            ..config()
        };
        let output = Rectangle::new((0, 0).into(), (800, 600).into());
        assert_eq!(zone_at((1.0, 1.0).into(), output, &config), None);
        assert_eq!(
            zone_at((799.0, 599.0).into(), output, &config),
            Some(SnapZone::BottomRight)
        );
    }

    #[test]
    fn target_rect_preserves_odd_pixels_and_global_origin() {
        let area = Rectangle::new((-101, 37).into(), (1001, 701).into());
        assert_eq!(
            target_rect(area, SnapZone::TopLeft, 0),
            Rectangle::new((-101, 37).into(), (500, 350).into())
        );
        assert_eq!(
            target_rect(area, SnapZone::BottomRight, 0),
            Rectangle::new((399, 387).into(), (501, 351).into())
        );
    }

    #[test]
    fn logical_geometry_handles_rotated_outputs_without_special_cases() {
        let rotated_logical = Rectangle::new((1920, 0).into(), (720, 1280).into());
        assert_eq!(
            zone_at((2639.0, 1.0).into(), rotated_logical, &config()),
            Some(SnapZone::TopRight)
        );
        assert_eq!(
            target_rect(rotated_logical, SnapZone::Right, 8),
            Rectangle::new((2288, 8).into(), (344, 1264).into())
        );
    }

    #[test]
    fn preview_origin_converts_from_global_logical_to_fractional_physical() {
        assert_eq!(
            preview_physical_location((2050, 75).into(), (1920, -25).into(), 1.5),
            (195, 150).into()
        );
    }
}
