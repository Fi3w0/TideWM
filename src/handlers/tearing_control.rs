//! Tracks wp-tearing-control-v1 hints in double-buffered surface state.
//! The hints are not yet honored by KMS because the pinned Smithay API does
//! not expose async-flip flags to `DrmCompositor`; no config toggle is exposed
//! until the backend can apply it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::{
    wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1},
    wp_tearing_control_v1::{self, PresentationHint, WpTearingControlV1},
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, Weak,
};
use smithay::wayland::compositor::{self, Cacheable};

use crate::Smallvil;

/// Double-buffered per-surface presentation hint. Lives in the surface's
/// own `cached_state`, so it needs no lifecycle hook -- it is dropped along
/// with the rest of the surface's compositor state when the surface dies.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TearingSurfaceCachedState {
    hint: PresentationHint,
}

impl Default for TearingSurfaceCachedState {
    fn default() -> Self {
        Self {
            hint: PresentationHint::Vsync,
        }
    }
}

#[allow(dead_code)] // read once a DRM async-flip path exists; see module doc comment
impl TearingSurfaceCachedState {
    pub(crate) fn hint(&self) -> PresentationHint {
        self.hint
    }
}

impl Cacheable for TearingSurfaceCachedState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        *self
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Guards the protocol's one-object-per-surface rule (`tearing_control_exists`).
#[derive(Debug, Default)]
struct TearingSurfaceData {
    attached: AtomicBool,
}

impl TearingSurfaceData {
    fn attached(&self) -> bool {
        self.attached.load(Ordering::Acquire)
    }

    fn set_attached(&self, value: bool) {
        self.attached.store(value, Ordering::Release)
    }
}

pub(crate) struct TearingControlUserData(Mutex<Weak<WlSurface>>);

impl TearingControlUserData {
    fn new(surface: WlSurface) -> Self {
        Self(Mutex::new(surface.downgrade()))
    }

    fn wl_surface(&self) -> Option<WlSurface> {
        self.0.lock().unwrap().upgrade().ok()
    }
}

impl GlobalDispatch<WpTearingControlManagerV1, ()> for Smallvil {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WpTearingControlManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpTearingControlManagerV1, ()> for Smallvil {
    fn request(
        _state: &mut Self,
        _client: &Client,
        manager: &WpTearingControlManagerV1,
        request: wp_tearing_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_tearing_control_manager_v1::Request::GetTearingControl { id, surface } => {
                let already_attached = compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(TearingSurfaceData::default);
                    let data = states.data_map.get::<TearingSurfaceData>().unwrap();
                    let already_attached = data.attached();
                    if !already_attached {
                        data.set_attached(true);
                    }
                    already_attached
                });

                if already_attached {
                    manager.post_error(
                        wp_tearing_control_manager_v1::Error::TearingControlExists,
                        "wl_surface already has a wp_tearing_control_v1 object",
                    );
                } else {
                    data_init.init(id, TearingControlUserData::new(surface));
                }
            }
            wp_tearing_control_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WpTearingControlV1, TearingControlUserData> for Smallvil {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpTearingControlV1,
        request: wp_tearing_control_v1::Request,
        data: &TearingControlUserData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_tearing_control_v1::Request::SetPresentationHint { hint } => {
                let WEnum::Value(hint) = hint else {
                    return;
                };
                let Some(surface) = data.wl_surface() else {
                    return;
                };
                compositor::with_states(&surface, |states| {
                    states
                        .cached_state
                        .get::<TearingSurfaceCachedState>()
                        .pending()
                        .hint = hint;
                });
            }
            // Reverts to vsync, applied on the next commit like the spec
            // requires, and frees the slot so a client can re-attach.
            wp_tearing_control_v1::Request::Destroy => {
                let Some(surface) = data.wl_surface() else {
                    return;
                };
                compositor::with_states(&surface, |states| {
                    if let Some(data) = states.data_map.get::<TearingSurfaceData>() {
                        data.set_attached(false);
                    }
                    states
                        .cached_state
                        .get::<TearingSurfaceCachedState>()
                        .pending()
                        .hint = PresentationHint::Vsync;
                });
            }
            _ => unreachable!(),
        }
    }
}
