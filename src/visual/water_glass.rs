//! Water-glass, the render roadmap's first actually-visible effect (Phase
//! R1, see AGENT.md's "Render and visual identity roadmap"). Renders a
//! captured backdrop (`backdrop.rs`, Phase R0.5) through a custom GLES
//! fragment shader that perturbs the sample coordinate, giving a wavy,
//! refracted look instead of the plain flat content that would otherwise
//! sit behind a semi-transparent window.
//!
//! Applies specifically to a floating window with a `window_opacity` rule
//! below 1.0 -- reusing that existing rule rather than inventing a new
//! config key, since "what shows through a semi-transparent window" is
//! exactly what this already means. The water-glass layer draws fully
//! opaque at the window's own rect, *behind* the window's own (semi-
//! transparent) surface element: without that, the real undistorted
//! backdrop would still show through underneath the distorted copy,
//! reading as a ghost rather than glass.

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

use std::hash::{Hash, Hasher};
use std::time::Instant;

/// Base phase drift rate, radians per second at `speed = 1.0`. Slow
/// enough that ambient mode reads as water, not vibration.
const PHASE_RATE: f32 = 2.2;

/// Per-window water-glass animation state, stored in
/// `Smallvil::glass_anim`. Closed-form like the rest of this codebase's
/// motion (`viscosity.rs`, `sway.rs`, `ripple.rs`): no per-frame
/// integration, just a phase derived from an epoch and a smoothstep
/// settle envelope over the time since the last disturbance.
pub struct GlassAnim {
    /// Phase origin for this window. Per-window (not process-global) so a
    /// window becoming glass mid-session doesn't jump to some arbitrary
    /// shared angle; continuity only has to hold per window.
    epoch: Instant,
    /// Last time the glass was disturbed: the window moved or a ripple
    /// passed underneath.
    last_kick: Instant,
    last_rect: Rectangle<i32, Physical>,
}

impl GlassAnim {
    /// A freshly appeared glass window starts kicked: mapping disturbs
    /// the water it lands on, same premise as the map ripple.
    pub fn new(rect: Rectangle<i32, Physical>) -> Self {
        let now = Instant::now();
        Self {
            epoch: now,
            last_kick: now,
            last_rect: rect,
        }
    }

    /// Returns true (and re-stamps the disturbance clock) when the glass
    /// rectangle changes or an active ripple intersects it. Backdrop
    /// recaptures do not restart the settle tail; otherwise the tail
    /// manufactures its own next kick.
    pub fn observe(&mut self, rect: Rectangle<i32, Physical>, ripple_passed: bool) -> bool {
        let changed = rect != self.last_rect;
        self.last_rect = rect;
        if changed || ripple_passed {
            self.last_kick = Instant::now();
            true
        } else {
            false
        }
    }

    /// Smoothstep decay from 1 at the last disturbance to 0 after
    /// `settle_ms`, so the distortion eases out instead of snapping off.
    pub fn envelope(&self, settle_ms: u32) -> f32 {
        let x = (self.last_kick.elapsed().as_secs_f32() * 1000.0 / settle_ms.max(1) as f32)
            .clamp(0.0, 1.0);
        1.0 - x * x * (3.0 - 2.0 * x)
    }

    /// Current wave phase in radians.
    pub fn phase(&self, speed: f32) -> f32 {
        self.epoch.elapsed().as_secs_f32() * speed * PHASE_RATE
    }
}

/// Adapted directly from Smithay's own default texture fragment shader
/// (`backend/renderer/gles/shaders/implicit/texture.frag` in the pinned
/// source) -- same `//_DEFINES_` placeholder and `#if defined(...)`
/// blocks, since `compile_custom_texture_shader` substitutes the same set
/// of defines regardless of which custom shader is compiled. The only
/// change from the original is the sampling coordinate: a small,
/// position-based sine/cosine offset instead of a straight `v_coords`
/// lookup. The offset is animated by two extra uniforms: `u_phase`
/// advances the wave over time and `u_amp` scales the distortion
/// strength. Static mode passes `u_phase = 0, u_amp = amplitude` and
/// reproduces the original fixed look exactly; reactive mode modulates
/// `u_amp` by a settle envelope; ambient mode lets `u_phase` run
/// continuously. See `config::WaterGlassConfig`.
const WATER_GLASS_FRAGMENT_SHADER: &str = r#"
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
uniform vec2 u_size;
uniform vec4 u_corner_radii;
uniform float u_rounding_power;
uniform float u_antialias;
uniform float u_phase;
uniform float u_amp;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

float rounded_coverage(vec2 point) {
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
    if (radius < 0.01)
        return 1.0;
    vec2 q = abs(point - center) / radius;
    float d = pow(
        pow(q.x, u_rounding_power) + pow(q.y, u_rounding_power),
        1.0 / u_rounding_power
    );
    float aa = u_antialias / radius;
    return 1.0 - smoothstep(1.0 - aa, 1.0 + aa, d);
}

