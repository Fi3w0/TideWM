//! Frame-local identity bridge for decorated client render elements.
//!
//! Rendering/damage keep the wrapper's namespaced ID. Protocol reporting
//! additionally associates its actual visible state with the source surface,
//! following niri's remapping of offscreen render IDs for presentation.

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement, RenderElementStates, UnderlyingStorage},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
        Renderer,
    },
    utils::{user_data::UserDataMap, Buffer, Physical, Point, Rectangle, Scale, Transform},
};

pub(crate) struct SurfaceRenderIdentity<E> {
    inner: E,
    surface_id: Id,
}

impl<E: Element> SurfaceRenderIdentity<E> {
    pub(crate) fn new(inner: E, surface_id: Id) -> Self {
        Self { inner, surface_id }
    }
    fn report(&self, states: &mut RenderElementStates) {
        alias_surface_state(states, self.inner.id(), &self.surface_id);
    }
}

impl<E: Element> Element for SurfaceRenderIdentity<E> {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
    fn is_framebuffer_effect(&self) -> bool {
        self.inner.is_framebuffer_effect()
    }
}

impl<R: Renderer, E: RenderElement<R>> RenderElement<R> for SurfaceRenderIdentity<E> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        self.inner
            .draw(frame, src, dst, damage, opaque_regions, cache)
    }
    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
    fn capture_framebuffer(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), R::Error> {
        self.inner.capture_framebuffer(frame, src, dst, cache)
    }
}

fn alias_surface_state(states: &mut RenderElementStates, rendered: &Id, surface: &Id) {
    let Some(actual) = states.element_render_state(rendered.clone()) else {
        return;
    };
    states
        .states
        .entry(surface.clone())
        .and_modify(|current| {
            if actual.visible_area > current.visible_area {
                *current = actual;
            }
        })
        .or_insert(actual);
}

pub(super) fn report_surface_identities(
    elements: &[super::udev::OutputRenderElements],
    states: &mut RenderElementStates,
) {
    for element in elements {
        match element {
            super::udev::OutputRenderElements::RoundedSurface(element) => element.report(states),
            super::udev::OutputRenderElements::AnimatedRoundedSurface(element) => {
                element.report(states)
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::renderer::element::{RenderElementPresentationState, RenderElementState};

    #[test]
    fn decorated_report_preserves_damage_identity_and_tracks_new_style_each_frame() {
        let surface = Id::new();
        let rounded = surface.clone().namespaced(12);
        let new_style = surface.clone().namespaced(13);
        let actual = RenderElementState {
            visible_area: 512,
            presentation_state: RenderElementPresentationState::Rendering { reason: None },
            needs_capture: false,
        };
        let mut first = RenderElementStates {
            states: [(rounded.clone(), actual)].into(),
        };
        assert!(!first.element_was_presented(surface.clone()));
        alias_surface_state(&mut first, &rounded, &surface);
        assert!(first.element_was_presented(surface.clone()));
        assert!(first.element_was_presented(rounded.clone()));
        let mut second = RenderElementStates {
            states: [(new_style.clone(), actual)].into(),
        };
        alias_surface_state(&mut second, &rounded, &surface);
        assert!(!second.element_was_presented(surface.clone()));
        alias_surface_state(&mut second, &new_style, &surface);
        assert!(second.element_was_presented(surface));
    }

    #[test]
    fn invisible_alias_cannot_hide_an_already_presented_source_or_invent_visibility() {
        let surface = Id::new();
        let rounded = surface.clone().namespaced(15);
        let skipped = RenderElementState {
            visible_area: 0,
            presentation_state: RenderElementPresentationState::Skipped,
            needs_capture: false,
        };
        let visible = RenderElementState {
            visible_area: 99,
            presentation_state: RenderElementPresentationState::ZeroCopy,
            needs_capture: false,
        };
        let mut states = RenderElementStates {
            states: [(rounded.clone(), skipped)].into(),
        };
        alias_surface_state(&mut states, &rounded, &surface);
        assert!(!states.element_was_presented(surface.clone()));
        states.states.insert(surface.clone(), visible);
        alias_surface_state(&mut states, &rounded, &surface);
        assert_eq!(
            states
                .element_render_state(surface)
                .unwrap()
                .presentation_state,
            RenderElementPresentationState::ZeroCopy
        );
    }
}
