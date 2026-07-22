//! wlr-output-power-management-unstable-v1: lets a client (wlogout-style
//! "turn off displays" tools, a QuickShell power widget) turn an output's
//! power on/off. Hand-rolled on `wayland-protocols-wlr`, same reason as
//! `wlr_output_management.rs` -- no Smithay module for it.
//!
//! Real DRM CRTC power control only exists on the udev backend
//! (`Smallvil::set_output_power`, installed by `backend::udev::init_udev`
//! once its device `Rc` exists, `None` under winit). A nested winit output
//! has no real display to power down, so a request there is tracked as
//! logical bookkeeping only and always succeeds -- the `mode` event
//! faithfully reflects what was asked, it just has no visible effect, the
//! same "protocol correct, physical effect needs real hardware" tier as
//! several other udev-only claims in this project.

use std::collections::HashMap;

use smithay::output::Output;
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
};

use crate::Smallvil;

/// Per-(output, client) control resource's user data. `None` means the
/// `wl_output` passed to `get_output_power` didn't resolve to a live
/// `Output` (a stale/foreign resource) -- the control was already sent
/// `failed` at creation, so every request on it is a harmless no-op.
struct ControlData(Option<Output>);

pub struct WlrOutputPowerManagementState {
    #[allow(dead_code)]
    global: GlobalId,
    /// Logical on/off per output, independent of whether the udev hook
    /// actually confirmed a real CRTC change -- read back on a fresh
    /// `get_output_power` bind so a second client sees the state the first
    /// one set, and defaults to "on" for an output never touched.
    power: HashMap<Output, bool>,
    /// Live per-(output, client) control resources, so a mode change from
    /// any source broadcasts to everyone watching that output, not just
    /// whichever client issued `set_mode` -- matches the protocol's own
    /// "the reason can be a client using set_mode or the compositor
    /// deciding to change an output's mode" framing.
    controls: Vec<(Output, ZwlrOutputPowerV1)>,
}

impl WlrOutputPowerManagementState {
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<Smallvil, ZwlrOutputPowerManagerV1, ()>(1, ());
        Self {
            global,
            power: HashMap::new(),
            controls: Vec::new(),
        }
    }

    pub fn is_on(&self, output: &Output) -> bool {
        self.power.get(output).copied().unwrap_or(true)
    }

    /// Records `output`'s tracked power state and broadcasts it to every
    /// client watching that output -- the two steps `SetMode` below and
    /// `Smallvil::toggle_dpms` (the bindable `toggle-dpms` action, Phase N
    /// tier 2) both need, factored out so there's one place that does them
    /// together rather than two copies drifting apart.
    pub fn set(&mut self, output: &Output, on: bool) {
        self.power.insert(output.clone(), on);
        self.broadcast(output, on);
    }

    fn broadcast(&self, output: &Output, on: bool) {
        let mode = if on { Mode::On } else { Mode::Off };
        for (o, control) in &self.controls {
            if o == output {
                control.mode(mode);
            }
        }
    }

    /// Forces `output` to tracked "on" and broadcasts it, without going
    /// through the backend hook -- for the one case where the backend
    /// itself unconditionally re-enables an output out from under any
    /// wlr-output-power-management-v1 state (a VT-switch back always
    /// wakes every CRTC, see `backend/udev.rs`'s `ActivateSession`
    /// handler). A no-op if `output` was already tracked as on.
    pub fn force_on(&mut self, output: &Output) {
        if self.power.insert(output.clone(), true) != Some(false) {
            return;
        }
        self.broadcast(output, true);
    }

    /// Drops tracked state for a disconnected output. A reconnected
    /// monitor with the same connector name gets a brand-new `Output`
    /// object (fresh identity, `Output::new` wraps a fresh `Arc` every
    /// time), so without this a disconnect/reconnect cycle would leak one
    /// stale entry per cycle rather than just start that output fresh
    /// (defaulting back to "on", the same as any output seen for the
    /// first time).
    pub fn output_removed(&mut self, output: &Output) {
        self.power.remove(output);
        self.controls.retain(|(o, _)| o != output);
    }
}

impl GlobalDispatch<ZwlrOutputPowerManagerV1, ()> for Smallvil {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } => {
                let target = Output::from_resource(&output);
                let control = data_init.init(id, ControlData(target.clone()));
                match target {
                    Some(output) => {
                        let on = state.wlr_output_power_management_state.is_on(&output);
                        control.mode(if on { Mode::On } else { Mode::Off });
                        state
                            .wlr_output_power_management_state
                            .controls
                            .push((output, control));
                    }
                    // Per spec, `failed` covers "the output disappeared";
                    // a `wl_output` that never resolved to a live `Output`
                    // at all is the same "nothing to control" situation
                    // from the client's point of view.
                    None => control.failed(),
                }
            }
            zwlr_output_power_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputPowerV1, ControlData> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        data: &ControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(output) = &data.0 else { return };

        match request {
            zwlr_output_power_v1::Request::SetMode { mode } => {
                let WEnum::Value(mode) = mode else {
                    resource.post_error(
                        zwlr_output_power_v1::Error::InvalidMode,
                        "power mode outside enum range",
                    );
                    return;
                };
                let want_on = matches!(mode, Mode::On);
                let applied = match &mut state.set_output_power {
                    Some(hook) => hook(output, want_on),
                    // No backend hook (winit): logical-only, always succeeds.
                    None => true,
                };
                if applied {
                    state.wlr_output_power_management_state.set(output, want_on);
                } else {
                    resource.failed();
                }
            }
            zwlr_output_power_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrOutputPowerV1,
        _data: &ControlData,
    ) {
        state
            .wlr_output_power_management_state
            .controls
            .retain(|(_, r)| r != resource);
    }
}
