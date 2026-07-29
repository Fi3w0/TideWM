//! R1 attention depth and buoyancy. Mapped windows begin at tier zero,
//! sink one bounded tier at a time after configurable inactivity, and float
//! straight back to the surface on focus/input. Tier one keeps live client
//! content with a cool translucent wash; tier two and deeper replace live
//! pixels with a cached schematic title card. Urgent windows retain a bright
//! bioluminescent border at every tier.
//!
//! The model is deliberately analytical and bounded. One timestamp and one
//! small state record are kept per mapped toplevel. Deep schematic buffers
//! are cached only while needed and are evicted with the window. Their total
//! tiled area is at most one output-sized ARGB buffer per visible depth plane;
//! there is no history or per-frame allocation.

use std::time::{Duration, Instant};

use fontdue::Font;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                Element, Id, Kind, RenderElement,
            },
            gles::{
                GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, Uniform, UniformName,
                UniformType,
            },
            utils::CommitCounter,
        },
    },
    utils::{user_data::UserDataMap, Buffer, Logical, Physical, Rectangle, Scale, Transform},
};

use crate::config::DepthConfig;

const DEPTH_OVERLAY_FRAGMENT_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec3 cool_color;
uniform float cool_alpha;
uniform vec3 urgent_color;
uniform float urgent_alpha;

void main() {
    vec2 edge_px = min(v_coords, vec2(1.0) - v_coords) * size;
    float edge_distance = min(edge_px.x, edge_px.y);
    float border = 1.0 - smoothstep(3.0, 9.0, edge_distance);
    float urgent = border * urgent_alpha;
    float a = clamp(max(cool_alpha, urgent) * alpha, 0.0, 1.0);
    vec3 color = mix(cool_color, urgent_color, step(cool_alpha, urgent));
    gl_FragColor = vec4(color * a, a);
}
"#;

pub fn depth_overlay_program(
    cache: &mut Option<GlesPixelProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesPixelProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_pixel_shader(
        DEPTH_OVERLAY_FRAGMENT_SHADER,
        &[
            UniformName::new("cool_color", UniformType::_3f),
            UniformName::new("cool_alpha", UniformType::_1f),
            UniformName::new("urgent_color", UniformType::_3f),
            UniformName::new("urgent_alpha", UniformType::_1f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile depth overlay shader");
            None
        }
    }
}

pub fn tier_for_elapsed(elapsed: Duration, cfg: &DepthConfig) -> u8 {
    if !cfg.enabled || cfg.max_tier == 0 {
        return 0;
    }
    let sink_after = Duration::from_millis(cfg.sink_after_ms as u64);
    if elapsed < sink_after {
        return 0;
    }
    let interval_ms = cfg.tier_interval_ms.max(1) as u128;
    let after_first = elapsed.saturating_sub(sink_after).as_millis();
    let tier = 1_u128.saturating_add(after_first / interval_ms);
    tier.min(cfg.max_tier as u128) as u8
}

pub struct WindowDepth {
    last_attention: Instant,
    tier: u8,
    pub id: Id,
    pub commit: CommitCounter,
}

impl WindowDepth {
    pub fn new() -> Self {
        Self {
            last_attention: Instant::now(),
            tier: 0,
            id: Id::new(),
            commit: CommitCounter::default(),
        }
    }

    pub fn tier(&self) -> u8 {
        self.tier
    }

    pub fn note_attention(&mut self) -> bool {
        self.last_attention = Instant::now();
        if self.tier == 0 {
            return false;
        }
        self.tier = 0;
        self.commit.increment();
        true
    }

    pub fn update(&mut self, cfg: &DepthConfig) -> bool {
        let wanted = tier_for_elapsed(self.last_attention.elapsed(), cfg);
        if wanted == self.tier {
            return false;
        }
        self.tier = wanted;
        self.commit.increment();
        true
    }

    pub fn reset_disabled(&mut self) -> bool {
        if self.tier == 0 {
            return false;
        }
        self.tier = 0;
        self.commit.increment();
        true
    }

    pub fn visual_changed(&mut self) {
        self.commit.increment();
    }
}

pub struct DepthOverlayElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<i32, Logical>,
    program: GlesPixelProgram,
    cool_color: [f32; 3],
    cool_alpha: f32,
    urgent_color: [f32; 3],
    urgent_alpha: f32,
}

