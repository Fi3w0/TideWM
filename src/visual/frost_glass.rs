//! Frosted-glass mode for TideWM's shared captured-backdrop pipeline.
//!
//! Unlike water glass, which offsets one sharp sample, frost diffuses the
//! captured backdrop with a bounded 25-tap Gaussian kernel and then applies
//! adjustable strength, opacity, saturation, contrast, brightness, grain,
//! vibrancy, tint, and rounded clipping. A bounded liquid treatment adds
//! edge-local refraction and a directional luminous rim, giving the blurred
//! material optical thickness without another texture or animation clock.
//! The capture is window-sized and reused from `backdrop.rs`; this pass
//! allocates no additional textures per frame.

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName,
            UniformType,
        },
        utils::CommitCounter,
        Texture,
    },
    utils::{user_data::UserDataMap, Buffer, Physical, Rectangle, Scale, Transform},
};

use crate::config::FrostConfig;

use std::hash::{Hash, Hasher};

const FROST_GLASS_FRAGMENT_SHADER: &str = r#"
#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
uniform vec2 u_texel;
uniform vec2 u_size;
uniform float u_liquid;
uniform float u_radius;
uniform float u_strength;
uniform float u_saturation;
uniform float u_contrast;
uniform float u_brightness;
uniform float u_noise;
uniform float u_noise_scale;
uniform float u_vibrancy;
uniform float u_vibrancy_darkness;
uniform vec3 u_tint_color;
uniform float u_tint_alpha;
uniform vec4 u_corner_radii;
uniform float u_rounding_power;
uniform float u_corner_softness;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

vec4 sample_clamped(vec2 origin, vec2 offset) {
    return texture2D(tex, clamp(origin + offset, 0.0, 1.0));
}

float grain(vec2 point) {
    vec2 cell = floor(point / u_noise_scale);
    return fract(sin(dot(cell, vec2(12.9898, 78.233))) * 43758.5453);
}

float rounded_mask() {
    vec2 point = v_coords * u_size;
    vec2 center;
    float radius;
    if (point.x < u_corner_radii.x && point.y < u_corner_radii.x) {
        radius = u_corner_radii.x;
        center = vec2(radius);
    } else if (point.x > u_size.x - u_corner_radii.y && point.y < u_corner_radii.y) {
        radius = u_corner_radii.y;
        center = vec2(u_size.x - radius, radius);
    } else if (point.x > u_size.x - u_corner_radii.z && point.y > u_size.y - u_corner_radii.z) {
        radius = u_corner_radii.z;
        center = vec2(u_size.x - radius, u_size.y - radius);
    } else if (point.x < u_corner_radii.w && point.y > u_size.y - u_corner_radii.w) {
        radius = u_corner_radii.w;
        center = vec2(radius, u_size.y - radius);
    } else {
        return 1.0;
    }
    radius = min(radius, min(u_size.x, u_size.y) * 0.5);
    if (radius < 0.01)
        return 1.0;
    vec2 q = abs(point - center) / radius;
    float distance = pow(
        pow(q.x, u_rounding_power) + pow(q.y, u_rounding_power),
        1.0 / u_rounding_power
    );
    float antialias = u_corner_softness / radius;
    return 1.0 - smoothstep(1.0 - antialias, 1.0 + antialias, distance);
}

