//! Captures the scene behind a window for water-glass refraction and frost.
//! Offscreen rendering runs before the visible output bind so winit's EGL
//! lifecycle remains valid and the same frame sees current window geometry.
//! Each capture keeps a texture and damage tracker, skipping unchanged scenes
//! while translated geometry still invalidates a moving window's capture.

use std::collections::HashSet;

use smithay::backend::{
    allocator::Fourcc,
    renderer::{
        damage::OutputDamageTracker,
        element::{
            utils::{Relocate, RelocateRenderElement, RescaleRenderElement},
            Id, RenderElement,
        },
        gles::{GlesRenderer, GlesTexture},
        Bind, Offscreen, Texture,
    },
};
use smithay::utils::{Physical, Point, Rectangle, Scale, Size, Transform};

/// Downscales a native size, rounding up and keeping each axis nonzero.
fn scaled_size(size: Size<i32, Physical>, scale: i32) -> (i32, i32) {
    let scale = scale.max(1);
    (
        ((size.w + scale - 1) / scale).max(1),
        ((size.h + scale - 1) / scale).max(1),
    )
}

/// A persistent capture with stable damage identity. `version` advances only
/// when new pixels are rendered into the texture.
pub struct BackdropCapture {
    pub texture: GlesTexture,
    pub id: Id,
    pub version: usize,
    /// Sized to the allocated texture so unchanged content can skip rendering.
    tracker: OutputDamageTracker,
    /// Native requested size, kept separately to detect size or scale changes.
    size: Size<i32, Physical>,
    scale: i32,
    /// Outputs whose latest scene snapshot still contained this surface.
    /// Names avoid retaining disconnected Smithay `Output` objects.
    visible_outputs: HashSet<String>,
}

impl BackdropCapture {
    /// ARGB pixel payload currently owned by this capture. Driver metadata,
    /// alignment, and allocator overhead are intentionally not guessed.
    pub fn estimated_texture_bytes(&self) -> u64 {
        let size = self.texture.size();
        u64::try_from(size.w.max(0))
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::try_from(size.h.max(0)).unwrap_or(u64::MAX))
            .saturating_mul(4)
    }

    /// Allocates a downscaled texture and matching damage tracker. Empty sizes
    /// and renderer failures return `None` after logging.
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
            visible_outputs: HashSet::new(),
        })
    }

    /// Reconciles one output's latest visibility result. Returns whether at
    /// least one output still needs this texture.
    pub fn set_output_visible(&mut self, output: &str, visible: bool) -> bool {
        update_output_visibility(&mut self.visible_outputs, output, visible)
    }

    /// Translates output-physical `behind` elements to `rect`'s origin and
    /// scales them into the capture texture. Size or scale changes reallocate
    /// both texture and tracker. Returns whether pixels were rendered, or
    /// `None` for an empty rectangle or logged renderer failure.
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
        // Keep one concrete element type for every live scale value.
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
        // The persistent texture contains exactly the previous render.
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

fn update_output_visibility(outputs: &mut HashSet<String>, output: &str, visible: bool) -> bool {
    if visible {
        outputs.insert(output.to_string());
    } else {
        outputs.remove(output);
    }
    !outputs.is_empty()
}

/// One-shot native-resolution capture for a full-output transition snapshot.
/// The per-window `backdrop_capture_scale` setting does not apply.
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

    #[test]
    fn capture_visibility_survives_until_its_last_output_drops_it() {
        let mut outputs = HashSet::new();

        assert!(update_output_visibility(&mut outputs, "left", true));
        assert!(update_output_visibility(&mut outputs, "right", true));
        assert!(update_output_visibility(&mut outputs, "left", false));
        assert!(!update_output_visibility(&mut outputs, "right", false));
    }
}
