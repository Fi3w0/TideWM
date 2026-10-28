use super::*;

/// Adaptive-sync (VRR) preference -- niri's three-way shape
/// (`variable-refresh-rate off|on|on-demand`). The udev render path applies
/// this through Smithay's `DrmCompositor::use_vrr`; `OnDemand` enables VRR
/// only while a fullscreen window owns the output. The nested winit backend
/// has no DRM surface, so the setting is intentionally inert there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdaptiveSync {
    #[default]
    Off,
    On,
    OnDemand,
}

#[derive(Debug, Clone)]
pub struct InputConfig {
    pub repeat_delay: i32,
    pub repeat_rate: i32,
    pub focus_follows_mouse: bool,
    /// Clear keyboard focus when hover focus reaches empty desktop/canvas.
    /// False preserves the last focused window while crossing gaps.
    pub unfocus_on_empty: bool,
    /// xkbcommon rules/model/layout/variant/options. Empty string / `None`
    /// (the default for each) means "let xkbcommon fall back to the
    /// `XKB_DEFAULT_*` env vars," same as `XkbConfig::default()` -- so an
    /// empty `input { }` block changes nothing versus today.
    pub xkb_rules: String,
    pub xkb_model: String,
    pub xkb_layout: String,
    pub xkb_variant: String,
    pub xkb_options: Option<String>,
    pub touchpad: TouchpadConfig,
    /// Per-libinput-device overrides selected by the exact descriptive
    /// device name. Matching blocks fold in config order, so a later block
    /// can replace one field without repeating the other.
    pub devices: Vec<InputDeviceConfig>,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            repeat_delay: 200,
            repeat_rate: 25,
            focus_follows_mouse: true,
            unfocus_on_empty: false,
            xkb_rules: String::new(),
            xkb_model: String::new(),
            xkb_layout: String::new(),
            xkb_variant: String::new(),
            xkb_options: None,
            touchpad: TouchpadConfig::default(),
            devices: Vec::new(),
        }
    }
}

impl InputConfig {
    /// Builds the `XkbConfig` to hand to `Seat::add_keyboard`/
    /// `KeyboardHandle::set_xkb_config`. Empty fields behave exactly like
    /// `XkbConfig::default()` (xkbcommon's own env-var fallback) since
    /// that's what an unset `String`/`None` field already is.
    pub fn xkb_config(&self) -> XkbConfig<'_> {
        XkbConfig {
            rules: &self.xkb_rules,
            model: &self.xkb_model,
            layout: &self.xkb_layout,
            variant: &self.xkb_variant,
            options: self.xkb_options.clone(),
        }
    }

    pub(crate) fn resolve_device(&self, name: &str) -> InputDeviceConfig {
        InputDeviceConfig::resolve(&self.devices, name)
    }
}

impl InputDeviceConfig {
    pub(crate) fn resolve(devices: &[Self], name: &str) -> Self {
        let mut resolved = InputDeviceConfig {
            name: name.to_string(),
            ..Default::default()
        };
        for device in devices.iter().filter(|device| device.name == name) {
            if let Some(output) = &device.map_to_output {
                resolved.map_to_output = Some(output.clone());
            }
            if let Some(matrix) = device.calibration_matrix {
                resolved.calibration_matrix = Some(matrix);
            }
        }
        resolved
    }
}

/// One exact-name libinput device selector inside `input { }`.
///
/// Output mapping is a compositor coordinate transform. Calibration is
/// handed to libinput, which applies the six-element affine matrix before
/// TideWM receives absolute events. Both are opt-in so an unmatched or
/// unconfigured device keeps the existing input path unchanged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputDeviceConfig {
    pub name: String,
    pub map_to_output: Option<String>,
    pub calibration_matrix: Option<[f32; 6]>,
}

