//! Continuous lateral "swim" camera from spatial roadmap S0.
//!
//! The lateral axis stays a sequence of discrete tiling spots, each an
//! ordinary BSP/master/cascade tree, so logical workspace identity is still
//! the `u32` number owned by `Layouts::active`. This module owns the
//! purely-visual continuous camera offset layered on top of that discrete
//! identity: a horizontal trackpad swipe pans the offset, the anchor
//! advances once it crosses the halfway mark, and a spring eases the
//! residual offset back to rest on release. At rest the offset is zero, so
//! the idle frame is identical to the discrete-switch mode and an idle
//! desktop still ticks zero frames.
//!
//! Like sway/viscosity/ripple, there is no per-frame integrator and no
//! motion history: the spring is a closed-form ease over R0's `Animation`
//! primitive, and a settled camera stops asking for frames entirely.

use std::time::{Duration, Instant};

use crate::animation::Animation;

/// One neighboring workspace strip which intersects the output viewport.
/// `delta` is relative to the current logical anchor: `1` is the workspace
/// to the right, `-1` the workspace to the left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleNeighbor {
    pub workspace: u32,
    pub delta: i32,
}

/// Finds the bounded set of neighboring workspace strips which currently
/// intersect the viewport. A strip at relative `delta` begins at
/// `delta - camera_offset` spot-widths, so it is visible exactly while that
/// interval overlaps `(-1, 1)`. Workspace zero remains the scratchpad edge,
/// matching the input path's refusal to swim down from workspace one.
///
/// This stays pure so selection, edge handling, and the configured bound can
/// be tested without constructing a compositor or renderer.
pub fn visible_neighbors(
    anchor: u32,
    camera_offset: f32,
    max_neighbors: u8,
) -> Vec<VisibleNeighbor> {
    if max_neighbors == 0 || camera_offset.abs() < 0.0001 {
        return Vec::new();
    }

    let limit = i32::from(max_neighbors);
    (-limit..=limit)
        .filter(|delta| *delta != 0)
        .filter(|delta| {
            let strip_offset = *delta as f32 - camera_offset;
            strip_offset > -1.0 && strip_offset < 1.0
        })
        .filter_map(|delta| {
            let workspace = if delta > 0 {
                anchor.checked_add(delta as u32)
            } else {
                anchor.checked_sub(delta.unsigned_abs())
            }?;
            (workspace > 0).then_some(VisibleNeighbor { workspace, delta })
        })
        .collect()
}

/// Per-output swim camera state.
#[derive(Debug)]
pub struct SwimCamera {
    /// Continuous visual offset in spot-widths. Positive means the camera
    /// has travelled toward higher-numbered spots, so content shifts left
    /// and the right neighbor enters. `drag` keeps this inside `(-0.5, 0.5]`
    /// by folding whole-spot crossings into anchor advances.
    offset: f32,
    /// In-flight spring-back to zero after a drag releases. `None` while a
    /// drag is actively driving `offset`, or once the spring has settled.
    spring: Option<Animation>,
}

impl Default for SwimCamera {
    fn default() -> Self {
        Self {
            offset: 0.0,
            spring: None,
        }
    }
}

impl SwimCamera {
    /// The current effective offset: the spring's interpolated value if one
    /// is running, otherwise the live drag value.
    pub fn current_offset(&self) -> f32 {
        match &self.spring {
            Some(spring) => spring.value(),
            None => self.offset,
        }
    }

    /// Advance the drag by `delta` spot-widths. Clears any in-flight spring
    /// (the fingers are back on the trackpad), resuming from wherever the
    /// spring currently sits so interrupting a settling pan stays
    /// continuous. Returns the signed number of anchor advances that fell
    /// out of wrapping the offset back into `(-0.5, 0.5]`.
    pub fn drag(&mut self, delta: f32) -> i32 {
        if let Some(spring) = self.spring.take() {
            self.offset = spring.value();
        }
        self.offset += delta;
        self.absorb_advances()
    }

