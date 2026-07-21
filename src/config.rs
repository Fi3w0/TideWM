use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use smithay::input::keyboard::{xkb, Keysym, ModifiersState, XkbConfig};
use smithay::reexports::calloop::channel::{self, Channel};

/// A parsed, ready-to-match keybind: which modifiers must be held, which base
/// (unshifted) key symbol triggers it, and what it does.
#[derive(Debug, Clone)]
pub struct Keybind {
    pub mods: Mods,
    pub keysym: Keysym,
    pub action: Action,
}

/// Modifier state a keybind requires. Matched against `ModifiersState` ignoring
/// caps lock / num lock, so those don't break matching.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl Mods {
    pub fn matches(&self, state: &ModifiersState) -> bool {
        self.ctrl == state.ctrl
            && self.alt == state.alt
            && self.shift == state.shift
            && self.logo == state.logo
    }
}

/// Which tiling geometry a workspace uses (see `layout::Layouts::layout`).
/// Not `Deserialize` -- like `Action`, this is resolved from the same
/// string syntax `[keybinds]`/the `default_layout` config key use (see
/// `parse_layout_algorithm`), not derived directly from TOML, so an
/// unrecognized value can be handled differently depending on whether it
/// came from a keybind (drop the bind) or the top-level default (warn and
/// fall back) instead of failing the whole config either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutAlgorithm {
    /// The existing adaptive-BSP engine (`layout::BspLayout`) -- already
    /// Hyprland's own "dwindle" behavior (split orientation follows each
    /// leaf's own aspect ratio at layout time), just not previously named
    /// as a distinct mode since it was the only one this project had.
    #[default]
    Bsp,
    /// One master window plus an evenly-split vertical stack (dwm/
    /// Hyprland's "master" layout). Always left/right regardless of the
    /// output's aspect ratio -- that fixed orientation is the actual
    /// visual point of choosing this over the adaptive BSP/dwindle one.
    Master,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone)]
pub enum Action {
    Spawn(String),
    CloseWindow,
    ToggleFloating,
    ToggleFullscreen,
    TogglePin,
    ToggleScratchpad,
    MoveToScratchpad,
    TogglePseudoTile,
    CycleFocus,
    FocusDirection(Direction),
    SwapDirection(Direction),
    /// Groups the focused tiled window with its neighbor in `Direction`
    /// into one shared tab slot. See `Smallvil::group_direction`.
    GroupDirection(Direction),
    /// Removes the focused window from its group, if any, back to being
    /// its own ordinary tile. See `Smallvil::ungroup`.
    Ungroup,
    /// Cycles the focused window's group to its next/previous tab. See
    /// `Smallvil::cycle_tab`.
    CycleTabForward,
    CycleTabBackward,
    SwitchWorkspace(u32),
    MoveToWorkspace(u32),
    /// Swaps the current output's active workspace content with the named
    /// output's. No default keybind -- output names are machine-specific
    /// (see `[[output]]`), so there's nothing sensible to bind out of the box.
    SwapWorkspacesWithOutput(String),
    /// Switches which keybind table is consulted (sway/Hyprland's "mode"/
    /// "submap" idea): a temporary layer of binds on top of `[keybinds]`,
    /// entered by name and left active until an explicit `exit-submap`
    /// bind, not tied to focus or any other implicit event. See
    /// `Config::submaps`, `Smallvil::active_submap`.
    EnterSubmap(String),
    ExitSubmap,
    /// Switches the current output's active workspace to this tiling
    /// algorithm. See `LayoutAlgorithm`, `layout::Layouts::set_algorithm`.
    SetLayout(LayoutAlgorithm),
    /// Nudges the current output's active workspace's master/stack ratio
    /// (master layout only -- a no-op under BSP, see
    /// `layout::Layouts::adjust_master_ratio`). Positive grows the master
    /// pane, negative shrinks it; dwm/Hyprland's keybind-step convention
    /// rather than click-drag, since master mode has no BSP-style split
    /// boundary to click on in the first place.
    GrowMaster,
    ShrinkMaster,
    /// Shows/hides a schematic (rects + titles, not live thumbnails)
    /// grid of every workspace on the current output. See `overview.rs`,
    /// `Smallvil::toggle_overview`.
    ToggleOverview,
    Quit,
}

pub struct Config {
    pub terminal: String,
    /// Shows a one-time startup toast pointing a new user at
    /// `Super+Enter`. True on a freshly-generated config; delete the key
    /// (or set it `false`) to stop seeing it. Not read on reload -- only
    /// checked once, at startup (see `main.rs`).
    pub show_welcome_hint: bool,
    pub water_effects: bool,
    /// Forces the udev backend's software cursor to stay visible even when
    /// a client asks to hide it (`wl_pointer.set_cursor` with a null
    /// buffer, e.g. a terminal hiding its pointer glyph after inactivity --
    /// see `backend/udev.rs::render_surface`'s `CursorImageStatus::Hidden`
    /// handling). Off by default: respecting a client's own hide request is
    /// the correct default behavior, this is an opt-in override for anyone
    /// who wants the pointer to never disappear.
    pub cursor_always_visible: bool,
    pub gaps: i32,
    /// Starting tiling algorithm for a workspace with no runtime override
    /// (see `"layout:bsp"`/`"layout:master"` keybind actions,
    /// `layout::Layouts::set_default_algorithm`). Read fresh on every
    /// config reload (`Smallvil::reload_config`), same as `gaps`.
    pub default_layout: LayoutAlgorithm,
    /// Fraction of its tile's size a pseudo-tiled window keeps, centered
    /// within it (see `toggle-pseudo-tile`). Clamped to [0.05, 1.0] on load
    /// so a bad value can't collapse or invert a window's size.
    pub pseudo_tile_scale: f64,
    pub keybinds: Vec<Keybind>,
    pub input: InputConfig,
    pub xwayland: XwaylandConfig,
    pub spawn_at_startup: Vec<String>,
    pub outputs: Vec<OutputConfig>,
    /// Laptop lid / tablet-mode switch bindings, udev backend only. winit
    /// has no host-independent access to libinput's switch capability, so
    /// on a nested session these just sit unused.
    pub switch_events: SwitchEventsConfig,
    /// Per-app placement applied when a window first maps (`for_window`/
    /// `windowrule`'s own idea). See `WindowRule::matches`.
    pub window_rules: Vec<WindowRule>,
    /// Named alternate keybind tables (`[submap.<name>]`), each parsed the
    /// same way `[keybinds]` is. See `Action::EnterSubmap`.
    pub submaps: HashMap<String, Vec<Keybind>>,
    /// Environment variables to set for TideWM's own process (so, e.g.,
    /// `XCURSOR_THEME` here actually affects the cursor theme TideWM loads
    /// itself, not just child processes) and to push into the systemd/
    /// D-Bus session-activation environment alongside `WAYLAND_DISPLAY`
    /// (same idea as Hyprland's `env = KEY,VALUE` lines -- their own
    /// config uses this for `QT_QPA_PLATFORMTHEME`/`GTK_USE_PORTAL`/
    /// `GDK_BACKEND`/`XCURSOR_*`). See `main.rs`'s `apply_user_env`/
    /// `export_session_environment`.
    pub env: HashMap<String, String>,
}

impl Config {
    /// Load `$XDG_CONFIG_HOME/tidewm/config.toml` (falling back to
    /// `~/.config/tidewm/config.toml`), writing out the default file on first
    /// run so there's something to edit. Falls back to in-memory defaults if
    /// the file can't be read or parsed.
    pub fn load() -> Self {
        let path = config_path();

        let raw = if path.exists() {
            load_raw_config(&path).unwrap_or_else(|err| {
                tracing::warn!(%err, path = %path.display(), "Failed to parse config, using defaults");
                RawConfig::default()
            })
        } else {
            let default = RawConfig::default();
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(err) = fs::write(&path, DEFAULT_CONFIG_TOML) {
                tracing::warn!(%err, path = %path.display(), "Failed to write default config");
            }
            default
        };

        Self::from_raw(raw)
    }

    /// Re-read the config file for a hot-reload. Unlike `load`, this never
    /// writes a default file and reports errors instead of silently falling
    /// back, so a reload with a typo in it doesn't quietly wipe out whatever
    /// the user had.
    pub fn reload() -> Result<Self, String> {
        let raw = load_raw_config(&config_path())?;
        Ok(Self::from_raw(raw))
    }

