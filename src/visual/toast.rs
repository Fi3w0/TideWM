//! TideWM's small first-party on-screen toast, used right now to confirm a config
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
    utils::{Logical, Physical, Point, Size, Transform},
};

use crate::animation::Animation;

const FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/AdwaitaSans-Regular.ttf");
const FONT_SIZE: f32 = 15.0;
const LABEL_SIZE: f32 = 9.5;
// Errors carry a lot more text (a file path plus a parser message) than the
// one-line "Config reloaded" confirmation -- a smaller size keeps a long
// error legible without the pill ballooning to an unreasonable width at the
// same size ordinary short toasts use.
const ERROR_FONT_SIZE: f32 = 12.0;
const MARGIN: i32 = 24;
/// The pill's card height. Exposed so sibling compositor UI can scale
/// their corner radius from the same reference: a surface taller than a
/// toast needs proportionally larger corners, or the rounding reads as
/// "sized for a tiny toast" on a bigger panel.
pub(crate) const CARD_HEIGHT: i32 = 70;
const CARD_INSET: i32 = 6;
const ICON_WIDTH: i32 = 46;
const TEXT_RIGHT_PAD: i32 = 22;
const MIN_WIDTH: i32 = 270;

const VISIBLE_FOR: Duration = Duration::from_millis(2400);
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
    pub fn new(message: &str, kind: ToastKind, theme: crate::ui_theme::UiTheme) -> Self {
        Self::with_duration(message, kind, theme, Some(VISIBLE_FOR))
    }

    /// Never fades or times out -- stays exactly as shown until its owner
    /// replaces or drops it. Used for the config-reload error toast: the
    /// message can be long (a file path plus a parser error) and the
    /// natural "you're done with this" signal is the *next* reload attempt
    /// replacing it, not a fixed timer someone may not finish reading
    /// before it's gone.
    pub fn persistent(message: &str, kind: ToastKind, theme: crate::ui_theme::UiTheme) -> Self {
        Self::with_duration(message, kind, theme, None)
    }

    fn with_duration(
        message: &str,
        kind: ToastKind,
        theme: crate::ui_theme::UiTheme,
        visible_for: Option<Duration>,
    ) -> Self {
        let (pixels, width, height) = rasterize_toast(message, kind, theme);
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

        let location: Point<f64, Physical> =
            ((output_size.w - self.size.0 - MARGIN) as f64, MARGIN as f64).into();

        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &self.buffer,
            Some(alpha),
            None,
            Some(Size::<i32, Logical>::from(self.size)),
            Kind::Unspecified,
        ) {
            Ok(element) => Some(element),
            Err(err) => {
                tracing::warn!(?err, "Failed to import TideWM toast texture");
                None
            }
        }
    }
}

