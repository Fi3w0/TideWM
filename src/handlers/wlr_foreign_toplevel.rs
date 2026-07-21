//! wlr-foreign-toplevel-management-v1: the older (read/write) foreign-toplevel
//! protocol that waybar's `wlr/taskbar` module and ags v1 hardcode against.
//!
//! Hand-rolled on `wayland-protocols-wlr`: Smithay 0.7 only ships a wrapper
//! module for the newer read-only `ext-foreign-toplevel-list-v1` (which we
//! also implement -- see `state.foreign_toplevel_list_state`). The two
//! coexist on separate globals; clients pick whichever they were written
//! against.
//!
//! Unlike the newer ext- protocol, the older wlr- variant is bidirectional:
//! clients send `activate`/`close`/`set_maximized`/`set_fullscreen`
//! requests, which we route through the same focus/fullscreen/maximize
//! paths the interactive keybinds use. State changes broadcast to every
//! bound client via the `state` event.
//!
//! First-pass scope: lifecycle (title/app_id/state/closed events) and the
//! bidirectional requests. The `output_enter`/`output_leave` events are
//! intentionally skipped: wayland-server has no direct `Output -> WlOutput`
//! accessor (a single Output has one WlOutput resource per bound client),
//! and waybar's taskbar still works without them -- it just lists all
//! toplevels rather than filtering by output. Future work if a real bar
//! needs output-filtering here.

use std::sync::{Arc, Mutex};

use smithay::reexports::wayland_server::{
    backend::ClientId, backend::GlobalId, Client, DataInit, Dispatch, GlobalDispatch, Resource,
};
use smithay::reexports::wayland_server::{DisplayHandle, New};
use smithay::wayland::shell::xdg::XdgShellHandler;
use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use crate::Smallvil;

/// Per-toplevel state, shared between the compositor and every bound client's
/// handle resource. Arc<Mutex> because wayland-server requires resource data
/// to be Send + Sync, and the lifecycle hooks mutate it from outside the
/// Dispatch path.
#[derive(Clone)]
pub struct WlrForeignToplevelHandle {
    inner: Arc<Mutex<WlrToplevelData>>,
}

struct WlrToplevelData {
    title: String,
    app_id: String,
    /// Per-client live handle resources. Each entry corresponds to one
    /// bound manager that received a `toplevel` event for this surface.
    instances: Vec<ZwlrForeignToplevelHandleV1>,
    closed: bool,
}

impl WlrForeignToplevelHandle {
    fn new(title: String, app_id: String) -> Self {
        Self {
            inner: Arc::new(Mutex::new(WlrToplevelData {
                title,
                app_id,
                instances: Vec::new(),
                closed: false,
            })),
        }
    }

    pub fn title(&self) -> String {
        self.inner.lock().unwrap().title.clone()
    }

    pub fn app_id(&self) -> String {
        self.inner.lock().unwrap().app_id.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    /// Pushes the initial title/app_id/state/done events to a freshly-created
    /// handle resource. Called once per bind (replays every live toplevel)
    /// and once per new toplevel (informs every already-bound manager).
    /// The empty initial `state` array is overwritten by the next
    /// `refresh_state` call from `state.rs`, which composes the real
    /// activated/fullscreen/maximized flags from compositor state.
    fn init_instance(&self, toplevel: ZwlrForeignToplevelHandleV1) {
        let mut inner = self.inner.lock().unwrap();
        toplevel.title(inner.title.clone());
        toplevel.app_id(inner.app_id.clone());
        toplevel.state(Vec::<u8>::new());
        toplevel.done();
        inner.instances.push(toplevel);
    }

    fn remove_instance(&self, instance: &ZwlrForeignToplevelHandleV1) {
        let mut inner = self.inner.lock().unwrap();
        inner.instances.retain(|i| i != instance);
    }

    pub fn send_title(&self, title: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.title == title {
            return;
        }
        inner.title = title.to_string();
        for instance in inner.instances.iter() {
            instance.title(title.to_string());
            instance.done();
        }
    }

    pub fn send_app_id(&self, app_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.app_id == app_id {
            return;
        }
        inner.app_id = app_id.to_string();
        for instance in inner.instances.iter() {
            instance.app_id(app_id.to_string());
            instance.done();
        }
    }

    /// Broadcasts the current fullscreen/maximized/activated state to every
    /// live handle resource, then a `done`. The `state_flags` argument is the
    /// raw protocol byte array (little-endian u32 state values packed
    /// together), composed by the caller from compositor state.
    pub fn send_state(&self, state_flags: Vec<u8>) {
        let inner = self.inner.lock().unwrap();
        for instance in inner.instances.iter() {
            instance.state(state_flags.clone());
            instance.done();
        }
    }

    /// Sends `closed` to every live handle, marks closed, drops per-instance
    /// refs. The handle is inert after this; only a fresh `track()` (next
    /// map lifecycle) produces a new one.
    pub fn send_closed(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return;
        }
        inner.closed = true;
        for instance in inner.instances.drain(..) {
            instance.closed();
        }
    }
}