/// Global libinput touchpad settings (udev backend only -- winit's nested
/// host input never reaches a real libinput device, so these just sit
/// unused there). Applied once per touchpad-capable device
/// (`config_tap_finger_count() > 0`, the same "is this a touchpad" check
/// sway/Hyprland use) when libinput reports it, see
/// `input::apply_touchpad_config`.
///
/// Every field is `Option`, left untouched (`None`) unless set -- matches
/// libinput's own "unconfigured means driver/hardware default" behavior
/// instead of silently forcing a value nobody asked for. One set of
/// settings for every touchpad, not per-device overrides; if a machine with
/// more than one touchpad needing different settings ever comes up, that's
/// the upgrade path.
///
/// Re-applied to every already-known touchpad on a config reload too
/// (`InputState::known_input_devices`, populated/pruned from `DeviceAdded`/
/// `DeviceRemoved` in `backend/udev.rs`), so editing this section reaches
/// a laptop's built-in touchpad, not just one plugged in after the edit.
#[derive(Debug, Clone, Default)]
pub struct TouchpadConfig {
    pub tap_to_click: Option<bool>,
    pub tap_and_drag: Option<bool>,
    pub drag_lock: Option<bool>,
    pub disable_while_typing: Option<bool>,
    pub natural_scroll: Option<bool>,
    pub left_handed: Option<bool>,
    pub middle_emulation: Option<bool>,
    /// `"button-areas"` or `"clickfinger"`. Unrecognized value: warned and
    /// ignored, same forgiving convention as `parse_layout_algorithm`.
    pub click_method: Option<String>,
    /// `"two-finger"`, `"edge"`, `"on-button-down"`, or `"none"`.
    pub scroll_method: Option<String>,
    /// -1.0 (slowest) .. 1.0 (fastest).
    pub accel_speed: Option<f64>,
    /// `"adaptive"` or `"flat"`.
    pub accel_profile: Option<String>,
    /// When set to 3 or more, horizontal swipes with this finger count are
    /// consumed by TideWM to change workspace instead of being forwarded to
    /// the focused client. Unset keeps client-only gesture forwarding.
    pub workspace_swipe_fingers: Option<u32>,
    /// Horizontal libinput delta required before the configured workspace
    /// swipe commits. Defaults to 200.0 when omitted.
    pub workspace_swipe_distance: Option<f64>,
    /// Optional compositor actions for each completed swipe direction. When
    /// any is set, a swipe with `gesture_swipe_fingers` is consumed by
    /// TideWM; an unbound direction is simply ignored.
    pub gesture_swipe_fingers: Option<u32>,
    pub swipe_left: Option<Action>,
    pub swipe_right: Option<Action>,
    pub swipe_up: Option<Action>,
    pub swipe_down: Option<Action>,
    /// Pinch bindings use the final scale reported by libinput: below 0.8
    /// is `pinch_in`, above 1.2 is `pinch_out`.
    pub gesture_pinch_fingers: Option<u32>,
    pub pinch_in: Option<Action>,
    pub pinch_out: Option<Action>,
    /// When set, a swipe with this many fingers held together with
    /// `pointer_modifier` moves/pans the same way `pointer_modifier`+
    /// left-drag does, with the touch itself standing in for the button
    /// press: over a window it's picked up (tiled or floating, same
    /// decision the mouse path makes), over empty Ocean canvas it pans the
    /// camera. Unset (the default) leaves the gesture unclaimed.
    pub modifier_pan_fingers: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct XwaylandConfig {
    pub enabled: bool,
    pub path: String,
}

impl Default for XwaylandConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "xwayland-satellite".to_string(),
        }
    }
}

