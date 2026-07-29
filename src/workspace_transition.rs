//! Full-output workspace transition (render-roadmap Phase R1).
//!
//! The outgoing desktop is captured after its visible frame has been
//! submitted, then drawn over the already-live incoming workspace. The
//! default style floods the complete output with a procedurally animated
//! water body before its trailing edge reveals the new workspace; the
//! original colored boundary remains as the alternate `glow` style. Water,
//! foam, spray, core, and glow are analytical shader geometry, so tuning
//! them adds no extra render targets. The owning state keeps at most one
//! captured texture per output and drops it as soon as the animation
//! finishes, giving the effect a bounded transient cost instead of retaining
//! workspace history.

use std::time::{Duration, Instant};

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

use crate::{
    animation::Animation,
    config::{RippleEase, WorkspaceTransitionConfig, WorkspaceTransitionStyle},
};

const WORKSPACE_TRANSITION_FRAGMENT_SHADER: &str = r#"
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
uniform float progress;
uniform float direction;
uniform float style;
uniform float output_aspect;
uniform float wave_amplitude;
uniform float wave_frequency;
uniform float edge_width;
uniform vec3 wave_color;
uniform float wave_size;
uniform float wave_alpha;
uniform float glow_size;
uniform float glow_alpha;
uniform float water_depth;
uniform vec3 foam_color;
uniform float foam_size;
uniform float foam_alpha;
uniform float spray_amount;
uniform float turbulence;
uniform float water_alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

float hash21(vec2 value) {
    value = fract(value * vec2(123.34, 456.21));
    value += dot(value, value + 45.32);
    return fract(value.x * value.y);
}

