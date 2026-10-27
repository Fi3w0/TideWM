//! `WindowSpace`: Smithay's `Space` with TideWM-owned output membership.
//!
//! `Space::refresh` sends `wl_surface.enter`/`leave` from pure geometry: any
//! output rectangle a window's bounding box touches is "entered". That is
//! wrong for windows whose position is a projection onto one output's
//! strip: a Scroll column scrolled past its output's edge overlaps the
//! neighbouring output's rectangle without ever being drawn there, so a
//! client on DP-4 was told it also sits on the next monitor and picked up
//! that monitor's geometry and integer scale.
//!
//! Each window may carry an owner output. An owned window only ever enters
//! its owner; an unowned window keeps Smithay's geometric behaviour, which
//! is correct for windows that really are drawn wherever they overlap.
//! Callers keep using plain `Window`s; the wrapper is private to this file.

use std::sync::Mutex;

use smithay::{
    desktop::{space::SpaceElement, Space, Window},
    output::{Output, WeakOutput},
    utils::{IsAlive, Logical, Point, Rectangle},
};

#[derive(Debug, Clone, PartialEq)]
struct OwnedWindow(Window);

#[derive(Default)]
struct OutputOwner(Mutex<Option<WeakOutput>>);

fn owner_of(window: &Window) -> Option<Output> {
    window.user_data().get::<OutputOwner>().and_then(|owner| {
        owner
            .0
            .lock()
            .unwrap()
            .as_ref()
            .and_then(WeakOutput::upgrade)
    })
}

fn may_enter(window: &Window, output: &Output) -> bool {
    owner_of(window).is_none_or(|owner| &owner == output)
}

impl IsAlive for OwnedWindow {
    fn alive(&self) -> bool {
        self.0.alive()
    }
}

impl SpaceElement for OwnedWindow {
    fn geometry(&self) -> Rectangle<i32, Logical> {
        SpaceElement::geometry(&self.0)
    }

    fn bbox(&self) -> Rectangle<i32, Logical> {
        SpaceElement::bbox(&self.0)
    }

    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        SpaceElement::is_in_input_region(&self.0, point)
    }

    fn z_index(&self) -> u8 {
        SpaceElement::z_index(&self.0)
    }

    fn set_activate(&self, activated: bool) {
        SpaceElement::set_activate(&self.0, activated);
    }

    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        if may_enter(&self.0, output) {
            SpaceElement::output_enter(&self.0, output, overlap);
        } else {
            SpaceElement::output_leave(&self.0, output);
        }
    }

    fn output_leave(&self, output: &Output) {
        SpaceElement::output_leave(&self.0, output);
    }

    fn refresh(&self) {
        SpaceElement::refresh(&self.0);
    }
}

/// See the module docs. Mirrors the subset of `Space` TideWM uses.
#[derive(Debug, Default)]
pub struct WindowSpace(Space<OwnedWindow>);

impl WindowSpace {
    pub fn map_element<P: Into<Point<i32, Logical>>>(
        &mut self,
        window: Window,
        location: P,
        activate: bool,
    ) {
        self.0.map_element(OwnedWindow(window), location, activate);
    }

    pub fn unmap_elem(&mut self, window: &Window) {
        self.0.unmap_elem(&OwnedWindow(window.clone()));
    }

    pub fn raise_element(&mut self, window: &Window, activate: bool) {
        self.0.raise_element(&OwnedWindow(window.clone()), activate);
    }

    pub fn elements(&self) -> impl DoubleEndedIterator<Item = &Window> + ExactSizeIterator {
        self.0.elements().map(|element| &element.0)
    }

    pub fn elements_for_output<'output>(
        &'output self,
        output: &'output Output,
    ) -> impl DoubleEndedIterator<Item = &'output Window> + use<'output> {
        self.0.elements_for_output(output).map(|element| &element.0)
    }

    pub fn element_under<P: Into<Point<f64, Logical>>>(
        &self,
        point: P,
    ) -> Option<(&Window, Point<i32, Logical>)> {
        self.0
            .element_under(point)
            .map(|(element, location)| (&element.0, location))
    }

    pub fn element_location(&self, window: &Window) -> Option<Point<i32, Logical>> {
        self.0.element_location(&OwnedWindow(window.clone()))
    }

    pub fn element_geometry(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        self.0.element_geometry(&OwnedWindow(window.clone()))
    }

    /// Outputs the window currently counts as being on, owner rule applied.
    pub fn outputs_for_element(&self, window: &Window) -> Vec<Output> {
        self.0
            .outputs_for_element(&OwnedWindow(window.clone()))
            .into_iter()
            .filter(|output| may_enter(window, output))
            .collect()
    }

    pub fn map_output<P: Into<Point<i32, Logical>>>(&mut self, output: &Output, location: P) {
        self.0.map_output(output, location);
    }

    pub fn unmap_output(&mut self, output: &Output) {
        self.0.unmap_output(output);
    }

    pub fn outputs(&self) -> impl Iterator<Item = &Output> {
        self.0.outputs()
    }

    pub fn output_geometry(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        self.0.output_geometry(output)
    }

    pub fn output_under<P: Into<Point<f64, Logical>>>(
        &self,
        point: P,
    ) -> impl Iterator<Item = &Output> {
        self.0.output_under(point)
    }

    pub fn refresh(&mut self) {
        self.0.refresh();
    }

    /// Sets (or clears, with `None`) the only output `window` may enter.
    /// Takes effect immediately: outputs the new rule forbids get a leave,
    /// and outputs it newly allows that the window overlaps get an enter,
    /// because `Space::refresh` only re-announces an output when the
    /// overlap changes.
    pub fn set_output_owner(&self, window: &Window, owner: Option<&Output>) {
        let state = window
            .user_data()
            .get_or_insert_threadsafe(OutputOwner::default);
        {
            let mut current = state.0.lock().unwrap();
            let unchanged = match (current.as_ref().and_then(WeakOutput::upgrade), owner) {
                (Some(current), Some(owner)) => &current == owner,
                (None, None) => current.is_none(),
                _ => false,
            };
            if unchanged {
                return;
            }
            *current = owner.map(Output::downgrade);
        }
        let Some(bbox) = self.0.element_bbox(&OwnedWindow(window.clone())) else {
            return;
        };
        for output in self.0.outputs() {
            let overlap = self
                .0
                .output_geometry(output)
                .and_then(|geometry| geometry.intersection(bbox));
            match overlap {
                Some(mut overlap) if may_enter(window, output) => {
                    overlap.loc -= bbox.loc;
                    SpaceElement::output_enter(window, output, overlap);
                }
                _ => SpaceElement::output_leave(window, output),
            }
        }
    }
}
