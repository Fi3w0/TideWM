//! Cascade-only liquid lifecycle animation.
//!
//! The state is deliberately tiny: one clock per opening tiled window and
//! one clock carried by each already-bounded closing snapshot.  Rendering
//! reuses the existing surface textures; no framebuffer or copied texture is
//! introduced here.

use std::time::{Duration, Instant};

use crate::config::{CascadeLiquidConfig, CascadeLiquidPreset, RippleEase};

/// Shared procedural crest/noise used by both the full-output workspace
/// transition and Cascade's per-window pour/drain.  Keeping this in one GLSL
/// fragment prevents the two water fronts from quietly developing unrelated
/// shapes while still letting each caller choose its own coordinate axis.
pub const LIQUID_FRONT_GLSL: &str = r#"
float tide_hash21(vec2 value) {
    value = fract(value * vec2(123.34, 456.21));
    value += dot(value, value + 45.32);
    return fract(value.x * value.y);
}

float tide_liquid_crest(
    float coordinate,
    float phase,
    float amplitude,
    float frequency,
    float lobe_size,
    float turbulence
) {
    float angle = coordinate * 6.2831853 * frequency - phase * 4.712389;
    float crest = sin(angle) * amplitude;
    crest += sin(angle * 2.17 + 1.3) * amplitude * 0.32 * turbulence;
    crest += pow(max(0.0, sin(angle * 0.51 - 0.8)), 3.0)
        * lobe_size * turbulence;
    return crest;
}
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CascadeDirection {
    Pour,
    Drain,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CascadeSample {
    pub progress: f32,
    pub direction: CascadeDirection,
    pub preset: CascadeLiquidPreset,
}

impl CascadeSample {
    pub fn direction_uniform(self) -> f32 {
        match self.direction {
            CascadeDirection::Pour => 1.0,
            CascadeDirection::Drain => -1.0,
        }
    }

    pub fn preset_uniform(self) -> f32 {
        match self.preset {
            CascadeLiquidPreset::None => 0.0,
            CascadeLiquidPreset::Wave => 1.0,
            CascadeLiquidPreset::Trickle => 2.0,
            CascadeLiquidPreset::Splash => 3.0,
        }
    }
}

pub struct CascadeTransition {
    start: Instant,
    duration: Duration,
    curve: RippleEase,
    direction: CascadeDirection,
    preset: CascadeLiquidPreset,
}

impl CascadeTransition {
    pub fn pour(config: &CascadeLiquidConfig) -> Option<Self> {
        Self::new(
            config.pour,
            config.pour_duration_ms,
            config.curve,
            CascadeDirection::Pour,
        )
    }

    pub fn drain(config: &CascadeLiquidConfig) -> Option<Self> {
        Self::new(
            config.drain,
            config.drain_duration_ms,
            config.curve,
            CascadeDirection::Drain,
        )
    }

    fn new(
        preset: CascadeLiquidPreset,
        duration_ms: u32,
        curve: RippleEase,
        direction: CascadeDirection,
    ) -> Option<Self> {
        (preset != CascadeLiquidPreset::None).then_some(Self {
            start: Instant::now(),
            duration: Duration::from_millis(u64::from(duration_ms)),
            curve,
            direction,
            preset,
        })
    }

    pub fn sample(&self) -> CascadeSample {
        let raw = if self.duration.is_zero() {
            1.0
        } else {
            (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
        };
        CascadeSample {
            progress: crate::ripple::ease_value(self.curve, raw),
            direction: self.direction,
            preset: self.preset,
        }
    }

    pub fn finished(&self) -> bool {
        self.start.elapsed() >= self.duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_disables_each_lifecycle_leg_without_disabling_the_other() {
        let config = CascadeLiquidConfig {
            pour: CascadeLiquidPreset::None,
            drain: CascadeLiquidPreset::Splash,
            ..CascadeLiquidConfig::default()
        };
        assert!(CascadeTransition::pour(&config).is_none());
        assert!(CascadeTransition::drain(&config).is_some());
    }

    #[test]
    fn shared_front_contains_noise_and_multi_harmonic_crest() {
        assert!(LIQUID_FRONT_GLSL.contains("tide_hash21"));
        assert!(LIQUID_FRONT_GLSL.contains("tide_liquid_crest"));
        assert!(LIQUID_FRONT_GLSL.contains("2.17"));
    }
}