void main() {
    vec4 sampled = texture2D(tex, v_coords);

    if (style > 0.5) {
        // In the first half the leading crest covers the outgoing workspace.
        // At the midpoint water fills the output completely. In the second
        // half its trailing edge exposes the already-live incoming workspace.
        float entering = progress < 0.5 ? 1.0 : 0.0;
        float phase = entering > 0.5 ? progress * 2.0 : (progress - 0.5) * 2.0;
        float travel_coordinate = direction < 0.0 ? v_coords.x : 1.0 - v_coords.x;
        float margin = max(water_depth, wave_amplitude + foam_size * 3.0);
        float travel = mix(-margin, 1.0 + margin, phase);
        float phase_angle = v_coords.y * 6.2831853 * wave_frequency
            - phase * 4.712389;
        float crest = sin(phase_angle) * wave_amplitude;
        crest += sin(phase_angle * 2.17 + 1.3)
            * wave_amplitude
            * 0.32
            * turbulence;
        crest += pow(max(0.0, sin(phase_angle * 0.51 - 0.8)), 3.0)
            * wave_size
            * turbulence;
        float boundary = travel + crest;
        float water_distance = entering > 0.5
            ? boundary - travel_coordinate
            : travel_coordinate - boundary;
        float water_mask = smoothstep(-edge_width, edge_width, water_distance);
        float old_workspace = entering
            * (1.0 - smoothstep(-edge_width, edge_width, water_distance));

        // Layered moving streaks keep the fully-covered midpoint reading as
        // animated water rather than a flat blue color card.
        float streak_a = sin(
            travel_coordinate * 43.0
            + v_coords.y * 18.0
            - progress * 18.0
            + sin(v_coords.y * 31.0 + progress * 9.0)
        );
        float streak_b = sin(
            travel_coordinate * 21.0
            - v_coords.y * 37.0
            + progress * 13.0
        );
        float caustic = smoothstep(
            0.72,
            1.0,
            (streak_a * 0.55 + streak_b * 0.45) * 0.5 + 0.5
        ) * turbulence;
        float depth = clamp(water_distance / max(water_depth, 0.0001), 0.0, 1.0);
        vec3 body_rgb = wave_color * mix(1.12, 0.56, depth);
        body_rgb = mix(body_rgb, foam_color, caustic * 0.22);
        float body_alpha = water_mask * water_alpha;

        float foam_noise = sin(v_coords.y * 113.0 + progress * 23.0)
            * 0.22
            + sin(v_coords.y * 47.0 - progress * 17.0) * 0.18;
        float foam_distance = abs(water_distance + foam_noise * foam_size);
        float foam = 1.0 - smoothstep(
            foam_size * 0.45,
            foam_size + edge_width,
            foam_distance
        );
        foam *= smoothstep(-edge_width * 2.0, edge_width, water_distance + foam_size);

        // Sparse procedural droplets ride just ahead of the entering crest.
        // The grid uses output aspect so the cells stay approximately square.
        vec2 spray_position = vec2(
            v_coords.x * 36.0,
            v_coords.y * 36.0 / max(output_aspect, 0.01)
        );
        spray_position.y += progress * 7.0;
        vec2 spray_cell = floor(spray_position);
        vec2 spray_local = fract(spray_position) - 0.5;
        float spray_random = hash21(spray_cell);
        float drop_radius = mix(0.055, 0.19, hash21(spray_cell + 7.3));
        float drop = 1.0 - smoothstep(
            drop_radius,
            drop_radius + 0.055,
            length(spray_local)
        );
        drop *= step(0.78 - spray_amount * 0.28, spray_random);
        float ahead = max(-water_distance, 0.0);
        float spray_reach = foam_size * 7.0 + wave_amplitude * 0.6;
        float spray_zone = entering
            * smoothstep(0.0, edge_width + 0.00001, ahead)
            * (1.0 - smoothstep(foam_size, spray_reach, ahead));
        float spray_alpha = drop * spray_zone * spray_amount * foam_alpha;
        float crest_alpha = max(foam * foam_alpha, spray_alpha);

        vec4 body = vec4(body_rgb * body_alpha, body_alpha);
        vec4 crest_color = vec4(foam_color * crest_alpha, crest_alpha);
        vec4 water = crest_color + body * (1.0 - crest_alpha);
        vec4 old_color;

#if defined(NO_ALPHA)
        old_color = vec4(sampled.rgb, 1.0) * alpha * old_workspace;
#else
        old_color = sampled * alpha * old_workspace;
#endif

        gl_FragColor = water + old_color * (1.0 - water.a);
        return;
    }

    float coordinate = direction > 0.0 ? v_coords.x : 1.0 - v_coords.x;
    float wave = sin(v_coords.y * 6.2831853 * wave_frequency + progress * 6.2831853)
        * wave_amplitude
        * sin(progress * 3.1415927);
    float boundary = 1.0 - progress + wave;
    float signed_distance = boundary - coordinate;
    float old_workspace = smoothstep(-edge_width, edge_width, signed_distance);
    float front_distance = abs(signed_distance);
    float core = 1.0 - smoothstep(wave_size, wave_size + edge_width, front_distance);
    float glow = 1.0 - smoothstep(
        wave_size,
        wave_size + glow_size + edge_width,
        front_distance
    );
    float front_alpha = max(core * wave_alpha, glow * glow_alpha);
    vec4 old_color;

#if defined(NO_ALPHA)
    old_color = vec4(sampled.rgb, 1.0) * alpha * old_workspace;
#else
    old_color = sampled * alpha * old_workspace;
#endif

    // Both terms are pre-multiplied. The colored wavefront sits above the
    // outgoing capture and remains visible for the part crossing the live
    // incoming workspace where old_workspace has already reached zero.
    vec4 front_color = vec4(wave_color * front_alpha, front_alpha);
    vec4 color = front_color + old_color * (1.0 - front_alpha);

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceTransitionDirection {
    LeftToRight,
    RightToLeft,
}

impl WorkspaceTransitionDirection {
    fn uniform(self) -> f32 {
        match self {
            // A positive uniform keeps old content left of a boundary that
            // moves right-to-left; a negative one mirrors coordinates.
            Self::RightToLeft => 1.0,
            Self::LeftToRight => -1.0,
        }
    }
}

pub fn workspace_transition_program(
    cache: &mut Option<GlesTexProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesTexProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    let uniforms = [
        UniformName::new("progress", UniformType::_1f),
        UniformName::new("direction", UniformType::_1f),
        UniformName::new("style", UniformType::_1f),
        UniformName::new("output_aspect", UniformType::_1f),
        UniformName::new("wave_amplitude", UniformType::_1f),
        UniformName::new("wave_frequency", UniformType::_1f),
        UniformName::new("edge_width", UniformType::_1f),
        UniformName::new("wave_color", UniformType::_3f),
        UniformName::new("wave_size", UniformType::_1f),
        UniformName::new("wave_alpha", UniformType::_1f),
        UniformName::new("glow_size", UniformType::_1f),
        UniformName::new("glow_alpha", UniformType::_1f),
        UniformName::new("water_depth", UniformType::_1f),
        UniformName::new("foam_color", UniformType::_3f),
        UniformName::new("foam_size", UniformType::_1f),
        UniformName::new("foam_alpha", UniformType::_1f),
        UniformName::new("spray_amount", UniformType::_1f),
        UniformName::new("turbulence", UniformType::_1f),
        UniformName::new("water_alpha", UniformType::_1f),
    ];
    match renderer.compile_custom_texture_shader(WORKSPACE_TRANSITION_FRAGMENT_SHADER, &uniforms) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile workspace-transition shader");
            None
        }
    }
}

