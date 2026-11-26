//! CPU-rendered overview of Ocean windows and live output camera viewports.
//! It is built once when the peek opens; clicking travels the triggering
//! output's camera, while release dismisses without moving. Screen-pinned
//! windows have no world position and are omitted. A camera already moving
//! on another output is sampled at its current position.
//!
//! The minimap owns pointer input while open. Closing restores focus at the
//! unchanged cursor position and suppresses the release paired with a
//! compositor-consumed click.

use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    utils::{Logical, Point, Rectangle, Size, Transform},
};

use crate::overview::{fill_rect, stroke_rect};
use crate::toast::rounded_rect_coverage_local;
use crate::ui_theme::mix;

/// Padding around the world content's bounding box, as a fraction of its
/// own size, so nothing sits flush against the canvas edge.
const MARGIN_FRACTION: f64 = 0.1;
/// Floor for the padded extent's width/height, world-logical pixels. Keeps
/// `scale` (canvas px / world px) finite for a near-empty world or a
/// single tight viewport instead of it blowing up toward infinity.
const MIN_EXTENT: f64 = 256.0;
/// Space between a label and its box edge, logical pixels.
const LABEL_PAD: f32 = 6.0;

/// Named visual baseline for the minimap, selected by `minimap.preset`.
/// Each preset resolves to a concrete `MinimapStyle`; every field can be
/// overridden in config.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MinimapPreset {
    /// The original flat schematic: sharp corners, no glow, identical to
    /// `overview.rs`'s own Classic grid.
    Plain,
    /// Default. Deep-water gradient backdrop, rounded glassy window boxes,
    /// and a cyan/teal bioluminescent rim -- the compass's palette.
    #[default]
    Bioluminescent,
    /// Frosted, low-contrast rounded panels with a neutral drop shadow
    /// instead of a colored glow.
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
        let base = MinimapStyle {
            background_top: [10, 10, 12],
            background_bottom: [10, 10, 12],
            background_alpha: 220,
            window_fill: [52, 52, 58],
            window_alpha: 235,
            window_border: [100, 100, 108],
            viewport_border: [100, 100, 108],
            accent: [60, 170, 200],
            radius: 0.0,
            soft_rim: None,
            rim_spread: 9.0,
            rim_alpha: 130,
            show_titles: true,
            show_output_names: true,
            font: crate::text::bundled_font(),
            font_size: 13.0,
            title_color: [225.0 / 255.0, 225.0 / 255.0, 225.0 / 255.0, 1.0],
            size: 1.0,
            position: MinimapPosition::Center,
            margin: 24.0,
            panel_radius: 0.0,
            cursor_color: [118, 241, 255, 255],
            cursor_size: 20.0,
            cursor_shape: CursorShape::Crosshair,
        };
        match self {
            MinimapPreset::Plain => base,
            MinimapPreset::Bioluminescent => MinimapStyle {
                background_top: [8, 22, 30],
                background_bottom: [18, 46, 56],
                background_alpha: 240,
                window_fill: [26, 64, 76],
                window_border: [90, 210, 225],
                viewport_border: [90, 210, 225],
                accent: [118, 241, 255],
                radius: 10.0,
                soft_rim: Some([90, 225, 240]),
                ..base
            },
            MinimapPreset::Glass => MinimapStyle {
                background_top: [19, 19, 25],
                background_bottom: [28, 28, 36],
                background_alpha: 206,
                window_fill: [58, 58, 68],
                window_border: [190, 190, 200],
                viewport_border: [190, 190, 200],
                accent: [190, 205, 255],
                radius: 12.0,
                soft_rim: Some([0, 0, 0]),
                cursor_color: [190, 205, 255, 255],
                ..base
            },
        }
    }
}

/// Where a minimap smaller than the monitor sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MinimapPosition {
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl MinimapPosition {
    pub fn parse(value: &str) -> Option<Self> {
        Some(
            match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
                "center" | "centre" => Self::Center,
                "top" => Self::Top,
                "bottom" => Self::Bottom,
                "left" => Self::Left,
                "right" => Self::Right,
                "top-left" => Self::TopLeft,
                "top-right" => Self::TopRight,
                "bottom-left" => Self::BottomLeft,
                "bottom-right" => Self::BottomRight,
                _ => return None,
            },
        )
    }

    /// (-1, 0, 1) alignment per axis.
    fn align(self) -> (i32, i32) {
        match self {
            Self::Center => (0, 0),
            Self::Top => (0, -1),
            Self::Bottom => (0, 1),
            Self::Left => (-1, 0),
            Self::Right => (1, 0),
            Self::TopLeft => (-1, -1),
            Self::TopRight => (1, -1),
            Self::BottomLeft => (-1, 1),
            Self::BottomRight => (1, 1),
        }
    }
}

