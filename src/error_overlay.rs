//! Persistent compositor-owned configuration error panel.
//!
//! Like Hyprland's error overlay, this is not a notification popup: while a
//! config error exists it occupies a fixed strip at the top of each output
//! and `Smallvil::output_tiling_area` reserves that space from tiled clients.
//! Fixing and successfully reloading the config removes it. Ordinary reload
//! and debug toasts remain a separate mechanism.

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

pub const PANEL_HEIGHT: i32 = 96;
const TITLE_SIZE: f32 = 16.0;
const BODY_SIZE: f32 = 13.0;
const PAD_X: i32 = 22;
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
    buffers: HashMap<i32, MemoryRenderBuffer>,
}

impl ConfigErrorOverlay {
    pub fn new(message: impl Into<String>, severity: OverlaySeverity) -> Self {
        Self {
            message: message.into(),
            severity,
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
            self.buffers
                .insert(width, build_buffer(&self.message, self.severity, width));
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

fn build_buffer(message: &str, severity: OverlaySeverity, width: i32) -> MemoryRenderBuffer {
    let mut pixels = vec![0u8; (width * PANEL_HEIGHT * 4) as usize];
    for y in 0..PANEL_HEIGHT {
        for x in 0..width {
            let t = x as f32 / width as f32;
            put_pixel(
                &mut pixels,
                width,
                x,
                y,
                (111 + (18.0 * t) as u8, 35, 43, 248),
            );
        }
    }
    // Aqua edge ties the diagnostic panel to Tide's palette while the warm
    // body remains unmistakably an error.
    for y in PANEL_HEIGHT - 3..PANEL_HEIGHT {
        for x in 0..width {
            put_pixel(&mut pixels, width, x, y, (61, 188, 215, 255));
        }
    }

    let font = crate::toast::font();
    let title = match severity {
        OverlaySeverity::Error => "TideWM configuration error",
        OverlaySeverity::Warning => "TideWM configuration warning",
    };
    draw_line(
        &mut pixels,
        (width, PANEL_HEIGHT),
        font,
        title,
        PAD_X,
        25,
        (TITLE_SIZE, (255, 244, 244)),
    );
    let available = (width - PAD_X * 2).max(1);
    let lines = wrap_text(font, message, BODY_SIZE, available, 3);
    for (index, line) in lines.iter().enumerate() {
        draw_line(
            &mut pixels,
            (width, PANEL_HEIGHT),
            font,
            line,
            PAD_X,
            48 + index as i32 * 17,
            (BODY_SIZE, (255, 224, 226)),
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
    style: (f32, (u8, u8, u8)),
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

fn blend_text_pixel(
    pixels: &mut [u8],
    width: i32,
    x: i32,
    y: i32,
    rgb: (u8, u8, u8),
    coverage: u8,
) {
    let i = ((y * width + x) * 4) as usize;
    let t = coverage as f32 / 255.0;
    let (r, g, b) = rgb;
    pixels[i] = (pixels[i] as f32 + (b as f32 - pixels[i] as f32) * t) as u8;
    pixels[i + 1] = (pixels[i + 1] as f32 + (g as f32 - pixels[i + 1] as f32) * t) as u8;
    pixels[i + 2] = (pixels[i + 2] as f32 + (r as f32 - pixels[i + 2] as f32) * t) as u8;
    pixels[i + 3] = pixels[i + 3].max(coverage);
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
