//! Bounded one-line text rasterization shared by compositor-owned UI.

use fontdue::{Font, Metrics};

pub(crate) struct RasterizedGlyph {
    #[cfg(test)]
    pub character: char,
    pub metrics: Metrics,
    pub bitmap: Vec<u8>,
    advance: i32,
}

pub(crate) struct RasterizedLine {
    pub glyphs: Vec<RasterizedGlyph>,
    pub advance: i32,
    #[cfg(test)]
    pub truncated: bool,
}

fn rasterize_glyph(font: &Font, character: char, font_size: f32) -> RasterizedGlyph {
    let (metrics, bitmap) = font.rasterize(character, font_size);
    let advance = metrics.advance_width.round().max(1.0).min(i32::MAX as f32) as i32;
    RasterizedGlyph {
        #[cfg(test)]
        character,
        metrics,
        bitmap,
        advance,
    }
}

/// Rasterizes at most the glyphs that fit in `max_advance`, plus one glyph
/// needed to discover truncation. Every glyph consumes at least one pixel of
/// advance, so both work and retained bitmaps are bounded by the live clip.
pub(crate) fn rasterize_line(
    font: &Font,
    text: &str,
    font_size: f32,
    max_advance: i32,
) -> RasterizedLine {
    let max_advance = max_advance.max(0);
    let mut glyphs = Vec::new();
    let mut advance = 0_i32;
    let mut truncated = false;

    for character in text.chars() {
        let glyph = rasterize_glyph(font, character, font_size);
        if advance.saturating_add(glyph.advance) > max_advance {
            truncated = true;
            break;
        }
        advance += glyph.advance;
        glyphs.push(glyph);
    }

    if truncated {
        let ellipsis = rasterize_glyph(font, '\u{2026}', font_size);
        while advance.saturating_add(ellipsis.advance) > max_advance {
            let Some(removed) = glyphs.pop() else {
                break;
            };
            advance -= removed.advance;
        }
        if advance.saturating_add(ellipsis.advance) <= max_advance {
            advance += ellipsis.advance;
            glyphs.push(ellipsis);
        }
    }

    RasterizedLine {
        glyphs,
        advance,
        #[cfg(test)]
        truncated,
    }
}

/// Byte length of a tightly packed ARGB8888 buffer, using the same i32
/// multiplication Smithay performs internally before converting to usize.
pub(crate) fn checked_argb_len(width: i32, height: i32) -> Option<usize> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let stride = width.checked_mul(4)?;
    let bytes = stride.checked_mul(height)?;
    usize::try_from(bytes).ok()
}

/// Copies at most `max_bytes` of UTF-8 and marks truncation without ever
/// slicing through a codepoint.
pub(crate) fn bounded_text(text: &str, max_bytes: usize) -> String {
    const ELLIPSIS: &str = "\u{2026}";
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    if max_bytes < ELLIPSIS.len() {
        return String::new();
    }

    let mut end = max_bytes - ELLIPSIS.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = String::with_capacity(end + ELLIPSIS.len());
    bounded.push_str(&text[..end]);
    bounded.push_str(ELLIPSIS);
    bounded
}

pub(crate) fn bounded_prefixed(prefix: &str, text: &str, max_bytes: usize) -> String {
    if prefix.len() >= max_bytes {
        return bounded_text(prefix, max_bytes);
    }
    let text = bounded_text(text, max_bytes - prefix.len());
    let mut bounded = String::with_capacity(prefix.len() + text.len());
    bounded.push_str(prefix);
    bounded.push_str(&text);
    bounded
}

/// Largest font file TideWM reads for compositor-owned text.
const MAX_FONT_BYTES: u64 = 32 * 1024 * 1024;
/// Distinct configured font files kept parsed at once.
const MAX_CACHED_FONTS: usize = 8;

type FontCache = Vec<(String, std::sync::Arc<Font>)>;

/// The bundled font, shared.
pub(crate) fn bundled_font() -> std::sync::Arc<Font> {
    static FONT: std::sync::OnceLock<std::sync::Arc<Font>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| std::sync::Arc::new(crate::toast::font().clone()))
        .clone()
}

/// A configured TTF/OTF file (`~/` allowed), parsed once per path; the
/// bundled font when unset or unreadable, with a warning per failed load.
pub(crate) fn font_from_path(path: Option<&str>) -> std::sync::Arc<Font> {
    let Some(path) = path else {
        return bundled_font();
    };
    static CACHE: std::sync::OnceLock<std::sync::Mutex<FontCache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut cache = cache.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some((_, font)) = cache.iter().find(|(cached, _)| cached == path) {
        return font.clone();
    }
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|home| std::path::Path::new(&home).join(rest))
            .unwrap_or_else(|| path.into()),
        None => path.into(),
    };
    let font = std::fs::metadata(&expanded)
        .ok()
        .filter(|meta| meta.len() <= MAX_FONT_BYTES)
        .and_then(|_| std::fs::read(&expanded).ok())
        .and_then(|bytes| Font::from_bytes(bytes, fontdue::FontSettings::default()).ok())
        .map(std::sync::Arc::new)
        .unwrap_or_else(|| {
            tracing::warn!(path, "Could not load font file, using the bundled font");
            bundled_font()
        });
    if cache.len() >= MAX_CACHED_FONTS {
        cache.remove(0);
    }
    cache.push((path.to_string(), font.clone()));
    font
}

