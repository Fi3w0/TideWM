//! Optional lateral sway for dragged floating windows (Phase R2).
//!
//! Pointer grabs keep logical geometry and hit-testing immediate. Each
//! horizontal drag delta kicks a closed-form damped oscillation that
//! offsets only what is drawn, decaying back to rest on its own. Like
//! `viscosity.rs`, there is no per-frame integrator, no motion history,
//! and no render allocation: the offset is a pure function of the time
//! elapsed since the last kick, so a settled window stops asking for
//! frames entirely.

use std::time::Instant;

/// Lateral offset below which the sway is treated as settled, logical
/// pixels. Matches `viscosity.rs`'s settle threshold.
const SETTLE_EPSILON: f64 = 0.25;

#[derive(Debug)]
pub struct FloatingSway {
    start: Instant,
    /// Signed lateral offset at kick time, logical pixels. The cosine
    /// phase is fixed so the swing always starts exactly here.
    amplitude: f64,
    /// Angular frequency, radians per second.
    omega: f64,
    /// Exponential decay rate, per second. Higher settles faster.
    damping: f64,
}

impl FloatingSway {
    pub fn kicked(amplitude: f64, frequency_hz: f64, damping: f64) -> Self {
        Self {
            start: Instant::now(),
            amplitude,
            omega: 2.0 * std::f64::consts::PI * frequency_hz.max(0.0),
            damping: damping.max(0.0),
        }
    }

    /// Absorbs one horizontal drag delta. Sampling first keeps an ongoing
    /// oscillation continuous: the new swing starts where the old one
    /// currently is, plus the configured fraction of the pointer step,
    /// capped to the configured reach.
    pub fn kick(&mut self, delta_x: f64, response: f64, max_offset: f64) {
        self.amplitude = (self.sample() + delta_x * response).clamp(-max_offset, max_offset);
        self.start = Instant::now();
    }

    pub fn sample(&self) -> f64 {
        self.sample_at(Instant::now())
    }

    fn sample_at(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        self.amplitude * (-self.damping * elapsed).exp() * (self.omega * elapsed).cos()
    }

    /// The decay envelope, not the instantaneous offset, decides rest:
    /// a swing passing through zero mid-oscillation is not finished.
    pub fn finished(&self) -> bool {
        self.amplitude.abs() * (-self.damping * self.start.elapsed().as_secs_f64()).exp()
            <= SETTLE_EPSILON
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sway_starts_at_the_kick_amplitude_and_oscillates_back_through_zero() {
        let mut sway = FloatingSway::kicked(10.0, 1.0, 0.5);
        assert!((sway.sample() - 10.0).abs() < 0.5);
        // Quarter period at 1 Hz: the cosine crosses zero.
        sway.start = Instant::now() - Duration::from_millis(250);
        assert!(sway.sample().abs() < 0.5);
        // Half period: one full swing to the other side, decayed.
        sway.start = Instant::now() - Duration::from_millis(500);
        let sample = sway.sample();
        assert!(sample < -7.0 && sample > -10.0);
    }

    #[test]
    fn decay_eventually_settles() {
        let mut sway = FloatingSway::kicked(24.0, 1.6, 3.0);
        assert!(!sway.finished());
        sway.start = Instant::now() - Duration::from_secs(3);
        assert!(sway.finished());
        assert!(sway.sample().abs() < SETTLE_EPSILON);
    }

    #[test]
    fn kick_continues_from_the_current_offset_and_respects_the_cap() {
        let mut sway = FloatingSway::kicked(5.0, 1.6, 3.0);
        sway.kick(100.0, 0.5, 24.0);
        assert!((sway.sample() - 24.0).abs() < 0.5);
        sway.kick(-4.0, 0.5, 24.0);
        assert!((sway.sample() - 22.0).abs() < 0.5);
    }

    #[test]
    fn zero_amplitude_is_immediately_finished() {
        let sway = FloatingSway::kicked(0.0, 1.6, 3.0);
        assert!(sway.finished());
        assert_eq!(sway.sample(), 0.0);
    }
}
