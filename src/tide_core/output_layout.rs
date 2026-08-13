//! Bounds for the shared logical desktop coordinate space.
//!
//! Output positions come from user config and `wlr-output-management` as
//! full-range `i32` coordinates. Smithay also represents logical output
//! rectangles with `i32`. Keep the whole live layout inside that real type
//! domain instead of inventing a monitor resolution or desktop-size limit.

use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DesktopLayoutError {
    NonPositiveSize,
    EdgeOverflow,
    SpanOverflow,
}

/// Returns the logical size Smithay will derive for a mode, transform, and
/// fractional scale, or `None` when it cannot be represented by `Space`.
pub(crate) fn logical_output_size(
    mode_size: Size<i32, Physical>,
    transform: Transform,
    scale: f64,
) -> Option<Size<i32, Logical>> {
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }

    let logical = transform
        .transform_size(mode_size)
        .to_f64()
        .to_logical(scale);
    let max = f64::from(i32::MAX);
    if !logical.w.is_finite()
        || !logical.h.is_finite()
        || logical.w <= 0.0
        || logical.h <= 0.0
        || logical.w.ceil() > max
        || logical.h.ceil() > max
    {
        return None;
    }

    Some(logical.to_i32_ceil())
}

/// Checks that every output edge and the full desktop span are representable
/// by Smithay's `i32` logical geometry. Overlap is deliberately valid.
pub(crate) fn validate_desktop_layout(
    rects: impl IntoIterator<Item = Rectangle<i32, Logical>>,
) -> Result<(), DesktopLayoutError> {
    let mut bounds: Option<(i64, i64, i64, i64)> = None;

    for rect in rects {
        if rect.size.w <= 0 || rect.size.h <= 0 {
            return Err(DesktopLayoutError::NonPositiveSize);
        }

        let left = i64::from(rect.loc.x);
        let top = i64::from(rect.loc.y);
        let right = left + i64::from(rect.size.w);
        let bottom = top + i64::from(rect.size.h);
        if right > i64::from(i32::MAX) || bottom > i64::from(i32::MAX) {
            return Err(DesktopLayoutError::EdgeOverflow);
        }

        bounds = Some(match bounds {
            None => (left, top, right, bottom),
            Some((min_x, min_y, max_x, max_y)) => (
                min_x.min(left),
                min_y.min(top),
                max_x.max(right),
                max_y.max(bottom),
            ),
        });
    }

    if let Some((min_x, min_y, max_x, max_y)) = bounds {
        let max_span = i64::from(i32::MAX);
        if max_x - min_x > max_span || max_y - min_y > max_span {
            return Err(DesktopLayoutError::SpanOverflow);
        }
    }

    Ok(())
}

/// Computes an exact logical translation between two output origins. A
/// layout can be valid before and after a change while a single output jumps
/// farther than `i32` can describe; reject that transition instead of
/// silently saturating the windows carried with it.
pub(crate) fn checked_output_delta(
    from: Point<i32, Logical>,
    to: Point<i32, Logical>,
) -> Option<Point<i32, Logical>> {
    Some(
        (
            i32::try_from(i64::from(to.x) - i64::from(from.x)).ok()?,
            i32::try_from(i64::from(to.y) - i64::from(from.y)).ok()?,
        )
            .into(),
    )
}