pub struct WorkspaceTransition {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    animation: Animation,
    curve: RippleEase,
    direction: WorkspaceTransitionDirection,
    style: WorkspaceTransitionStyle,
    geometry: Rectangle<i32, Physical>,
    wave_amplitude: f32,
    wave_frequency: f32,
    edge_width: f32,
    wave_color: [f32; 3],
    wave_size: f32,
    wave_alpha: f32,
    glow_size: f32,
    glow_alpha: f32,
    water_depth: f32,
    foam_color: [f32; 3],
    foam_size: f32,
    foam_alpha: f32,
    spray_amount: f32,
    turbulence: f32,
    water_alpha: f32,
}

impl WorkspaceTransition {
    pub fn new(
        texture: GlesTexture,
        direction: WorkspaceTransitionDirection,
        geometry: Rectangle<i32, Physical>,
        config: &WorkspaceTransitionConfig,
    ) -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            texture,
            animation: Animation::new(
                0.0,
                1.0,
                Instant::now(),
                Duration::from_secs_f32(
                    Duration::from_millis(config.duration_ms as u64).as_secs_f32() / config.speed,
                ),
            ),
            curve: config.curve,
            direction,
            style: config.style,
            geometry,
            wave_amplitude: config.wave_amplitude,
            wave_frequency: config.wave_frequency,
            edge_width: config.edge_width,
            wave_color: config.color,
            wave_size: config.wave_size,
            wave_alpha: config.wave_alpha,
            glow_size: config.glow_size,
            glow_alpha: config.glow_alpha,
            water_depth: config.water_depth,
            foam_color: config.foam_color,
            foam_size: config.foam_size,
            foam_alpha: config.foam_alpha,
            spray_amount: config.spray_amount,
            turbulence: config.turbulence,
            water_alpha: config.water_alpha,
        }
    }

    pub fn finished(&self) -> bool {
        self.animation.finished()
    }

    pub fn frame_element(&mut self, program: GlesTexProgram) -> WorkspaceTransitionElement {
        self.commit.increment();
        let width = self.geometry.size.w.max(1) as f32;
        let height = self.geometry.size.h.max(1) as f32;
        WorkspaceTransitionElement {
            id: self.id.clone(),
            commit: self.commit,
            texture: self.texture.clone(),
            progress: crate::ripple::ease_value(self.curve, self.animation.value()),
            direction: self.direction,
            style: self.style,
            output_aspect: width / height,
            geometry: self.geometry,
            wave_amplitude: self.wave_amplitude / width,
            wave_frequency: self.wave_frequency,
            edge_width: self.edge_width / width,
            wave_color: self.wave_color,
            wave_size: self.wave_size / width,
            wave_alpha: self.wave_alpha,
            glow_size: self.glow_size / width,
            glow_alpha: self.glow_alpha,
            water_depth: self.water_depth / width,
            foam_color: self.foam_color,
            foam_size: self.foam_size / width,
            foam_alpha: self.foam_alpha,
            spray_amount: self.spray_amount,
            turbulence: self.turbulence,
            water_alpha: self.water_alpha,
            program,
        }
    }
}

