//! Drags the gap between two tiled windows to adjust their split ratio,
//! the tiled counterpart to `resize_grab.rs` (which resizes a single
//! floating window's buffer via xdg-shell configure). This grab doesn't
//! touch any client's buffer size directly -- it mutates the `BspLayout`
//! split ratio addressed by `path` and lets `Smallvil::retile()` push the
//! resulting geometry out, same as any other layout change.

use crate::{layout::Axis, layout::SplitHit, Smallvil};
use smithay::{
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
        GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
        PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
};

pub struct TileResizeGrab {
    start_data: PointerGrabStartData<Smallvil>,
    start_ratio: f32,
    /// Includes the split path, topology revision, and its fixed area at
    /// grab start. Recomputing the latter from the live ratio would make
    /// the ratio math chase its own tail.
    hit: SplitHit,
}

impl TileResizeGrab {
    pub fn start(start_data: PointerGrabStartData<Smallvil>, hit: SplitHit, start_ratio: f32) -> Self {
        Self {
            start_data,
            start_ratio,
            hit,
        }
    }
}

impl PointerGrab<Smallvil> for TileResizeGrab {
    fn motion(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);

        if !data.layout.split_is_current(&self.hit) {
            return;
        }

        let delta = event.location - self.start_data.location;
        let span = match self.hit.axis {
            Axis::Horizontal => self.hit.area.size.w,
            Axis::Vertical => self.hit.area.size.h,
        };
        if span <= 0 {
            return;
        }
        let delta_ratio = match self.hit.axis {
            Axis::Horizontal => delta.x / span as f64,
            Axis::Vertical => delta.y / span as f64,
        };
        let new_ratio = self.start_ratio as f64 + delta_ratio;

        data.layout
            .set_ratio(&self.hit.output, self.hit.workspace, &self.hit.path, new_ratio as f32);
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
