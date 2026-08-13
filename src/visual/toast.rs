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
    utils::{Buffer as BufferSpace, Logical, Physical, Point, Rectangle, Size, Transform},
};

use crate::animation::Animation;
use crate::config::ToastStyle;

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

// `ToastStyle::Banner`: a long, short bar instead of the pill's tall card,
// with a bottom progress line instead of the pill's orb/wordmark. Own
// geometry, not shared with the pill constants above -- see toast.rs's
// module doc for why the two styles exist.
const BANNER_HEIGHT: i32 = 46;
const BANNER_INSET: i32 = 6;
const BANNER_TEXT_PAD_X: i32 = 20;
const BANNER_PROGRESS_HEIGHT: i32 = 3;
const BANNER_PROGRESS_INSET_X: i32 = 14;
const BANNER_PROGRESS_BOTTOM_GAP: i32 = 6;
const BANNER_MIN_WIDTH: i32 = 320;

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
    message: String,
    kind: ToastKind,
    theme: crate::ui_theme::UiTheme,
    buffer: MemoryRenderBuffer,
    size: (i32, i32),
    /// `ToastStyle::Banner`'s bottom progress-line geometry, if the active
    /// style has one. Each visible frame repaints just this rectangle in
    /// place via `MemoryRenderBuffer::render().draw` (which takes a damage
    /// rect and re-uploads only that part of the texture) instead of
    /// rebuilding the whole buffer -- see `render_element`. `None` for
    /// `Pill`, and for a persistent toast: nothing to animate a countdown
    /// against.
    progress_track: Option<ProgressTrack>,
    layout_output_width: Option<i32>,
    shown_at: Instant,
    /// `None` means this toast never fades on its own -- see `persistent`.
    visible_for: Option<Duration>,
}

impl Toast {
    pub fn new(
        message: &str,
        kind: ToastKind,
        theme: crate::ui_theme::UiTheme,
        narrowest_output_width: Option<i32>,
    ) -> Option<Self> {
        Self::with_duration(
            message,
            kind,
            theme,
            Some(VISIBLE_FOR),
            narrowest_output_width,
        )
    }

    /// Never fades or times out -- stays exactly as shown until its owner
    /// replaces or drops it. Used for the config-reload error toast: the
    /// message can be long (a file path plus a parser error) and the
    /// natural "you're done with this" signal is the *next* reload attempt
    /// replacing it, not a fixed timer someone may not finish reading
    /// before it's gone.
    pub fn persistent(
        message: &str,
        kind: ToastKind,
        theme: crate::ui_theme::UiTheme,
        narrowest_output_width: Option<i32>,
    ) -> Option<Self> {
        Self::with_duration(message, kind, theme, None, narrowest_output_width)
    }