void main() {
    vec2 point = v_coords * u_size;
    vec2 edge_pair = min(point, u_size - point);
    float edge_distance = min(edge_pair.x, edge_pair.y);
    float rim_width = clamp(min(u_size.x, u_size.y) * 0.04, 4.0, 18.0);
    float rim = 1.0 - smoothstep(0.0, rim_width, edge_distance);

    // Pick the closest outward edge normal. Near a corner both axes
    // contribute, producing a diagonal normal instead of a hard seam.
    float corner_mix = 1.0 - smoothstep(0.0, rim_width, abs(edge_pair.x - edge_pair.y));
    vec2 horizontal = vec2(point.x < u_size.x * 0.5 ? -1.0 : 1.0, 0.0);
    vec2 vertical = vec2(0.0, point.y < u_size.y * 0.5 ? -1.0 : 1.0);
    vec2 primary = edge_pair.x < edge_pair.y ? horizontal : vertical;
    vec2 diagonal = normalize(horizontal + vertical);
    vec2 edge_normal = normalize(mix(primary, diagonal, corner_mix));

    // An inward sample shift makes the rim behave like a thicker curved
    // pane. It is confined to the edge, so text-area blur remains calm and
    // the effect costs no persistent animation redraw.
    vec2 refracted_coords = clamp(
        v_coords - edge_normal * u_texel * (2.0 + 4.0 * u_liquid) * rim * u_liquid,
        0.0,
        1.0
    );

    // The outer axial taps sit at 2x step, so half-radius here keeps the
    // configured value equal to the kernel's maximum physical reach.
    vec2 step_uv = u_texel * u_radius * 0.5;
    vec4 original = texture2D(tex, refracted_coords);
    vec4 blurred;
    if (u_radius < 0.01) {
        blurred = original;
    } else {
        // Separable [1 4 6 4 1] Gaussian weights expanded into one fixed
        // 5x5 pass. Unlike a sparse radial kernel, adjacent samples overlap
        // across both axes instead of producing visible duplicate images.
        blurred  = sample_clamped(refracted_coords, vec2(-2.0 * step_uv.x, -2.0 * step_uv.y)) * 0.00390625;
        blurred += sample_clamped(refracted_coords, vec2(-1.0 * step_uv.x, -2.0 * step_uv.y)) * 0.015625;
        blurred += sample_clamped(refracted_coords, vec2( 0.0,              -2.0 * step_uv.y)) * 0.0234375;
        blurred += sample_clamped(refracted_coords, vec2( 1.0 * step_uv.x, -2.0 * step_uv.y)) * 0.015625;
        blurred += sample_clamped(refracted_coords, vec2( 2.0 * step_uv.x, -2.0 * step_uv.y)) * 0.00390625;

        blurred += sample_clamped(refracted_coords, vec2(-2.0 * step_uv.x, -1.0 * step_uv.y)) * 0.015625;
        blurred += sample_clamped(refracted_coords, vec2(-1.0 * step_uv.x, -1.0 * step_uv.y)) * 0.0625;
        blurred += sample_clamped(refracted_coords, vec2( 0.0,              -1.0 * step_uv.y)) * 0.09375;
        blurred += sample_clamped(refracted_coords, vec2( 1.0 * step_uv.x, -1.0 * step_uv.y)) * 0.0625;
        blurred += sample_clamped(refracted_coords, vec2( 2.0 * step_uv.x, -1.0 * step_uv.y)) * 0.015625;

        blurred += sample_clamped(refracted_coords, vec2(-2.0 * step_uv.x, 0.0)) * 0.0234375;
        blurred += sample_clamped(refracted_coords, vec2(-1.0 * step_uv.x, 0.0)) * 0.09375;
        blurred += sample_clamped(refracted_coords, vec2( 0.0,              0.0)) * 0.140625;
        blurred += sample_clamped(refracted_coords, vec2( 1.0 * step_uv.x, 0.0)) * 0.09375;
        blurred += sample_clamped(refracted_coords, vec2( 2.0 * step_uv.x, 0.0)) * 0.0234375;

        blurred += sample_clamped(refracted_coords, vec2(-2.0 * step_uv.x, 1.0 * step_uv.y)) * 0.015625;
        blurred += sample_clamped(refracted_coords, vec2(-1.0 * step_uv.x, 1.0 * step_uv.y)) * 0.0625;
        blurred += sample_clamped(refracted_coords, vec2( 0.0,              1.0 * step_uv.y)) * 0.09375;
        blurred += sample_clamped(refracted_coords, vec2( 1.0 * step_uv.x, 1.0 * step_uv.y)) * 0.0625;
        blurred += sample_clamped(refracted_coords, vec2( 2.0 * step_uv.x, 1.0 * step_uv.y)) * 0.015625;

        blurred += sample_clamped(refracted_coords, vec2(-2.0 * step_uv.x, 2.0 * step_uv.y)) * 0.00390625;
        blurred += sample_clamped(refracted_coords, vec2(-1.0 * step_uv.x, 2.0 * step_uv.y)) * 0.015625;
        blurred += sample_clamped(refracted_coords, vec2( 0.0,              2.0 * step_uv.y)) * 0.0234375;
        blurred += sample_clamped(refracted_coords, vec2( 1.0 * step_uv.x, 2.0 * step_uv.y)) * 0.015625;
        blurred += sample_clamped(refracted_coords, vec2( 2.0 * step_uv.x, 2.0 * step_uv.y)) * 0.00390625;
    }

    vec4 color = mix(original, blurred, u_strength);
    float luminance = dot(color.rgb, vec3(0.299, 0.587, 0.114));
    float dark_bias = mix(1.0, 1.0 - luminance, u_vibrancy_darkness);
    float saturation = u_saturation + u_vibrancy * dark_bias;
    color.rgb = mix(vec3(luminance), color.rgb, saturation);
    color.rgb = (color.rgb - vec3(0.5)) * u_contrast + vec3(0.5);
    color.rgb *= u_brightness;
    color.rgb += vec3((grain(gl_FragCoord.xy) - 0.5) * 2.0 * u_noise);
    color.rgb = mix(color.rgb, u_tint_color, u_tint_alpha);

    // A stationary top-left key light and a restrained opposite-edge shade
    // make the refracted band readable as a glass rim. Because both are
    // geometry-derived, a static desktop remains damage-idle.
    vec2 light_direction = normalize(vec2(-0.72, -0.69));
    float highlight = pow(max(dot(edge_normal, light_direction), 0.0), 1.5);
    float shade = pow(max(dot(edge_normal, -light_direction), 0.0), 1.5);
    color.rgb += vec3(rim * u_liquid * (0.025 + 0.16 * highlight));
    color.rgb *= 1.0 - rim * u_liquid * 0.055 * shade;
    color.rgb = clamp(color.rgb, 0.0, 1.0);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

    color *= rounded_mask();

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

pub fn frost_glass_program(
    cache: &mut Option<GlesTexProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesTexProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_texture_shader(
        FROST_GLASS_FRAGMENT_SHADER,
        &[
            UniformName::new("u_texel", UniformType::_2f),
            UniformName::new("u_size", UniformType::_2f),
            UniformName::new("u_liquid", UniformType::_1f),
            UniformName::new("u_radius", UniformType::_1f),
            UniformName::new("u_strength", UniformType::_1f),
            UniformName::new("u_saturation", UniformType::_1f),
            UniformName::new("u_contrast", UniformType::_1f),
            UniformName::new("u_brightness", UniformType::_1f),
            UniformName::new("u_noise", UniformType::_1f),
            UniformName::new("u_noise_scale", UniformType::_1f),
            UniformName::new("u_vibrancy", UniformType::_1f),
            UniformName::new("u_vibrancy_darkness", UniformType::_1f),
            UniformName::new("u_tint_color", UniformType::_3f),
            UniformName::new("u_tint_alpha", UniformType::_1f),
            UniformName::new("u_corner_radii", UniformType::_4f),
            UniformName::new("u_rounding_power", UniformType::_1f),
            UniformName::new("u_corner_softness", UniformType::_1f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile frost-glass shader");
            None
        }
    }
}

/// Rendered-value fingerprint for the frost element's damage identity, the
/// same equality-only contract as `water_glass::water_glass_commit` (and
/// `decoration::border_commit` before it): the backdrop capture's content
/// `version` plus every uniform drawn from the config. Frost has no
/// animation clock, so an unchanged config over an unchanged capture yields
/// an unchanged commit and the visible frame stops redrawing the layer.
pub fn frost_glass_commit(
    capture_version: usize,
    config: &FrostConfig,
    corner_radii: [f32; 4],
    rounding_power: f32,
    corner_softness: f32,
) -> CommitCounter {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    capture_version.hash(&mut hash);
    for value in [
        config.radius,
        config.liquid,
        config.strength,
        config.saturation,
        config.contrast,
        config.brightness,
        config.noise,
        config.noise_scale,
        config.vibrancy,
        config.vibrancy_darkness,
        config.tint_alpha,
    ] {
        value.to_bits().hash(&mut hash);
    }
    for channel in config.tint_color {
        channel.to_bits().hash(&mut hash);
    }
    for radius in corner_radii {
        radius.to_bits().hash(&mut hash);
    }
    rounding_power.to_bits().hash(&mut hash);
    corner_softness.to_bits().hash(&mut hash);
    CommitCounter::from(hash.finish() as usize)
}

pub struct FrostGlassElement {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    geometry: Rectangle<i32, Physical>,
    program: GlesTexProgram,
    texel: [f32; 2],
    config: FrostConfig,
    corner_radii: [f32; 4],
    rounding_power: f32,
    corner_softness: f32,
}

impl FrostGlassElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Id,
        commit: CommitCounter,
        texture: GlesTexture,
        geometry: Rectangle<i32, Physical>,
        program: GlesTexProgram,
        config: FrostConfig,
        corner_radii: [f32; 4],
        rounding_power: f32,
        corner_softness: f32,
    ) -> Self {
        let size = texture.size();
        let texel = [1.0 / size.w.max(1) as f32, 1.0 / size.h.max(1) as f32];
        Self {
            id,
            commit,
            texture,
            geometry,
            program,
            texel,
            config,
            corner_radii,
            rounding_power,
            corner_softness,
        }
    }
}

