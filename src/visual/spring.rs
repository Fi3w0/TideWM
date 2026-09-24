//! Analytic, zero-initial-velocity spring easing in seconds. No frame-step
//! integration or history is retained. Parameters describe m*x''+c*x'+k*x=0
//! for normalized displacement; the position target is one.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringCurve {
    beta: f64,
    omega_squared: f64,
    settle: f64,
}

impl SpringCurve {
    /// Mass is in normalized mass units, stiffness in mass/s² and damping
    /// in mass/s. Require positive finite coefficients and a conservative
    /// position/velocity settling bound within ten seconds before slowdown.
    pub fn new(mass: f64, stiffness: f64, damping: f64) -> Option<Self> {
        Self::try_new(mass, stiffness, damping).ok()
    }

    /// `new`, saying which check a rejected spring failed.
    pub fn try_new(mass: f64, stiffness: f64, damping: f64) -> Result<Self, &'static str> {
        if !mass.is_finite()
            || !stiffness.is_finite()
            || !damping.is_finite()
            || !(0.01..=100.0).contains(&mass)
            || !(0.01..=100_000.0).contains(&stiffness)
            || !(0.01..=10_000.0).contains(&damping)
        {
            return Err(
                "spring needs mass 0.01 to 100, stiffness 0.01 to 100000 and damping 0.01 to 10000",
            );
        }
        let mut spring = Self {
            beta: damping / (2.0 * mass),
            omega_squared: stiffness / mass,
            settle: 0.0,
        };
        if !spring.settled_bound(10.0) {
            return Err(
                "spring does not settle within ten seconds; raise its stiffness or damping",
            );
        }
        let mut low = 0.0;
        let mut high = 10.0;
        for _ in 0..32 {
            let middle = (low + high) * 0.5;
            if spring.settled_bound(middle) {
                high = middle;
            } else {
                low = middle;
            }
        }
        spring.settle = high;
        Ok(spring)
    }

    fn discriminant(self) -> f64 {
        self.beta * self.beta - self.omega_squared
    }

    fn critical(self) -> bool {
        self.discriminant().abs() <= self.omega_squared * 1e-8
    }

    fn roots(self) -> (f64, f64, f64, f64) {
        let fast = -self.beta - self.discriminant().sqrt();
        // Equivalent to -beta+sqrt(beta²-omega²), without cancellation.
        let slow = self.omega_squared / fast;
        let a = fast / (slow - fast);
        let b = -1.0 - a;
        (slow, fast, a, b)
    }

    fn state_at(self, seconds: f64) -> (f64, f64) {
        let decay = (-self.beta * seconds).exp();
        if self.critical() {
            (
                1.0 - decay * (1.0 + self.beta * seconds),
                decay * self.beta * self.beta * seconds,
            )
        } else if self.discriminant() < 0.0 {
            let omega = (-self.discriminant()).sqrt();
            let (sin, cos) = (omega * seconds).sin_cos();
            (
                1.0 - decay * (cos + self.beta / omega * sin),
                decay * self.omega_squared / omega * sin,
            )
        } else {
            let (slow, fast, a, b) = self.roots();
            let slow_term = a * (slow * seconds).exp();
            let fast_term = b * (fast * seconds).exp();
            (
                1.0 + slow_term + fast_term,
                slow * slow_term + fast * fast_term,
            )
        }
    }

    /// Bounds both displacement and normalized velocity for all later times,
    /// so crossing the target once cannot prematurely finish an oscillation.
    fn settled_bound(self, seconds: f64) -> bool {
        let decay = (-self.beta * seconds).exp();
        let (position, velocity) = if self.critical() {
            if seconds < 1.0 / self.beta {
                return false; // velocity envelope has not peaked yet
            }
            (
                decay * (1.0 + self.beta * seconds),
                decay * self.beta * self.beta * seconds,
            )
        } else if self.discriminant() < 0.0 {
            let omega = (-self.discriminant()).sqrt();
            (
                decay * 1.0_f64.hypot(self.beta / omega),
                decay * self.omega_squared / omega,
            )
        } else {
            let (slow, fast, a, b) = self.roots();
            let first = a.abs() * (slow * seconds).exp();
            let second = b.abs() * (fast * seconds).exp();
            (first + second, slow.abs() * first + fast.abs() * second)
        };
        position <= 0.001 && velocity <= 0.001
    }

    pub fn duration_seconds(self) -> f64 {
        self.settle
    }

    pub fn progress(self, progress: f32) -> f32 {
        if progress <= 0.0 {
            0.0
        } else if progress >= 1.0 {
            1.0
        } else {
            self.state_at(f64::from(progress) * self.settle).0 as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_damping_regimes_start_at_rest_and_settle_permanently() {
        for damping in [10.0, 39.9999999, 40.0, 40.0000001, 80.0, 400.0] {
            let spring = SpringCurve::new(1.0, 400.0, damping).unwrap();
            assert_eq!(spring.progress(0.0), 0.0);
            assert_eq!(spring.progress(1.0), 1.0);
            let (position, velocity) = spring.state_at(0.0);
            assert!(position.abs() < 1e-10);
            assert!(velocity.abs() < 1e-10);
            for sample in 0..100 {
                let time = spring.settle + f64::from(sample) * 0.05;
                let (position, velocity) = spring.state_at(time);
                assert!((position - 1.0).abs() <= 0.00101);
                assert!(velocity.abs() <= 0.00101);
            }
        }
    }

    #[test]
    fn underdamping_overshoots_and_critical_damping_does_not() {
        let under = SpringCurve::new(1.0, 400.0, 10.0).unwrap();
        let critical = SpringCurve::new(1.0, 400.0, 40.0).unwrap();
        assert!((1..100).any(|step| under.progress(step as f32 / 100.0) > 1.0));
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = critical.progress(step as f32 / 100.0);
            assert!((previous..=1.0).contains(&value));
            previous = value;
        }
    }

    #[test]
    fn invalid_or_unbounded_springs_are_rejected() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(SpringCurve::new(bad, 400.0, 30.0).is_none());
            assert!(SpringCurve::new(1.0, bad, 30.0).is_none());
            assert!(SpringCurve::new(1.0, 400.0, bad).is_none());
        }
        assert!(SpringCurve::new(100.0, 0.01, 0.01).is_none());
        assert!(SpringCurve::new(1.0, 1.0, 10_000.0).is_none());
    }

    #[test]
    fn coefficient_scaling_preserves_the_same_motion() {
        let first = SpringCurve::new(1.0, 400.0, 30.0).unwrap();
        let second = SpringCurve::new(2.0, 800.0, 60.0).unwrap();
        assert_eq!(first, second);
    }
}