/// Composites the toast into a straight-alpha ARGB8888 buffer: a rounded-rect
/// background with a 1px antialiased edge, then the message text on top.
fn rasterize_toast(
    message: &str,
    kind: ToastKind,
    theme: crate::ui_theme::UiTheme,
) -> (Vec<u8>, i32, i32) {
    let font = font();
    let font_size = kind.font_size();
    let mut glyphs = Vec::with_capacity(message.len());
    let mut text_width = 0.0f32;
    for ch in message.chars() {
        let (metrics, bitmap) = font.rasterize(ch, font_size);
        text_width += metrics.advance_width;
        glyphs.push((metrics, bitmap));
    }

    let width =
        (text_width.ceil() as i32 + ICON_WIDTH + TEXT_RIGHT_PAD + CARD_INSET * 2).max(MIN_WIDTH);
    let height = CARD_HEIGHT + CARD_INSET * 2;
    let mut pixels = vec![0u8; (width * height * 4) as usize];

    let card_x = CARD_INSET;
    let card_y = CARD_INSET;
    let card_w = width - CARD_INSET * 2;
    let radius = theme.radius.min(CARD_HEIGHT / 2) as f32;
    for y in 0..height {
        for x in 0..width {
            let shadow = rounded_rect_coverage_local(
                x,
                y,
                card_x - 3,
                card_y + 3,
                card_w + 6,
                CARD_HEIGHT,
                radius + 3.0,
            );
            if shadow > 0.0 {
                put_pixel(&mut pixels, width, x, y, (0, 0, 0, (70.0 * shadow) as u8));
            }
            let coverage =
                rounded_rect_coverage_local(x, y, card_x, card_y, card_w, CARD_HEIGHT, radius);
            if coverage <= 0.0 {
                continue;
            }
            let t = ((y - card_y) as f32 / CARD_HEIGHT as f32).clamp(0.0, 1.0);
            let bg = crate::ui_theme::mix(theme.panel_from, theme.panel_to, t);
            put_pixel(
                &mut pixels,
                width,
                x,
                y,
                (bg[0], bg[1], bg[2], (242.0 * coverage) as u8),
            );
            let stroke = theme.border_width.round().max(1.0) as i32;
            let border = coverage
                - rounded_rect_coverage_local(
                    x,
                    y,
                    card_x + stroke,
                    card_y + stroke,
                    card_w - stroke * 2,
                    CARD_HEIGHT - stroke * 2,
                    (radius - stroke as f32).max(1.0),
                );
            if border > 0.0 {
                let accent = theme.popup_accent(kind == ToastKind::Error, x as f32 / width as f32);
                blend_color_pixel(&mut pixels, width, x, y, accent, (border * 235.0) as u8);
            }
        }
    }

    // Small themed current/orb: recognizable Tide chrome without baking a
    // logo bitmap into every notification texture.
    let accent = theme.accent(kind == ToastKind::Error, 0.35);
    let center = (card_x + 23, card_y + CARD_HEIGHT / 2);
    for y in center.1 - 11..=center.1 + 11 {
        for x in center.0 - 11..=center.0 + 11 {
            let distance = (((x - center.0).pow(2) + (y - center.1).pow(2)) as f32).sqrt();
            let coverage = (11.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend_color_pixel(&mut pixels, width, x, y, accent, (coverage * 225.0) as u8);
            }
        }
    }
    for x in center.0 - 7..=center.0 + 7 {
        let phase = (x - (center.0 - 7)) as f32 / 14.0 * std::f32::consts::TAU;
        let y = center.1 + (phase.sin() * 2.0).round() as i32;
        blend_color_pixel(&mut pixels, width, x, y, theme.text, 220);
    }

    draw_text(
        &mut pixels,
        (width, height),
        "TIDEWM",
        card_x + ICON_WIDTH,
        card_y + 22,
        LABEL_SIZE,
        theme.muted_text,
    );

    let baseline = card_y + 48;
    let mut pen_x = card_x + ICON_WIDTH;

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
                blend_color_pixel(&mut pixels, width, x, y, theme.text, coverage);
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
/// corners so the pill doesn't look jagged. `pub(crate)`: also reused by
/// `crate::minimap`'s decorated presets for rounded window/viewport boxes
/// and their soft glow/shadow rims.
pub(crate) fn rounded_rect_coverage_local(
    x: i32,
    y: i32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    radius: f32,
) -> f32 {
    let (fx, fy) = ((x - left) as f32 + 0.5, (y - top) as f32 + 0.5);
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

fn blend_color_pixel(pixels: &mut [u8], width: i32, x: i32, y: i32, rgb: [u8; 3], coverage: u8) {
    let i = ((y * width + x) * 4) as usize;
    let t = coverage as f32 / 255.0;
    let [r, g, b] = rgb;
    for (channel, target) in [b, g, r].into_iter().enumerate() {
        let bg = pixels[i + channel] as f32;
        pixels[i + channel] = (bg + (target as f32 - bg) * t) as u8;
    }
    pixels[i + 3] = pixels[i + 3].max(coverage);
}

fn draw_text(
    pixels: &mut [u8],
    canvas: (i32, i32),
    text: &str,
    x0: i32,
    baseline: i32,
    size: f32,
    rgb: [u8; 3],
) {
    let (width, height) = canvas;
    let mut pen_x = x0;
    for ch in text.chars() {
        let (metrics, bitmap) = font().rasterize(ch, size);
        let glyph_x = pen_x + metrics.xmin;
        let glyph_y = baseline - metrics.ymin - metrics.height as i32;
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let coverage = bitmap[gy * metrics.width + gx];
                let x = glyph_x + gx as i32;
                let y = glyph_y + gy as i32;
                if coverage > 0 && x >= 0 && y >= 0 && x < width && y < height {
                    blend_color_pixel(pixels, width, x, y, rgb, coverage);
                }
            }
        }
        pen_x += metrics.advance_width.round().max(1.0) as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_timed_toasts_request_animation_frames() {
        let theme = crate::ui_theme::UiTheme::for_test();
        assert!(Toast::new("ok", ToastKind::Info, theme).needs_continued_redraw());
        assert!(!Toast::persistent("error", ToastKind::Error, theme).needs_continued_redraw());
    }

    #[test]
    fn themed_card_raster_has_visible_panel_border_and_text() {
        let (pixels, width, height) = rasterize_toast(
            "Configuration reloaded",
            ToastKind::Info,
            crate::ui_theme::UiTheme::for_test(),
        );
        assert!(width >= MIN_WIDTH);
        assert_eq!(height, CARD_HEIGHT + CARD_INSET * 2);
        let visible = pixels.chunks_exact(4).filter(|pixel| pixel[3] > 0).count();
        assert!(visible > (width * CARD_HEIGHT / 2) as usize);
    }
}
