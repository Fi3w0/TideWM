//! Persistent compositor-owned configuration error panel for TideWM.
//!
//! While a config error exists, this occupies a reserved strip at the top of
//! each output until a successful reload. Transient reload/debug messages use
//! the separate toast path.

use std::collections::HashMap;

use fontdue::Font;
use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    utils::{Logical, Physical, Point, Size, Transform},
};

pub const PANEL_HEIGHT: i32 = 116;
const TITLE_SIZE: f32 = 16.0;
const BODY_SIZE: f32 = 13.0;
const LABEL_SIZE: f32 = 9.5;
const CARD_MARGIN_X: i32 = 18;
const CARD_MARGIN_Y: i32 = 9;
const PAD_X: i32 = 58;
const MAX_CACHED_WIDTHS: usize = 4;

/// `Error` means the file failed to parse and the previous config is still
/// the one actually in effect; `Warning` means the new config applied fine
/// but something in it is worth a second look (a dropped keybind entry, or
/// a footgun lint -- see `Config::from_raw`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OverlaySeverity {
    Error,
    Warning,
}

pub struct ConfigErrorOverlay {
    message: String,
    severity: OverlaySeverity,
    theme: crate::ui_theme::UiTheme,
    buffers: HashMap<i32, MemoryRenderBuffer>,
}

impl ConfigErrorOverlay {
    pub fn new(
        message: impl Into<String>,
        severity: OverlaySeverity,
        theme: crate::ui_theme::UiTheme,
    ) -> Self {
        Self {
            message: message.into(),
            severity,
            theme,
            buffers: HashMap::new(),
        }
    }

    pub fn reserved_height(&self) -> i32 {
        PANEL_HEIGHT
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        logical_width: i32,
        logical_y: i32,
        scale: f64,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let width = logical_width.max(240);
        if !self.buffers.contains_key(&width) {
            if self.buffers.len() >= MAX_CACHED_WIDTHS {
                self.buffers.clear();
            }
            self.buffers.insert(
                width,
                build_buffer(&self.message, self.severity, self.theme, width),
            );
        }
        let buffer = self.buffers.get(&width)?;
        let location: Point<f64, Physical> = (0.0, logical_y as f64 * scale).into();
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            buffer,
            None,
            None,
            Some(Size::<i32, Logical>::from((width, PANEL_HEIGHT))),
            Kind::Unspecified,
        )
        .ok()
    }
}

