//! Cosmetic 2D bob-and-drift for floating windows (spatial roadmap F1,
//! `light` tier). The generalization of `sway.rs`: instead of one lateral
//! axis kicked by a drag, two axes are kicked by any disturbance -- a
//! floating drag, a window mapping, or a workspace-transition wave passing
//! across the output -- and decay back to rest on their own.
//!
//! Like `sway.rs`/`viscosity.rs`/`ripple.rs`, there is no per-frame
//! integrator, no motion history, and no render allocation. The offset is a
//! closed-form function of the time elapsed since the last kick, so a
//! settled window stops asking for frames entirely and an idle desktop
//! still ticks zero frames. A kick is just a sample-then-reseed of the two
//! amplitudes.
//!
//! The lateral axis uses cosine (exact continuity on re-kick, the sway
//! precedent); the vertical axis uses sine, a fixed quarter-period offset,
//! so a window energized by a disturbance reads as bobbing in place rather
//! than sliding diagonally. Because the vertical term is zero at the kick
//! instant, an actively dragged window stays put vertically while the
//! pointer has authority, then bobs once the drag releases and the
//! accumulated vertical amplitude has room to oscillate. The exact phase
//! relationship and every default below are feel parameters, open to the
//! user's nested tuning pass; the shape here is the deliberate starting
//! point.

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
        self.amp_x
            .hypot(self.amp_y)
            * (-self.damping * self.start.elapsed().as_secs_f64()).exp()
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
    let x = amplitude
        * (0.6 * (omega * elapsed).sin() + 0.4 * (omega * 1.9 * elapsed + 1.3).sin());
    let y = amplitude
        * (0.5 * (omega * 1.3 * elapsed + 0.6).sin() + 0.3 * (omega * 2.7 * elapsed + 2.1).sin());
    (x, y)
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
        assert_eq!(ambient_sample(3.7, 24.0, 5.0), ambient_sample(3.7, 24.0, 5.0));
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
}
