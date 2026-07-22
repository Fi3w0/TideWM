//! wlr-output-management-unstable-v1: lets kanshi, `wlr-randr`, and wdisplays
//! read and change output layout at runtime. Hand-rolled on
//! `wayland-protocols-wlr`: unlike most other protocols this codebase
//! implements, Smithay 0.7 ships no convenience module for this one.
//!
//! Deliberately scoped for this first pass. Heads are fully advertised
//! (name/description/physical_size/mode/enabled/current_mode/position/
//! transform/scale, matching what `Output` and `Space` already track), and
//! `apply`/`test` genuinely live-applies position/transform/scale changes
//! to an already-enabled head -- pure `Output::change_current_state` +
//! `Space::map_output` bookkeeping, no DRM involved, and actually
//! nested-testable (a winit-backed output has no DRM at all, so this path
//! runs identically under both backends).
//!
//! What this does NOT support yet: disabling a head, or changing its mode
//! (resolution/refresh) to anything other than what it's already running.
//! Both would need a real DRM modeset renegotiation on the udev backend --
//! untestable in this sandbox (no real hardware) and the same risk class as
//! `TileMoveGrab`'s 0.15.1 machine-freeze incident (shipped after only a
//! code review, froze the entire machine on first real-hardware use). The
//! protocol explicitly allows a compositor to fail any apply/test for any
//! reason ("a compositor might round the scale if it doesn't support
//! fractional scaling"), so an honest `failed` event is the correct,
//! spec-compliant response here, not a workaround. Revisit once real
//! hardware is available to verify a live modeset actually works.
//!
//! Only one mode is ever advertised per head: the current one, marked
//! preferred. `Output` doesn't track a connector's full list of
//! hardware-supported modes outside `backend/udev.rs`'s local DRM scan, and
//! since mode changes are refused anyway, advertising modes a client
//! couldn't actually switch to would just be misleading.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use smithay::desktop::{Space, Window};
use smithay::output::{Output, Scale};
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    protocol::wl_output::Transform as WlTransform,
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use smithay::utils::{Logical, Point, Rectangle, Transform};
use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use crate::Smallvil;

/// Highest version this hand-rolled implementation advertises. v4 only adds
/// adaptive_sync (VRR), which TideWM doesn't support on any backend yet;
/// v3's head `release` request is otherwise the newest thing clients need.
const VERSION: u32 = 3;

