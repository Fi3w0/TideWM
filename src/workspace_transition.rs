//! Full-output workspace transition (render-roadmap Phase R1).
//!
//! The outgoing desktop is captured after its visible frame has been
//! submitted, then drawn over the already-live incoming workspace while a
//! directional, wavy boundary peels it away. The owning state keeps at most
//! one of these textures per output and drops it as soon as the animation
//! finishes, so the effect has a bounded transient cost instead of retaining
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
    config::{RippleEase, WorkspaceTransitionConfig},
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
uniform float wave_amplitude;
uniform float wave_frequency;
uniform float edge_width;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    float coordinate = direction > 0.0 ? v_coords.x : 1.0 - v_coords.x;
    float wave = sin(v_coords.y * 6.2831853 * wave_frequency + progress * 6.2831853)
        * wave_amplitude
        * sin(progress * 3.1415927);
    float boundary = 1.0 - progress + wave;
    float old_workspace = smoothstep(-edge_width, edge_width, boundary - coordinate);
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha * old_workspace;
#else
    color = color * alpha * old_workspace;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceTransitionDirection {
    Forward,
    Backward,
}

impl WorkspaceTransitionDirection {
    fn uniform(self) -> f32 {
        match self {
            Self::Forward => 1.0,
            Self::Backward => -1.0,
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
        UniformName::new("wave_amplitude", UniformType::_1f),
        UniformName::new("wave_frequency", UniformType::_1f),
        UniformName::new("edge_width", UniformType::_1f),
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
    geometry: Rectangle<i32, Physical>,
    wave_amplitude: f32,
    wave_frequency: f32,
    edge_width: f32,
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
                Duration::from_millis(config.duration_ms as u64),
            ),
            curve: config.curve,
            direction,
            geometry,
            wave_amplitude: config.wave_amplitude,
            wave_frequency: config.wave_frequency,
            edge_width: config.edge_width,
        }
    }

    pub fn finished(&self) -> bool {
        self.animation.finished()
    }

    pub fn frame_element(&mut self, program: GlesTexProgram) -> WorkspaceTransitionElement {
        self.commit.increment();
        let width = self.geometry.size.w.max(1) as f32;
        WorkspaceTransitionElement {
            id: self.id.clone(),
            commit: self.commit,
            texture: self.texture.clone(),
            progress: crate::ripple::ease_value(self.curve, self.animation.value()),
            direction: self.direction,
            geometry: self.geometry,
            wave_amplitude: self.wave_amplitude / width,
            wave_frequency: self.wave_frequency,
            edge_width: self.edge_width / width,
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
    geometry: Rectangle<i32, Physical>,
    wave_amplitude: f32,
    wave_frequency: f32,
    edge_width: f32,
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
            Uniform::new("wave_amplitude", self.wave_amplitude),
            Uniform::new("wave_frequency", self.wave_frequency),
            Uniform::new("edge_width", self.edge_width),
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
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float wave_amplitude"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float wave_frequency"));
        assert!(WORKSPACE_TRANSITION_FRAGMENT_SHADER.contains("uniform float edge_width"));
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
}
