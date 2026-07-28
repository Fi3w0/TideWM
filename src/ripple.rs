//! Impulse ripple: the second piece of Phase R1's identity slice (see
//! AGENT.md's "Render and visual identity roadmap"). One shared primitive
//! for a radial disturbance from a point, decaying over time -- intended
//! to be triggered by different window-manager events (a window mapping
//! is a droplet impact at its center, a focus change ripples from the old
//! focus to the new one, an urgent hint pulses until acknowledged) rather
//! than three separate effects. The wave/aqua workspace transition
//! (Phase R1, later sub-slice) will be this same primitive's directional
//! variant, not a separate effect.
//!
//! This first cut ships only the primitive itself, one trigger (window
//! map), and a static-tint expanding-ring visual. The roadmap explicitly
//! calls R1 "built to be felt, not to be complete" -- the point right now
//! is something visible and recognizable as water, not the full event
//! surface. Gated on the `water_effects` config toggle the same way
//! `water_glass` is, since neither is meaningful with the identity off.

use std::time::{Duration, Instant};

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
        },
        utils::CommitCounter,
    },
    utils::{
        user_data::UserDataMap, Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
    },
};

/// Procedural fragment shader for an expanding ring with a soft falloff.
/// `v_coords` is in the element's buffer space (equal to logical pixels
/// here, since `RippleElement::src` builds the buffer rect at scale 1.0),
/// so `u_ring_radius` is supplied in the same logical-pixel units
/// `Ripple::radius` already returns. The Smithay pixel-shader framework
/// supplies `size` (the element's buffer size) and `alpha` (the element's
/// own alpha, see `RippleElement::alpha`) automatically -- only the two
/// `u_` uniforms are additional.
///
/// The shader must not contain a `#version` directive; Smithay prepends
/// `#version 100` itself (see `GlesRenderer::compile_custom_pixel_shader`'s
/// own contract). Mirrors `water_glass::WATER_GLASS_FRAGMENT_SHADER`'s
/// approach to that contract.
const RIPPLE_FRAGMENT_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform float u_ring_radius;
uniform vec3 u_tint;

void main() {
    vec2 center = size * 0.5;
    float d = length(v_coords - center);
    float ring_dist = abs(d - u_ring_radius);
    // Fixed half-width in pixels (not a fraction of size) so the ring
    // stays a consistent visual thickness as it expands rather than
    // thickening with the element's bounding square.
    float thickness = 8.0;
    float ring_alpha = smoothstep(thickness, 0.0, ring_dist) * alpha;
    gl_FragColor = vec4(u_tint, ring_alpha);
}
"#;

