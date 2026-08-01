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

use smithay::backend::{
    allocator::Fourcc,
    renderer::{
        damage::OutputDamageTracker,
        element::{
            utils::{Relocate, RelocateRenderElement},
            Id, RenderElement,
        },
        gles::{GlesRenderer, GlesTexture},
        utils::CommitCounter,
        Bind, Offscreen, Texture,
    },
};
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};

/// A backdrop capture plus the stable identity (`Id`) and change counter
/// (`CommitCounter`) a `water_glass::WaterGlassElement` built from it needs
/// to report to the damage tracker. `id` is created once per window the
/// first time it's captured and reused on every recapture -- a fresh `Id`
/// every frame would leak an orphaned entry in the damage tracker's own
/// per-element bookkeeping for every frame this window is water-glass-
/// eligible, never pruned. `commit` increments on every recapture so the
/// tracker knows the content changed and redraws it, rather than assuming
/// unchanged content and skipping the draw.
pub struct BackdropCapture {
    pub texture: GlesTexture,
    pub id: Id,
    pub commit: CommitCounter,
}

/// Renders `behind` -- elements positioned in the same output-physical
/// space `rect` itself is given in -- into a fresh texture sized to
/// `rect`, translating each one so `rect`'s own top-left lands at the
/// texture's origin. A same-sized `reusable` texture is rebound in place;
/// a size change allocates one replacement. `None` on an empty rect or a
/// renderer/GL failure (logged, not fatal -- a missed capture just means
/// whatever effect wanted it skips a frame, not a crash).
pub fn capture_backdrop<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    rect: Rectangle<i32, Physical>,
    behind: Vec<E>,
    reusable: Option<GlesTexture>,
) -> Option<GlesTexture> {
    if rect.size.w <= 0 || rect.size.h <= 0 {
        return None;
    }
    let offset = (-rect.loc.x, -rect.loc.y);
    let translated: Vec<RelocateRenderElement<E>> = behind
        .into_iter()
        .map(|elem| RelocateRenderElement::from_element(elem, offset, Relocate::Relative))
        .collect();

    let buffer_size: Size<i32, Buffer> = Size::from((rect.size.w, rect.size.h));
    let mut texture = match reusable.filter(|texture| texture.size() == buffer_size) {
        Some(texture) => texture,
        None => renderer
            .create_buffer(Fourcc::Argb8888, buffer_size)
            .map_err(|err| tracing::warn!(%err, "Failed to allocate backdrop capture texture"))
            .ok()?,
    };
    let mut target = renderer
        .bind(&mut texture)
        .map_err(|err| tracing::warn!(%err, "Failed to bind backdrop capture target"))
        .ok()?;
    let mut damage_tracker =
        OutputDamageTracker::new((rect.size.w, rect.size.h), 1.0, Transform::Normal);
    if let Err(err) =
        damage_tracker.render_output(renderer, &mut target, 0, &translated, [0.0, 0.0, 0.0, 0.0])
    {
        tracing::warn!(%err, "Failed to render backdrop capture frame");
        return None;
    }
    drop(target);
    Some(texture)
}
