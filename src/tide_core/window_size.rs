//! Shared floating-window limits in logical coordinates.
//!
//! Client hints and live rules intersect. Zero client maxima mean unbounded;
//! conflicting limits favor the minimum, as in the existing resize path and
//! niri's floating size policy. No output dimensions enter this policy.

use smithay::utils::{Logical, Size};

use crate::config::WindowRule;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FloatingSizeConstraints {
    min: Size<i32, Logical>,
    max: Size<i32, Logical>,
}

impl FloatingSizeConstraints {
    pub(crate) fn from_hints(
        min: Size<i32, Logical>,
        max: Size<i32, Logical>,
        rule: &WindowRule,
    ) -> Self {
        let axis = |min: i32, max: i32, rule_min: Option<i32>, rule_max: Option<i32>| {
            let min = min.max(rule_min.unwrap_or(0)).max(1);
            let max = if max > 0 { max } else { i32::MAX };
            let max = max.min(rule_max.filter(|max| *max > 0).unwrap_or(i32::MAX));
            (min, max.max(min))
        };
        let width = axis(min.w, max.w, rule.min_width, rule.max_width);
        let height = axis(min.h, max.h, rule.min_height, rule.max_height);
        Self {
            min: (width.0, height.0).into(),
            max: (width.1, height.1).into(),
        }
    }

    pub(crate) fn clamp(self, size: Size<i32, Logical>) -> Size<i32, Logical> {
        self.clamp_dimensions(size.w, size.h)
    }

    /// Clamp raw drag deltas before constructing a Size, whose constructor
    /// rejects negative dimensions in debug builds.
    pub(crate) fn clamp_dimensions(self, width: i32, height: i32) -> Size<i32, Logical> {
        (
            width.clamp(self.min.w, self.max.w),
            height.clamp(self.min.h, self.max.h),
        )
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_explicit_and_remembered_sizes_share_intersected_limits() {
        let rule = WindowRule {
            min_width: Some(400),
            max_width: Some(900),
            min_height: Some(200),
            max_height: Some(800),
            ..Default::default()
        };
        let bounds =
            FloatingSizeConstraints::from_hints((300, 250).into(), (1000, 600).into(), &rule);
        assert_eq!(bounds.clamp((200, 100).into()), (400, 250).into());
        assert_eq!(bounds.clamp((1200, 1000).into()), (900, 600).into());
        assert_eq!(bounds.clamp((700, 400).into()), (700, 400).into());
    }

    #[test]
    fn loose_rules_do_not_relax_client_limits() {
        let rule = WindowRule {
            min_width: Some(10),
            max_width: Some(2000),
            min_height: Some(10),
            max_height: Some(2000),
            ..Default::default()
        };
        let bounds =
            FloatingSizeConstraints::from_hints((300, 200).into(), (800, 600).into(), &rule);
        assert_eq!(bounds.clamp((1, 1).into()), (300, 200).into());
        assert_eq!(bounds.clamp((1000, 1000).into()), (800, 600).into());
    }

    #[test]
    fn unbounded_hints_preserve_sizes_without_output_assumptions() {
        let bounds = FloatingSizeConstraints::from_hints(
            (0, 0).into(),
            (0, 0).into(),
            &WindowRule::default(),
        );
        assert_eq!(bounds.clamp((37, 23).into()), (37, 23).into());
        assert_eq!(
            bounds.clamp((i32::MAX, i32::MAX).into()),
            (i32::MAX, i32::MAX).into()
        );
        assert_eq!(bounds.clamp_dimensions(0, -5), (1, 1).into());
    }

    #[test]
    fn conflicting_bounds_favor_minimum_without_panicking() {
        let rule = WindowRule {
            min_width: Some(400),
            max_width: Some(100),
            max_height: Some(50),
            ..Default::default()
        };
        let bounds =
            FloatingSizeConstraints::from_hints((300, 250).into(), (200, 600).into(), &rule);
        assert_eq!(bounds.clamp((1000, 1000).into()), (400, 250).into());
        assert_eq!(bounds.clamp_dimensions(-10, -10), (400, 250).into());
    }

    #[test]
    fn removing_rule_limits_does_not_restore_an_old_size() {
        let bounded = FloatingSizeConstraints::from_hints(
            (0, 0).into(),
            (0, 0).into(),
            &WindowRule {
                max_width: Some(500),
                ..Default::default()
            },
        );
        let current = bounded.clamp((900, 400).into());
        let unbounded = FloatingSizeConstraints::from_hints(
            (0, 0).into(),
            (0, 0).into(),
            &WindowRule::default(),
        );
        assert_eq!(unbounded.clamp(current), current);
    }
}
