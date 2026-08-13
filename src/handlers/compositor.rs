use std::{cell::Cell, rc::Rc};

use crate::{grabs::resize_grab, state::ClientState, Smallvil};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    reexports::{
        calloop::Interest,
        wayland_server::{
            backend::protocol::ProtocolError,
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

const MAX_PENDING_DMABUF_BLOCKERS_PER_CLIENT: usize = 64;
const MAX_PENDING_DMABUF_BLOCKERS_TOTAL: usize = 1024;

fn dmabuf_blocker_admitted(per_client: usize, total: usize) -> bool {
    per_client < MAX_PENDING_DMABUF_BLOCKERS_PER_CLIENT && total < MAX_PENDING_DMABUF_BLOCKERS_TOTAL
}

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
            let client_id = client.id();
            let per_client = state
                .dmabuf_blocker_sources
                .get(&client_id)
                .map_or(0, Vec::len);
            let total = state.dmabuf_blocker_source_count;
            if !dmabuf_blocker_admitted(per_client, total) {
                tracing::warn!(
                    per_client,
                    total,
                    "Disconnecting client after excessive pending DMA-BUF fences"
                );
                client.kill(
                    &state.display_handle,
                    ProtocolError {
                        code: 0,
                        object_id: surface.id().protocol_id(),
                        object_interface: "wl_surface".to_string(),
                        message: "too many pending DMA-BUF readiness fences".to_string(),
                    },
                );
                return;
            }

            let token_slot = Rc::new(Cell::new(None));
            let callback_token = token_slot.clone();
            let callback_client_id = client_id.clone();

            match state
                .loop_handle
                .insert_source(source, move |_, _, state: &mut Smallvil| {
                    if let Some(token) = callback_token.get() {
                        state.untrack_dmabuf_blocker_source(&callback_client_id, token);
                    }
                    if let Some(client_state) = client.get_data::<ClientState>() {
                        let dh = state.display_handle.clone();
                        client_state.compositor_state.blocker_cleared(state, &dh);
                    }
                    Ok(())
                }) {
                Ok(token) => {
                    token_slot.set(Some(token));
                    state.track_dmabuf_blocker_source(client_id, token);
                    add_blocker(surface, blocker);
                }
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
        if let Some(client) = surface.client() {
            if let Some(client_state) = client.get_data::<ClientState>() {
                let dh = self.display_handle.clone();
                client_state.compositor_state.blocker_cleared(self, &dh);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmabuf_blocker_caps_accept_below_and_reject_at_each_limit() {
        assert!(dmabuf_blocker_admitted(0, 0));
        assert!(dmabuf_blocker_admitted(
            MAX_PENDING_DMABUF_BLOCKERS_PER_CLIENT - 1,
            MAX_PENDING_DMABUF_BLOCKERS_TOTAL - 1,
        ));
        assert!(!dmabuf_blocker_admitted(
            MAX_PENDING_DMABUF_BLOCKERS_PER_CLIENT,
            0,
        ));
        assert!(!dmabuf_blocker_admitted(
            0,
            MAX_PENDING_DMABUF_BLOCKERS_TOTAL,
        ));
    }
}