/// Per-output override, matched against the udev backend's connector name
/// (e.g. `"eDP-1"`, `"DP-2"` -- see `connector_type_name` in
/// `backend/udev.rs`). Purely opt-in: an output with no matching entry
/// keeps the existing auto behavior (preferred mode, auto-positioned to
/// the right of whatever's already mapped, scale 1, no rotation). Only
/// applies to the udev backend -- winit's single simulated output has no
/// real mode list to pick from and its transform is a rendering-pipeline
/// requirement, not a monitor-orientation choice, so overriding it isn't
/// meaningful the way it is for a real connector.
#[derive(Debug, Clone)]
pub struct OutputConfig {
    pub name: String,
    pub enabled: bool,
    /// `"1920x1080"` or `"1920x1080@60"`. Falls back to the connector's own
    /// preferred mode if unset or if nothing matches.
    pub mode: Option<String>,
    /// Falls back to auto-layout (rightmost edge of already-mapped
    /// outputs) if unset.
    pub position: Option<(i32, i32)>,
    pub scale: f64,
    pub transform: OutputTransformConfig,
    /// Per-output gap override; `None` falls back to the global `gaps`.
    /// A `workspace_gaps` entry beats both (see `Smallvil::gaps_for`).
    pub gaps: Option<i32>,
    /// Per-output adaptive-sync override; `None` falls back to the global
    /// `adaptive_sync`. See `AdaptiveSync`'s own doc for current scope.
    pub adaptive_sync: Option<AdaptiveSync>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            mode: None,
            position: None,
            scale: 1.0,
            transform: OutputTransformConfig::default(),
            gaps: None,
            adaptive_sync: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub enum OutputTransformConfig {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl OutputTransformConfig {
    pub fn to_transform(self) -> smithay::utils::Transform {
        use smithay::utils::Transform;
        match self {
            Self::Normal => Transform::Normal,
            Self::Rotate90 => Transform::_90,
            Self::Rotate180 => Transform::_180,
            Self::Rotate270 => Transform::_270,
            Self::Flipped => Transform::Flipped,
            Self::Flipped90 => Transform::Flipped90,
            Self::Flipped180 => Transform::Flipped180,
            Self::Flipped270 => Transform::Flipped270,
        }
    }
}

/// Which DRM device TideWM should use for its own GLES rendering and
/// scanout. Paths may name either a primary (`cardN`) or render node; the
/// backend resolves both to the same physical device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuSelector {
    Auto,
    Path(PathBuf),
    Vendor(GpuVendor),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Amd,
    Nvidia,
    Intel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuConfig {
    pub render: GpuSelector,
    pub exclude: Vec<GpuSelector>,
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            render: GpuSelector::Auto,
            exclude: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct GpuConfigRaw {
    render: String,
    exclude: Vec<String>,
}

impl Default for GpuConfigRaw {
    fn default() -> Self {
        Self {
            render: "auto".to_string(),
            exclude: Vec::new(),
        }
    }
}

impl GpuConfig {
    pub(super) fn from_raw(raw: GpuConfigRaw, warnings: &mut Vec<String>) -> Self {
        let render = parse_gpu_selector(&raw.render, true).unwrap_or_else(|| {
            warnings.push(format!(
                "Invalid gpu.render {:?}; using auto (expected auto, /dev/dri/..., or vendor:amd|nvidia|intel)",
                raw.render
            ));
            GpuSelector::Auto
        });
        let exclude = raw
            .exclude
            .into_iter()
            .filter_map(|value| match parse_gpu_selector(&value, false) {
                Some(selector) => Some(selector),
                None => {
                    warnings.push(format!(
                        "Invalid gpu.exclude entry {value:?}; ignoring (expected /dev/dri/... or vendor:amd|nvidia|intel)"
                    ));
                    None
                }
            })
            .collect();
        Self { render, exclude }
    }
}

fn parse_gpu_selector(value: &str, allow_auto: bool) -> Option<GpuSelector> {
    let value = value.trim();
    match value.to_ascii_lowercase().as_str() {
        "auto" if allow_auto => Some(GpuSelector::Auto),
        "vendor:amd" => Some(GpuSelector::Vendor(GpuVendor::Amd)),
        "vendor:nvidia" => Some(GpuSelector::Vendor(GpuVendor::Nvidia)),
        "vendor:intel" => Some(GpuSelector::Vendor(GpuVendor::Intel)),
        _ => {
            let path = PathBuf::from(value);
            (path.is_absolute() && path.starts_with("/dev/dri")).then_some(GpuSelector::Path(path))
        }
    }
}

/// Parsed `switch_events { }` entries: one optional [`Action`] per lid /
/// tablet-mode transition. Reaches the compositor through the same
/// `Smallvil::run_action` every keybind and IPC `action` request uses, so
/// anything a keybind can do, a lid event can do too -- but you almost
/// always want `spawn:` here (suspend, lock, brightness, an onboard
/// keyboard, etc.), since none of the WM-internal actions have an obvious
/// meaning for a non-user-triggered event.
#[derive(Debug, Clone, Default)]
pub struct SwitchEventsConfig {
    pub lid_close: Option<Action>,
    pub lid_open: Option<Action>,
    pub tablet_mode_on: Option<Action>,
    pub tablet_mode_off: Option<Action>,
}

/// String form of [`SwitchEventsConfig`], kept separate because [`Action`]
/// isn't `Deserialize` -- it's parsed from the same string syntax
/// bind values use (see `parse_action`). Matches the keybinds
/// pattern of storing raw strings in `RawConfig` and resolving them in
/// `Config::from_raw`.
#[derive(Debug, Default)]
pub(super) struct SwitchEventsRaw {
    pub(super) lid_close: Option<String>,
    pub(super) lid_open: Option<String>,
    pub(super) tablet_mode_on: Option<String>,
    pub(super) tablet_mode_off: Option<String>,
}

impl SwitchEventsConfig {
    pub(super) fn from_raw(raw: SwitchEventsRaw) -> Self {
        Self {
            lid_close: raw.lid_close.and_then(|s| parse_action(&s)),
            lid_open: raw.lid_open.and_then(|s| parse_action(&s)),
            tablet_mode_on: raw.tablet_mode_on.and_then(|s| parse_action(&s)),
            tablet_mode_off: raw.tablet_mode_off.and_then(|s| parse_action(&s)),
        }
    }
}

/// Parses a `mode` string like `"1920x1080"` or `"1920x1080@60"` into
/// `(width, height, refresh_hz)`.
pub fn parse_mode_str(s: &str) -> Option<(i32, i32, Option<f64>)> {
    let (res, refresh) = match s.split_once('@') {
        Some((res, r)) => {
            let refresh = r.trim().parse::<f64>().ok()?;
            if !refresh.is_finite() || refresh <= 0.0 {
                return None;
            }
            (res, Some(refresh))
        }
        None => (s, None),
    };
    let (w, h) = res.split_once('x')?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h, refresh))
}

pub(super) fn apply_gpu_block(gpu: &mut GpuConfigRaw, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `gpu` block, ignoring");
            continue;
        };
        match key.as_str() {
            "render" => gpu.render = value.clone(),
            "exclude" => {
                gpu.exclude = if value.trim() == "[]" {
                    Vec::new()
                } else if let Some(values) = parse_list_value(value) {
                    values
                } else {
                    vec![value.clone()]
                };
            }
            other => tracing::warn!(key = %other, "Unknown key in `gpu` block, ignoring"),
        }
    }
}