fn wl_transform_to_smithay(t: WlTransform) -> Transform {
    match t {
        WlTransform::Normal => Transform::Normal,
        WlTransform::_90 => Transform::_90,
        WlTransform::_180 => Transform::_180,
        WlTransform::_270 => Transform::_270,
        WlTransform::Flipped => Transform::Flipped,
        WlTransform::Flipped90 => Transform::Flipped90,
        WlTransform::Flipped180 => Transform::Flipped180,
        WlTransform::Flipped270 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

struct HeadResources {
    manager: ZwlrOutputManagerV1,
    head: ZwlrOutputHeadV1,
    mode: ZwlrOutputModeV1,
}

struct TrackedHead {
    output: Output,
    resources: Vec<HeadResources>,
}

/// Global state: every bound `zwlr_output_manager_v1` instance and every
/// live head's per-client resources, so a hotplug or a live property change
/// can be replayed/broadcast to all of them.
pub struct WlrOutputManagementState {
    #[allow(dead_code)]
    global: GlobalId,
    dh: DisplayHandle,
    instances: Vec<ZwlrOutputManagerV1>,
    heads: Vec<TrackedHead>,
    current_serial: u32,
}

impl WlrOutputManagementState {
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<Smallvil, ZwlrOutputManagerV1, ()>(VERSION, ());
        Self {
            global,
            dh: dh.clone(),
            instances: Vec::new(),
            heads: Vec::new(),
            current_serial: 0,
        }
    }

    /// Reconciles tracked heads against `space`'s live outputs, creates
    /// resources for any head a bound manager hasn't seen yet, resends
    /// every live head's current dynamic properties (enabled/current_mode/
    /// position/transform/scale -- never the static ones, which don't
    /// change over a head's lifetime and are only sent once, at creation),
    /// sends `finished` for heads that disappeared, and finally broadcasts
    /// a fresh `done` serial. Call after any hotplug (map_output/
    /// unmap_output) or after a live output-management apply.
    pub fn refresh(&mut self, space: &Space<Window>) {
        let outputs: Vec<Output> = space.outputs().cloned().collect();

        for output in &outputs {
            if !self.heads.iter().any(|h| &h.output == output) {
                self.heads.push(TrackedHead {
                    output: output.clone(),
                    resources: Vec::new(),
                });
            }
        }

        self.heads.retain(|h| {
            let live = outputs.contains(&h.output);
            if !live {
                for r in &h.resources {
                    r.head.finished();
                }
            }
            live
        });

        for head in &mut self.heads {
            for manager in &self.instances {
                if !head.resources.iter().any(|r| &r.manager == manager) {
                    if let Some(r) = create_head_resources(&self.dh, manager, &head.output) {
                        send_static_head_properties(&r, &head.output);
                        head.resources.push(r);
                    }
                }
            }
        }

        for head in &self.heads {
            let geo = space.output_geometry(&head.output);
            for r in &head.resources {
                send_dynamic_head_properties(r, &head.output, geo);
            }
        }

        self.current_serial = self.current_serial.wrapping_add(1);
        for manager in &self.instances {
            manager.done(self.current_serial);
        }
    }
}

/// Creates the per-client head+mode resource pair for `output` and sends
/// the `head`/`mode` introduction events. Property events are the caller's
/// job (see `send_static_head_properties`/`send_dynamic_head_properties`).
fn create_head_resources(
    dh: &DisplayHandle,
    manager: &ZwlrOutputManagerV1,
    output: &Output,
) -> Option<HeadResources> {
    let client = dh.get_client(manager.id()).ok()?;
    let head = client
        .create_resource::<ZwlrOutputHeadV1, Output, Smallvil>(
            dh,
            manager.version(),
            output.clone(),
        )
        .ok()?;
    manager.head(&head);
    let mode = client
        .create_resource::<ZwlrOutputModeV1, Output, Smallvil>(
            dh,
            manager.version(),
            output.clone(),
        )
        .ok()?;
    head.mode(&mode);
    Some(HeadResources {
        manager: manager.clone(),
        head,
        mode,
    })
}

/// Name/description/physical_size/make/model/serial_number (and the one
/// mode's size/refresh/preferred) -- sent once, right after creation. Per
/// spec these never change over a head's lifetime.
fn send_static_head_properties(r: &HeadResources, output: &Output) {
    r.head.name(output.name());
    r.head.description(output.description());
    let props = output.physical_properties();
    if props.size.w > 0 && props.size.h > 0 {
        r.head.physical_size(props.size.w, props.size.h);
    }
    if r.head.version() >= 2 {
        r.head.make(props.make.clone());
        r.head.model(props.model.clone());
        r.head.serial_number(props.serial_number.clone());
    }
    if let Some(mode) = output.current_mode() {
        r.mode.size(mode.size.w, mode.size.h);
        r.mode.refresh(mode.refresh);
        r.mode.preferred();
    }
}

/// enabled/current_mode/position/transform/scale -- resent on every
/// `refresh()`, since these are exactly the properties output-management
/// apply can change live.
fn send_dynamic_head_properties(
    r: &HeadResources,
    output: &Output,
    geo: Option<Rectangle<i32, Logical>>,
) {
    // TideWM never tracks a head that isn't currently mapped (see the
    // module doc comment), so this is always enabled.
    r.head.enabled(1);
    if output.current_mode().is_some() {
        r.head.current_mode(&r.mode);
    }
    if let Some(geo) = geo {
        r.head.position(geo.loc.x, geo.loc.y);
    }
    r.head.transform(output.current_transform().into());
    r.head.scale(output.current_scale().fractional_scale());
}

impl GlobalDispatch<ZwlrOutputManagerV1, ()> for Smallvil {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let mgmt = &mut state.wlr_output_management_state;
        for head in &mut mgmt.heads {
            if let Some(r) = create_head_resources(dh, &manager, &head.output) {
                send_static_head_properties(&r, &head.output);
                head.resources.push(r);
            }
        }
        // Dynamic properties for the freshly-created resources: needs
        // `state.space` for position, which isn't reachable from `mgmt`
        // alone, so this is a second pass over the same heads.
        for head in &state.wlr_output_management_state.heads {
            let geo = state.space.output_geometry(&head.output);
            if let Some(r) = head.resources.last() {
                send_dynamic_head_properties(r, &head.output, geo);
            }
        }
        let mgmt = &mut state.wlr_output_management_state;
        mgmt.current_serial = mgmt.current_serial.wrapping_add(1);
        manager.done(mgmt.current_serial);
        mgmt.instances.push(manager);
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    ConfigurationData(Arc::new(Mutex::new(ConfigurationInner {
                        created_serial: serial,
                        used: false,
                        ops: HashMap::new(),
                    }))),
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                manager.finished();
                state
                    .wlr_output_management_state
                    .instances
                    .retain(|m| m != manager);
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &ZwlrOutputManagerV1, _data: &()) {
        state
            .wlr_output_management_state
            .instances
            .retain(|m| m != resource);
    }
}

