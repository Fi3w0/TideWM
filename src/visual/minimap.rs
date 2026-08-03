//! Whole-world Ocean overview minimap (spatial roadmap S5's other half,
//! alongside `crate::compass`). Hold `minimap.key` (default `Super+Space`)
//! to peek: a schematic map of every window in the shared world, plus every
//! connected output's current camera viewport, scaled to fit the
//! triggering output. Click a window or region to travel that output's
//! camera there and dismiss; release without clicking just dismisses.
//! Ocean-only, and deliberately *not* gated by `water_effects` -- see
//! `MinimapConfig`'s own doc for why.
//!
//! Same CPU-composited-texture approach as `overview.rs` (built once per
//! peek via `MinimapPeek::build`, not rebuilt every frame) and literally
//! reuses its rasterization primitives (`fill_rect`, `stroke_rect`,
//! `draw_label`) for an identical dark-panel/labeled-box visual language --
//! this is Ocean's equivalent of Classic's schematic workspace grid, not a
//! new visual vocabulary.
//!
//! **Known-lossy, same spirit as `overview.rs`'s own admitted gaps:**
//! screen-pinned windows (`Smallvil::screen_pins`) aren't drawn -- a pin is
//! glued to one output's screen space, not a world location, so it has
//! nothing meaningful to show on a *world* map. And because the whole
//! canvas is built once at peek-open rather than kept live, a camera
//! mid-`OceanCameraMotion` ease on another output at that exact instant is
//! snapshotted at whatever point its animation had reached, not its final
//! destination -- a single-seat compositor can't move a second output's
//! camera *during* the hold itself (there's only one keyboard/pointer), so
//! this is the one residual staleness case, not a general "goes stale"
//! problem.
//!
//! **Pointer focus isn't explicitly restored on close.** Every motion
//! event while the peek is open moves the cursor with no focus target
//! (`pointer.motion(self, None, ..)`), which sends whatever surface was
//! under it a real Wayland `leave`. Closing the peek doesn't re-enter
//! anything itself -- the very next ordinary motion event re-runs
//! `surface_under` and re-enters normally, so this self-heals on the next
//! pointer move, but a click landing before any further motion (pointer
//! perfectly still since the peek closed) goes to focus `None` and is
//! lost. Narrow enough not to be worth a dedicated re-focus call for.

use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    utils::{Logical, Point, Rectangle, Size, Transform},
};

use crate::overview::{draw_label, fill_rect, rgba, stroke_rect};

const BG_RGBA: (u8, u8, u8, u8) = (10, 10, 12, 220);
const WINDOW_BG: (u8, u8, u8) = (52, 52, 58);
const WINDOW_BORDER: (u8, u8, u8) = (100, 100, 108);
const WINDOW_BORDER_PX: i32 = 1;
/// Same water-palette accent `overview.rs` uses for its own active-cell
/// border -- the triggering output's own viewport gets this; every other
/// output's viewport gets the plainer `VIEWPORT_BORDER` instead.
const VIEWPORT_ACTIVE_BORDER: (u8, u8, u8) = (60, 170, 200);
const VIEWPORT_BORDER: (u8, u8, u8) = (150, 150, 158);
const VIEWPORT_BORDER_PX: i32 = 2;
/// Padding around the world content's bounding box, as a fraction of its
/// own size, so nothing sits flush against the canvas edge.
const MARGIN_FRACTION: f64 = 0.1;
/// Floor for the padded extent's width/height, world-logical pixels. Keeps
/// `scale` (canvas px / world px) finite for a near-empty world or a
/// single tight viewport instead of it blowing up toward infinity.
const MIN_EXTENT: f64 = 256.0;

/// One peek's built state: the cached world-map texture plus the transform
/// needed to invert a click back to a world point. Held by
/// `Smallvil::minimap_peek` for the hold gesture's duration.
pub struct MinimapPeek {
    /// Which output this was built for -- also which output's camera
    /// travels on click, decided once at open time rather than re-derived
    /// later through anything pointer-location-dependent.
    output_name: String,
    buffer: MemoryRenderBuffer,
    /// World point mapped to canvas-space (0, 0).
    world_origin: Point<f64, Logical>,
    /// Canvas pixels per world logical pixel.
    scale: f64,
    /// Last known pointer position in canvas-space logical pixels. Seeded
    /// from the real pointer position at peek-open time and kept live by
    /// ordinary motion-event handling for the rest of the hold -- never
    /// read via `current_location()` from inside a grab/click dispatch,
    /// the same freeze shape as the `TileMoveGrab` (0.15.1) and
    /// `sync_visible_floating_window` incidents already in this project's
    /// history.
    last_location: Point<f64, Logical>,
}