pub(super) fn apply_input_block(input: &mut InputConfig, body: &[waves::Entry]) {
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) => match key.as_str() {
                "repeat_delay" => set_i32(&mut input.repeat_delay, key, value),
                "repeat_rate" => set_i32(&mut input.repeat_rate, key, value),
                "focus_follows_mouse" => set_bool(&mut input.focus_follows_mouse, key, value),
                "unfocus_on_empty" => set_bool(&mut input.unfocus_on_empty, key, value),
                "xkb_rules" => input.xkb_rules = value.clone(),
                "xkb_model" => input.xkb_model = value.clone(),
                "xkb_layout" => input.xkb_layout = value.clone(),
                "xkb_variant" => input.xkb_variant = value.clone(),
                "xkb_options" => input.xkb_options = Some(value.clone()),
                other => tracing::warn!(key = %other, "Unknown key in `input` block, ignoring"),
            },
            waves::Entry::Block(keyword, _, touchpad_body) if keyword == "touchpad" => {
                apply_touchpad_block(&mut input.touchpad, touchpad_body);
            }
            waves::Entry::Block(keyword, header, device_body) if keyword == "device" => {
                if let Some(device) = lower_input_device_block(header, device_body) {
                    input.devices.push(device);
                }
            }
            _ => tracing::warn!("Unexpected entry in `input` block, ignoring"),
        }
    }
}

