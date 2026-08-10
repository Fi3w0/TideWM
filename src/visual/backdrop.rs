//! Backdrop capture renders whatever sits behind a window's rect into an
//! offscreen texture -- the shared plumbing every future water/decoration
//! effect that samples "what's behind this window" (water-glass refraction,
//! frost-glass blur) needs before it can run its own shader against that
//! content. Render roadmap Phase R0.5, see AGENT.md's "Render and visual
//! identity roadmap".
//!
//! Reuses the exact bind/render_output technique `capture.rs` uses for
//! screenshots, but runs immediately before the visible output is bound.
//! Offscreen work between a visible bind and submit breaks winit's EGL
//! lifecycle; doing it before that bind is safe and lets the same visible
//! frame consume a capture made for the window's current geometry. This is
//! important during an interactive drag, where a post-submit capture would
//! always be displayed one pointer event behind.
//!
//! Captures are damage-tracked: each `BackdropCapture` owns a persistent
//! `OutputDamageTracker` and reuses one texture, so a capture pass over an
//! unchanged behind-scene costs zero GL work (`render_output` returns with
//! no damage and nothing is drawn). The tracker also sees a moved window on
//! its own -- the translated element geometry no longer matches its
//! bookkeeping -- so an interactive drag still recaptures every frame.

use smithay::backend::{
    allocator::Fourcc,
    renderer::{
        damage::OutputDamageTracker,
        element::{
            utils::{Relocate, RelocateRenderElement, RescaleRenderElement},
            Id, RenderElement,
        },
        gles::{GlesRenderer, GlesTexture},
        Bind, Offscreen,
    },
};
use smithay::utils::{Physical, Point, Rectangle, Scale, Size, Transform};

/// Shrinks a captured rect's native size by `scale` (clamped to at least
/// `1`), rounding each axis up so the allocated texture never rounds down to
/// zero on a thin window. `1` reproduces the exact prior full-resolution
/// behavior. Returned as a plain tuple, not a typed `Size`, since callers
/// need it in both `Buffer` space (`create_buffer`) and `Physical` space
/// (the damage tracker's own size).
fn scaled_size(size: Size<i32, Physical>, scale: i32) -> (i32, i32) {
    let scale = scale.max(1);
    (
        ((size.w + scale - 1) / scale).max(1),
        ((size.h + scale - 1) / scale).max(1),
    )
}

/// A backdrop capture plus the stable identity (`Id`) and content version a
/// `water_glass::WaterGlassElement` built from it needs to report to the
/// damage tracker. `id` is created once per window the first time it's
/// captured and reused on every recapture -- a fresh `Id` every frame would
/// leak an orphaned entry in the damage tracker's own per-element
/// bookkeeping for every frame this window is water-glass-eligible, never
/// pruned. `version` increments only when a recapture actually re-rendered
/// the texture, so consumers can tell unchanged content apart from new
/// content instead of assuming every frame brought a fresh capture.
pub struct BackdropCapture {
    pub texture: GlesTexture,
    pub id: Id,
    pub version: usize,
    /// Persistent damage tracker, sized to the *allocated* (possibly
    /// downscaled) texture. This is what lets an unchanged behind-scene
    /// skip the offscreen render entirely; the old per-call tracker (and
    /// its `age = 0`) forced a full scene render for every glass window on
    /// every frame.
    tracker: OutputDamageTracker,
    /// Native (unscaled) requested rect size, tracked separately from the
    /// texture's own allocated size so a resize and a live
    /// `backdrop_capture_scale` config change are both detected against the
    /// same field a caller actually passes in.
    size: Size<i32, Physical>,
    scale: i32,
}

impl BackdropCapture {
    /// Allocates the capture texture (at `size` downscaled by `scale`, `1`
    /// meaning full native resolution) and its damage tracker. `None` on an
    /// empty size or a renderer/GL failure (logged, not fatal -- the caller
    /// simply tries again on a later frame).
    pub fn new(renderer: &mut GlesRenderer, size: Size<i32, Physical>, scale: i32) -> Option<Self> {
        if size.w <= 0 || size.h <= 0 {
            return None;
        }
        let scale = scale.max(1);
        let (texture_w, texture_h) = scaled_size(size, scale);
        let texture = renderer
            .create_buffer(Fourcc::Argb8888, Size::from((texture_w, texture_h)))
            .map_err(|err| tracing::warn!(%err, "Failed to allocate backdrop capture texture"))
            .ok()?;
        Some(Self {
            texture,
            id: Id::new(),
            version: 0,
            tracker: OutputDamageTracker::new((texture_w, texture_h), 1.0, Transform::Normal),
            size,
            scale,
        })
    }