    fn with_duration(
        message: &str,
        kind: ToastKind,
        theme: crate::ui_theme::UiTheme,
        visible_for: Option<Duration>,
        narrowest_output_width: Option<i32>,
    ) -> Option<Self> {
        let message = crate::text::bounded_text(message, crate::ipc::MAX_REQUEST_BYTES);
        // A persistent toast has no countdown to show, so it bakes its
        // banner's progress line as steady/full instead of empty -- see
        // `progress_track`'s own doc comment for why it then tracks nothing
        // further to animate.
        let initial_fill = if visible_for.is_some() { 0.0 } else { 1.0 };
        let (buffer, size, progress_track) =
            build_buffer(&message, kind, theme, narrowest_output_width, initial_fill)?;
        Some(Self {
            message,
            kind,
            theme,
            buffer,
            size,
            progress_track: if visible_for.is_some() {
                progress_track
            } else {
                None
            },
            layout_output_width: narrowest_output_width,
            shown_at: Instant::now(),
            visible_for,
        })
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

    pub fn expired(&self) -> bool {
        self.visible_for.is_some_and(|visible_for| {
            self.shown_at.elapsed() >= visible_for.saturating_add(FADE_FOR)
        })
    }

    /// The render element for this toast, anchored to the top-right of the
    /// live logical output geometry. `None` can mean expired, temporarily too
    /// narrow, or an import failure; callers use `expired()` to decide whether
    /// to drop the toast globally.
    pub fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        logical_output_width: i32,
        narrowest_output_width: Option<i32>,
        scale: f64,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        if self.layout_output_width != narrowest_output_width {
            let initial_fill = if self.visible_for.is_some() { 0.0 } else { 1.0 };
            let (buffer, size, progress_track) = build_buffer(
                &self.message,
                self.kind,
                self.theme,
                narrowest_output_width,
                initial_fill,
            )?;
            self.buffer = buffer;
            self.size = size;
            self.progress_track = if self.visible_for.is_some() {
                progress_track
            } else {
                None
            };
            self.layout_output_width = narrowest_output_width;
        }
        // Live countdown: repaint just the progress-line rectangle in place
        // (damage-scoped re-upload, not a whole-buffer rebuild) instead of
        // re-rasterizing text/background, so a Banner toast's bottom line
        // actually fills as it counts down.
        if let (Some(track), Some(visible_for)) = (self.progress_track, self.visible_for) {
            let fraction = Animation::new(0.0, 1.0, self.shown_at, visible_for).value();
            let accent = self.theme.popup_accent(self.kind == ToastKind::Error, 0.5);
            let filled = ((track.w as f32) * fraction).round().max(0.0) as i32;
            let width = self.size.0;
            let _ = self.buffer.render().draw::<_, ()>(|mem| {
                stamp_progress_fill(mem, width, track, accent, fraction);
                Ok(vec![Rectangle::<i32, BufferSpace>::new(
                    (track.x, track.y).into(),
                    (filled, track.h).into(),
                )])
            });
        }
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

        let (x, y) = toast_location(logical_output_width, self.size.0, scale)?;
        let location: Point<f64, Physical> = (x, y).into();

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

fn toast_location(logical_output_width: i32, toast_width: i32, scale: f64) -> Option<(f64, f64)> {
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let right_inset = toast_width.checked_add(MARGIN)?;
    let logical_x = logical_output_width.checked_sub(right_inset)?;
    if logical_x < MARGIN {
        return None;
    }
    Some((f64::from(logical_x) * scale, f64::from(MARGIN) * scale))
}

/// A style's rasterized canvas, plus (`Banner` only) where its live
/// progress line sits so a redraw can restamp just that rectangle.
struct Rasterized {
    pixels: Vec<u8>,
    width: i32,
    height: i32,
    progress_track: Option<ProgressTrack>,
}

#[derive(Debug, Clone, Copy)]
struct ProgressTrack {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// `(texture, its size, Banner's progress-line geometry if the style has
/// one)`.
type BuiltBuffer = (MemoryRenderBuffer, (i32, i32), Option<ProgressTrack>);

/// Rasterizes and uploads a fresh texture for the toast's current style,
/// message, and theme.
fn build_buffer(
    message: &str,
    kind: ToastKind,
    theme: crate::ui_theme::UiTheme,
    narrowest_output_width: Option<i32>,
    fill: f32,
) -> Option<BuiltBuffer> {
    let rasterized =
        rasterize_toast_for_output(message, kind, theme, narrowest_output_width, fill)?;
    let buffer = MemoryRenderBuffer::from_slice(
        &rasterized.pixels,
        Fourcc::Argb8888,
        (rasterized.width, rasterized.height),
        1,
        Transform::Normal,
        None,
    );
    let size = (rasterized.width, rasterized.height);
    Some((buffer, size, rasterized.progress_track))
}

/// Composites the toast into a straight-alpha ARGB8888 buffer: a rounded-rect
/// background with a 1px antialiased edge, then the message text on top.
#[cfg(test)]
fn rasterize_toast(
    message: &str,
    kind: ToastKind,
    theme: crate::ui_theme::UiTheme,
) -> Option<(Vec<u8>, i32, i32)> {
    rasterize_toast_for_output(message, kind, theme, None, 0.0)
        .map(|r| (r.pixels, r.width, r.height))
}

/// Dispatches to the configured style's own rasterizer. `fill` only matters
/// for `Banner` (its live progress line's current 0.0-1.0 fraction);
/// `Pill` ignores it.
fn rasterize_toast_for_output(
    message: &str,
    kind: ToastKind,
    theme: crate::ui_theme::UiTheme,
    narrowest_output_width: Option<i32>,
    fill: f32,
) -> Option<Rasterized> {
    match theme.style {
        ToastStyle::Pill => {
            rasterize_pill_for_output(message, kind, theme, narrowest_output_width).map(
                |(pixels, width, height)| Rasterized {
                    pixels,
                    width,
                    height,
                    progress_track: None,
                },
            )
        }
        ToastStyle::Banner => {
            rasterize_banner_for_output(message, kind, theme, narrowest_output_width, fill)
        }
    }
}

fn rasterize_pill_for_output(
    message: &str,
    kind: ToastKind,
    theme: crate::ui_theme::UiTheme,
    narrowest_output_width: Option<i32>,
) -> Option<(Vec<u8>, i32, i32)> {
    let font = font();
    let font_size = kind.font_size();
    let height = CARD_HEIGHT + CARD_INSET * 2;
    let chrome_width = ICON_WIDTH + TEXT_RIGHT_PAD + CARD_INSET * 2;
    let available_width = narrowest_output_width
        .map(|width| width.saturating_sub(MARGIN * 2))
        .unwrap_or(MIN_WIDTH);
    if available_width < chrome_width {
        return None;
    }
    let width_limit = available_width;
    crate::text::checked_argb_len(width_limit, height)?;
    let text_limit = width_limit.saturating_sub(chrome_width);
    let line = crate::text::rasterize_line(font, message, font_size, text_limit);
    let width = line
        .advance
        .saturating_add(chrome_width)
        .max(MIN_WIDTH.min(width_limit))
        .min(width_limit);
    let pixel_len = crate::text::checked_argb_len(width, height)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(pixel_len).ok()?;
    pixels.resize(pixel_len, 0);

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

    Some((pixels, width, height))
}

/// `ToastStyle::Banner`: a long, short bar (no orb, no "TIDEWM" label --
/// the tinted border already reads as Tide chrome, and a shorter box has
/// less room for both a caption and a message anyway) with a bottom
/// progress line that fills in as `fill` (0.0-1.0) grows. Same background
/// gradient, border-stroke, and rounded-corner recipe as the pill so the
/// two styles read as one family, just reshaped -- not a port of
/// Hyprland's own notification, which slides a second rect in over the
/// first to fake a colored left edge and has no configurable corners at
/// all.
fn rasterize_banner_for_output(
    message: &str,
    kind: ToastKind,
    theme: crate::ui_theme::UiTheme,
    narrowest_output_width: Option<i32>,
    fill: f32,
) -> Option<Rasterized> {
    let font = font();
    let font_size = kind.font_size();
    let height = BANNER_HEIGHT + BANNER_INSET * 2;
    let chrome_width = BANNER_TEXT_PAD_X * 2 + BANNER_INSET * 2;
    let available_width = narrowest_output_width
        .map(|width| width.saturating_sub(MARGIN * 2))
        .unwrap_or(BANNER_MIN_WIDTH);
    if available_width < chrome_width {
        return None;
    }
    let width_limit = available_width;
    crate::text::checked_argb_len(width_limit, height)?;
    let text_limit = width_limit.saturating_sub(chrome_width);
    let line = crate::text::rasterize_line(font, message, font_size, text_limit);
    let width = line
        .advance
        .saturating_add(chrome_width)
        .max(BANNER_MIN_WIDTH.min(width_limit))
        .min(width_limit);
    let pixel_len = crate::text::checked_argb_len(width, height)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(pixel_len).ok()?;
    pixels.resize(pixel_len, 0);

    let card_x = BANNER_INSET;
    let card_y = BANNER_INSET;
    let card_w = width - BANNER_INSET * 2;
    let radius = theme.radius.min(BANNER_HEIGHT / 2) as f32;
    let accent = theme.popup_accent(kind == ToastKind::Error, 0.5);

    for y in 0..height {
        for x in 0..width {
            let shadow = rounded_rect_coverage_local(
                x,
                y,
                card_x - 3,
                card_y + 3,
                card_w + 6,
                BANNER_HEIGHT,
                radius + 3.0,
            );
            if shadow > 0.0 {
                put_pixel(&mut pixels, width, x, y, (0, 0, 0, (70.0 * shadow) as u8));
            }
            let coverage =
                rounded_rect_coverage_local(x, y, card_x, card_y, card_w, BANNER_HEIGHT, radius);
            if coverage <= 0.0 {
                continue;
            }
            let t = ((y - card_y) as f32 / BANNER_HEIGHT as f32).clamp(0.0, 1.0);
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
                    BANNER_HEIGHT - stroke * 2,
                    (radius - stroke as f32).max(1.0),
                );
            if border > 0.0 {
                blend_color_pixel(&mut pixels, width, x, y, accent, (border * 235.0) as u8);
            }
        }
    }

    let track = ProgressTrack {
        x: card_x + BANNER_PROGRESS_INSET_X,
        y: card_y + BANNER_HEIGHT - BANNER_PROGRESS_BOTTOM_GAP - BANNER_PROGRESS_HEIGHT,
        w: (card_w - BANNER_PROGRESS_INSET_X * 2).max(0),
        h: BANNER_PROGRESS_HEIGHT,
    };
    // Faint resting groove, so the track reads even at 0% fill -- then
    // `stamp_progress_fill` overlays a brighter run of the same accent on
    // top as `fill` grows.
    for y in track.y..track.y + track.h {
        for x in track.x..track.x + track.w {
            blend_color_pixel(&mut pixels, width, x, y, accent, 55);
        }
    }
    stamp_progress_fill(&mut pixels, width, track, accent, fill);

    let baseline = card_y + 28;
    let mut pen_x = card_x + BANNER_TEXT_PAD_X;

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
                if x < 0 || y < 0 || x >= width || y >= height {
                    continue;
                }
                blend_color_pixel(&mut pixels, width, x, y, theme.text, coverage);
            }
        }

