//! Render-only damping for interactive window move and resize from Phase R2.
//!
//! Pointer grabs keep logical geometry and hit-testing immediate. This
//! state retains only the short-lived visual rectangle that follows that
//! target with exponential decay. Re-targeting samples the current curve
//! first, so a high-rate pointer stream stays continuous without storing
//! motion history or allocating render resources.

use std::time::{Duration, Instant};

use smithay::utils::{Logical, Point, Rectangle, Size};

const SETTLE_EPSILON: f64 = 0.25;

#[derive(Debug)]
pub struct ViscousMotion {
    start: Instant,
    half_life: Duration,
    from: Rectangle<f64, Logical>,
    target: Rectangle<f64, Logical>,
}

impl ViscousMotion {
    pub fn new(
        from: Rectangle<f64, Logical>,
        target: Rectangle<f64, Logical>,
        half_life: Duration,
    ) -> Self {
        Self {
            start: Instant::now(),
            half_life,
            from,
            target,
        }
    }

    /// Re-targets from the rectangle currently on screen. The previous
    /// logical target never leaks into the new curve, which prevents a
    /// snap when pointer events arrive faster than frames are presented.
    pub fn retarget(&mut self, target: Rectangle<f64, Logical>, half_life: Duration) {
        let now = Instant::now();
        self.from = self.sample_at(now);
        self.target = target;
        self.start = now;
        self.half_life = half_life;
    }

    pub fn sample(&self) -> Rectangle<f64, Logical> {
        self.sample_at(Instant::now())
    }

    fn sample_at(&self, now: Instant) -> Rectangle<f64, Logical> {
        if self.half_life.is_zero() {
            return self.target;
        }
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        let remaining = 2.0_f64.powf(-elapsed / self.half_life.as_secs_f64());
        Rectangle::new(
            Point::from((
                self.target.loc.x + (self.from.loc.x - self.target.loc.x) * remaining,
                self.target.loc.y + (self.from.loc.y - self.target.loc.y) * remaining,
            )),
            Size::from((
                (self.target.size.w + (self.from.size.w - self.target.size.w) * remaining).max(1.0),
                (self.target.size.h + (self.from.size.h - self.target.size.h) * remaining).max(1.0),
            )),
        )
    }

    pub fn finished(&self) -> bool {
        let sample = self.sample();
        rect_distance(sample, self.target) <= SETTLE_EPSILON
    }
}

fn rect_distance(a: Rectangle<f64, Logical>, b: Rectangle<f64, Logical>) -> f64 {
    (a.loc.x - b.loc.x)
        .abs()
        .max((a.loc.y - b.loc.y).abs())
        .max((a.size.w - b.size.w).abs())
        .max((a.size.h - b.size.h).abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn one_half_life_halves_every_geometry_delta() {
        let mut motion = ViscousMotion::new(
            rect(0.0, 20.0, 100.0, 300.0),
            rect(200.0, 100.0, 300.0, 100.0),
            Duration::from_millis(30),
        );
        motion.start = Instant::now() - motion.half_life;
        let sample = motion.sample();
        assert!((sample.loc.x - 100.0).abs() < 1.0);
        assert!((sample.loc.y - 60.0).abs() < 1.0);
        assert!((sample.size.w - 200.0).abs() < 1.0);
        assert!((sample.size.h - 200.0).abs() < 1.0);
    }

    #[test]
    fn retarget_starts_from_the_current_visual_rectangle() {
        let mut motion = ViscousMotion::new(
            rect(0.0, 0.0, 100.0, 100.0),
            rect(200.0, 0.0, 100.0, 100.0),
            Duration::from_millis(30),
        );
        motion.start = Instant::now() - motion.half_life;
        motion.retarget(rect(300.0, 0.0, 100.0, 100.0), Duration::from_millis(30));
        let sample = motion.sample();
        assert!((sample.loc.x - 100.0).abs() < 2.0);
    }

    #[test]
    fn zero_viscosity_reaches_the_target_immediately() {
        let motion = ViscousMotion::new(
            rect(0.0, 0.0, 100.0, 100.0),
            rect(200.0, 50.0, 300.0, 400.0),
            Duration::ZERO,
        );
        assert_eq!(motion.sample(), rect(200.0, 50.0, 300.0, 400.0));
        assert!(motion.finished());
    }
}
