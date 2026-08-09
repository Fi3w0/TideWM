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