        pen_x += metrics.advance_width.round().max(1.0) as i32;
    }

    Some(Rasterized {
        pixels,
        width,
        height,
        progress_track: Some(track),
    })
}

/// Paints the filled portion of a `Banner` toast's bottom line, growing
/// left-to-right from `track`'s own start. `fill` only ever grows for a
/// given toast (see `Toast::render_element`), so repainting `0..filled`
/// from scratch each call is idempotent -- nothing to erase when it moves.
/// Flat rect, no antialiasing: it's a 3px line already inset from the
/// card's rounded corners.
fn stamp_progress_fill(
    pixels: &mut [u8],
    width: i32,
    track: ProgressTrack,
    accent: [u8; 3],
    fill: f32,
) {
    let filled = ((track.w as f32) * fill.clamp(0.0, 1.0)).round() as i32;
    for y in track.y..track.y + track.h {
        for x in track.x..track.x + filled {
            put_pixel(pixels, width, x, y, (accent[0], accent[1], accent[2], 255));
        }
    }
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

    // See the identical fix in `error_overlay::rounded_rect_coverage`'s own
    // doc comment: floor each axis's flat-region distance at zero instead
    // of short-circuiting to "fully inside" the moment either axis alone
    // is flat, or a point far outside on one axis read as fully covered as
    // long as the other axis was in its flat middle.
    let dx = ((fx - fw / 2.0).abs() - (fw / 2.0 - radius)).max(0.0);
    let dy = ((fy - fh / 2.0).abs() - (fh / 2.0 - radius)).max(0.0);

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

    /// Same regression as `error_overlay`'s identical fix: a point far
    /// outside the pill on the y axis, but in the flat (non-corner) x
    /// region, must read as uncovered rather than the old short-circuit's
    /// "flat on one axis means fully inside".
    #[test]
    fn coverage_is_zero_far_outside_the_shape_even_in_the_flat_region() {
        let (left, top, width, height, radius) = (6, 6, 300, 70, 14.0);
        let x_mid = left + width / 2;
        assert_eq!(
            rounded_rect_coverage_local(x_mid, 0, left, top, width, height, radius),
            0.0
        );
        assert_eq!(
            rounded_rect_coverage_local(x_mid, top + height / 2, left, top, width, height, radius),
            1.0
        );
    }

    #[test]
    fn only_timed_toasts_request_animation_frames() {
        let theme = crate::ui_theme::UiTheme::for_test();
        assert!(Toast::new("ok", ToastKind::Info, theme, None)
            .unwrap()
            .needs_continued_redraw());
        assert!(!Toast::persistent("error", ToastKind::Error, theme, None)
            .unwrap()
            .needs_continued_redraw());
    }

    #[test]
    fn themed_card_raster_has_visible_panel_border_and_text() {
        let (pixels, width, height) = rasterize_toast(
            "Configuration reloaded",
            ToastKind::Info,
            crate::ui_theme::UiTheme::for_test(),
        )
        .unwrap();
        assert!(width >= MIN_WIDTH);
        assert_eq!(height, CARD_HEIGHT + CARD_INSET * 2);
        let visible = pixels.chunks_exact(4).filter(|pixel| pixel[3] > 0).count();
        assert!(visible > (width * CARD_HEIGHT / 2) as usize);
    }

    #[test]
    fn multi_megabyte_toast_is_bounded_by_live_logical_width() {
        let message = "current ".repeat(256 * 1024);
        let output_width = 911;
        let raster = rasterize_toast_for_output(
            &message,
            ToastKind::Error,
            crate::ui_theme::UiTheme::for_test(),
            Some(output_width),
            0.0,
        )
        .unwrap();

        assert!(raster.width <= output_width - MARGIN * 2);
        assert_eq!(
            raster.pixels.len(),
            (raster.width * raster.height * 4) as usize
        );
    }

    #[test]
    fn toast_position_scales_from_logical_geometry() {
        let logical_width = 913;
        let toast_width = 271;
        let logical = toast_location(logical_width, toast_width, 1.0).unwrap();
        let scaled = toast_location(logical_width, toast_width, 1.75).unwrap();

        assert_eq!(scaled.0, logical.0 * 1.75);
        assert_eq!(scaled.1, logical.1 * 1.75);
        assert!(scaled.0 >= 0.0);
    }

    #[test]
    fn toast_skips_geometry_too_narrow_for_its_chrome() {
        let chrome_width = ICON_WIDTH + TEXT_RIGHT_PAD + CARD_INSET * 2;
        let output_width = MARGIN * 2 + chrome_width - 1;
        assert!(rasterize_toast_for_output(
            "message",
            ToastKind::Info,
            crate::ui_theme::UiTheme::for_test(),
            Some(output_width),
            0.0,
        )
        .is_none());
        assert!(toast_location(output_width, chrome_width, 1.0).is_none());
    }

    fn banner_theme() -> crate::ui_theme::UiTheme {
        crate::ui_theme::UiTheme::for_test_with_style(ToastStyle::Banner)
    }

    #[test]
    fn banner_raster_is_a_short_wide_bar_with_visible_border_and_text() {
        let raster = rasterize_toast_for_output(
            "Migrated live windows to the ocean engine",
            ToastKind::Info,
            banner_theme(),
            None,
            0.0,
        )
        .unwrap();
        assert_eq!(raster.height, BANNER_HEIGHT + BANNER_INSET * 2);
        assert!(raster.width > raster.height * 2, "banner should read long, not squarish");
        assert!(raster.progress_track.is_some());
        let visible = raster
            .pixels
            .chunks_exact(4)
            .filter(|pixel| pixel[3] > 0)
            .count();
        assert!(visible > 0);
    }

    #[test]
    fn banner_progress_line_grows_with_fill_then_disappears_with_the_toast() {
        let theme = banner_theme();
        let empty = rasterize_toast_for_output("Configuration reloaded", ToastKind::Info, theme, None, 0.0)
            .unwrap();
        let half = rasterize_toast_for_output("Configuration reloaded", ToastKind::Info, theme, None, 0.5)
            .unwrap();
        let full = rasterize_toast_for_output("Configuration reloaded", ToastKind::Info, theme, None, 1.0)
            .unwrap();
        let track = empty.progress_track.unwrap();

        let filled_pixels = |raster: &Rasterized| {
            (track.x..track.x + track.w)
                .filter(|&x| {
                    let i = ((track.y * raster.width + x) * 4) as usize;
                    raster.pixels[i + 3] == 255
                })
                .count()
        };
        let empty_count = filled_pixels(&empty);
        let half_count = filled_pixels(&half);
        let full_count = filled_pixels(&full);
        assert!(empty_count < half_count);
        assert!(half_count < full_count);
        assert_eq!(full_count, track.w as usize);

        let toast = Toast::new("Configuration reloaded", ToastKind::Info, theme, None).unwrap();
        assert!(!toast.expired());
    }

    #[test]
    fn pill_style_stays_the_default_and_ignores_fill() {
        assert_eq!(crate::ui_theme::UiTheme::for_test().style, ToastStyle::Pill);
        let a = rasterize_toast_for_output(
            "Configuration reloaded",
            ToastKind::Info,
            crate::ui_theme::UiTheme::for_test(),
            None,
            0.0,
        )
        .unwrap();
        let b = rasterize_toast_for_output(
            "Configuration reloaded",
            ToastKind::Info,
            crate::ui_theme::UiTheme::for_test(),
            None,
            1.0,
        )
        .unwrap();
        assert!(a.progress_track.is_none());
        assert_eq!(a.pixels, b.pixels);
    }
}