    /// Release the drag: spring the residual offset back to zero over
    /// `duration`. A spring is only armed when there is something to settle;
    /// releasing at (near) zero is a no-op that asks for no extra frames.
    pub fn release(&mut self, duration: Duration) {
        if self.offset.abs() < 0.001 {
            self.offset = 0.0;
            self.spring = None;
            return;
        }
        let from = self.offset;
        self.offset = 0.0;
        self.spring = Some(Animation::new(from, 0.0, Instant::now(), duration));
    }

    /// Drop any in-flight motion and snap to rest immediately. Used when
    /// swim is disabled mid-pan, an output disappears, or the session locks.
    pub fn snap_to_rest(&mut self) {
        self.offset = 0.0;
        self.spring = None;
    }

    /// Whether the camera is still settling under its release spring. An
    /// active drag (fingers down) does not count: the swipe-update path arms
    /// its own redraws while it runs.
    pub fn settling(&self) -> bool {
        self.spring
            .as_ref()
            .is_some_and(|spring| !spring.finished())
    }

    /// Whether the camera is fully idle: no live drag offset, no in-flight
    /// spring. The one state where this entry carries no information and
    /// can be pruned from the tracking map, same "finished means gone"
    /// convention `window_sway`/`window_viscosity` already use. Checks
    /// `settling()` rather than `spring.is_none()` directly -- a finished
    /// spring stays `Some` until something takes or overwrites it (`drag`,
    /// another `release`), so `is_none()` alone would never observe a
    /// settled camera as prunable.
    pub fn at_rest(&self) -> bool {
        !self.settling() && self.offset == 0.0
    }

    /// Reverses `steps` whole-spot advances the discrete workspace axis
    /// refused to apply (e.g. the scratchpad boundary), folding them back
    /// into the live offset. Without this a drag pressed against an edge
    /// would just wrap silently and pan into nothing; with it, the offset
    /// keeps growing past the wrap point so the pan reads as resistance.
    pub fn cancel_advance(&mut self, steps: i32) {
        self.offset += steps as f32;
    }

