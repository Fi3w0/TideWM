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
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexture, Uniform,
            UniformName, UniformType,
        },
        utils::CommitCounter,
        Bind, Color32F, ContextId, Frame, Offscreen, Renderer, Texture,
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

/// Maximum exponential recovery interval in units of the live configured or
/// output refresh cadence. This bounds retries without assuming a frame rate.
const MAX_FAILURE_BACKOFF_MULTIPLIER: u32 = 1024;

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

#[derive(Default)]
struct FailureBackoff {
    failures: u32,
    last_failure: Option<Instant>,
}

impl FailureBackoff {
    fn retry_delay(&self, cadence: Duration) -> Duration {
        if self.failures == 0 {
            return Duration::ZERO;
        }
        let shift = self.failures.saturating_sub(1).min(10);
        cadence.saturating_mul((1_u32 << shift).min(MAX_FAILURE_BACKOFF_MULTIPLIER))
    }

    fn retry_in_at(&self, now: Instant, cadence: Duration) -> Duration {
        let Some(last_failure) = self.last_failure else {
            return Duration::ZERO;
        };
        self.retry_delay(cadence)
            .saturating_sub(now.saturating_duration_since(last_failure))
    }

    fn ready_at(&self, now: Instant, cadence: Duration) -> bool {
        self.retry_in_at(now, cadence).is_zero()
    }

    fn record_failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.last_failure = Some(now);
    }

    fn clear(&mut self) {
        self.failures = 0;
        self.last_failure = None;
    }
}

#[derive(Default)]
pub struct CausticsProgramCache {
    program: Option<GlesPixelProgram>,
    context: Option<ContextId<GlesTexture>>,
    failures: FailureBackoff,
}

impl CausticsProgramCache {
    pub fn retry_in(&self, cadence: Duration) -> Duration {
        self.failures.retry_in_at(Instant::now(), cadence)
    }
}