/// The "you are here" pointer marker drawn over the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Crosshair,
    Ring,
    Dot,
    None,
}

impl CursorShape {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "crosshair" | "cross" => Self::Crosshair,
            "ring" | "circle" => Self::Ring,
            "dot" => Self::Dot,
            "none" | "off" => Self::None,
            _ => return None,
        })
    }
}

/// Resolved look for one peek's render: a preset's defaults with every
/// configured override applied. Sizes are logical pixels.
#[derive(Clone, Debug)]
pub struct MinimapStyle {
    pub background_top: [u8; 3],
    pub background_bottom: [u8; 3],
    pub background_alpha: u8,
    pub window_fill: [u8; 3],
    pub window_alpha: u8,
    pub window_border: [u8; 3],
    /// Frames of the other monitors' views.
    pub viewport_border: [u8; 3],
    /// The triggering monitor's "you are here" frame and urgent windows.
    pub accent: [u8; 3],
    pub radius: f32,
    /// Soft colored rim just outside each box. `None` skips the pass.
    pub soft_rim: Option<[u8; 3]>,
    pub rim_spread: f32,
    pub rim_alpha: u8,
    pub show_titles: bool,
    pub show_output_names: bool,
    pub font: std::sync::Arc<fontdue::Font>,
    pub font_size: f32,
    pub title_color: [f32; 4],
    pub size: f32,
    pub position: MinimapPosition,
    pub margin: f32,
    pub panel_radius: f32,
    pub cursor_color: [u8; 4],
    pub cursor_size: f32,
    pub cursor_shape: CursorShape,
}

impl MinimapStyle {
    pub fn resolve(cfg: &crate::config::MinimapConfig) -> Self {
        let mut style = cfg.preset.base_style();
        if let Some(color) = cfg.background_color {
            let color = to_u8(color);
            style.background_top = color;
            style.background_bottom = color;
        }
        if let Some(color) = cfg.window_color {
            style.window_fill = to_u8(color);
        }
        if let Some(color) = cfg.accent_color {
            let color = to_u8(color);
            style.accent = color;
            style.window_border = color;
            style.viewport_border = color;
            if style.soft_rim.is_some() {
                style.soft_rim = Some(color);
            }
            style.cursor_color = [color[0], color[1], color[2], 255];
        }
        // Individual overrides beat the coarse ones above.
        if let Some(color) = cfg.background_top {
            style.background_top = to_u8(color);
        }
        if let Some(color) = cfg.background_bottom {
            style.background_bottom = to_u8(color);
        }
        if let Some(opacity) = cfg.background_opacity {
            style.background_alpha = unit_to_u8(opacity);
        }
        if let Some(opacity) = cfg.window_opacity {
            style.window_alpha = unit_to_u8(opacity);
        }
        if let Some(color) = cfg.window_border_color {
            style.window_border = to_u8(color);
        }
        if let Some(color) = cfg.viewport_color {
            style.viewport_border = to_u8(color);
        }
        if let Some(radius) = cfg.radius {
            style.radius = radius;
        }
        if let Some(color) = cfg.glow_color {
            style.soft_rim = Some(to_u8(color));
        }
        if let Some(spread) = cfg.glow_spread {
            style.rim_spread = spread;
        }
        if let Some(opacity) = cfg.glow_opacity {
            style.rim_alpha = unit_to_u8(opacity);
            if opacity <= 0.0 {
                style.soft_rim = None;
            } else if style.soft_rim.is_none() {
                style.soft_rim = Some(style.window_border);
            }
        }
        style.show_titles = cfg.show_titles;
        style.show_output_names = cfg.show_output_names;
        style.font = crate::text::font_from_path(cfg.font.as_deref());
        style.font_size = cfg.font_size;
        style.title_color = cfg.title_color;
        style.size = cfg.size.clamp(0.15, 1.0);
        style.position = cfg.position;
        style.margin = cfg.margin;
        style.panel_radius = cfg
            .panel_radius
            .unwrap_or(if style.size < 1.0 { 18.0 } else { 0.0 });
        if let Some(color) = cfg.cursor_color {
            style.cursor_color = [
                unit_to_u8(color[0]),
                unit_to_u8(color[1]),
                unit_to_u8(color[2]),
                unit_to_u8(color[3]),
            ];
        }
        style.cursor_size = cfg.cursor_size;
        style.cursor_shape = cfg.cursor_shape;
        style
    }

