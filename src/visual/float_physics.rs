//! Render-only 2D bob and drift for floating windows. A disturbance samples
//! the current offset and reseeds a bounded, exponentially decaying cosine/
//! sine pair. The closed-form sample stores no motion history, allocates no
//! render resource, and stops requesting frames after settling.

use std::time::Instant;

/// Combined offset magnitude below which the motion is treated as settled,
/// logical pixels. Matches `sway.rs`/`viscosity.rs`'s settle threshold.
const SETTLE_EPSILON: f64 = 0.25;

#[derive(Debug)]
pub struct FloatPhysics {
    start: Instant,
    /// Signed lateral amplitude at kick time, logical pixels. Cosine phase,
    /// so the swing always starts exactly here and re-kicks are continuous.
    amp_x: f64,
    /// Vertical amplitude at kick time, logical pixels. Sine phase, so it
    /// starts at a zero crossing and bobs as the lateral term swings.
    amp_y: f64,
    /// Angular frequency, radians per second. Shared by both axes so the
    /// bob reads as one coherent motion rather than two unrelated ones.
    omega: f64,
    /// Exponential decay rate, per second. Higher settles faster.
    damping: f64,
}

impl FloatPhysics {
    /// First kick from rest. Equivalent to a zeroed record followed by
    /// `kick`, kept as its own constructor to mirror `sway.rs`'s shape and
    /// make the from-rest case legible at the call site.
    pub fn kicked(
        impulse: (f64, f64),
        response: f64,
        max_offset: f64,
        bob_ratio: f64,
        frequency_hz: f64,
        damping: f64,
    ) -> Self {
        let mut state = Self {
            start: Instant::now(),
            amp_x: 0.0,
            amp_y: 0.0,
            omega: 2.0 * std::f64::consts::PI * frequency_hz.max(0.0),
            damping: damping.max(0.0),
        };
        state.kick(impulse, response, max_offset, bob_ratio);
        state
    }

    /// Absorbs one 2D disturbance. Sampling both axes first keeps an ongoing
    /// oscillation continuous on the lateral term: the new swing starts
    /// where the old one currently is, plus the configured fraction of the
    /// impulse. The vertical term adds `bob_ratio` of the impulse magnitude
    /// on top, so even a purely horizontal disturbance produces a bob. The
    /// combined envelope is clamped to `max_offset` so a continuous gesture
    /// can't accumulate unbounded displacement.
    pub fn kick(&mut self, impulse: (f64, f64), response: f64, max_offset: f64, bob_ratio: f64) {
        let (cur_x, cur_y) = self.sample();
        let new_x = cur_x + impulse.0 * response;
        let magnitude = impulse.0.hypot(impulse.1);
        let new_y = cur_y + impulse.1 * response + bob_ratio * magnitude;
        let env = new_x.hypot(new_y);
        let (amp_x, amp_y) = if max_offset > 0.0 && env > max_offset {
            let scale = max_offset / env;
            (new_x * scale, new_y * scale)
        } else {
            (new_x, new_y)
        };
        self.amp_x = amp_x;
        self.amp_y = amp_y;
        self.start = Instant::now();
    }

    pub fn sample(&self) -> (f64, f64) {
        self.sample_at(Instant::now())
    }

    fn sample_at(&self, now: Instant) -> (f64, f64) {
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        let env = (-self.damping * elapsed).exp();
        let phase = self.omega * elapsed;
        (
            self.amp_x * env * phase.cos(),
            self.amp_y * env * phase.sin(),
        )
    }

    /// The decay envelope, not the instantaneous offset, decides rest: a
    /// window passing through the midpoint of a bob is not finished.
    pub fn finished(&self) -> bool {
        self.amp_x.hypot(self.amp_y) * (-self.damping * self.start.elapsed().as_secs_f64()).exp()
            <= SETTLE_EPSILON
    }
}