    /// Folds `offset` into `(-0.5, 0.5]`, returning how many whole-spot
    /// anchor steps the wrap represents (signed). Called after a drag
    /// mutates `offset`.
    fn absorb_advances(&mut self) -> i32 {
        let advances = self.offset.round() as i32;
        if advances != 0 {
            self.offset -= advances as f32;
        }
        advances
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_half_drag_advances_nothing() {
        let mut cam = SwimCamera::default();
        assert_eq!(cam.drag(0.3), 0);
        assert!((cam.current_offset() - 0.3).abs() < 0.001);
    }

    #[test]
    fn half_crossing_advances_and_wraps_negative() {
        let mut cam = SwimCamera::default();
        cam.drag(0.3);
        let advances = cam.drag(0.3);
        assert_eq!(advances, 1);
        assert!((cam.current_offset() - (-0.4)).abs() < 0.001);
    }

    #[test]
    fn fast_drag_can_advance_multiple_at_once() {
        let mut cam = SwimCamera::default();
        let advances = cam.drag(2.4);
        assert_eq!(advances, 2);
        assert!((cam.current_offset() - 0.4).abs() < 0.001);
    }

    #[test]
    fn negative_drag_advances_backward() {
        let mut cam = SwimCamera::default();
        let advances = cam.drag(-0.6);
        assert_eq!(advances, -1);
        assert!((cam.current_offset() - 0.4).abs() < 0.001);
    }

    #[test]
    fn release_at_near_zero_does_not_arm_a_spring() {
        let mut cam = SwimCamera::default();
        cam.release(Duration::from_millis(200));
        assert!(!cam.settling());
        assert_eq!(cam.current_offset(), 0.0);
    }

    #[test]
    fn release_springs_back_to_zero_over_duration() {
        let mut cam = SwimCamera::default();
        cam.drag(0.4);
        cam.release(Duration::from_millis(10));
        assert!(cam.settling());
        // At the start the spring still reports the released offset.
        assert!((cam.current_offset() - 0.4).abs() < 0.1);
        std::thread::sleep(Duration::from_millis(25));
        assert!(!cam.settling());
        assert_eq!(cam.current_offset(), 0.0);
    }

    #[test]
    fn drag_resumes_from_the_spring_not_zero() {
        let mut cam = SwimCamera::default();
        cam.drag(0.4);
        cam.release(Duration::from_secs(10));
        // Mid-settle, a new drag resumes from the spring's current value.
        let advances = cam.drag(0.05);
        assert_eq!(advances, 0);
        assert!((cam.current_offset() - 0.45).abs() < 0.05);
        assert!(!cam.settling());
    }

    #[test]
    fn snap_to_rest_drops_everything() {
        let mut cam = SwimCamera::default();
        cam.drag(0.4);
        cam.release(Duration::from_secs(10));
        cam.snap_to_rest();
        assert!(!cam.settling());
        assert_eq!(cam.current_offset(), 0.0);
    }

    #[test]
    fn default_camera_is_at_rest() {
        assert!(SwimCamera::default().at_rest());
    }

    #[test]
    fn mid_drag_camera_is_not_at_rest() {
        let mut cam = SwimCamera::default();
        cam.drag(0.3);
        assert!(!cam.at_rest());
    }

    #[test]
    fn settling_spring_is_not_at_rest() {
        let mut cam = SwimCamera::default();
        cam.drag(0.4);
        cam.release(Duration::from_secs(10));
        assert!(!cam.at_rest());
    }

    #[test]
    fn finished_spring_becomes_at_rest_without_a_further_call() {
        // Regression: a finished `Animation` stays `Some` until something
        // takes or overwrites it, so `at_rest()` must not key off
        // `spring.is_none()` directly or a settled camera would never be
        // observed as prunable.
        let mut cam = SwimCamera::default();
        cam.drag(0.4);
        cam.release(Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(20));
        assert!(!cam.settling());
        assert!(cam.at_rest());
    }

    #[test]
    fn cancel_advance_refunds_offset() {
        let mut cam = SwimCamera::default();
        let advances = cam.drag(0.6);
        assert_eq!(advances, 1);
        assert!((cam.current_offset() - (-0.4)).abs() < 0.001);
        // A boundary refused the step: give the whole spot-width back,
        // landing back on the pre-wrap offset.
        cam.cancel_advance(advances);
        assert!((cam.current_offset() - 0.6).abs() < 0.001);
    }

    #[test]
    fn positive_offset_reveals_the_right_neighbor() {
        assert_eq!(
            visible_neighbors(3, 0.3, 1),
            vec![VisibleNeighbor {
                workspace: 4,
                delta: 1,
            }]
        );
    }

    #[test]
    fn negative_offset_reveals_the_left_neighbor() {
        assert_eq!(
            visible_neighbors(3, -0.3, 1),
            vec![VisibleNeighbor {
                workspace: 2,
                delta: -1,
            }]
        );
    }

    #[test]
    fn idle_camera_selects_no_neighbor_work() {
        assert!(visible_neighbors(3, 0.0, 4).is_empty());
    }

    #[test]
    fn configured_neighbor_bound_limits_wide_offsets() {
        assert_eq!(
            visible_neighbors(3, 1.4, 1),
            vec![VisibleNeighbor {
                workspace: 4,
                delta: 1,
            }]
        );
        assert_eq!(
            visible_neighbors(3, 1.4, 2),
            vec![
                VisibleNeighbor {
                    workspace: 4,
                    delta: 1,
                },
                VisibleNeighbor {
                    workspace: 5,
                    delta: 2,
                },
            ]
        );
    }

    #[test]
    fn workspace_edges_never_select_scratchpad_or_overflow() {
        assert!(visible_neighbors(1, -0.4, 4).is_empty());
        assert!(visible_neighbors(u32::MAX, 0.4, 4).is_empty());
    }
}