impl Element for FrostGlassElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.texture.size().to_f64())
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn alpha(&self) -> f32 {
        self.config.opacity
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for FrostGlassElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            self.alpha(),
            Some(&self.program),
            &[
                Uniform::new("u_texel", self.texel),
                Uniform::new(
                    "u_size",
                    [
                        self.geometry.size.w.max(1) as f32,
                        self.geometry.size.h.max(1) as f32,
                    ],
                ),
                Uniform::new("u_radius", self.config.radius),
                Uniform::new("u_liquid", self.config.liquid),
                Uniform::new("u_strength", self.config.strength),
                Uniform::new("u_saturation", self.config.saturation),
                Uniform::new("u_contrast", self.config.contrast),
                Uniform::new("u_brightness", self.config.brightness),
                Uniform::new("u_noise", self.config.noise),
                Uniform::new("u_noise_scale", self.config.noise_scale),
                Uniform::new("u_vibrancy", self.config.vibrancy),
                Uniform::new("u_vibrancy_darkness", self.config.vibrancy_darkness),
                Uniform::new("u_tint_color", self.config.tint_color),
                Uniform::new("u_tint_alpha", self.config.tint_alpha),
                Uniform::new("u_corner_radii", self.corner_radii),
                Uniform::new("u_rounding_power", self.rounding_power),
                Uniform::new("u_corner_softness", self.corner_softness),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_contract_and_bounded_kernel_stay_intact() {
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("//_DEFINES_"));
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("uniform sampler2D tex"));
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("uniform float alpha"));
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("uniform vec2 u_texel"));
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("uniform float u_strength"));
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("uniform float u_liquid"));
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("uniform float u_noise"));
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("uniform vec4 u_corner_radii"));
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("uniform float u_rounding_power"));
        assert!(FROST_GLASS_FRAGMENT_SHADER.contains("rounded_mask()"));
        assert_eq!(
            FROST_GLASS_FRAGMENT_SHADER
                .matches("sample_clamped(")
                .count(),
            26
        );
    }

    #[test]
    fn frost_glass_commit_is_stable_when_the_scene_is_static() {
        // Frost has no animation clock, so an unchanged config over an
        // unchanged capture must hash to the same value across frames --
        // that stability is what stops the visible output redrawing a frosted
        // bar or window every frame while nothing behind it changed.
        let config = FrostConfig::default();
        let baseline = frost_glass_commit(2, &config, [6.0; 4], 2.0, 1.0);
        assert_eq!(baseline, frost_glass_commit(2, &config, [6.0; 4], 2.0, 1.0));
    }

    #[test]
    fn frost_glass_commit_advances_when_the_capture_re_renders() {
        let config = FrostConfig::default();
        let before = frost_glass_commit(2, &config, [6.0; 4], 2.0, 1.0);
        let after = frost_glass_commit(3, &config, [6.0; 4], 2.0, 1.0);
        assert_ne!(before, after);
    }

    #[test]
    fn frost_glass_commit_advances_on_strength_change() {
        // A `layer_rule` / window-rule strength override applied via
        // hot-reload has to invalidate the frost layer the same frame.
        let config = FrostConfig::default();
        let mut changed = config.clone();
        changed.strength = 0.25;
        let before = frost_glass_commit(2, &config, [6.0; 4], 2.0, 1.0);
        let after = frost_glass_commit(2, &changed, [6.0; 4], 2.0, 1.0);
        assert_ne!(before, after);
    }

    #[test]
    fn frost_glass_commit_advances_on_liquid_change() {
        let config = FrostConfig::default();
        let mut changed = config.clone();
        changed.liquid = 0.0;
        let before = frost_glass_commit(2, &config, [6.0; 4], 2.0, 1.0);
        let after = frost_glass_commit(2, &changed, [6.0; 4], 2.0, 1.0);
        assert_ne!(before, after);
    }
}
