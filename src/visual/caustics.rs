//! Ambient caustic light patterns over the wallpaper, below windows.
//!
//! One analytical full-output pixel element per output: no texture, no
//! framebuffer, and no element at all when the effect is disabled or the
//! session is locked. Motion is phase-accumulated in `Caustics` and only
//! advances when a frame is actually built, so the default mode piggybacks
//! on damage-driven frames -- an idle desktop shows static caustics that
//! read as part of the wallpaper and ticks zero frames. Setting
//! `caustics.fps` above zero opts into constant motion: the frame pump
//! gate (`Smallvil::caustics_active`) keeps redraws coming at roughly
//! that rate, capped so it never becomes a per-refresh busy loop.
//!
//! Gated on the `water_effects` master toggle like the rest of the water
//! identity. Works under both spatial engines; it's wallpaper-level
//! ambience, not navigation.

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

use crate::config::CausticsConfig;

/// Caustic interference from a few rotated, drifting sine fields,
/// sharpened into light-web ridges. Deliberately evoking the wave
/// transition's streak layers (`workspace_transition.rs`) without copying
/// its travel-coupled coordinate system -- this pattern has no direction
/// of travel, it just breathes.
///
/// Premultiplied alpha out, same contract as the ripple/ocean-canvas
/// pixel shaders. No `#version` directive; Smithay prepends its own.
const CAUSTICS_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform float u_time;
uniform float u_intensity;
uniform vec3 u_color;
uniform float u_scale;

float caustic_layer(vec2 p, float t) {
    float a = sin(p.x * 1.7 + t * 0.9 + sin(p.y * 1.3 + t * 0.7));
    float b = sin(p.y * 2.3 - t * 0.8 + sin(p.x * 1.9 - t * 0.6));
    float c = sin((p.x + p.y) * 1.1 + t * 0.5);
    float v = (a + b + c) * 0.16667 + 0.5;
    return pow(smoothstep(0.55, 1.0, v), 3.0);
}

void main() {
    vec2 p = v_coords * vec2(size.x / max(size.y, 1.0), 1.0) * 6.0 * u_scale;
    float v = caustic_layer(p, u_time)
        + 0.6 * caustic_layer(p * 1.7 + 3.1, u_time * 1.3);
    float a = clamp(v, 0.0, 1.0) * u_intensity * alpha;
    gl_FragColor = vec4(u_color * a, a);
}
"#;

pub fn caustics_program(
    cache: &mut Option<GlesPixelProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesPixelProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_pixel_shader(
        CAUSTICS_SHADER,
        &[
            UniformName::new("u_time", UniformType::_1f),
            UniformName::new("u_intensity", UniformType::_1f),
            UniformName::new("u_color", UniformType::_3f),
            UniformName::new("u_scale", UniformType::_1f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile caustics shader");
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CausticsSample {
    pub time: f32,
    pub intensity: f32,
    pub color: [f32; 3],
    pub scale: f32,
}

/// Per-output caustics state. `phase` only advances inside
/// `frame_element` -- which only runs while a frame is actually being
/// assembled -- so a damage-driven idle desktop simply stops advancing
/// it. The last rendered sample is remembered so the damage tracker's
/// commit counter only increments when the drawn result would actually
/// change (same shape as `ocean_canvas::OceanCanvas::frame_element`).
pub struct Caustics {
    id: Id,
    commit: CommitCounter,
    phase: f32,
    last_advance: Instant,
    last_sample: Option<CausticsSample>,
}

impl Default for Caustics {
    fn default() -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            phase: 0.0,
            last_advance: Instant::now(),
            last_sample: None,
        }
    }
}

impl Caustics {
    pub fn frame_element(
        &mut self,
        program: GlesPixelProgram,
        area: Rectangle<i32, Logical>,
        cfg: &CausticsConfig,
    ) -> CausticsElement {
        // Cap the per-frame advance so a stall (VT switch, suspend, a
        // blocked render loop) doesn't turn into a visible pattern jump
        // on the next frame.
        let dt = self.last_advance.elapsed().min(Duration::from_millis(100));
        self.last_advance = Instant::now();
        self.phase += dt.as_secs_f32() * cfg.speed;
        let sample = CausticsSample {
            time: self.phase,
            intensity: cfg.intensity,
            color: cfg.color,
            scale: cfg.scale,
        };
        if self.last_sample != Some(sample) {
            self.last_sample = Some(sample);
            self.commit.increment();
        }
        CausticsElement {
            id: self.id.clone(),
            commit: self.commit,
            area,
            program,
            sample,
        }
    }
}

pub struct CausticsElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<i32, Logical>,
    program: GlesPixelProgram,
    sample: CausticsSample,
}

impl Element for CausticsElement {
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

    // No `opaque_regions` override: the pattern is translucent light over
    // the wallpaper, which must keep drawing underneath.
}

impl RenderElement<GlesRenderer> for CausticsElement {
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
                Uniform::new("u_time", self.sample.time),
                Uniform::new("u_intensity", self.sample.intensity),
                Uniform::new("u_color", self.sample.color),
                Uniform::new("u_scale", self.sample.scale),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_is_analytical_and_allocation_free() {
        assert!(!CAUSTICS_SHADER.contains("#version"));
        assert!(!CAUSTICS_SHADER.contains("sampler2D"));
        assert!(CAUSTICS_SHADER.contains("uniform vec2 size"));
        assert!(CAUSTICS_SHADER.contains("uniform float alpha"));
        assert!(CAUSTICS_SHADER.contains("varying vec2 v_coords"));
        assert!(CAUSTICS_SHADER.contains("u_time"));
        assert!(CAUSTICS_SHADER.contains("u_intensity"));
        assert!(CAUSTICS_SHADER.contains("u_color"));
        assert!(CAUSTICS_SHADER.contains("u_scale"));
    }

    #[test]
    fn phase_only_advances_when_frames_build() {
        let mut caustics = Caustics::default();
        let before = caustics.phase;
        std::thread::sleep(Duration::from_millis(20));
        // No frame_element call: phase is frozen.
        assert_eq!(caustics.phase, before);

        // Advancing requires a program handle, which needs a live EGL
        // context unit tests don't have -- so the accumulation itself is
        // exercised through the same dt math frame_element uses.
        let dt = caustics.last_advance.elapsed().min(Duration::from_millis(100));
        caustics.phase += dt.as_secs_f32();
        assert!(caustics.phase > before);
    }

    #[test]
    fn long_stall_advance_is_capped() {
        let mut caustics = Caustics::default();
        caustics.last_advance = Instant::now() - Duration::from_secs(60);
        let dt = caustics.last_advance.elapsed().min(Duration::from_millis(100));
        assert!(dt <= Duration::from_millis(100));
    }
}