impl MinimapPeek {
    /// `windows` and `viewports` are world-space rects paired with the
    /// label drawn on them (a window's title, or an output's own name) --
    /// the exact `(Rectangle, String)` shape `overview.rs`'s
    /// `OverviewCell::windows` already uses, so `draw_label` needs no
    /// window/toplevel type at all. `reef_rects` only widens the framed
    /// extent (an empty reef is still a landmark worth keeping in view);
    /// nothing is drawn for it, matching the plain "boxes only" visual
    /// language `overview.rs` already established.
    pub fn build(
        output_name: String,
        output_size: (i32, i32),
        pointer_location: Point<f64, Logical>,
        windows: &[(Rectangle<i32, Logical>, String)],
        reef_rects: &[Rectangle<i32, Logical>],
        viewports: &[(String, Rectangle<i32, Logical>)],
    ) -> Self {
        let (width, height) = output_size;

        let mut extent: Option<Rectangle<f64, Logical>> = None;
        let mut grow = |rect: Rectangle<i32, Logical>| {
            let rect = rect.to_f64();
            extent = Some(match extent {
                Some(current) => current.merge(rect),
                None => rect,
            });
        };
        for (rect, _) in windows {
            grow(*rect);
        }
        for rect in reef_rects {
            grow(*rect);
        }
        for (_, rect) in viewports {
            grow(*rect);
        }
        let content = extent.unwrap_or_else(|| {
            Rectangle::new(
                Point::from((0.0, 0.0)),
                Size::from((MIN_EXTENT, MIN_EXTENT)),
            )
        });

        let pad_w = (content.size.w * MARGIN_FRACTION).max(MIN_EXTENT * MARGIN_FRACTION);
        let pad_h = (content.size.h * MARGIN_FRACTION).max(MIN_EXTENT * MARGIN_FRACTION);
        let padded: Rectangle<f64, Logical> = Rectangle::new(
            Point::from((content.loc.x - pad_w, content.loc.y - pad_h)),
            Size::from((
                (content.size.w + pad_w * 2.0).max(MIN_EXTENT),
                (content.size.h + pad_h * 2.0).max(MIN_EXTENT),
            )),
        );

        let scale = (width as f64 / padded.size.w).min(height as f64 / padded.size.h);
        let letterbox_x = (width as f64 - padded.size.w * scale) / 2.0;
        let letterbox_y = (height as f64 - padded.size.h * scale) / 2.0;
        // Folds letterbox centering into world_origin so world<->canvas is
        // one multiply/divide each way, with no separate offset to carry
        // (and no separate offset to get wrong at click-inversion time).
        let world_origin = Point::from((
            padded.loc.x - letterbox_x / scale,
            padded.loc.y - letterbox_y / scale,
        ));

        let mut pixels = vec![0u8; (width * height * 4) as usize];
        fill_rect(
            &mut pixels,
            width,
            height,
            Rectangle::new(Point::from((0, 0)), Size::from((width, height))),
            BG_RGBA,
        );

        let world_to_canvas = |rect: Rectangle<i32, Logical>| -> Rectangle<i32, Logical> {
            let rect = rect.to_f64();
            let x0 = (rect.loc.x - world_origin.x) * scale;
            let y0 = (rect.loc.y - world_origin.y) * scale;
            Rectangle::new(
                Point::from((x0.round() as i32, y0.round() as i32)),
                Size::from((
                    (rect.size.w * scale).round().max(1.0) as i32,
                    (rect.size.h * scale).round().max(1.0) as i32,
                )),
            )
        };

        let font = crate::toast::font();
        for (rect, title) in windows {
            let canvas_rect = world_to_canvas(*rect);
            fill_rect(&mut pixels, width, height, canvas_rect, rgba(WINDOW_BG));
            stroke_rect(
                &mut pixels,
                width,
                height,
                canvas_rect,
                WINDOW_BORDER,
                WINDOW_BORDER_PX,
            );
            draw_label(&mut pixels, width, height, font, title, canvas_rect);
        }
        for (name, rect) in viewports {
            let canvas_rect = world_to_canvas(*rect);
            let (border, thickness) = if name == &output_name {
                (VIEWPORT_ACTIVE_BORDER, VIEWPORT_BORDER_PX + 1)
            } else {
                (VIEWPORT_BORDER, VIEWPORT_BORDER_PX)
            };
            stroke_rect(&mut pixels, width, height, canvas_rect, border, thickness);
            draw_label(&mut pixels, width, height, font, name, canvas_rect);
        }

        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (width, height),
            1,
            Transform::Normal,
            None,
        );

