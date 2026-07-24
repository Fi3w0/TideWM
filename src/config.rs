use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use smithay::input::keyboard::{xkb, Keysym, ModifiersState, XkbConfig};
use smithay::reexports::calloop::channel::{self, Channel};

use crate::waves;

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

/// Which side the master pane sits on under `LayoutAlgorithm::Master`
/// (Hyprland master's own `orientation` key, minus its niche `center`
/// variant -- no analog need surfaced for it, skip outright rather than
/// build unused complexity). `Left`/`Right` stack the other windows
/// vertically in the remaining strip; `Top`/`Bottom` stack them
/// horizontally instead -- matches Hyprland's own actual behavior for
/// those two, not just a naive 90-degree rotation of left/right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MasterOrientation {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

/// Manual override for `BspLayout`'s per-split axis choice (Hyprland
/// dwindle's `force_split` idea, scoped down to just the axis: this project
/// has no analog for `split_width_multiplier`/`smart_split`'s mouse-position
/// insertion logic or `permanent_direction_override`'s transient
/// preselect-next-window state -- no request for either). `Auto` (default)
/// is the existing, unchanged aspect-ratio-driven behavior (the module's
/// own deliberate identity, see `layout.rs`'s doc comment) -- this is
/// strictly an opt-in escape hatch for anyone who wants every split forced
/// one way regardless of window/output shape, not a change to the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SplitBias {
    #[default]
    Auto,
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// A `"workspace:N"`/`"move-to-workspace:N"` target, addressed either by
/// its raw number or by a `workspace_name` alias (niri's
/// `set-workspace-name`, Hyprland's `workspace name:foo` -- purely an
/// addressing convenience layered on the existing numbered model, not a
/// second workspace identity: the workspace itself is still just its
/// `u32`, same as an unnamed one). Kept unresolved at parse time --
/// `parse_action` has no `Config` to look a name up against -- and resolved
/// at the point of use by `Smallvil::resolve_workspace_ref`.
#[derive(Debug, Clone)]
pub enum WorkspaceRef {
    Number(u32),
    Name(String),
}

#[derive(Debug, Clone)]
pub enum Action {
    Spawn(String),
    CloseWindow,
    ToggleFloating,
    ToggleFullscreen,
    TogglePin,
    /// `None` is the classic single scratchpad; `Some(name)` a named one
    /// (Hyprland's named special workspaces) -- see
    /// `Smallvil::scratchpad_workspace`.
    ToggleScratchpad(Option<String>),
    MoveToScratchpad(Option<String>),
    TogglePseudoTile,
    /// Raises/lowers the focused *floating* window within the floating
    /// stack (a no-op on a tiled one -- tiled windows never overlap, so
    /// z-order has no meaning there, same reasoning `TogglePseudoTile`'s
    /// tiled-only restriction uses in reverse). See `Smallvil::raise_window`/
    /// `lower_window`.
    RaiseWindow,
    LowerWindow,
    /// Focuses whichever window is currently marked urgent, if any -- the
    /// bindable counterpart to a bar reading the `urgent` IPC flag. See
    /// `Smallvil::focus_urgent`.
    FocusUrgent,
    /// Toggles every output's power together (all on, or all off). See
    /// `Smallvil::toggle_dpms`.
    ToggleDpms,
    CycleFocus,
    FocusDirection(Direction),
    SwapDirection(Direction),
    /// Keyboard-driven resize. Right/down grow and left/up shrink the
    /// focused window along that axis; floating windows change by pixels,
    /// BSP windows move their nearest enclosing split.
    Resize(Direction),
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
    SwitchWorkspace(WorkspaceRef),
    MoveToWorkspace(WorkspaceRef),
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
    /// Hides the udev backend's software cursor after this many
    /// milliseconds of no real pointer motion (niri's
    /// `cursor.hide-after-inactive-ms`). `0` (default) disables it --
    /// independent of `cursor_always_visible` just above: that key only
    /// overrides a *client's* hide request, this is a separate
    /// compositor-driven timer, and the two can be combined (e.g. always
    /// override a client hiding it, but still auto-hide after idling).
    /// See `Smallvil::note_pointer_motion`.
    pub cursor_hide_after_ms: i32,
    /// niri's `workspace-auto-back-and-forth`: re-selecting the
    /// already-active workspace jumps back to whichever one was active
    /// immediately before it, instead of no-opping. Off by default --
    /// matches the existing plain-no-op behavior unless opted into.
    pub workspace_auto_back_and_forth: bool,
    /// Persistent `name -> workspace number` aliases, from repeatable
    /// `workspace_name = <N> <name>` config lines (see `WorkspaceRef`,
    /// `Smallvil::resolve_workspace_ref`). A workspace's real identity
    /// stays its number -- this is purely an addressing convenience, not a
    /// second identity, so nothing but action-string resolution reads it.
    pub workspace_names: HashMap<String, u32>,
    pub gaps: i32,
    /// Per-workspace gap overrides from repeatable
    /// `workspace_gaps = <N|name> <pixels>` lines; a workspace not in the
    /// map uses its output's `gaps` override, then the global `gaps`.
    /// Same accumulating-line shape as `workspace_name`, and a name here
    /// resolves through `workspace_names`.
    pub workspace_gaps: HashMap<u32, i32>,
    /// Starting tiling algorithm for a workspace with no runtime override
    /// (see `"layout:bsp"`/`"layout:master"` keybind actions,
    /// `layout::Layouts::set_default_algorithm`). Read fresh on every
    /// config reload (`Smallvil::reload_config`), same as `gaps`.
    pub default_layout: LayoutAlgorithm,
    /// Which side the master pane sits on under `LayoutAlgorithm::Master`
    /// (see `layout::Layouts::set_master_orientation`). One global value,
    /// not per-workspace like `master_ratio` -- this is a taste setting,
    /// not something interactively adjusted per split.
    pub master_orientation: MasterOrientation,
    /// Manual override for BSP's per-split axis choice (see
    /// `layout::Layouts::set_split_bias`). `Auto` (default) is the existing
    /// aspect-ratio-driven behavior, unchanged. One global value, same
    /// "taste setting, not per-workspace" reasoning as `master_orientation`.
    pub bsp_split_bias: SplitBias,
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
    /// Layer-shell surfaces (bars, panels) excluded from screenshots and
    /// screencasts by namespace (niri's `layer-rule { block-out-from ... }`).
    /// See `LayerRule::matches`, `Config::layer_blocks_capture`.
    pub layer_rules: Vec<LayerRule>,
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
    /// Load `$XDG_CONFIG_HOME/tidewm/config.wave` (falling back to
    /// `~/.config/tidewm/config.wave`), writing out the default file on first
    /// run so there's something to edit. Falls back to in-memory defaults if
    /// the file can't be read or parsed.
    pub fn load() -> Self {
        Self::load_with_error().0
    }

    /// Startup variant of [`Config::load`] that also retains a hard parse
    /// error, plus any dropped-keybind/footgun-lint diagnostics from
    /// `from_raw`, for the compositor-owned error panel -- kept as two
    /// separate outputs (rather than one merged message) because they're
    /// different severities: a hard error means the previous/default
    /// config is what's actually in effect, while these diagnostics mean
    /// the parsed config applied but something in it deserves a look.
    /// Callers that only need the old fallback behavior can continue using
    /// `load()`.
    pub fn load_with_error() -> (Self, Option<String>, Vec<String>) {
        let path = config_path();

        let (raw, error) = if path.exists() {
            match load_raw_config(&path) {
                Ok(raw) => (raw, None),
                Err(err) => {
                    tracing::warn!(%err, path = %path.display(), "Failed to parse config, using defaults");
                    (RawConfig::default(), Some(err))
                }
            }
        } else {
            let default = RawConfig::default();
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(err) = fs::write(&path, DEFAULT_CONFIG_WAVE) {
                tracing::warn!(%err, path = %path.display(), "Failed to write default config");
            }
            (default, None)
        };

        let (config, warnings) = Self::from_raw(raw);
        (config, error, warnings)
    }

    /// Re-read the config file for a hot-reload. Unlike `load`, this never
    /// writes a default file and reports a hard parse failure instead of
    /// silently falling back, so a reload with a typo in it doesn't quietly
    /// wipe out whatever the user had. The `Vec<String>` alongside a
    /// successful reload is the same dropped-keybind/footgun-lint
    /// diagnostics `from_raw` produces -- empty in the common case.
    pub fn reload() -> Result<(Self, Vec<String>), String> {
        let raw = load_raw_config(&config_path())?;
        Ok(Self::from_raw(raw))
    }