/// Compiles the ripple shader against `renderer` the first time it's
/// needed and caches it -- a `GlesPixelProgram` is cheap to clone
/// (`Arc`-backed) once compiled, so every frame just clones the cached
/// handle rather than recompiling. Per-renderer, not a process-global
/// `OnceLock`, since compiling needs a live `&mut GlesRenderer`. Same
/// pattern as `water_glass::water_glass_program`.
pub fn ripple_program(
    cache: &mut Option<GlesPixelProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesPixelProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_pixel_shader(
        RIPPLE_FRAGMENT_SHADER,
        &[
            UniformName::new("u_ring_radius", UniformType::_1f),
            UniformName::new("u_tint", UniformType::_3f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile ripple shader");
            None
        }
    }
}

/// One impulse: a center point on a specific output, a start time, and a
/// peak radius. Purely analytical -- no per-frame simulation, just
/// closed-form radius/alpha given elapsed time, sampled by
/// `RippleElement::from_ripple` building each frame's render element.
pub struct Ripple {
    /// Which output this ripple lives on. Ripples are output-local (the
    /// center is in `output`'s own logical space), and each output's
    /// render loop only draws the ripples it owns; a ripple spawned on
    /// output A never appears on output B even if its radius reaches
    /// into B's pixels. A multi-output ripple that visibly crosses the
    /// boundary in both outputs is a later scope item.
    pub output: String,
    /// Center in `output`-local logical space.
    pub center: Point<f64, Logical>,
    pub start: Instant,
    pub peak_radius: f32,
    pub duration: Duration,
    pub tint: [f32; 3],
    /// Stable identity carried into each frame's `RippleElement` so the
    /// damage tracker keys its bookkeeping off one entry for the lifetime
    /// of this impulse, not a fresh entry every frame -- see
    /// `water_glass::WaterGlassElement`'s own `id` field doc for the
    /// shape of the leak a fresh `Id` every frame would create in the
    /// tracker's per-element map.
    pub id: Id,
    /// Incremented every frame this ripple is rendered, so the damage
    /// tracker sees a fresh commit and redraws rather than assuming
    /// unchanged content and skipping the draw.
    pub commit: CommitCounter,
}

impl Ripple {
    /// Default tint: pale cyan, TideWM's identity color. Deliberately
    /// soft so the ring reads as a ripple in water rather than a neon
    /// highlight.
    pub const DEFAULT_TINT: [f32; 3] = [0.55, 0.85, 1.0];

    /// Default peak radius in logical pixels. Tuned to be visible at
    /// typical terminal/window sizes without dominating a small tile.
    pub const DEFAULT_PEAK_RADIUS: f32 = 220.0;

    /// Default lifetime. Long enough to register as water motion, short
    /// enough that rapid window mapping (opening three at once) doesn't
    /// stack into visual noise.
    pub const DEFAULT_DURATION: Duration = Duration::from_millis(650);

    pub fn new(output: String, center: Point<f64, Logical>) -> Self {
        Self {
            output,
            center,
            start: Instant::now(),
            peak_radius: Self::DEFAULT_PEAK_RADIUS,
            duration: Self::DEFAULT_DURATION,
            tint: Self::DEFAULT_TINT,
            id: Id::new(),
            commit: CommitCounter::default(),
        }
    }

    fn progress(&self) -> f32 {
        (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }

    /// Current ring radius in logical pixels. Ease-out cubic: fast
    /// expansion that decelerates as it approaches `peak_radius`, the
    /// shape a real droplet impact's wavefront makes on a still surface
    /// (energy radiating outward against increasing circumference).
    pub fn radius(&self) -> f32 {
        let p = self.progress();
        let eased = 1.0 - (1.0 - p).powi(3);
        eased * self.peak_radius
    }

    /// Current alpha. Quadratic fade from 1 to 0 over the lifetime --
    /// subtler than linear at the start (the ring stays visibly energetic
    /// for the first half) and faster near the end (it vanishes rather
    /// than trailing off into visual noise).
    pub fn alpha(&self) -> f32 {
        let p = self.progress();
        (1.0 - p).powi(2)
    }

    pub fn finished(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    /// Bounding square for the per-frame render element: side `2*radius`,
    /// centered on the impulse. Returned in the same output-local
    /// logical space `center` itself is given in.
    fn element_rect(&self) -> Rectangle<i32, Logical> {
        let r = self.radius() as i32;
        Rectangle::new(
            Point::from(((self.center.x as i32) - r, (self.center.y as i32) - r)),
            Size::from((2 * r, 2 * r)),
        )
    }
}

/// One ripple's render element for one frame: a procedural pixel-shader
/// quad sized to the ripple's current bounding square, drawing an
/// expanding fading ring within it. Built fresh every frame, but with the
/// underlying `Ripple`'s stable `id`/`commit` so the damage tracker
/// continues to recognize it as the same logical element across frames.
pub struct RippleElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<i32, Logical>,
    program: GlesPixelProgram,
    ring_radius: f32,
    alpha: f32,
    tint: [f32; 3],
}

impl RippleElement {
    /// Builds the element `ripple` produces for this frame, incrementing
    /// `ripple`'s commit counter so the next frame is visibly distinct to
    /// the damage tracker. Returns `None` if `ripple` is already
    /// `finished()` -- callers should `retain` finished ripples out of
    /// their list rather than relying on this, but the guard is here as
    /// a belt-and-braces check so a stray finished ripple can't render a
    /// zero-size element (`element_rect` would still produce one from a
    /// stale `radius`, since `finished` clamps to `peak_radius`, not
    /// zero).
    pub fn from_ripple(ripple: &mut Ripple, program: GlesPixelProgram) -> Option<Self> {
        if ripple.finished() {
            return None;
        }
        ripple.commit.increment();
        Some(Self {
            id: ripple.id.clone(),
            commit: ripple.commit,
            area: ripple.element_rect(),
            program,
            ring_radius: ripple.radius(),
            alpha: ripple.alpha(),
            tint: ripple.tint,
        })
    }
}

impl Element for RippleElement {
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
        self.alpha
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }

    // `opaque_regions` deliberately left at its default (empty): the
    // ring is fully translucent outside its thin annulus, and claiming
    // any opaque region would let the damage tracker skip drawing
    // whatever's behind it -- the same load-bearing reasoning as
    // `water_glass::WaterGlassElement`'s own omitted override.
}

impl RenderElement<GlesRenderer> for RippleElement {
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
            self.alpha,
            &[
                Uniform::new("u_ring_radius", self.ring_radius),
                Uniform::new("u_tint", self.tint),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_source_keeps_required_uniforms_and_omits_version_directive() {
        // Not a GL compile check (no EGL context in unit tests, see every
        // other backend/gles-adjacent module in this project) -- a guard
        // against accidentally dropping a uniform `compile_custom_pixel_shader`'s
        // contract requires, or accidentally adding a `#version` directive
        // the same function rejects (it prepends its own).
        assert!(!RIPPLE_FRAGMENT_SHADER.contains("#version"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float alpha"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform vec2 size"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("varying vec2 v_coords"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_ring_radius"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform vec3 u_tint"));
    }

    #[test]
    fn ripple_starts_at_zero_alpha_one_radius_zero_and_is_not_finished() {
        let ripple = Ripple::new(
            "eDP-1".to_string(),
            Point::from((100.0, 100.0)),
        );
        // progress() at elapsed=0 is 0, so radius is eased(0)*peak=0 and
        // alpha is (1-0)^2=1. Use small epsilon for timing jitter.
        assert!(ripple.radius() < 1.0);
        assert!((ripple.alpha() - 1.0).abs() < 0.05);
        assert!(!ripple.finished());
    }

    #[test]
    fn ripple_reaches_peak_radius_and_finishes_after_duration() {
        let mut ripple = Ripple::new("eDP-1".to_string(), Point::from((0.0, 0.0)));
        ripple.duration = Duration::from_millis(10);
        std::thread::sleep(Duration::from_millis(25));
        assert!((ripple.radius() - ripple.peak_radius).abs() < 0.1);
        assert!(ripple.alpha() < 0.01);
        assert!(ripple.finished());
        // The early-return `None` branch of `from_ripple` (which runs
        // before any access to the program argument) is exercised by
        // integration testing against a real renderer; here we only
        // confirm the precondition `finished()` itself reports true, so
        // that branch is the one that would run.
    }

    #[test]
    fn element_rect_is_a_centered_square_of_side_two_radius() {
        let ripple = Ripple::new(
            "eDP-1".to_string(),
            Point::from((300.0, 200.0)),
        );
        let rect = ripple.element_rect();
        assert_eq!(rect.size.w, rect.size.h);
        // The rect's center should match `ripple.center` (within
        // i32 truncation). Center is at loc + size/2.
        let cx = rect.loc.x + rect.size.w / 2;
        let cy = rect.loc.y + rect.size.h / 2;
        assert!((cx - 300).abs() <= 1);
        assert!((cy - 200).abs() <= 1);
    }
}