fn lower_input_device_block(header: &str, body: &[waves::Entry]) -> Option<InputDeviceConfig> {
    let name = header.trim();
    if name.is_empty() {
        tracing::warn!("An `input.device` block needs a libinput device name");
        return None;
    }

    let mut device = InputDeviceConfig {
        name: name.to_string(),
        ..Default::default()
    };
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!(
                device = name,
                "Unexpected entry in `input.device`, ignoring"
            );
            continue;
        };
        match key.as_str() {
            "map_to_output" => {
                if value.trim().is_empty() {
                    tracing::warn!(device = name, "map_to_output cannot be empty, ignoring");
                } else {
                    device.map_to_output = Some(value.clone());
                }
            }
            "calibration_matrix" => match parse_calibration_matrix(value) {
                Some(matrix) => device.calibration_matrix = Some(matrix),
                None => tracing::warn!(
                    device = name,
                    value,
                    "calibration_matrix needs exactly six finite numbers, ignoring"
                ),
            },
            other => tracing::warn!(
                device = name,
                key = %other,
                "Unknown key in `input.device`, ignoring"
            ),
        }
    }
    Some(device)
}

fn parse_calibration_matrix(value: &str) -> Option<[f32; 6]> {
    let values = parse_list_value(value)?;
    if values.len() != 6 {
        return None;
    }

    let mut matrix = [0.0; 6];
    for (slot, value) in matrix.iter_mut().zip(values) {
        let value = value.parse::<f32>().ok()?;
        if !value.is_finite() {
            return None;
        }
        *slot = value;
    }
    Some(matrix)
}

pub(super) fn apply_touchpad_block(touchpad: &mut TouchpadConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `touchpad` block, ignoring");
            continue;
        };
        match key.as_str() {
            "tap_to_click" => set_opt_bool(&mut touchpad.tap_to_click, key, value),
            "tap_and_drag" => set_opt_bool(&mut touchpad.tap_and_drag, key, value),
            "drag_lock" => set_opt_bool(&mut touchpad.drag_lock, key, value),
            "disable_while_typing" => set_opt_bool(&mut touchpad.disable_while_typing, key, value),
            "natural_scroll" => set_opt_bool(&mut touchpad.natural_scroll, key, value),
            "left_handed" => set_opt_bool(&mut touchpad.left_handed, key, value),
            "middle_emulation" => set_opt_bool(&mut touchpad.middle_emulation, key, value),
            "click_method" => touchpad.click_method = Some(value.clone()),
            "scroll_method" => touchpad.scroll_method = Some(value.clone()),
            "accel_speed" => match value.parse::<f64>() {
                Ok(speed) if speed.is_finite() && (-1.0..=1.0).contains(&speed) => {
                    touchpad.accel_speed = Some(speed)
                }
                _ => tracing::warn!(
                    key,
                    value,
                    "Expected accel_speed from -1.0 to 1.0, ignoring"
                ),
            },
            "accel_profile" => touchpad.accel_profile = Some(value.clone()),
            "workspace_swipe_fingers" => match value.parse::<u32>() {
                Ok(0) => touchpad.workspace_swipe_fingers = None,
                Ok(fingers @ 3..) => touchpad.workspace_swipe_fingers = Some(fingers),
                Ok(_) => tracing::warn!(
                    key,
                    value,
                    "workspace_swipe_fingers must be 0 or at least 3, ignoring"
                ),
                Err(err) => tracing::warn!(key, value, %err, "Expected an integer, ignoring"),
            },
            "workspace_swipe_distance" => match value.parse::<f64>() {
                Ok(distance) if distance.is_finite() && distance > 0.0 => {
                    touchpad.workspace_swipe_distance = Some(distance)
                }
                _ => tracing::warn!(
                    key,
                    value,
                    "Expected a positive workspace_swipe_distance, ignoring"
                ),
            },
            "gesture_swipe_fingers" => {
                set_gesture_fingers(&mut touchpad.gesture_swipe_fingers, key, value)
            }
            "swipe_left" => touchpad.swipe_left = parse_action(value),
            "swipe_right" => touchpad.swipe_right = parse_action(value),
            "swipe_up" => touchpad.swipe_up = parse_action(value),
            "swipe_down" => touchpad.swipe_down = parse_action(value),
            "gesture_pinch_fingers" => {
                set_gesture_fingers(&mut touchpad.gesture_pinch_fingers, key, value)
            }
            "pinch_in" => touchpad.pinch_in = parse_action(value),
            "pinch_out" => touchpad.pinch_out = parse_action(value),
            "modifier_pan_fingers" => {
                set_gesture_fingers(&mut touchpad.modifier_pan_fingers, key, value)
            }
            other => tracing::warn!(key = %other, "Unknown key in `touchpad` block, ignoring"),
        }
    }
}