/// Distance-scaled impulse fraction for a nearby floating window: `1.0` at
/// the source itself, falling off linearly to zero at `radius`, and `None`
/// beyond it (so distant windows are not disturbed and no zeroed entry is
/// inserted). Pure and unit-testable; the state layer passes global logical
/// coordinates so the result is identical under either spatial engine.
pub fn falloff_kick(source: (f64, f64), target: (f64, f64), radius: f64) -> Option<f64> {
    let dist = (target.0 - source.0).hypot(target.1 - source.1);
    if dist <= f64::EPSILON {
        return Some(1.0);
    }
    if radius <= 0.0 || dist >= radius {
        return None;
    }
    Some(1.0 - dist / radius)
}

/// Continuous ambient "sitting on water" offset (`toggle-float-ambient`),
/// independent of the kick/decay model above. Real buoyancy sims don't
/// fire discrete impulses at an idle floating object -- they track a
/// smooth wave-height function and let the object follow it. This is that
/// function: a small sum of sine waves at incommensurate frequency
/// multiples and phases (`period_s` scales the slowest/dominant one; the
/// others are fixed ratios off it), so the path is continuous, never
/// settles, and doesn't visibly repeat on a short cycle -- the classic
/// "sum a few waves" trick real ocean-shader/buoyancy implementations use
/// instead of simulating a full wave field for one bobbing object. `elapsed`
/// is seconds since ambient was toggled on for this window, so two windows
/// toggled on at different times drift out of phase with each other for
/// free. Each axis's own coefficients sum to `<= 1.0`, so the result is
/// bounded to `amplitude` by construction -- no separate clamp needed.
pub fn ambient_sample(elapsed: f64, amplitude: f64, period_s: f64) -> (f64, f64) {
    let omega = std::f64::consts::TAU / period_s.max(0.001);
    let x = amplitude * (0.6 * (omega * elapsed).sin() + 0.4 * (omega * 1.9 * elapsed + 1.3).sin());
    let y = amplitude
        * (0.5 * (omega * 1.3 * elapsed + 0.6).sin() + 0.3 * (omega * 2.7 * elapsed + 2.1).sin());
    (x, y)
}

/// Combined offset+velocity magnitude below which a `full`-tier body is
/// treated as settled. A separate constant from `SETTLE_EPSILON` (rather
/// than reusing it for both position and velocity) since the two are
/// different units; both happen to read fine at the same numeric value.
const BODY_SETTLE_EPSILON: f64 = 0.25;

/// A `full`-tier floating window: a unit mass-spring-damper body anchored
/// at the window's own logical position (offset `(0, 0)`), carrying real
/// velocity state instead of `FloatPhysics`' closed-form amplitude/phase.
/// Velocity state is what makes collision impulse exchange possible --
/// closed-form motion has no notion of "how fast is this moving right
/// now" to hand to another body. Advanced by `step_body`, a plain
/// semi-implicit-Euler integrator; the state itself holds no clock, unlike
/// `FloatPhysics`, since a rigid-body sim needs a fixed timestep loop
/// driving it forward regardless (see `Smallvil::update_float_physics_full`),
/// not a "sample any time" closed form.
#[derive(Debug, Clone, Copy, Default)]
pub struct FloatBody {
    pub offset: (f64, f64),
    pub velocity: (f64, f64),
}

impl FloatBody {
    pub fn at_rest() -> Self {
        Self::default()
    }

    /// Absorbs one instantaneous disturbance as a velocity change (an
    /// impulse, not a position jump) -- the collision solver and the wave
    /// field both already work purely in velocity/acceleration terms, so a
    /// drag/map/workspace-wave kick joins them the same way rather than
    /// needing its own special-cased application.
    pub fn kick(&mut self, impulse: (f64, f64), response: f64) {
        self.velocity.0 += impulse.0 * response;
        self.velocity.1 += impulse.1 * response;
    }