pub struct WorkspaceTransitionElement {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    progress: f32,
    direction: WorkspaceTransitionDirection,
    style: WorkspaceTransitionStyle,
    output_aspect: f32,
    geometry: Rectangle<i32, Physical>,
    wave_amplitude: f32,
    wave_frequency: f32,
    edge_width: f32,
    wave_color: [f32; 3],
    wave_size: f32,
    wave_alpha: f32,
    glow_size: f32,
    glow_alpha: f32,
    water_depth: f32,
    foam_color: [f32; 3],
    foam_size: f32,
    foam_alpha: f32,
    spray_amount: f32,
    turbulence: f32,
    water_alpha: f32,
    program: GlesTexProgram,
}

impl Element for WorkspaceTransitionElement {
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

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for WorkspaceTransitionElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let uniforms = [
            Uniform::new("progress", self.progress),
            Uniform::new("direction", self.direction.uniform()),
            Uniform::new(
                "style",
                match self.style {
                    WorkspaceTransitionStyle::Glow => 0.0,
                    WorkspaceTransitionStyle::Water => 1.0,
                },
            ),
            Uniform::new("output_aspect", self.output_aspect),
            Uniform::new("wave_amplitude", self.wave_amplitude),
            Uniform::new("wave_frequency", self.wave_frequency),
            Uniform::new("edge_width", self.edge_width),
            Uniform::new("wave_color", self.wave_color),
            Uniform::new("wave_size", self.wave_size),
            Uniform::new("wave_alpha", self.wave_alpha),
            Uniform::new("glow_size", self.glow_size),
            Uniform::new("glow_alpha", self.glow_alpha),
            Uniform::new("water_depth", self.water_depth),
            Uniform::new("foam_color", self.foam_color),
            Uniform::new("foam_size", self.foam_size),
            Uniform::new("foam_alpha", self.foam_alpha),
            Uniform::new("spray_amount", self.spray_amount),
            Uniform::new("turbulence", self.turbulence),
            Uniform::new("water_alpha", self.water_alpha),
        ];
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            self.alpha(),
            Some(&self.program),
            &uniforms,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_source_keeps_smithays_contract_and_transition_uniforms() {
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("//_DEFINES_"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform sampler2D tex"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float alpha"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float progress"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float direction"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float style"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float output_aspect"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float wave_amplitude"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float wave_frequency"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float edge_width"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform vec3 wave_color"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float wave_size"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float wave_alpha"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float glow_size"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float glow_alpha"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float water_depth"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform vec3 foam_color"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float foam_size"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float foam_alpha"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float spray_amount"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float turbulence"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float water_alpha"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("varying vec2 v_coords"));
    }

    #[test]
    fn transition_memory_is_one_argb_texture_per_output() {
        fn bytes(width: usize, height: usize) -> usize {
            width * height * 4
        }

        assert_eq!(bytes(1920, 1080), 8_294_400);
        assert_eq!(bytes(3840, 2160), 33_177_600);
    }

    #[test]
    fn direction_uniform_matches_the_named_sweep() {
        assert_eq!(WorkspaceTransitionDirection::LeftToRight.uniform(), -1.0);
        assert_eq!(WorkspaceTransitionDirection::RightToLeft.uniform(), 1.0);
    }
}