    /// The path being watched/loaded, so callers can set up a file watcher on it.
    pub fn path() -> PathBuf {
        config_path()
    }

    /// Returns the parsed config plus any diagnostics worth showing on the
    /// compositor-owned panel (dropped keybind entries, footgun lints) --
    /// see `parse_keybind`. Empty when nothing needs a second look.
    fn from_raw(raw: RawConfig) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let keybinds = raw
            .keybinds
            .iter()
            .filter_map(|(combo, action)| parse_keybind(combo, action, true, &mut warnings))
            .collect();
        let submaps = raw
            .submaps
            .iter()
            .map(|(name, binds)| {
                let parsed = binds
                    .iter()
                    .filter_map(|(combo, action)| {
                        parse_keybind(combo, action, false, &mut warnings)
                    })
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
        let master_orientation = parse_master_orientation(&raw.master_orientation).unwrap_or_else(|| {
            if !raw.master_orientation.is_empty() {
                tracing::warn!(value = %raw.master_orientation, "Unknown master_orientation, using left");
            }
            MasterOrientation::Left
        });
        let bsp_split_bias = parse_split_bias(&raw.bsp_split_bias).unwrap_or_else(|| {
            if !raw.bsp_split_bias.is_empty() {
                tracing::warn!(value = %raw.bsp_split_bias, "Unknown bsp_split_bias, using auto");
            }
            SplitBias::Auto
        });
        let workspace_names = parse_workspace_names(&raw.workspace_names);
        let workspace_gaps = parse_workspace_gaps(&raw.workspace_gaps, &workspace_names);

        let config = Self {
            terminal: raw.terminal,
            show_welcome_hint: raw.show_welcome_hint,
            water_effects: raw.water_effects,
            cursor_always_visible: raw.cursor_always_visible,
            cursor_hide_after_ms: raw.cursor_hide_after_ms,
            workspace_auto_back_and_forth: raw.workspace_auto_back_and_forth,
            workspace_names,
            workspace_gaps,
            gaps: raw.gaps,
            default_layout,
            master_orientation,
            bsp_split_bias,
            pseudo_tile_scale: raw.pseudo_tile_scale.clamp(0.05, 1.0),
            keybinds,
            input: raw.input,
            xwayland: raw.xwayland,
            spawn_at_startup: raw.spawn_at_startup,
            outputs: raw.outputs,
            switch_events: SwitchEventsConfig::from_raw(raw.switch_events),
            window_rules: raw.window_rules,
            layer_rules: raw.layer_rules,
            submaps,
            env: raw.env,
        };
        (config, warnings)
    }

    /// Folds every `[[window_rule]]` entry matching `app_id`/`title` into
    /// one effective rule: later matching entries override an earlier
    /// one's `workspace`/`output` (last match wins, so a more specific
    /// rule can sit after a general one and win), while `float`/
    /// `pseudo_tile`/`pin` accumulate (any match sets it, never unsets --
    /// booleans have no "leave alone" state to fall back to). Called once
    /// per newly-mapped window, not per frame, so folding rather than
    /// caching is fine.
    pub(crate) fn resolve_window_rules(
        &self,
        app_id: Option<&str>,
        title: Option<&str>,
    ) -> WindowRule {
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
            effective.tile |= rule.tile;
            effective.no_focus |= rule.no_focus;
            effective.maximize |= rule.maximize;
            effective.fullscreen |= rule.fullscreen;
            effective.block_capture |= rule.block_capture;
            if rule.position.is_some() {
                effective.position = rule.position;
            }
            if rule.size.is_some() {
                effective.size = rule.size;
            }
            if rule.opacity.is_some() {
                effective.opacity = rule.opacity;
            }
        }
        effective
    }

    /// Whether any `[[layer_rule]]` matching `namespace` sets
    /// `block_capture` -- a single boolean effect, so unlike
    /// `resolve_window_rules` there's nothing to fold across matches beyond
    /// "did any of them say yes."
    pub(crate) fn layer_blocks_capture(&self, namespace: &str) -> bool {
        self.layer_rules
            .iter()
            .any(|rule| rule.block_capture && rule.matches(namespace))
    }

    pub(crate) fn has_layer_capture_exclusions(&self) -> bool {
        self.layer_rules.iter().any(|rule| rule.block_capture)
    }
}

