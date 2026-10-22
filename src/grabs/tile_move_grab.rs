//! Pointer grab for tiled-window drag-to-swap. The window follows the cursor
//! visually while its layout membership remains unchanged. A completed drop
//! over another slot on the same output swaps the windows; a different
//! monitor inserts the tile into its active workspace. Cancellation or an
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
    /// Cross-output drops use the destination's active layout, while swaps
    /// on the original output remain scoped to the original workspace.
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
        if let Some(output) = data.output_for_point(self.last_location) {
            if output.name() != self.output {
                let workspace = data.layout.active_workspace(&output.name());
                let target = data.output_tiling_area(&output).and_then(|area| {
                    data.layout
                        .layout(
                            &output.name(),
                            workspace,
                            area,
                            data.gaps_for(&output.name(), workspace),
                        )
                        .into_iter()
                        .find(|(_, rect)| rect.contains(self.last_location.to_i32_round()))
                        .and_then(|(window, _)| window.toplevel().map(|t| t.wl_surface().clone()))
                });
                data.move_tiled_window_to_output(&self.surface, &output, target.as_ref());
                return;
            }
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
                let zone = data
                    .scroll_drop_target(
                        &self.surface,
                        &self.output,
                        self.workspace,
                        self.last_location,
                    )
                    .filter(|(scroll_target, _, _)| scroll_target == &target)
                    .map(|(_, zone, _)| zone);
                match zone {
                    Some(zone) if zone != crate::scroll::DropZone::Swap => {
                        data.layout.scroll_drop(&self.surface, &target, zone);
                    }
                    _ => data.layout.swap(&self.surface, &target),
                }
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

        data.classic_tile_drag = Some(self.surface.clone());
        let delta = event.location - self.start_data.location;
        let new_location = self.initial_location.to_f64() + delta;
        let new_location = new_location.to_i32_round();
        data.retarget_window_viscosity(
            &self.surface,
            Rectangle::new(new_location, self.window.geometry().size),
        );
        data.space
            .map_element(self.window.clone(), new_location, false);
        // Scroll shows where the drop lands: a new column, a place in a
        // column, or a swap.
        let hint = data
            .scroll_drop_target(&self.surface, &self.output, self.workspace, event.location)
            .map(|(_, zone, rect)| {
                let thickness = data.gaps_for(&self.output, self.workspace).max(6);
                (
                    self.output.clone(),
                    crate::scroll::drop_hint(rect, zone, data.config.scroll.orientation, thickness),
                )
            });
        data.set_drag_preview(hint);
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
        data.classic_tile_drag = None;
        data.set_drag_preview(None);
        // Motion moved only the visual Space element. Always snap it back
        // to authoritative tiling, including on cancellation.
        data.retile_viscous();
    }
}
