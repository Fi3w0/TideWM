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

pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<Smallvil>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    pub view_scale: f64,
    pub smart_attach_ocean: bool,
    pub(crate) last_location: Point<f64, Logical>,
}

impl PointerGrab<Smallvil> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Smallvil,
        handle: &mut PointerInnerHandle<'_, Smallvil>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        if !self.window.alive() {
            return;
        }
        self.last_location = event.location;
        let Some(surface) = self.window.toplevel().map(|toplevel| toplevel.wl_surface()) else {
            return;
        };
        // A workspace switch or null-buffer unmap can end the window's
        // visible lifetime while the physical button remains held. `alive`
        // only checks the resource, so mapping here without this ownership
        // guard would resurrect a hidden/unmapped window into Space.
        if !data.window_is_visible(surface)
            || (!data.floating_workspace.contains_key(surface)
                && data.ocean.floating_rect(surface).is_none())
            || data.fullscreen.contains_key(surface)
            || data.maximized.contains_key(surface)
        {
            return;
        }

        let view_delta = event.location - self.start_data.location;
        let scale = self.view_scale.max(0.05);
        let delta = Point::from((view_delta.x / scale, view_delta.y / scale));
        let new_location = self.initial_window_location.to_f64() + delta;
        let new_location = new_location.to_i32_round();
        let previous_location = data.space.element_location(&self.window);
        data.retarget_window_viscosity(
            surface,
            Rectangle::new(new_location, self.window.geometry().size),
        );
        data.space
            .map_element(self.window.clone(), new_location, false);
        if let Some(mut rect) = data.ocean.floating_rect(surface) {
            rect.loc = new_location;
            data.ocean.set_floating_rect(surface, rect);
            if data.pinned.contains(surface) {
                if let Some(output) = data.ocean.entry_output(surface).map(str::to_string) {
                    data.ocean.unpin_from_screen(surface);
                    data.ocean.pin_to_screen(surface, &output);
                }
            }
            // Live feedback for a floating Ocean drag, mirroring
            // `OceanTileMoveGrab::motion`'s own hint computation: hovering
            // directly over a tile highlights it (a drop would reattach
            // there); everywhere else shows nothing (a drop stays
            // floating). Precise point-containment, matching
            // `smart_attach_ocean_floating`'s own drop decision -- see that
            // function's comment for why the proximity-radius
            // `smart_tiling_target` isn't used here either. Reuses the same
            // `drag_hint` the tile-swap grab already draws through
            // `window_border_element` -- no new visual code, just feeding it
            // from a second grab.
            if self.smart_attach_ocean {
                let hint = data
                    .ocean
                    .entry_output(surface)
                    .map(str::to_string)
                    .and_then(|output| {
                        data.output_by_name(&output)
                            .and_then(|o| data.space.output_geometry(&o))
                            .map(|output_geo| (output, output_geo))
                    })
                    .and_then(|(output, output_geo)| {
                        let pointer_view = self.last_location - output_geo.loc.to_f64();
                        data.ocean.tiled_target_at_view(
                            surface,
                            &output,
                            pointer_view,
                            data.config.gaps,
                            data.config.bsp_split_bias,
                        )
                    });
                data.ocean.set_tile_drag(surface.clone(), rect, hint);
            }
        }
        if let Some(previous_location) = previous_location {
            let delta = (
                (new_location.x - previous_location.x) as f64,
                (new_location.y - previous_location.y) as f64,
            );
            // `light` subsumes `sway` for a window that resolves both
            // enabled -- the two mechanics never stack on the same window.
            if data.float_physics_enabled_for_surface(surface) {
                data.float_physics_kick(surface, delta);
            } else {
                data.sway_kick(surface, delta.0);
            }
        }
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
            // The button that started the grab was released; release the grab.
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
        if self.smart_attach_ocean {
            if let Some(surface) = self.window.toplevel().map(|toplevel| toplevel.wl_surface()) {
                data.smart_attach_ocean_floating(surface, self.last_location);
            }
            data.ocean.clear_tile_drag();
        }
        data.sync_visible_floating_window(&self.window);
    }
}