#[derive(Debug)]
struct RawConfig {
    terminal: String,
    show_welcome_hint: bool,
    water_effects: bool,
    cursor_always_visible: bool,
    cursor_hide_after_ms: i32,
    workspace_auto_back_and_forth: bool,
    gaps: i32,
    /// `"bsp"`/`"master"`, resolved via `parse_layout_algorithm` in
    /// `Config::from_raw`. Raw string (not `LayoutAlgorithm` itself) for
    /// the same reason `switch_events`/`keybinds` store strings -- so a
    /// bad value warns and falls back rather than failing the whole
    /// config parse.
    default_layout: String,
    /// `"left"`/`"right"`/`"top"`/`"bottom"`, resolved via
    /// `parse_master_orientation` -- same raw-string-then-resolve shape as
    /// `default_layout` just above, for the same reason.
    master_orientation: String,
    /// `"auto"`/`"horizontal"`/`"vertical"`, resolved via
    /// `parse_split_bias` -- same raw-string-then-resolve shape as
    /// `master_orientation` just above, for the same reason.
    bsp_split_bias: String,
    /// Raw `"<N> <name>"` lines, one per `workspace_name` entry, resolved
    /// via `parse_workspace_names`. Repeatable/accumulating like
    /// `spawn_at_startup` (see `waves::assign_is_multi`), not a scalar --
    /// a config defines many of these, one per named workspace.
    workspace_names: Vec<String>,
    /// Raw `"<N|name> <pixels>"` lines, one per `workspace_gaps` entry,
    /// resolved via `parse_workspace_gaps`. Accumulating like
    /// `workspace_name` (see `waves::assign_is_multi`).
    workspace_gaps: Vec<String>,
    pseudo_tile_scale: f64,
    keybinds: HashMap<String, String>,
    input: InputConfig,
    xwayland: XwaylandConfig,
    spawn_at_startup: Vec<String>,
    outputs: Vec<OutputConfig>,
    switch_events: SwitchEventsRaw,
    window_rules: Vec<WindowRule>,
    layer_rules: Vec<LayerRule>,
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
        keybinds.insert(
            "Super+BracketRight".to_string(),
            "cycle-tab-next".to_string(),
        );
        keybinds.insert(
            "Super+BracketLeft".to_string(),
            "cycle-tab-prev".to_string(),
        );
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
            terminal: "kitty".to_string(),
            // Deliberately false, not true: a real config.wave always ships
            // with `show_welcome_hint = true` written explicitly (see
            // DEFAULT_CONFIG_WAVE), so this default is only ever consulted
            // when a user deletes the key from an existing file -- and per
            // the on-screen hint's own "delete this to dismiss" advice
            // (welcome.rs), that must resolve to off, not back to on.
            show_welcome_hint: false,
            water_effects: true,
            cursor_always_visible: false,
            cursor_hide_after_ms: 0,
            workspace_auto_back_and_forth: false,
            gaps: 8,
            default_layout: String::new(),
            master_orientation: String::new(),
            bsp_split_bias: String::new(),
            workspace_names: Vec::new(),
            workspace_gaps: Vec::new(),
            pseudo_tile_scale: 0.7,
            keybinds,
            input: InputConfig::default(),
            xwayland: XwaylandConfig::default(),
            spawn_at_startup: Vec::new(),
            outputs: Vec::new(),
            switch_events: SwitchEventsRaw::default(),
            window_rules: Vec::new(),
            layer_rules: Vec::new(),
            submaps,
            env: HashMap::new(),
            variables: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
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
/// Re-applied to every already-known touchpad on a config reload too
/// (`Smallvil::known_touchpads`, populated/pruned from `DeviceAdded`/
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
#[derive(Debug, Default)]
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
#[derive(Debug, Clone, Default)]
pub struct WindowRule {
    pub app_id: Option<String>,
    pub title: Option<String>,
    /// Compiled once while loading the config. Keeping the compiled form is
    /// important because capture rules may be checked for every recorded
    /// frame, not only when the window first maps.
    pub app_id_regex: Option<regex::Regex>,
    pub title_regex: Option<regex::Regex>,
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
    /// Overrides the implicit auto-float heuristic (see `map_toplevel`'s
    /// `has_parent`/`is_fixed_size` check) back to tiled for a window that
    /// would otherwise auto-float -- the one flag in this struct that
    /// means "force tiled" rather than "force floating," so it's the only
    /// way to counteract the heuristic per-app. No effect if `float`/`pin`
    /// also match, same as niri's own `open-floating false` precedence.
    pub tile: bool,
    /// Suppresses the automatic focus-on-map a newly-mapped window
    /// normally gets (niri's `open-focused false`) -- useful for a
    /// background/scanner-style app that shouldn't steal focus from
    /// whatever's currently active. Leaves prior focus untouched entirely,
    /// rather than picking some other window to focus instead.
    pub no_focus: bool,
    pub maximize: bool,
    pub fullscreen: bool,
    pub block_capture: bool,
    /// Per-window render alpha in the inclusive range 0.0..=1.0. Applied
    /// to the complete surface tree (including subsurfaces and popups).
    pub opacity: Option<f32>,
    /// Exact floating placement (top-left corner), `<x>x<y>` -- the same
    /// syntax `[[output]]`'s `position` already uses. No-op unless the
    /// window ends up floating (from `float`/`pin`/the auto-float
    /// heuristic), same "only means something once floating" restriction
    /// `pseudo_tile` has in reverse for tiled.
    pub position: Option<(i32, i32)>,
    /// Exact floating size, `<width>x<height>` (same syntax as `position`).
    pub size: Option<(i32, i32)>,
}

impl WindowRule {
    /// A rule with neither `app_id` nor `title` set never matches anything,
    /// rather than silently matching every window -- a blank rule is far
    /// more likely to be a config mistake than an intentional "match all".
    pub(crate) fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        if self.app_id.is_none()
            && self.title.is_none()
            && self.app_id_regex.is_none()
            && self.title_regex.is_none()
        {
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
        if let Some(pattern) = &self.app_id_regex {
            let Some(app_id) = app_id else { return false };
            if !pattern.is_match(app_id) {
                return false;
            }
        }
        if let Some(pattern) = &self.title_regex {
            let Some(title) = title else { return false };
            if !pattern.is_match(title) {
                return false;
            }
        }
        true
    }
}

/// Matches a layer-shell surface by its `namespace` (the string a client
/// passes to `get_layer_surface`, e.g. a bar setting `"waybar"`) -- a layer
/// surface has no app_id/title the way an xdg toplevel does, so namespace
/// is the only thing to match on. One effect today: `block_capture`, letting
/// a sensitive layer surface (a password-manager quick-access panel, say)
/// opt out of screenshots/screencasts without hiding it from the user's own
/// screen -- niri's `layer-rule { block-out-from ... }`. See
/// `Smallvil::render_one_capture`'s excluded-rect pass.
#[derive(Debug, Clone, Default)]
pub struct LayerRule {
    pub namespace: Option<String>,
    pub block_capture: bool,
}

impl LayerRule {
    /// A rule with no `namespace` never matches, same "blank rule matches
    /// nothing" precedent `WindowRule::matches` sets.
    pub(crate) fn matches(&self, namespace: &str) -> bool {
        match &self.namespace {
            Some(want) => namespace.contains(want.as_str()),
            None => false,
        }
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
/// recursively, so a `keybinds.wave` sitting next to `config.wave` -- or in
/// a subdirectory -- is covered too) and forwards a `()` into the returned
/// calloop `Channel` whenever a `.wave` file under it changes. Keep the
/// returned `RecommendedWatcher` alive for as long as watching should
/// continue; dropping it stops the watch.
///
/// Recursive-plus-filtered rather than tracking the exact set of files an
/// `include` chain currently references: the include graph can only be
/// known *after* parsing (which itself needs the watcher to already be
/// running), and in every real layout -- this project's own default,
/// Hyprland's `~/.config/hypr/` -- every included file lives somewhere
/// under the same config directory anyway, so this covers the actual case
/// without re-deriving the watch list on every reload. The `.wave`
/// extension filter (plus skipping any path with a dotfile/dotdir
/// component) also keeps a config directory that's its own git repo (as
/// this user's real Hyprland config is) from spamming reloads on every
/// `.git/index` write a `git status`/`checkout` causes.
pub fn spawn_watcher() -> notify::Result<(RecommendedWatcher, Channel<Arc<AtomicBool>>)> {
    let (tx, rx) = channel::channel();
    let event_pending = Arc::new(AtomicBool::new(false));
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
            p.extension().is_some_and(|ext| ext == "wave")
                && !rel
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        });
        if relevant
            && !event_pending.swap(true, Ordering::AcqRel)
            && tx.send(event_pending.clone()).is_err()
        {
            event_pending.store(false, Ordering::Release);
        }
    })?;

    // `Config::load()` normally creates this directory already, but the
    // watcher needs it to exist regardless of load order.
    let _ = fs::create_dir_all(&watch_dir);
    if let Err(recursive_err) = watcher.watch(&watch_dir, RecursiveMode::Recursive) {
        // A custom `--config /tmp/foo.wave` is legitimate, but recursively
        // traversing a broad parent such as /tmp can encounter unrelated
        // systemd-private directories that are intentionally unreadable.
        // Keep the main config (and sibling includes) hot-reloadable instead
        // of disabling the watcher completely. Nested include directories
        // still get the full recursive behavior whenever the parent allows
        // it, which is the normal ~/.config/tidewm case.
        tracing::warn!(
            %recursive_err,
            path = %watch_dir.display(),
            "Recursive config watch failed; falling back to the config directory only"
        );
        watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
    }

    Ok((watcher, rx))
}

/// Reads `path` as Waves, resolving `include` statements first (see
/// `waves::resolve`), then lowers the merged entry list into a
/// `RawConfig`. The single entry point both `Config::load` and
/// `Config::reload` use so they stay consistent about how includes are
/// resolved.
fn load_raw_config(path: &Path) -> Result<RawConfig, String> {
    let entries = waves::resolve(path)?;
    let mut raw = lower_entries(&entries);
    substitute_variables_in_raw(&mut raw);
    Ok(raw)
}

/// Lowers a fully-merged Waves entry list into a `RawConfig`, starting
/// from its defaults and overwriting whatever was actually present.
/// Unknown keys/blocks warn and are ignored rather than failing the whole
/// config -- same forgiving convention TOML loading always used (a typo
/// shouldn't take down a working session).
fn lower_entries(entries: &[waves::Entry]) -> RawConfig {
    let mut raw = RawConfig::default();
    for entry in entries {
        match entry {
            waves::Entry::VarDef(name, value) => {
                raw.variables.insert(name.clone(), value.clone());
            }
            waves::Entry::Bind(combo, action) => {
                raw.keybinds.insert(combo.clone(), action.clone());
            }
            // Already resolved away by `waves::resolve` before this ever runs.
            waves::Entry::Include(_) => {}
            waves::Entry::Assign(key, value) => apply_top_level_assign(&mut raw, key, value),
            waves::Entry::Block(keyword, header, body) => {
                apply_top_level_block(&mut raw, keyword, header, body)
            }
        }
    }
    raw
}

fn apply_top_level_assign(raw: &mut RawConfig, key: &str, value: &str) {
    match key {
        "terminal" => raw.terminal = value.to_string(),
        "show_welcome_hint" => set_bool(&mut raw.show_welcome_hint, key, value),
        "water_effects" => set_bool(&mut raw.water_effects, key, value),
        "cursor_always_visible" => set_bool(&mut raw.cursor_always_visible, key, value),
        "cursor_hide_after_ms" => set_i32(&mut raw.cursor_hide_after_ms, key, value),
        "workspace_auto_back_and_forth" => {
            set_bool(&mut raw.workspace_auto_back_and_forth, key, value)
        }
        "gaps" => set_i32(&mut raw.gaps, key, value),
        "default_layout" => raw.default_layout = value.to_string(),
        "master_orientation" => raw.master_orientation = value.to_string(),
        "bsp_split_bias" => raw.bsp_split_bias = value.to_string(),
        "workspace_name" => raw.workspace_names.push(value.to_string()),
        "workspace_gaps" => raw.workspace_gaps.push(value.to_string()),
        "pseudo_tile_scale" => set_f64(&mut raw.pseudo_tile_scale, key, value),
        // List-shaped, not scalar -- accumulates because `waves::merge_into`
        // already let every occurrence of this one key through instead of
        // deduping to the last (see `waves::assign_is_multi`).
        "spawn_at_startup" => raw.spawn_at_startup.push(value.to_string()),
        other => tracing::warn!(key = %other, value, "Unknown config key, ignoring"),
    }
}

