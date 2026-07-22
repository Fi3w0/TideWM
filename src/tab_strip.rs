//! Tab-strip UI for a window group: a thin bar along the top of a grouped
//! leaf's rect, one segment per member, the active one visually distinct.
//! Same CPU-composited-texture approach as `toast.rs` (rasterize once into
//! an RGBA buffer, hand it to the renderer as a texture) rather than every
//! frame -- see `Smallvil::tab_strip_elements`, the only caller, for when a
//! group's cached buffer actually gets rebuilt.
//!
//! Titles are read fresh at rasterize time rather than cached per member:
//! a title changing while its window sits parked in a background tab will
//! only show up next time the strip is rebuilt for some other reason
//! (regrouping, cycling, a resize) rather than immediately -- a deliberate
//! v1 simplification. Hooking every client's `title_changed` into this
//! cache is more plumbing than a first pass warrants.

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

/// The title a client has set for `surface`, or a placeholder if it hasn't
/// (or unset it) -- same accessor `ipc.rs`'s `window_json` already uses.
pub fn window_title(surface: &WlSurface) -> String {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()?
            .lock()
            .ok()?
            .title
            .clone()
    })
    .unwrap_or_else(|| "(untitled)".to_string())
}

/// Builds the cached render buffer for a strip `width` wide, one segment
/// per entry in `titles`, `active` highlighted.
pub fn build_buffer(titles: &[String], active: usize, width: i32) -> (MemoryRenderBuffer, i32) {
    let width = width.max(1);
    let mut pixels = vec![0u8; (width * HEIGHT * 4) as usize];
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
    (buffer, width)
}

/// Draws `title` inside the segment spanning `[x0, x0 + seg_w)`, clipped to
/// it -- overflowing glyphs are simply cut off rather than measured and
/// pre-truncated, same effect with far less code.
fn draw_label(pixels: &mut [u8], width: i32, font: &Font, title: &str, x0: i32, seg_w: i32) {
    let baseline = HEIGHT - (HEIGHT - FONT_SIZE as i32) / 2 - 3;
    let mut pen_x = x0 + LABEL_PAD;

    for ch in title.chars() {
        let (metrics, bitmap) = font.rasterize(ch, FONT_SIZE);
        let glyph_x0 = pen_x + metrics.xmin;
        let glyph_y0 = baseline - metrics.ymin - metrics.height as i32;

        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let coverage = bitmap[gy * metrics.width + gx];
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
        // See overview.rs's identical guard for why: a 0-advance-width
        // glyph (zero-width joiners, combining marks, .notdef fallback)
        // would otherwise stall `pen_x` while `glyph_y0` still varies per
        // character, stacking glyphs near the same column instead of a
        // horizontal line.
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