    /// Renders `behind` -- elements positioned in the same output-physical
    /// space `rect` itself is given in -- into the capture texture,
    /// translating each one so `rect`'s own top-left lands at the texture's
    /// origin, then scaling that translated geometry down by `scale` so it
    /// fits a texture allocated at `rect.size / scale`. A native-size or
    /// `scale` change reallocates the texture and resets the tracker (both
    /// stay sized to the downscaled region). Returns `Some(true)` when new
    /// content was actually rendered (`version` then already incremented),
    /// `Some(false)` when the tracker found no damage and skipped the
    /// render, and `None` on an empty rect or a renderer/GL failure (logged,
    /// not fatal -- a missed capture just means whatever effect wanted it
    /// skips a frame, not a crash).
    pub fn capture<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        rect: Rectangle<i32, Physical>,
        behind: &[E],
        scale: i32,
    ) -> Option<bool> {
        if rect.size.w <= 0 || rect.size.h <= 0 {
            return None;
        }
        let scale = scale.max(1);
        if rect.size != self.size || scale != self.scale {
            let (texture_w, texture_h) = scaled_size(rect.size, scale);
            let texture = renderer
                .create_buffer(Fourcc::Argb8888, Size::from((texture_w, texture_h)))
                .map_err(|err| tracing::warn!(%err, "Failed to resize backdrop capture texture"))
                .ok()?;
            self.texture = texture;
            self.tracker = OutputDamageTracker::new((texture_w, texture_h), 1.0, Transform::Normal);
            self.size = rect.size;
            self.scale = scale;
        }

        let offset = (-rect.loc.x, -rect.loc.y);
        let origin = Point::<i32, Physical>::from((0, 0));
        // Always wrapped, even at scale 1 (an identity rescale), so the
        // element list stays one concrete type regardless of the live
        // config value instead of branching into two `Vec` element types.
        let downscale = Scale::from(1.0 / scale as f64);
        let translated: Vec<RescaleRenderElement<RelocateRenderElement<&E>>> = behind
            .iter()
            .map(|elem| {
                let relocated =
                    RelocateRenderElement::from_element(elem, offset, Relocate::Relative);
                RescaleRenderElement::from_element(relocated, origin, downscale)
            })
            .collect();

        let mut target = renderer
            .bind(&mut self.texture)
            .map_err(|err| tracing::warn!(%err, "Failed to bind backdrop capture target"))
            .ok()?;
        // One persistent texture rendered once per capture pass, so the
        // buffer age is exactly 1: the texture holds the previous render's
        // content and only new damage has to be drawn.
        match self.tracker.render_output(
            renderer,
            &mut target,
            1,
            &translated,
            [0.0, 0.0, 0.0, 0.0],
        ) {
            Ok(result) => {
                let rendered = result.damage.is_some();
                if rendered {
                    self.version = self.version.wrapping_add(1);
                }
                Some(rendered)
            }
            Err(err) => {
                tracing::warn!(%err, "Failed to render backdrop capture frame");
                None
            }
        }
    }
}

/// One-shot full render of `behind` into a fresh texture, for callers that
/// capture once and move on (the workspace-transition snapshot) and so have
/// no use for a persistent damage tracker. A fresh tracker treats the whole
/// region as damaged, which is exactly right for a texture that has never
/// been rendered. Always captures at native resolution (`scale = 1`),
/// independent of `backdrop_capture_scale`: this feeds a full-output texture
/// displayed 1:1 during the workspace transition, not a per-window glass
/// effect, so softening it would be a visible, unrelated regression rather
/// than the VRAM trade that knob is for.
pub fn capture_once<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    rect: Rectangle<i32, Physical>,
    behind: &[E],
) -> Option<GlesTexture> {
    let mut capture = BackdropCapture::new(renderer, rect.size, 1)?;
    capture.capture(renderer, rect, behind, 1)?;
    Some(capture.texture)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_one_reproduces_the_native_size_exactly() {
        assert_eq!(scaled_size(Size::from((1920, 1080)), 1), (1920, 1080));
    }

    #[test]
    fn scale_rounds_up_so_a_thin_window_never_hits_zero() {
        assert_eq!(scaled_size(Size::from((7, 3)), 4), (2, 1));
        assert_eq!(scaled_size(Size::from((1, 1)), 4), (1, 1));
    }

    #[test]
    fn scale_below_one_is_clamped_to_native_size() {
        assert_eq!(scaled_size(Size::from((800, 600)), 0), (800, 600));
        assert_eq!(scaled_size(Size::from((800, 600)), -3), (800, 600));
    }
}