/// Resolves a requested position, falling back to automatic placement when
/// it would violate the coordinate-domain invariant. Automatic placement
/// first tries the current right edge. If the desktop has reached that edge,
/// it aligns the new output with the live minimum corner, allowing overlap
/// rather than dropping real hardware merely because no adjacent slot fits.
pub(crate) fn resolve_output_position(
    existing: impl IntoIterator<Item = Rectangle<i32, Logical>>,
    requested: Option<Point<i32, Logical>>,
    size: Size<i32, Logical>,
) -> Option<(Point<i32, Logical>, bool)> {
    let existing: Vec<_> = existing.into_iter().collect();
    if validate_desktop_layout(existing.iter().copied()).is_err()
        || validate_desktop_layout([Rectangle::new(Point::default(), size)]).is_err()
    {
        return None;
    }

    if let Some(position) = requested {
        let candidate = Rectangle::new(position, size);
        if validate_desktop_layout(existing.iter().copied().chain([candidate])).is_ok() {
            return Some((position, true));
        }
    }

    let right = existing
        .iter()
        .map(|rect| i64::from(rect.loc.x) + i64::from(rect.size.w))
        .max()
        .unwrap_or(0);
    if let Ok(right) = i32::try_from(right) {
        let candidate = Rectangle::new((right, 0).into(), size);
        if validate_desktop_layout(existing.iter().copied().chain([candidate])).is_ok() {
            return Some((candidate.loc, false));
        }
    }

    let min_x = existing.iter().map(|rect| rect.loc.x).min().unwrap_or(0);
    let min_y = existing.iter().map(|rect| rect.loc.y).min().unwrap_or(0);
    let position = Point::from((min_x.min(i32::MAX - size.w), min_y.min(i32::MAX - size.h)));
    let candidate = Rectangle::new(position, size);
    validate_desktop_layout(existing.into_iter().chain([candidate]))
        .ok()
        .map(|()| (position, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_size_uses_live_mode_transform_and_scale() {
        let size = logical_output_size((113, 71).into(), Transform::_90, 1.25).unwrap();
        assert_eq!(size, (57, 91).into());
    }

    #[test]
    fn desktop_bounds_follow_the_i32_geometry_domain() {
        let valid = [
            Rectangle::new((-10_000, 73).into(), (347, 199).into()),
            Rectangle::new((-9_653, -211).into(), (503, 281).into()),
        ];
        assert_eq!(validate_desktop_layout(valid), Ok(()));

        let edge_overflow = [Rectangle::new((i32::MAX - 9, 0).into(), (10, 1).into())];
        assert_eq!(
            validate_desktop_layout(edge_overflow),
            Err(DesktopLayoutError::EdgeOverflow)
        );

        let span_overflow = [
            Rectangle::new((i32::MIN, 0).into(), (1, 1).into()),
            Rectangle::new((0, 0).into(), (1, 1).into()),
        ];
        assert_eq!(
            validate_desktop_layout(span_overflow),
            Err(DesktopLayoutError::SpanOverflow)
        );
    }

    #[test]
    fn requested_positions_are_preserved_when_representable() {
        let existing = [Rectangle::new((-401, 83).into(), (347, 199).into())];
        let requested = Point::from((12_345, -678));
        assert_eq!(
            resolve_output_position(existing, Some(requested), (503, 281).into()),
            Some((requested, true))
        );

        let overlap = Point::from((-300, 100));
        assert_eq!(
            resolve_output_position(existing, Some(overlap), (503, 281).into()),
            Some((overlap, true))
        );
    }

    #[test]
    fn output_translation_must_fit_the_same_coordinate_domain() {
        assert_eq!(
            checked_output_delta((-50, 80).into(), (75, -20).into()),
            Some((125, -100).into())
        );
        assert_eq!(
            checked_output_delta((i32::MIN, 0).into(), (i32::MAX, 0).into()),
            None
        );
    }

    #[test]
    fn invalid_requested_position_uses_live_geometry_fallback() {
        let existing = [Rectangle::new((120, -40).into(), (347, 199).into())];
        let resolved = resolve_output_position(
            existing,
            Some((i32::MAX, i32::MAX).into()),
            (503, 281).into(),
        )
        .unwrap();
        assert_eq!(resolved, ((467, 0).into(), false));
    }

    #[test]
    fn automatic_position_can_overlap_at_the_coordinate_edge() {
        let existing = [Rectangle::new(
            (i32::MAX - 346, -40).into(),
            (346, 199).into(),
        )];
        let (position, preserved) =
            resolve_output_position(existing, None, (503, 281).into()).unwrap();
        assert!(!preserved);
        assert_eq!(position, (i32::MAX - 503, -40).into());
    }
}
