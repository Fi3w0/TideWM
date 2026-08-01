//! Static schematic overlay for Classic's per-workspace Depth Deck.
//!
//! It intentionally shows titles rather than live thumbnails: parked windows
//! are absent from `Space`, and remapping them solely to take a screenshot
//! would violate the deck's focus/hit-testing isolation. The texture is only
//! rebuilt when the deck opens or its selection changes, so it costs no idle
//! frames.

use fontdue::Font;
use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    utils::{Logical, Rectangle, Transform},
};

const BACKDROP: (u8, u8, u8, u8) = (5, 13, 18, 190);
const CARD: (u8, u8, u8, u8) = (22, 38, 47, 245);
const CARD_SELECTED: (u8, u8, u8, u8) = (30, 74, 88, 255);
const BORDER: (u8, u8, u8, u8) = (60, 170, 200, 255);
const TEXT: (u8, u8, u8) = (225, 240, 244);
const FONT_SIZE: f32 = 16.0;

pub struct DepthDeckOverlay {
    output_name: String,
    buffer: MemoryRenderBuffer,
}

impl DepthDeckOverlay {
    pub fn build(
        output_name: String,
        workspace: u32,
        titles: &[String],
        selected: usize,
        output_size: (i32, i32),
    ) -> Self {
        let (width, height) = output_size;
        let mut pixels = vec![0; (width * height * 4) as usize];
        fill_rect(
            &mut pixels,
            width,
            height,
            Rectangle::new((0, 0).into(), (width, height).into()),
            BACKDROP,
        );

        let deck_width = (width - 64).clamp(280, 900);
        let card_height = 54;
        let gap = 10;
        let visible = titles
            .len()
            .min(((height - 120) / (card_height + gap)).max(1) as usize);
        let panel_height = 46 + visible as i32 * (card_height + gap);
        let panel_x = (width - deck_width) / 2;
        let panel_y = (height - panel_height) / 2;
        let font = crate::toast::font();
        draw_text(
            &mut pixels,
            (width, height),
            font,
            &format!("Depth Deck · workspace {workspace}"),
            Rectangle::new((panel_x, panel_y).into(), (deck_width, 28).into()),
        );

        let start = selection_window_start(selected, titles.len(), visible);
        for (row, (index, title)) in titles
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .enumerate()
        {
            let rect = Rectangle::new(
                (panel_x, panel_y + 36 + row as i32 * (card_height + gap)).into(),
                (deck_width, card_height).into(),
            );
            fill_rect(
                &mut pixels,
                width,
                height,
                rect,
                if index == selected {
                    CARD_SELECTED
                } else {
                    CARD
                },
            );
            if index == selected {
                stroke_rect(&mut pixels, width, height, rect, BORDER, 2);
            }
            draw_text(
                &mut pixels,
                (width, height),
                font,
                title,
                Rectangle::new(
                    (rect.loc.x + 16, rect.loc.y + 17).into(),
                    (rect.size.w - 32, 24).into(),
                ),
            );
        }

        Self {
            output_name,
            buffer: MemoryRenderBuffer::from_slice(
                &pixels,
                Fourcc::Argb8888,
                (width, height),
                1,
                Transform::Normal,
                None,
            ),
        }
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    pub fn render_element(
        &self,
        renderer: &mut GlesRenderer,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (0.0, 0.0),
            &self.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }
}

fn selection_window_start(selected: usize, count: usize, visible: usize) -> usize {
    if count <= visible {
        0
    } else {
        selected.saturating_sub(visible / 2).min(count - visible)
    }
}

fn fill_rect(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    rect: Rectangle<i32, Logical>,
    color: (u8, u8, u8, u8),
) {
    let x0 = rect.loc.x.clamp(0, width);
    let y0 = rect.loc.y.clamp(0, height);
    let x1 = (rect.loc.x + rect.size.w).clamp(0, width);
    let y1 = (rect.loc.y + rect.size.h).clamp(0, height);
    for y in y0..y1 {
        for x in x0..x1 {
            put_pixel(pixels, width, x, y, color);
        }
    }
}

fn stroke_rect(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    rect: Rectangle<i32, Logical>,
    color: (u8, u8, u8, u8),
    thickness: i32,
) {
    for edge in [
        Rectangle::new(rect.loc, (rect.size.w, thickness).into()),
        Rectangle::new(
            (rect.loc.x, rect.loc.y + rect.size.h - thickness).into(),
            (rect.size.w, thickness).into(),
        ),
        Rectangle::new(rect.loc, (thickness, rect.size.h).into()),
        Rectangle::new(
            (rect.loc.x + rect.size.w - thickness, rect.loc.y).into(),
            (thickness, rect.size.h).into(),
        ),
    ] {
        fill_rect(pixels, width, height, edge, color);
    }
}

fn draw_text(
    pixels: &mut [u8],
    canvas: (i32, i32),
    font: &Font,
    text: &str,
    rect: Rectangle<i32, Logical>,
) {
    let (width, height) = canvas;
    let baseline = rect.loc.y + FONT_SIZE as i32;
    let mut pen_x = rect.loc.x;
    for ch in text.chars() {
        if pen_x >= rect.loc.x + rect.size.w {
            break;
        }
        let (metrics, bitmap) = font.rasterize(ch, FONT_SIZE);
        let glyph_x = pen_x + metrics.xmin;
        let glyph_y = baseline - metrics.ymin - metrics.height as i32;
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let coverage = bitmap[gy * metrics.width + gx];
                let px = glyph_x + gx as i32;
                let py = glyph_y + gy as i32;
                if coverage > 0
                    && px >= rect.loc.x
                    && px < rect.loc.x + rect.size.w
                    && px >= 0
                    && py >= 0
                    && px < width
                    && py < height
                {
                    blend_text_pixel(pixels, width, px, py, coverage);
                }
            }
        }
        pen_x += metrics.advance_width.round().max(1.0) as i32;
    }
}

fn put_pixel(pixels: &mut [u8], width: i32, x: i32, y: i32, (r, g, b, a): (u8, u8, u8, u8)) {
    let index = ((y * width + x) * 4) as usize;
    pixels[index] = b;
    pixels[index + 1] = g;
    pixels[index + 2] = r;
    pixels[index + 3] = a;
}

fn blend_text_pixel(pixels: &mut [u8], width: i32, x: i32, y: i32, coverage: u8) {
    let index = ((y * width + x) * 4) as usize;
    let alpha = coverage as f32 / 255.0;
    pixels[index] = (pixels[index] as f32 + (TEXT.2 as f32 - pixels[index] as f32) * alpha) as u8;
    pixels[index + 1] =
        (pixels[index + 1] as f32 + (TEXT.1 as f32 - pixels[index + 1] as f32) * alpha) as u8;
    pixels[index + 2] =
        (pixels[index + 2] as f32 + (TEXT.0 as f32 - pixels[index + 2] as f32) * alpha) as u8;
    pixels[index + 3] = pixels[index + 3].max(coverage);
}

#[cfg(test)]
mod tests {
    use super::selection_window_start;

    #[test]
    fn selected_card_stays_inside_visible_slice() {
        assert_eq!(selection_window_start(0, 10, 4), 0);
        assert_eq!(selection_window_start(5, 10, 4), 3);
        assert_eq!(selection_window_start(9, 10, 4), 6);
    }
}