/// Composes the raw little-endian byte array for the `state` event from the
/// three flags the protocol actually cares about (it does not track minimized
/// -- TideWM has no minimize concept, so we never advertise it).
pub fn state_bytes(activated: bool, maximized: bool, fullscreen: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12);
    use zwlr_foreign_toplevel_handle_v1::State;
    if maximized {
        bytes.extend_from_slice(&(State::Maximized as u32).to_le_bytes());
    }
    // State::Minimized is intentionally never advertised (see above).
    if activated {
        bytes.extend_from_slice(&(State::Activated as u32).to_le_bytes());
    }
    if fullscreen {
        bytes.extend_from_slice(&(State::Fullscreen as u32).to_le_bytes());
    }
    bytes
}

/// Global state for `zwlr_foreign_toplevel_manager_v1`. Holds the bound
/// manager instances and the live toplevel handles, so a new client bind
/// can be replayed against the current set.
pub struct WlrForeignToplevelState {
    #[allow(dead_code)]
    global: GlobalId,
    instances: Vec<ZwlrForeignToplevelManagerV1>,
    toplevels: Vec<WlrForeignToplevelHandle>,
    dh: DisplayHandle,
}

impl WlrForeignToplevelState {
    /// Registers a `zwlr_foreign_toplevel_manager_v1` global at version 3
    /// (the highest wayland-protocols-wlr 0.3.12 advertises; matches what
    /// waybar and ags bind).
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<Smallvil, ZwlrForeignToplevelManagerV1, ()>(3, ());
        Self {
            global,
            instances: Vec::new(),
            toplevels: Vec::new(),
            dh: dh.clone(),
        }
    }

    /// Tracks a freshly-mapped toplevel. Creates one per-bound-manager
    /// handle resource for it, sending the initial `toplevel` event to
    /// each. Called from `handlers/xdg_shell.rs::map_toplevel`, parallel
    /// to the newer-ext-protocol path that creates its own handle.
    pub fn track(&mut self, title: String, app_id: String) -> WlrForeignToplevelHandle {
        let handle = WlrForeignToplevelHandle::new(title, app_id);
        for instance in &self.instances {
            let Ok(client) = self.dh.get_client(instance.id()) else {
                continue;
            };
            let Ok(toplevel) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, Smallvil>(
                &self.dh,
                instance.version(),
                handle.clone(),
            ) else {
                continue;
            };
            instance.toplevel(&toplevel);
            handle.init_instance(toplevel);
        }
        self.toplevels.push(handle.clone());
        handle
    }

    /// Removes a tracked toplevel and sends `closed` to every live handle.
    pub fn untrack(&mut self, handle: &WlrForeignToplevelHandle) {
        handle.send_closed();
        self.toplevels
            .retain(|h| !Arc::ptr_eq(&h.inner, &handle.inner));
    }

    /// GCs closed handles so a long-running session doesn't accumulate
    /// dead entries. In the current code every removal path (`untrack`)
    /// already drains its own entry synchronously, so this should normally
    /// find nothing to do -- it exists as a safety net against a future
    /// path that marks a handle closed without also untracking it, cheap
    /// enough to run unconditionally every tick.
    pub fn cleanup_closed(&mut self) {
        self.toplevels.retain(|h| !h.is_closed());
    }
}

impl Smallvil {
    /// Wired into both backends' per-frame cleanup tick, next to
    /// `cleanup_capture`/`refresh_popup_grab` -- see `WlrForeignToplevelState::cleanup_closed`.
    pub fn cleanup_wlr_foreign_toplevels(&mut self) {
        if let Some(wlr_state) = self.wlr_foreign_toplevel_state.as_mut() {
            wlr_state.cleanup_closed();
        }
    }
}

