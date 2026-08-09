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
            utils::{Relocate, RelocateRenderElement},
            Id, RenderElement,
        },
        gles::{GlesRenderer, GlesTexture},
        Bind, Offscreen,
    },
};
use smithay::utils::{Physical, Rectangle, Size, Transform};

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
    /// Persistent damage tracker, sized to the captured rect. This is what
    /// lets an unchanged behind-scene skip the offscreen render entirely;
    /// the old per-call tracker (and its `age = 0`) forced a full scene
    /// render for every glass window on every frame.
    tracker: OutputDamageTracker,
    size: Size<i32, Physical>,
}

impl BackdropCapture {
    /// Allocates the capture texture and its damage tracker. `None` on an
    /// empty size or a renderer/GL failure (logged, not fatal -- the caller
    /// simply tries again on a later frame).
    pub fn new(renderer: &mut GlesRenderer, size: Size<i32, Physical>) -> Option<Self> {
        if size.w <= 0 || size.h <= 0 {
            return None;
        }
        let texture = renderer
            .create_buffer(Fourcc::Argb8888, Size::from((size.w, size.h)))
            .map_err(|err| tracing::warn!(%err, "Failed to allocate backdrop capture texture"))
            .ok()?;
        Some(Self {
            texture,
            id: Id::new(),
            version: 0,
            tracker: OutputDamageTracker::new((size.w, size.h), 1.0, Transform::Normal),
            size,
        })
    }

    /// Renders `behind` -- elements positioned in the same output-physical
    /// space `rect` itself is given in -- into the capture texture,
    /// translating each one so `rect`'s own top-left lands at the texture's
    /// origin. A size change reallocates the texture and resets the tracker
    /// (both are sized to the captured region). Returns `Some(true)` when
    /// new content was actually rendered (`version` then already
    /// incremented), `Some(false)` when the tracker found no damage and
    /// skipped the render, and `None` on an empty rect or a renderer/GL
    /// failure (logged, not fatal -- a missed capture just means whatever
    /// effect wanted it skips a frame, not a crash).
    pub fn capture<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        rect: Rectangle<i32, Physical>,
        behind: &[E],
    ) -> Option<bool> {
        if rect.size.w <= 0 || rect.size.h <= 0 {
            return None;
        }
        if rect.size != self.size {
            let texture = renderer
                .create_buffer(Fourcc::Argb8888, Size::from((rect.size.w, rect.size.h)))
                .map_err(|err| tracing::warn!(%err, "Failed to resize backdrop capture texture"))
                .ok()?;
            self.texture = texture;
            self.tracker =
                OutputDamageTracker::new((rect.size.w, rect.size.h), 1.0, Transform::Normal);
            self.size = rect.size;
        }

        let offset = (-rect.loc.x, -rect.loc.y);
        let translated: Vec<RelocateRenderElement<&E>> = behind
            .iter()
            .map(|elem| RelocateRenderElement::from_element(elem, offset, Relocate::Relative))
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
/// been rendered.
pub fn capture_once<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    rect: Rectangle<i32, Physical>,
    behind: &[E],
) -> Option<GlesTexture> {
    let mut capture = BackdropCapture::new(renderer, rect.size)?;
    capture.capture(renderer, rect, behind)?;
    Some(capture.texture)
}