impl Dispatch<ZwlrOutputHeadV1, Output> for Smallvil {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &Output,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Only request is `release` (destructor); nothing to do beyond the
        // `destroyed` cleanup below.
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &ZwlrOutputHeadV1, _data: &Output) {
        for head in &mut state.wlr_output_management_state.heads {
            head.resources.retain(|r| &r.head != resource);
        }
    }
}

impl Dispatch<ZwlrOutputModeV1, Output> for Smallvil {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &Output,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

/// One requested change to a single head within an in-flight configuration
/// transaction. `Disabled` is always unsupported for v1 (see module doc);
/// kept as its own variant rather than folded into `Enabled` so
/// `finish_configuration` can flag it without needing a sentinel value.
enum HeadOp {
    Disabled,
    Enabled(EnabledHeadConfig),
}

#[derive(Default)]
struct EnabledHeadConfig {
    position: Option<(i32, i32)>,
    transform: Option<Transform>,
    scale: Option<f64>,
    /// Set by `set_custom_mode`; `None` means "keep the current mode".
    /// `set_mode` never populates this: the only mode we ever advertise
    /// *is* the head's current one, so referencing it back is always a
    /// no-op by construction (see module doc).
    custom_mode: Option<(i32, i32, i32)>,
    mode_assigned: bool,
    position_set: bool,
    transform_set: bool,
    scale_set: bool,
}

struct ConfigurationInner {
    /// The serial the client passed to `create_configuration`, compared
    /// against `WlrOutputManagementState::current_serial` at apply/test
    /// time. A mismatch means the output state changed after this
    /// configuration was built -- the race-safety valve the protocol's own
    /// `cancelled` event exists for, no other validation needed.
    created_serial: u32,
    used: bool,
    ops: HashMap<Output, HeadOp>,
}

#[derive(Clone)]
struct ConfigurationData(Arc<Mutex<ConfigurationInner>>);

struct ConfigHeadData {
    config: ConfigurationData,
    /// `None` if the head vanished (disconnected) between the client
    /// learning its id and this `enable_head` request arriving. Every
    /// request on the resulting resource is then a harmless no-op; the
    /// client sees the race play out as `cancelled` at apply/test time via
    /// the stale-serial check, same as any other hotplug race.
    output: Option<Output>,
}

impl Dispatch<ZwlrOutputConfigurationV1, ConfigurationData> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &ConfigurationData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_configuration_v1::{Error, Request};

        match request {
            Request::EnableHead { id, head } => {
                let output = head.data::<Output>().cloned();
                data_init.init(
                    id,
                    ConfigHeadData {
                        config: data.clone(),
                        output: output.clone(),
                    },
                );
                let Some(output) = output else {
                    return;
                };
                let mut inner = data.0.lock().unwrap();
                if inner.used {
                    resource.post_error(Error::AlreadyUsed, "enable_head after apply/test");
                    return;
                }
                if inner.ops.contains_key(&output) {
                    resource.post_error(Error::AlreadyConfiguredHead, "head already configured");
                    return;
                }
                inner
                    .ops
                    .insert(output, HeadOp::Enabled(EnabledHeadConfig::default()));
            }
            Request::DisableHead { head } => {
                let Some(output) = head.data::<Output>().cloned() else {
                    return;
                };
                let mut inner = data.0.lock().unwrap();
                if inner.used {
                    resource.post_error(Error::AlreadyUsed, "disable_head after apply/test");
                    return;
                }
                if inner.ops.contains_key(&output) {
                    resource.post_error(Error::AlreadyConfiguredHead, "head already configured");
                    return;
                }
                inner.ops.insert(output, HeadOp::Disabled);
            }
            Request::Apply => finish_configuration(state, resource, data, true),
            Request::Test => finish_configuration(state, resource, data, false),
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData> for Smallvil {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &ConfigHeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_configuration_head_v1::{Error, Request};

        let Some(output) = &data.output else {
            return;
        };
        let mut inner = data.config.0.lock().unwrap();
        let Some(HeadOp::Enabled(cfg)) = inner.ops.get_mut(output) else {
            return;
        };

        match request {
            Request::SetMode { mode } => {
                if cfg.mode_assigned {
                    resource.post_error(Error::AlreadySet, "mode already set");
                    return;
                }
                if mode.data::<Output>() != Some(output) {
                    resource.post_error(Error::InvalidMode, "mode does not belong to this head");
                    return;
                }
                cfg.mode_assigned = true;
            }
            Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                if cfg.mode_assigned {
                    resource.post_error(Error::AlreadySet, "mode already set");
                    return;
                }
                if width <= 0 || height <= 0 {
                    resource.post_error(Error::InvalidCustomMode, "width/height must be positive");
                    return;
                }
                cfg.mode_assigned = true;
                cfg.custom_mode = Some((width, height, refresh));
            }
            Request::SetPosition { x, y } => {
                if cfg.position_set {
                    resource.post_error(Error::AlreadySet, "position already set");
                    return;
                }
                cfg.position_set = true;
                cfg.position = Some((x, y));
            }
            Request::SetTransform { transform } => {
                if cfg.transform_set {
                    resource.post_error(Error::AlreadySet, "transform already set");
                    return;
                }
                let WEnum::Value(transform) = transform else {
                    resource.post_error(Error::InvalidTransform, "transform outside enum range");
                    return;
                };
                cfg.transform_set = true;
                cfg.transform = Some(wl_transform_to_smithay(transform));
            }
            Request::SetScale { scale } => {
                if cfg.scale_set {
                    resource.post_error(Error::AlreadySet, "scale already set");
                    return;
                }
                if scale <= 0.0 {
                    resource.post_error(Error::InvalidScale, "scale must be positive");
                    return;
                }
                cfg.scale_set = true;
                cfg.scale = Some(scale);
            }
            _ => {}
        }
    }
}