        Self {
            output_name,
            buffer,
            world_origin,
            scale,
            last_location: pointer_location,
        }
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    pub fn render_element(
        &self,
        renderer: &mut GlesRenderer,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (0.0, 0.0),
            &self.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }

    pub fn set_last_location(&mut self, location: Point<f64, Logical>) {
        self.last_location = location;
    }

    /// Inverts `world_to_canvas` at the peek's last known pointer position
    /// -- the world point a click should travel the camera to.
    pub fn world_point_at_last_location(&self) -> Point<f64, Logical> {
        Point::from((
            self.world_origin.x + self.last_location.x / self.scale,
            self.world_origin.y + self.last_location.y / self.scale,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(
        windows: &[(Rectangle<i32, Logical>, String)],
        viewports: &[(String, Rectangle<i32, Logical>)],
        pointer: Point<f64, Logical>,
    ) -> MinimapPeek {
        MinimapPeek::build(
            "output-1".to_string(),
            (1920, 1080),
            pointer,
            windows,
            &[],
            viewports,
        )
    }

    /// The core transform-correctness check: build a world with one known
    /// window, click its drawn box's exact center, and the resolved world
    /// point must land inside that window's real world rect. A scale or
    /// offset mixup (forgetting padding, forgetting the letterbox term)
    /// fails this immediately instead of only showing up as "clicks feel
    /// slightly off" during manual testing.
    /// Click squarely on a single window with nothing else in the world:
    /// the resolved world point must fall inside that window's own rect.
    #[test]
    fn click_on_the_only_window_resolves_inside_it() {
        let window_rect =
            Rectangle::<i32, Logical>::new(Point::from((5000, -3000)), Size::from((1200, 800)));
        let mut peek = build(
            &[(window_rect, "code".to_string())],
            &[],
            Point::from((0.0, 0.0)),
        );

        // Canvas center is the padded extent's center, which (with only
        // one rect in the world) is the window's own center.
        peek.set_last_location(Point::from((960.0, 540.0)));
        let world = peek.world_point_at_last_location();

        let world_point: Point<i32, Logical> =
            Point::from((world.x.round() as i32, world.y.round() as i32));
        assert!(window_rect.contains(world_point));
    }

    /// A click on a drawn camera-viewport rect must resolve to a world
    /// point inside that output's own current viewport -- the assert most
    /// likely to catch a scale-vs-offset mixup that a pure center
    /// round-trip could still pass (center math cancels a wrong offset;
    /// an off-center point does not).
    #[test]
    fn click_on_a_viewport_rect_resolves_inside_that_viewport() {
        let viewport_b =
            Rectangle::<i32, Logical>::new(Point::from((-4000, 1000)), Size::from((1920, 1080)));
        let mut peek = build(
            &[],
            &[
                (
                    "output-1".to_string(),
                    Rectangle::new(Point::from((0, 0)), Size::from((1920, 1080))),
                ),
                ("output-2".to_string(), viewport_b),
            ],
            Point::from((0.0, 0.0)),
        );

        peek.set_last_location(Point::from((960.0, 540.0)));
        let world = peek.world_point_at_last_location();
        let world_point: Point<i32, Logical> =
            Point::from((world.x.round() as i32, world.y.round() as i32));
        let union = viewport_b.merge(Rectangle::new(
            Point::from((0, 0)),
            Size::from((1920, 1080)),
        ));
        assert!(union.contains(world_point));
    }

    /// Nothing in the world at all (no windows, no reefs, one degenerate
    /// or absent viewport) must not divide scale toward infinity or NaN --
    /// the `MIN_EXTENT` floor's whole reason for existing.
    #[test]
    fn empty_world_does_not_produce_a_degenerate_scale() {
        let peek = build(&[], &[], Point::from((0.0, 0.0)));
        assert!(peek.scale.is_finite() && peek.scale > 0.0);
        assert!(peek.world_origin.x.is_finite() && peek.world_origin.y.is_finite());
    }
}
