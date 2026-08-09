//! TideWM's deliberately small built-in fallback wallpaper.
//!
//! Fi3w0's canonical 4K artwork is embedded so installs do not depend on a
//! loose runtime file. It is decoded lazily, uploaded once per GLES context,
//! and then released from CPU memory while the GPU texture remains alive.
//! A layer-shell background from swaybg/swww/awww/hyprpaper naturally renders
//! above it, so richer tools need no TideWM-specific integration.

use std::io::Cursor;

use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::{
        element::{
            texture::{TextureBuffer, TextureRenderElement},
            Kind,
        },
        gles::{GlesRenderer, GlesTexture},
        ContextId, Renderer,
    },
    utils::{Logical, Rectangle, Size, Transform},
};

const WIDTH: i32 = 3840;
const HEIGHT: i32 = 2160;
const WALLPAPER_PNG: &[u8] = include_bytes!("../../assets/tide-aqua-4k.png");

pub struct BuiltinWallpaper {
    imported: Option<ImportedWallpaper>,
    pending_pixels: Option<Vec<u8>>,
    failed_context: Option<ContextId<GlesTexture>>,
}

struct ImportedWallpaper {
    context: ContextId<GlesTexture>,
    buffer: TextureBuffer<GlesTexture>,
}

#[derive(Debug, PartialEq, Eq)]
enum ImportDecision {
    Reuse,
    Import,
    SkipFailedContext,
}

fn import_decision(
    imported: Option<&ContextId<GlesTexture>>,
    failed: Option<&ContextId<GlesTexture>>,
    current: &ContextId<GlesTexture>,
) -> ImportDecision {
    if imported.is_some_and(|imported| imported == current) {
        ImportDecision::Reuse
    } else if failed.is_some_and(|failed| failed == current) {
        ImportDecision::SkipFailedContext
    } else {
        ImportDecision::Import
    }
}

impl BuiltinWallpaper {
    pub fn build() -> Self {
        Self {
            imported: None,
            pending_pixels: None,
            failed_context: None,
        }
    }

    pub fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        logical_size: Size<i32, Logical>,
    ) -> Option<TextureRenderElement<GlesTexture>> {
        self.ensure_imported(renderer)?;
        let buffer = &self.imported.as_ref()?.buffer;
        let source = cover_source_rect(logical_size);
        Some(TextureRenderElement::from_texture_buffer(
            (0.0, 0.0),
            buffer,
            None,
            Some(source),
            Some(logical_size),
            Kind::Unspecified,
        ))
    }

    fn ensure_imported(&mut self, renderer: &mut GlesRenderer) -> Option<()> {
        let context = renderer.context_id();
        match import_decision(
            self.imported.as_ref().map(|imported| &imported.context),
            self.failed_context.as_ref(),
            &context,
        ) {
            ImportDecision::Reuse => return Some(()),
            ImportDecision::SkipFailedContext => return None,
            ImportDecision::Import => {}
        }

        // A renderer replacement cannot consume a texture owned by the old
        // context. Re-decode only at that boundary; successful imports never
        // retain the full native artwork in CPU memory.
        if self.imported.take().is_some() {
            self.pending_pixels = None;
        }

        // Import failure is normally persistent for the current GLES
        // context (unsupported format or resource exhaustion). Suppress
        // frame-rate retries and try again if the backend creates a new
        // context. The decoded pixels stay available for that retry.
        self.failed_context = None;

        if self.pending_pixels.is_none() {
            self.pending_pixels = Some(match decode_canonical_wallpaper() {
                Ok(pixels) => pixels,
                Err(err) => {
                    // The bytes are compile-time-owned, not user input, but
                    // a bad asset must not make the compositor fail to start.
                    tracing::warn!(%err, "Built-in wallpaper decode failed; using procedural fallback");
                    procedural_fallback()
                }
            });
        }

        let pixels = self.pending_pixels.as_deref()?;
        let buffer = match TextureBuffer::from_memory(
            renderer,
            pixels,
            Fourcc::Argb8888,
            (WIDTH, HEIGHT),
            false,
            1,
            Transform::Normal,
            None,
        ) {
            Ok(buffer) => buffer,
            Err(err) => {
                tracing::warn!(%err, "Built-in wallpaper GPU import failed");
                self.failed_context = Some(context);
                return None;
            }
        };

        self.imported = Some(ImportedWallpaper { context, buffer });
        self.pending_pixels = None;
        Some(())
    }
}

/// Center-crop the source to the destination aspect ratio (CSS
/// `background-size: cover` semantics). Stretching the artwork would deform
/// the logo on portrait or unusually shaped outputs.
fn cover_source_rect(destination: Size<i32, Logical>) -> Rectangle<f64, Logical> {
    let destination_aspect = destination.w.max(1) as f64 / destination.h.max(1) as f64;
    let source_aspect = WIDTH as f64 / HEIGHT as f64;
    if destination_aspect > source_aspect {
        let height = WIDTH as f64 / destination_aspect;
        Rectangle::new(
            (0.0, (HEIGHT as f64 - height) / 2.0).into(),
            (WIDTH as f64, height).into(),
        )
    } else {
        let width = HEIGHT as f64 * destination_aspect;
        Rectangle::new(
            ((WIDTH as f64 - width) / 2.0, 0.0).into(),
            (width, HEIGHT as f64).into(),
        )
    }
}

