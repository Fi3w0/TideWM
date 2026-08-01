//! Keeps an Ocean tiled window in its reef while dragging it between slots.
//!
//! The tree stays unchanged during the gesture, so the pointer can be
//! compared with the real tile rectangles on release. A successful drop swaps
//! two leaves; any other drop simply retile-snaps the window back to its slot.
//!
//! Visually the window lifts out of the grid and follows the pointer --
//! `OceanSpace::set_tile_drag` overrides its rendered rectangle for the
//! gesture's duration, since Ocean's renderer reads placement from the reef
//! tree, not from `Space` (`Space` is still updated here, but only for
//! hit-testing). The current swap target gets a magnet-style border
//! highlight for the same reason a fridge magnet needs a light: so dropping
//! is a decision, not a guess.

use crate::Smallvil;
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

pub struct OceanTileMoveGrab {
    start_data: PointerGrabStartData<Smallvil>,
    window: Window,
    surface: WlSurface,
    output: String,
    initial_location: Point<i32, Logical>,
    last_location: Point<f64, Logical>,
    view_scale: f64,
}

impl OceanTileMoveGrab {
    pub fn start(
        start_data: PointerGrabStartData<Smallvil>,
        window: Window,
        surface: WlSurface,
        output: String,
        initial_location: Point<i32, Logical>,
        view_scale: f64,
    ) -> Self {
        Self {
            last_location: start_data.location,
            start_data,
            window,
            surface,
            output,
            initial_location,
            view_scale: view_scale.max(0.05),
        }
    }

    fn drop(&self, data: &mut Smallvil) {
        if !data.window_is_visible(&self.surface)
            || !data.ocean.is_tiled(&self.surface)
            || data.fullscreen.contains_key(&self.surface)
        {
            return;
        }
        let Some(output) = data.output_by_name(&self.output) else {
            return;
        };
        let Some(output_geo) = data.space.output_geometry(&output) else {
            return;
        };
        let pointer_view = self.last_location - output_geo.loc.to_f64();
        if let Some(target) = data.ocean.tiled_target_at_view(
            &self.surface,
            &self.output,
            pointer_view,
            data.config.gaps,
            data.config.bsp_split_bias,
        ) {
            data.ocean.swap_tiled(&self.surface, &target);
        }
        data.retile_viscous();
    }
}

impl PointerGrab<Smallvil> for OceanTileMoveGrab {
    fn motion(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        self.last_location = event.location;
        if !self.window.alive()
            || !data.window_is_visible(&self.surface)
            || !data.ocean.is_tiled(&self.surface)
            || data.fullscreen.contains_key(&self.surface)
        {
            return;
        }
        let delta = event.location - self.start_data.location;
        let new_location = (self.initial_location.to_f64()
            + Point::from((delta.x / self.view_scale, delta.y / self.view_scale)))
        .to_i32_round();
        let size = data
            .ocean
            .world_rect(&self.surface, data.config.gaps, data.config.bsp_split_bias)
            .map(|rect| rect.size)
            .unwrap_or_else(|| self.window.geometry().size);
        let world_rect = Rectangle::new(new_location, size);
        data.retarget_window_viscosity(&self.surface, world_rect);
        // Lifts the window out of its frozen reef slot for rendering (the
        // tree itself stays untouched until release) and picks the current
        // swap target for the magnet-highlight border -- see
        // `OceanSpace::set_tile_drag`'s doc comment for why `Space` alone
        // (updated below for hit-testing) isn't enough here.
        let hint = data
            .output_by_name(&self.output)
            .and_then(|output| data.space.output_geometry(&output))
            .and_then(|output_geo| {
                let pointer_view = self.last_location - output_geo.loc.to_f64();
                data.ocean.tiled_target_at_view(
                    &self.surface,
                    &self.output,
                    pointer_view,
                    data.config.gaps,
                    data.config.bsp_split_bias,
                )
            });
        data.ocean
            .set_tile_drag(self.surface.clone(), world_rect, hint);
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
        handle.relative_motion(data, focus, event)
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
            self.drop(data);
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

    fn start_data(&self) -> &PointerGrabStartData<Smallvil> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut Smallvil) {
        // Always runs before `drop()` (Smithay calls it from
        // `unset_grab` first), and also covers every other way this grab
        // can end -- window death, a competing `set_grab` elsewhere -- that
        // never reaches `drop()` at all. Without this, a drag that ends any
        // way other than a plain button release leaves the window pinned
        // to a phantom rectangle forever.
        data.ocean.clear_tile_drag();
    }
}