    /// The panel's rectangle in output-local logical pixels.
    fn panel(&self, output: Size<i32, Logical>) -> Rectangle<i32, Logical> {
        if self.size >= 1.0 {
            return Rectangle::from_size(output);
        }
        let w = ((output.w as f32 * self.size).round() as i32).max(1);
        let h = ((output.h as f32 * self.size).round() as i32).max(1);
        let m = self.margin.round() as i32;
        let place = |align: i32, outer: i32, inner: i32| match align {
            -1 => m.min((outer - inner).max(0)),
            1 => (outer - inner - m).max(0),
            _ => (outer - inner) / 2,
        };
        let (ax, ay) = self.position.align();
        Rectangle::new(
            (place(ax, output.w, w), place(ay, output.h, h)).into(),
            (w, h).into(),
        )
    }
}

fn to_u8(c: [f32; 3]) -> [u8; 3] {
    [unit_to_u8(c[0]), unit_to_u8(c[1]), unit_to_u8(c[2])]
}

fn unit_to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// One peek's built state: the cached world-map texture plus the transform
/// needed to invert a click back to a world point. Held by
/// `Smallvil::minimap_peek` for the hold gesture's duration.
pub struct MinimapPeek {
    /// Which output this was built for -- also which output's camera
    /// travels on click, decided once at open time.
    output_name: String,
    buffer: MemoryRenderBuffer,
    /// Panel rectangle, output-local logical pixels.
    panel: Rectangle<i32, Logical>,
    /// Buffer size in physical pixels.
    physical: (i32, i32),
    /// Global logical origin of the output. Pointer events arrive globally
    /// and must subtract this (and the panel origin) first.
    output_loc: Point<i32, Logical>,
    /// Pointer marker over the map, rebuilt only per peek.
    cursor_buffer: Option<(MemoryRenderBuffer, i32)>,
    cursor_size: f32,
    /// World point mapped to panel-space (0, 0).
    world_origin: Point<f64, Logical>,
    /// Panel logical pixels per world logical pixel.
    scale: f64,
    /// Last known pointer position, global logical. Kept live by ordinary
    /// motion events -- never read via `current_location()` from inside a
    /// grab/click dispatch (the `TileMoveGrab` 0.15.1 freeze shape).
    last_location: Point<f64, Logical>,
}

fn build_cursor_glyph(style: &MinimapStyle, scale: f64) -> Option<(MemoryRenderBuffer, i32)> {
    if style.cursor_shape == CursorShape::None {
        return None;
    }
    let size = ((style.cursor_size as f64 * scale).round() as i32).max(3);
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 1.0;
    let line = (scale as f32).max(1.0);
    let [r, g, b, a] = style.cursor_color;
    let color = [
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        f32::from(a) / 255.0,
    ];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let coverage = match style.cursor_shape {
                CursorShape::Crosshair => {
                    let ring = 1.0 - ((dist - radius).abs() - line * 0.75).clamp(0.0, 1.0);
                    let core = if dx.abs() <= line * 1.5 && dy.abs() <= line * 1.5 {
                        1.0
                    } else {
                        0.0
                    };
                    let ticks = if (dy.abs() <= line * 0.5 && dx.abs() > radius - line * 2.5)
                        || (dx.abs() <= line * 0.5 && dy.abs() > radius - line * 2.5)
                    {
                        1.0
                    } else {
                        0.0
                    };
                    f32::max(ring, f32::max(core, ticks))
                }
                CursorShape::Ring => 1.0 - ((dist - radius).abs() - line * 0.75).clamp(0.0, 1.0),
                CursorShape::Dot => (radius * 0.55 - dist + 0.5).clamp(0.0, 1.0),
                CursorShape::None => 0.0,
            };
            if coverage > 0.0 {
                crate::text::blend_premultiplied(
                    &mut pixels,
                    size,
                    x,
                    y,
                    [color[0], color[1], color[2], color[3] * coverage],
                );
            }
        }
    }
    Some((
        MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (size, size),
            1,
            Transform::Normal,
            None,
        ),
        size,
    ))
}

