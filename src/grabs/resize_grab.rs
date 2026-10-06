use crate::{
    window_size::{anchored_rect, SizeAnchor},
    Smallvil,
};
use smithay::{
    desktop::{Space, Window},
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{IsAlive, Logical, Point, Rectangle, Size},
    wayland::compositor,
};
use std::cell::RefCell;

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const TOP          = 0b0001;
        const BOTTOM       = 0b0010;
        const LEFT         = 0b0100;
        const RIGHT        = 0b1000;

        const TOP_LEFT     = Self::TOP.bits() | Self::LEFT.bits();
        const BOTTOM_LEFT  = Self::BOTTOM.bits() | Self::LEFT.bits();

        const TOP_RIGHT    = Self::TOP.bits() | Self::RIGHT.bits();
        const BOTTOM_RIGHT = Self::BOTTOM.bits() | Self::RIGHT.bits();
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap()
    }
}

pub struct ResizeSurfaceGrab {
    start_data: PointerGrabStartData<Smallvil>,
    window: Window,

    edges: ResizeEdge,

    initial_rect: Rectangle<i32, Logical>,
    last_window_size: Size<i32, Logical>,
    view_scale: f64,
}

impl ResizeSurfaceGrab {
    pub fn start(
        start_data: PointerGrabStartData<Smallvil>,
        window: Window,
        edges: ResizeEdge,
        initial_window_rect: Rectangle<i32, Logical>,
        view_scale: f64,
    ) -> Self {
        let initial_rect = initial_window_rect;

        ResizeSurfaceState::with(window.toplevel().unwrap().wl_surface(), |state| {
            *state = ResizeSurfaceState::Resizing {
                edges,
                initial_rect,
            };
        });

        Self {
            start_data,
            window,
            edges,
            initial_rect,
            last_window_size: initial_rect.size,
            view_scale: view_scale.max(0.05),
        }
    }
}

impl PointerGrab<Smallvil> for ResizeSurfaceGrab {
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
        let surface = self.window.toplevel().unwrap().wl_surface();
        if !data.window_is_visible(surface)
            || (!data.floating_workspace.contains_key(surface)
                && data.ocean.floating_rect(surface).is_none())
            || data.fullscreen.contains_key(surface)
            || data.maximized.contains_key(surface)
        {
            return;
        }

        let view_delta = event.location - self.start_data.location;
        let mut delta: Point<f64, Logical> = Point::from((
            view_delta.x / self.view_scale,
            view_delta.y / self.view_scale,
        ));

        let mut new_window_width = self.initial_rect.size.w;
        let mut new_window_height = self.initial_rect.size.h;

        if self.edges.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT) {
            if self.edges.intersects(ResizeEdge::LEFT) {
                delta.x = -delta.x;
            }

            new_window_width = (self.initial_rect.size.w as f64 + delta.x) as i32;
        }

        if self.edges.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM) {
            if self.edges.intersects(ResizeEdge::TOP) {
                delta.y = -delta.y;
            }

            new_window_height = (self.initial_rect.size.h as f64 + delta.y) as i32;
        }

        self.last_window_size = data
            .floating_size_constraints_for(surface)
            .clamp_dimensions(new_window_width, new_window_height);

        let xdg = self.window.toplevel().unwrap();
        let target_location = resize_rect(self.initial_rect, self.last_window_size, self.edges).loc;
        data.retarget_window_viscosity(
            xdg.wl_surface(),
            Rectangle::new(target_location, self.last_window_size),
        );
        if data.ocean.floating_rect(xdg.wl_surface()).is_some() {
            data.ocean.set_floating_rect(
                xdg.wl_surface(),
                Rectangle::new(target_location, self.last_window_size),
            );
        }
        xdg.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
            state.size = Some(self.last_window_size);
        });

        xdg.send_pending_configure();
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

            if !self.window.alive() {
                return;
            }

            let surface = self.window.toplevel().unwrap().wl_surface();
            if !data.window_is_visible(surface)
                || (!data.floating_workspace.contains_key(surface)
                    && data.ocean.floating_rect(surface).is_none())
                || data.fullscreen.contains_key(surface)
                || data.maximized.contains_key(surface)
            {
                return;
            }

            let xdg = self.window.toplevel().unwrap();
            xdg.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Resizing);
                state.size = Some(self.last_window_size);
            });

            xdg.send_pending_configure();

            ResizeSurfaceState::with(xdg.wl_surface(), |state| {
                *state = ResizeSurfaceState::WaitingForLastCommit {
                    edges: self.edges,
                    initial_rect: self.initial_rect,
                };
            });
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

    fn unset(&mut self, _data: &mut Smallvil) {
        let Some(toplevel) = self.window.toplevel() else {
            return;
        };
        cancel(toplevel.wl_surface());
        toplevel.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Resizing);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