// --- Protocol dispatch (impls on Smallvil directly, matching the pattern
//     handlers/screencopy.rs uses -- no separate dispatch target). ---

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for Smallvil {
    fn bind(
        state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let instance = data_init.init(resource, ());
        let Some(wlr_state) = state.wlr_foreign_toplevel_state.as_mut() else {
            return;
        };
        wlr_state.toplevels.retain(|h| !h.is_closed());

        for handle in &wlr_state.toplevels {
            let Ok(client) = wlr_state.dh.get_client(instance.id()) else {
                continue;
            };
            let Ok(toplevel) = client
                .create_resource::<ZwlrForeignToplevelHandleV1, _, Smallvil>(
                    &wlr_state.dh,
                    instance.version(),
                    handle.clone(),
                )
            else {
                continue;
            };
            instance.toplevel(&toplevel);
            handle.init_instance(toplevel);
        }
        wlr_state.instances.push(instance);
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Client signals it's done listening for new toplevels. We reply
        // `finished` and drop our instance; spec says the client must send
        // no further requests after this.
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            manager.finished();
            if let Some(wlr_state) = state.wlr_foreign_toplevel_state.as_mut() {
                wlr_state.instances.retain(|i| i != manager);
            }
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        if let Some(wlr_state) = state.wlr_foreign_toplevel_state.as_mut() {
            wlr_state.instances.retain(|i| i != resource);
        }
    }
}

/// Per-handle requests. The handle's data is the shared `WlrForeignToplevelHandle`,
/// which we map back to its `WlSurface` via `state.wlr_foreign_toplevels` so
/// the underlying window can be operated on.
impl Dispatch<ZwlrForeignToplevelHandleV1, WlrForeignToplevelHandle> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        _handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &WlrForeignToplevelHandle,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(surface) = state
            .wlr_foreign_toplevels
            .iter()
            .find_map(|(s, h)| Arc::ptr_eq(&h.inner, &data.inner).then_some(s.clone()))
        else {
            // Toplevel already closed at the compositor level; the
            // protocol still allows `destroy` after that, which we no-op.
            return;
        };

        match request {
            zwlr_foreign_toplevel_handle_v1::Request::Activate { seat: _ } => {
                // The seat arg is informational; per spec activation
                // isn't guaranteed. `activate_toplevel` is the real
                // xdg-activation-v1 sequence (switch workspace if hidden,
                // promote a parked group tab, raise if floating, then
                // focus) -- reuse it rather than the plain `focus_window`
                // this used to call, which silently no-ops on a hidden
                // window (`focus_window`'s target is filtered to visible
                // surfaces only), making a waybar taskbar click on a
                // window from another workspace do nothing. Its own
                // `exclusive_layer` check replaces the one this site used
                // to do itself.
                state.activate_toplevel(&surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                if let Some(toplevel) = state
                    .mapped_toplevel_window(&surface)
                    .as_ref()
                    .and_then(|w| w.toplevel())
                {
                    toplevel.send_close();
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                // No `!state.fullscreen.contains_key(&surface)` guard here:
                // `do_maximize_request` is explicitly designed to accept a
                // fullscreen floating window, recording restore-intent (it
                // reads the fullscreen entry's own restore rect as the
                // basis for the maximized one) rather than trying to
                // change anything visually while fullscreen still
                // dominates the render -- excluding that case here would
                // just silently drop a legitimate request.
                if !state.maximized.contains_key(&surface) && !state.layout.contains(&surface) {
                    if let Some(toplevel) = state
                        .mapped_toplevel_window(&surface)
                        .as_ref()
                        .and_then(|w| w.toplevel())
                        .cloned()
                    {
                        Smallvil::maximize_request(state, toplevel);
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                if state.maximized.contains_key(&surface) {
                    if let Some(toplevel) = state
                        .mapped_toplevel_window(&surface)
                        .as_ref()
                        .and_then(|w| w.toplevel())
                        .cloned()
                    {
                        Smallvil::unmaximize_request(state, toplevel);
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output: _ } => {
                if !state.fullscreen.contains_key(&surface) {
                    if let Some(toplevel) = state
                        .mapped_toplevel_window(&surface)
                        .as_ref()
                        .and_then(|w| w.toplevel())
                        .cloned()
                    {
                        Smallvil::fullscreen_request(state, toplevel, None);
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                if state.fullscreen.contains_key(&surface) {
                    if let Some(toplevel) = state
                        .mapped_toplevel_window(&surface)
                        .as_ref()
                        .and_then(|w| w.toplevel())
                        .cloned()
                    {
                        Smallvil::unfullscreen_request(state, toplevel);
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
                // TideWM has no minimize concept. Per spec a no-op reply
                // is acceptable; we never advertise the `minimized` state
                // flag, so a client's toggle visually does nothing.
                tracing::debug!(
                    "wlr-foreign-toplevel minimize request ignored: TideWM has no minimize"
                );
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {
                // Minimize-target rectangle hint. We don't animate
                // minimize, so there's nothing to use this for. Spec
                // explicitly allows ignoring it.
            }
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        _state: &mut Self,
        _client: ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
        handle: &WlrForeignToplevelHandle,
    ) {
        handle.remove_instance(resource);
    }
}