impl MinimapPeek {
    /// `windows` are world-space rects paired with a title and whether that
    /// window is urgent (drawn in the accent color). `reef_rects` only widen
    /// the framed extent. `viewports` pairs an output's name with its camera
    /// rect; the entry matching `output_name` is drawn in the accent as a
    /// "you are here" frame. `scale` is the output's scale: the map is
    /// rasterized at physical resolution.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        output_name: String,
        output_geometry: Rectangle<i32, Logical>,
        scale: f64,
        pointer_location: Point<f64, Logical>,
        style: MinimapStyle,
        windows: &[(Rectangle<i32, Logical>, String, bool)],
        reef_rects: &[Rectangle<i32, Logical>],
        viewports: &[(String, Rectangle<i32, Logical>)],
    ) -> Self {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let map = paint(
            &output_name,
            output_geometry.size,
            scale,
            &style,
            windows,
            reef_rects,
            viewports,
        );
        let buffer = MemoryRenderBuffer::from_slice(
            &map.pixels,
            Fourcc::Argb8888,
            map.physical,
            1,
            Transform::Normal,
            None,
        );
        Self {
            output_name,
            buffer,
            panel: map.panel,
            physical: map.physical,
            output_loc: output_geometry.loc,
            cursor_buffer: build_cursor_glyph(&style, scale),
            cursor_size: style.cursor_size,
            world_origin: map.world_origin,
            scale: map.world_scale,
            last_location: pointer_location,
        }
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    pub fn render_element(
        &self,
        renderer: &mut GlesRenderer,
        output_scale: f64,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            self.panel.loc.to_f64().to_physical(output_scale),
            &self.buffer,
            None,
            Some(Rectangle::from_size(
                (f64::from(self.physical.0), f64::from(self.physical.1)).into(),
            )),
            Some(self.panel.size),
            Kind::Unspecified,
        )
        .ok()
    }

    pub fn set_last_location(&mut self, location: Point<f64, Logical>) {
        if self.last_location != location {
            self.last_location = location;
        }
    }

    /// Pointer position relative to the panel's top-left, logical.
    fn panel_local(&self) -> Point<f64, Logical> {
        self.last_location - self.output_loc.to_f64() - self.panel.loc.to_f64()
    }

    /// Whether the last pointer position is over the map panel. A click
    /// outside a smaller panel dismisses without travel.
    pub fn pointer_on_panel(&self) -> bool {
        let local = self.panel_local();
        local.x >= 0.0
            && local.y >= 0.0
            && local.x <= f64::from(self.panel.size.w)
            && local.y <= f64::from(self.panel.size.h)
    }

    /// The pointer marker element, centered on the pointer, while it is over
    /// the panel.
    pub fn cursor_element(
        &self,
        renderer: &mut GlesRenderer,
        output_loc: Point<i32, Logical>,
        scale: f64,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let (buffer, physical) = self.cursor_buffer.as_ref()?;
        if !self.pointer_on_panel() {
            return None;
        }
        let half = f64::from(self.cursor_size) / 2.0;
        let physical_loc = (self.last_location - output_loc.to_f64() - Point::from((half, half)))
            .to_physical(scale);
        let logical = (self.cursor_size.round() as i32).max(1);
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            physical_loc,
            buffer,
            None,
            Some(Rectangle::from_size(
                (f64::from(*physical), f64::from(*physical)).into(),
            )),
            Some(Size::from((logical, logical))),
            Kind::Unspecified,
        )
        .ok()
    }

    /// Inverts the world->panel transform at the last pointer position --
    /// the world point a click should travel the camera to.
    pub fn world_point_at_last_location(&self) -> Point<f64, Logical> {
        let local = self.panel_local();
        Point::from((
            self.world_origin.x + local.x / self.scale,
            self.world_origin.y + local.y / self.scale,
        ))
    }
}

/// The map's pixels and the transform that drew them.
struct MapRaster {
    pixels: Vec<u8>,
    physical: (i32, i32),
    panel: Rectangle<i32, Logical>,
    world_origin: Point<f64, Logical>,
    world_scale: f64,
}

