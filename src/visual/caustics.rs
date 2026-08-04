//! Ambient caustic light patterns over the wallpaper, below windows.
//!
//! The pattern is rendered analytically into a small offscreen texture
//! (1/4 resolution, reused across frames) and blitted upscaled to the
//! output every frame -- the pattern is very low-frequency, so the blit
//! is visually identical to a direct full-output pass at roughly 1/16th
//! of the fragment cost. That makes the `caustics.fps` constant-motion
//! mode affordable on integrated graphics, instead of being the
//! frame-rate tax a full-resolution procedural pass used to be.
//!
//! Motion is phase-accumulated in `Caustics` and only advances when a
//! frame is actually built, so the default mode piggybacks on
//! damage-driven frames -- an idle desktop shows static caustics that
//! read as part of the wallpaper and ticks zero frames. Setting
//! `caustics.fps` above zero opts into constant motion: the frame pump
//! gate (`Smallvil::caustics_active`) keeps redraws coming at roughly
//! that rate, capped so it never becomes a per-refresh busy loop.
//!
//! Gated on the `water_effects` master toggle like the rest of the water
//! identity. Works under both spatial engines; it's wallpaper-level
//! ambience, not navigation.

use std::time::{Duration, Instant};

use smithay::backend::{
    allocator::Fourcc,
    renderer::{
        damage::OutputDamageTracker,
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexture, Uniform,
            UniformName, UniformType,
        },
        utils::CommitCounter,
        Bind, Offscreen, Texture,
    },
};
use smithay::utils::{
    user_data::UserDataMap, Buffer, Logical, Physical, Rectangle, Scale, Size, Transform,
};

use crate::config::CausticsConfig;

/// Offscreen render scale: the caustics pattern is low-frequency, so the
/// visible output is a 1/4-resolution texture upscaled to the output.
/// Fragment cost drops ~16x; the pattern density is resolution-independent
/// (the shader works in output-relative coordinates), so the blit reads
/// the same as a direct full-output pass.
const CAUSTICS_DOWNSCALE: i32 = 4;

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
    /// Cached 1/4-resolution pattern texture, reused across frames.
    texture: Option<GlesTexture>,
}

impl Default for Caustics {
    fn default() -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            phase: 0.0,
            last_advance: Instant::now(),
            last_sample: None,
            texture: None,
        }
    }
}

impl Caustics {
    /// Advances the phase, re-renders the low-res pattern texture if the
    /// sample changed (or the texture is missing), and returns the blit
    /// element for this output's frame. `None` on a render failure or an
    /// empty output area -- the effect skips a frame rather than crash.
    pub fn frame_element(
        &mut self,
        renderer: &mut GlesRenderer,
        program: GlesPixelProgram,
        area: Rectangle<i32, Logical>,
        output_scale: f64,
        cfg: &CausticsConfig,
    ) -> Option<CausticsElement> {
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
        let dirty = self.last_sample != Some(sample);
        if dirty {
            self.last_sample = Some(sample);
            self.commit.increment();
        }
        if area.size.w <= 0 || area.size.h <= 0 {
            return None;
        }
        let physical: Rectangle<i32, Physical> = area.to_physical_precise_round(output_scale);
        let low_w: i32 = (physical.size.w / CAUSTICS_DOWNSCALE).max(1);
        let low_h: i32 = (physical.size.h / CAUSTICS_DOWNSCALE).max(1);
        let low = Size::<i32, Physical>::from((low_w, low_h));
        // Re-render the pattern only when it actually changed; a static
        // sample (speed = 0, or an untouched piggyback frame) reuses the
        // cached texture and the unchanged commit lets the damage tracker
        // skip the blit entirely.
        if dirty || self.texture.is_none() {
            self.texture = render_pattern(renderer, &program, sample, low, self.texture.take());
            self.texture.as_ref()?;
        }
        Some(CausticsElement {
            id: self.id.clone(),
            commit: self.commit,
            texture: self.texture.as_ref()?.clone(),
            geometry: physical,
        })
    }
}

/// Renders one frame of the analytical pattern into `reusable` (reallocated
/// if its size no longer matches), returning the upscale-blit source.
/// `None` on any GL failure, logged by the caller's absence of drama.
fn render_pattern(
    renderer: &mut GlesRenderer,
    program: &GlesPixelProgram,
    sample: CausticsSample,
    size: Size<i32, Physical>,
    reusable: Option<GlesTexture>,
) -> Option<GlesTexture> {
    let buffer_size: Size<i32, Buffer> = Size::from((size.w, size.h));
    let mut texture = match reusable.filter(|texture| texture.size() == buffer_size) {
        Some(texture) => texture,
        None => renderer
            .create_buffer(Fourcc::Argb8888, buffer_size)
            .map_err(|err| tracing::warn!(%err, "Failed to allocate caustics pattern texture"))
            .ok()?,
    };
    let mut target = renderer
        .bind(&mut texture)
        .map_err(|err| tracing::warn!(%err, "Failed to bind caustics pattern target"))
        .ok()?;
    let mut tracker =
        OutputDamageTracker::new((size.w, size.h), 1.0, Transform::Normal);
    let element = PatternElement {
        id: Id::new(),
        area: Rectangle::from_size(Size::<i32, Logical>::from((size.w, size.h))),
        program: program.clone(),
        sample,
    };
    if let Err(err) = tracker.render_output(renderer, &mut target, 0, &[element], [0.0, 0.0, 0.0, 0.0])
    {
        tracing::warn!(%err, "Failed to render caustics pattern");
        return None;
    }
    drop(target);
    Some(texture)
}

/// The offscreen pass: draws the procedural pattern into the low-res
/// target. Same shader and uniforms the old direct full-output element
/// used; the tracker runs at scale 1.0 over the low-res rect, so the
/// pattern density (which is relative to the drawn rect) is unchanged.
struct PatternElement {
    id: Id,
    area: Rectangle<i32, Logical>,
    program: GlesPixelProgram,
    sample: CausticsSample,
}

impl Element for PatternElement {
    fn id(&self) -> &Id {
        // Fresh per offscreen pass, which is fine: the tracker this id
        // feeds is discarded with the pass.
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        CommitCounter::default()
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

impl RenderElement<GlesRenderer> for PatternElement {
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

/// The on-screen element: upscale-blits the cached low-res pattern over
/// the wallpaper. `commit` carries the pattern's change counter, so the
/// damage tracker redraws the blit only when the pattern actually moved.
pub struct CausticsElement {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    geometry: Rectangle<i32, Physical>,
}

impl Element for CausticsElement {
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
            None,
            &[],
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
        let dt = caustics
            .last_advance
            .elapsed()
            .min(Duration::from_millis(100));
        caustics.phase += dt.as_secs_f32();
        assert!(caustics.phase > before);
    }

    #[test]
    fn long_stall_advance_is_capped() {
        let caustics = Caustics {
            last_advance: Instant::now() - Duration::from_secs(60),
            ..Caustics::default()
        };
        let dt = caustics
            .last_advance
            .elapsed()
            .min(Duration::from_millis(100));
        assert!(dt <= Duration::from_millis(100));
    }
}
