//! Cached CPU-rendered tab strip for a grouped tiled leaf. Title commits from
//! active or parked members invalidate the owning group so the next frame uses
//! current labels.

use fontdue::Font;
use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::Transform,
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

pub const HEIGHT: i32 = 22;
const FONT_SIZE: f32 = 13.0;
const LABEL_PAD: i32 = 6;
const ACTIVE_BG: (u8, u8, u8) = (28, 94, 116); // same water-palette tint as Toast's Info kind
const INACTIVE_BG: (u8, u8, u8) = (46, 46, 46);
const TEXT_RGB: (u8, u8, u8) = (230, 230, 230);

fn display_title(title: Option<String>) -> String {
    title
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string())
}

/// The current client title, or a placeholder when absent.
pub fn window_title(surface: &WlSurface) -> String {
    display_title(with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()?
            .lock()
            .ok()?
            .title
            .clone()
    }))
}

/// Builds the cached render buffer for a strip `width` wide, one segment
/// per entry in `titles`, `active` highlighted.
pub fn build_buffer(
    titles: &[String],
    active: usize,
    width: i32,
) -> Option<(MemoryRenderBuffer, i32)> {
    let width = width.max(1);
    let pixel_len = crate::text::checked_argb_len(width, HEIGHT)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(pixel_len).ok()?;
    pixels.resize(pixel_len, 0);
    if !titles.is_empty() {
        let font = crate::toast::font();
        let segment_w = width / titles.len() as i32;
        for (i, title) in titles.iter().enumerate() {
            let x0 = i as i32 * segment_w;
            let seg_w = if i == titles.len() - 1 {
                width - x0
            } else {
                segment_w
            };
            let bg = if i == active { ACTIVE_BG } else { INACTIVE_BG };
            for y in 0..HEIGHT {
                for x in x0..x0 + seg_w {
                    put_pixel(&mut pixels, width, x, y, (bg.0, bg.1, bg.2, 235));
                }
            }
            draw_label(&mut pixels, width, font, title, x0, seg_w);
        }
    }

    let buffer = MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (width, HEIGHT),
        1,
        Transform::Normal,
        None,
    );
    Some((buffer, width))
}

/// Draws `title` inside the segment spanning `[x0, x0 + seg_w)`. Layout stops
/// at the live segment width and adds an ellipsis instead of rasterizing text
/// that cannot become visible.
fn draw_label(pixels: &mut [u8], width: i32, font: &Font, title: &str, x0: i32, seg_w: i32) {
    let baseline = HEIGHT - (HEIGHT - FONT_SIZE as i32) / 2 - 3;
    let mut pen_x = x0 + LABEL_PAD;
    let line =
        crate::text::rasterize_line(font, title, FONT_SIZE, seg_w.saturating_sub(LABEL_PAD * 2));

    for glyph in line.glyphs {
        let metrics = glyph.metrics;
        let glyph_x0 = pen_x + metrics.xmin;
        let glyph_y0 = baseline - metrics.ymin - metrics.height as i32;

        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let coverage = glyph.bitmap[gy * metrics.width + gx];
                if coverage == 0 {
                    continue;
                }
                let x = glyph_x0 + gx as i32;
                let y = glyph_y0 + gy as i32;
                if x < x0 || y < 0 || x >= x0 + seg_w || y >= HEIGHT {
                    continue;
                }
                blend_text_pixel(pixels, width, x, y, coverage);
            }
        }
        pen_x += metrics.advance_width.round().max(1.0) as i32;
    }
}

fn put_pixel(pixels: &mut [u8], width: i32, x: i32, y: i32, (r, g, b, a): (u8, u8, u8, u8)) {
    let i = ((y * width + x) * 4) as usize;
    // Fourcc::Argb8888 in memory (little-endian) is byte order B, G, R, A.
    pixels[i] = b;
    pixels[i + 1] = g;
    pixels[i + 2] = r;
    pixels[i + 3] = a;
}

fn blend_text_pixel(pixels: &mut [u8], width: i32, x: i32, y: i32, coverage: u8) {
    let i = ((y * width + x) * 4) as usize;
    let t = coverage as f32 / 255.0;
    let (tr, tg, tb) = TEXT_RGB;
    pixels[i] = (pixels[i] as f32 + (tb as f32 - pixels[i] as f32) * t) as u8;
    pixels[i + 1] = (pixels[i + 1] as f32 + (tg as f32 - pixels[i + 1] as f32) * t) as u8;
    pixels[i + 2] = (pixels[i + 2] as f32 + (tr as f32 - pixels[i + 2] as f32) * t) as u8;
    pixels[i + 3] = pixels[i + 3].max(coverage);
}

pub fn render_element(
    buffer: &MemoryRenderBuffer,
    renderer: &mut GlesRenderer,
    location: (f64, f64),
) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        location,
        buffer,
        None,
        None,
        None,
        Kind::Unspecified,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_and_empty_titles_share_the_untitled_display() {
        assert_eq!(display_title(None), "(untitled)");
        assert_eq!(display_title(Some(String::new())), "(untitled)");
        assert_eq!(display_title(Some("Tide".into())), "Tide");
    }

    #[test]
    fn strip_rejects_geometry_that_overflows_smithays_i32_stride() {
        assert!(build_buffer(&[], 0, i32::MAX).is_none());
    }
}
