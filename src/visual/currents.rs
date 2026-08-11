//! Slow, bounded downstream drift for unfocused Ocean floating windows.
//!
//! A current is deliberately cosmetic: it advances a small phase clock and
//! turns that phase into a render offset. The authoritative Ocean rectangle,
//! focus target, pointer hit testing, and drag geometry never move. Holding
//! the clock while a window is focused or dragged avoids both background
//! motion under direct manipulation and a phase jump when the current resumes.

use std::time::Instant;

/// Per-window phase state. One record is retained only while an Ocean
/// floating window is visible and currents are enabled.
#[derive(Debug, Clone)]
pub struct CurrentDrift {
    elapsed: f64,
    last_tick: Instant,
    phase_offset: f64,
    paused: bool,
    weight: f64,
}

impl CurrentDrift {
    pub fn new(now: Instant, phase_offset: f64, paused: bool) -> Self {
        Self {
            elapsed: 0.0,
            last_tick: now,
            phase_offset: phase_offset.rem_euclid(std::f64::consts::TAU),
            paused,
            weight: 0.0,
        }
    }

    /// Advances only while the current is allowed to act. The wall-clock
    /// timestamp advances in both states, so resuming never catches up a
    /// focused/dragged interval in one frame.
    pub fn tick(&mut self, now: Instant, paused: bool) {
        let dt = now
            .saturating_duration_since(self.last_tick)
            .as_secs_f64()
            .min(0.1);
        self.last_tick = now;
        self.paused = paused;
        if !paused {
            self.elapsed += dt;
        }
        // Direct manipulation should converge back onto the authoritative
        // input rectangle instead of snapping there. Resume fades the same
        // way so a newly-unfocused window enters the flow gently.
        let target = if paused { 0.0 } else { 1.0 };
        let blend = 1.0 - (-14.0 * dt).exp();
        self.weight += (target - self.weight) * blend;
        if (self.weight - target).abs() < 0.001 {
            self.weight = target;
        }
    }

    pub fn needs_frame(&self) -> bool {
        !self.paused || self.weight > 0.0
    }

    /// Returns a screen-space logical-pixel offset. The two harmonics form a
    /// smooth closed eddy with a stronger along-current component and a small
    /// cross-current roll. A closed path is important: an unbounded linear
    /// drift would eventually separate rendered content arbitrarily far from
    /// its immutable input rectangle, while wrapping a sawtooth would jump.
    pub fn sample(&self, direction_degrees: f64, strength: f64, period_seconds: f64) -> (f64, f64) {
        let (x, y) = current_offset(
            self.elapsed,
            self.phase_offset,
            direction_degrees,
            strength,
            period_seconds,
        );
        (x * self.weight, y * self.weight)
    }
}

pub fn current_offset(
    elapsed: f64,
    phase_offset: f64,
    direction_degrees: f64,
    strength: f64,
    period_seconds: f64,
) -> (f64, f64) {
    let strength = strength.max(0.0);
    let period_seconds = period_seconds.max(0.001);
    let phase = std::f64::consts::TAU * elapsed / period_seconds + phase_offset;

    // A broad downstream roll plus a smaller higher-frequency eddy keeps
    // separate windows from reading as synchronized desktop bobbing.
    let along = strength * (0.72 * phase.sin() + 0.16 * (phase * 2.0 + 0.7).sin());
    let across = strength * 0.24 * (phase * 2.0 + 1.3).sin();
    let angle = direction_degrees.to_radians();
    let (sin, cos) = angle.sin_cos();
    (along * cos - across * sin, along * sin + across * cos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn current_is_bounded_and_direction_rotates() {
        let right = current_offset(1.7, 0.4, 0.0, 12.0, 10.0);
        let down = current_offset(1.7, 0.4, 90.0, 12.0, 10.0);
        assert!((down.0 + right.1).abs() < 1e-9);
        assert!((down.1 - right.0).abs() < 1e-9);
        assert!(right.0.hypot(right.1) <= 12.0);
    }

    #[test]
    fn pause_does_not_accumulate_hidden_time() {
        let start = Instant::now();
        let mut drift = CurrentDrift::new(start, 0.0, false);
        drift.tick(start + Duration::from_millis(50), false);
        let before = drift.sample(0.0, 10.0, 8.0);
        drift.tick(start + Duration::from_millis(100), true);
        let settling = drift.sample(0.0, 10.0, 8.0);
        assert_ne!(before, settling);
        for step in 2..=20 {
            drift.tick(start + Duration::from_millis(step * 50), true);
        }
        let paused = drift.sample(0.0, 10.0, 8.0);
        assert!(paused.0.hypot(paused.1) < 0.001);
        drift.tick(start + Duration::from_millis(1050), false);
        let resumed = drift.sample(0.0, 10.0, 8.0);
        assert_ne!(paused, resumed);
    }
}
