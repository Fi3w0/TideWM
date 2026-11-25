//! Cached CPU-rendered tabs for a grouped window: a bar with titles or a
//! slim per-tab indicator, on any edge, styled by `tabs { }` and following
//! `border { }`/`rounding { }` where a value is unset. Rasterized at the
//! output's physical scale so text stays sharp at fractional scales, and
//! rebuilt only when titles, the active/urgent tab, size, scale or style
//! change. Title commits from active or parked members invalidate the owning
//! group so the next frame uses current labels.

use std::sync::Arc;

use fontdue::Font;
use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size, Transform},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use crate::config::{
    BorderConfig, RoundingConfig, TabPlacement, TabPosition, TabStyle, TabsConfig,
};

fn display_title(title: Option<String>) -> String {
    title
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string())
}

/// The current client title, or a placeholder when absent.
pub fn window_title(surface: &WlSurface) -> String {
    display_title(with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()?
            .lock()
            .ok()?
            .title
            .clone()
    }))
}

/// One tab as drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub title: String,
    pub active: bool,
    pub urgent: bool,
}

/// Everything a cached strip depends on; equal keys reuse the buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct StripKey {
    tabs: Vec<Tab>,
    physical: (i32, i32),
    scale_bits: u64,
    generation: u64,
}

/// A rendered strip and the logical rectangle it covers.
pub struct Strip {
    key: StripKey,
    buffer: MemoryRenderBuffer,
    pub rect: Rectangle<i32, Logical>,
}

/// Resolved colors and geometry for one frame's strips.
#[derive(Clone)]
pub struct TabLook {
    style: TabStyle,
    position: TabPosition,
    placement: TabPlacement,
    size: f32,
    side_width: f32,
    gap: f32,
    margin: f32,
    padding: f32,
    radius: f32,
    font: Arc<Font>,
    font_size: f32,
    active: ([f32; 4], [f32; 4]),
    inactive: ([f32; 4], [f32; 4]),
    urgent: ([f32; 4], [f32; 4]),
    angle: f32,
    text: [f32; 4],
    inactive_text: [f32; 4],
    opacity: f32,
}

impl TabLook {
    pub fn resolve(tabs: &TabsConfig, border: &BorderConfig, rounding: &RoundingConfig) -> Self {
        let size = tabs.size_or_default();
        let theme_radius = if rounding.enabled {
            rounding.radii.iter().copied().fold(f32::INFINITY, f32::min)
        } else {
            0.0
        };
        let radius = tabs
            .radius
            .unwrap_or(theme_radius.min(8.0))
            .clamp(0.0, size / 2.0);
        let pair = |from: Option<[f32; 4]>, to: Option<[f32; 4]>, theme: ([f32; 4], [f32; 4])| {
            let from = from.unwrap_or(theme.0);
            (
                from,
                to.unwrap_or(if from == theme.0 { theme.1 } else { from }),
            )
        };
        Self {
            style: tabs.style,
            position: tabs.position,
            placement: tabs.placement,
            size,
            side_width: tabs.side_width,
            gap: tabs.gap,
            margin: tabs.margin,
            padding: tabs.padding,
            radius,
            font: crate::text::font_from_path(tabs.font.as_deref()),
            font_size: tabs.font_size,
            active: pair(
                tabs.active_from,
                tabs.active_to,
                (border.active_from, border.active_to),
            ),
            inactive: pair(
                tabs.inactive_from,
                tabs.inactive_to,
                (border.inactive_from, border.inactive_to),
            ),
            urgent: pair(
                tabs.urgent_from,
                tabs.urgent_to,
                (border.urgent_from, border.urgent_to),
            ),
            angle: tabs.angle.unwrap_or(border.angle),
            text: tabs.text_color,
            inactive_text: tabs.inactive_text_color,
            opacity: tabs.opacity,
        }
    }

