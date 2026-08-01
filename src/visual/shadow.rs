//! Configurable analytical window shadows for TideWM.
//!
//! The useful parts of niri's CSS-like shadow model (softness, spread,
//! offset, draw-behind-window, active/inactive colors) and Hyprland's
//! falloff controls (render power, sharp mode, scale) are combined in one
//! fixed-cost signed-distance-field shader. No blur texture, framebuffer,
//! or per-window cache is allocated: every shadow is one procedural pixel
//! element placed directly behind its window in the desktop z-order.

use std::hash::{Hash, Hasher};

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
        },
        utils::CommitCounter,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{user_data::UserDataMap, Buffer, Physical, Rectangle, Scale},
};

use crate::config::ShadowConfig;

/// Stable per-surface identity whose namespace changes whenever a visual
/// uniform or focus/urgent state changes. This lets Smithay's damage tracker
/// notice a color-only config reload without retaining one fresh element id
/// per frame (the exact leak shape backdrop elements already avoid).
pub fn shadow_id(surface: &WlSurface, config: &ShadowConfig, focused: bool, urgent: bool) -> Id {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    for value in [
        config.softness,
        config.spread,
        config.offset.0,
        config.offset.1,
        config.scale,
        config.render_power,
        config.opacity,
        config.inactive_opacity,
        config.urgent_opacity,
        config.corner_radius,
    ] {
        value.to_bits().hash(&mut hash);
    }
    for color in [config.color, config.inactive_color, config.urgent_color] {
        for channel in color {
            channel.to_bits().hash(&mut hash);
        }
    }
    config.enabled.hash(&mut hash);
    config.sharp.hash(&mut hash);
    config.draw_behind_window.hash(&mut hash);
    config.floating_only.hash(&mut hash);
    config.fullscreen.hash(&mut hash);
    focused.hash(&mut hash);
    urgent.hash(&mut hash);
    Id::from(surface).namespaced(hash.finish() as usize)
}

const SHADOW_FRAGMENT_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec2 u_shadow_center;
uniform vec2 u_shadow_half_size;
uniform vec2 u_window_center;
uniform vec2 u_window_half_size;
uniform float u_shadow_radius;
uniform float u_window_radius;
uniform float u_softness;
uniform float u_render_power;
uniform float u_sharp;
uniform float u_draw_behind_window;
uniform vec4 u_color;

float rounded_box_distance(vec2 point, vec2 center, vec2 half_size, float radius) {
    radius = min(radius, min(half_size.x, half_size.y));
    vec2 q = abs(point - center) - (half_size - vec2(radius));
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
}

void main() {
    vec2 point = v_coords * size;
    float distance = rounded_box_distance(
        point,
        u_shadow_center,
        u_shadow_half_size,
        u_shadow_radius
    );

    float coverage;
    if (u_sharp > 0.5 || u_softness < 0.01) {
        coverage = 1.0 - step(0.0, distance);
    } else {
        float edge = clamp(1.0 - max(distance, 0.0) / u_softness, 0.0, 1.0);
        coverage = pow(edge, u_render_power);
    }

    if (u_draw_behind_window < 0.5) {
        float window_distance = rounded_box_distance(
            point,
            u_window_center,
            u_window_half_size,
            u_window_radius
        );
        // A one-physical-pixel antialiased cutout keeps the shadow from
        // tinting translucent/frosted content while avoiding a jagged edge.
        coverage *= smoothstep(-1.0, 1.0, window_distance);
    }

    float out_alpha = coverage * u_color.a * alpha;
    gl_FragColor = vec4(u_color.rgb * out_alpha, out_alpha);
}
"#;

