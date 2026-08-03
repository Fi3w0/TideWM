//! Whole-world Ocean overview minimap (spatial roadmap S5's other half,
//! alongside `crate::compass`). Hold `minimap.key` (default `Super+Space`)
//! to peek: a schematic map of every window in the shared world, plus every
//! connected output's current camera viewport, scaled to fit the
//! triggering output. Click a window or region to travel that output's
//! camera there and dismiss; release without clicking just dismisses.
//! Ocean-only, and deliberately *not* gated by `water_effects` -- see
//! `MinimapConfig`'s own doc for why.
//!
//! Same CPU-composited-texture approach as `overview.rs` (built once per
//! peek via `MinimapPeek::build`, not rebuilt every frame). Box/label
//! placement still reuses `overview.rs`'s `fill_rect`/`stroke_rect`/
//! `draw_label` directly for the flat `Plain` preset; the two decorated
//! presets (`Bioluminescent`, `Glass`) layer rounded corners, a gradient
//! backdrop, and a soft colored rim on top, reusing `toast.rs`'s
//! `rounded_rect_coverage_local` and `ui_theme::mix` -- the same
//! rounded-box-with-soft-shadow technique the toast card already uses,
//! just with the shadow's color parameterized instead of fixed black.
//!
//! **Presets pick a baseline look; `background_color`/`window_color`/
//! `accent_color` still override individual colors on top of it** -- the
//! same shape `compass`'s `shape` + `urgent_color`/`deep_color` already
//! established, not a second config pattern to learn.
//!
//! **Known-lossy, same spirit as `overview.rs`'s own admitted gaps:**
//! screen-pinned windows (`Smallvil::screen_pins`) aren't drawn -- a pin is
//! glued to one output's screen space, not a world location, so it has
//! nothing meaningful to show on a *world* map. And because the whole
//! canvas is built once at peek-open rather than kept live, a camera
//! mid-`OceanCameraMotion` ease on another output at that exact instant is
//! snapshotted at whatever point its animation had reached, not its final
//! destination -- a single-seat compositor can't move a second output's
//! camera *during* the hold itself (there's only one keyboard/pointer), so
//! this is the one residual staleness case, not a general "goes stale"
//! problem.
//!
//! **Pointer focus isn't explicitly restored on close.** Every motion
//! event while the peek is open moves the cursor with no focus target
//! (`pointer.motion(self, None, ..)`), which sends whatever surface was
//! under it a real Wayland `leave`. Closing the peek doesn't re-enter
//! anything itself -- the very next ordinary motion event re-runs
//! `surface_under` and re-enters normally, so this self-heals on the next
//! pointer move, but a click landing before any further motion (pointer
//! perfectly still since the peek closed) goes to focus `None` and is
//! lost. Narrow enough not to be worth a dedicated re-focus call for.

use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    utils::{Logical, Point, Rectangle, Size, Transform},
};

use crate::overview::{draw_label, fill_rect, rgba, stroke_rect};
use crate::toast::rounded_rect_coverage_local;
use crate::ui_theme::mix;

/// Padding around the world content's bounding box, as a fraction of its
/// own size, so nothing sits flush against the canvas edge.
const MARGIN_FRACTION: f64 = 0.1;
/// Floor for the padded extent's width/height, world-logical pixels. Keeps
/// `scale` (canvas px / world px) finite for a near-empty world or a
/// single tight viewport instead of it blowing up toward infinity.
const MIN_EXTENT: f64 = 256.0;
/// How far the soft rim/glow expands beyond a box's own edge, canvas px.
const RIM_SPREAD: i32 = 9;

/// Named visual baseline for the minimap, selected by `minimap.preset`.
/// Each preset resolves to a concrete `MinimapStyle`; `background_color`/
/// `window_color`/`accent_color` in config override individual fields of
/// whichever preset is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MinimapPreset {
    /// The original flat schematic: sharp corners, no glow, identical to
    /// `overview.rs`'s own Classic grid. Kept for anyone who wants the
    /// minimal look back.
    Plain,
    /// Default. Deep-water gradient backdrop, rounded glassy window boxes,
    /// and a cyan/teal bioluminescent rim -- the same water-identity
    /// palette the compass's own urgent glow uses, so the two S5 halves
    /// read as one family rather than two different UIs.
    #[default]
    Bioluminescent,
    /// Frosted, low-contrast rounded panels with a neutral drop shadow
    /// instead of a colored glow -- for a subtler, less neon look.
    Glass,
}