fn build_buffer(
    message: &str,
    severity: OverlaySeverity,
    theme: crate::ui_theme::UiTheme,
    width: i32,
) -> MemoryRenderBuffer {
    let mut pixels = vec![0u8; (width * PANEL_HEIGHT * 4) as usize];
    let left = CARD_MARGIN_X;
    let top = CARD_MARGIN_Y;
    let card_width = (width - CARD_MARGIN_X * 2).max(1);
    let card_height = PANEL_HEIGHT - CARD_MARGIN_Y * 2;
    // Preserve the toast card's radius-to-height ratio for this taller panel.
    let radius = (theme.radius as f32 * card_height as f32 / crate::toast::CARD_HEIGHT as f32)
        .min(card_height as f32 / 2.0)
        .max(4.0);
    for y in 0..PANEL_HEIGHT {
        for x in 0..width {
            let shadow = rounded_rect_coverage(
                x,
                y,
                left - 3,
                top + 4,
                card_width + 6,
                card_height,
                radius + 3.0,
            );
            if shadow > 0.0 {
                put_pixel(&mut pixels, width, x, y, (0, 0, 0, (72.0 * shadow) as u8));
            }
            let coverage = rounded_rect_coverage(x, y, left, top, card_width, card_height, radius);
            if coverage <= 0.0 {
                continue;
            }
            let t = ((x - left) as f32 / card_width as f32).clamp(0.0, 1.0);
            let bg = crate::ui_theme::mix(theme.panel_from, theme.panel_to, t);
            put_pixel(
                &mut pixels,
                width,
                x,
                y,
                (bg[0], bg[1], bg[2], (244.0 * coverage) as u8),
            );
            let stroke = theme.border_width.round().max(1.0) as i32;
            let inner = rounded_rect_coverage(
                x,
                y,
                left + stroke,
                top + stroke,
                card_width - stroke * 2,
                card_height - stroke * 2,
                (radius - stroke as f32).max(1.0),
            );
            let border = (coverage - inner).max(0.0);
            if border > 0.0 {
                let accent = theme.popup_accent(severity == OverlaySeverity::Error, t);
                blend_text_pixel(&mut pixels, width, x, y, accent, (border * 235.0) as u8);
            }
        }
    }

    let accent = theme.accent(severity == OverlaySeverity::Error, 0.25);
    let center = (left + 25, top + card_height / 2);
    for y in center.1 - 13..=center.1 + 13 {
        for x in center.0 - 13..=center.0 + 13 {
            let distance = (((x - center.0).pow(2) + (y - center.1).pow(2)) as f32).sqrt();
            let coverage = (13.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend_text_pixel(&mut pixels, width, x, y, accent, (coverage * 220.0) as u8);
            }
        }
    }

    let font = crate::toast::font();
    let title = match severity {
        OverlaySeverity::Error => "Configuration needs attention",
        OverlaySeverity::Warning => "Configuration note",
    };
    draw_line(
        &mut pixels,
        (width, PANEL_HEIGHT),
        font,
        "TIDEWM",
        left + PAD_X,
        top + 19,
        (LABEL_SIZE, theme.muted_text),
    );
    draw_line(
        &mut pixels,
        (width, PANEL_HEIGHT),
        font,
        title,
        left + PAD_X,
        top + 42,
        (TITLE_SIZE, theme.text),
    );
    let available = (width - left - PAD_X - CARD_MARGIN_X - 18).max(1);
    let lines = wrap_text(font, message, BODY_SIZE, available, 2);
    for (index, line) in lines.iter().enumerate() {
        draw_line(
            &mut pixels,
            (width, PANEL_HEIGHT),
            font,
            line,
            left + PAD_X,
            top + 66 + index as i32 * 17,
            (BODY_SIZE, theme.muted_text),
        );
    }

    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (width, PANEL_HEIGHT),
        1,
        Transform::Normal,
        None,
    )
}

fn wrap_text(font: &Font, text: &str, size: f32, max_width: i32, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0.0f32;
    for word in text.split_whitespace() {
        let word_width: f32 = word
            .chars()
            .map(|ch| font.metrics(ch, size).advance_width.max(1.0))
            .sum();
        let space = if current.is_empty() {
            0.0
        } else {
            font.metrics(' ', size).advance_width
        };
        if !current.is_empty() && current_width + space + word_width > max_width as f32 {
            lines.push(std::mem::take(&mut current));
            current_width = 0.0;
            if lines.len() == max_lines {
                let last = lines.last_mut().unwrap();
                last.push('…');
                return lines;
            }
        }
        if !current.is_empty() {
            current.push(' ');
            current_width += space;
        }
        current.push_str(word);
        current_width += word_width;
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    lines
}

fn draw_line(
    pixels: &mut [u8],
    canvas: (i32, i32),
    font: &Font,
    text: &str,
    x0: i32,
    baseline: i32,
    style: (f32, [u8; 3]),
) {
    let (size, rgb) = style;
    let (width, height) = canvas;
    let mut pen_x = x0;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size);
        let glyph_x = pen_x + metrics.xmin;
        let glyph_y = baseline - metrics.ymin - metrics.height as i32;
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let coverage = bitmap[gy * metrics.width + gx];
                let x = glyph_x + gx as i32;
                let y = glyph_y + gy as i32;
                if coverage > 0 && x >= 0 && y >= 0 && x < width && y < height {
                    blend_text_pixel(pixels, width, x, y, rgb, coverage);
                }
            }
        }
        pen_x += metrics.advance_width.round().max(1.0) as i32;
        if pen_x >= width - PAD_X {
            break;
        }
    }
}