impl DepthOverlayElement {
    pub fn new(
        state: &WindowDepth,
        area: Rectangle<i32, Logical>,
        program: GlesPixelProgram,
        cfg: &DepthConfig,
        urgent: bool,
    ) -> Self {
        Self {
            id: state.id.clone(),
            commit: state.commit,
            area,
            program,
            cool_color: cfg.cool_color,
            cool_alpha: if state.tier() == 1 {
                cfg.cool_alpha
            } else {
                0.0
            },
            urgent_color: cfg.urgent_color,
            urgent_alpha: if urgent { cfg.urgent_alpha } else { 0.0 },
        }
    }
}

impl Element for DepthOverlayElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.area.size.to_f64().to_buffer(1.0, Transform::Normal))
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area.to_physical_precise_round(scale)
    }

    fn alpha(&self) -> f32 {
        1.0
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for DepthOverlayElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.render_pixel_shader_to(
            &self.program,
            src,
            dst,
            self.area.size.to_buffer(1, Transform::Normal),
            Some(damage),
            1.0,
            &[
                Uniform::new("cool_color", self.cool_color),
                Uniform::new("cool_alpha", self.cool_alpha),
                Uniform::new("urgent_color", self.urgent_color),
                Uniform::new("urgent_alpha", self.urgent_alpha),
            ],
        )
    }
}

pub struct DepthSchematic {
    size: (i32, i32),
    title: String,
    tier: u8,
    background: [u8; 3],
    background_alpha: u8,
    border: [u8; 3],
    buffer: MemoryRenderBuffer,
}

impl DepthSchematic {
    pub fn matches(&self, size: (i32, i32), title: &str, tier: u8, cfg: &DepthConfig) -> bool {
        self.size == size
            && self.title == title
            && self.tier == tier
            && self.background == rgb8(cfg.schematic_color)
            && self.background_alpha == alpha8(cfg.schematic_alpha)
            && self.border == rgb8(cfg.border_color)
    }

    pub fn build(size: (i32, i32), title: String, tier: u8, cfg: &DepthConfig) -> Self {
        let width = size.0.max(1);
        let height = size.1.max(1);
        let background = rgb8(cfg.schematic_color);
        let background_alpha = alpha8(cfg.schematic_alpha);
        let border = rgb8(cfg.border_color);
        let mut pixels = vec![0u8; (width * height * 4) as usize];

        let depth_darken = 1.0 / (1.0 + (tier.saturating_sub(2) as f32 * 0.22));
        let background = [
            (background[0] as f32 * depth_darken) as u8,
            (background[1] as f32 * depth_darken) as u8,
            (background[2] as f32 * depth_darken) as u8,
        ];
        fill_rect(
            &mut pixels,
            width,
            height,
            (0, 0, width, height),
            (
                background[0],
                background[1],
                background[2],
                background_alpha,
            ),
        );
        let border_width = 3.min(width / 2).min(height / 2).max(1);
        fill_rect(
            &mut pixels,
            width,
            height,
            (0, 0, width, border_width),
            (border[0], border[1], border[2], 255),
        );
        fill_rect(
            &mut pixels,
            width,
            height,
            (0, height - border_width, width, border_width),
            (border[0], border[1], border[2], 255),
        );
        fill_rect(
            &mut pixels,
            width,
            height,
            (0, 0, border_width, height),
            (border[0], border[1], border[2], 255),
        );
        fill_rect(
            &mut pixels,
            width,
            height,
            (width - border_width, 0, border_width, height),
            (border[0], border[1], border[2], 255),
        );
        draw_label(&mut pixels, width, height, crate::toast::font(), &title);

        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (width, height),
            1,
            Transform::Normal,
            None,
        );
        Self {
            size: (width, height),
            title,
            tier,
            background: rgb8(cfg.schematic_color),
            background_alpha,
            border,
            buffer,
        }
    }

    pub fn render_element(
        &self,
        renderer: &mut GlesRenderer,
        location: (f64, f64),
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &self.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }
}