impl MinimapPreset {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "plain" | "schematic" | "flat" => MinimapPreset::Plain,
            "bioluminescent" | "glow" | "reef" => MinimapPreset::Bioluminescent,
            "glass" | "frost" | "frosted" => MinimapPreset::Glass,
            _ => return None,
        })
    }

    fn base_style(self) -> MinimapStyle {
        match self {
            MinimapPreset::Plain => MinimapStyle {
                background_top: [10, 10, 12],
                background_bottom: [10, 10, 12],
                background_alpha: 220,
                window_fill: [52, 52, 58],
                window_border: [100, 100, 108],
                accent: [60, 170, 200],
                radius: 0.0,
                soft_rim: None,
            },
            MinimapPreset::Bioluminescent => MinimapStyle {
                background_top: [8, 22, 30],
                background_bottom: [18, 46, 56],
                background_alpha: 240,
                window_fill: [26, 64, 76],
                window_border: [90, 210, 225],
                accent: [118, 241, 255],
                radius: 10.0,
                soft_rim: Some([90, 225, 240]),
            },
            MinimapPreset::Glass => MinimapStyle {
                background_top: [19, 19, 25],
                background_bottom: [28, 28, 36],
                background_alpha: 206,
                window_fill: [58, 58, 68],
                window_border: [190, 190, 200],
                accent: [190, 205, 255],
                radius: 12.0,
                soft_rim: Some([0, 0, 0]),
            },
        }
    }
}

/// Resolved colors/shape for one peek's render, either a preset's plain
/// defaults or those defaults with config's own color overrides applied.
#[derive(Clone, Copy, Debug)]
pub struct MinimapStyle {
    pub background_top: [u8; 3],
    pub background_bottom: [u8; 3],
    pub background_alpha: u8,
    pub window_fill: [u8; 3],
    pub window_border: [u8; 3],
    /// Drives both the triggering output's viewport "you are here" glow
    /// and an urgent window's highlight -- one color for "pay attention
    /// here," matching the compass's own single-urgent-color language
    /// rather than a second independent knob.
    pub accent: [u8; 3],
    pub radius: f32,
    /// Soft colored rim drawn just outside a box's own edge. `None` (only
    /// `Plain`) skips the pass entirely rather than drawing at zero alpha.
    pub soft_rim: Option<[u8; 3]>,
}

impl MinimapStyle {
    pub fn resolve(
        preset: MinimapPreset,
        background: Option<[f32; 3]>,
        window: Option<[f32; 3]>,
        accent: Option<[f32; 3]>,
    ) -> Self {
        let mut style = preset.base_style();
        if let Some(color) = background {
            let color = to_u8(color);
            style.background_top = color;
            style.background_bottom = color;
        }
        if let Some(color) = window {
            style.window_fill = to_u8(color);
        }
        if let Some(color) = accent {
            let color = to_u8(color);
            style.accent = color;
            style.window_border = color;
            if style.soft_rim.is_some() {
                style.soft_rim = Some(color);
            }
        }
        style
    }
}