    /// Settled when not visibly displaced and not visibly moving. Callers
    /// combine this with whether continuous wave forcing is active for the
    /// window -- a body can be numerically "settled" for an instant while
    /// still being forced right back out by the wave on the very next step,
    /// which is exactly the "wave on never settles" behavior by design.
    pub fn finished(&self) -> bool {
        self.offset.0.hypot(self.offset.1) <= BODY_SETTLE_EPSILON
            && self.velocity.0.hypot(self.velocity.1) <= BODY_SETTLE_EPSILON
    }
}

/// Whether a full-tier simulation still needs its complete set of bodies.
/// Continuous wave forcing keeps every body present; with the wave off, a
/// kick keeps the set alive only until every body has settled. Keeping this
/// decision separate from body discovery prevents an idle floating window
/// from being recreated at rest on every backend timer tick.
pub(crate) fn bodies_need_simulation<'a>(
    wave_enabled: bool,
    bodies: impl IntoIterator<Item = &'a FloatBody>,
) -> bool {
    wave_enabled || bodies.into_iter().any(|body| !body.finished())
}

/// Advances one body by `dt` seconds under a damped-spring pull toward
/// `target` -- `(0, 0)` when the wave field is off (so the body settles
/// back to its real logical position), or the current wave height when
/// it's on (so the body never fully settles, chasing a moving anchor
/// instead). `stiffness`/`drag` play the spring/damping role
/// `frequency`/`damping` already play for `light` tier's closed form,
/// reusing those same two config knobs rather than adding a parallel pair
/// -- see `Smallvil::update_float_physics_full` for the mapping. Uses
/// semi-implicit ("symplectic") Euler: velocity updates from the current
/// acceleration first, then position updates from the *new* velocity --
/// unconditionally stable for a damped spring at reasonable step sizes,
/// unlike plain (explicit) Euler which can blow up. `max_offset` is a hard
/// positional clamp (not just an energy/amplitude clamp like `kick`'s
/// above): this is render-offset-only, so any unclamped drift means a
/// click lands where the window visually isn't. Clamping zeroes the
/// outward radial velocity component so a body pressed against its own
/// leash doesn't keep straining against it frame after frame -- it can
/// still slide tangentially along the boundary.
pub fn step_body(
    body: &mut FloatBody,
    target: (f64, f64),
    stiffness: f64,
    drag: f64,
    max_offset: f64,
    dt: f64,
) {
    let ax = stiffness * (target.0 - body.offset.0) - drag * body.velocity.0;
    let ay = stiffness * (target.1 - body.offset.1) - drag * body.velocity.1;
    body.velocity.0 += ax * dt;
    body.velocity.1 += ay * dt;
    body.offset.0 += body.velocity.0 * dt;
    body.offset.1 += body.velocity.1 * dt;
    if max_offset <= 0.0 {
        return;
    }
    let mag = body.offset.0.hypot(body.offset.1);
    if mag <= max_offset {
        return;
    }
    let scale = max_offset / mag;
    body.offset.0 *= scale;
    body.offset.1 *= scale;
    let (nx, ny) = (body.offset.0 / max_offset, body.offset.1 / max_offset);
    let radial = body.velocity.0 * nx + body.velocity.1 * ny;
    if radial > 0.0 {
        body.velocity.0 -= radial * nx;
        body.velocity.1 -= radial * ny;
    }
}

/// Continuous traveling-wave target for `full` tier's spring anchor,
/// keyed by a body's own fixed world X position (not its current offset,
/// which would feed the wave's own output back into itself) rather than
/// per-window elapsed time like `ambient_sample` -- so windows spread out
/// left to right visibly ripple in sequence like one shared surface,
/// instead of every window bobbing in lockstep. Reuses `ambient_sample`'s
/// "sum a couple of incommensurate sines" shape for the same
/// never-quite-repeats feel, as a real traveling wave `sin(kx - wt)`
/// rather than a per-window phase offset.
pub fn wave_target(
    world_x: f64,
    elapsed: f64,
    amplitude: f64,
    wavelength: f64,
    speed: f64,
) -> (f64, f64) {
    let k = std::f64::consts::TAU / wavelength.max(1.0);
    let omega = k * speed;
    let phase = k * world_x - omega * elapsed;
    let y = amplitude * (0.7 * phase.sin() + 0.3 * (1.9 * phase + 1.1).sin());
    let x = amplitude * 0.3 * phase.cos();
    (x, y)
}