fn rgb8(color: [f32; 3]) -> [u8; 3] {
    color.map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn alpha8(alpha: f32) -> u8 {
    (alpha.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn fill_rect(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    rect: (i32, i32, i32, i32),
    color: (u8, u8, u8, u8),
) {
    let (x, y, rect_width, rect_height) = rect;
    let x0 = x.clamp(0, width);
    let y0 = y.clamp(0, height);
    let x1 = (x + rect_width).clamp(0, width);
    let y1 = (y + rect_height).clamp(0, height);
    for py in y0..y1 {
        for px in x0..x1 {
            put_pixel(pixels, width, px, py, color);
        }
    }
}

fn draw_label(pixels: &mut [u8], width: i32, height: i32, font: &Font, title: &str) {
    const FONT_SIZE: f32 = 16.0;
    const PAD: i32 = 10;
    let baseline = PAD + FONT_SIZE as i32;
    let mut pen_x = PAD;
    let clip_right = width - PAD;
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
                if x < PAD || y < PAD || x >= clip_right || y >= height - PAD {
                    continue;
                }
                blend_text_pixel(pixels, width, x, y, coverage);
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
    let t = coverage as f32 / 255.0;
    let text = (226.0, 246.0, 250.0);
    pixels[index] = (pixels[index] as f32 + (text.2 - pixels[index] as f32) * t) as u8;
    pixels[index + 1] = (pixels[index + 1] as f32 + (text.1 - pixels[index + 1] as f32) * t) as u8;
    pixels[index + 2] = (pixels[index + 2] as f32 + (text.0 - pixels[index + 2] as f32) * t) as u8;
    pixels[index + 3] = pixels[index + 3].max(coverage);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_tiers_advance_at_configured_boundaries_and_cap() {
        let cfg = DepthConfig {
            sink_after_ms: 100,
            tier_interval_ms: 50,
            max_tier: 3,
            ..DepthConfig::default()
        };
        assert_eq!(tier_for_elapsed(Duration::from_millis(99), &cfg), 0);
        assert_eq!(tier_for_elapsed(Duration::from_millis(100), &cfg), 1);
        assert_eq!(tier_for_elapsed(Duration::from_millis(149), &cfg), 1);
        assert_eq!(tier_for_elapsed(Duration::from_millis(150), &cfg), 2);
        assert_eq!(tier_for_elapsed(Duration::from_millis(200), &cfg), 3);
        assert_eq!(tier_for_elapsed(Duration::from_secs(20), &cfg), 3);
    }

    #[test]
    fn disabled_depth_always_stays_at_surface() {
        let cfg = DepthConfig {
            enabled: false,
            ..DepthConfig::default()
        };
        assert_eq!(tier_for_elapsed(Duration::from_secs(20), &cfg), 0);
    }

    #[test]
    fn overlay_shader_contract_is_stable() {
        assert!(!DEPTH_OVERLAY_FRAGMENT_SHADER.contains("#version"));
        assert!(DEPTH_OVERLAY_FRAGMENT_SHADER.contains("uniform vec3 cool_color"));
        assert!(DEPTH_OVERLAY_FRAGMENT_SHADER.contains("uniform float cool_alpha"));
        assert!(DEPTH_OVERLAY_FRAGMENT_SHADER.contains("uniform vec3 urgent_color"));
        assert!(DEPTH_OVERLAY_FRAGMENT_SHADER.contains("uniform float urgent_alpha"));
    }
}