pub fn shadow_program(
    cache: &mut Option<GlesPixelProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesPixelProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_pixel_shader(
        SHADOW_FRAGMENT_SHADER,
        &[
            UniformName::new("u_shadow_center", UniformType::_2f),
            UniformName::new("u_shadow_half_size", UniformType::_2f),
            UniformName::new("u_window_center", UniformType::_2f),
            UniformName::new("u_window_half_size", UniformType::_2f),
            UniformName::new("u_shadow_radius", UniformType::_1f),
            UniformName::new("u_window_radius", UniformType::_1f),
            UniformName::new("u_softness", UniformType::_1f),
            UniformName::new("u_render_power", UniformType::_1f),
            UniformName::new("u_sharp", UniformType::_1f),
            UniformName::new("u_draw_behind_window", UniformType::_1f),
            UniformName::new("u_color", UniformType::_4f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile window-shadow shader");
            None
        }
    }
}

pub struct ShadowElement {
    id: Id,
    area: Rectangle<i32, Physical>,
    program: GlesPixelProgram,
    shadow_center: [f32; 2],
    shadow_half_size: [f32; 2],
    window_center: [f32; 2],
    window_half_size: [f32; 2],
    shadow_radius: f32,
    window_radius: f32,
    softness: f32,
    render_power: f32,
    sharp: bool,
    draw_behind_window: bool,
    color: [f32; 4],
}

impl ShadowElement {
    pub fn new(
        id: Id,
        window: Rectangle<i32, Physical>,
        output_scale: f64,
        program: GlesPixelProgram,
        config: ShadowConfig,
        focused: bool,
        urgent: bool,
    ) -> Self {
        let px_scale = output_scale as f32;
        let window_half_size = [
            window.size.w.max(1) as f32 * 0.5,
            window.size.h.max(1) as f32 * 0.5,
        ];
        let window_center_absolute = [
            window.loc.x as f32 + window_half_size[0],
            window.loc.y as f32 + window_half_size[1],
        ];
        let spread = config.spread * px_scale;
        let shadow_half_size = [
            (window_half_size[0] * config.scale + spread).max(0.5),
            (window_half_size[1] * config.scale + spread).max(0.5),
        ];
        let shadow_center_absolute = [
            window_center_absolute[0] + config.offset.0 * px_scale,
            window_center_absolute[1] + config.offset.1 * px_scale,
        ];
        let softness = if config.sharp {
            0.0
        } else {
            config.softness * px_scale
        };
        let padding = softness.max(1.0) + 2.0;
        let left = (shadow_center_absolute[0] - shadow_half_size[0] - padding).floor() as i32;
        let top = (shadow_center_absolute[1] - shadow_half_size[1] - padding).floor() as i32;
        let right = (shadow_center_absolute[0] + shadow_half_size[0] + padding).ceil() as i32;
        let bottom = (shadow_center_absolute[1] + shadow_half_size[1] + padding).ceil() as i32;
        let area = Rectangle::new(
            (left, top).into(),
            ((right - left).max(1), (bottom - top).max(1)).into(),
        );
        let origin = [area.loc.x as f32, area.loc.y as f32];
        let mut color = if urgent {
            config.urgent_color
        } else if focused {
            config.color
        } else {
            config.inactive_color
        };
        color[3] *= if urgent {
            config.urgent_opacity
        } else if focused {
            config.opacity
        } else {
            config.inactive_opacity
        };

        Self {
            id,
            area,
            program,
            shadow_center: [
                shadow_center_absolute[0] - origin[0],
                shadow_center_absolute[1] - origin[1],
            ],
            shadow_half_size,
            window_center: [
                window_center_absolute[0] - origin[0],
                window_center_absolute[1] - origin[1],
            ],
            window_half_size,
            shadow_radius: (config.corner_radius * config.scale * px_scale + spread).max(0.0),
            window_radius: (config.corner_radius * px_scale).max(0.0),
            softness,
            render_power: config.render_power,
            sharp: config.sharp,
            draw_behind_window: config.draw_behind_window,
            color,
        }
    }
}

impl Element for ShadowElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        CommitCounter::default()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size((self.area.size.w as f64, self.area.size.h as f64).into())
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area
    }

    fn alpha(&self) -> f32 {
        1.0
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for ShadowElement {
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
            (self.area.size.w, self.area.size.h).into(),
            Some(damage),
            1.0,
            &[
                Uniform::new("u_shadow_center", self.shadow_center),
                Uniform::new("u_shadow_half_size", self.shadow_half_size),
                Uniform::new("u_window_center", self.window_center),
                Uniform::new("u_window_half_size", self.window_half_size),
                Uniform::new("u_shadow_radius", self.shadow_radius),
                Uniform::new("u_window_radius", self.window_radius),
                Uniform::new("u_softness", self.softness),
                Uniform::new("u_render_power", self.render_power),
                Uniform::new("u_sharp", if self.sharp { 1.0 } else { 0.0 }),
                Uniform::new(
                    "u_draw_behind_window",
                    if self.draw_behind_window { 1.0 } else { 0.0 },
                ),
                Uniform::new("u_color", self.color),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_contract_is_analytical_and_complete() {
        assert!(!SHADOW_FRAGMENT_SHADER.contains("#version"));
        assert!(SHADOW_FRAGMENT_SHADER.contains("rounded_box_distance"));
        assert!(SHADOW_FRAGMENT_SHADER.contains("uniform float u_softness"));
        assert!(SHADOW_FRAGMENT_SHADER.contains("uniform float u_render_power"));
        assert!(SHADOW_FRAGMENT_SHADER.contains("uniform float u_sharp"));
        assert!(SHADOW_FRAGMENT_SHADER.contains("uniform vec4 u_color"));
        assert!(!SHADOW_FRAGMENT_SHADER.contains("sampler2D"));
    }
}