    /// The path being watched/loaded, so callers can set up a file watcher on it.
    pub fn path() -> PathBuf {
        config_path()
    }

    fn from_raw(raw: RawConfig) -> Self {
        let keybinds = raw
            .keybinds
            .iter()
            .filter_map(|(combo, action)| parse_keybind(combo, action))
            .collect();
        let submaps = raw
            .submaps
            .iter()
            .map(|(name, binds)| {
                let parsed = binds
                    .iter()
                    .filter_map(|(combo, action)| parse_keybind(combo, action))
                    .collect();
                (name.clone(), parsed)
            })
            .collect();

        let default_layout = parse_layout_algorithm(&raw.default_layout).unwrap_or_else(|| {
            if !raw.default_layout.is_empty() {
                tracing::warn!(value = %raw.default_layout, "Unknown default_layout, using bsp");
            }
            LayoutAlgorithm::Bsp
        });

        Self {
            terminal: raw.terminal,
            show_welcome_hint: raw.show_welcome_hint,
            water_effects: raw.water_effects,
            cursor_always_visible: raw.cursor_always_visible,
            gaps: raw.gaps,
            default_layout,
            pseudo_tile_scale: raw.pseudo_tile_scale.clamp(0.05, 1.0),
            keybinds,
            input: raw.input,
            xwayland: raw.xwayland,
            spawn_at_startup: raw.spawn_at_startup,
            outputs: raw.outputs,
            switch_events: SwitchEventsConfig::from_raw(raw.switch_events),
            window_rules: raw.window_rules,
            submaps,
            env: raw.env,
        }
    }