    /// The logical area the tab row covers: inside `window` for inside
    /// tabs, in the band `TabsConfig::reserve` freed next to it otherwise.
    pub fn strip_rect(
        &self,
        window: Rectangle<i32, Logical>,
        count: usize,
    ) -> Rectangle<i32, Logical> {
        if self.placement == TabPlacement::Outside {
            return self.outside_rect(window, count);
        }
        let m = self.margin.round() as i32;
        let thickness = match (self.style, self.position.vertical()) {
            (TabStyle::Bar, true) => self.side_width,
            _ => self.size,
        }
        .round() as i32;
        let inner_w = (window.size.w - 2 * m).max(1);
        let inner_h = (window.size.h - 2 * m).max(1);
        let thickness = thickness.clamp(
            1,
            if self.position.vertical() {
                inner_w
            } else {
                inner_h
            },
        );
        let along = if self.position.vertical() {
            match self.style {
                // Bar rows keep their own height and stack from the top.
                TabStyle::Bar => {
                    let n = count.max(1) as i32;
                    (n * self.size.round() as i32 + (n - 1) * self.gap.round() as i32)
                        .clamp(1, inner_h)
                }
                TabStyle::Indicator => inner_h,
            }
        } else {
            inner_w
        };
        let (x, y, w, h) = match self.position {
            TabPosition::Top => (window.loc.x + m, window.loc.y + m, along, thickness),
            TabPosition::Bottom => (
                window.loc.x + m,
                window.loc.y + window.size.h - m - thickness,
                along,
                thickness,
            ),
            TabPosition::Left => (window.loc.x + m, window.loc.y + m, thickness, along),
            TabPosition::Right => (
                window.loc.x + window.size.w - m - thickness,
                window.loc.y + m,
                thickness,
                along,
            ),
        };
        Rectangle::new((x, y).into(), (w.max(1), h.max(1)).into())
    }
}

impl TabLook {
    /// Tabs drawn over the window edge regardless of `tabs.placement`: for
    /// layouts that cannot give the tabs their own band (Ocean reefs).
    pub fn overlaid(mut self) -> Self {
        self.placement = TabPlacement::Inside;
        self
    }

    fn thickness(&self) -> i32 {
        match (self.style, self.position.vertical()) {
            (TabStyle::Bar, true) => self.side_width,
            _ => self.size,
        }
        .round()
        .max(1.0) as i32
    }

    fn outside_rect(
        &self,
        window: Rectangle<i32, Logical>,
        count: usize,
    ) -> Rectangle<i32, Logical> {
        let m = self.margin.round() as i32;
        let t = self.thickness();
        let along = if self.position.vertical() && self.style == TabStyle::Bar {
            let n = count.max(1) as i32;
            (n * self.size.round() as i32 + (n - 1) * self.gap.round() as i32)
                .clamp(1, window.size.h.max(1))
        } else if self.position.vertical() {
            window.size.h
        } else {
            window.size.w
        };
        let (x, y, w, h) = match self.position {
            TabPosition::Top => (window.loc.x, window.loc.y - m - t, along, t),
            TabPosition::Bottom => (window.loc.x, window.loc.y + window.size.h + m, along, t),
            TabPosition::Left => (window.loc.x - m - t, window.loc.y, t, along),
            TabPosition::Right => (window.loc.x + window.size.w + m, window.loc.y, t, along),
        };
        Rectangle::new((x, y).into(), (w.max(1), h.max(1)).into())
    }
}

impl Strip {
    pub fn key(&self) -> &StripKey {
        &self.key
    }
}

impl StripKey {
    pub fn new(tabs: Vec<Tab>, physical: (i32, i32), scale: f64, generation: u64) -> Self {
        Self {
            tabs,
            physical,
            scale_bits: scale.to_bits(),
            generation,
        }
    }
}

/// Rasterizes `key.tabs` into a buffer of `key.physical` pixels covering
/// `rect` (logical) at `scale`.
pub fn build_strip(
    look: &TabLook,
    key: StripKey,
    rect: Rectangle<i32, Logical>,
    scale: f64,
) -> Option<Strip> {
    let pixels = rasterize(look, &key, scale)?;
    let buffer = MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        key.physical,
        1,
        Transform::Normal,
        None,
    );
    Some(Strip { key, buffer, rect })
}

/// Premultiplied ARGB8888 pixels for `key`, or `None` when the size cannot be
/// allocated.
fn rasterize(look: &TabLook, key: &StripKey, scale: f64) -> Option<Vec<u8>> {
    let (width, height) = key.physical;
    let pixel_len = crate::text::checked_argb_len(width, height)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(pixel_len).ok()?;
    pixels.resize(pixel_len, 0);
    let count = key.tabs.len().max(1);
    let s = scale as f32;
    let gap = (look.gap * s).round() as i32;
    let radius = look.radius * s;
    let vertical = look.position.vertical();
    let row = (look.size * s).round() as i32;
    for (index, tab) in key.tabs.iter().enumerate() {
        let i = index as i32;
        let (x0, y0, w, h) = if vertical {
            match look.style {
                TabStyle::Bar => (0, i * (row + gap), width, row),
                TabStyle::Indicator => segment(height, count, i, gap).map_y(width),
            }
        } else {
            segment(width, count, i, gap).map_x(height)
        };
        if w <= 0 || h <= 0 || y0 >= height || x0 >= width {
            continue;
        }
        let (from, to) = if tab.urgent {
            look.urgent
        } else if tab.active {
            look.active
        } else {
            look.inactive
        };
        fill_tab(
            &mut pixels,
            width,
            height,
            (x0, y0, w, h),
            radius,
            from,
            to,
            look,
        );
        if look.style == TabStyle::Bar {
            let color = if tab.active || tab.urgent {
                look.text
            } else {
                look.inactive_text
            };
            draw_label(
                &mut pixels,
                width,
                height,
                look,
                &tab.title,
                (x0, y0, w, h),
                s,
                color,
            );
        }
    }
    Some(pixels)
}

