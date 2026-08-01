//! Workspace overview: TideWM's schematic grid (rects + titles, not live window
//! content) of every workspace on one output, toggled with a single
//! keybind (`"toggle-overview"`). Same CPU-composited-texture approach as
//! `toast.rs`/`tab_strip.rs` -- built once when toggled on
//! (`Smallvil::toggle_overview`), not rebuilt every frame, since nothing
//! about it animates or otherwise needs to change on its own.
//!
//! **v1 is schematic, not live thumbnails.** Each window is drawn as a
//! labeled box (geometry straight from `layout::Layouts::layout`, computed
//! directly at the smaller cell size -- no separate scaling math needed)
//! rather than a real screenshot of its content. Live thumbnails would
//! mean rendering each *hidden* workspace's windows into an offscreen
//! texture one at a time, which needs temporarily remapping them into
//! `Smallvil::space` first (a hidden window isn't render-able at all
//! otherwise -- it's simply not in `space.elements()`, same as every other
//! "hidden window" gap this project has hit before) -- real render-path
//! work with its own latency/memory tradeoffs, deliberately deferred. The
//! mode itself (toggle on/off, grid arrangement) doesn't change if that's
//! ever built later: only what a cell draws would need to.
//!
//! Also known-lossy for v1, worth remembering rather than treating as a
//! bug report later: floating windows aren't tracked by `Layouts` at all
//! (see `Smallvil::floating_workspace`), so they never appear in a cell;
//! and fullscreen/maximized/pseudo-tile rect overrides are applied in
//! `Smallvil::retile`, not `Layouts::layout`, so an overridden window's
//! schematic box reflects its plain tiled slot, not the override. Good
//! enough for "roughly what's on each workspace," not a pixel-accurate
//! preview.

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

const BG_RGBA: (u8, u8, u8, u8) = (10, 10, 12, 220);
const CELL_BG: (u8, u8, u8) = (30, 30, 34);
const CELL_ACTIVE_BORDER: (u8, u8, u8) = (60, 170, 200); // same water-palette accent as Toast's Info kind
const CELL_BORDER: (u8, u8, u8) = (70, 70, 76);
const WINDOW_BG: (u8, u8, u8) = (52, 52, 58);
const WINDOW_BORDER: (u8, u8, u8) = (100, 100, 108);
const CELL_BORDER_PX: i32 = 2;
const WINDOW_BORDER_PX: i32 = 1;
const FONT_SIZE: f32 = 13.0;
const LABEL_PAD: i32 = 6;
const TEXT_RGB: (u8, u8, u8) = (225, 225, 225);

/// One workspace's grid cell: its own on-canvas area, whether it's the
/// output's currently active workspace (drawn with a highlighted border,
/// same idea as `tab_strip`'s active segment), and its windows' rects
/// (already computed at cell scale) plus titles.
pub struct OverviewCell {
    pub workspace: u32,
    pub area: Rectangle<i32, Logical>,
    pub active: bool,
    pub windows: Vec<(Rectangle<i32, Logical>, String)>,
}

pub struct Overview {
    /// Which output this was built for -- a render loop iterating every
    /// output must only draw this on the one it matches, not all of them
    /// (it's sized and laid out for exactly one output's mode).
    output_name: String,
    #[cfg(feature = "accessibility")]
    workspaces: Vec<u32>,
    buffer: MemoryRenderBuffer,
}

impl Overview {
    pub fn build(output_name: String, cells: &[OverviewCell], output_size: (i32, i32)) -> Self {
        let (width, height) = output_size;
        let mut pixels = vec![0u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                put_pixel(&mut pixels, width, x, y, BG_RGBA);
            }
        }