/// Draws the map panel at physical `scale` (see `MinimapPeek::build`).
#[allow(clippy::too_many_arguments)]
fn paint(
    output_name: &str,
    output_size: Size<i32, Logical>,
    scale: f64,
    style: &MinimapStyle,
    windows: &[(Rectangle<i32, Logical>, String, bool)],
    reef_rects: &[Rectangle<i32, Logical>],
    viewports: &[(String, Rectangle<i32, Logical>)],
) -> MapRaster {
    let panel = style.panel(output_size);
    let (width, height) = (
        ((f64::from(panel.size.w) * scale).round() as i32).max(1),
        ((f64::from(panel.size.h) * scale).round() as i32).max(1),
    );

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

    // World <-> panel (logical); the raster multiplies by `scale`.
    let (panel_w, panel_h) = (f64::from(panel.size.w), f64::from(panel.size.h));
    let world_scale = (panel_w / padded.size.w).min(panel_h / padded.size.h);
    let letterbox_x = (panel_w - padded.size.w * world_scale) / 2.0;
    let letterbox_y = (panel_h - padded.size.h * world_scale) / 2.0;
    // Folds letterbox centering into world_origin so world<->panel is
    // one multiply/divide each way.
    let world_origin = Point::from((
        padded.loc.x - letterbox_x / world_scale,
        padded.loc.y - letterbox_y / world_scale,
    ));

    let mut pixels = vec![0u8; (width * height * 4) as usize];
    fill_gradient(
        &mut pixels,
        width,
        height,
        style.background_top,
        style.background_bottom,
        style.background_alpha,
    );
    let panel_radius = style.panel_radius * scale as f32;
    if panel_radius > 0.0 {
        // Fade the panel's corners out.
        for y in 0..height {
            for x in 0..width {
                let coverage = rounded_rect_coverage_local(x, y, 0, 0, width, height, panel_radius);
                if coverage < 1.0 {
                    // Fade color with alpha: the buffer is composited as
                    // premultiplied, so leftover color would show as a hard
                    // square corner.
                    let i = ((y * width + x) * 4) as usize;
                    for channel in &mut pixels[i..i + 4] {
                        *channel = (f32::from(*channel) * coverage) as u8;
                    }
                }
            }
        }
    }

    let px = scale * world_scale;
    let world_to_canvas = |rect: Rectangle<i32, Logical>| -> Rectangle<i32, Logical> {
        let rect = rect.to_f64();
        let x0 = (rect.loc.x - world_origin.x) * px;
        let y0 = (rect.loc.y - world_origin.y) * px;
        Rectangle::new(
            Point::from((x0.round() as i32, y0.round() as i32)),
            Size::from((
                (rect.size.w * px).round().max(1.0) as i32,
                (rect.size.h * px).round().max(1.0) as i32,
            )),
        )
    };
    let s = scale as f32;
    let radius = style.radius * s;
    let spread = (style.rim_spread * s).round() as i32;
    let thin = (s.round() as i32).max(1);
    let font_px = style.font_size * s;
    let label_h = (font_px * 1.6).round() as i32 + (LABEL_PAD * s) as i32;
    let label = |pixels: &mut [u8], text: &str, rect: Rectangle<i32, Logical>| {
        crate::text::draw_line(
            pixels,
            width,
            height,
            &style.font,
            text,
            font_px,
            (
                rect.loc.x,
                rect.loc.y,
                rect.size.w,
                label_h.min(rect.size.h),
            ),
            (LABEL_PAD * s).round() as i32,
            style.title_color,
        );
    };

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
                Some((style.window_fill, style.window_alpha)),
                (border, 235),
                radius,
                rim.map(|color| (color, style.rim_alpha)),
                spread,
            );
        } else {
            fill_rect(
                &mut pixels,
                width,
                height,
                canvas_rect,
                rgba_a(style.window_fill, style.window_alpha),
            );
            stroke_rect(
                &mut pixels,
                width,
                height,
                canvas_rect,
                as_tuple(border),
                thin,
            );
        }
        if style.show_titles {
            label(&mut pixels, title, canvas_rect);
        }
    }
    for (name, rect) in viewports {
        let canvas_rect = world_to_canvas(*rect);
        let active = name == output_name;
        let border = if active {
            style.accent
        } else {
            style.viewport_border
        };
        if style.radius > 0.0 {
            draw_rounded_box(
                &mut pixels,
                width,
                height,
                canvas_rect,
                None,
                (border, 255),
                radius,
                active.then_some((style.accent, 170)),
                spread,
            );
        } else {
            stroke_rect(
                &mut pixels,
                width,
                height,
                canvas_rect,
                as_tuple(border),
                if active { thin * 2 } else { thin },
            );
        }
        if style.show_output_names {
            label(&mut pixels, name, canvas_rect);
        }
    }

    MapRaster {
        pixels,
        physical: (width, height),
        panel,
        world_origin,
        world_scale,
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
/// optional soft colored rim spread `rim_spread` px beyond its own edge --
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
    rim_spread: i32,
) {
    let (left, top, w, h) = (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);
    let radius = radius.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);

    if let Some((rim_color, rim_alpha)) = rim {
        let x0 = (left - rim_spread).max(0);
        let y0 = (top - rim_spread).max(0);
        let x1 = (left + w + rim_spread).min(canvas_w);
        let y1 = (top + h + rim_spread).min(canvas_h);
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
                // Skip only what the rounded box itself covers: skipping its
                // whole square left the corner pixels between box and rim
                // unpainted, as dark notches.
                let inside = rounded_rect_coverage_local(x, y, left, top, w, h, radius);
                if inside >= 1.0 {
                    continue;
                }
                let outer = rounded_rect_coverage_local(
                    x,
                    y,
                    left - rim_spread,
                    top - rim_spread,
                    w + rim_spread * 2,
                    h + rim_spread * 2,
                    radius + rim_spread as f32,
                );
                if outer > 0.0 {
                    // Light falls off with distance from the box, so the rim
                    // reads as a glow rather than a second, solid border.
                    let distance = rounded_rect_distance(x, y, left, top, w, h, radius);
                    let falloff = (1.0 - distance / rim_spread.max(1) as f32).clamp(0.0, 1.0);
                    let falloff = falloff * falloff;
                    blend_pixel(
                        pixels,
                        canvas_w,
                        x,
                        y,
                        rim_color,
                        (outer * (1.0 - inside) * falloff * rim_alpha as f32) as u8,
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

/// Distance in pixels from the pixel center to a rounded rectangle's edge,
/// 0 inside it.
fn rounded_rect_distance(x: i32, y: i32, left: i32, top: i32, w: i32, h: i32, radius: f32) -> f32 {
    let (fx, fy) = ((x - left) as f32 + 0.5, (y - top) as f32 + 0.5);
    let (hw, hh) = (w as f32 / 2.0, h as f32 / 2.0);
    let radius = radius.min(hw).min(hh).max(0.0);
    let qx = ((fx - hw).abs() - (hw - radius)).max(0.0);
    let qy = ((fy - hh).abs() - (hh - radius)).max(0.0);
    ((qx * qx + qy * qy).sqrt() - radius).max(0.0)
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
        let output_geometry = Rectangle::new((-430, 275).into(), (1111, 777).into());
        MinimapPeek::build(
            "output-1".to_string(),
            output_geometry,
            1.25,
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
        peek.set_last_location(Point::from((125.5, 663.5)));
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

        peek.set_last_location(Point::from((125.5, 663.5)));
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
    fn smaller_panels_sit_where_configured_and_clicks_beside_them_miss() {
        let output = Size::from((1000, 600));
        let mut style = MinimapPreset::Bioluminescent.base_style();
        style.size = 0.5;
        style.margin = 20.0;
        style.position = MinimapPosition::BottomRight;
        let panel = style.panel(output);
        assert_eq!(
            (panel.loc.x, panel.loc.y, panel.size.w, panel.size.h),
            (480, 280, 500, 300)
        );
        style.position = MinimapPosition::Center;
        let centered = style.panel(output);
        assert_eq!((centered.loc.x, centered.loc.y), (250, 150));

        let output_geometry = Rectangle::new((100, 0).into(), output);
        let window = Rectangle::<i32, Logical>::new((0, 0).into(), (800, 600).into());
        let mut peek = MinimapPeek::build(
            "out".into(),
            output_geometry,
            1.0,
            Point::from((0.0, 0.0)),
            style,
            &[(window, "a".into(), false)],
            &[],
            &[],
        );
        peek.set_last_location(Point::from((100.0 + 500.0, 300.0)));
        assert!(peek.pointer_on_panel());
        let world = peek.world_point_at_last_location();
        assert!(
            window.to_f64().contains(world),
            "panel center maps to the world center"
        );
        peek.set_last_location(Point::from((100.0 + 10.0, 10.0)));
        assert!(!peek.pointer_on_panel());
    }

    #[test]
    fn individual_overrides_beat_presets_and_glow_can_be_turned_off() {
        let style = MinimapStyle::resolve(&crate::config::MinimapConfig {
            accent_color: Some([0.0, 1.0, 0.0]),
            window_border_color: Some([1.0, 0.0, 0.0]),
            glow_opacity: Some(0.0),
            radius: Some(3.0),
            ..Default::default()
        });
        assert_eq!(style.accent, [0, 255, 0]);
        assert_eq!(style.window_border, [255, 0, 0], "specific beats coarse");
        assert!(style.soft_rim.is_none());
        assert_eq!(style.radius, 3.0);
        assert_eq!(CursorShape::parse("dot"), Some(CursorShape::Dot));
        assert_eq!(
            MinimapPosition::parse("top_right"),
            Some(MinimapPosition::TopRight)
        );
    }

    /// Writes the painted panel for a few configs as raw BGRA files into
    /// `$TIDEWM_MINIMAP_DUMP`, so the look can be inspected without holding
    /// the trigger key. Run with `--ignored`.
    #[test]
    #[ignore]
    fn dump_minimap_looks() {
        let Some(dir) = std::env::var_os("TIDEWM_MINIMAP_DUMP") else {
            return;
        };
        let windows = vec![
            (
                Rectangle::new((12, 12).into(), (526, 696).into()),
                "Editor".to_string(),
                false,
            ),
            (
                Rectangle::new((562, 12).into(), (526, 696).into()),
                "Music player".to_string(),
                true,
            ),
            (
                Rectangle::new((562, 744).into(), (526, 336).into()),
                "Chat".to_string(),
                false,
            ),
        ];
        let viewports = vec![(
            "winit-0".to_string(),
            Rectangle::new((960, -480).into(), (1100, 720).into()),
        )];
        let configs: Vec<(&str, crate::config::MinimapConfig)> = vec![
            ("bioluminescent", Default::default()),
            (
                "glass",
                crate::config::MinimapConfig {
                    preset: MinimapPreset::Glass,
                    ..Default::default()
                },
            ),
            (
                "plain",
                crate::config::MinimapConfig {
                    preset: MinimapPreset::Plain,
                    ..Default::default()
                },
            ),
            (
                "custom",
                crate::config::MinimapConfig {
                    size: 0.55,
                    position: MinimapPosition::BottomRight,
                    background_top: Some([0.10, 0.04, 0.16]),
                    background_bottom: Some([0.20, 0.06, 0.24]),
                    window_color: Some([0.30, 0.10, 0.32]),
                    accent_color: Some([1.0, 0.56, 0.82]),
                    radius: Some(14.0),
                    glow_spread: Some(14.0),
                    font_size: 15.0,
                    title_color: [1.0, 0.92, 0.97, 1.0],
                    ..Default::default()
                },
            ),
        ];
        for (name, cfg) in configs {
            let style = MinimapStyle::resolve(&cfg);
            let map = paint(
                "winit-0",
                Size::from((1100, 720)),
                1.25,
                &style,
                &windows,
                &[],
                &viewports,
            );
            let path = std::path::Path::new(&dir).join(format!(
                "{name}_{}x{}_{}_{}.bgra",
                map.physical.0, map.physical.1, map.panel.loc.x, map.panel.loc.y
            ));
            std::fs::write(path, &map.pixels).unwrap();
        }
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
        let overridden = MinimapStyle::resolve(&crate::config::MinimapConfig {
            window_color: Some([1.0, 0.0, 0.0]),
            ..Default::default()
        });
        assert_eq!(overridden.window_fill, [255, 0, 0]);
        // Everything else stays exactly as the preset's own default.
        assert_eq!(overridden.background_top, base.background_top);
        assert_eq!(overridden.accent, base.accent);
        assert_eq!(overridden.radius, base.radius);
    }
}
