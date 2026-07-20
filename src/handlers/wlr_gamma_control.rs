//! zwlr_gamma_control_manager_v1: per-output gamma ramp control (wlsunset/
//! gammastep night-light). Hand-rolled on `wayland-protocols-wlr`, same
//! reason as the other two wlr- protocols in this project -- no Smithay
//! module for it.
//!
//! Real DRM gamma-LUT access only exists on the udev backend
//! (`Smallvil::gamma_size`/`set_gamma`, installed by
//! `backend::udev::init_udev`, both `None` under winit). A nested winit
//! output has no real color pipeline to adjust at all -- unlike DPMS,
//! there's no meaningful "logical-only" fallback here, so a request under
//! winit is refused outright with `failed`, matching the protocol's own
//! "the output doesn't support gamma tables" reason.

use std::collections::HashMap;

use smithay::output::Output;
use smithay::reexports::rustix;
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

use crate::Smallvil;

/// `Some((output, gamma_size))` once resolved at creation; `None` if the
/// `wl_output` never resolved to a live `Output`, or the backend has no
/// gamma support -- either way `failed` was already sent at creation, so
/// every request on the resource after that is a harmless no-op.
struct ControlData(Option<(Output, u32)>);

/// At most one live control per output. A second `get_gamma_control` for
/// an already-controlled output transfers control to the new client,
/// sending `failed` to whoever held it before -- matches the protocol's own
/// "the compositor has transferred gamma control to another client" reason,
/// and the actual behavior wlsunset/gammastep expect when one replaces the
/// other (e.g. a restart).
pub struct WlrGammaControlState {
    #[allow(dead_code)]
    global: GlobalId,
    controls: HashMap<Output, ZwlrGammaControlV1>,
}

impl WlrGammaControlState {
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<Smallvil, ZwlrGammaControlManagerV1, ()>(1, ());
        Self { global, controls: HashMap::new() }
    }
}

/// A linear (identity) ramp of `size` steps -- what a freshly-booted CRTC's
/// gamma LUT already looks like, and what "restore the original value" (the
/// protocol's own wording for what happens on control destroy) means in
/// practice: this project doesn't read back the hardware's own ramp before
/// a client first touches it, matching wlroots' own reference behavior of
/// resetting to linear on last-client-disconnect rather than snapshotting.
fn identity_ramp(size: u32) -> Vec<u16> {
    let last = size.saturating_sub(1).max(1) as u64;
    (0..size)
        .map(|i| ((i as u64 * u16::MAX as u64) / last) as u16)
        .collect()
}

impl GlobalDispatch<ZwlrGammaControlManagerV1, ()> for Smallvil {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = request else {
            return;
        };

        let target = Output::from_resource(&output);
        let size = target
            .as_ref()
            .and_then(|o| state.gamma_size.as_mut().and_then(|hook| hook(o)));

        let control = data_init.init(id, ControlData(target.clone().zip(size)));

        match (target, size) {
            (Some(output), Some(size)) => {
                if let Some(old) = state.wlr_gamma_control_state.controls.insert(output, control.clone())
                {
                    old.failed();
                }
                control.gamma_size(size);
            }
            _ => control.failed(),
        }
    }
}

impl Dispatch<ZwlrGammaControlV1, ControlData> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        data: &ControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some((output, size)) = &data.0 else { return };
        let zwlr_gamma_control_v1::Request::SetGamma { fd } = request else {
            return;
        };

        let size = *size as usize;
        let mut buf = vec![0u8; size * 3 * std::mem::size_of::<u16>()];
        let mut filled = 0;
        while filled < buf.len() {
            match rustix::io::read(&fd, &mut buf[filled..]) {
                Ok(0) => break, // short: client sent less than size*3 u16s
                Ok(n) => filled += n,
                Err(e) => {
                    tracing::warn!(%e, "Failed to read gamma table fd");
                    break;
                }
            }
        }
        if filled != buf.len() {
            resource.post_error(
                zwlr_gamma_control_v1::Error::InvalidGamma,
                "gamma table fd did not contain exactly size*3 u16 values",
            );
            return;
        }

        let channel = |bytes: &[u8]| -> Vec<u16> {
            bytes.chunks_exact(2).map(|c| u16::from_ne_bytes([c[0], c[1]])).collect()
        };
        let red = channel(&buf[..size * 2]);
        let green = channel(&buf[size * 2..size * 4]);
        let blue = channel(&buf[size * 4..size * 6]);

        let applied = state
            .set_gamma
            .as_mut()
            .map(|hook| hook(output, &red, &green, &blue))
            .unwrap_or(false);
        if !applied {
            resource.failed();
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &ZwlrGammaControlV1, data: &ControlData) {
        let Some((output, size)) = &data.0 else { return };
        // Only reset gamma if this resource is still the tracked owner --
        // if a newer client already transferred control away (see
        // `GetGammaControl` above), the map entry no longer points at
        // `resource`, and resetting here would incorrectly clobber
        // whatever the new owner has already set.
        let is_current_owner = state.wlr_gamma_control_state.controls.get(output) == Some(resource);
        if !is_current_owner {
            return;
        }
        state.wlr_gamma_control_state.controls.remove(output);
        if let Some(hook) = state.set_gamma.as_mut() {
            let ramp = identity_ramp(*size);
            hook(output, &ramp, &ramp, &ramp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::identity_ramp;

    #[test]
    fn identity_ramp_spans_full_range_monotonically() {
        let ramp = identity_ramp(256);
        assert_eq!(ramp.len(), 256);
        assert_eq!(ramp[0], 0);
        assert_eq!(ramp[255], u16::MAX);
        assert!(ramp.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn identity_ramp_handles_degenerate_sizes() {
        assert_eq!(identity_ramp(0), Vec::<u16>::new());
        assert_eq!(identity_ramp(1), vec![0]);
    }
}
