//! Pointer grab for tiled-window drag-to-swap. The window follows the cursor
//! visually while its layout membership remains unchanged. A completed drop
//! over another immutable layout slot swaps the windows; cancellation or an
//! invalid target retiles the original tree. Pointer position is retained
//! from motion events and never re-queried from inside a pointer callback.

use crate::{grabs::GrabCompletion, Smallvil};
use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Point, Rectangle},
};

pub struct TileMoveGrab {
    start_data: PointerGrabStartData<Smallvil>,
    window: Window,
    surface: WlSurface,
    output: String,
    workspace: u32,
    initial_location: Point<i32, Logical>,
    last_location: Point<f64, Logical>,
    completion: GrabCompletion,
}

impl TileMoveGrab {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        start_data: PointerGrabStartData<Smallvil>,
        window: Window,
        surface: WlSurface,
        output: String,
        workspace: u32,
        initial_location: Point<i32, Logical>,
    ) -> Self {
        let last_location = start_data.location;
        Self {
            start_data,
            window,
            surface,
            output,
            workspace,
            initial_location,
            last_location,
            completion: GrabCompletion::default(),
        }
    }

    pub(crate) fn with_completion(mut self, completion: GrabCompletion) -> Self {
        self.completion = completion;
        self
    }

    /// Hit-tests wherever the pointer is *now* against the tiling layout
    /// as it stands right now -- not against `space`'s live state, which
    /// mid-drag reflects wherever this window's being visually dragged to,
    /// not its real tree position, so hit-testing against `space` would
    /// almost always just find this same (topmost, still-mapped-there)
    /// window under the cursor instead of whatever it's actually being
    /// dropped onto. `Layouts::layout` instead reports every tile at its
    /// real, unchanged slot, `self.window`'s included, since nothing
    /// about tree membership moved during the drag.
    ///
    /// Scoped to this window's own (output, workspace) -- dropping on a
    /// different output or workspace than it started on doesn't relocate
    /// it there, just snaps it back; that's a deliberate first-pass scope
    /// limit (see `AGENT.md`), not an oversight.
    ///
    /// Runs from `unset()` only after a real initiating-button release or
    /// a successful gesture end marks the shared completion token. Forced
    /// teardown still snaps the visual drag back without mutating the tree.
    fn commit(&self, data: &mut Smallvil) {
        if !data.window_is_visible(&self.surface)
            || !data.layout.contains(&self.surface)
            || data.fullscreen.contains_key(&self.surface)
        {
            return;
        }
        if let Some(area) = data
            .output_by_name(&self.output)
            .and_then(|output| data.output_tiling_area(&output))
        {
            let pointer_loc = self.last_location.to_i32_round();
            let target = data
                .layout
                .layout(
                    &self.output,
                    self.workspace,
                    area,
                    data.gaps_for(&self.output, self.workspace),
                )
                .into_iter()
                .find(|(_, rect)| rect.contains(pointer_loc))
                .and_then(|(window, _)| window.toplevel().map(|t| t.wl_surface().clone()))
                .filter(|surface| *surface != self.surface);
            if let Some(target) = target {
                data.layout.swap(&self.surface, &target);
            }
        }
    }
}

impl PointerGrab<Smallvil> for TileMoveGrab {
    fn motion(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        self.last_location = event.location;

        if !self.window.alive() {
            return;
        }
        if !data.window_is_visible(&self.surface)
            || !data.layout.contains(&self.surface)
            || data.fullscreen.contains_key(&self.surface)
        {
            return;
        }

        let delta = event.location - self.start_data.location;
        let new_location = self.initial_location.to_f64() + delta;
        let new_location = new_location.to_i32_round();
        data.retarget_window_viscosity(
            &self.surface,
            Rectangle::new(new_location, self.window.geometry().size),
        );
        data.space
            .map_element(self.window.clone(), new_location, false);
        data.request_redraw();
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
            self.completion.mark_complete();
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

    fn unset(&mut self, data: &mut Smallvil) {
        if self.completion.take_complete() {
            self.commit(data);
        }
        // Motion moved only the visual Space element. Always snap it back
        // to authoritative tiling, including on cancellation.
        data.retile_viscous();
    }
}