/// Resolves one AABB overlap between two `full`-tier bodies as a single
/// impulse along the shallower overlap axis (the usual "minimum
/// translation axis" pick for a box-vs-box collision normal) -- velocity
/// exchange only, never positional separation. Floating windows already
/// sit stacked at rest by user choice (deliberately overlapped windows are
/// a normal floating-desktop thing), so a resting, fully-overlapped pair
/// must exchange nothing: returns `false` without touching either
/// velocity whenever the relative velocity along the resolved normal is
/// separating or zero, and only actually exchanges an impulse when the
/// two are closing. `restitution` is the usual bounciness coefficient (`0`
/// perfectly inelastic, `1` perfectly elastic); masses are assumed
/// positive (callers clamp window area to a small minimum). Rects are
/// `(x, y, w, h)` in the same coordinate space as both bodies' offsets.
pub fn resolve_collision(
    a_rect: (f64, f64, f64, f64),
    b_rect: (f64, f64, f64, f64),
    a_vel: &mut (f64, f64),
    b_vel: &mut (f64, f64),
    a_mass: f64,
    b_mass: f64,
    restitution: f64,
) -> bool {
    let (ax, ay, aw, ah) = a_rect;
    let (bx, by, bw, bh) = b_rect;
    let (acx, acy) = (ax + aw / 2.0, ay + ah / 2.0);
    let (bcx, bcy) = (bx + bw / 2.0, by + bh / 2.0);
    let overlap_x = (aw + bw) / 2.0 - (acx - bcx).abs();
    let overlap_y = (ah + bh) / 2.0 - (acy - bcy).abs();
    if overlap_x <= 0.0 || overlap_y <= 0.0 {
        return false;
    }
    // Normal points from A's center to B's, along whichever axis has the
    // shallower overlap (the usual box-collision "minimum translation
    // axis" pick).
    let (nx, ny) = if overlap_x < overlap_y {
        (if bcx >= acx { 1.0 } else { -1.0 }, 0.0)
    } else {
        (0.0, if bcy >= acy { 1.0 } else { -1.0 })
    };
    let (rvx, rvy) = (b_vel.0 - a_vel.0, b_vel.1 - a_vel.1);
    let closing_speed = rvx * nx + rvy * ny;
    if closing_speed >= 0.0 {
        return false;
    }
    let inv_a = if a_mass > 0.0 { 1.0 / a_mass } else { 0.0 };
    let inv_b = if b_mass > 0.0 { 1.0 / b_mass } else { 0.0 };
    let j = -(1.0 + restitution) * closing_speed / (inv_a + inv_b).max(f64::EPSILON);
    a_vel.0 -= j * inv_a * nx;
    a_vel.1 -= j * inv_a * ny;
    b_vel.0 += j * inv_b * nx;
    b_vel.1 += j * inv_b * ny;
    true
}

