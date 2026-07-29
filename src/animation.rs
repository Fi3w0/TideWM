//! Shared animation primitive -- the foundation piece of the render/visual
//! identity roadmap (AGENT.md's "Render and visual identity roadmap", Phase
//! R0). Before this, nothing in the codebase interpolated a value over time;
//! every state change (opacity, toast fade, resize) was either instant or,
//! in `toast.rs`'s case, hand-rolled elapsed-time arithmetic. This is that
//! arithmetic pulled out into something any future effect (ripple decay,
//! workspace-transition progress, resize damping) can reuse instead of
//! re-deriving it, and `toast.rs`'s fade is the first thing built on it --
//! see its own use for the shape of a delayed-start animation (one that
//! begins at a past instant, not "now").

use std::time::{Duration, Instant};

use crate::config::WindowAnimationCurve;

/// A linear interpolation from `from` to `to` over `duration`, anchored at
/// `start` -- which does not have to be "now": a delayed-start animation
/// (e.g. a fade that only begins once an earlier hold period elapses) just
/// anchors `start` in the past or future relative to construction time.
pub struct Animation {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
}

impl Animation {
    pub fn new(from: f32, to: f32, start: Instant, duration: Duration) -> Self {
        Self {
            from,
            to,
            start,
            duration,
        }
    }

    /// Current interpolated value. Clamped to `from`/`to` outside
    /// `[start, start + duration]` -- a caller never needs to guard against
    /// overshoot before or after the animation's own window.
    pub fn value(&self) -> f32 {
        let t = if self.duration.is_zero() {
            1.0
        } else {
            let elapsed = self.start.elapsed().as_secs_f32();
            (elapsed / self.duration.as_secs_f32()).clamp(0.0, 1.0)
        };
        self.from + (self.to - self.from) * t
    }

    /// Whether `start + duration` has passed. `toast.rs` uses this to know
    /// when to stop rendering a one-shot fade and drop it; a future effect
    /// that needs to keep the compositor forcing redraws while it's still
    /// running should check this (or a wrapper around it) the same way
    /// `Toast::needs_continued_redraw` does today for `has_active_animation`.
    pub fn finished(&self) -> bool {
        self.start.elapsed() >= self.duration
    }
}

/// Converts linear elapsed progress into a configured window-animation
/// progress. Cubic Bézier uses CSS semantics: solve the x curve for elapsed
/// time, then return y at the same parameter. Twelve bisection steps are
/// deterministic and comfortably below a physical pixel of error for the
/// short compositor animations that consume it.
pub fn ease_progress(curve: WindowAnimationCurve, progress: f32) -> f32 {
    let progress = progress.clamp(0.0, 1.0);
    match curve {
        WindowAnimationCurve::Linear => progress,
        WindowAnimationCurve::QuadOut => 1.0 - (1.0 - progress).powi(2),
        WindowAnimationCurve::CubicOut => 1.0 - (1.0 - progress).powi(3),
        WindowAnimationCurve::CubicInOut => {
            if progress < 0.5 {
                4.0 * progress.powi(3)
            } else {
                1.0 - (-2.0 * progress + 2.0).powi(3) / 2.0
            }
        }
        WindowAnimationCurve::ExpOut => {
            if progress >= 1.0 {
                1.0
            } else {
                1.0 - 2.0_f32.powf(-10.0 * progress)
            }
        }
        WindowAnimationCurve::CubicBezier(points) => {
            fn coordinate(t: f32, first: f32, second: f32) -> f32 {
                let inverse = 1.0 - t;
                3.0 * inverse * inverse * t * first + 3.0 * inverse * t * t * second + t * t * t
            }

            let mut low = 0.0;
            let mut high = 1.0;
            for _ in 0..12 {
                let middle = (low + high) * 0.5;
                if coordinate(middle, points[0], points[2]) < progress {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            coordinate((low + high) * 0.5, points[1], points[3])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_now_begins_at_from_and_is_not_finished() {
        let anim = Animation::new(0.0, 1.0, Instant::now(), Duration::from_millis(200));
        assert!((anim.value() - 0.0).abs() < 0.1);
        assert!(!anim.finished());
    }

    #[test]
    fn reaches_to_and_finishes_after_duration_elapses() {
        let anim = Animation::new(0.0, 1.0, Instant::now(), Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(anim.value(), 1.0);
        assert!(anim.finished());
    }

    #[test]
    fn zero_duration_is_immediately_finished_at_to() {
        let anim = Animation::new(2.0, 5.0, Instant::now(), Duration::ZERO);
        assert_eq!(anim.value(), 5.0);
        assert!(anim.finished());
    }

    #[test]
    fn delayed_start_holds_at_from_until_its_own_window_begins() {
        // A start instant in the future (relative to "now") models a fade
        // that hasn't begun yet -- exactly toast.rs's hold-then-fade shape.
        let future_start = Instant::now() + Duration::from_millis(50);
        let anim = Animation::new(1.0, 0.0, future_start, Duration::from_millis(10));
        assert_eq!(anim.value(), 1.0);
        assert!(!anim.finished());
    }

    #[test]
    fn css_bezier_preserves_endpoints_and_is_monotonic() {
        let curve = WindowAnimationCurve::CubicBezier([0.16, 1.0, 0.3, 1.0]);
        assert!(ease_progress(curve, 0.0).abs() < 0.001);
        assert!((ease_progress(curve, 1.0) - 1.0).abs() < 0.001);
        let mut previous = 0.0;
        for step in 1..=100 {
            let value = ease_progress(curve, step as f32 / 100.0);
            assert!(value >= previous);
            previous = value;
        }
    }
}
