//! Render-only apparent weight for floating windows.
//!
//! The authoritative window rectangle never moves. A weight maps to a small
//! downward screen-space offset, and (in Ocean only) attenuates the cosmetic
//! current/physics response. Focus and direct manipulation ease the apparent
//! weight to zero so input and drawing visibly reunite while the user acts.

use std::time::Instant;

#[derive(Debug, Clone)]
pub struct BuoyancyState {
    configured_weight: f64,
    weight: f64,
    target: f64,
    last_tick: Instant,
}

impl BuoyancyState {
    pub fn new(now: Instant, configured_weight: f64) -> Self {
        Self {
            configured_weight: configured_weight.clamp(0.0, 1.0),
            weight: 0.0,
            target: 0.0,
            last_tick: now,
        }
    }

    pub fn configured_weight(&self) -> f64 {
        self.configured_weight
    }

    pub fn set_configured_weight(&mut self, weight: f64) {
        self.configured_weight = weight.clamp(0.0, 1.0);
    }

    pub fn tick(&mut self, now: Instant, target: f64, settle_seconds: f64) {
        let target = target.clamp(0.0, 1.0);
        let dt = now
            .saturating_duration_since(self.last_tick)
            .as_secs_f64()
            .min(0.1);
        self.last_tick = now;
        self.target = target;

        if settle_seconds <= f64::EPSILON {
            self.weight = target;
            return;
        }

        // Six time constants put the remaining error below 0.25% at the
        // configured settle duration, then the threshold finishes exactly.
        let blend = 1.0 - (-6.0 * dt / settle_seconds).exp();
        self.weight += (target - self.weight) * blend;
        if (self.weight - target).abs() < 0.001 {
            self.weight = target;
        }
    }

    pub fn needs_frame(&self) -> bool {
        (self.weight - self.target).abs() >= 0.001
    }

    pub fn sample(&self, max_sink: f64, flow_reduction: f64) -> (f64, f64) {
        buoyancy_sample(self.weight, max_sink, flow_reduction)
    }
}

pub fn buoyancy_sample(weight: f64, max_sink: f64, flow_reduction: f64) -> (f64, f64) {
    let weight = weight.clamp(0.0, 1.0);
    let sink = weight * max_sink.max(0.0);
    let flow_scale = 1.0 - weight * flow_reduction.clamp(0.0, 1.0);
    (sink, flow_scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sample_is_bounded_and_heavier_means_less_flow() {
        assert_eq!(buoyancy_sample(-1.0, 18.0, 0.65), (0.0, 1.0));
        assert_eq!(buoyancy_sample(1.5, 18.0, 0.65), (18.0, 0.35));
        assert_eq!(buoyancy_sample(0.5, 18.0, 0.65), (9.0, 0.675));
    }

    #[test]
    fn focus_transition_settles_and_stops_requesting_frames() {
        let start = Instant::now();
        let mut state = BuoyancyState::new(start, 1.0);
        state.tick(start, 1.0, 0.24);
        assert!(state.needs_frame());
        for step in 1..=20 {
            state.tick(start + Duration::from_millis(step * 20), 1.0, 0.24);
        }
        assert!(!state.needs_frame());
        assert_eq!(state.sample(18.0, 0.65), (18.0, 0.35));

        state.tick(start + Duration::from_millis(420), 0.0, 0.24);
        assert!(state.needs_frame());
        for step in 22..=42 {
            state.tick(start + Duration::from_millis(step * 20), 0.0, 0.24);
        }
        assert!(!state.needs_frame());
        assert_eq!(state.sample(18.0, 0.65), (0.0, 1.0));
    }

    #[test]
    fn zero_settle_changes_immediately() {
        let now = Instant::now();
        let mut state = BuoyancyState::new(now, 0.8);
        state.tick(now, 0.8, 0.0);
        assert!(!state.needs_frame());
        let (sink, flow) = state.sample(10.0, 1.0);
        assert!((sink - 8.0).abs() < 1e-9);
        assert!((flow - 0.2).abs() < 1e-9);
    }
}