fn put_pixel(pixels: &mut [u8], width: i32, x: i32, y: i32, (r, g, b, a): (u8, u8, u8, u8)) {
    let i = ((y * width + x) * 4) as usize;
    pixels[i] = b;
    pixels[i + 1] = g;
    pixels[i + 2] = r;
    pixels[i + 3] = a;
}

fn blend_text_pixel(pixels: &mut [u8], width: i32, x: i32, y: i32, rgb: [u8; 3], coverage: u8) {
    let i = ((y * width + x) * 4) as usize;
    let t = coverage as f32 / 255.0;
    let [r, g, b] = rgb;
    pixels[i] = (pixels[i] as f32 + (b as f32 - pixels[i] as f32) * t) as u8;
    pixels[i + 1] = (pixels[i + 1] as f32 + (g as f32 - pixels[i + 1] as f32) * t) as u8;
    pixels[i + 2] = (pixels[i + 2] as f32 + (r as f32 - pixels[i + 2] as f32) * t) as u8;
    pixels[i + 3] = pixels[i + 3].max(coverage);
}

fn rounded_rect_coverage(
    x: i32,
    y: i32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    radius: f32,
) -> f32 {
    let fx = (x - left) as f32 + 0.5;
    let fy = (y - top) as f32 + 0.5;
    let fw = width as f32;
    let fh = height as f32;
    // Clamp each axis's distance from its flat region independently, then
    // evaluate the corner distance; being inside one axis cannot hide an
    // out-of-bounds coordinate on the other.
    let dx = ((fx - fw / 2.0).abs() - (fw / 2.0 - radius)).max(0.0);
    let dy = ((fy - fh / 2.0).abs() - (fh / 2.0 - radius)).max(0.0);
    let distance = (dx * dx + dy * dy).sqrt();
    (radius - distance + 0.5).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A point outside one axis remains uncovered even when it lies in the
    /// other axis's flat region.
    #[test]
    fn coverage_is_zero_far_outside_the_shape_even_in_the_flat_region() {
        let (left, top, width, height, radius) = (18, 9, 1884, 98, 12.0);
        let x_mid = left + width / 2;
        assert_eq!(
            rounded_rect_coverage(x_mid, 0, left, top, width, height, radius),
            0.0
        );
        assert_eq!(
            rounded_rect_coverage(x_mid, top + height + 20, left, top, width, height, radius),
            0.0
        );
        // Still fully covered well inside the same flat column.
        assert_eq!(
            rounded_rect_coverage(x_mid, top + height / 2, left, top, width, height, radius),
            1.0
        );
    }

    #[test]
    fn wrapping_is_bounded_and_keeps_message_content() {
        let lines = wrap_text(
            crate::toast::font(),
            "one two three four five six",
            13.0,
            55,
            2,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with('…'));
    }

    /// Both body lines must remain inside the card on a narrow output.
    #[test]
    fn wrapped_body_text_stays_above_the_cards_bottom_edge() {
        let font = crate::toast::font();
        let message = "Failed to load included config file: in file \
                        /home/fiw/.config/tidewm/water.wave at line 38: \
                        unexpected end of file, missing a closing `}`, skipping";
        let width = 320;
        let left = CARD_MARGIN_X;
        let available = (width - left - PAD_X - CARD_MARGIN_X - 18).max(1);
        let lines = wrap_text(font, message, BODY_SIZE, available, 2);
        assert_eq!(lines.len(), 2, "narrow width should force both body lines");

        let card_bottom = PANEL_HEIGHT - CARD_MARGIN_Y;
        for (index, line) in lines.iter().enumerate() {
            let baseline = CARD_MARGIN_Y + 66 + index as i32 * 17;
            for ch in line.chars() {
                let (metrics, _) = font.rasterize(ch, BODY_SIZE);
                let glyph_bottom = baseline - metrics.ymin;
                assert!(
                    glyph_bottom <= card_bottom,
                    "line {index} ('{line}') glyph '{ch}' bottom at y={glyph_bottom} \
                     spills past the card's bottom edge at y={card_bottom}"
                );
            }
        }
    }
}