fn apply_top_level_block(raw: &mut RawConfig, keyword: &str, header: &str, body: &[waves::Entry]) {
    match keyword {
        "input" => apply_input_block(&mut raw.input, body),
        "xwayland" => apply_xwayland_block(&mut raw.xwayland, body),
        "output" => raw.outputs.push(lower_output_block(header, body)),
        "rule" => raw.window_rules.push(lower_window_rule_block(body)),
        "layer_rule" => raw.layer_rules.push(lower_layer_rule_block(body)),
        "submap" => {
            let name = header.trim();
            if name.is_empty() {
                tracing::warn!("`submap` block needs a name, ignoring");
                return;
            }
            let binds = raw.submaps.entry(name.to_string()).or_default();
            for entry in body {
                match entry {
                    waves::Entry::Bind(combo, action) => {
                        binds.insert(combo.clone(), action.clone());
                    }
                    _ => tracing::warn!(
                        name,
                        "A `submap` block may only contain `bind` statements, ignoring an entry"
                    ),
                }
            }
        }
        "env" => {
            for entry in body {
                match entry {
                    waves::Entry::Assign(key, value) => {
                        raw.env.insert(key.clone(), value.clone());
                    }
                    _ => tracing::warn!("An `env` block may only contain `key = value` assignments, ignoring an entry"),
                }
            }
        }
        "switch_events" => apply_switch_events_block(&mut raw.switch_events, body),
        other => tracing::warn!(keyword = %other, "Unknown config block, ignoring"),
    }
}

fn apply_input_block(input: &mut InputConfig, body: &[waves::Entry]) {
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) => match key.as_str() {
                "repeat_delay" => set_i32(&mut input.repeat_delay, key, value),
                "repeat_rate" => set_i32(&mut input.repeat_rate, key, value),
                "focus_follows_mouse" => set_bool(&mut input.focus_follows_mouse, key, value),
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
            _ => tracing::warn!("Unexpected entry in `input` block, ignoring"),
        }
    }
}

fn apply_touchpad_block(touchpad: &mut TouchpadConfig, body: &[waves::Entry]) {
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
            "accel_speed" => set_opt_f64(&mut touchpad.accel_speed, key, value),
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
            "workspace_swipe_distance" => {
                set_opt_f64(&mut touchpad.workspace_swipe_distance, key, value)
            }
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

fn apply_xwayland_block(xwayland: &mut XwaylandConfig, body: &[waves::Entry]) {
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

fn apply_switch_events_block(switch_events: &mut SwitchEventsRaw, body: &[waves::Entry]) {
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

fn lower_output_block(header: &str, body: &[waves::Entry]) -> OutputConfig {
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
            "scale" => set_f64(&mut cfg.scale, key, value),
            "transform" => match parse_transform(value) {
                Some(t) => cfg.transform = t,
                None => tracing::warn!(value, "Unknown transform, ignoring"),
            },
            "gaps" => match value.parse() {
                Ok(n) => cfg.gaps = Some(n),
                Err(_) => tracing::warn!(value, "Expected a pixel gap value, ignoring"),
            },
            other => tracing::warn!(key = %other, "Unknown key in `output` block, ignoring"),
        }
    }
    cfg
}

fn lower_window_rule_block(body: &[waves::Entry]) -> WindowRule {
    let mut rule = WindowRule::default();
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `rule` block, ignoring");
            continue;
        };
        match key.as_str() {
            "app_id" => rule.app_id = Some(value.clone()),
            "title" => rule.title = Some(value.clone()),
            "app_id_regex" => match regex::Regex::new(value) {
                Ok(pattern) => rule.app_id_regex = Some(pattern),
                Err(err) => tracing::warn!(value, %err, "Invalid app_id_regex, ignoring"),
            },
            "title_regex" => match regex::Regex::new(value) {
                Ok(pattern) => rule.title_regex = Some(pattern),
                Err(err) => tracing::warn!(value, %err, "Invalid title_regex, ignoring"),
            },
            "workspace" => match value.parse() {
                Ok(n) => rule.workspace = Some(n),
                Err(_) => tracing::warn!(value, "Expected a workspace number, ignoring"),
            },
            "output" => rule.output = Some(value.clone()),
            "float" => set_bool(&mut rule.float, key, value),
            "pseudo_tile" => set_bool(&mut rule.pseudo_tile, key, value),
            "pin" => set_bool(&mut rule.pin, key, value),
            "tile" => set_bool(&mut rule.tile, key, value),
            "no_focus" => set_bool(&mut rule.no_focus, key, value),
            "maximize" => set_bool(&mut rule.maximize, key, value),
            "fullscreen" => set_bool(&mut rule.fullscreen, key, value),
            "block_capture" => set_bool(&mut rule.block_capture, key, value),
            "opacity" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => rule.opacity = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected a finite opacity from 0.0 to 1.0, ignoring"),
            },
            "position" => match parse_position(value) {
                Some(pos) => rule.position = Some(pos),
                None => tracing::warn!(value, "Expected <x>x<y> for a rule's position, ignoring"),
            },
            "size" => match parse_position(value) {
                Some(size) => rule.size = Some(size),
                None => tracing::warn!(
                    value,
                    "Expected <width>x<height> for a rule's size, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `rule` block, ignoring"),
        }
    }
    rule
}

fn lower_layer_rule_block(body: &[waves::Entry]) -> LayerRule {
    let mut rule = LayerRule::default();
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `layer_rule` block, ignoring");
            continue;
        };
        match key.as_str() {
            "namespace" => rule.namespace = Some(value.clone()),
            "block_capture" => set_bool(&mut rule.block_capture, key, value),
            other => tracing::warn!(key = %other, "Unknown key in `layer_rule` block, ignoring"),
        }
    }
    rule
}

