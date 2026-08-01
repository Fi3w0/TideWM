//! Classic depth-switch transition: a vertical pressure front, distinct from
//! the horizontal captured-workspace water wipe.
//!
//! This is one analytical full-output element with no texture or framebuffer.
//! Diving sweeps top-to-bottom; surfacing sweeps bottom-to-top. Several narrow
//! wake bands and deterministic bubble glints make it read as pressure moving
//! through water rather than a rotated workspace transition.

use std::time::{Duration, Instant};

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
        },
        utils::CommitCounter,
    },
    utils::{user_data::UserDataMap, Buffer, Logical, Physical, Rectangle, Scale, Transform},
};

const DEPTH_TRANSITION_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform float u_progress;
uniform float u_direction;
uniform vec3 u_tint;
uniform float u_strength;

float band(float value, float center, float width) {
    return 1.0 - smoothstep(width, width * 1.8, abs(value - center));
}

float hash21(vec2 p) {
    p = fract(p * vec2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return fract(p.x * p.y);
}

void main() {
    float progress = clamp(u_progress, 0.0, 1.0);
    float center = u_direction > 0.0 ? progress : 1.0 - progress;
    float travel = u_direction > 0.0 ? 1.0 : -1.0;
    float wave = sin(v_coords.x * 18.8496 + progress * 8.0) * 0.014
        + sin(v_coords.x * 43.9823 - progress * 13.0) * 0.006;
    float distance = v_coords.y - center - wave;

    float crest = band(distance, 0.0, 0.018);
    float wake_a = band(distance, -travel * 0.052, 0.010) * 0.52;
    float wake_b = band(distance, -travel * 0.094, 0.008) * 0.28;
    float pressure = exp(-abs(distance) * 9.0) * 0.20;

    vec2 cells = vec2(v_coords.x * 18.0, v_coords.y * 28.0);
    vec2 cell = floor(cells);
    vec2 local = fract(cells) - 0.5;
    float random = hash21(cell);
    float bubble = 1.0 - smoothstep(0.07, 0.13, length(local));
    bubble *= step(0.86, random);
    bubble *= 1.0 - smoothstep(0.03, 0.16, abs(distance));

    float life = sin(progress * 3.14159265);
    float amount = clamp(
        (crest + wake_a + wake_b + pressure + bubble * 0.7) * u_strength * life,
        0.0,
        1.0
    );
    vec3 deep = u_tint * 0.34;
    vec3 bright = mix(u_tint, vec3(0.78, 0.97, 1.0), crest * 0.72 + bubble * 0.5);
    vec3 color = mix(deep, bright, clamp(crest + wake_a + bubble, 0.0, 1.0));
    float out_alpha = amount * alpha;
    gl_FragColor = vec4(color * out_alpha, out_alpha);
}
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepthTransitionDirection {
    Down,
    Up,
}

pub struct DepthTransition {
    started: Instant,
    duration: Duration,
    direction: DepthTransitionDirection,
    color: [f32; 3],
    strength: f32,
    id: Id,
    commit: CommitCounter,
}

impl DepthTransition {
    pub fn new(
        direction: DepthTransitionDirection,
        duration: Duration,
        color: [f32; 3],
        strength: f32,
    ) -> Self {
        Self {
            started: Instant::now(),
            duration,
            direction,
            color,
            strength,
            id: Id::new(),
            commit: CommitCounter::default(),
        }
    }

    pub fn finished(&self) -> bool {
        self.duration.is_zero() || self.started.elapsed() >= self.duration
    }

    fn progress(&self) -> f32 {
        if self.duration.is_zero() {
            1.0
        } else {
            (self.started.elapsed().as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
        }
    }

    pub fn frame_element(
        &mut self,
        program: GlesPixelProgram,
        area: Rectangle<i32, Logical>,
    ) -> DepthTransitionElement {
        self.commit.increment();
        DepthTransitionElement {
            id: self.id.clone(),
            commit: self.commit,
            area,
            program,
            progress: self.progress(),
            direction: self.direction,
            color: self.color,
            strength: self.strength,
        }
    }
}

pub fn depth_transition_program(
    cache: &mut Option<GlesPixelProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesPixelProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_pixel_shader(
        DEPTH_TRANSITION_SHADER,
        &[
            UniformName::new("u_progress", UniformType::_1f),
            UniformName::new("u_direction", UniformType::_1f),
            UniformName::new("u_tint", UniformType::_3f),
            UniformName::new("u_strength", UniformType::_1f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile Classic depth transition shader");
            None
        }
    }
}

pub struct DepthTransitionElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<i32, Logical>,
    program: GlesPixelProgram,
    progress: f32,
    direction: DepthTransitionDirection,
    color: [f32; 3],
    strength: f32,
}

impl Element for DepthTransitionElement {
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

impl RenderElement<GlesRenderer> for DepthTransitionElement {
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
                Uniform::new("u_progress", self.progress),
                Uniform::new(
                    "u_direction",
                    if self.direction == DepthTransitionDirection::Down {
                        1.0
                    } else {
                        -1.0
                    },
                ),
                Uniform::new("u_tint", self.color),
                Uniform::new("u_strength", self.strength),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_is_vertical_and_keeps_smithay_contract() {
        assert!(!DEPTH_TRANSITION_SHADER.contains("#version"));
        assert!(DEPTH_TRANSITION_SHADER.contains("varying vec2 v_coords"));
        assert!(DEPTH_TRANSITION_SHADER.contains("v_coords.y - center"));
        assert!(DEPTH_TRANSITION_SHADER.contains("uniform float u_direction"));
    }

    #[test]
    fn zero_duration_finishes_without_requesting_frames() {
        assert!(DepthTransition::new(
            DepthTransitionDirection::Down,
            Duration::ZERO,
            [0.0; 3],
            1.0,
        )
        .finished());
    }
}
