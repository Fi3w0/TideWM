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

    /// Commits the drag: a drop over another tile swaps the two; a drop
    /// anywhere else leaves the reef entirely and becomes floating at the
    /// drop position -- the same point-containment rule
    /// `smart_attach_ocean_floating` uses in the opposite direction, so
    /// moving a window out of the tiling area is how you float it and
    /// moving one in is how you tile it, with no separate keybind needed
    /// for either. Runs from `unset()`, not `button()`'s release detection,
    /// so it fires exactly once whenever this grab ends -- a real button
    /// release, a gesture-driven `unset_grab` (see
    /// `Smallvil::start_gesture_modifier_move`), or any other teardown path
    /// -- rather than only the one Smithay happens to reach it through.
    fn commit(&self, data: &mut Smallvil) {
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
        match data.ocean.tiled_target_at_view(
            &self.surface,
            &self.output,
            pointer_view,
            data.config.gaps,
            data.config.bsp_split_bias,
        ) {
            Some(target) => {
                data.ocean.swap_tiled(&self.surface, &target);
            }
            None => {
                let world_rect = self.current_world_rect(data);
                data.ocean
                    .make_floating(&self.surface, data.config.gaps, data.config.bsp_split_bias);
                data.ocean.set_floating_rect(&self.surface, world_rect);
            }
        }
        data.retile_viscous();
    }

    /// The window's live dragged rectangle in world space, shared by
    /// `motion` (to render/hit-test mid-drag) and `commit` (to know where
    /// to place it if the drop floats it instead of swapping).
    fn current_world_rect(&self, data: &Smallvil) -> Rectangle<i32, Logical> {
        let delta = self.last_location - self.start_data.location;
        let new_location = (self.initial_location.to_f64()
            + Point::from((delta.x / self.view_scale, delta.y / self.view_scale)))
        .to_i32_round();
        let size = data
            .ocean
            .world_rect(&self.surface, data.config.gaps, data.config.bsp_split_bias)
            .map(|rect| rect.size)
            .unwrap_or_else(|| self.window.geometry().size);
        Rectangle::new(new_location, size)
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
        let world_rect = self.current_world_rect(data);
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
            .map_element(self.window.clone(), world_rect.loc, false);
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
        // Runs whenever this grab ends, however it ends -- a real button
        // release, `unset_grab` called directly (the gesture-driven path),
        // window death, or a competing `set_grab` elsewhere -- so `commit`
        // always fires exactly once and the drag override never outlives
        // the gesture it was created for.
        self.commit(data);
        data.ocean.clear_tile_drag();
    }
}
