use crate::{grabs::resize_grab, state::ClientState, Smallvil};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    reexports::{
        calloop::Interest,
        wayland_server::{
            protocol::{wl_buffer, wl_surface::WlSurface},
            Client, Resource,
        },
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface, with_states,
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes,
        },
        dmabuf::get_dmabuf,
        shm::{ShmHandler, ShmState},
    },
};

use super::{layer_shell, xdg_shell};

impl CompositorHandler for Smallvil {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        // A DMA-BUF may still be busy when the client commits it. Install the
        // readiness blocker before Smithay merges pending surface state; the
        // regular `commit` callback is too late because the transaction has
        // already been applied by then.
        add_pre_commit_hook::<Smallvil, _>(surface, |state, _dh, surface| {
            let maybe_dmabuf = with_states(surface, |surface_data| {
                surface_data
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    })
            });
            let Some(dmabuf) = maybe_dmabuf else {
                return;
            };
            let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) else {
                // `AlreadyReady` means no blocker is necessary.
                return;
            };
            let Some(client) = surface.client() else {
                return;
            };

            match state.loop_handle.insert_source(
                source,
                move |_, _, state: &mut Smallvil| {
                    if let Some(client_state) = client.get_data::<ClientState>() {
                        let dh = state.display_handle.clone();
                        client_state.compositor_state.blocker_cleared(state, &dh);
                    }
                    Ok(())
                },
            ) {
                Ok(_) => add_blocker(surface, blocker),
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "Failed to register DMA-BUF readiness source; commit will not be blocked"
                    );
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Smithay requires this to be the first commit-processing step: its
        // persistent renderer state is the source of truth for whether a
        // surface currently has a usable buffer.
        on_commit_buffer_handler::<Self>(surface);
        self.request_redraw();
        let committed_window = if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            self.mapped_toplevel_window(&root)
        } else {
            None
        };

        // A surface only ever has one role, so at most one of these two
        // actually does anything for a given commit; layer-shell surfaces
        // skip the xdg-toplevel/popup handling entirely.
        if !layer_shell::handle_commit(self, surface) {
            xdg_shell::handle_commit(self, surface);
        }

        // Capture before lifecycle handling so a null-buffer commit still
        // reaches `Window::on_commit`; look up again afterward so a first
        // non-null buffer does too. This also handles commits to
        // non-synchronized subsurfaces by resolving their toplevel root.
        if let Some(window) = committed_window.or_else(|| {
            if is_sync_subsurface(surface) {
                return None;
            }
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            self.mapped_toplevel_window(&root)
        }) {
            let is_toplevel_commit = window.toplevel().unwrap().wl_surface() == surface;
            window.on_commit();
            if is_toplevel_commit {
                // A reactive xdg-popup follows changes to its parent's
                // geometry. Recompute only on root commits so ordinary
                // subsurface damage cannot create configure churn.
                self.update_reactive_popups(&window);
            }
        }
        resize_grab::handle_commit(&mut self.space, surface);
        if let Some(window) = self.mapped_toplevel_window(surface) {
            // The resize helper may have adjusted top/left placement only
            // after Window::on_commit. Persist the settled floating geometry
            // and cross-output ownership after that final adjustment.
            self.sync_visible_floating_window(&window);
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        // Idle-inhibit isn't specific to xdg_toplevels (a layer-shell surface
        // or a bare wl_surface with no shell role at all can hold one too),
        // so this generic wl_surface-destroyed hook -- not
        // XdgShellHandler::toplevel_destroyed -- is the correct place to
        // clean up any inhibitors a client left behind by disconnecting
        // without explicit Destroy requests on them. The surface is gone, so
        // every inhibitor it held dies with it regardless of count.
        if self.idle_inhibitors.remove(surface).is_some() && self.idle_inhibitors.is_empty() {
            self.idle_notifier_state.set_is_inhibited(false);
        }
    }
}

impl BufferHandler for Smallvil {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Smallvil {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(Smallvil);
delegate_shm!(Smallvil);