/// The `index`-th of `count` equal segments along `length`, `gap` apart.
fn segment(length: i32, count: usize, index: i32, gap: i32) -> Segment {
    let count = count as i32;
    let total_gap = gap * (count - 1);
    let usable = (length - total_gap).max(count);
    let base = usable / count;
    let start = index * (base + gap);
    let end = if index == count - 1 {
        length
    } else {
        start + base
    };
    Segment(start, (end - start).max(0))
}

struct Segment(i32, i32);

impl Segment {
    fn map_x(self, height: i32) -> (i32, i32, i32, i32) {
        (self.0, 0, self.1, height)
    }
    fn map_y(self, width: i32) -> (i32, i32, i32, i32) {
        (0, self.0, width, self.1)
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_tab(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    (x0, y0, w, h): (i32, i32, i32, i32),
    radius: f32,
    from: [f32; 4],
    to: [f32; 4],
    look: &TabLook,
) {
    let radius = radius.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
    let (sin, cos) = look.angle.to_radians().sin_cos();
    // Project the corners onto the gradient direction to normalize `t`.
    let project = |x: f32, y: f32| x * cos + y * sin;
    let corners = [
        project(0.0, 0.0),
        project(w as f32, 0.0),
        project(0.0, h as f32),
        project(w as f32, h as f32),
    ];
    let lo = corners.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = corners.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let span = (hi - lo).max(1.0);
    for y in y0.max(0)..(y0 + h).min(height) {
        for x in x0.max(0)..(x0 + w).min(width) {
            let coverage = crate::toast::rounded_rect_coverage_local(x, y, x0, y0, w, h, radius);
            if coverage <= 0.0 {
                continue;
            }
            let t = ((project((x - x0) as f32 + 0.5, (y - y0) as f32 + 0.5) - lo) / span)
                .clamp(0.0, 1.0);
            let mix = |a: f32, b: f32| a + (b - a) * t;
            let color = [
                mix(from[0], to[0]),
                mix(from[1], to[1]),
                mix(from[2], to[2]),
                mix(from[3], to[3]) * look.opacity * coverage,
            ];
            put_premultiplied(pixels, width, x, y, color);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_label(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    look: &TabLook,
    title: &str,
    rect: (i32, i32, i32, i32),
    scale: f32,
    color: [f32; 4],
) {
    crate::text::draw_line(
        pixels,
        width,
        height,
        &look.font,
        title,
        look.font_size * scale,
        rect,
        (look.padding * scale).round() as i32,
        [color[0], color[1], color[2], color[3] * look.opacity],
    );
}

fn put_premultiplied(pixels: &mut [u8], width: i32, x: i32, y: i32, color: [f32; 4]) {
    crate::text::blend_premultiplied(pixels, width, x, y, color);
}

/// The strip's element at output-local physical `location`, drawn at its
/// logical size so a physical-resolution buffer maps 1:1 to pixels.
pub fn render_element(
    strip: &Strip,
    renderer: &mut GlesRenderer,
    location: Point<f64, smithay::utils::Physical>,
) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
    // Without an explicit source Smithay crops the buffer to the destination
    // size, which cut and stretched every strip on a fractional scale.
    let (width, height) = strip.key.physical;
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        location,
        &strip.buffer,
        None,
        Some(Rectangle::from_size(
            (f64::from(width), f64::from(height)).into(),
        )),
        Some(Size::from((strip.rect.size.w, strip.rect.size.h))),
        Kind::Unspecified,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(style: TabStyle, position: TabPosition) -> TabLook {
        let tabs = TabsConfig {
            style,
            position,
            placement: TabPlacement::Inside,
            ..TabsConfig::default()
        };
        TabLook::resolve(&tabs, &BorderConfig::default(), &RoundingConfig::default())
    }

    #[test]
    fn absent_and_empty_titles_share_the_untitled_display() {
        assert_eq!(display_title(None), "(untitled)");
        assert_eq!(display_title(Some(String::new())), "(untitled)");
        assert_eq!(display_title(Some("Tide".into())), "Tide");
    }

    #[test]
    fn strip_rejects_geometry_that_overflows_smithays_i32_stride() {
        let look = look(TabStyle::Bar, TabPosition::Top);
        let key = StripKey::new(Vec::new(), (i32::MAX, 26), 1.0, 0);
        assert!(build_strip(&look, key, Rectangle::from_size((1, 1).into()), 1.0).is_none());
    }

    #[test]
    fn strip_rects_follow_position_style_and_margin() {
        let window = Rectangle::new((100, 50).into(), (800, 600).into());
        let top = look(TabStyle::Bar, TabPosition::Top).strip_rect(window, 3);
        assert_eq!(
            (top.loc.x, top.loc.y, top.size.w, top.size.h),
            (104, 54, 792, 26)
        );
        let bottom = look(TabStyle::Indicator, TabPosition::Bottom).strip_rect(window, 3);
        assert_eq!(
            (bottom.loc.x, bottom.loc.y, bottom.size.w, bottom.size.h),
            (104, 642, 792, 4)
        );
        let left = look(TabStyle::Indicator, TabPosition::Left).strip_rect(window, 2);
        assert_eq!(
            (left.loc.x, left.loc.y, left.size.w, left.size.h),
            (104, 54, 4, 592)
        );
        // A side bar stacks fixed-height rows: 3 * 26 + 2 * 4.
        let side = look(TabStyle::Bar, TabPosition::Right).strip_rect(window, 3);
        assert_eq!((side.loc.x, side.size.w, side.size.h), (716, 180, 86));
    }

    #[test]
    fn outside_tabs_fill_the_band_the_window_gave_up() {
        let slot = Rectangle::new((100, 50).into(), (800, 600).into());
        for position in [
            TabPosition::Top,
            TabPosition::Bottom,
            TabPosition::Left,
            TabPosition::Right,
        ] {
            let tabs = TabsConfig {
                position,
                ..TabsConfig::default()
            };
            let window = tabs.reserve(slot);
            let look =
                TabLook::resolve(&tabs, &BorderConfig::default(), &RoundingConfig::default());
            let strip = look.strip_rect(window, 2);
            assert!(
                slot.contains_rect(window),
                "{position:?}: window left its slot"
            );
            assert!(
                slot.contains_rect(strip),
                "{position:?}: tabs left the slot"
            );
            assert!(
                !strip.overlaps(window),
                "{position:?}: tabs cover the window"
            );
        }
        let inside = TabsConfig {
            placement: TabPlacement::Inside,
            ..TabsConfig::default()
        };
        assert_eq!(inside.reserve(slot), slot);
    }

    #[test]
    fn segments_tile_the_length_with_gaps_and_no_overflow() {
        for count in 1..6 {
            let mut end = 0;
            for i in 0..count as i32 {
                let Segment(start, len) = segment(793, count, i, 4);
                assert!(start >= end, "segment {i} overlaps the previous one");
                end = start + len;
            }
            assert_eq!(end, 793, "segments must reach the end exactly");
        }
    }

    #[test]
    fn unset_colors_follow_the_border_theme() {
        let border = BorderConfig::default();
        let look = TabLook::resolve(&TabsConfig::default(), &border, &RoundingConfig::default());
        assert_eq!(look.active, (border.active_from, border.active_to));
        assert_eq!(look.inactive, (border.inactive_from, border.inactive_to));
        let tabs = TabsConfig {
            active_from: Some([1.0, 0.0, 0.0, 1.0]),
            ..TabsConfig::default()
        };
        // A single custom color is solid, not a gradient into the theme.
        let custom = TabLook::resolve(&tabs, &border, &RoundingConfig::default());
        assert_eq!(custom.active, ([1.0, 0.0, 0.0, 1.0], [1.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn active_and_inactive_tabs_rasterize_different_pixels() {
        let look = look(TabStyle::Indicator, TabPosition::Top);
        let tabs = vec![
            Tab {
                title: "a".into(),
                active: true,
                urgent: false,
            },
            Tab {
                title: "b".into(),
                active: false,
                urgent: false,
            },
        ];
        let key = StripKey::new(tabs, (200, 4), 1.0, 0);
        let pixels = rasterize(&look, &key, 1.0).unwrap();
        let pixel = |x: usize| &pixels[(2 * 200 + x) * 4..(2 * 200 + x) * 4 + 4];
        // Middle of each half: the active (left) tab uses the active theme.
        assert_ne!(pixel(50), pixel(150));
        assert!(pixel(50)[3] > 0 && pixel(150)[3] > 0);
        // The gap between the two tabs stays transparent.
        assert_eq!(pixel(99)[3].min(pixel(100)[3]), 0);
    }
}