fn to_u8(c: [f32; 3]) -> [u8; 3] {
    [
        (c[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (c[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (c[2].clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

/// One peek's built state: the cached world-map texture plus the transform
/// needed to invert a click back to a world point. Held by
/// `Smallvil::minimap_peek` for the hold gesture's duration.
pub struct MinimapPeek {
    /// Which output this was built for -- also which output's camera
    /// travels on click, decided once at open time rather than re-derived
    /// later through anything pointer-location-dependent.
    output_name: String,
    buffer: MemoryRenderBuffer,
    /// Canvas size in logical pixels: the output size the map was built
    /// for. Stored because `MemoryRenderBuffer` doesn't expose its size.
    canvas_size: Size<i32, Logical>,
    /// Small crosshair glyph drawn over the map at the pointer's current
    /// canvas position, so aiming a click doesn't mean guessing. Rebuilt
    /// only when the pointer moves (`set_last_location`), never per frame.
    cursor_buffer: MemoryRenderBuffer,
    /// World point mapped to canvas-space (0, 0).
    world_origin: Point<f64, Logical>,
    /// Canvas pixels per world logical pixel.
    scale: f64,
    /// Last known pointer position in canvas-space logical pixels. Seeded
    /// from the real pointer position at peek-open time and kept live by
    /// ordinary motion-event handling for the rest of the hold -- never
    /// read via `current_location()` from inside a grab/click dispatch,
    /// the same freeze shape as the `TileMoveGrab` (0.15.1) and
    /// `sync_visible_floating_window` incidents already in this project's
    /// history.
    last_location: Point<f64, Logical>,
}

/// Crosshair glyph size, logical pixels. Small, centered on the pointer.
const CURSOR_GLYPH: i32 = 20;
/// Glyph color: the same accent as the "you are here" viewport beacon.
const CURSOR_COLOR: [u8; 4] = [118, 241, 255, 255];

fn build_cursor_glyph() -> MemoryRenderBuffer {
    let size = CURSOR_GLYPH;
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    let center = size / 2;
    let radius = size / 2 - 1;
    let cross = 3;
    for y in 0..size {
        for x in 0..size {
            let dx = x - center;
            let dy = y - center;
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            let on_ring = (dist - radius as f32).abs() <= 1.5;
            let on_cross = (dx.abs() <= cross && dy.abs() <= cross)
                || (dy.abs() <= 1 && dx.abs() > radius - 2)
                || (dx.abs() <= 1 && dy.abs() > radius - 2);
            if on_ring || on_cross {
                let i = ((y * size + x) * 4) as usize;
                pixels[i..i + 4].copy_from_slice(&CURSOR_COLOR);
            }
        }
    }
    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (size, size),
        1,
        Transform::Normal,
        None,
    )
}

impl MinimapPeek {
    /// `windows` are world-space rects paired with a title and whether
    /// that window is currently urgent (drawn with the style's `accent`
    /// instead of its plain border, same "urgent glows" language the
    /// compass already uses). `reef_rects` only widens the framed extent
    /// (an empty reef is still a landmark worth keeping in view); nothing
    /// is drawn for it. `viewports` pairs an output's name with its
    /// current camera rect; the entry matching `output_name` is drawn with
    /// the style's accent as a "you are here" beacon, every other output's
    /// plainer.
    pub fn build(
        output_name: String,
        output_size: (i32, i32),
        pointer_location: Point<f64, Logical>,
        style: MinimapStyle,
        windows: &[(Rectangle<i32, Logical>, String, bool)],
        reef_rects: &[Rectangle<i32, Logical>],
        viewports: &[(String, Rectangle<i32, Logical>)],
    ) -> Self {
        let (width, height) = output_size;

        let mut extent: Option<Rectangle<f64, Logical>> = None;
        let mut grow = |rect: Rectangle<i32, Logical>| {
            let rect = rect.to_f64();
            extent = Some(match extent {
                Some(current) => current.merge(rect),
                None => rect,
            });
        };
        for (rect, _, _) in windows {
            grow(*rect);
        }
        for rect in reef_rects {
            grow(*rect);
        }
        for (_, rect) in viewports {
            grow(*rect);
        }
        let content = extent.unwrap_or_else(|| {
            Rectangle::new(
                Point::from((0.0, 0.0)),
                Size::from((MIN_EXTENT, MIN_EXTENT)),
            )
        });

        let pad_w = (content.size.w * MARGIN_FRACTION).max(MIN_EXTENT * MARGIN_FRACTION);
        let pad_h = (content.size.h * MARGIN_FRACTION).max(MIN_EXTENT * MARGIN_FRACTION);
        let padded: Rectangle<f64, Logical> = Rectangle::new(
            Point::from((content.loc.x - pad_w, content.loc.y - pad_h)),
            Size::from((
                (content.size.w + pad_w * 2.0).max(MIN_EXTENT),
                (content.size.h + pad_h * 2.0).max(MIN_EXTENT),
            )),
        );

        let scale = (width as f64 / padded.size.w).min(height as f64 / padded.size.h);
        let letterbox_x = (width as f64 - padded.size.w * scale) / 2.0;
        let letterbox_y = (height as f64 - padded.size.h * scale) / 2.0;
        // Folds letterbox centering into world_origin so world<->canvas is
        // one multiply/divide each way, with no separate offset to carry
        // (and no separate offset to get wrong at click-inversion time).
        let world_origin = Point::from((
            padded.loc.x - letterbox_x / scale,
            padded.loc.y - letterbox_y / scale,
        ));

        let mut pixels = vec![0u8; (width * height * 4) as usize];
        if style.background_top == style.background_bottom {
            fill_rect(
                &mut pixels,
                width,
                height,
                Rectangle::new(Point::from((0, 0)), Size::from((width, height))),
                rgba_a(style.background_top, style.background_alpha),
            );
        } else {
            fill_gradient(
                &mut pixels,
                width,
                height,
                style.background_top,
                style.background_bottom,
                style.background_alpha,
            );
        }

        let world_to_canvas = |rect: Rectangle<i32, Logical>| -> Rectangle<i32, Logical> {
            let rect = rect.to_f64();
            let x0 = (rect.loc.x - world_origin.x) * scale;
            let y0 = (rect.loc.y - world_origin.y) * scale;
            Rectangle::new(
                Point::from((x0.round() as i32, y0.round() as i32)),
                Size::from((
                    (rect.size.w * scale).round().max(1.0) as i32,
                    (rect.size.h * scale).round().max(1.0) as i32,
                )),
            )
        };

        let font = crate::toast::font();
        for (rect, title, urgent) in windows {
            let canvas_rect = world_to_canvas(*rect);
            let border = if *urgent {
                style.accent
            } else {
                style.window_border
            };
            let rim = if *urgent {
                Some(style.accent)
            } else {
                style.soft_rim
            };
            if style.radius > 0.0 || style.soft_rim.is_some() {
                draw_rounded_box(
                    &mut pixels,
                    width,
                    height,
                    canvas_rect,
                    Some((style.window_fill, 235)),
                    (border, 235),
                    style.radius,
                    rim.map(|color| (color, 130)),
                );
            } else {
                fill_rect(
                    &mut pixels,
                    width,
                    height,
                    canvas_rect,
                    rgba(as_tuple(style.window_fill)),
                );
                stroke_rect(&mut pixels, width, height, canvas_rect, as_tuple(border), 1);
            }
            draw_label(&mut pixels, width, height, font, title, canvas_rect);
        }
        for (name, rect) in viewports {
            let canvas_rect = world_to_canvas(*rect);
            let active = name == &output_name;
            let border = if active {
                style.accent
            } else {
                style.window_border
            };
            let thickness = if active { 2 } else { 1 };
            if style.radius > 0.0 {
                draw_rounded_box(
                    &mut pixels,
                    width,
                    height,
                    canvas_rect,
                    None,
                    (border, 255),
                    style.radius,
                    active.then_some((style.accent, 170)),
                );
            } else {
                stroke_rect(
                    &mut pixels,
                    width,
                    height,
                    canvas_rect,
                    as_tuple(border),
                    thickness,
                );
            }
            draw_label(&mut pixels, width, height, font, name, canvas_rect);
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
            buffer,
            canvas_size: Size::from((width, height)),
            cursor_buffer: build_cursor_glyph(),
            world_origin,
            scale,
            last_location: pointer_location,
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

    pub fn set_last_location(&mut self, location: Point<f64, Logical>) {
        if self.last_location != location {
            self.last_location = location;
        }
    }

    /// The crosshair element drawn over the map at the pointer's current
    /// canvas position. The map fills the whole output, so the pointer's
    /// canvas-space position is also its output-local position; the glyph
    /// is centered on it. `None` while the pointer sits outside the output.
    pub fn cursor_element(
        &self,
        renderer: &mut GlesRenderer,
        output_loc: Point<i32, Logical>,
        scale: f64,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let loc = self.last_location;
        let size = self.canvas_size;
        if loc.x < output_loc.x as f64
            || loc.y < output_loc.y as f64
            || loc.x > (output_loc.x + size.w) as f64
            || loc.y > (output_loc.y + size.h) as f64
        {
            return None;
        }
        let half = CURSOR_GLYPH as f64 / 2.0;
        let physical_loc = (loc - output_loc.to_f64() - Point::from((half, half))).to_physical(scale);
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            physical_loc,
            &self.cursor_buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }

    /// Inverts `world_to_canvas` at the peek's last known pointer position
    /// -- the world point a click should travel the camera to.
    pub fn world_point_at_last_location(&self) -> Point<f64, Logical> {
        Point::from((
            self.world_origin.x + self.last_location.x / self.scale,
            self.world_origin.y + self.last_location.y / self.scale,
        ))
    }
}

fn as_tuple([r, g, b]: [u8; 3]) -> (u8, u8, u8) {
    (r, g, b)
}

fn rgba_a([r, g, b]: [u8; 3], a: u8) -> (u8, u8, u8, u8) {
    (r, g, b, a)
}

fn fill_gradient(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    top: [u8; 3],
    bottom: [u8; 3],
    alpha: u8,
) {
    for y in 0..height {
        let t = y as f32 / (height - 1).max(1) as f32;
        let color = mix(top, bottom, t);
        for x in 0..width {
            put_pixel(pixels, width, x, y, rgba_a(color, alpha));
        }
    }
}

/// Rounded box with an optional fill, a rounded border ring, and an
/// optional soft colored rim spread `RIM_SPREAD` px beyond its own edge --
/// the same "coverage of an expanded rounded rect, fading a few px out"
/// technique `toast.rs`'s card shadow uses, just tinted instead of black.
/// `fill: None` draws the border/rim only (used for viewport outlines,
/// which frame a screen region rather than represent a discrete object).
#[allow(clippy::too_many_arguments)]
fn draw_rounded_box(
    pixels: &mut [u8],
    canvas_w: i32,
    canvas_h: i32,
    rect: Rectangle<i32, Logical>,
    fill: Option<([u8; 3], u8)>,
    border: ([u8; 3], u8),
    radius: f32,
    rim: Option<([u8; 3], u8)>,
) {
    let (left, top, w, h) = (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);
    let radius = radius.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);

    if let Some((rim_color, rim_alpha)) = rim {
        let x0 = (left - RIM_SPREAD).max(0);
        let y0 = (top - RIM_SPREAD).max(0);
        let x1 = (left + w + RIM_SPREAD).min(canvas_w);
        let y1 = (top + h + RIM_SPREAD).min(canvas_h);
        for y in y0..y1 {
            for x in x0..x1 {
                // Inside the original box: the fill/border pass below
                // covers it, so the rim contributes nothing there. This
                // can't be "outer minus inner" coverage from the same
                // corner-only SDF `rounded_rect_coverage_local` computes --
                // along a *straight* edge (away from either rect's own
                // corners) that function returns "fully inside" for both
                // the inner and outer test, so the difference cancels to
                // zero and the glow would only ever show up at corners.
                let inside_original = x >= left && x < left + w && y >= top && y < top + h;
                if inside_original {
                    continue;
                }
                let outer = rounded_rect_coverage_local(
                    x,
                    y,
                    left - RIM_SPREAD,
                    top - RIM_SPREAD,
                    w + RIM_SPREAD * 2,
                    h + RIM_SPREAD * 2,
                    radius + RIM_SPREAD as f32,
                );
                if outer > 0.0 {
                    blend_pixel(
                        pixels,
                        canvas_w,
                        x,
                        y,
                        rim_color,
                        (outer * rim_alpha as f32) as u8,
                    );
                }
            }
        }
    }

    let x0 = left.max(0);
    let y0 = top.max(0);
    let x1 = (left + w).min(canvas_w);
    let y1 = (top + h).min(canvas_h);
    let (border_color, border_alpha) = border;
    for y in y0..y1 {
        for x in x0..x1 {
            let coverage = rounded_rect_coverage_local(x, y, left, top, w, h, radius);
            if coverage <= 0.0 {
                continue;
            }
            if let Some((fill_color, fill_alpha)) = fill {
                blend_pixel(
                    pixels,
                    canvas_w,
                    x,
                    y,
                    fill_color,
                    (coverage * fill_alpha as f32) as u8,
                );
            }
            let inset = rounded_rect_coverage_local(
                x,
                y,
                left + 1,
                top + 1,
                (w - 2).max(0),
                (h - 2).max(0),
                (radius - 1.0).max(0.0),
            );
            let ring = (coverage - inset).clamp(0.0, 1.0);
            if ring > 0.0 {
                blend_pixel(
                    pixels,
                    canvas_w,
                    x,
                    y,
                    border_color,
                    (ring * border_alpha as f32) as u8,
                );
            }
        }
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

fn blend_pixel(pixels: &mut [u8], width: i32, x: i32, y: i32, [r, g, b]: [u8; 3], a: u8) {
    let i = ((y * width + x) * 4) as usize;
    let t = a as f32 / 255.0;
    for (channel, target) in [b, g, r].into_iter().enumerate() {
        let bg = pixels[i + channel] as f32;
        pixels[i + channel] = (bg + (target as f32 - bg) * t) as u8;
    }
    pixels[i + 3] = pixels[i + 3].max(a);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(
        windows: &[(Rectangle<i32, Logical>, String, bool)],
        viewports: &[(String, Rectangle<i32, Logical>)],
        pointer: Point<f64, Logical>,
    ) -> MinimapPeek {
        MinimapPeek::build(
            "output-1".to_string(),
            (1920, 1080),
            pointer,
            MinimapPreset::Bioluminescent.base_style(),
            windows,
            &[],
            viewports,
        )
    }

    /// Click squarely on a single window with nothing else in the world:
    /// the resolved world point must fall inside that window's own rect.
    /// This is the core transform-correctness check -- a scale or offset
    /// mixup (forgetting padding, forgetting the letterbox term) fails it
    /// immediately instead of only showing up as "clicks feel slightly
    /// off" during manual testing.
    #[test]
    fn click_on_the_only_window_resolves_inside_it() {
        let window_rect =
            Rectangle::<i32, Logical>::new(Point::from((5000, -3000)), Size::from((1200, 800)));
        let mut peek = build(
            &[(window_rect, "code".to_string(), false)],
            &[],
            Point::from((0.0, 0.0)),
        );

        // Canvas center is the padded extent's center, which (with only
        // one rect in the world) is the window's own center.
        peek.set_last_location(Point::from((960.0, 540.0)));
        let world = peek.world_point_at_last_location();

        let world_point: Point<i32, Logical> =
            Point::from((world.x.round() as i32, world.y.round() as i32));
        assert!(window_rect.contains(world_point));
    }

    /// A click on a drawn camera-viewport rect must resolve to a world
    /// point inside that output's own current viewport -- the assert most
    /// likely to catch a scale-vs-offset mixup that a pure center
    /// round-trip could still pass (center math cancels a wrong offset;
    /// an off-center point does not).
    #[test]
    fn click_on_a_viewport_rect_resolves_inside_that_viewport() {
        let viewport_b =
            Rectangle::<i32, Logical>::new(Point::from((-4000, 1000)), Size::from((1920, 1080)));
        let mut peek = build(
            &[],
            &[
                (
                    "output-1".to_string(),
                    Rectangle::new(Point::from((0, 0)), Size::from((1920, 1080))),
                ),
                ("output-2".to_string(), viewport_b),
            ],
            Point::from((0.0, 0.0)),
        );

        peek.set_last_location(Point::from((960.0, 540.0)));
        let world = peek.world_point_at_last_location();
        let world_point: Point<i32, Logical> =
            Point::from((world.x.round() as i32, world.y.round() as i32));
        let union = viewport_b.merge(Rectangle::new(
            Point::from((0, 0)),
            Size::from((1920, 1080)),
        ));
        assert!(union.contains(world_point));
    }

    /// Nothing in the world at all (no windows, no reefs, one degenerate
    /// or absent viewport) must not divide scale toward infinity or NaN --
    /// the `MIN_EXTENT` floor's whole reason for existing.
    #[test]
    fn empty_world_does_not_produce_a_degenerate_scale() {
        let peek = build(&[], &[], Point::from((0.0, 0.0)));
        assert!(peek.scale.is_finite() && peek.scale > 0.0);
        assert!(peek.world_origin.x.is_finite() && peek.world_origin.y.is_finite());
    }

    #[test]
    fn preset_parses_known_names_and_rejects_unknown() {
        assert_eq!(MinimapPreset::parse("plain"), Some(MinimapPreset::Plain));
        assert_eq!(MinimapPreset::parse("flat"), Some(MinimapPreset::Plain));
        assert_eq!(
            MinimapPreset::parse("bioluminescent"),
            Some(MinimapPreset::Bioluminescent)
        );
        assert_eq!(
            MinimapPreset::parse("glow"),
            Some(MinimapPreset::Bioluminescent)
        );
        assert_eq!(MinimapPreset::parse("glass"), Some(MinimapPreset::Glass));
        assert_eq!(MinimapPreset::parse("frost"), Some(MinimapPreset::Glass));
        assert_eq!(MinimapPreset::parse("nautical"), None);
    }

    #[test]
    fn color_overrides_replace_only_their_own_fields() {
        let base = MinimapPreset::Bioluminescent.base_style();
        let overridden = MinimapStyle::resolve(
            MinimapPreset::Bioluminescent,
            None,
            Some([1.0, 0.0, 0.0]),
            None,
        );
        assert_eq!(overridden.window_fill, [255, 0, 0]);
        // Everything else stays exactly as the preset's own default.
        assert_eq!(overridden.background_top, base.background_top);
        assert_eq!(overridden.accent, base.accent);
        assert_eq!(overridden.radius, base.radius);
    }
}