void main() {
    vec2 distorted = v_coords + u_amp * vec2(
        sin(v_coords.y * 24.0 + u_phase) * 0.012,
        cos(v_coords.x * 20.0 + u_phase * 0.85) * 0.012
    );
    distorted = clamp(distorted, 0.0, 1.0);
    vec4 color = texture2D(tex, distorted);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif
    color *= rounded_coverage(v_coords * u_size);

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

/// Compiles the shader against `renderer` the first time it's needed and
/// caches it -- a `GlesTexProgram` is cheap to clone (`Arc`-backed) once
/// compiled, so every frame just clones the cached handle rather than
/// recompiling. Per-renderer, not a process-global `OnceLock` like
/// `toast::font()`, since compiling needs a live `&mut GlesRenderer`.
pub fn water_glass_program(
    cache: &mut Option<GlesTexProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesTexProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_texture_shader(
        WATER_GLASS_FRAGMENT_SHADER,
        &[
            UniformName::new("u_size", UniformType::_2f),
            UniformName::new("u_corner_radii", UniformType::_4f),
            UniformName::new("u_rounding_power", UniformType::_1f),
            UniformName::new("u_antialias", UniformType::_1f),
            UniformName::new("u_phase", UniformType::_1f),
            UniformName::new("u_amp", UniformType::_1f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile water-glass shader");
            None
        }
    }
}

/// Rendered-value fingerprint for the glass element's damage identity, the
/// same contract `decoration::border_commit` already documents: the damage
/// tracker only ever compares it for equality, never treats it as a
/// chronological counter. Folds in everything that changes the drawn result
/// without moving the element: the backdrop capture's content `version`,
/// the wave `phase`/`amp`, and the corner uniforms. `phase` only counts
/// while it can visibly distort (`amp > 0`) -- a settled reactive tail or
/// static mode holds a constant commit even though the phase clock keeps
/// running, which is what lets a static desktop stop redrawing the glass
/// layer at all.
pub fn water_glass_commit(
    capture_version: usize,
    phase: f32,
    amp: f32,
    corner_radii: [f32; 4],
    rounding_power: f32,
    antialias: f32,
) -> CommitCounter {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    capture_version.hash(&mut hash);
    amp.to_bits().hash(&mut hash);
    if amp > 0.0 {
        phase.to_bits().hash(&mut hash);
    }
    for radius in corner_radii {
        radius.to_bits().hash(&mut hash);
    }
    rounding_power.to_bits().hash(&mut hash);
    antialias.to_bits().hash(&mut hash);
    CommitCounter::from(hash.finish() as usize)
}

/// One window's water-glass layer for one frame: the captured backdrop
/// texture run through the refraction shader, drawn at the window's own
/// rect. `id`/`commit` are the *stable* per-window identity kept in
/// `backdrop::BackdropCapture` (not a fresh `Id` each frame) -- the damage
/// tracker keys its own per-element bookkeeping off `Id`, and a fresh one
/// every frame would leak an orphaned entry in that tracker's internal
/// map for every frame this window is water-glass-eligible, never pruned.
pub struct WaterGlassElement {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    geometry: Rectangle<i32, Physical>,
    program: GlesTexProgram,
    corner_radii: [f32; 4],
    rounding_power: f32,
    antialias: f32,
    opacity: f32,
    /// Wave phase in radians and distortion-strength multiplier for this
    /// frame, resolved by `Smallvil::glass_frame_elements` from
    /// `config::WaterGlassConfig` plus the window's animation state.
    phase: f32,
    amp: f32,
}

impl WaterGlassElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Id,
        commit: CommitCounter,
        texture: GlesTexture,
        geometry: Rectangle<i32, Physical>,
        program: GlesTexProgram,
        corner_radii: [f32; 4],
        rounding_power: f32,
        antialias: f32,
        opacity: f32,
        phase: f32,
        amp: f32,
    ) -> Self {
        Self {
            id,
            commit,
            texture,
            geometry,
            program,
            corner_radii,
            rounding_power,
            antialias,
            opacity,
            phase,
            amp,
        }
    }
}

impl Element for WaterGlassElement {
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
        self.opacity
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }

    // `opaque_regions` deliberately left at its default (empty): this
    // element normally draws at alpha 1.0 (temporarily lower during a
    // lifecycle fade), but claiming opacity here would tell the damage tracker it
    // can skip drawing whatever's behind it -- and the whole point of
    // this element existing is that it visually replaces that content,
    // not that nothing needs to be there. Getting this wrong is exactly
    // the kind of "looks plausible, quietly skips a draw" bug this
    // project has already been burned by once (session-lock's element
    // ordering).
}

