//! A small first-party on-screen toast, used right now to confirm a config
//! hot-reload. CPU-composited once into an RGBA buffer (background pill +
//! rasterized text), then handed to the renderer as a single texture. No
//! layer-shell/notification-daemon dependency: this is TideWM's own UI.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use fontdue::{Font, FontSettings};
use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    utils::{Physical, Size, Transform},
};

use crate::animation::Animation;

const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/AdwaitaSans-Regular.ttf");
const FONT_SIZE: f32 = 15.0;
// Errors carry a lot more text (a file path plus a parser message) than the
// one-line "Config reloaded" confirmation -- a smaller size keeps a long
// error legible without the pill ballooning to an unreasonable width at the
// same size ordinary short toasts use.
const ERROR_FONT_SIZE: f32 = 12.0;
const PADDING_X: i32 = 18;
const PADDING_Y: i32 = 12;
const MARGIN: i32 = 24;
const CORNER_RADIUS: f32 = 12.0;

const VISIBLE_FOR: Duration = Duration::from_millis(1800);
const FADE_FOR: Duration = Duration::from_millis(450);

/// Shared with `tab_strip.rs`, which composites text the same way.
pub(crate) fn font() -> &'static Font {
    static FONT: OnceLock<Font> = OnceLock::new();
    FONT.get_or_init(|| {
        Font::from_bytes(FONT_BYTES, FontSettings::default()).expect("bundled font is valid")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

impl ToastKind {
    /// Errors carry much longer messages than a routine confirmation --
    /// see `ERROR_FONT_SIZE`'s own doc comment.
    fn font_size(self) -> f32 {
        match self {
            ToastKind::Info => FONT_SIZE,
            ToastKind::Error => ERROR_FONT_SIZE,
        }
    }

    /// Background tint, RGB. Kept in the water/aqua palette for info; a warm
    /// tone for errors so a broken config is obviously not the happy path.
    fn rgb(self) -> (u8, u8, u8) {
        match self {
            ToastKind::Info => (28, 94, 116),
            ToastKind::Error => (140, 58, 58),
        }
    }
}

pub struct Toast {
    #[cfg(feature = "accessibility")]
    message: String,
    #[cfg(feature = "accessibility")]
    kind: ToastKind,
    buffer: MemoryRenderBuffer,
    size: (i32, i32),
    shown_at: Instant,
    /// `None` means this toast never fades on its own -- see `persistent`.
    visible_for: Option<Duration>,
}

impl Toast {
    pub fn new(message: &str, kind: ToastKind) -> Self {
        Self::with_duration(message, kind, Some(VISIBLE_FOR))
    }

    /// Never fades or times out -- stays exactly as shown until its owner
    /// replaces or drops it. Used for the config-reload error toast: the
    /// message can be long (a file path plus a parser error) and the
    /// natural "you're done with this" signal is the *next* reload attempt
    /// replacing it, not a fixed timer someone may not finish reading
    /// before it's gone.
    pub fn persistent(message: &str, kind: ToastKind) -> Self {
        Self::with_duration(message, kind, None)
    }

    fn with_duration(message: &str, kind: ToastKind, visible_for: Option<Duration>) -> Self {
        let (pixels, width, height) = rasterize_toast(message, kind);
        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (width, height),
            1,
            Transform::Normal,
            None,
        );
        Self {
            #[cfg(feature = "accessibility")]
            message: message.to_owned(),
            #[cfg(feature = "accessibility")]
            kind,
            buffer,
            size: (width, height),
            shown_at: Instant::now(),
            visible_for,
        }
    }

    /// Text and urgency retained alongside the render buffer so optional
    /// accessibility support can expose the same notification to AT-SPI.
    #[cfg(feature = "accessibility")]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    #[cfg(feature = "accessibility")]
    pub(crate) fn kind(&self) -> ToastKind {
        self.kind
    }

    /// Whether the caller should keep re-requesting a redraw purely to give
    /// this toast another frame, even though nothing else went dirty in the
    /// meantime -- true for an ordinary timed toast for its whole
    /// visible-then-fading lifetime, always false for a `persistent` one.
    /// A persistent toast's pixels never change after the first render (its
    /// alpha is a flat 1.0 forever), so there is nothing later to redraw
    /// *for* -- without this, the per-tick "a toast is showing" redraw loop
    /// both backends run would spin forever instead of stopping once the
    /// toast settles, burning CPU/GPU on frames that look identical to the
    /// one already on screen.
    pub fn needs_continued_redraw(&self) -> bool {
        self.visible_for.is_some()
    }

    /// The render element for this toast, anchored to the top-right of
    /// `output_size`, or `None` once it has fully faded out (the caller
    /// should drop the `Toast` at that point) -- never for a `persistent`
    /// one, which has no fade to reach.
    pub fn render_element(
        &self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Physical>,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let alpha = match self.visible_for {
            None => 1.0,
            Some(visible_for) => {
                // Anchored at `shown_at + visible_for`, not "now" -- holds
                // at 1.0 (its own `from`) until that instant arrives, same
                // hold-then-fade shape the old elapsed-time branches had.
                let fade = Animation::new(1.0, 0.0, self.shown_at + visible_for, FADE_FOR);
                if fade.finished() {
                    return None;
                }
                fade.value()
            }
        };

        let location = ((output_size.w - self.size.0 - MARGIN) as f64, MARGIN as f64);

        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &self.buffer,
            Some(alpha),
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }
}

/// Composites the toast into a straight-alpha ARGB8888 buffer: a rounded-rect
/// background with a 1px antialiased edge, then the message text on top.
fn rasterize_toast(message: &str, kind: ToastKind) -> (Vec<u8>, i32, i32) {
    let font = font();
    let font_size = kind.font_size();
    let line_height = font_size * 1.3;

    let mut glyphs = Vec::with_capacity(message.len());
    let mut text_width = 0.0f32;
    for ch in message.chars() {
        let (metrics, bitmap) = font.rasterize(ch, font_size);
        text_width += metrics.advance_width;
        glyphs.push((metrics, bitmap));
    }

    let width = (text_width.ceil() as i32 + PADDING_X * 2).max(PADDING_X * 2 + 1);
    let height = (line_height.ceil() as i32) + PADDING_Y * 2;
    let mut pixels = vec![0u8; (width * height * 4) as usize];

    let (bg_r, bg_g, bg_b) = kind.rgb();
    let bg_alpha = 235u8;

    for y in 0..height {
        for x in 0..width {
            let edge_alpha = rounded_rect_coverage(x, y, width, height, CORNER_RADIUS);
            if edge_alpha <= 0.0 {
                continue;
            }
            let alpha = (bg_alpha as f32 * edge_alpha) as u8;
            put_pixel(&mut pixels, width, x, y, (bg_r, bg_g, bg_b, alpha));
        }
    }

    // Baseline sits one line-height down from the top padding, leaving a small
    // ascent/descent margin either side rather than kissing the pill's edge.
    let baseline = PADDING_Y + (line_height * 0.8) as i32;
    let mut pen_x = PADDING_X;

    for (metrics, bitmap) in &glyphs {
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
                if x < 0 || y < 0 || x >= width || y >= height {
                    continue;
                }
                blend_text_pixel(&mut pixels, width, x, y, coverage);
            }
        }

        // See overview.rs's identical guard for why the `.max(1.0)` floor
        // matters: a 0-advance-width glyph would otherwise stall `pen_x`
        // while `glyph_y0` still varies per character.
        pen_x += metrics.advance_width.round().max(1.0) as i32;
    }

    (pixels, width, height)
}

/// 1.0 fully inside, 0.0 fully outside, feathered over ~1px at the rounded
/// corners so the pill doesn't look jagged.
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
    for channel in 0..3 {
        let bg = pixels[i + channel] as f32;
        pixels[i + channel] = (bg + (255.0 - bg) * t) as u8;
    }
    pixels[i + 3] = pixels[i + 3].max(coverage);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_timed_toasts_request_animation_frames() {
        assert!(Toast::new("ok", ToastKind::Info).needs_continued_redraw());
        assert!(!Toast::persistent("error", ToastKind::Error).needs_continued_redraw());
    }
}
