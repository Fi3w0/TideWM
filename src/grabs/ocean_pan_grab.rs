//! Direct manipulation grab for Ocean's empty world canvas.
//!
//! TideWM keeps pointer coordinates in output/screen space, while Ocean's
//! camera origin is world space. Each screen-pixel drag therefore moves the
//! camera by the inverse delta divided by zoom: the canvas remains attached
//! to the pointer at every scale without moving any window geometry.

use crate::Smallvil;
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

pub struct OceanPanGrab {
    pub start_data: PointerGrabStartData<Smallvil>,
    pub output: String,
    last_location: Point<f64, Logical>,
}

impl OceanPanGrab {
    pub fn start(start_data: PointerGrabStartData<Smallvil>, output: String) -> Self {
        let last_location = start_data.location;
        Self {
            start_data,
            output,
            last_location,
        }
    }
}

impl PointerGrab<Smallvil> for OceanPanGrab {
    fn motion(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let delta = event.location - self.last_location;
        self.last_location = event.location;
        let camera_delta = camera_delta_for_drag(delta, data.ocean.camera(&self.output).zoom);
        data.ocean.pan(&self.output, camera_delta.x, camera_delta.y);
        data.request_redraw();
    }

    fn relative_motion(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
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

fn camera_delta_for_drag(screen_delta: Point<f64, Logical>, zoom: f64) -> Point<f64, Logical> {
    let zoom = zoom.max(0.05);
    Point::from((-screen_delta.x / zoom, -screen_delta.y / zoom))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_stays_attached_to_pointer_at_every_zoom() {
        assert_eq!(
            camera_delta_for_drag(Point::from((40.0, -20.0)), 1.0),
            Point::from((-40.0, 20.0))
        );
        assert_eq!(
            camera_delta_for_drag(Point::from((40.0, -20.0)), 0.5),
            Point::from((-80.0, 40.0))
        );
        assert_eq!(
            camera_delta_for_drag(Point::from((40.0, -20.0)), 2.0),
            Point::from((-20.0, 10.0))
        );
    }
}
