//! A persistent "fake window" placeholder shown on an otherwise-empty
//! desktop, pointing a first-time user at how to open a terminal --
//! Hyprland's own default-config hint, adapted to this project's existing
//! CPU-composited-texture pattern (same approach as `toast.rs`/
//! `overview.rs`). Deliberately not a fading toast: a brand-new user might
//! not be looking at the exact moment a timed popup would show, so this
//! stays up as long as the desktop is actually empty, not on a timer --
//! and disappears the instant a real window maps, the same way Hyprland's
//! own hint gives way to the first window you open.
//!
//! Built once at startup (`main.rs`, gated on `config.show_welcome_hint`)
//! and never rebuilt -- its content is static. Whether it's drawn at all
//! is decided fresh every frame by the render call sites (`Smallvil::
//! should_show_welcome_hint`): config flag still on, and nothing mapped
//! anywhere in `space`. Dismissed permanently by deleting (or setting
//! `false`) `show_welcome_hint` in config -- checked on every reload, same
//! as any other config value, unlike the one-shot toast this replaced.

use fontdue::Font;
use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    utils::{Physical, Size, Transform},
};

const CARD_W: i32 = 560;
const CARD_H: i32 = 170;
const CARD_RADIUS: f32 = 14.0;
const CARD_BG: (u8, u8, u8, u8) = (22, 30, 34, 235);
const CARD_BORDER: (u8, u8, u8) = (60, 170, 200); // same water-palette accent Toast/Overview use
const BORDER_PX: i32 = 2;
const TITLE_SIZE: f32 = 20.0;
const BODY_SIZE: f32 = 15.0;
const TEXT_RGB: (u8, u8, u8) = (225, 225, 225);
const PAD: i32 = 24;
const LINE_GAP: i32 = 10;

/// The one static card. `render_element` is called every frame the caller
/// decides it should be visible; this struct itself doesn't track that.
pub struct WelcomeHint {
    buffer: MemoryRenderBuffer,
    size: (i32, i32),
}

impl WelcomeHint {
    pub fn build(terminal: &str) -> Self {
        let (width, height) = (CARD_W, CARD_H);
        let mut pixels = vec![0u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                let edge_alpha = rounded_rect_coverage(x, y, width, height, CARD_RADIUS);
                if edge_alpha <= 0.0 {
                    continue;
                }
                let (r, g, b, a) = CARD_BG;
                put_pixel(
                    &mut pixels,
                    width,
                    x,
                    y,
                    (r, g, b, (a as f32 * edge_alpha) as u8),
                );
            }
        }
        stroke_rect(&mut pixels, width, height, BORDER_PX, CARD_BORDER);

        let font = crate::toast::font();
        let canvas = (width, height);
        let mut y = PAD + TITLE_SIZE as i32;
        draw_line(
            &mut pixels,
            canvas,
            font,
            "Welcome to TideWM",
            PAD,
            y,
            TITLE_SIZE,
        );
        y += TITLE_SIZE as i32 + LINE_GAP;
        draw_line(
            &mut pixels,
            canvas,
            font,
            &format!("Super+Enter opens a terminal ({terminal})"),
            PAD,
            y,
            BODY_SIZE,
        );
        y += BODY_SIZE as i32 + LINE_GAP;
        draw_line(
            &mut pixels,
            canvas,
            font,
            "Delete show_welcome_hint from config.wave to dismiss this",
            PAD,
            y,
            BODY_SIZE,
        );

        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (width, height),
            1,
            Transform::Normal,
            None,
        );
        Self {
            buffer,
            size: (width, height),
        }
    }

    /// Centered on whatever output/render area it's drawn into.
    pub fn render_element(
        &self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Physical>,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let location = (
            ((output_size.w - self.size.0) / 2) as f64,
            ((output_size.h - self.size.1) / 2) as f64,
        );
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &self.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }
}

fn stroke_rect(pixels: &mut [u8], width: i32, height: i32, thickness: i32, rgb: (u8, u8, u8)) {
    let (r, g, b) = rgb;
    let inner_w = width - thickness * 2;
    let inner_h = height - thickness * 2;
    let inner_radius = (CARD_RADIUS - thickness as f32).max(0.0);
    for y in 0..height {
        for x in 0..width {
            let outer = rounded_rect_coverage(x, y, width, height, CARD_RADIUS);
            if outer <= 0.0 {
                continue;
            }
            let inside_inner_bounds =
                x >= thickness && y >= thickness && x < width - thickness && y < height - thickness;
            let inner = if inside_inner_bounds {
                rounded_rect_coverage(x - thickness, y - thickness, inner_w, inner_h, inner_radius)
            } else {
                0.0
            };
            if inner <= 0.0 {
                put_pixel(pixels, width, x, y, (r, g, b, (255.0 * outer) as u8));
            }
        }
    }
}

/// Left-aligned text at baseline `(x0, baseline_y)`, clipped to the card's
/// own bounds -- same overflow-is-just-cut-off choice `overview.rs`'s
/// `draw_label` makes.
fn draw_line(
    pixels: &mut [u8],
    canvas: (i32, i32),
    font: &Font,
    text: &str,
    x0: i32,
    baseline_y: i32,
    font_size: f32,
) {
    let (canvas_w, canvas_h) = canvas;
    let mut pen_x = x0;
    for ch in text.chars() {
        if pen_x >= canvas_w - PAD {
            break;
        }
        let (metrics, bitmap) = font.rasterize(ch, font_size);
        let glyph_x0 = pen_x + metrics.xmin;
        let glyph_y0 = baseline_y - metrics.ymin - metrics.height as i32;

        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let coverage = bitmap[gy * metrics.width + gx];
                if coverage == 0 {
                    continue;
                }
                let x = glyph_x0 + gx as i32;
                let y = glyph_y0 + gy as i32;
                if x < 0 || y < 0 || x >= canvas_w || y >= canvas_h {
                    continue;
                }
                blend_text_pixel(pixels, canvas_w, x, y, coverage);
            }
        }
        pen_x += metrics.advance_width.round() as i32;
    }
}

fn rounded_rect_coverage(x: i32, y: i32, width: i32, height: i32, radius: f32) -> f32 {
    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
    let (fw, fh) = (width as f32, height as f32);

    let dx = (fx - fw / 2.0).abs() - (fw / 2.0 - radius);
    let dy = (fy - fh / 2.0).abs() - (fh / 2.0 - radius);

    if dx <= 0.0 || dy <= 0.0 {
        return 1.0;
    }

    let dist = (dx * dx + dy * dy).sqrt();
    (radius - dist + 0.5).clamp(0.0, 1.0)
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