    /// Folds every `[[window_rule]]` entry matching `app_id`/`title` into
    /// one effective rule: later matching entries override an earlier
    /// one's `workspace`/`output` (last match wins, so a more specific
    /// rule can sit after a general one and win), while `float`/
    /// `pseudo_tile`/`pin` accumulate (any match sets it, never unsets --
    /// booleans have no "leave alone" state to fall back to). Called once
    /// per newly-mapped window, not per frame, so folding rather than
    /// caching is fine.
    pub(crate) fn resolve_window_rules(&self, app_id: Option<&str>, title: Option<&str>) -> WindowRule {
        let mut effective = WindowRule::default();
        for rule in &self.window_rules {
            if !rule.matches(app_id, title) {
                continue;
            }
            if rule.workspace.is_some() {
                effective.workspace = rule.workspace;
            }
            if rule.output.is_some() {
                effective.output = rule.output.clone();
            }
            effective.float |= rule.float;
            effective.pseudo_tile |= rule.pseudo_tile;
            effective.pin |= rule.pin;
        }
        effective
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawConfig {
    /// Paths of other TOML files to fold in before this file's own keys
    /// (see `load_raw_config`/`merge_toml`). Always empty by the time
    /// `RawConfig` itself deserializes -- `load_raw_config` resolves and
    /// strips this key from every file's `toml::Value` first -- kept as a
    /// real field purely so the schema documents it, rather than `include`
    /// being an unknown key serde would otherwise just silently accept
    /// and drop with no explanation of where it went.
    include: Vec<String>,
    terminal: String,
    show_welcome_hint: bool,
    water_effects: bool,
    cursor_always_visible: bool,
    gaps: i32,
    /// `"bsp"`/`"master"`, resolved via `parse_layout_algorithm` in
    /// `Config::from_raw`. Raw string (not `LayoutAlgorithm` itself, which
    /// isn't `Deserialize`) for the same reason `switch_events`/`keybinds`
    /// store strings -- so a bad value warns and falls back rather than
    /// failing the whole config parse.
    default_layout: String,
    pseudo_tile_scale: f64,
    keybinds: HashMap<String, String>,
    input: InputConfig,
    xwayland: XwaylandConfig,
    spawn_at_startup: Vec<String>,
    #[serde(rename = "output", default)]
    outputs: Vec<OutputConfig>,
    switch_events: SwitchEventsRaw,
    #[serde(rename = "window_rule", default)]
    window_rules: Vec<WindowRule>,
    #[serde(rename = "submap", default)]
    submaps: HashMap<String, HashMap<String, String>>,
    env: HashMap<String, String>,
    /// `$name` values substituted into `terminal`/`spawn_at_startup`/
    /// `[keybinds]`/`[submap.*]`/`[switch_events]` right after parsing (see
    /// `substitute_variables_in_raw`) -- Hyprland's own `$mainMod`/
    /// `$terminal` convention, so one keybind or spawn command can be
    /// reused instead of repeated verbatim everywhere. Never touches
    /// anything itself once substitution runs.
    variables: HashMap<String, String>,
}

impl Default for RawConfig {
    fn default() -> Self {
        let mut keybinds = HashMap::new();
        keybinds.insert("Super+Return".to_string(), "spawn:kitty".to_string());
        keybinds.insert("Super+Q".to_string(), "close-window".to_string());
        keybinds.insert("Super+V".to_string(), "toggle-floating".to_string());
        keybinds.insert("Super+F".to_string(), "toggle-fullscreen".to_string());
        keybinds.insert("Super+Tab".to_string(), "cycle-focus".to_string());
        keybinds.insert("Super+H".to_string(), "focus-left".to_string());
        keybinds.insert("Super+L".to_string(), "focus-right".to_string());
        keybinds.insert("Super+K".to_string(), "focus-up".to_string());
        keybinds.insert("Super+J".to_string(), "focus-down".to_string());
        keybinds.insert("Super+Shift+H".to_string(), "swap-left".to_string());
        keybinds.insert("Super+Shift+L".to_string(), "swap-right".to_string());
        keybinds.insert("Super+Shift+K".to_string(), "swap-up".to_string());
        keybinds.insert("Super+Shift+J".to_string(), "swap-down".to_string());
        keybinds.insert("Super+Shift+Q".to_string(), "quit".to_string());
        keybinds.insert("Super+P".to_string(), "toggle-pin".to_string());
        keybinds.insert(
            "Super+Shift+P".to_string(),
            "toggle-pseudo-tile".to_string(),
        );
        keybinds.insert("Super+W".to_string(), "layout:bsp".to_string());
        keybinds.insert("Super+Shift+W".to_string(), "layout:master".to_string());
        keybinds.insert("Super+Ctrl+Minus".to_string(), "master-shrink".to_string());
        keybinds.insert("Super+Ctrl+Equal".to_string(), "master-grow".to_string());
        keybinds.insert("Super+O".to_string(), "toggle-overview".to_string());
        // Merges the focused tiled window with its neighbor into one
        // shared tab slot; Super+Shift+G splits it back out.
        keybinds.insert("Super+Ctrl+H".to_string(), "group-left".to_string());
        keybinds.insert("Super+Ctrl+L".to_string(), "group-right".to_string());
        keybinds.insert("Super+Ctrl+K".to_string(), "group-up".to_string());
        keybinds.insert("Super+Ctrl+J".to_string(), "group-down".to_string());
        keybinds.insert("Super+Shift+G".to_string(), "ungroup".to_string());
        keybinds.insert("Super+BracketRight".to_string(), "cycle-tab-next".to_string());
        keybinds.insert("Super+BracketLeft".to_string(), "cycle-tab-prev".to_string());
        // i3/sway's own default scratchpad binds.
        keybinds.insert("Super+Minus".to_string(), "toggle-scratchpad".to_string());
        keybinds.insert(
            "Super+Shift+Minus".to_string(),
            "move-to-scratchpad".to_string(),
        );
        // Workspaces 1-9 on their own number key, 10 on the "0" key,
        // matching i3/sway's convention.
        for workspace in 1..=10 {
            let key = if workspace == 10 { 0 } else { workspace };
            keybinds.insert(format!("Super+{key}"), format!("workspace:{workspace}"));
            keybinds.insert(
                format!("Super+Shift+{key}"),
                format!("move-to-workspace:{workspace}"),
            );
        }
        keybinds.insert("Super+N".to_string(), "submap:nav".to_string());

        // A submap (sway/Hyprland's "mode" idea): a temporary alternate
        // keybind table, entered above via Super+N, left active until its
        // own exit-submap bind -- not tied to focus or any other implicit
        // event. This one's a vim-motion focus-move mode (h/j/k/l with no
        // modifier held); a resize mode is the more common example
        // elsewhere, but needs a keyboard resize action this project
        // doesn't have yet.
        let mut nav_submap = HashMap::new();
        nav_submap.insert("h".to_string(), "focus-left".to_string());
        nav_submap.insert("l".to_string(), "focus-right".to_string());
        nav_submap.insert("k".to_string(), "focus-up".to_string());
        nav_submap.insert("j".to_string(), "focus-down".to_string());
        nav_submap.insert("Escape".to_string(), "exit-submap".to_string());
        let mut submaps = HashMap::new();
        submaps.insert("nav".to_string(), nav_submap);

        Self {
            include: Vec::new(),
            terminal: "kitty".to_string(),
            // Deliberately false, not true: a real config.toml always ships
            // with `show_welcome_hint = true` written explicitly (see
            // DEFAULT_CONFIG_TOML), so this default is only ever consulted
            // when a user deletes the key from an existing file -- and per
            // the on-screen hint's own "delete this to dismiss" advice
            // (welcome.rs), that must resolve to off, not back to on.
            show_welcome_hint: false,
            water_effects: true,
            cursor_always_visible: false,
            gaps: 8,
            default_layout: String::new(),
            pseudo_tile_scale: 0.7,
            keybinds,
            input: InputConfig::default(),
            xwayland: XwaylandConfig::default(),
            spawn_at_startup: Vec::new(),
            outputs: Vec::new(),
            switch_events: SwitchEventsRaw::default(),
            window_rules: Vec::new(),
            submaps,
            env: HashMap::new(),
            variables: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    pub repeat_delay: i32,
    pub repeat_rate: i32,
    pub focus_follows_mouse: bool,
    /// xkbcommon rules/model/layout/variant/options. Empty string / `None`
    /// (the default for each) means "let xkbcommon fall back to the
    /// `XKB_DEFAULT_*` env vars," same as `XkbConfig::default()` -- so an
    /// empty `[input]` section changes nothing versus today.
    pub xkb_rules: String,
    pub xkb_model: String,
    pub xkb_layout: String,
    pub xkb_variant: String,
    pub xkb_options: Option<String>,
    pub touchpad: TouchpadConfig,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            repeat_delay: 200,
            repeat_rate: 25,
            focus_follows_mouse: true,
            xkb_rules: String::new(),
            xkb_model: String::new(),
            xkb_layout: String::new(),
            xkb_variant: String::new(),
            xkb_options: None,
            touchpad: TouchpadConfig::default(),
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
/// ponytail: applied only when libinput reports the device (startup
/// enumeration and hotplug), not re-applied to an already-connected
/// touchpad on a config reload -- the `Libinput` handle lives inside the
/// udev backend's own event-loop closures, not reachable from
/// `Smallvil::reload_config`. A touchpad-settings edit needs a compositor
/// restart to take effect; upgrade path is threading a device registry
/// into `Smallvil` if that gap actually bites.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
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
#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `"1920x1080"` or `"1920x1080@60"`. Falls back to the connector's own
    /// preferred mode if unset or if nothing matches.
    pub mode: Option<String>,
    /// Falls back to auto-layout (rightmost edge of already-mapped
    /// outputs) if unset.
    pub position: Option<(i32, i32)>,
    #[serde(default = "default_scale")]
    pub scale: f64,
    #[serde(default)]
    pub transform: OutputTransformConfig,
}

fn default_true() -> bool {
    true
}

fn default_scale() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputTransformConfig {
    #[default]
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    Flipped,
    #[serde(rename = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped-270")]
    Flipped270,
}

/// Parsed `[switch_events]` entries: one optional [`Action`] per lid /
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
/// `[keybinds]` values use (see `parse_action`). Matches the keybinds
/// pattern of storing raw strings in `RawConfig` and resolving them in
/// `Config::from_raw`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SwitchEventsRaw {
    lid_close: Option<String>,
    lid_open: Option<String>,
    tablet_mode_on: Option<String>,
    tablet_mode_off: Option<String>,
}

impl SwitchEventsConfig {
    fn from_raw(raw: SwitchEventsRaw) -> Self {
        Self {
            lid_close: raw.lid_close.and_then(|s| parse_action(&s)),
            lid_open: raw.lid_open.and_then(|s| parse_action(&s)),
            tablet_mode_on: raw.tablet_mode_on.and_then(|s| parse_action(&s)),
            tablet_mode_off: raw.tablet_mode_off.and_then(|s| parse_action(&s)),
        }
    }
}

/// One `[[window_rule]]` entry: match newly-mapped windows by `app_id`/
/// `title` (i3/sway's `for_window`, Hyprland's `windowrule` idea) and apply
/// a placement before the window's first tile/floating rect is ever
/// computed, so a matched window never visibly starts in its default spot
/// and jumps. `app_id` matches the whole string exactly (app IDs are
/// stable identifiers, not prose); `title` matches case-insensitively
/// anywhere in the string (titles change constantly -- exact matching
/// would be nearly useless for them). See `Config::resolve_window_rules`
/// for how multiple matching entries combine, and `handlers/xdg_shell.rs`'s
/// `map_toplevel` for where this actually applies.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WindowRule {
    pub app_id: Option<String>,
    pub title: Option<String>,
    /// Initial workspace number -- same numbering `workspace:N` keybinds
    /// use, including 0 (the reserved scratchpad) if you want a window to
    /// always start hidden.
    pub workspace: Option<u32>,
    /// Initial output by connector name (see the `outputs` IPC query, or
    /// your logs). Falls back to the usual placement logic if unset, or if
    /// no output with this name is currently connected.
    pub output: Option<String>,
    pub float: bool,
    /// No-op unless the window ends up tiled -- pseudo-tiling only has
    /// meaning as a rect override on a real tile, same as the interactive
    /// `toggle-pseudo-tile` keybind. Ignored if this rule (or another
    /// matching one) also sets `float`/`pin`.
    pub pseudo_tile: bool,
    /// Implies `float` -- pinning only has meaning for a floating window,
    /// the same invariant `toggle-pin` enforces interactively.
    pub pin: bool,
}

impl WindowRule {
    /// A rule with neither `app_id` nor `title` set never matches anything,
    /// rather than silently matching every window -- a blank rule is far
    /// more likely to be a config mistake than an intentional "match all".
    pub(crate) fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        if self.app_id.is_none() && self.title.is_none() {
            return false;
        }
        if let Some(want) = &self.app_id {
            if app_id != Some(want.as_str()) {
                return false;
            }
        }
        if let Some(want) = &self.title {
            let Some(title) = title else { return false };
            if !title.to_lowercase().contains(&want.to_lowercase()) {
                return false;
            }
        }
        true
    }
}

/// Parses a `mode` string like `"1920x1080"` or `"1920x1080@60"` into
/// `(width, height, refresh_hz)`.
pub fn parse_mode_str(s: &str) -> Option<(i32, i32, Option<f64>)> {
    let (res, refresh) = match s.split_once('@') {
        Some((res, r)) => (res, Some(r.parse::<f64>().ok()?)),
        None => (s, None),
    };
    let (w, h) = res.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?, refresh))
}

/// Watches the whole config directory tree (not just the main file, and
/// recursively, so a `keybinds.toml` sitting next to `config.toml` -- or in
/// a subdirectory -- is covered too) and forwards a `()` into the returned
/// calloop `Channel` whenever a `.toml` file under it changes. Keep the
/// returned `RecommendedWatcher` alive for as long as watching should
/// continue; dropping it stops the watch.
///
/// Recursive-plus-filtered rather than tracking the exact set of files an
/// `include` chain currently references: the include graph can only be
/// known *after* parsing (which itself needs the watcher to already be
/// running), and in every real layout -- this project's own default,
/// Hyprland's `~/.config/hypr/` -- every included file lives somewhere
/// under the same config directory anyway, so this covers the actual case
/// without re-deriving the watch list on every reload. The `.toml`
/// extension filter (plus skipping any path with a dotfile/dotdir
/// component) also keeps a config directory that's its own git repo (as
/// this user's real Hyprland config is) from spamming reloads on every
/// `.git/index` write a `git status`/`checkout` causes.
pub fn spawn_watcher() -> notify::Result<(RecommendedWatcher, Channel<()>)> {
    let (tx, rx) = channel::channel();
    let watch_dir = config_path()
        .parent()
        .expect("config path always has a parent")
        .to_path_buf();

    // The dotfile/dotdir check below has to run against the path *relative
    // to this directory*, not the absolute path notify reports -- the real
    // default config lives under `~/.config/tidewm`, and `.config` is
    // itself a dotdir component of the absolute path, which would wrongly
    // filter out every real config path if checked directly.
    let watch_root = watch_dir.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if !(event.kind.is_modify() || event.kind.is_create()) {
            return;
        }
        let relevant = event.paths.iter().any(|p| {
            let rel = p.strip_prefix(&watch_root).unwrap_or(p);
            p.extension().is_some_and(|ext| ext == "toml")
                && !rel
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        });
        if relevant {
            let _ = tx.send(());
        }
    })?;

    // `Config::load()` normally creates this directory already, but the
    // watcher needs it to exist regardless of load order.
    let _ = fs::create_dir_all(&watch_dir);
    watcher.watch(&watch_dir, RecursiveMode::Recursive)?;

    Ok((watcher, rx))
}