        let font = crate::toast::font();
        for cell in cells {
            draw_cell(&mut pixels, width, height, font, cell);
        }

        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (width, height),
            1,
            Transform::Normal,
            None,
        );
        Self {
            output_name,
            #[cfg(feature = "accessibility")]
            workspaces: cells.iter().map(|cell| cell.workspace).collect(),
            buffer,
        }
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    #[cfg(feature = "accessibility")]
    pub(crate) fn workspaces(&self) -> &[u32] {
        &self.workspaces
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

fn draw_cell(pixels: &mut [u8], canvas_w: i32, canvas_h: i32, font: &Font, cell: &OverviewCell) {
    let border = if cell.active {
        CELL_ACTIVE_BORDER
    } else {
        CELL_BORDER
    };
    fill_rect(pixels, canvas_w, canvas_h, cell.area, rgba(CELL_BG));
    stroke_rect(
        pixels,
        canvas_w,
        canvas_h,
        cell.area,
        border,
        CELL_BORDER_PX,
    );

    for (rect, title) in &cell.windows {
        fill_rect(pixels, canvas_w, canvas_h, *rect, rgba(WINDOW_BG));
        stroke_rect(
            pixels,
            canvas_w,
            canvas_h,
            *rect,
            WINDOW_BORDER,
            WINDOW_BORDER_PX,
        );
        draw_label(pixels, canvas_w, canvas_h, font, title, *rect);
    }
}

fn rgba((r, g, b): (u8, u8, u8)) -> (u8, u8, u8, u8) {
    (r, g, b, 255)
}

fn fill_rect(
    pixels: &mut [u8],
    canvas_w: i32,
    canvas_h: i32,
    rect: Rectangle<i32, Logical>,
    rgba: (u8, u8, u8, u8),
) {
    let x0 = rect.loc.x.clamp(0, canvas_w);
    let y0 = rect.loc.y.clamp(0, canvas_h);
    let x1 = (rect.loc.x + rect.size.w).clamp(0, canvas_w);
    let y1 = (rect.loc.y + rect.size.h).clamp(0, canvas_h);
    for y in y0..y1 {
        for x in x0..x1 {
            put_pixel(pixels, canvas_w, x, y, rgba);
        }
    }
}

fn stroke_rect(
    pixels: &mut [u8],
    canvas_w: i32,
    canvas_h: i32,
    rect: Rectangle<i32, Logical>,
    rgb: (u8, u8, u8),
    thickness: i32,
) {
    let edges = [
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
    ];
    for edge in edges {
        fill_rect(pixels, canvas_w, canvas_h, edge, rgba(rgb));
    }
}

/// Draws `title` clipped to `rect`'s bounds -- overflowing glyphs are
/// simply cut off rather than measured and pre-truncated, same choice
/// `tab_strip::draw_label` makes for its own segments.
fn draw_label(
    pixels: &mut [u8],
    canvas_w: i32,
    canvas_h: i32,
    font: &Font,
    title: &str,
    rect: Rectangle<i32, Logical>,
) {
    let baseline = rect.loc.y + LABEL_PAD + FONT_SIZE as i32;
    let mut pen_x = rect.loc.x + LABEL_PAD;
    let clip_right = rect.loc.x + rect.size.w - LABEL_PAD;

    for ch in title.chars() {
        if pen_x >= clip_right {
            break;
        }
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
                if x < rect.loc.x
                    || y < rect.loc.y
                    || x >= clip_right
                    || y >= rect.loc.y + rect.size.h
                    || x >= canvas_w
                    || y >= canvas_h
                    || x < 0
                    || y < 0
                {
                    continue;
                }
                blend_text_pixel(pixels, canvas_w, x, y, coverage);
            }
        }
        // `.max(1.0)`: a font can legitimately return 0 advance width for a
        // glyph it has no real width for (zero-width joiners, combining
        // marks, or any codepoint the font falls back to .notdef for -- a
        // window title is arbitrary client-supplied text, nothing stops one
        // from containing these). Without the floor, `pen_x` stalls near
        // the label's start x while `glyph_y0` keeps varying per character
        // (it depends on that character's own metrics, not on `pen_x`),
        // drawing every remaining character in a near-vertical column
        // instead of a horizontal line -- the exact shape of a real,
        // reproduced-once bug this guards against (see AGENT.md's
        // "Real-hardware verification pass" section).
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