/// Source-over blend of a straight-alpha color onto premultiplied ARGB8888
/// (memory order B, G, R, A).
pub(crate) fn blend_premultiplied(pixels: &mut [u8], width: i32, x: i32, y: i32, color: [f32; 4]) {
    let a = color[3].clamp(0.0, 1.0);
    if a <= 0.0 || x < 0 || y < 0 || x >= width {
        return;
    }
    let i = ((y * width + x) * 4) as usize;
    if i + 3 >= pixels.len() {
        return;
    }
    let blend = |dst: u8, src: f32| {
        let src = (src.clamp(0.0, 1.0) * a * 255.0).round();
        (src + f32::from(dst) * (1.0 - a)).round().clamp(0.0, 255.0) as u8
    };
    pixels[i] = blend(pixels[i], color[2]);
    pixels[i + 1] = blend(pixels[i + 1], color[1]);
    pixels[i + 2] = blend(pixels[i + 2], color[0]);
    pixels[i + 3] = ((a * 255.0) + f32::from(pixels[i + 3]) * (1.0 - a))
        .round()
        .clamp(0.0, 255.0) as u8;
}

/// Draws one line of `text` into the box `(x0, y0, w, h)` of a premultiplied
/// ARGB8888 canvas, `pad` from the left and vertically centered on the
/// font's line metrics, truncated with an ellipsis to the box.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_line(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    font: &Font,
    text: &str,
    font_size: f32,
    (x0, y0, w, h): (i32, i32, i32, i32),
    pad: i32,
    color: [f32; 4],
) {
    let line = rasterize_line(font, text, font_size, (w - pad * 2).max(0));
    let (ascent, descent) = font
        .horizontal_line_metrics(font_size)
        .map(|m| (m.ascent, m.descent))
        .unwrap_or((font_size * 0.8, -font_size * 0.2));
    let baseline = y0 + ((h as f32 - (ascent - descent)) / 2.0 + ascent).round() as i32;
    let mut pen_x = x0 + pad;
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
                if x < x0 || y < y0 || x >= (x0 + w).min(width) || y >= (y0 + h).min(height) {
                    continue;
                }
                let a = f32::from(coverage) / 255.0 * color[3];
                blend_premultiplied(pixels, width, x, y, [color[0], color[1], color[2], a]);
            }
        }
        pen_x += metrics.advance_width.round().max(1.0) as i32;
    }
}

/// Width in pixels `text` needs at `font_size`, capped at `max`.
pub(crate) fn line_width(font: &Font, text: &str, font_size: f32, max: i32) -> i32 {
    rasterize_line(font, text, font_size, max).advance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_megabyte_line_stops_at_the_pixel_budget() {
        let font = crate::toast::font();
        let text = "W".repeat(2 * 1024 * 1024);
        let line = rasterize_line(font, &text, 15.0, 173);

        assert!(line.truncated);
        assert!(line.advance <= 173);
        assert!(line.glyphs.len() <= 173);
        assert_eq!(
            line.glyphs.last().map(|glyph| glyph.character),
            Some('\u{2026}')
        );
    }

    #[test]
    fn short_line_keeps_every_glyph_without_ellipsis() {
        let line = rasterize_line(crate::toast::font(), "Tide", 15.0, 500);

        assert!(!line.truncated);
        assert_eq!(line.glyphs.len(), 4);
        assert_eq!(
            line.glyphs
                .iter()
                .map(|glyph| glyph.character)
                .collect::<String>(),
            "Tide"
        );
    }

    #[test]
    fn bounded_text_preserves_utf8_boundaries() {
        let bounded = bounded_text("water \u{1f30a} current", 12);
        assert!(bounded.len() <= 12);
        assert!(bounded.ends_with('\u{2026}'));
    }

    #[test]
    fn bounded_text_reuses_the_ipc_limit_exactly() {
        let limit = crate::ipc::MAX_REQUEST_BYTES;
        let exact = "x".repeat(limit);
        assert_eq!(bounded_text(&exact, limit), exact);

        let over = format!("{}\u{1f30a}", "x".repeat(limit - 1));
        let bounded = bounded_text(&over, limit);
        assert!(bounded.len() <= limit);
        assert!(bounded.ends_with('\u{2026}'));
    }

    #[test]
    fn bounded_prefix_does_not_copy_the_unbounded_tail() {
        let bounded = bounded_prefixed("Failed: ", &"x".repeat(1024 * 1024), 97);
        assert!(bounded.len() <= 97);
        assert!(bounded.starts_with("Failed: "));
        assert!(bounded.ends_with('\u{2026}'));
    }

    #[test]
    fn argb_length_rejects_invalid_and_overflowing_geometry() {
        assert_eq!(checked_argb_len(13, 7), Some(13 * 7 * 4));
        assert_eq!(checked_argb_len(0, 7), None);
        assert_eq!(checked_argb_len(-13, -7), None);
        assert_eq!(checked_argb_len(i32::MAX, 2), None);
    }
}