fn set_gesture_fingers(field: &mut Option<u32>, key: &str, value: &str) {
    match value.parse::<u32>() {
        Ok(0) => *field = None,
        Ok(fingers @ 2..) => *field = Some(fingers),
        Ok(_) => tracing::warn!(
            key,
            value,
            "gesture finger count must be 0 or at least 2, ignoring"
        ),
        Err(err) => tracing::warn!(key, value, %err, "Expected an integer, ignoring"),
    }
}

pub(super) fn apply_xwayland_block(xwayland: &mut XwaylandConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `xwayland` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut xwayland.enabled, key, value),
            "path" => xwayland.path = value.clone(),
            other => tracing::warn!(key = %other, "Unknown key in `xwayland` block, ignoring"),
        }
    }
}

pub(super) fn apply_switch_events_block(
    switch_events: &mut SwitchEventsRaw,
    body: &[waves::Entry],
) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `switch_events` block, ignoring");
            continue;
        };
        match key.as_str() {
            "lid_close" => switch_events.lid_close = Some(value.clone()),
            "lid_open" => switch_events.lid_open = Some(value.clone()),
            "tablet_mode_on" => switch_events.tablet_mode_on = Some(value.clone()),
            "tablet_mode_off" => switch_events.tablet_mode_off = Some(value.clone()),
            other => tracing::warn!(key = %other, "Unknown key in `switch_events` block, ignoring"),
        }
    }
}

pub(super) fn lower_output_block(header: &str, body: &[waves::Entry]) -> OutputConfig {
    let mut cfg = OutputConfig {
        name: header.trim().to_string(),
        ..Default::default()
    };
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!(name = %cfg.name, "Unexpected entry in `output` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "mode" => cfg.mode = Some(value.clone()),
            "position" => match parse_position(value) {
                Some(pos) => cfg.position = Some(pos),
                None => tracing::warn!(value, "Expected a position like `1920x0`, ignoring"),
            },
            "scale" => match value.parse::<f64>() {
                Ok(scale) if scale.is_finite() && scale > 0.0 => cfg.scale = scale,
                _ => tracing::warn!(value, "Expected a positive finite output scale, ignoring"),
            },
            "transform" => match parse_transform(value) {
                Some(t) => cfg.transform = t,
                None => tracing::warn!(value, "Unknown transform, ignoring"),
            },
            "gaps" => match value.parse() {
                Ok(n) => cfg.gaps = Some(n),
                Err(_) => tracing::warn!(value, "Expected a pixel gap value, ignoring"),
            },
            "adaptive_sync" | "vrr" | "variable_refresh_rate" => match parse_adaptive_sync(value) {
                Some(mode) => cfg.adaptive_sync = Some(mode),
                None => tracing::warn!(
                    value,
                    "Expected output adaptive_sync: off on on-demand, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `output` block, ignoring"),
        }
    }
    cfg
}

/// `"1920x0"` -> `(1920, 0)`, the same `WxH` shorthand `parse_mode_str`
/// already uses for resolution, reused here for position.
pub(super) fn parse_position(s: &str) -> Option<(i32, i32)> {
    let (x, y) = s.split_once('x')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn parse_transform(s: &str) -> Option<OutputTransformConfig> {
    match s {
        "normal" => Some(OutputTransformConfig::Normal),
        "90" => Some(OutputTransformConfig::Rotate90),
        "180" => Some(OutputTransformConfig::Rotate180),
        "270" => Some(OutputTransformConfig::Rotate270),
        "flipped" => Some(OutputTransformConfig::Flipped),
        "flipped-90" => Some(OutputTransformConfig::Flipped90),
        "flipped-180" => Some(OutputTransformConfig::Flipped180),
        "flipped-270" => Some(OutputTransformConfig::Flipped270),
        _ => None,
    }
}

pub(super) fn parse_adaptive_sync(s: &str) -> Option<AdaptiveSync> {
    match s {
        "off" | "false" => Some(AdaptiveSync::Off),
        "on" | "true" => Some(AdaptiveSync::On),
        "on-demand" | "on_demand" | "ondemand" => Some(AdaptiveSync::OnDemand),
        _ => None,
    }
}