/// Decode the repository's trusted 3840x2160 RGB artwork into the BGRA layout
/// Smithay's memory renderer expects. Keeping every source pixel is deliberate:
/// reducing this to the old 640x360 fallback size made the logo visibly soft
/// when the GPU scaled it back up on a Full HD or 4K output.
fn decode_canonical_wallpaper() -> Result<Vec<u8>, String> {
    let mut decoder = png::Decoder::new(Cursor::new(WALLPAPER_PNG));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|err| err.to_string())?;
    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| "decoded wallpaper size exceeds addressable memory".to_owned())?;
    let mut source = vec![0; buffer_size];
    let info = reader
        .next_frame(&mut source)
        .map_err(|err| err.to_string())?;

    if info.width != WIDTH as u32
        || info.height != HEIGHT as u32
        || info.color_type != png::ColorType::Rgb
    {
        return Err(format!(
            "expected {WIDTH}x{HEIGHT} RGB8, got {}x{} {:?}",
            info.width, info.height, info.color_type,
        ));
    }

    let mut pixels = vec![0; (WIDTH * HEIGHT * 4) as usize];
    for y in 0..HEIGHT as usize {
        for x in 0..WIDTH as usize {
            let i = (y * WIDTH as usize + x) * 3;
            put_pixel(
                &mut pixels,
                x as i32,
                y as i32,
                (source[i], source[i + 1], source[i + 2], 255),
            );
        }
    }
    Ok(pixels)
}

fn procedural_fallback() -> Vec<u8> {
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for y in 0..HEIGHT {
        let t = y as f32 / (HEIGHT - 1) as f32;
        for x in 0..WIDTH {
            let glow_x = (x as f32 / WIDTH as f32 - 0.72).powi(2);
            let glow_y = (y as f32 / HEIGHT as f32 - 0.24).powi(2);
            let glow = (1.0 - (glow_x + glow_y).sqrt() * 1.7).clamp(0.0, 1.0);
            let r = (7.0 + 5.0 * (1.0 - t) + 4.0 * glow) as u8;
            let g = (18.0 + 16.0 * (1.0 - t) + 35.0 * glow) as u8;
            let b = (26.0 + 19.0 * (1.0 - t) + 46.0 * glow) as u8;
            put_pixel(&mut pixels, x, y, (r, g, b, 255));
        }
    }

    // Three understated tide lines. They make the fallback recognisable
    // without trying to become a full image loader or wallpaper daemon.
    for band in 0..3 {
        let base = 224 + band * 24;
        for x in 0..WIDTH {
            let phase = x as f32 / WIDTH as f32 * std::f32::consts::TAU * 1.35;
            let y = base + (phase.sin() * 12.0) as i32;
            for thickness in -2..=2 {
                let yy = y + thickness;
                if (0..HEIGHT).contains(&yy) {
                    let alpha = if thickness.abs() == 2 { 38 } else { 72 };
                    blend_pixel(&mut pixels, x, yy, (61, 188, 215), alpha);
                }
            }
        }
    }

    pixels
}

fn put_pixel(pixels: &mut [u8], x: i32, y: i32, (r, g, b, a): (u8, u8, u8, u8)) {
    let i = ((y * WIDTH + x) * 4) as usize;
    pixels[i] = b;
    pixels[i + 1] = g;
    pixels[i + 2] = r;
    pixels[i + 3] = a;
}

fn blend_pixel(pixels: &mut [u8], x: i32, y: i32, rgb: (u8, u8, u8), alpha: u8) {
    let i = ((y * WIDTH + x) * 4) as usize;
    let t = alpha as f32 / 255.0;
    let (r, g, b) = rgb;
    pixels[i] = (pixels[i] as f32 + (b as f32 - pixels[i] as f32) * t) as u8;
    pixels[i + 1] = (pixels[i + 1] as f32 + (g as f32 - pixels[i + 1] as f32) * t) as u8;
    pixels[i + 2] = (pixels[i + 2] as f32 + (r as f32 - pixels[i + 2] as f32) * t) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallpaper_build_is_lazy() {
        let wallpaper = BuiltinWallpaper::build();
        assert!(wallpaper.imported.is_none());
        assert!(wallpaper.pending_pixels.is_none());
        assert!(wallpaper.failed_context.is_none());
    }

    #[test]
    fn texture_import_policy_is_context_aware_and_suppresses_failure_retries() {
        let first = ContextId::<GlesTexture>::new();
        let replacement = ContextId::<GlesTexture>::new();

        assert_eq!(
            import_decision(Some(&first), None, &first),
            ImportDecision::Reuse
        );
        assert_eq!(
            import_decision(None, Some(&first), &first),
            ImportDecision::SkipFailedContext
        );
        assert_eq!(
            import_decision(None, Some(&first), &replacement),
            ImportDecision::Import
        );
        assert_eq!(
            import_decision(Some(&first), None, &replacement),
            ImportDecision::Import
        );
    }

    #[test]
    fn canonical_wallpaper_decodes_into_the_fixed_bgra_buffer() {
        let pixels = decode_canonical_wallpaper().unwrap();
        assert_eq!(pixels.len(), (WIDTH * HEIGHT * 4) as usize);
        assert!(pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn cover_crop_preserves_aspect_ratio_without_stretching() {
        let portrait = cover_source_rect((900, 1200).into());
        assert_eq!(portrait.size.h, HEIGHT as f64);
        assert_eq!(portrait.size.w / portrait.size.h, 0.75);
        assert!(portrait.loc.x > 0.0);

        let ultrawide = cover_source_rect((3440, 1440).into());
        assert_eq!(ultrawide.size.w, WIDTH as f64);
        assert!((ultrawide.size.w / ultrawide.size.h - 3440.0 / 1440.0).abs() < f64::EPSILON);
        assert!(ultrawide.loc.y > 0.0);
    }
}