/// Reads `path` as TOML and deserializes it into a `RawConfig`, resolving
/// any `include = [...]` arrays first (see `load_toml_merged`). The single
/// entry point both `Config::load` and `Config::reload` use so they stay
/// consistent about how includes are resolved.
fn load_raw_config(path: &Path) -> Result<RawConfig, String> {
    let value = load_toml_merged(path)?;
    let mut raw: RawConfig = value.try_into().map_err(|err: toml::de::Error| err.to_string())?;
    substitute_variables_in_raw(&mut raw);
    Ok(raw)
}

/// Replaces `$name` tokens with their `[variables]` value, but only for
/// names actually defined there -- Hyprland's own `$mainMod`/`$terminal`
/// convention (see their real `keybinds.conf`: `bind = $mainMod, Q, exec,
/// $terminal`). Every other `$` (a spawn command's own `$HOME`, `$PATH`,
/// anything not one of this config's variables) is left exactly as
/// written -- substituting *any* `$word` unconditionally would corrupt
/// those instead of just leaving them for the shell/program that actually
/// understands them.
fn substitute_variables(s: &str, variables: &HashMap<String, String>) -> String {
    if variables.is_empty() || !s.contains('$') {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(dollar) = rest.find('$') {
        result.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        let name_len = after
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..name_len];
        if let Some(value) = (!name.is_empty()).then(|| variables.get(name)).flatten() {
            result.push_str(value);
            rest = &after[name_len..];
        } else {
            result.push('$');
            rest = after;
        }
    }
    result.push_str(rest);
    result
}

/// Applies `substitute_variables` to every field that plausibly reuses a
/// variable -- keybind/submap combos and actions, `spawn_at_startup`,
/// `terminal`, `switch_events` -- deliberately skipping match criteria like
/// `[[window_rule]]`'s `app_id`/`title` and `[[output]]`'s connector name,
/// since those describe something to match against, not a command or bind
/// where reusing a value makes sense.
fn substitute_variables_in_raw(raw: &mut RawConfig) {
    if raw.variables.is_empty() {
        return;
    }

    raw.terminal = substitute_variables(&raw.terminal, &raw.variables);
    raw.spawn_at_startup = raw
        .spawn_at_startup
        .iter()
        .map(|s| substitute_variables(s, &raw.variables))
        .collect();
    raw.keybinds = raw
        .keybinds
        .iter()
        .map(|(combo, action)| {
            (
                substitute_variables(combo, &raw.variables),
                substitute_variables(action, &raw.variables),
            )
        })
        .collect();
    raw.submaps = raw
        .submaps
        .iter()
        .map(|(name, binds)| {
            let binds = binds
                .iter()
                .map(|(combo, action)| {
                    (
                        substitute_variables(combo, &raw.variables),
                        substitute_variables(action, &raw.variables),
                    )
                })
                .collect();
            (name.clone(), binds)
        })
        .collect();
    raw.switch_events.lid_close = raw.switch_events.lid_close.as_deref().map(|s| substitute_variables(s, &raw.variables));
    raw.switch_events.lid_open = raw.switch_events.lid_open.as_deref().map(|s| substitute_variables(s, &raw.variables));
    raw.switch_events.tablet_mode_on =
        raw.switch_events.tablet_mode_on.as_deref().map(|s| substitute_variables(s, &raw.variables));
    raw.switch_events.tablet_mode_off =
        raw.switch_events.tablet_mode_off.as_deref().map(|s| substitute_variables(s, &raw.variables));
}

/// Reads `path` as TOML, recursively resolving any top-level `include =
/// [...]` array (paths relative to the including file's own directory, `~`
/// expanded) into one merged `toml::Value` -- Hyprland's multi-file
/// `source = path` idea, adapted to something valid TOML can express.
///
/// Merge order matches Hyprland's own `source` semantics: includes are
/// folded in left-to-right (a later entry in the list overlays an earlier
/// one), and the including file's own keys always win over anything it
/// included -- the same relationship its own `moonlit.conf` comment relies
/// on ("generated overrides, must stay last"), just expressed as "define it
/// in the file doing the including" instead of "list it last".
///
/// Only the top-level file's own errors (missing, unreadable, unparseable,
/// or a genuine `include` cycle) propagate to the caller. A problem in an
/// *included* file is logged as a warning and that one include is skipped
/// -- one bad split-out file shouldn't take down the whole config.
fn load_toml_merged(path: &Path) -> Result<toml::Value, String> {
    let mut ancestors = Vec::new();
    load_toml_merged_inner(path, &mut ancestors)
}

fn load_toml_merged_inner(path: &Path, ancestors: &mut Vec<PathBuf>) -> Result<toml::Value, String> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if ancestors.contains(&canonical) {
        return Err(format!("include cycle detected at {}", path.display()));
    }
    ancestors.push(canonical);
    let result = load_toml_merged_uncycled(path, ancestors);
    ancestors.pop();
    result
}

fn load_toml_merged_uncycled(path: &Path, ancestors: &mut Vec<PathBuf>) -> Result<toml::Value, String> {
    let contents = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut value: toml::Value = contents.parse().map_err(|err: toml::de::Error| format!("{}: {err}", path.display()))?;

    let includes: Vec<String> = match value.get("include") {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item.as_str() {
                Some(s) => Some(s.to_string()),
                None => {
                    tracing::warn!(path = %path.display(), "Non-string entry in `include`, skipping");
                    None
                }
            })
            .collect(),
        Some(_) => {
            tracing::warn!(path = %path.display(), "`include` must be an array of paths, ignoring");
            Vec::new()
        }
    };
    if let Some(table) = value.as_table_mut() {
        table.remove("include");
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut merged = toml::Value::Table(Default::default());
    for include in includes {
        let include_path = resolve_include_path(parent, &include);
        match load_toml_merged_inner(&include_path, ancestors) {
            Ok(included) => merged = merge_toml(merged, included),
            Err(err) => tracing::warn!(path = %include_path.display(), %err, "Failed to load included config file, skipping"),
        }
    }

    Ok(merge_toml(merged, value))
}

/// Expands a leading `~/` against `$HOME` and resolves the result against
/// `base_dir` (the including file's own directory) if it's not already
/// absolute -- so a split-out file can `include` a sibling by a plain
/// relative name, the common case in every real multi-file layout (compare
/// Hyprland's own `~/.config/hypr/`, where every sourced file just sits
/// next to `hyprland.conf`).
fn resolve_include_path(base_dir: &Path, include: &str) -> PathBuf {
    let expanded = match include.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(include),
        },
        None => PathBuf::from(include),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