impl RenderElement<GlesRenderer> for WaterGlassElement {
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
                Uniform::new(
                    "u_size",
                    [
                        self.geometry.size.w.max(1) as f32,
                        self.geometry.size.h.max(1) as f32,
                    ],
                ),
                Uniform::new("u_corner_radii", self.corner_radii),
                Uniform::new("u_rounding_power", self.rounding_power),
                Uniform::new("u_antialias", self.antialias),
                Uniform::new("u_phase", self.phase),
                Uniform::new("u_amp", self.amp),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_source_keeps_the_defines_placeholder_and_required_uniforms() {
        // Not a GL compile check (no EGL context in unit tests, see every
        // other backend/gles-adjacent module in this project) -- just a
        // guard against accidentally dropping the substitution point or a
        // uniform `compile_custom_texture_shader`'s contract requires.
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("//_DEFINES_"));
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("uniform sampler2D tex"));
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("uniform float alpha"));
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("varying vec2 v_coords"));
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("u_corner_radii"));
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("uniform float u_phase"));
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("uniform float u_amp"));
    }

    #[test]
    fn envelope_decays_from_one_to_zero_over_settle() {
        let rect = Rectangle::new((0, 0).into(), (100, 100).into());
        let anim = GlassAnim::new(rect);
        assert!(anim.envelope(1200) > 0.99);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mid = anim.envelope(60);
        assert!(mid < 0.99, "envelope should be decaying, got {mid}");
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert_eq!(anim.envelope(60), 0.0);
    }

    #[test]
    fn observe_ignores_self_generated_capture_commits() {
        let rect = Rectangle::new((0, 0).into(), (100, 100).into());
        let mut anim = GlassAnim::new(rect);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let before = anim.envelope(40);
        assert!(!anim.observe(rect, false));

        assert!(!anim.observe(rect, false));
        assert!(anim.envelope(40) <= before);

        std::thread::sleep(std::time::Duration::from_millis(20));
        let settled = anim.envelope(40);
        assert!(anim.observe(rect, true));
        assert!(anim.envelope(40) > settled);

        let moved = Rectangle::new((10, 0).into(), (100, 100).into());
        assert!(anim.observe(moved, false));
    }

    #[test]
    fn water_glass_commit_is_stable_when_the_scene_is_static() {
        // The damage-driven capture only bumps `version` when it actually
        // re-rendered; the element's commit is built from it, so a static
        // desktop (same capture, same geometry/config/animation) must hash
        // to the same value across frames or the visible output never stops
        // redrawing the glass layer.
        let baseline = water_glass_commit(3, 1.42, 0.0, [4.0; 4], 2.0, 1.5);
        assert_eq!(
            baseline,
            water_glass_commit(3, 1.42, 0.0, [4.0; 4], 2.0, 1.5)
        );
    }

    #[test]
    fn water_glass_commit_advances_when_the_capture_re_renders() {
        // `version` is what connects a recapture to the visible frame's
        // decision to redraw the glass: a behind-scene change that produced
        // a new texture has to flip the commit so the new pixels are drawn.
        let before = water_glass_commit(3, 1.42, 0.0, [4.0; 4], 2.0, 1.5);
        let after = water_glass_commit(4, 1.42, 0.0, [4.0; 4], 2.0, 1.5);
        assert_ne!(before, after);
    }

    #[test]
    fn water_glass_commit_advances_while_the_wave_is_animating() {
        // Ambient and reactive tails advance the phase every frame while the
        // distortion is visible; the commit has to follow so the wave keeps
        // moving on screen instead of freezing after the first frame.
        let first = water_glass_commit(3, 1.42, 0.5, [4.0; 4], 2.0, 1.5);
        let second = water_glass_commit(3, 1.43, 0.5, [4.0; 4], 2.0, 1.5);
        assert_ne!(first, second);
    }

    #[test]
    fn water_glass_commit_ignores_phase_once_settled() {
        // Reactive mode keeps the phase clock running forever, but once the
        // settle envelope drives amplitude to zero the phase can no longer
        // change the image. Holding the commit constant at that point is
        // exactly what stops the static-tail from self-sustaining redraws.
        let settled = water_glass_commit(3, 5.0, 0.0, [4.0; 4], 2.0, 1.5);
        assert_eq!(settled, water_glass_commit(3, 9.9, 0.0, [4.0; 4], 2.0, 1.5));
    }

    #[test]
    fn water_glass_commit_advances_on_corner_config_change() {
        // A config hot-reload that changes only the rounded-corner uniforms
        // has to redraw, and `damage_since` compares the commit for that;
        // geometry/alpha/z changes are caught by the tracker separately.
        let before = water_glass_commit(3, 0.0, 0.5, [4.0; 4], 2.0, 1.5);
        let after = water_glass_commit(3, 0.0, 0.5, [8.0; 4], 2.0, 1.5);
        assert_ne!(before, after);
    }
}