pub fn caustics_program(
    cache: &mut CausticsProgramCache,
    renderer: &mut GlesRenderer,
    cadence: Duration,
) -> Option<GlesPixelProgram> {
    let context = renderer.context_id();
    if cache.context.as_ref() != Some(&context) {
        cache.program = None;
        cache.failures.clear();
        cache.context = Some(context);
    }
    if let Some(program) = &cache.program {
        return Some(program.clone());
    }
    let now = Instant::now();
    if !cache.failures.ready_at(now, cadence) {
        return None;
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
            cache.failures.clear();
            cache.program = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            cache.failures.record_failure(Instant::now());
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
    context: Option<ContextId<GlesTexture>>,
    phase: f32,
    last_advance: Instant,
    last_sample: Option<CausticsSample>,
    /// Last successfully rendered 1/4-resolution pattern texture.
    texture: Option<GlesTexture>,
    /// Alternate target used for transactional updates. The visible texture
    /// is never rendered into, so a failed pass cannot corrupt it in place.
    scratch_texture: Option<GlesTexture>,
    render_failures: FailureBackoff,
}

impl Default for Caustics {
    fn default() -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            context: None,
            phase: 0.0,
            last_advance: Instant::now(),
            last_sample: None,
            texture: None,
            scratch_texture: None,
            render_failures: FailureBackoff::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CausticsCandidate {
    phase: f32,
    sample: CausticsSample,
}

pub struct CausticsFrameTiming {
    pub advance: bool,
    pub retry_cadence: Duration,
}

impl Caustics {
    /// Time until a configured constant-motion frame is due. `None` means
    /// piggyback mode; an uninitialized output is immediately due once.
    pub fn next_frame_in(&self, fps: u32) -> Option<Duration> {
        if fps == 0 {
            return None;
        }
        if self.last_sample.is_none() {
            return Some(self.render_failures.retry_in_at(
                Instant::now(),
                Duration::from_secs_f64(1.0 / f64::from(fps)),
            ));
        }
        let period = Duration::from_secs_f64(1.0 / f64::from(fps));
        let cadence = period.saturating_sub(self.last_advance.elapsed());
        Some(cadence.max(self.render_failures.retry_in_at(Instant::now(), period)))
    }

    fn candidate(&self, cfg: &CausticsConfig, advance: bool, now: Instant) -> CausticsCandidate {
        let phase = if advance {
            // Cap the per-frame advance so a stall (VT switch, suspend, a
            // blocked render loop) doesn't turn into a visible pattern jump
            // on the next frame.
            let dt = now
                .saturating_duration_since(self.last_advance)
                .min(Duration::from_millis(100));
            self.phase + dt.as_secs_f32() * cfg.speed
        } else {
            self.phase
        };
        CausticsCandidate {
            phase,
            sample: CausticsSample {
                time: phase,
                intensity: cfg.intensity,
                color: cfg.color,
                scale: cfg.scale,
            },
        }
    }

    fn accept_candidate(&mut self, candidate: CausticsCandidate, now: Instant) {
        self.phase = candidate.phase;
        self.last_advance = now;
        self.last_sample = Some(candidate.sample);
        self.commit.increment();
        self.render_failures.clear();
    }

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
        timing: CausticsFrameTiming,
    ) -> Option<CausticsElement> {
        if area.size.w <= 0 || area.size.h <= 0 {
            return None;
        }
        let context = renderer.context_id();
        if self.context.as_ref() != Some(&context) {
            self.context = Some(context);
            self.texture = None;
            self.scratch_texture = None;
            self.last_sample = None;
            self.render_failures.clear();
        }
        let now = Instant::now();
        let candidate = self.candidate(cfg, timing.advance, now);
        let physical: Rectangle<i32, Physical> = area.to_physical_precise_round(output_scale);
        let low_w: i32 = (physical.size.w / CAUSTICS_DOWNSCALE).max(1);
        let low_h: i32 = (physical.size.h / CAUSTICS_DOWNSCALE).max(1);
        let low = Size::<i32, Physical>::from((low_w, low_h));
        let buffer_size: Size<i32, Buffer> = Size::from((low.w, low.h));
        let dirty = self.last_sample != Some(candidate.sample)
            || self
                .texture
                .as_ref()
                .is_none_or(|texture| texture.size() != buffer_size);
        // Re-render the pattern only when it actually changed; a static
        // sample (speed = 0, or an untouched piggyback frame) reuses the
        // cached texture and the unchanged commit lets the damage tracker
        // skip the blit entirely.
        if dirty {
            if self.render_failures.ready_at(now, timing.retry_cadence) {
                match render_pattern(
                    renderer,
                    &program,
                    candidate.sample,
                    low,
                    self.scratch_texture.take(),
                ) {
                    Ok(texture) => {
                        self.scratch_texture = self.texture.replace(texture);
                        self.accept_candidate(candidate, Instant::now());
                    }
                    Err(_) => {
                        // The failed target may be incompatible after a GL
                        // reset. Keep the distinct visible texture, but make
                        // the next bounded attempt allocate a clean target.
                        self.scratch_texture = None;
                        self.render_failures.record_failure(Instant::now());
                    }
                }
            }
        } else if timing.advance {
            // No visible f32 sample changed, but this scheduled opportunity
            // was still consumed. Keep the configured deadline from becoming
            // an immediate retry loop.
            self.phase = candidate.phase;
            self.last_advance = now;
        }
        Some(CausticsElement {
            id: self.id.clone(),
            commit: self.commit,
            texture: self.texture.as_ref()?.clone(),
            geometry: physical,
        })
    }
}

struct PatternRenderFailure;

/// Renders one full frame of the analytical pattern into `reusable`
/// (reallocated if its size no longer matches). This target is the caller's
/// scratch texture, never the texture currently on screen, so any failure
/// leaves the last good frame untouched.
fn render_pattern(
    renderer: &mut GlesRenderer,
    program: &GlesPixelProgram,
    sample: CausticsSample,
    size: Size<i32, Physical>,
    reusable: Option<GlesTexture>,
) -> Result<GlesTexture, PatternRenderFailure> {
    let buffer_size: Size<i32, Buffer> = Size::from((size.w, size.h));
    let mut texture = match reusable.filter(|texture| texture.size() == buffer_size) {
        Some(texture) => texture,
        None => match renderer.create_buffer(Fourcc::Argb8888, buffer_size) {
            Ok(texture) => texture,
            Err(err) => {
                tracing::warn!(%err, "Failed to allocate caustics pattern texture");
                return Err(PatternRenderFailure);
            }
        },
    };
    let full = Rectangle::from_size(size);
    let render_result = (|| -> Result<(), GlesError> {
        let mut target = renderer.bind(&mut texture)?;
        let mut frame = renderer.render(&mut target, size, Transform::Normal)?;
        let damage = [full];
        let clear_result = frame.clear(Color32F::TRANSPARENT, &damage);
        let draw_result = if clear_result.is_ok() {
            frame.render_pixel_shader_to(
                program,
                Rectangle::from_size(buffer_size.to_f64()),
                full,
                buffer_size,
                Some(&damage),
                1.0,
                &[
                    Uniform::new("u_time", sample.time),
                    Uniform::new("u_intensity", sample.intensity),
                    Uniform::new("u_color", sample.color),
                    Uniform::new("u_scale", sample.scale),
                ],
            )
        } else {
            Ok(())
        };
        let finish_result = frame.finish();
        clear_result?;
        draw_result?;
        let _ = finish_result?;
        Ok(())
    })();
    if let Err(err) = render_result {
        tracing::warn!(%err, "Failed to update caustics pattern texture");
        return Err(PatternRenderFailure);
    }
    Ok(texture)
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
    fn candidate_state_is_committed_only_after_success() {
        let mut caustics = Caustics::default();
        let start = caustics.last_advance;
        let now = start + Duration::from_millis(20);
        let cfg = CausticsConfig::default();
        let candidate = caustics.candidate(&cfg, true, now);

        assert!(candidate.phase > caustics.phase);
        assert_eq!(caustics.phase, 0.0);
        assert!(caustics.last_sample.is_none());

        caustics.accept_candidate(candidate, now);
        assert_eq!(caustics.phase, candidate.phase);
        assert_eq!(caustics.last_sample, Some(candidate.sample));
        assert_eq!(caustics.last_advance, now);
    }

    #[test]
    fn long_stall_advance_is_capped() {
        let caustics = Caustics {
            last_advance: Instant::now() - Duration::from_secs(60),
            ..Caustics::default()
        };
        let candidate = caustics.candidate(&CausticsConfig::default(), true, Instant::now());
        assert!(candidate.phase <= Duration::from_millis(100).as_secs_f32());
    }

    #[test]
    fn repeated_failures_back_off_from_runtime_cadence_and_cap() {
        let cadence = Duration::from_secs_f64(1.0 / 37.0);
        let mut backoff = FailureBackoff::default();
        let mut now = Instant::now();
        assert!(backoff.ready_at(now, cadence));

        backoff.record_failure(now);
        assert_eq!(backoff.retry_in_at(now, cadence), cadence);
        now += cadence;
        assert!(backoff.ready_at(now, cadence));

        backoff.record_failure(now);
        assert_eq!(backoff.retry_in_at(now, cadence), cadence.saturating_mul(2));
        for _ in 0..32 {
            backoff.record_failure(now);
        }
        assert_eq!(
            backoff.retry_delay(cadence),
            cadence.saturating_mul(MAX_FAILURE_BACKOFF_MULTIPLIER)
        );

        backoff.clear();
        assert!(backoff.ready_at(now, cadence));
    }

    #[test]
    fn scheduled_frame_deadline_comes_from_configured_fps() {
        let fps = 37;
        let mut caustics = Caustics {
            last_sample: Some(CausticsSample {
                time: 0.0,
                intensity: 0.4,
                color: [0.2, 0.5, 0.8],
                scale: 1.3,
            }),
            ..Caustics::default()
        };
        let period = Duration::from_secs_f64(1.0 / f64::from(fps));

        assert!(caustics.next_frame_in(fps).unwrap() <= period);
        assert_eq!(caustics.next_frame_in(0), None);

        caustics.last_advance = Instant::now() - period.saturating_mul(2);
        assert_eq!(caustics.next_frame_in(fps), Some(Duration::ZERO));

        caustics.render_failures.record_failure(Instant::now());
        assert!(caustics.next_frame_in(fps).unwrap() > Duration::ZERO);
    }
}