/// Combines two parsed TOML values the way folding multiple files together
/// should behave: tables merge key-by-key, recursing so a nested table
/// (`[keybinds]`, `[submap.nav]`) combines entry-by-entry instead of one
/// file's whole table replacing another's; arrays concatenate, so
/// `[[output]]`/`[[window_rule]]` entries from every file all end up
/// present rather than only the last file's list surviving; anything else
/// (a scalar, or two values of different shapes) has `overlay` win outright.
fn merge_toml(base: toml::Value, overlay: toml::Value) -> toml::Value {
    match (base, overlay) {
        (toml::Value::Table(mut base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                let merged = match base.remove(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            toml::Value::Table(base)
        }
        (toml::Value::Array(mut base), toml::Value::Array(overlay)) => {
            base.extend(overlay);
            toml::Value::Array(base)
        }
        (_, overlay) => overlay,
    }
}

static CONFIG_PATH_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Overrides the config path for the rest of this process's lifetime
/// (`-c`/`--config`, see `main.rs`). Must be called before the first
/// `Config::load()`/`Config::path()` -- `config_path()` reads it on every
/// call, including the hot-reload watcher's own path lookup, so one
/// override this early covers both. First call wins; later calls are a
/// silent no-op, same as a `OnceLock` normally behaves.
pub fn set_config_path_override(path: PathBuf) {
    let _ = CONFIG_PATH_OVERRIDE.set(path);
}

fn config_path() -> PathBuf {
    if let Some(path) = CONFIG_PATH_OVERRIDE.get() {
        return path.clone();
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("tidewm").join("config.toml");
    }
    let Some(home) = std::env::var_os("HOME") else {
        // Same last-resort fallback `ipc.rs` uses for a missing
        // XDG_RUNTIME_DIR: a config that won't persist across reboots
        // beats a compositor that won't start at all in a stripped
        // environment (minimal container, restricted PAM session) that
        // happens to clear both variables.
        tracing::warn!("Neither XDG_CONFIG_HOME nor HOME is set; falling back to /tmp for config storage");
        return PathBuf::from("/tmp").join("tidewm-config").join("config.toml");
    };
    PathBuf::from(home)
        .join(".config")
        .join("tidewm")
        .join("config.toml")
}

/// Parses a keybind key like `"Super+Shift+Q"` into modifiers plus a base
/// (unshifted) key name, then resolves that name to a `Keysym`. Matching
/// happens against the unshifted symbol so a bind doesn't silently break
/// depending on whether the configured letter happened to be upper/lowercase.
fn parse_keybind(combo: &str, action: &str) -> Option<Keybind> {
    let mut mods = Mods::default();
    let mut key_name = None;

    for part in combo.split('+') {
        match part.to_lowercase().as_str() {
            "super" | "logo" | "mod4" => mods.logo = true,
            "ctrl" | "control" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            other => key_name = Some(other.to_string()),
        }
    }

    let key_name = key_name?;
    let keysym = xkb::keysym_from_name(&key_name, xkb::KEYSYM_CASE_INSENSITIVE);
    if keysym.raw() == 0 {
        tracing::warn!(key = %key_name, combo, "Unknown key name in keybind, skipping");
        return None;
    }

    let action = parse_action(action)?;
    Some(Keybind {
        mods,
        keysym,
        action,
    })
}

/// Also the entry point for `ipc.rs`'s `action` request -- the exact same
/// string syntax used in `[keybinds]`, so anything a keybind can trigger is
/// IPC-addressable for free, including actions added by later phases.
pub(crate) fn parse_action(action: &str) -> Option<Action> {
    if let Some(cmd) = action.strip_prefix("spawn:") {
        return Some(Action::Spawn(cmd.to_string()));
    }
    if let Some(n) = action.strip_prefix("workspace:") {
        return parse_workspace_number(n, action).map(Action::SwitchWorkspace);
    }
    if let Some(n) = action.strip_prefix("move-to-workspace:") {
        return parse_workspace_number(n, action).map(Action::MoveToWorkspace);
    }
    if let Some(name) = action.strip_prefix("swap-workspaces:") {
        return Some(Action::SwapWorkspacesWithOutput(name.to_string()));
    }
    if let Some(name) = action.strip_prefix("submap:") {
        return Some(Action::EnterSubmap(name.to_string()));
    }
    if let Some(name) = action.strip_prefix("layout:") {
        return match parse_layout_algorithm(name) {
            Some(algorithm) => Some(Action::SetLayout(algorithm)),
            None => {
                tracing::warn!(name, "Unknown layout algorithm in keybind, skipping");
                None
            }
        };
    }
    match action {
        "exit-submap" => Some(Action::ExitSubmap),
        "master-grow" => Some(Action::GrowMaster),
        "master-shrink" => Some(Action::ShrinkMaster),
        "toggle-overview" => Some(Action::ToggleOverview),
        "close-window" => Some(Action::CloseWindow),
        "toggle-floating" => Some(Action::ToggleFloating),
        "toggle-fullscreen" => Some(Action::ToggleFullscreen),
        "toggle-pin" => Some(Action::TogglePin),
        "toggle-scratchpad" => Some(Action::ToggleScratchpad),
        "move-to-scratchpad" => Some(Action::MoveToScratchpad),
        "toggle-pseudo-tile" => Some(Action::TogglePseudoTile),
        "cycle-focus" => Some(Action::CycleFocus),
        "focus-left" => Some(Action::FocusDirection(Direction::Left)),
        "focus-right" => Some(Action::FocusDirection(Direction::Right)),
        "focus-up" => Some(Action::FocusDirection(Direction::Up)),
        "focus-down" => Some(Action::FocusDirection(Direction::Down)),
        "swap-left" => Some(Action::SwapDirection(Direction::Left)),
        "swap-right" => Some(Action::SwapDirection(Direction::Right)),
        "swap-up" => Some(Action::SwapDirection(Direction::Up)),
        "swap-down" => Some(Action::SwapDirection(Direction::Down)),
        "group-left" => Some(Action::GroupDirection(Direction::Left)),
        "group-right" => Some(Action::GroupDirection(Direction::Right)),
        "group-up" => Some(Action::GroupDirection(Direction::Up)),
        "group-down" => Some(Action::GroupDirection(Direction::Down)),
        "ungroup" => Some(Action::Ungroup),
        "cycle-tab-next" => Some(Action::CycleTabForward),
        "cycle-tab-prev" => Some(Action::CycleTabBackward),
        "quit" => Some(Action::Quit),
        other => {
            tracing::warn!(action = %other, "Unknown keybind action, skipping");
            None
        }
    }
}

/// Parses `"bsp"`/`"master"` into a `LayoutAlgorithm`. Shared by
/// `"layout:<name>"` keybind actions and the top-level `default_layout`
/// config key; callers differ in what they do with an unrecognized name
/// (a keybind drops the whole bind, same as any other bad action string;
/// the top-level default warns and falls back to `Bsp` instead of failing
/// the whole config over one typo, matching `pseudo_tile_scale`'s
/// clamp-rather-than-reject precedent for a plain scalar setting).
fn parse_layout_algorithm(s: &str) -> Option<LayoutAlgorithm> {
    match s {
        "bsp" => Some(LayoutAlgorithm::Bsp),
        "master" => Some(LayoutAlgorithm::Master),
        _ => None,
    }
}

/// Parses the `N` in `"workspace:N"`/`"move-to-workspace:N"`. `full_action`
/// is only for the warning message, so a bad config points at what was
/// actually written.
fn parse_workspace_number(n: &str, full_action: &str) -> Option<u32> {
    match n.parse::<u32>() {
        Ok(n) => Some(n),
        Err(_) => {
            tracing::warn!(action = %full_action, "Invalid workspace number, skipping");
            None
        }
    }
}

const DEFAULT_CONFIG_TOML: &str = r#"# TideWM configuration.
# See DOCUMENTATION.md in the TideWM repo for the full reference.

# Split this file across others (Hyprland's "source" idea) by adding an
# `include` array -- each path is resolved relative to *this* file's own
# directory, and can itself include further files. A later entry overlays
# an earlier one, but whatever this file sets below always wins over
# anything it includes. [keybinds]/[submap.*] tables merge key-by-key
# across files; [[output]]/[[window_rule]] arrays all accumulate.
# include = ["monitors.toml", "keybinds.toml"]

# Change this (and the matching "spawn:" keybind below) to whatever
# terminal you have, e.g. "kitty", "alacritty", "foot".
terminal = "kitty"
# Shows a one-time startup message pointing you at Super+Enter (open a
# terminal). Delete this line, or set it to false, to stop seeing it.
show_welcome_hint = true
water_effects = true
# Forces the on-screen pointer to stay visible even when a client asks to
# hide it (e.g. a terminal hiding its own cursor glyph after inactivity).
# Off by default -- respecting a client's own hide request is correct
# behavior; this is an opt-in override. udev backend only (winit never
# draws its own cursor).
cursor_always_visible = false
gaps = 8
# Starting tiling algorithm for a workspace with no runtime override (see
# the "layout:bsp"/"layout:master" keybinds below). "bsp" is this
# project's existing adaptive engine -- already Hyprland's own "dwindle"
# behavior (split orientation follows each window's own aspect ratio) --
# "master" is one master pane plus an evenly-split stack (dwm/Hyprland's
# "master" layout), always left/right regardless of output aspect ratio.
default_layout = "bsp"
# Fraction of its tile a pseudo-tiled window keeps, centered within it
# (see the "toggle-pseudo-tile" keybind below). 0-1; clamped on load.
pseudo_tile_scale = 0.7

# Commands to launch once at startup, e.g. a bar or wallpaper daemon.
# Args are split on whitespace (no shell involved), same as any other
# spawn in this config -- quoting/globs/pipes aren't supported; wrap in
# `sh -c "..."` yourself if you need those.
spawn_at_startup = []
# spawn_at_startup = ["waybar", "swaybg -i ~/wallpaper.png"]

# Environment variables for TideWM's own process, applied before the
# backend starts (so e.g. XCURSOR_THEME here affects the cursor theme
# TideWM itself loads, not just child processes) and pushed into the
# systemd/D-Bus session-activation environment alongside WAYLAND_DISPLAY,
# so anything session-activated (a portal backend, a polkit agent) sees
# them too -- same idea as Hyprland's "env = KEY,VALUE" lines.
# [env]
# XCURSOR_THEME = "Adwaita"
# XCURSOR_SIZE = "24"
# QT_QPA_PLATFORMTHEME = "gtk3"
# GDK_BACKEND = "wayland"

# `$name` values, substituted into terminal/spawn_at_startup/keybinds/
# submaps/switch_events (Hyprland's own "$mainMod"/"$terminal" idea) so a
# key or command can be defined once and reused. Only names defined here
# are ever substituted -- any other "$" (a spawn command's own $HOME,
# $PATH, ...) is left exactly as written.
# [variables]
# mainMod = "Super"
# terminal = "kitty"
# then use them below, e.g. "$mainMod+Return" = "spawn:$terminal"

[keybinds]
"Super+Return" = "spawn:kitty"
"Super+Q" = "close-window"
"Super+V" = "toggle-floating"
"Super+F" = "toggle-fullscreen"
"Super+Tab" = "cycle-focus"
"Super+H" = "focus-left"
"Super+L" = "focus-right"
"Super+K" = "focus-up"
"Super+J" = "focus-down"
"Super+Shift+H" = "swap-left"
"Super+Shift+L" = "swap-right"
"Super+Shift+K" = "swap-up"
"Super+Shift+J" = "swap-down"
"Super+Shift+Q" = "quit"
# Un-tiles the focused window (floating it first, if it isn't already)
# and keeps it visible across every workspace switch on its output --
# handy for a music player or notes app you always want on top.
"Super+P" = "toggle-pin"
# Shrinks the focused tiled window to pseudo_tile_scale of its tile,
# centered within it, instead of filling the tile -- handy for something
# like a calculator or picture-in-picture video that doesn't want to
# stretch. Stays tiled (unlike floating): its slot in the layout is
# unchanged, just the rect it actually renders at.
"Super+Shift+P" = "toggle-pseudo-tile"
# Switches the current workspace between the two tiling algorithms (see
# default_layout above). "master-grow"/"master-shrink" nudge the master/
# stack ratio in steps (dwm/Hyprland's convention) -- a no-op while bsp
# is active, since there's no master ratio for it to affect.
"Super+W" = "layout:bsp"
"Super+Shift+W" = "layout:master"
"Super+Ctrl+Minus" = "master-shrink"
"Super+Ctrl+Equal" = "master-grow"
# Shows/hides a schematic grid of every workspace on the current output
# (rects + titles, not live thumbnails of window content -- see AGENT.md's
# Overview note for why). Press again to dismiss.
"Super+O" = "toggle-overview"
# Merges the focused tiled window with its neighbor into one shared tab
# slot (i3/sway's "tabbed container" idea) -- cycle between them with
# Super+]/Super+[, split the focused tab back out with Super+Shift+G.
"Super+Ctrl+H" = "group-left"
"Super+Ctrl+L" = "group-right"
"Super+Ctrl+K" = "group-up"
"Super+Ctrl+J" = "group-down"
"Super+Shift+G" = "ungroup"
"Super+BracketRight" = "cycle-tab-next"
"Super+BracketLeft" = "cycle-tab-prev"
# i3/sway's own default scratchpad binds: a hidden holding workspace,
# shown/hidden with one key rather than switched to like a normal one.
"Super+Minus" = "toggle-scratchpad"
"Super+Shift+Minus" = "move-to-scratchpad"
# Workspaces 1-9 on their own number key, 10 on the "0" key, matching
# i3/sway's convention. Super+Shift+<N> moves the focused window there
# without switching your own view to it.
"Super+1" = "workspace:1"
"Super+2" = "workspace:2"
"Super+3" = "workspace:3"
"Super+4" = "workspace:4"
"Super+5" = "workspace:5"
"Super+6" = "workspace:6"
"Super+7" = "workspace:7"
"Super+8" = "workspace:8"
"Super+9" = "workspace:9"
"Super+0" = "workspace:10"
"Super+Shift+1" = "move-to-workspace:1"
"Super+Shift+2" = "move-to-workspace:2"
"Super+Shift+3" = "move-to-workspace:3"
"Super+Shift+4" = "move-to-workspace:4"
"Super+Shift+5" = "move-to-workspace:5"
"Super+Shift+6" = "move-to-workspace:6"
"Super+Shift+7" = "move-to-workspace:7"
"Super+Shift+8" = "move-to-workspace:8"
"Super+Shift+9" = "move-to-workspace:9"
"Super+Shift+0" = "move-to-workspace:10"
# Swaps what's on this output with what's on the named one -- no default
# bind, output names are machine-specific (check your logs, or the
# `outputs` IPC query, for what TideWM calls yours). Uncomment and fill
# in your own second monitor's name to use it.
# "Super+Shift+O" = "swap-workspaces:DP-2"
# Enters the "nav" submap below (sway/Hyprland's "mode" idea): a
# temporary alternate keybind table, active until its own exit-submap
# bind, not tied to focus. Query which submap (if any) is currently
# active via the IPC socket's `active-submap` request, or `tidectl
# active-submap`.
"Super+N" = "submap:nav"

# A submap: vim-motion focus-move with no modifier held, since you're
# already "in a mode." A resize mode is the more common example
# elsewhere, but needs a keyboard resize action this project doesn't
# have yet. Add more [submap.<name>] tables the same way for others.
[submap.nav]
h = "focus-left"
l = "focus-right"
k = "focus-up"
j = "focus-down"
Escape = "exit-submap"

[input]
repeat_delay = 200
repeat_rate = 25
focus_follows_mouse = true
# Keyboard layout (xkbcommon rules/model/layout/variant/options). Leave
# these unset and xkbcommon falls back to your XKB_DEFAULT_* env vars, same
# as today. A comma-separated layout list plus xkb_options is how you get a
# switchable multi-layout setup (setxkbmap/Hyprland/niri use the same
# syntax), e.g. xkb_layout = "us,de" with a grp: toggle option below.
# xkb_layout = "us"
# xkb_variant = ""
# xkb_options = "grp:alt_shift_toggle"
# xkb_model = ""
# xkb_rules = ""

# Touchpad settings, udev backend only -- winit's nested host input can't
# reach a real libinput device. Every key is opt-in: leave it commented out
# and that setting is untouched, using whatever your driver already
# defaults to. Takes effect for a touchpad connected at or after startup;
# an edit here needs a restart to reach one already connected.
[input.touchpad]
# tap_to_click = true
# tap_and_drag = true
# drag_lock = false
# disable_while_typing = true
# natural_scroll = true
# left_handed = false
# middle_emulation = false
# click_method = "clickfinger"   # or "button-areas"
# scroll_method = "two-finger"   # or "edge", "on-button-down", "none"
# accel_speed = 0.0              # -1.0 (slowest) .. 1.0 (fastest)
# accel_profile = "adaptive"     # or "flat"

[xwayland]
enabled = true
path = "xwayland-satellite"

# Per-output overrides, udev backend only. Purely opt-in -- omit entirely
# and every connected output just auto-configures (preferred mode,
# auto-positioned, scale 1, no rotation), same as today. `name` is the
# connector name (check your logs for what TideWM detected, e.g.
# "eDP-1", "DP-2"). Every other field is optional; specify only what
# you want to override.
#
# [[output]]
# name = "eDP-1"
# enabled = true
# mode = "1920x1080@60"
# position = [0, 0]
# scale = 1.0
# transform = "normal"  # or "90", "180", "270", "flipped", "flipped-90", "flipped-180", "flipped-270"

# Laptop lid / tablet-mode switch events, udev backend only (libinput's
# switch capability isn't reachable through winit's nested-session
# backend). Each entry takes the same action string [keybinds] does --
# "spawn:...", "close-window", "workspace:N", anything -- but in practice
# you almost always want "spawn:", since the things you'd want to react
# with (suspend, screen lock, brightness, an onboard keyboard) live
# outside the compositor. All four default to empty (no action); comment
# in whichever you want. systemd-logind already triggers suspend on lid
# close independently of this (its `HandleLidSwitch=` policy in
# /etc/systemd/logind.conf), so a `lid_close` entry here is for whatever
# extra you want on top of that, not a replacement for it. (No logind?
# Nothing suspends on lid-close on its own -- put "spawn:systemctl suspend"
# or your init's equivalent here.)
#
# [switch_events]
# lid_close = "spawn:systemctl suspend"
# lid_open = "spawn:brightnessctl s 50%"
# tablet_mode_on = "spawn:onboard"
# tablet_mode_off = "spawn:pkill onboard"

# Per-app placement applied the moment a window first maps, before it's
# ever tiled/rendered at its default spot (i3/sway's "for_window",
# Hyprland's "windowrule" idea). Purely opt-in -- no rules, no behavior
# change. `app_id` matches exactly; `title` matches case-insensitively
# anywhere in the string. At least one of the two is required, or the rule
# never matches anything. Multiple [[window_rule]] blocks can match the
# same window; workspace/output take the last match, float/pseudo_tile/pin
# accumulate (any match sets it).
#
# [[window_rule]]
# app_id = "pavucontrol"
# float = true
#
# [[window_rule]]
# title = "Picture-in-Picture"
# float = true
# pin = true
#
# [[window_rule]]
# app_id = "Slack"
# workspace = 3
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_events_parse_valid_actions_and_drop_invalid_ones() {
        // Same string syntax `[keybinds]` uses, including the colon-prefixed
        // forms. An unrecognized action string matches the keybind path:
        // logged as a warning, dropped to `None`, never panics the whole
        // config -- a typo in one switch event must not silently disable
        // the other three.
        let parsed = SwitchEventsConfig::from_raw(SwitchEventsRaw {
            lid_close: Some("spawn:systemctl suspend".to_string()),
            lid_open: Some("close-window".to_string()),
            tablet_mode_on: Some("workspace:2".to_string()),
            tablet_mode_off: Some("not-a-real-action".to_string()),
        });

        assert!(matches!(parsed.lid_close, Some(Action::Spawn(_))));
        assert!(matches!(parsed.lid_open, Some(Action::CloseWindow)));
        assert!(matches!(parsed.tablet_mode_on, Some(Action::SwitchWorkspace(2))));
        assert!(parsed.tablet_mode_off.is_none());
    }

    #[test]
    fn switch_events_default_to_all_unset() {
        // Empty TOML section -> no actions, switch events become a pure
        // no-op. This is what ships out of the box and what an empty
        // `[switch_events]` block in an existing config produces.
        let parsed = SwitchEventsConfig::from_raw(SwitchEventsRaw::default());

        assert!(parsed.lid_close.is_none());
        assert!(parsed.lid_open.is_none());
        assert!(parsed.tablet_mode_on.is_none());
        assert!(parsed.tablet_mode_off.is_none());
    }

    #[test]
    fn window_rule_matches_app_id_exactly_and_title_by_substring() {
        let by_app_id = WindowRule { app_id: Some("firefox".to_string()), ..Default::default() };
        assert!(by_app_id.matches(Some("firefox"), None));
        assert!(!by_app_id.matches(Some("firefox-nightly"), None));
        assert!(!by_app_id.matches(None, Some("Firefox")));

        let by_title = WindowRule { title: Some("Picture-in-Picture".to_string()), ..Default::default() };
        assert!(by_title.matches(None, Some("Video - picture-in-picture")));
        assert!(!by_title.matches(None, Some("Video")));
        assert!(!by_title.matches(None, None));

        let both = WindowRule {
            app_id: Some("firefox".to_string()),
            title: Some("pip".to_string()),
            ..Default::default()
        };
        assert!(!both.matches(Some("firefox"), Some("normal tab")));
        assert!(both.matches(Some("firefox"), Some("Video - PIP")));
    }

    #[test]
    fn window_rule_with_no_criteria_never_matches() {
        // A blank [[window_rule]] block (or one that's only actions, no
        // app_id/title) is far more likely to be a config mistake than an
        // intentional "match every window" -- never silently apply it.
        let blank = WindowRule { float: true, ..Default::default() };
        assert!(!blank.matches(Some("anything"), Some("anything")));
        assert!(!blank.matches(None, None));
    }

    #[test]
    fn resolve_window_rules_folds_last_scalar_wins_bools_accumulate() {
        let mut config = Config {
            terminal: String::new(),
            show_welcome_hint: false,
            water_effects: true,
            cursor_always_visible: false,
            gaps: 0,
            default_layout: LayoutAlgorithm::Bsp,
            pseudo_tile_scale: 0.7,
            keybinds: Vec::new(),
            input: InputConfig::default(),
            xwayland: XwaylandConfig::default(),
            spawn_at_startup: Vec::new(),
            outputs: Vec::new(),
            switch_events: SwitchEventsConfig::default(),
            submaps: HashMap::new(),
            env: HashMap::new(),
            window_rules: vec![
                WindowRule {
                    app_id: Some("kitty".to_string()),
                    workspace: Some(2),
                    float: true,
                    ..Default::default()
                },
                WindowRule {
                    app_id: Some("kitty".to_string()),
                    workspace: Some(5),
                    pin: true,
                    ..Default::default()
                },
                WindowRule {
                    app_id: Some("firefox".to_string()),
                    workspace: Some(9),
                    ..Default::default()
                },
            ],
        };
        // Only the two "kitty" rules should ever combine; the "firefox"
        // one must not leak in just because it's in the same list.
        let effective = config.resolve_window_rules(Some("kitty"), None);
        assert_eq!(effective.workspace, Some(5)); // later match overrides earlier
        assert!(effective.float); // set by the first match, not unset by the second
        assert!(effective.pin); // set by the second match

        config.window_rules.clear();
        let none_matched = config.resolve_window_rules(Some("kitty"), None);
        assert_eq!(none_matched.workspace, None);
        assert!(!none_matched.float);
    }

    #[test]
    fn substitute_variables_replaces_only_defined_names_leaves_other_dollars_alone() {
        let mut variables = HashMap::new();
        variables.insert("mainMod".to_string(), "SUPER".to_string());
        variables.insert("terminal".to_string(), "kitty".to_string());

        assert_eq!(substitute_variables("$mainMod+Return", &variables), "SUPER+Return");
        assert_eq!(substitute_variables("spawn:$terminal", &variables), "spawn:kitty");
        // $HOME/$PATH aren't defined variables -- must survive untouched,
        // since these commonly appear in real spawn commands and corrupting
        // them would be far worse than leaving an unknown $name alone.
        assert_eq!(substitute_variables("spawn:sh -c \"echo $HOME\"", &variables), "spawn:sh -c \"echo $HOME\"");
        // A bare trailing `$` (no identifier following) must not panic or loop.
        assert_eq!(substitute_variables("cost is $5", &variables), "cost is $5");
        // No variables defined at all -- short-circuits, returns unchanged.
        assert_eq!(substitute_variables("$mainMod+Q", &HashMap::new()), "$mainMod+Q");
    }

    #[test]
    fn load_raw_config_substitutes_variables_into_keybinds_and_spawn_at_startup() {
        let dir = TestDir::new("variables");
        let main = dir.write(
            "config.toml",
            r#"
            spawn_at_startup = ["$terminal --daemon"]
            [variables]
            mainMod = "SUPER"
            terminal = "kitty"
            [keybinds]
            "$mainMod+Return" = "spawn:$terminal"
            "#,
        );

        let raw = load_raw_config(&main).expect("should parse");
        assert_eq!(raw.spawn_at_startup, vec!["kitty --daemon".to_string()]);
        assert_eq!(raw.keybinds.get("SUPER+Return").map(String::as_str), Some("spawn:kitty"));
        assert!(raw.keybinds.get("$mainMod+Return").is_none());
    }

    #[test]
    fn merge_toml_merges_tables_recursively_concats_arrays_scalar_overlay_wins() {
        let base: toml::Value = toml::from_str(
            r#"
            terminal = "kitty"
            gaps = 8
            [keybinds]
            "Super+Q" = "close-window"
            [[output]]
            name = "eDP-1"
            "#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
            gaps = 10
            [keybinds]
            "Super+F" = "toggle-fullscreen"
            [[output]]
            name = "DP-2"
            "#,
        )
        .unwrap();

        let merged = merge_toml(base, overlay);

        // Scalar: overlay wins outright.
        assert_eq!(merged.get("gaps").and_then(toml::Value::as_integer), Some(10));
        // Untouched-by-overlay scalar survives from base.
        assert_eq!(merged.get("terminal").and_then(toml::Value::as_str), Some("kitty"));
        // Table: keys from both sides present, merged key-wise.
        let keybinds = merged.get("keybinds").unwrap();
        assert_eq!(keybinds.get("Super+Q").and_then(toml::Value::as_str), Some("close-window"));
        assert_eq!(keybinds.get("Super+F").and_then(toml::Value::as_str), Some("toggle-fullscreen"));
        // Array of tables: concatenated, not replaced.
        let outputs = merged.get("output").and_then(toml::Value::as_array).unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].get("name").and_then(toml::Value::as_str), Some("eDP-1"));
        assert_eq!(outputs[1].get("name").and_then(toml::Value::as_str), Some("DP-2"));
    }

    /// Sets up an isolated directory under the system temp dir for a single
    /// test, cleaned up on drop -- these tests exercise real file I/O
    /// (`load_toml_merged` resolving `include` paths relative to the file
    /// doing the including), which in-memory `toml::Value` construction
    /// can't cover.
    struct TestDir(PathBuf);
    impl TestDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("tidewm-config-test-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            path
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn load_toml_merged_resolves_relative_includes_with_including_files_own_keys_winning() {
        let dir = TestDir::new("includes");
        dir.write(
            "keybinds.toml",
            "[keybinds]\n\"Super+Q\" = \"close-window\"\n",
        );
        let main = dir.write(
            "config.toml",
            "include = [\"keybinds.toml\"]\nterminal = \"kitty\"\n[keybinds]\n\"Super+F\" = \"toggle-fullscreen\"\n",
        );

        let merged = load_toml_merged(&main).expect("include chain should resolve");

        assert_eq!(merged.get("terminal").and_then(toml::Value::as_str), Some("kitty"));
        // Included file's key survives (tables merge key-wise)...
        let keybinds = merged.get("keybinds").unwrap();
        assert_eq!(keybinds.get("Super+Q").and_then(toml::Value::as_str), Some("close-window"));
        // ...and the including file's own key is present too, not clobbered
        // by the include (merge order: include first, own keys folded on top).
        assert_eq!(keybinds.get("Super+F").and_then(toml::Value::as_str), Some("toggle-fullscreen"));
        // The `include` directive itself must not leak into the merged value.
        assert!(merged.get("include").is_none());
    }

    #[test]
    fn load_toml_merged_later_include_overlays_earlier_one() {
        let dir = TestDir::new("include-order");
        dir.write("a.toml", "gaps = 4\n");
        dir.write("b.toml", "gaps = 12\n");
        let main = dir.write("config.toml", "include = [\"a.toml\", \"b.toml\"]\n");

        let merged = load_toml_merged(&main).unwrap();
        assert_eq!(merged.get("gaps").and_then(toml::Value::as_integer), Some(12));
    }

    #[test]
    fn load_toml_merged_skips_a_missing_include_instead_of_failing_the_whole_load() {
        let dir = TestDir::new("missing-include");
        let main = dir.write(
            "config.toml",
            "include = [\"does-not-exist.toml\"]\nterminal = \"kitty\"\n",
        );

        let merged = load_toml_merged(&main).expect("a bad include must not fail the top-level file");
        assert_eq!(merged.get("terminal").and_then(toml::Value::as_str), Some("kitty"));
    }

    #[test]
    fn load_toml_merged_detects_include_cycles_without_infinite_recursion() {
        // A cycle is caught deep in the include graph (b -> a, while a is
        // still an in-progress ancestor), the same place any other broken
        // include would be caught -- so it takes the same "log a warning,
        // skip just that include, keep going" path as a missing file,
        // rather than failing the whole top-level load. What actually
        // matters here is that this terminates at all instead of
        // recursing forever; if it didn't, this test would hang, not fail.
        let dir = TestDir::new("cycle");
        dir.write("b.toml", "include = [\"a.toml\"]\n");
        let a = dir.write("a.toml", "include = [\"b.toml\"]\nterminal = \"kitty\"\n");

        let merged = load_toml_merged(&a).expect("a cycle is skipped with a warning, not a hard failure");
        assert_eq!(merged.get("terminal").and_then(toml::Value::as_str), Some("kitty"));
    }

    #[test]
    fn default_submap_parses_from_both_the_in_memory_and_written_defaults() {
        // Two independently hand-maintained representations of the same
        // default (see `RawConfig::default()` and `DEFAULT_CONFIG_TOML`'s
        // own doc note) -- assert both actually agree, not just that one
        // of them happens to parse.
        for config in [Config::from_raw(RawConfig::default()), {
            let raw: RawConfig = toml::from_str(DEFAULT_CONFIG_TOML).expect("DEFAULT_CONFIG_TOML must parse");
            Config::from_raw(raw)
        }] {
            let nav = config.submaps.get("nav").expect("default config should ship a `nav` submap");
            let find = |key: &str| nav.iter().find(|b| b.keysym == xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE));
            assert!(matches!(find("h").map(|b| &b.action), Some(Action::FocusDirection(Direction::Left))));
            assert!(matches!(find("l").map(|b| &b.action), Some(Action::FocusDirection(Direction::Right))));
            assert!(matches!(find("k").map(|b| &b.action), Some(Action::FocusDirection(Direction::Up))));
            assert!(matches!(find("j").map(|b| &b.action), Some(Action::FocusDirection(Direction::Down))));
            assert!(matches!(find("Escape").map(|b| &b.action), Some(Action::ExitSubmap)));

            let enters_nav = config
                .keybinds
                .iter()
                .any(|b| matches!(&b.action, Action::EnterSubmap(name) if name == "nav"));
            assert!(enters_nav, "default keybinds should have a bind entering the `nav` submap");
        }
    }

    #[test]
    fn default_layout_keybinds_parse_from_both_the_in_memory_and_written_defaults() {
        // Same two-representations-must-agree check as the submap test
        // above, for the layout-algorithm keybinds: also catches, for free,
        // a duplicate TOML key in DEFAULT_CONFIG_TOML (the parse itself
        // would fail loudly rather than silently overwrite, unlike the
        // in-memory HashMap side).
        for config in [Config::from_raw(RawConfig::default()), {
            let raw: RawConfig = toml::from_str(DEFAULT_CONFIG_TOML).expect("DEFAULT_CONFIG_TOML must parse");
            Config::from_raw(raw)
        }] {
            let find = |key: &str, mods: Mods| {
                config
                    .keybinds
                    .iter()
                    .find(|b| b.keysym == xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE) && b.mods == mods)
            };
            let logo = Mods { logo: true, ..Default::default() };
            let logo_shift = Mods { logo: true, shift: true, ..Default::default() };
            let logo_ctrl = Mods { logo: true, ctrl: true, ..Default::default() };

            assert!(matches!(find("w", logo).map(|b| &b.action), Some(Action::SetLayout(LayoutAlgorithm::Bsp))));
            assert!(matches!(find("w", logo_shift).map(|b| &b.action), Some(Action::SetLayout(LayoutAlgorithm::Master))));
            assert!(matches!(find("minus", logo_ctrl).map(|b| &b.action), Some(Action::ShrinkMaster)));
            assert!(matches!(find("equal", logo_ctrl).map(|b| &b.action), Some(Action::GrowMaster)));
        }
    }
}
