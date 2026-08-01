//! Model-neutral window placement input for the shared renderer.
//!
//! A spatial engine owns where windows live. Rendering only needs the window,
//! its logical rectangle, the camera/view transform, and a small amount of
//! presentation policy. Classic currently produces these records from its
//! active `Space` plus bounded swim previews. Ocean will produce the same
//! records from world rectangles and per-output cameras without teaching the
//! renderer about reefs, bookmarks, or workspace ownership.

use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
};

/// Whether a placement is part of the spatial engine's authoritative scene or
/// a non-interactive preview of content owned elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementRole {
    Authoritative,
    Preview,
}

/// How the renderer should size a window's last committed buffer for this
/// placement when no lifecycle/layout animation already supplies a size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentSizing {
    Committed,
    FitPlacement,
}

/// Spatial geometry class used by renderer-side `floating_only` policies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementKind {
    Tiled,
    Floating,
}

/// Whether this window is composited in the ordinary desktop band or the
/// fullscreen band above Top/Overlay layer-shell surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementStack {
    Normal,
    Fullscreen,
}

/// Applies a fractional camera/view translation at the final logical-pixel
/// boundary. Shared by compositor-owned chrome which follows a placement but
/// does not consume the full window animation sample.
pub(crate) fn translated_rect(
    mut rect: Rectangle<i32, Logical>,
    view_offset: Point<f64, Logical>,
) -> Rectangle<i32, Logical> {
    rect.loc += Point::from((view_offset.x.round() as i32, view_offset.y.round() as i32));
    rect
}

/// One window positioned in an output's model-neutral render scene.
///
/// `rect` remains in global logical coordinates, matching Smithay `Space` and
/// TideWM's decoration helpers. `view_offset` is render-only and may contain a
/// fractional camera translation. Keeping the transform separate avoids
/// rounding until the existing visual-sample aggregation point.
#[derive(Clone)]
pub(crate) struct PlacedWindow {
    pub(crate) window: Window,
    pub(crate) rect: Rectangle<i32, Logical>,
    pub(crate) view_offset: Point<f64, Logical>,
    /// View pixels per model pixel. Ocean uses this to invert pointer input
    /// after producing an already camera-transformed rectangle.
    pub(crate) view_scale: f64,
    pub(crate) role: PlacementRole,
    pub(crate) content_sizing: ContentSizing,
    pub(crate) kind: PlacementKind,
    pub(crate) stack: PlacementStack,
}

impl PlacedWindow {
    pub(crate) fn authoritative(window: Window, rect: Rectangle<i32, Logical>) -> Self {
        Self {
            window,
            rect,
            view_offset: Point::from((0.0, 0.0)),
            view_scale: 1.0,
            role: PlacementRole::Authoritative,
            content_sizing: ContentSizing::Committed,
            kind: PlacementKind::Tiled,
            stack: PlacementStack::Normal,
        }
    }

    pub(crate) fn preview(window: Window, rect: Rectangle<i32, Logical>) -> Self {
        Self {
            window,
            rect,
            view_offset: Point::from((0.0, 0.0)),
            view_scale: 1.0,
            role: PlacementRole::Preview,
            content_sizing: ContentSizing::FitPlacement,
            kind: PlacementKind::Tiled,
            stack: PlacementStack::Normal,
        }
    }

    pub(crate) fn with_view_offset(mut self, view_offset: Point<f64, Logical>) -> Self {
        self.view_offset = view_offset;
        self
    }

    pub(crate) fn with_view_scale(mut self, view_scale: f64) -> Self {
        self.view_scale = view_scale.max(0.05);
        self
    }

    pub(crate) fn fit_content_to_placement(mut self) -> Self {
        self.content_sizing = ContentSizing::FitPlacement;
        self
    }

    pub(crate) fn with_kind(mut self, kind: PlacementKind) -> Self {
        self.kind = kind;
        self
    }

    pub(crate) fn with_stack(mut self, stack: PlacementStack) -> Self {
        self.stack = stack;
        self
    }

    pub(crate) fn surface(&self) -> Option<&WlSurface> {
        self.window.toplevel().map(|toplevel| toplevel.wl_surface())
    }

    pub(crate) fn replacement_eligible(&self) -> bool {
        self.role == PlacementRole::Authoritative
    }

    pub(crate) fn fits_content_to_placement(&self) -> bool {
        self.content_sizing == ContentSizing::FitPlacement
    }

    pub(crate) fn is_floating(&self) -> bool {
        self.kind == PlacementKind::Floating
    }

    pub(crate) fn is_fullscreen(&self) -> bool {
        self.stack == PlacementStack::Fullscreen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Size;

    #[test]
    fn translated_rect_rounds_the_shared_view_transform_once() {
        let rect = Rectangle::new(Point::from((20, 30)), Size::from((400, 250)));
        assert_eq!(
            translated_rect(rect, Point::from((10.6, -4.6))),
            Rectangle::new(Point::from((31, 25)), Size::from((400, 250)))
        );
    }

    #[test]
    fn zero_view_transform_preserves_the_spatial_rect() {
        let rect = Rectangle::new(Point::from((-20, 5)), Size::from((80, 60)));
        assert_eq!(translated_rect(rect, Point::from((0.0, 0.0))), rect);
    }
}