/// `"1920x0"` -> `(1920, 0)`, the same `WxH` shorthand `parse_mode_str`
/// already uses for resolution, reused here for position.
fn parse_position(s: &str) -> Option<(i32, i32)> {
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

/// Mutates `field` only on a successful parse -- a bad value logs a
/// warning and leaves whatever was already there (the default, or an
/// earlier include's value) rather than silently resetting it.
fn set_bool(field: &mut bool, key: &str, value: &str) {
    match value {
        "true" => *field = true,
        "false" => *field = false,
        _ => tracing::warn!(key, value, "Expected `true` or `false`, ignoring"),
    }
}

fn set_opt_bool(field: &mut Option<bool>, key: &str, value: &str) {
    match value {
        "true" => *field = Some(true),
        "false" => *field = Some(false),
        _ => tracing::warn!(key, value, "Expected `true` or `false`, ignoring"),
    }
}

fn set_i32(field: &mut i32, key: &str, value: &str) {
    match value.parse() {
        Ok(n) => *field = n,
        Err(_) => tracing::warn!(key, value, "Expected an integer, ignoring"),
    }
}

fn set_f64(field: &mut f64, key: &str, value: &str) {
    match value.parse() {
        Ok(n) => *field = n,
        Err(_) => tracing::warn!(key, value, "Expected a number, ignoring"),
    }
}

fn set_opt_f64(field: &mut Option<f64>, key: &str, value: &str) {
    match value.parse() {
        Ok(n) => *field = Some(n),
        Err(_) => tracing::warn!(key, value, "Expected a number, ignoring"),
    }
}

/// Replaces `$name` tokens with their `[variables]` value, but only for
/// names actually defined there -- Hyprland's own `$mainMod`/`$terminal`
/// convention (see their real `keybinds.conf`: `bind = $mainMod, Q, exec,
/// $terminal`). Every other `$` (a spawn command's own `$HOME`, `$PATH`,
/// anything not one of this config's variables) is left exactly as
/// written -- substituting *any* `$word` unconditionally would corrupt
/// those instead of just leaving them for the shell/program that actually
/// understands them.
///
/// `$wave(a, b, c)` is checked first, ahead of the plain-name lookup --
/// it's a built-in, not something `[variables]` could ever define, so it
/// works even with no variables set at all. See `resolve_wave_fallback`.
fn substitute_variables(s: &str, variables: &HashMap<String, String>) -> String {
    if !s.contains('$') {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(dollar) = rest.find('$') {
        result.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];

        if let Some(args) = after.strip_prefix("wave(") {
            if let Some(end) = args.find(')') {
                result.push_str(&resolve_wave_fallback(
                    args[..end].split(',').map(str::trim),
                ));
                rest = &args[end + 1..];
                continue;
            }
            // No closing `)` -- not actually a well-formed `$wave(...)`,
            // fall through to plain-name handling below instead of
            // silently eating the rest of the line.
        }

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

/// Resolves `$wave(a, b, c)` to the first candidate whose own first
/// whitespace-separated word names a real, executable file -- on `$PATH`,
/// or directly if it contains a `/`. Falls back to the last candidate,
/// untried, if none resolve, so a spawn still gets attempted and fails
/// with a normal, visible "command not found" rather than silently
/// spawning nothing; an empty candidate list resolves to an empty string.
fn resolve_wave_fallback<'a>(candidates: impl Iterator<Item = &'a str>) -> String {
    let candidates: Vec<&str> = candidates.collect();
    let found = candidates.iter().find(|c| command_exists(c));
    if found.is_none() {
        tracing::warn!(
            ?candidates,
            "$wave(...): none of these were found, using the last one anyway"
        );
    }
    found
        .or(candidates.last())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Whether `candidate`'s first whitespace-separated word is a real,
/// executable file -- checked directly if it contains a `/`, otherwise
/// searched on `$PATH`, the same resolution order a shell uses.
fn command_exists(candidate: &str) -> bool {
    let bin = candidate.split_whitespace().next().unwrap_or("");
    if bin.is_empty() {
        return false;
    }
    if bin.contains('/') {
        return is_executable_file(Path::new(bin));
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| is_executable_file(&dir.join(bin)))
    })
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Applies `substitute_variables` to every field that plausibly reuses a
/// variable -- keybind/submap combos and actions, `spawn_at_startup`,
/// `terminal`, `switch_events` -- deliberately skipping match criteria like
/// `[[window_rule]]`'s `app_id`/`title` and `[[output]]`'s connector name,
/// since those describe something to match against, not a command or bind
/// where reusing a value makes sense.
fn substitute_variables_in_raw(raw: &mut RawConfig) {
    // No early return on `raw.variables.is_empty()` -- `$wave(...)` is a
    // built-in `substitute_variables` resolves on its own, not something
    // `[variables]` defines, so it must still run with zero variables set.
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
    raw.switch_events.lid_close = raw
        .switch_events
        .lid_close
        .as_deref()
        .map(|s| substitute_variables(s, &raw.variables));
    raw.switch_events.lid_open = raw
        .switch_events
        .lid_open
        .as_deref()
        .map(|s| substitute_variables(s, &raw.variables));
    raw.switch_events.tablet_mode_on = raw
        .switch_events
        .tablet_mode_on
        .as_deref()
        .map(|s| substitute_variables(s, &raw.variables));
    raw.switch_events.tablet_mode_off = raw
        .switch_events
        .tablet_mode_off
        .as_deref()
        .map(|s| substitute_variables(s, &raw.variables));
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
        return PathBuf::from(dir).join("tidewm").join("config.wave");
    }
    let Some(home) = std::env::var_os("HOME") else {
        // Same last-resort fallback `ipc.rs` uses for a missing
        // XDG_RUNTIME_DIR: a config that won't persist across reboots
        // beats a compositor that won't start at all in a stripped
        // environment (minimal container, restricted PAM session) that
        // happens to clear both variables.
        tracing::warn!(
            "Neither XDG_CONFIG_HOME nor HOME is set; falling back to /tmp for config storage"
        );
        return PathBuf::from("/tmp")
            .join("tidewm-config")
            .join("config.wave");
    };
    PathBuf::from(home)
        .join(".config")
        .join("tidewm")
        .join("config.wave")
}

/// Parses a keybind key like `"Super+Shift+Q"` into modifiers plus a base
/// (unshifted) key name, then resolves that name to a `Keysym`. Matching
/// happens against the unshifted symbol so a bind doesn't silently break
/// depending on whether the configured letter happened to be upper/lowercase.
///
/// `warnings` collects anything worth surfacing on the compositor-owned
/// diagnostics panel instead of only a `tracing::warn!` line nobody sees
/// during normal use: dropped entries (bad key name, bad action), and, when
/// `lint_footguns` is set, a heads-up for a modifier-less bind on a key
/// normally used for typing. `lint_footguns` is false for submap tables --
/// bare keys there are the whole point of a submap (see the default `nav`
/// submap's bare `Escape = exit-submap`), only the always-active base
/// `[keybinds]` table can silently steal a key from every other window.
fn parse_keybind(
    combo: &str,
    action: &str,
    lint_footguns: bool,
    warnings: &mut Vec<String>,
) -> Option<Keybind> {
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

    let Some(key_name) = key_name else {
        warnings.push(format!("Keybind \"{combo}\" has no key, skipped"));
        return None;
    };
    let keysym = xkb::keysym_from_name(&key_name, xkb::KEYSYM_CASE_INSENSITIVE);
    if keysym.raw() == 0 {
        tracing::warn!(key = %key_name, combo, "Unknown key name in keybind, skipping");
        warnings.push(format!(
            "Unknown key \"{key_name}\" in keybind \"{combo}\", skipped"
        ));
        return None;
    }

    let Some(action) = parse_action(action) else {
        warnings.push(format!(
            "Unknown action \"{action}\" for keybind \"{combo}\", skipped"
        ));
        return None;
    };

    if lint_footguns
        && !mods.ctrl
        && !mods.alt
        && !mods.shift
        && !mods.logo
        && is_typing_key(&key_name)
    {
        warnings.push(format!(
            "\"{combo}\" has no modifier -- it will capture that key everywhere, including text fields"
        ));
    }

    Some(Keybind {
        mods,
        keysym,
        action,
    })
}

/// Keys almost never meant to be bound bare (no modifier) in the base
/// keybind table: doing so steals that key from every focused client,
/// including plain typing. Single ASCII letters/digits plus the common
/// editing keys. Deliberately excludes F-keys, media keys, and arrows,
/// which are commonly and safely bound bare.
fn is_typing_key(key_name: &str) -> bool {
    match key_name.to_lowercase().as_str() {
        "return" | "enter" | "kp_enter" | "tab" | "space" | "backspace" | "escape" => true,
        name => name.chars().count() == 1 && name.chars().next().unwrap().is_ascii_alphanumeric(),
    }
}

/// Also the entry point for `ipc.rs`'s `action` request -- the exact same
/// string syntax used in `[keybinds]`, so anything a keybind can trigger is
/// IPC-addressable for free, including actions added by later phases.
pub(crate) fn parse_action(action: &str) -> Option<Action> {
    if let Some(cmd) = action.strip_prefix("spawn:") {
        return Some(Action::Spawn(cmd.to_string()));
    }
    if let Some(n) = action.strip_prefix("workspace:") {
        return Some(Action::SwitchWorkspace(parse_workspace_ref(n)));
    }
    if let Some(n) = action.strip_prefix("move-to-workspace:") {
        return Some(Action::MoveToWorkspace(parse_workspace_ref(n)));
    }
    if let Some(name) = action.strip_prefix("swap-workspaces:") {
        return Some(Action::SwapWorkspacesWithOutput(name.to_string()));
    }
    if let Some(name) = action.strip_prefix("toggle-scratchpad:") {
        return Some(Action::ToggleScratchpad(
            (!name.is_empty()).then(|| name.to_string()),
        ));
    }
    if let Some(name) = action.strip_prefix("move-to-scratchpad:") {
        return Some(Action::MoveToScratchpad(
            (!name.is_empty()).then(|| name.to_string()),
        ));
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
        "toggle-scratchpad" => Some(Action::ToggleScratchpad(None)),
        "move-to-scratchpad" => Some(Action::MoveToScratchpad(None)),
        "toggle-pseudo-tile" => Some(Action::TogglePseudoTile),
        "raise-window" => Some(Action::RaiseWindow),
        "lower-window" => Some(Action::LowerWindow),
        "focus-urgent" => Some(Action::FocusUrgent),
        "toggle-dpms" => Some(Action::ToggleDpms),
        "cycle-focus" => Some(Action::CycleFocus),
        "focus-left" => Some(Action::FocusDirection(Direction::Left)),
        "focus-right" => Some(Action::FocusDirection(Direction::Right)),
        "focus-up" => Some(Action::FocusDirection(Direction::Up)),
        "focus-down" => Some(Action::FocusDirection(Direction::Down)),
        "swap-left" => Some(Action::SwapDirection(Direction::Left)),
        "swap-right" => Some(Action::SwapDirection(Direction::Right)),
        "swap-up" => Some(Action::SwapDirection(Direction::Up)),
        "swap-down" => Some(Action::SwapDirection(Direction::Down)),
        "resize-left" => Some(Action::Resize(Direction::Left)),
        "resize-right" => Some(Action::Resize(Direction::Right)),
        "resize-up" => Some(Action::Resize(Direction::Up)),
        "resize-down" => Some(Action::Resize(Direction::Down)),
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

fn parse_master_orientation(s: &str) -> Option<MasterOrientation> {
    match s {
        "left" => Some(MasterOrientation::Left),
        "right" => Some(MasterOrientation::Right),
        "top" => Some(MasterOrientation::Top),
        "bottom" => Some(MasterOrientation::Bottom),
        _ => None,
    }
}

fn parse_split_bias(s: &str) -> Option<SplitBias> {
    match s {
        "auto" => Some(SplitBias::Auto),
        "horizontal" => Some(SplitBias::Horizontal),
        "vertical" => Some(SplitBias::Vertical),
        _ => None,
    }
}

/// Parses every raw `"<N> <name>"` `workspace_name` line into a
/// `name -> number` map. A malformed entry (no name, or a non-numeric `N`)
/// warns and is skipped, same "bad config, log and move on" convention as
/// an unrecognized keybind action. A repeated name keeps the last entry,
/// same "last one wins" convention `resolve_window_rules` uses for a
/// scalar field.
/// Parses every raw `"<N|name> <pixels>"` `workspace_gaps` line into a
/// `workspace number -> gap` map. A name resolves through the already-parsed
/// `workspace_name` aliases; malformed entries warn and are skipped, same
/// convention as `parse_workspace_names`.
fn parse_workspace_gaps(raw: &[String], names: &HashMap<String, u32>) -> HashMap<u32, i32> {
    let mut gaps = HashMap::new();
    for entry in raw {
        let Some((workspace, pixels)) = entry.split_once(char::is_whitespace) else {
            tracing::warn!(entry, "workspace_gaps needs a workspace and a pixel value, ignoring");
            continue;
        };
        let number = match workspace.parse::<u32>() {
            Ok(n) => Some(n),
            Err(_) => names.get(workspace.trim()).copied(),
        };
        let (Some(number), Ok(pixels)) = (number, pixels.trim().parse::<i32>()) else {
            tracing::warn!(entry, "Invalid workspace or pixel value in workspace_gaps, ignoring");
            continue;
        };
        gaps.insert(number, pixels);
    }
    gaps
}

fn parse_workspace_names(raw: &[String]) -> HashMap<String, u32> {
    let mut names = HashMap::new();
    for entry in raw {
        let Some((number, name)) = entry.split_once(char::is_whitespace) else {
            tracing::warn!(entry, "workspace_name needs a number and a name, ignoring");
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            tracing::warn!(entry, "workspace_name needs a number and a name, ignoring");
            continue;
        }
        match number.parse::<u32>() {
            Ok(n) => {
                names.insert(name.to_string(), n);
            }
            Err(_) => tracing::warn!(
                entry,
                "Invalid workspace number in workspace_name, ignoring"
            ),
        }
    }
    names
}

/// The `N` in `"workspace:N"`/`"move-to-workspace:N"` -- a plain number
/// parses as-is, anything else is deferred as a `workspace_name` alias to
/// resolve later (`Smallvil::resolve_workspace_ref`), since parsing here
/// has no `Config` to check a name against yet.
fn parse_workspace_ref(n: &str) -> WorkspaceRef {
    match n.parse::<u32>() {
        Ok(n) => WorkspaceRef::Number(n),
        Err(_) => WorkspaceRef::Name(n.to_string()),
    }
}

const DEFAULT_CONFIG_WAVE: &str = r#"# TideWM configuration.
# Full reference: DOCUMENTATION.md in the TideWM repo.

# include "monitors.wave"
# include "keybinds.wave"

$mod = SUPER

terminal = $wave(kitty, alacritty, foot, xterm)
show_welcome_hint = true
water_effects = true
cursor_always_visible = false
cursor_hide_after_ms = 0
workspace_auto_back_and_forth = false
gaps = 8
default_layout = bsp
master_orientation = left
bsp_split_bias = auto
pseudo_tile_scale = 0.7

# spawn_at_startup = waybar
# spawn_at_startup = swaybg -i ~/wallpaper.png

# env {
#     XCURSOR_THEME = Adwaita
#     GDK_BACKEND = wayland
# }

# Windows
bind $mod+Return = spawn:kitty
bind $mod+Q = close-window
bind $mod+V = toggle-floating
bind $mod+F = toggle-fullscreen
bind $mod+P = toggle-pin
bind $mod+Shift+P = toggle-pseudo-tile
bind $mod+Shift+Q = quit

# Focus and layout
bind $mod+Tab = cycle-focus
bind $mod+H = focus-left
bind $mod+L = focus-right
bind $mod+K = focus-up
bind $mod+J = focus-down
bind $mod+Shift+H = swap-left
bind $mod+Shift+L = swap-right
bind $mod+Shift+K = swap-up
bind $mod+Shift+J = swap-down
bind $mod+W = layout:bsp
bind $mod+Shift+W = layout:master
bind $mod+Ctrl+Minus = master-shrink
bind $mod+Ctrl+Equal = master-grow
bind $mod+O = toggle-overview

# Groups (tabbing)
bind $mod+Ctrl+H = group-left
bind $mod+Ctrl+L = group-right
bind $mod+Ctrl+K = group-up
bind $mod+Ctrl+J = group-down
bind $mod+Shift+G = ungroup
bind $mod+BracketRight = cycle-tab-next
bind $mod+BracketLeft = cycle-tab-prev

# Scratchpad
bind $mod+Minus = toggle-scratchpad
bind $mod+Shift+Minus = move-to-scratchpad

# Workspaces
bind $mod+1 = workspace:1
bind $mod+2 = workspace:2
bind $mod+3 = workspace:3
bind $mod+4 = workspace:4
bind $mod+5 = workspace:5
bind $mod+6 = workspace:6
bind $mod+7 = workspace:7
bind $mod+8 = workspace:8
bind $mod+9 = workspace:9
bind $mod+0 = workspace:10
bind $mod+Shift+1 = move-to-workspace:1
bind $mod+Shift+2 = move-to-workspace:2
bind $mod+Shift+3 = move-to-workspace:3
bind $mod+Shift+4 = move-to-workspace:4
bind $mod+Shift+5 = move-to-workspace:5
bind $mod+Shift+6 = move-to-workspace:6
bind $mod+Shift+7 = move-to-workspace:7
bind $mod+Shift+8 = move-to-workspace:8
bind $mod+Shift+9 = move-to-workspace:9
bind $mod+Shift+0 = move-to-workspace:10
# bind $mod+Shift+O = swap-workspaces:DP-2

bind $mod+N = submap:nav

submap nav {
    bind h = focus-left
    bind l = focus-right
    bind k = focus-up
    bind j = focus-down
    bind Escape = exit-submap
}

input {
    repeat_delay = 200
    repeat_rate = 25
    focus_follows_mouse = true

    # xkb_layout = us
    # xkb_options = grp:alt_shift_toggle

    touchpad {
        # tap_to_click = true
        # natural_scroll = true
        # accel_profile = adaptive
        # workspace_swipe_fingers = 3
        # workspace_swipe_distance = 200
        # gesture_swipe_fingers = 3
        # swipe_left = workspace:2
        # swipe_right = workspace:1
        # gesture_pinch_fingers = 4
        # pinch_in = toggle-overview
    }
}

xwayland {
    enabled = true
    path = xwayland-satellite
}

# output eDP-1 {
#     mode = 1920x1080@60
#     position = 0x0
#     scale = 1.0
# }

# switch_events {
#     lid_close = spawn:systemctl suspend
#     lid_open = spawn:brightnessctl s 50%
# }

# rule {
#     app_id = pavucontrol
#     float = true
# }
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
        assert!(matches!(
            parsed.tablet_mode_on,
            Some(Action::SwitchWorkspace(WorkspaceRef::Number(2)))
        ));
        assert!(parsed.tablet_mode_off.is_none());
    }

    #[test]
    fn scratchpad_actions_parse_bare_and_named_forms() {
        // Bare form stays the classic unnamed scratchpad; a `:name` suffix
        // selects a named one; a bare trailing colon degrades to unnamed
        // rather than creating a scratchpad named "".
        assert!(matches!(
            parse_action("toggle-scratchpad"),
            Some(Action::ToggleScratchpad(None))
        ));
        assert!(matches!(
            parse_action("toggle-scratchpad:music"),
            Some(Action::ToggleScratchpad(Some(ref n))) if n == "music"
        ));
        assert!(matches!(
            parse_action("move-to-scratchpad:music"),
            Some(Action::MoveToScratchpad(Some(ref n))) if n == "music"
        ));
        assert!(matches!(
            parse_action("toggle-scratchpad:"),
            Some(Action::ToggleScratchpad(None))
        ));
    }

    #[test]
    fn parse_workspace_gaps_resolves_names_and_skips_malformed_entries() {
        let mut names = HashMap::new();
        names.insert("web".to_string(), 3);
        let gaps = parse_workspace_gaps(
            &[
                "1 0".to_string(),
                "web 16".to_string(),      // name resolves via workspace_name
                "nope 4".to_string(),      // unknown name, skipped
                "2".to_string(),           // no pixel value, skipped
                "2 lots".to_string(),      // bad pixel value, skipped
            ],
            &names,
        );

        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps.get(&1), Some(&0));
        assert_eq!(gaps.get(&3), Some(&16));
    }

    #[test]
    fn parse_workspace_names_skips_malformed_entries_and_last_duplicate_wins() {
        let names = parse_workspace_names(&[
            "3 web".to_string(),
            "4 chat".to_string(),
            "nope".to_string(),           // no name, skipped
            "  ".to_string(),             // no name, skipped
            "notanumber web".to_string(), // bad number, skipped
            "5 web".to_string(),          // duplicate name, last one wins
        ]);

        assert_eq!(names.len(), 2);
        assert_eq!(names.get("web"), Some(&5));
        assert_eq!(names.get("chat"), Some(&4));
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
        let by_app_id = WindowRule {
            app_id: Some("firefox".to_string()),
            ..Default::default()
        };
        assert!(by_app_id.matches(Some("firefox"), None));
        assert!(!by_app_id.matches(Some("firefox-nightly"), None));
        assert!(!by_app_id.matches(None, Some("Firefox")));

        let by_title = WindowRule {
            title: Some("Picture-in-Picture".to_string()),
            ..Default::default()
        };
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
        let blank = WindowRule {
            float: true,
            ..Default::default()
        };
        assert!(!blank.matches(Some("anything"), Some("anything")));
        assert!(!blank.matches(None, None));
    }

    #[test]
    fn window_rule_regexes_compile_once_and_invalid_patterns_are_ignored() {
        let rule = WindowRule {
            app_id_regex: Some(regex::Regex::new(r"^(org\.)?mozilla\.firefox$").unwrap()),
            title_regex: Some(regex::Regex::new("(?i)private browsing").unwrap()),
            ..Default::default()
        };
        assert!(rule.matches(Some("org.mozilla.firefox"), Some("Private Browsing")));
        assert!(!rule.matches(Some("kitty"), Some("Private Browsing")));

        let invalid =
            lower_window_rule_block(&[waves::Entry::Assign("app_id_regex".into(), "[".into())]);
        assert!(invalid.app_id_regex.is_none());
        assert!(!invalid.matches(Some("anything"), None));
    }

    #[test]
    fn window_rule_opacity_is_clamped_and_gesture_actions_parse() {
        let low = lower_window_rule_block(&[
            waves::Entry::Assign("app_id".into(), "dimmed".into()),
            waves::Entry::Assign("opacity".into(), "-0.5".into()),
        ]);
        let high = lower_window_rule_block(&[
            waves::Entry::Assign("app_id".into(), "bright".into()),
            waves::Entry::Assign("opacity".into(), "1.5".into()),
        ]);
        assert_eq!(low.opacity, Some(0.0));
        assert_eq!(high.opacity, Some(1.0));

        let mut touchpad = TouchpadConfig::default();
        apply_touchpad_block(
            &mut touchpad,
            &[
                waves::Entry::Assign("gesture_swipe_fingers".into(), "3".into()),
                waves::Entry::Assign("swipe_left".into(), "toggle-overview".into()),
                waves::Entry::Assign("gesture_pinch_fingers".into(), "4".into()),
                waves::Entry::Assign("pinch_out".into(), "close-window".into()),
            ],
        );
        assert_eq!(touchpad.gesture_swipe_fingers, Some(3));
        assert!(matches!(touchpad.swipe_left, Some(Action::ToggleOverview)));
        assert_eq!(touchpad.gesture_pinch_fingers, Some(4));
        assert!(matches!(touchpad.pinch_out, Some(Action::CloseWindow)));
    }

    #[test]
    fn layer_blocks_capture_matches_by_namespace_substring_only_when_flagged() {
        let mut config = Config {
            layer_rules: Vec::new(),
            ..parse_default_config()
        };

        // No rule at all: never blocked.
        assert!(!config.layer_blocks_capture("waybar"));

        config.layer_rules.push(LayerRule {
            namespace: Some("bitwarden".to_string()),
            block_capture: true,
        });
        assert!(config.layer_blocks_capture("bitwarden-quick-access"));
        assert!(!config.layer_blocks_capture("waybar"));

        // A rule with no namespace never matches, same "blank rule matches
        // nothing" precedent WindowRule sets.
        let blank = LayerRule {
            namespace: None,
            block_capture: true,
        };
        assert!(!blank.matches("anything"));

        // A matching namespace but block_capture: false must not block --
        // the rule matching and the effect firing are separate things.
        config.layer_rules.push(LayerRule {
            namespace: Some("waybar".to_string()),
            block_capture: false,
        });
        assert!(!config.layer_blocks_capture("waybar"));
    }

    #[test]
    fn resolve_window_rules_folds_last_scalar_wins_bools_accumulate() {
        let mut config = Config {
            terminal: String::new(),
            show_welcome_hint: false,
            water_effects: true,
            cursor_always_visible: false,
            cursor_hide_after_ms: 0,
            workspace_auto_back_and_forth: false,
            workspace_names: HashMap::new(),
            workspace_gaps: HashMap::new(),
            gaps: 0,
            default_layout: LayoutAlgorithm::Bsp,
            master_orientation: MasterOrientation::Left,
            bsp_split_bias: SplitBias::Auto,
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
            layer_rules: Vec::new(),
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

        assert_eq!(
            substitute_variables("$mainMod+Return", &variables),
            "SUPER+Return"
        );
        assert_eq!(
            substitute_variables("spawn:$terminal", &variables),
            "spawn:kitty"
        );
        // $HOME/$PATH aren't defined variables -- must survive untouched,
        // since these commonly appear in real spawn commands and corrupting
        // them would be far worse than leaving an unknown $name alone.
        assert_eq!(
            substitute_variables("spawn:sh -c \"echo $HOME\"", &variables),
            "spawn:sh -c \"echo $HOME\""
        );
        // A bare trailing `$` (no identifier following) must not panic or loop.
        assert_eq!(substitute_variables("cost is $5", &variables), "cost is $5");
        // No variables defined at all -- short-circuits, returns unchanged.
        assert_eq!(
            substitute_variables("$mainMod+Q", &HashMap::new()),
            "$mainMod+Q"
        );
    }

    #[test]
    fn load_raw_config_substitutes_variables_into_keybinds_and_spawn_at_startup() {
        let dir = TestDir::new("variables");
        let main = dir.write(
            "config.wave",
            "spawn_at_startup = $terminal --daemon\n\
             $mainMod = SUPER\n\
             $terminal = kitty\n\
             bind $mainMod+Return = spawn:$terminal\n",
        );

        let raw = load_raw_config(&main).expect("should parse");
        assert_eq!(raw.spawn_at_startup, vec!["kitty --daemon".to_string()]);
        assert_eq!(
            raw.keybinds.get("SUPER+Return").map(String::as_str),
            Some("spawn:kitty")
        );
        assert!(!raw.keybinds.contains_key("$mainMod+Return"));
    }

    #[test]
    fn load_raw_config_resolves_wave_fallback_to_the_first_real_command() {
        // /bin/sh always exists on any system that can even run this test
        // suite; "definitely-not-a-real-binary" never will. First match
        // wins over an earlier miss, not just the first candidate overall.
        let dir = TestDir::new("wave-fallback");
        let main = dir.write(
            "config.wave",
            "terminal = $wave(definitely-not-a-real-binary, /bin/sh, kitty)\n",
        );

        let raw = load_raw_config(&main).expect("should parse");
        assert_eq!(raw.terminal, "/bin/sh");
    }

    /// Sets up an isolated directory under the system temp dir for a single
    /// test, cleaned up on drop -- these tests exercise real file I/O
    /// (`load_raw_config` resolving `include` paths relative to the file
    /// doing the including), which in-memory AST construction can't cover.
    struct TestDir(PathBuf);
    impl TestDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("tidewm-config-test-{name}-{}", std::process::id()));
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
    fn load_raw_config_resolves_relative_includes_with_including_files_own_keys_winning() {
        let dir = TestDir::new("includes");
        dir.write("keybinds.wave", "bind Super+Q = close-window\n");
        let main = dir.write(
            "config.wave",
            "include \"keybinds.wave\"\nterminal = kitty\nbind Super+F = toggle-fullscreen\n",
        );

        let raw = load_raw_config(&main).expect("include chain should resolve");

        assert_eq!(raw.terminal, "kitty");
        // Included file's bind survives...
        assert_eq!(
            raw.keybinds.get("Super+Q").map(String::as_str),
            Some("close-window")
        );
        // ...and the including file's own bind is present too, not
        // clobbered by the include (merge order: include first, own
        // entries folded on top).
        assert_eq!(
            raw.keybinds.get("Super+F").map(String::as_str),
            Some("toggle-fullscreen")
        );
    }

    #[test]
    fn load_raw_config_later_include_overlays_earlier_one() {
        let dir = TestDir::new("include-order");
        dir.write("a.wave", "gaps = 4\n");
        dir.write("b.wave", "gaps = 12\n");
        let main = dir.write("config.wave", "include \"a.wave\"\ninclude \"b.wave\"\n");

        let raw = load_raw_config(&main).unwrap();
        assert_eq!(raw.gaps, 12);
    }

    #[test]
    fn load_raw_config_skips_a_missing_include_instead_of_failing_the_whole_load() {
        let dir = TestDir::new("missing-include");
        let main = dir.write(
            "config.wave",
            "include \"does-not-exist.wave\"\nterminal = kitty\n",
        );

        let raw = load_raw_config(&main).expect("a bad include must not fail the top-level file");
        assert_eq!(raw.terminal, "kitty");
    }

    #[test]
    fn load_raw_config_detects_include_cycles_without_infinite_recursion() {
        // A cycle is caught deep in the include graph (b -> a, while a is
        // still an in-progress ancestor), the same place any other broken
        // include would be caught -- so it takes the same "log a warning,
        // skip just that include, keep going" path as a missing file,
        // rather than failing the whole top-level load. What actually
        // matters here is that this terminates at all instead of
        // recursing forever; if it didn't, this test would hang, not fail.
        let dir = TestDir::new("cycle");
        dir.write("b.wave", "include \"a.wave\"\n");
        let a = dir.write("a.wave", "include \"b.wave\"\nterminal = kitty\n");

        let raw =
            load_raw_config(&a).expect("a cycle is skipped with a warning, not a hard failure");
        assert_eq!(raw.terminal, "kitty");
    }

    /// Parses and lowers `DEFAULT_CONFIG_WAVE` exactly the way `load_raw_config`
    /// would for a real file (including `$mod` substitution), without
    /// needing a real path on disk -- there's nothing to `include` in the
    /// shipped default, so `waves::parse` alone (no `waves::resolve`) is
    /// enough.
    fn parse_default_config() -> Config {
        let entries = waves::parse(DEFAULT_CONFIG_WAVE, Path::new("<default>"))
            .expect("DEFAULT_CONFIG_WAVE must parse");
        let mut raw = lower_entries(&entries);
        substitute_variables_in_raw(&mut raw);
        Config::from_raw(raw).0
    }

    #[test]
    fn default_submap_parses_from_both_the_in_memory_and_written_defaults() {
        // Two independently hand-maintained representations of the same
        // default (see `RawConfig::default()` and `DEFAULT_CONFIG_WAVE`'s
        // own doc note) -- assert both actually agree, not just that one
        // of them happens to parse.
        for config in [
            Config::from_raw(RawConfig::default()).0,
            parse_default_config(),
        ] {
            let nav = config
                .submaps
                .get("nav")
                .expect("default config should ship a `nav` submap");
            let find = |key: &str| {
                nav.iter()
                    .find(|b| b.keysym == xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE))
            };
            assert!(matches!(
                find("h").map(|b| &b.action),
                Some(Action::FocusDirection(Direction::Left))
            ));
            assert!(matches!(
                find("l").map(|b| &b.action),
                Some(Action::FocusDirection(Direction::Right))
            ));
            assert!(matches!(
                find("k").map(|b| &b.action),
                Some(Action::FocusDirection(Direction::Up))
            ));
            assert!(matches!(
                find("j").map(|b| &b.action),
                Some(Action::FocusDirection(Direction::Down))
            ));
            assert!(matches!(
                find("Escape").map(|b| &b.action),
                Some(Action::ExitSubmap)
            ));

            let enters_nav = config
                .keybinds
                .iter()
                .any(|b| matches!(&b.action, Action::EnterSubmap(name) if name == "nav"));
            assert!(
                enters_nav,
                "default keybinds should have a bind entering the `nav` submap"
            );
        }
    }

    #[test]
    fn default_layout_keybinds_parse_from_both_the_in_memory_and_written_defaults() {
        // Same two-representations-must-agree check as the submap test
        // above, for the layout-algorithm keybinds: also catches, for free,
        // a duplicate `bind` for the same combo in DEFAULT_CONFIG_WAVE
        // (last one silently wins there, unlike a literal duplicate TOML
        // key, which used to fail the parse loudly -- worth a second look
        // at the template by eye if this test ever breaks unexpectedly).
        for config in [
            Config::from_raw(RawConfig::default()).0,
            parse_default_config(),
        ] {
            let find = |key: &str, mods: Mods| {
                config.keybinds.iter().find(|b| {
                    b.keysym == xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE)
                        && b.mods == mods
                })
            };
            let logo = Mods {
                logo: true,
                ..Default::default()
            };
            let logo_shift = Mods {
                logo: true,
                shift: true,
                ..Default::default()
            };
            let logo_ctrl = Mods {
                logo: true,
                ctrl: true,
                ..Default::default()
            };

            assert!(matches!(
                find("w", logo).map(|b| &b.action),
                Some(Action::SetLayout(LayoutAlgorithm::Bsp))
            ));
            assert!(matches!(
                find("w", logo_shift).map(|b| &b.action),
                Some(Action::SetLayout(LayoutAlgorithm::Master))
            ));
            assert!(matches!(
                find("minus", logo_ctrl).map(|b| &b.action),
                Some(Action::ShrinkMaster)
            ));
            assert!(matches!(
                find("equal", logo_ctrl).map(|b| &b.action),
                Some(Action::GrowMaster)
            ));
        }
    }

    #[test]
    fn shipped_default_keybinds_never_trip_the_footgun_lint() {
        // The default config itself must stay clean, or every fresh
        // install would show the new warning panel on first boot.
        let (_, warnings) = Config::from_raw(RawConfig::default());
        assert!(
            warnings.is_empty(),
            "RawConfig::default() produced diagnostics: {warnings:?}"
        );
    }

    #[test]
    fn bare_typing_key_in_base_keybinds_is_flagged_but_still_bound() {
        let mut raw = RawConfig::default();
        raw.keybinds
            .insert("Return".to_string(), "spawn:kitty".to_string());
        let (config, warnings) = Config::from_raw(raw);

        // Matches the real incident this lint exists for: applying the
        // bind is still correct (it's valid config, just risky), the lint
        // only flags it.
        let bound = config.keybinds.iter().any(|b| {
            b.keysym == xkb::keysym_from_name("Return", xkb::KEYSYM_CASE_INSENSITIVE)
                && b.mods == Mods::default()
        });
        assert!(bound, "the bare bind should still be applied");
        assert!(
            warnings.iter().any(|w| w.contains("Return")),
            "expected a footgun warning for bare Return, got: {warnings:?}"
        );
    }

    #[test]
    fn bare_typing_key_in_a_submap_is_not_flagged() {
        // Submaps rely on bare keys by design (the default `nav` submap's
        // own Escape = exit-submap) -- only the always-active base table
        // can silently steal a key from every other window.
        let mut raw = RawConfig::default();
        let mut submap = HashMap::new();
        submap.insert("a".to_string(), "close-window".to_string());
        raw.submaps.insert("test".to_string(), submap);
        let (_, warnings) = Config::from_raw(raw);
        assert!(
            !warnings.iter().any(|w| w.contains('a')),
            "submap binds must not trigger the base-table footgun lint: {warnings:?}"
        );
    }

    #[test]
    fn unknown_keybind_action_is_dropped_and_reported() {
        let mut raw = RawConfig::default();
        raw.keybinds
            .insert("Super+Z".to_string(), "not-a-real-action".to_string());
        let (config, warnings) = Config::from_raw(raw);
        assert!(!config
            .keybinds
            .iter()
            .any(|b| b.keysym == xkb::keysym_from_name("z", xkb::KEYSYM_CASE_INSENSITIVE)));
        assert!(
            warnings.iter().any(|w| w.contains("not-a-real-action")),
            "expected the dropped action to be reported: {warnings:?}"
        );
    }
}