/// Bounces a body's velocity off an output edge when its current rect has
/// crossed that edge while still moving further out -- an infinite-mass
/// wall, so only the body's own velocity changes, the same one-sided
/// "only when approaching" rule `resolve_collision` uses against another
/// body. Lets floating windows read as bobbing in a basin instead of open
/// water with no walls. `rect`/`output` are `(x, y, w, h)` in the same
/// coordinate space.
pub fn resolve_edge_collision(
    rect: (f64, f64, f64, f64),
    vel: &mut (f64, f64),
    output: (f64, f64, f64, f64),
    restitution: f64,
) {
    let (x, y, w, h) = rect;
    let (ox, oy, ow, oh) = output;
    if x < ox && vel.0 < 0.0 {
        vel.0 = -vel.0 * restitution;
    }
    if x + w > ox + ow && vel.0 > 0.0 {
        vel.0 = -vel.0 * restitution;
    }
    if y < oy && vel.1 < 0.0 {
        vel.1 = -vel.1 * restitution;
    }
    if y + h > oy + oh && vel.1 > 0.0 {
        vel.1 = -vel.1 * restitution;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn lateral_axis_starts_at_the_impulse_and_oscillates_through_zero() {
        // Purely lateral impulse: amp_x carries it, amp_y stays zero.
        let mut state = FloatPhysics::kicked((10.0, 0.0), 1.0, 24.0, 0.0, 1.0, 0.5);
        let (x, y) = state.sample();
        assert!((x - 10.0).abs() < 0.5, "lateral starts at impulse");
        assert!(y.abs() < 0.25, "no vertical motion from a lateral impulse");
        // Quarter period at 1 Hz: cosine crosses zero, sine peaks.
        state.start = Instant::now() - Duration::from_millis(250);
        let (x, y) = state.sample();
        assert!(x.abs() < 0.5, "lateral crosses zero at quarter period");
        // amp_y is zero here, so y stays zero regardless of phase.
        assert!(y.abs() < SETTLE_EPSILON);
    }

    #[test]
    fn vertical_bob_is_phase_offset_from_the_lateral_term() {
        // Equal lateral and vertical energy: at the kick instant the lateral
        // term is at full amplitude while the vertical term is at its zero
        // crossing, then a quarter period later they trade places -- the
        // elliptical read that distinguishes a bob from a diagonal slide.
        let mut state = FloatPhysics::kicked((10.0, 0.0), 1.0, 24.0, 1.0, 1.0, 0.5);
        // bob_ratio 1.0 + |impulse| 10 -> amp_y = 10, matching amp_x.
        let (x, y) = state.sample();
        assert!((x - 10.0).abs() < 0.5, "lateral at peak at kick");
        assert!(y.abs() < 0.5, "vertical at zero crossing at kick");
        state.start = Instant::now() - Duration::from_millis(250);
        let (x, y) = state.sample();
        assert!(x.abs() < 0.5, "lateral at zero a quarter period later");
        assert!(
            y > 7.0 && y < 10.0,
            "vertical near its peak a quarter period later, decayed"
        );
    }

    #[test]
    fn decay_eventually_settles() {
        let mut state = FloatPhysics::kicked((24.0, 0.0), 1.0, 24.0, 0.6, 1.6, 3.0);
        assert!(!state.finished());
        state.start = Instant::now() - Duration::from_secs(3);
        assert!(state.finished());
        let (x, y) = state.sample();
        assert!(x.hypot(y) < SETTLE_EPSILON);
    }

    #[test]
    fn lateral_kick_continues_from_the_current_offset() {
        let mut state = FloatPhysics::kicked((5.0, 0.0), 1.0, 24.0, 0.0, 1.6, 3.0);
        // Lateral continuity: a new kick samples the current x and adds to it.
        state.kick((5.0, 0.0), 1.0, 24.0, 0.0);
        let (x, _) = state.sample();
        assert!((x - 10.0).abs() < 0.5, "lateral re-kick is continuous");
    }

    #[test]
    fn combined_envelope_is_clamped_to_max_offset() {
        let mut state = FloatPhysics::kicked((0.0, 0.0), 1.0, 24.0, 0.0, 1.6, 3.0);
        // Huge impulse: the combined magnitude clamps to max_offset, not each axis.
        state.kick((100.0, 100.0), 1.0, 24.0, 0.0);
        let env = state.amp_x.hypot(state.amp_y);
        assert!((env - 24.0).abs() < 0.5, "envelope clamped to max_offset");
        // Direction preserved: both components positive, roughly equal.
        assert!(state.amp_x > 0.0 && state.amp_y > 0.0);
    }

    #[test]
    fn zero_impulse_is_immediately_finished() {
        let state = FloatPhysics::kicked((0.0, 0.0), 1.0, 24.0, 0.6, 1.6, 3.0);
        assert!(state.finished());
        let (x, y) = state.sample();
        assert_eq!((x, y), (0.0, 0.0));
    }

    #[test]
    fn falloff_is_full_at_the_source_and_zero_at_radius() {
        let source = (100.0, 100.0);
        assert_eq!(falloff_kick(source, source, 256.0), Some(1.0));
        // Halfway: half the impulse.
        assert_eq!(falloff_kick(source, (100.0, 228.0), 256.0), Some(0.5));
        // At the radius edge: None, not a zero-strength kick.
        assert_eq!(falloff_kick(source, (100.0, 356.0), 256.0), None);
        // Beyond: None.
        assert_eq!(falloff_kick(source, (100.0, 500.0), 256.0), None);
        // Zero radius disables neighbor disturbance entirely.
        assert_eq!(falloff_kick(source, (100.0, 1.0), 0.0), None);
    }

    #[test]
    fn ambient_sample_stays_bounded_to_amplitude() {
        let mut t = 0.0;
        while t < 60.0 {
            let (x, y) = ambient_sample(t, 24.0, 5.0);
            assert!(x.abs() <= 24.0 + 1e-9, "x exceeded amplitude at t={t}");
            assert!(y.abs() <= 24.0 + 1e-9, "y exceeded amplitude at t={t}");
            t += 0.137;
        }
    }

    #[test]
    fn ambient_sample_is_a_pure_function_of_elapsed_time() {
        assert_eq!(
            ambient_sample(3.7, 24.0, 5.0),
            ambient_sample(3.7, 24.0, 5.0)
        );
    }

    #[test]
    fn ambient_sample_moves_between_distinct_times() {
        let a = ambient_sample(0.0, 24.0, 5.0);
        let b = ambient_sample(1.0, 24.0, 5.0);
        assert_ne!(a, b);
    }

    #[test]
    fn ambient_sample_is_zero_amplitude_at_zero_amplitude() {
        assert_eq!(ambient_sample(4.2, 0.0, 5.0), (0.0, 0.0));
    }

    #[test]
    fn body_kick_is_a_velocity_change_not_a_position_jump() {
        let mut body = FloatBody::at_rest();
        body.kick((10.0, 0.0), 1.0);
        assert_eq!(body.offset, (0.0, 0.0), "a kick alone never moves the body");
        assert_eq!(body.velocity, (10.0, 0.0));
    }

    #[test]
    fn step_body_settles_toward_zero_target_with_no_forcing() {
        let mut body = FloatBody::at_rest();
        body.kick((100.0, 0.0), 1.0);
        for _ in 0..2000 {
            step_body(&mut body, (0.0, 0.0), 40.0, 8.0, 24.0, 1.0 / 120.0);
        }
        assert!(
            body.finished(),
            "a kicked body settles back to rest over time"
        );
    }

    #[test]
    fn settled_bodies_only_run_under_continuous_wave_forcing() {
        let settled = FloatBody::at_rest();
        assert!(!bodies_need_simulation(false, [&settled]));
        assert!(bodies_need_simulation(true, [&settled]));

        let mut moving = settled;
        moving.kick((4.0, 0.0), 1.0);
        assert!(bodies_need_simulation(false, [&settled, &moving]));
    }

    #[test]
    fn step_body_never_settles_while_chasing_a_moving_target() {
        let mut body = FloatBody::at_rest();
        for i in 0..600 {
            // A target that keeps moving (a stand-in for live wave forcing)
            // never lets the spring catch up and go quiet.
            let target = ((i as f64 * 0.05).sin() * 20.0, 0.0);
            step_body(&mut body, target, 40.0, 8.0, 24.0, 1.0 / 120.0);
        }
        assert!(
            body.velocity.0.abs() > BODY_SETTLE_EPSILON
                || body.offset.0.abs() > BODY_SETTLE_EPSILON,
            "continuous forcing should keep the body from reading as settled"
        );
    }

    #[test]
    fn step_body_offset_stays_hard_clamped_to_max_offset() {
        let mut body = FloatBody::at_rest();
        body.kick((500.0, 500.0), 1.0);
        for _ in 0..600 {
            step_body(&mut body, (0.0, 0.0), 40.0, 8.0, 24.0, 1.0 / 120.0);
            let mag = body.offset.0.hypot(body.offset.1);
            assert!(
                mag <= 24.0 + 1e-6,
                "offset {mag} exceeded max_offset mid-simulation"
            );
        }
    }

    #[test]
    fn wave_target_is_a_pure_function_of_position_and_time() {
        assert_eq!(
            wave_target(120.0, 3.0, 10.0, 400.0, 60.0),
            wave_target(120.0, 3.0, 10.0, 400.0, 60.0)
        );
    }

    #[test]
    fn wave_target_differs_by_world_position_at_the_same_instant() {
        let a = wave_target(0.0, 5.0, 10.0, 400.0, 60.0);
        let b = wave_target(600.0, 5.0, 10.0, 400.0, 60.0);
        assert_ne!(a, b, "windows apart in x should be out of phase");
    }

    #[test]
    fn resting_overlap_exchanges_no_collision_impulse() {
        // Two boxes overlapping heavily, both at rest: a naive
        // AABB-separation solver would explode them apart, but this must
        // exchange nothing since neither is approaching the other.
        let mut a_vel = (0.0, 0.0);
        let mut b_vel = (0.0, 0.0);
        let fired = resolve_collision(
            (0.0, 0.0, 100.0, 100.0),
            (20.0, 20.0, 100.0, 100.0),
            &mut a_vel,
            &mut b_vel,
            1.0,
            1.0,
            0.5,
        );
        assert!(!fired);
        assert_eq!(a_vel, (0.0, 0.0));
        assert_eq!(b_vel, (0.0, 0.0));
    }

    #[test]
    fn approaching_overlap_exchanges_a_collision_impulse() {
        let mut a_vel = (50.0, 0.0);
        let mut b_vel = (0.0, 0.0);
        let fired = resolve_collision(
            (0.0, 0.0, 100.0, 100.0),
            (90.0, 0.0, 100.0, 100.0),
            &mut a_vel,
            &mut b_vel,
            1.0,
            1.0,
            0.5,
        );
        assert!(fired);
        // Equal masses, a approaching b along +x: a slows, b picks up speed.
        assert!(a_vel.0 < 50.0);
        assert!(b_vel.0 > 0.0);
    }

    #[test]
    fn separating_overlap_exchanges_no_collision_impulse() {
        let mut a_vel = (-50.0, 0.0);
        let mut b_vel = (0.0, 0.0);
        let fired = resolve_collision(
            (0.0, 0.0, 100.0, 100.0),
            (90.0, 0.0, 100.0, 100.0),
            &mut a_vel,
            &mut b_vel,
            1.0,
            1.0,
            0.5,
        );
        assert!(!fired);
        assert_eq!(a_vel, (-50.0, 0.0));
        assert_eq!(b_vel, (0.0, 0.0));
    }

    #[test]
    fn edge_collision_bounces_only_when_moving_further_out() {
        let mut vel = (-10.0, 0.0);
        let output = (0.0, 0.0, 1000.0, 800.0);
        // Rect already past the left edge, moving further left: bounces.
        resolve_edge_collision((-5.0, 100.0, 50.0, 50.0), &mut vel, output, 0.5);
        assert_eq!(vel.0, 5.0);

        // Same overlap, but moving back inward already: left untouched.
        let mut vel = (10.0, 0.0);
        resolve_edge_collision((-5.0, 100.0, 50.0, 50.0), &mut vel, output, 0.5);
        assert_eq!(vel.0, 10.0);
    }
}
