use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    delegate_layer_shell,
    desktop::{self, layer_map_for_output, PopupKind, WindowSurfaceType},
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    utils::SERIAL_COUNTER,
    wayland::{
        compositor::{get_parent, with_states},
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer, LayerSurface, LayerSurfaceData, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::PopupSurface,
        },
    },
};

use crate::Smallvil;

/// Should be called on `WlSurface::commit`. Returns whether `surface`
/// belongs to a layer-surface tree, so `compositor::commit` can skip its
/// xdg-toplevel commit handling (a surface only ever has one role).
pub fn handle_commit(state: &mut Smallvil, surface: &WlSurface) -> bool {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }

    let Some(output) = state
        .space
        .outputs()
        .find(|out| {
            layer_map_for_output(out)
                .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
        .cloned()
    else {
        return false;
    };

    // Unsynchronized subsurface commits reach `CompositorHandler::commit`
    // independently, but only the layer role's root surface owns the map
    // lifecycle. The generic compositor path already requested a redraw.
    if surface != &root {
        return true;
    }

    let mut map = layer_map_for_output(&output);
    // Arrange before the initial configure so it reflects any size the
    // client already set via set_size on its first commit, same ordering
    // niri uses and the same "configure only after the surface's own first
    // commit" rule xdg_shell::handle_commit already follows for toplevels.
    map.arrange();
    let layer = map
        .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
        .unwrap()
        .clone();
    drop(map);

    let tracking = if state.unmapped_layer_surfaces.contains(surface) {
        LayerTracking::Unmapped
    } else {
        LayerTracking::Mapped
    };
    let has_buffer =
        with_renderer_surface_state(surface, |renderer_state| renderer_state.buffer().is_some())
            .unwrap_or(false);
    let transition = layer_lifecycle_transition(tracking, has_buffer);

    match transition {
        LayerTransition::Map => {
            state.unmapped_layer_surfaces.remove(surface);
        }
        LayerTransition::Unmap => {
            state.unmapped_layer_surfaces.insert(surface.clone());
        }
        LayerTransition::None => {}
    }

    // As with xdg-toplevels, the null-buffer commit ends the mapped
    // lifetime and is not itself the new initial commit. Smithay's
    // pre-commit hook has reset the role state; a subsequent bufferless
    // commit enters here already tracked as Unmapped and receives the fresh
    // configure.
    if tracking == LayerTracking::Unmapped && !has_buffer {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        if !initial_configure_sent {
            layer.layer_surface().send_configure();
        }
    }

    state.retile();
    match transition {
        LayerTransition::Map
            if layer.cached_state().keyboard_interactivity != KeyboardInteractivity::None =>
        {
            state.focus_layer(surface.clone(), SERIAL_COUNTER.next_serial());
        }
        LayerTransition::Unmap => {
            state.forget_layer_focus(surface);
            state.repair_keyboard_focus(None, SERIAL_COUNTER.next_serial());
        }
        _ => state.reconcile_keyboard_focus(SERIAL_COUNTER.next_serial()),
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerTracking {
    Unmapped,
    Mapped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerTransition {
    None,
    Map,
    Unmap,
}

fn layer_lifecycle_transition(tracking: LayerTracking, has_buffer: bool) -> LayerTransition {
    match (tracking, has_buffer) {
        (LayerTracking::Unmapped, true) => LayerTransition::Map,
        (LayerTracking::Mapped, false) => LayerTransition::Unmap,
        _ => LayerTransition::None,
    }
}

impl WlrLayerShellHandler for Smallvil {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        tracing::info!(namespace, "New layer surface");

        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .filter(|out| self.space.outputs().any(|mapped| mapped == out))
            .or_else(|| self.primary_output());

        let Some(output) = output else {
            tracing::warn!("No output available for layer surface, closing it");
            surface.send_close();
            return;
        };

        let desktop_surface = desktop::LayerSurface::new(surface, namespace);
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&desktop_surface) {
            tracing::warn!(%err, "Failed to map layer surface");
        } else {
            // Seed the preferred scale immediately; live output-scale
            // changes refresh every mapped layer through output management.
            self.set_layer_fractional_scale(&desktop_surface, &output);
            self.unmapped_layer_surfaces
                .insert(desktop_surface.wl_surface().clone());
        }
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        self.unmapped_layer_surfaces.remove(surface.wl_surface());
        // `layer_rule { blur = true }`'s captured backdrop texture: without
        // this a layer surface that's opened and closed repeatedly (rofi,
        // any launcher) leaks one full-rect GLES texture per destroy, since
        // nothing else ever removes this map's entries for a layer surface.
        self.backdrop_textures.remove(surface.wl_surface());

        // Unmap from whichever output actually has it, then let retile()
        // (which re-tiles every output, re-reading each one's fresh
        // non-exclusive zone) sort out the fallout on that output.
        for out in self.space.outputs() {
            let mut map = layer_map_for_output(out);
            let found = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = found {
                map.unmap_layer(&layer);
                break;
            }
        }
        self.forget_layer_focus(surface.wl_surface());
        self.retile();
        self.repair_keyboard_focus(None, SERIAL_COUNTER.next_serial());
    }

    fn new_popup(&mut self, _parent: LayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(popup)) {
            tracing::warn!(?err, "Failed to track layer-shell popup");
        }
    }
}

delegate_layer_shell!(Smallvil);

impl Smallvil {
    /// The topmost mapped layer surface across all outputs that wants
    /// exclusive keyboard interactivity, respecting protocol/render layer
    /// priority before per-layer stacking order. `None` means normal window
    /// focus rules apply.
    pub(crate) fn exclusive_layer(&self) -> Option<desktop::LayerSurface> {
        [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background]
            .into_iter()
            .find_map(|kind| {
                self.space.outputs().find_map(|output| {
                    layer_map_for_output(output)
                        .layers_on(kind)
                        .rev()
                        .find(|layer| {
                            !self.unmapped_layer_surfaces.contains(layer.wl_surface())
                                && layer.cached_state().keyboard_interactivity
                                    == KeyboardInteractivity::Exclusive
                        })
                        .cloned()
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{layer_lifecycle_transition, LayerTracking, LayerTransition};

    #[test]
    fn layer_role_without_buffer_stays_unmapped() {
        assert_eq!(
            layer_lifecycle_transition(LayerTracking::Unmapped, false),
            LayerTransition::None
        );
    }

    #[test]
    fn layer_maps_once_and_unmaps_on_null_buffer() {
        assert_eq!(
            layer_lifecycle_transition(LayerTracking::Unmapped, true),
            LayerTransition::Map
        );
        assert_eq!(
            layer_lifecycle_transition(LayerTracking::Mapped, true),
            LayerTransition::None
        );
        assert_eq!(
            layer_lifecycle_transition(LayerTracking::Mapped, false),
            LayerTransition::Unmap
        );
    }

    #[test]
    fn layer_remap_requires_unmapped_phase_before_buffer() {
        assert_eq!(
            layer_lifecycle_transition(LayerTracking::Unmapped, false),
            LayerTransition::None
        );
        assert_eq!(
            layer_lifecycle_transition(LayerTracking::Unmapped, true),
            LayerTransition::Map
        );
    }
}
