//! Super+Right-drag on a *tiled* window's own body, the tiled counterpart
//! to `resize_grab.rs`'s floating resize and matching Hyprland's own
//! `bindm ... resizewindow` convention -- unlike `tile_resize_grab.rs`
//! (dragging the shared border directly, no modifier needed), this doesn't
//! require hitting the border pixel-exactly. Drives up to two split
//! ratios at once (`Layouts::resize_splits` finds the nearest enclosing
//! split per axis), so a diagonal drag can resize both dimensions in one
//! gesture when the window has ancestors on both axes.

use crate::{layout::Axis, layout::SplitHit, Smallvil};
use smithay::{
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
};

pub struct TileWindowResizeGrab {
    start_data: PointerGrabStartData<Smallvil>,
    /// Each split to drive, paired with its ratio at grab start (see
    /// `TileResizeGrab`'s own `start_ratio`/`area_size` for why: recomputing
    /// either from the live, already-changing ratio would make the math
    /// chase its own tail).
    handles: Vec<(SplitHit, f32)>,
}

impl TileWindowResizeGrab {
    pub fn start(
        start_data: PointerGrabStartData<Smallvil>,
        handles: Vec<(SplitHit, f32)>,
    ) -> Self {
        Self {
            start_data,
            handles,
        }
    }
}

impl PointerGrab<Smallvil> for TileWindowResizeGrab {
    fn motion(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);

        if self
            .handles
            .iter()
            .any(|(hit, _)| !data.layout.split_is_current(hit))
        {
            return;
        }

        let delta = event.location - self.start_data.location;
        for (hit, start_ratio) in &self.handles {
            let span = match hit.axis {
                Axis::Horizontal => hit.area.size.w,
                Axis::Vertical => hit.area.size.h,
            };
            if span <= 0 {
                continue;
            }
            let delta_ratio = match hit.axis {
                Axis::Horizontal => delta.x / span as f64,
                Axis::Vertical => delta.y / span as f64,
            };
            let new_ratio = *start_ratio as f64 + delta_ratio;
            data.layout
                .set_ratio(&hit.output, hit.workspace, &hit.path, new_ratio as f32);
        }
        data.retile();
    }

    fn relative_motion(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if !handle.current_pressed().contains(&self.start_data.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(&mut self, data: &mut Smallvil, handle: &mut PointerInnerHandle<'_, Smallvil>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event)
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event)
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event)
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event)
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event)
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event)
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event)
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event)
    }

    fn start_data(&self) -> &PointerGrabStartData<Smallvil> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut Smallvil) {}
}
