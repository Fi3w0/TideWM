//! Modifier+right-drag on a Scroll tile: dragging along the strip changes
//! the column width, dragging across it changes the window's share of a
//! shared column. Scroll's counterpart to `tile_window_resize_grab.rs`.

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

/// What the drag started from, measured once at button press.
pub struct ScrollResizeStart {
    /// Column head, which carries the column width.
    pub head: WlSurface,
    pub width: f64,
    /// Usable viewport length along the strip, in logical pixels.
    pub viewport: f64,
    /// The dragged window's row share and current cross size; `None` when
    /// it has its column to itself.
    pub row: Option<(f64, f64)>,
    pub horizontal: bool,
}

pub struct ScrollResizeGrab {
    start_data: PointerGrabStartData<Smallvil>,
    surface: WlSurface,
    start: ScrollResizeStart,
}

impl ScrollResizeGrab {
    pub fn start(
        start_data: PointerGrabStartData<Smallvil>,
        surface: WlSurface,
        start: ScrollResizeStart,
    ) -> Self {
        Self {
            start_data,
            surface,
            start,
        }
    }
}

impl PointerGrab<Smallvil> for ScrollResizeGrab {
    fn motion(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);
        if !data.spatial.workspaces.layout.contains(&self.surface)
            || !data.spatial.workspaces.layout.contains(&self.start.head)
        {
            return;
        }
        let delta = event.location - self.start_data.location;
        let (along, across) = if self.start.horizontal {
            (delta.x, delta.y)
        } else {
            (delta.y, delta.x)
        };
        data.spatial.workspaces.layout.scroll.resize(
            self.start.head.clone(),
            self.start.width + along / self.start.viewport.max(1.0),
        );
        if let Some((weight, cross)) = self.start.row {
            let cross = cross.max(1.0);
            data.spatial
                .workspaces
                .layout
                .scroll
                .set_weight(&self.surface, weight * (cross + across).max(1.0) / cross);
        }
        data.retile_viscous();
        data.reveal_scroll_window(&self.surface);
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