/// Both motion and committed client geometry keep the opposite edge fixed.
/// This uses the shared wide-coordinate arithmetic for extreme client sizes.
fn resize_rect(
    rect: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
    edges: ResizeEdge,
) -> Rectangle<i32, Logical> {
    anchored_rect(
        rect,
        size,
        if edges.contains(ResizeEdge::LEFT) {
            SizeAnchor::End
        } else {
            SizeAnchor::Start
        },
        if edges.contains(ResizeEdge::TOP) {
            SizeAnchor::End
        } else {
            SizeAnchor::Start
        },
    )
}

/// State of the resize operation.
///
/// It is stored inside of WlSurface,
/// and can be accessed using [`ResizeSurfaceState::with`]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum ResizeSurfaceState {
    #[default]
    Idle,
    Resizing {
        edges: ResizeEdge,
        /// The initial window size and location.
        initial_rect: Rectangle<i32, Logical>,
    },
    /// Resize is done, we are now waiting for last commit, to do the final move
    WaitingForLastCommit {
        edges: ResizeEdge,
        /// The initial window size and location.
        initial_rect: Rectangle<i32, Logical>,
    },
}

impl ResizeSurfaceState {
    fn with<F, T>(surface: &WlSurface, cb: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        compositor::with_states(surface, |states| {
            states.data_map.insert_if_missing(RefCell::<Self>::default);
            let state = states.data_map.get::<RefCell<Self>>().unwrap();

            cb(&mut state.borrow_mut())
        })
    }

    fn commit(&mut self) -> Option<(ResizeEdge, Rectangle<i32, Logical>)> {
        match *self {
            Self::Resizing {
                edges,
                initial_rect,
            } => Some((edges, initial_rect)),
            Self::WaitingForLastCommit {
                edges,
                initial_rect,
            } => {
                // The resize is done, let's go back to idle
                *self = Self::Idle;

                Some((edges, initial_rect))
            }
            Self::Idle => None,
        }
    }
}

pub fn cancel(surface: &WlSurface) {
    compositor::with_states(surface, |states| {
        if let Some(state) = states.data_map.get::<RefCell<ResizeSurfaceState>>() {
            *state.borrow_mut() = ResizeSurfaceState::Idle;
        }
    });
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(space: &mut Space<Window>, surface: &WlSurface) -> Option<()> {
    let window = space
        .elements()
        .find(|w| w.toplevel().unwrap().wl_surface() == surface)
        .cloned()?;

    let mut window_loc = space.element_location(&window)?;
    let geometry = window.geometry();

    let new_loc: Point<Option<i32>, Logical> = ResizeSurfaceState::with(surface, |state| {
        state
            .commit()
            .and_then(|(edges, initial_rect)| {
                // If the window is being resized by top or left, its location must be adjusted
                // accordingly.
                edges.intersects(ResizeEdge::TOP_LEFT).then(|| {
                    let resized = resize_rect(initial_rect, geometry.size, edges);
                    let new_x = edges.intersects(ResizeEdge::LEFT).then_some(resized.loc.x);

                    let new_y = edges.intersects(ResizeEdge::TOP).then_some(resized.loc.y);

                    (new_x, new_y).into()
                })
            })
            .unwrap_or_default()
    });

    if let Some(new_x) = new_loc.x {
        window_loc.x = new_x;
    }
    if let Some(new_y) = new_loc.y {
        window_loc.y = new_y;
    }

    if new_loc.x.is_some() || new_loc.y.is_some() {
        // If TOP or LEFT side of the window got resized, we have to move it
        space.map_element(window, window_loc, false);
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_resize_edges_keep_the_correct_original_edges() {
        let rect = Rectangle::new((100, 200).into(), (500, 300).into());
        for edges in [
            ResizeEdge::LEFT,
            ResizeEdge::RIGHT,
            ResizeEdge::TOP,
            ResizeEdge::BOTTOM,
            ResizeEdge::TOP_LEFT,
            ResizeEdge::TOP_RIGHT,
            ResizeEdge::BOTTOM_LEFT,
            ResizeEdge::BOTTOM_RIGHT,
        ] {
            let resized = resize_rect(rect, (200, 400).into(), edges);
            assert_eq!(
                resized.loc.x,
                if edges.contains(ResizeEdge::LEFT) {
                    400
                } else {
                    100
                }
            );
            assert_eq!(
                resized.loc.y,
                if edges.contains(ResizeEdge::TOP) {
                    100
                } else {
                    200
                }
            );
        }
    }
}