/// Shared `apply`/`test` handling: validates the transaction (already-used,
/// stale serial, every live head covered), decides whether the whole batch
/// is within what this first pass supports, and -- only for `apply`, and
/// only if supported -- actually mutates `Output`/`Space` state before
/// replying. Never applies a partial batch: either everything requested is
/// supported and all of it lands, or none of it does and the client gets
/// `failed`.
fn finish_configuration(
    state: &mut Smallvil,
    resource: &ZwlrOutputConfigurationV1,
    data: &ConfigurationData,
    is_apply: bool,
) {
    use zwlr_output_configuration_v1::Error;

    let mut inner = data.0.lock().unwrap();
    if inner.used {
        resource.post_error(Error::AlreadyUsed, "apply/test already sent");
        return;
    }
    inner.used = true;

    if inner.created_serial != state.wlr_output_management_state.current_serial {
        resource.cancelled();
        return;
    }

    let live_heads: Vec<Output> = state.space.outputs().cloned().collect();
    if live_heads.iter().any(|o| !inner.ops.contains_key(o)) {
        resource.post_error(Error::UnconfiguredHead, "not every head was configured");
        return;
    }
    if inner.ops.keys().any(|o| !live_heads.contains(o)) {
        // A configured head disconnected mid-transaction.
        resource.cancelled();
        return;
    }

    let supported = inner.ops.iter().all(|(output, op)| match op {
        HeadOp::Disabled => false,
        HeadOp::Enabled(cfg) => match cfg.custom_mode {
            None => true,
            Some((w, h, r)) => output
                .current_mode()
                .is_some_and(|m| m.size.w == w && m.size.h == h && (r == 0 || m.refresh == r)),
        },
    });

    if !supported {
        resource.failed();
        return;
    }

    if is_apply {
        for (output, op) in inner.ops.iter() {
            let HeadOp::Enabled(cfg) = op else { continue };
            if cfg.transform.is_none() && cfg.scale.is_none() && cfg.position.is_none() {
                continue;
            }
            let scale = cfg.scale.map(Scale::Fractional);
            let old_position = state.space.output_geometry(output).map(|geo| geo.loc);
            output.change_current_state(None, cfg.transform, scale, cfg.position.map(Into::into));
            if let Some(pos) = cfg.position {
                state.space.map_output(output, pos);
                // retile() below repositions tiled windows for free (their
                // tree is recomputed against the output's fresh area);
                // floating windows have no equivalent automatic step, so
                // without this they'd keep their old absolute coordinates
                // -- possibly landing on a different output or off-screen
                // -- even though this apply is about to report success.
                if let Some(old_position) = old_position {
                    let delta: Point<i32, Logical> = Point::from(pos) - old_position;
                    if delta != (0, 0).into() {
                        state.translate_floating_windows_on_output(&output.name(), delta);
                    }
                }
            }
        }
        state.retile();
        state.wlr_output_management_state.refresh(&state.space);
    }

    resource.succeeded();
}
