use super::*;

/// A parsed, ready-to-match keybind: which modifiers must be held, which base
/// (unshifted) key symbol triggers it, and what it does.
#[derive(Debug, Clone)]
pub struct Keybind {
    pub mods: Mods,
    /// Ordinary non-XKB keys held as user-defined helper modifiers. In
    /// `P+Ctrl+H`, P is here, Ctrl is in `mods`, and H is `keysym`.
    pub held_keysyms: Vec<Keysym>,
    pub keysym: Keysym,
    pub action: Action,
}

impl Keybind {
    pub fn held_keys_match(&self, held: &HashSet<Keysym>) -> bool {
        self.held_keysyms.iter().all(|keysym| held.contains(keysym))
    }

    pub fn uses_helper_key(&self, keysym: Keysym) -> bool {
        self.held_keysyms.contains(&keysym)
    }
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

    /// Whether every configured modifier is present in a physically-held
    /// modifier set. Extra held modifiers are deliberately allowed so, for
    /// example, Alt+Shift+drag still behaves like Alt+drag.
    pub fn is_held_by(&self, held: Self) -> bool {
        (!self.ctrl || held.ctrl)
            && (!self.alt || held.alt)
            && (!self.shift || held.shift)
            && (!self.logo || held.logo)
    }
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

/// Target of `switch-layout`: `next`, `prev`, a zero-based index, or a
/// layout code/name (matched case-insensitively against the keymap's own
/// layout names and the configured `xkb_layout` codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutSwitch {
    Next,
    Previous,
    Index(u32),
    Name(String),
}

/// What a Scroll column action does to the focused window's column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAction {
    /// Leave a shared column for a new one on that side, or join the
    /// neighbor column when alone.
    ConsumeOrExpel { forward: bool },
    /// Pull the next column's first window into this column.
    ConsumeInto,
    /// Move the focused window out of its shared column.
    Expel,
    /// Cycle the column through `scroll.preset_widths`.
    SwitchPresetWidth,
    /// `SwitchPresetWidth` in the other direction.
    SwitchPresetWidthBack,
    /// Set or adjust the column width (`set-column-width:<v>`).
    SetWidth(crate::scroll::SizeChange),
    /// Toggle the column between full width and its previous width.
    Maximize,
    /// Widen the column into the space other fully visible columns leave.
    Expand,
    /// Center the focused column once.
    Center,
    /// Focus the first (`last == false`) or last column.
    FocusEnd { last: bool },
    /// Focus the next (`forward`) or previous column, wrapping at the ends.
    FocusWrap { forward: bool },
    /// Focus the first (`last == false`) or last window inside the column.
    FocusRowEnd { last: bool },
    /// Move the column to the first or last place.
    MoveToEnd { last: bool },
    /// Turn the column into one tab group, or a tab group back into a
    /// stacked column.
    ToggleTabbed,
}

#[derive(Debug, Clone)]
pub enum Action {
    Spawn(String),
    CloseWindow,
    ToggleFloating,
    ToggleFullscreen,
    /// "Border fullscreen": fills the output and is pinned to the visible
    /// viewport regardless of the Ocean camera (via `space.map_element`,
    /// bypassing world/camera tracking entirely, same as Classic's
    /// maximize) -- but keeps window borders/decoration, unlike
    /// `ToggleFullscreen`. The same xdg-shell maximize geometry a client's
    /// own request or a window rule already produces, just reachable from
    /// a keybind. Distinct from a plain zoom-independent resize -- see
    /// `Action::ResizeToMonitor` for that.
    ToggleBorderFullscreen,
    /// Ocean only: sets a floating window's world-space size to its
    /// output's resolution (inset by the configured gap, so its border
    /// stays inside the visible screen -- see `Smallvil::resize_to_monitor`)
    /// -- ignoring the camera's current zoom entirely, not compensating
    /// for it -- while keeping it an ordinary floating window in the
    /// ordinary world/camera pipeline (no viewport pinning, no
    /// fullscreen/maximize state). For games/apps that want to be
    /// configured at close to native monitor resolution regardless of how
    /// zoomed in or out the canvas happens to be. See
    /// `ToggleBorderFullscreen` for the "pinned to the screen" alternative
    /// this is deliberately not.
    ResizeToMonitor,
    TogglePin,
    /// Pins/unpins the focused window to whichever output it's currently
    /// on (the interactive counterpart to `rule { pin_output }`) -- see
    /// `Smallvil::toggle_output_pin`. Unlike `TogglePin` (workspace-sticky,
    /// floating-only), this has nothing to do with tiling state: a tiled
    /// window pins just as well as a floating one.
    ToggleOutputPin,
    /// `None` is the classic single scratchpad; `Some(name)` a named one
    /// (Hyprland's named special workspaces) -- see
    /// `Smallvil::scratchpad_workspace`.
    ToggleScratchpad(Option<String>),
    MoveToScratchpad(Option<String>),
    TogglePseudoTile,
    /// Per-window ambient "sitting on water" nudges (spatial roadmap F1
    /// `light`): a small random-direction kick at a randomized interval,
    /// independent of the drag/map/wave kick sources, until toggled off
    /// again. Floating windows only -- see `Smallvil::toggle_float_ambient`.
    ToggleFloatAmbient,
    /// Flips `scroll.float_anchor` between `screen` and `strip` until the
    /// next config reload.
    ToggleFloatAnchor,
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
    /// Re-reads the config files now, exactly like the file watcher does.
    /// Useful where a watcher misses a change (network mounts, replaced
    /// directories). See `Smallvil::reload_config`.
    ReloadConfig,
    /// Toggles every output's power together (all on, or all off). See
    /// `Smallvil::toggle_dpms`.
    ToggleDpms,
    /// Switches the XKB layout group: next/previous with wraparound, or an
    /// explicit index/code/name. The action a bar's layout widget needs;
    /// niri's `switch-layout` is the same shape, and the same keymap
    /// toggle a `grp:` xkb option performs can be driven from here instead
    /// of a key. See `Smallvil::run_action`.
    SwitchLayout(LayoutSwitch),
    CycleFocus,
    FocusDirection(Direction),
    SwapDirection(Direction),
    /// Keyboard-driven resize. Right/down grow and left/up shrink the
    /// focused window along that axis; floating windows change by pixels,
    /// BSP windows move their nearest enclosing split.
    Resize(Direction),
    /// Keyboard-driven move, floating windows only (spatial roadmap Phase
    /// 4's escape hatch alongside `toggle-floating`): steps the focused
    /// floating window 24 logical pixels in `Direction`. A no-op on a tiled
    /// window -- tiled windows have no independent position to move, same
    /// reasoning `RaiseWindow`/`TogglePseudoTile`'s tiled/floating-only
    /// restrictions already use. See `Smallvil::keyboard_move_floating`.
    MoveFloating(Direction),
    /// Places the focused Classic floating window into one configured snap
    /// zone. Pointer drops and keyboard actions share the same target math.
    Snap(crate::snap::SnapZone),
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
    /// Classic: switches the output's active numbered/named workspace.
    /// Ocean has no workspace concept -- a numbered ref instead jumps to
    /// that app-slot (see `Smallvil::jump_to_app_slot`, `OceanSpace::
    /// app_slot`); a named ref keeps the older camera-bookmark-jump
    /// behavior (`OceanSpace::animate_to_bookmark`), since app-slots are
    /// inherently numbered.
    SwitchWorkspace(WorkspaceRef),
    /// `workspace-on:<output>:<N>`: the same switch, on the named output
    /// instead of `primary_output`. For bars and other external tools,
    /// where the click lands on one monitor while the focused window may
    /// sit on another.
    SwitchWorkspaceOn(String, WorkspaceRef),
    /// `workspace-down` / `workspace-up` (niri's `focus-workspace-down`/
    /// `focus-workspace-up`): step one numbered workspace on the focused
    /// output. In Scroll with `scroll.workspace_scroll`, the camera slides
    /// vertically instead of switching instantly.
    WorkspaceStep(i32),
    MoveToWorkspace(WorkspaceRef),
    /// Swaps the current output's active workspace content with the named
    /// output's. No default keybind -- output names are machine-specific
    /// (see `output { }`), so there's nothing sensible to bind out of the box.
    SwapWorkspacesWithOutput(String),
    /// Switches which keybind table is consulted (sway/Hyprland's "mode"
    /// idea): a temporary layer of binds on top of the top-level binds,
    /// entered by name and left active until an explicit `exit-mode`
    /// bind, not tied to focus or any other implicit event. See
    /// `Config::modes`, `InputState::active_mode`.
    EnterMode(String),
    ExitMode,
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
    /// Parks the focused ordinary tiled window in its Classic workspace's
    /// Depth Deck. No-op unless `classic_depth.enabled` is true.
    SinkWindow,
    /// Opens the current Classic workspace's Depth Deck.
    Dive,
    DepthNext,
    DepthPrevious,
    DepthSelect,
    DepthCancel,
    /// Direct workspace-like rotation through the focused tile's Classic
    /// deck, without opening the picker.
    DepthDown,
    DepthUp,
    /// Moves one output's Ocean camera without moving any window. These are
    /// harmless no-ops under Classic.
    OceanPan(Direction),
    OceanZoomIn,
    OceanZoomOut,
    OceanZoomReset,
    OceanCenterFocused,
    OceanDredgeWindow,
    OceanSurfaceWindow,
    /// Jumps the current output camera to a named world point.
    OceanBookmark(String),
    /// Stores the current output camera as a runtime bookmark. Configured
    /// bookmarks remain the startup baseline; this does not rewrite config.
    OceanSaveBookmark(String),
    /// Focuses the most recently used mapped window of the given app_id
    /// including hidden workspace windows, switching
    /// workspace / traveling the camera there first if needed -- the dock
    /// and taskbar's "smart open": running app, click icon, it comes to
    /// you. No-op when the app has no mapped window; the caller (a bar's
    /// pin) is expected to spawn it when it has none.
    FocusApp(String),
    /// Reveals and activates one exact live window, including hidden owners.
    FocusWindow(u64),
    SetEngine(SpatialEngine),
    ScrollView(Direction),
    /// Scroll column editing (niri's consume/expel family). No-op outside
    /// Scroll.
    Column(ColumnAction),
    /// Sends a graceful close request to every mapped window of the given
    /// app_id -- the dock's "quit app". Apps that confirm-and-close on
    /// their own handle it; anything stuck must be killed by the caller.
    CloseApp(String),
    /// Adds a tag to the focused window -- the runtime counterpart to
    /// `rule { add_tag = [...] }`. No-op with nothing focused. See
    /// `Smallvil::tag_window`.
    TagWindow(String),
    /// Removes a tag from the focused window. No-op with nothing focused
    /// or if it doesn't carry the tag. See `Smallvil::untag_window`.
    UntagWindow(String),
    /// Focuses a window carrying this tag, cycling to the next one
    /// carrying it on repeated presses. No-op if no mapped window carries
    /// the tag. See `Smallvil::toggle_tag`.
    ToggleTag(String),
    Quit,
}

pub(super) fn is_depth_action(action: &Action) -> bool {
    matches!(
        action,
        Action::SinkWindow
            | Action::Dive
            | Action::DepthNext
            | Action::DepthPrevious
            | Action::DepthSelect
            | Action::DepthCancel
            | Action::DepthDown
            | Action::DepthUp
            | Action::OceanDredgeWindow
            | Action::OceanSurfaceWindow
    )
}

pub(super) fn depth_action_enabled(
    action: &Action,
    ocean_selected: bool,
    classic_enabled: bool,
    ocean_enabled: bool,
) -> bool {
    if !is_depth_action(action) {
        return true;
    }
    if ocean_selected {
        ocean_enabled
            && matches!(
                action,
                Action::SinkWindow
                    | Action::DepthDown
                    | Action::DepthUp
                    | Action::OceanDredgeWindow
                    | Action::OceanSurfaceWindow
            )
    } else {
        classic_enabled
    }
}

pub(crate) fn is_ocean_action(action: &Action) -> bool {
    matches!(
        action,
        Action::OceanPan(_)
            | Action::OceanZoomIn
            | Action::OceanZoomOut
            | Action::OceanZoomReset
            | Action::OceanCenterFocused
            | Action::OceanDredgeWindow
            | Action::OceanSurfaceWindow
            | Action::OceanBookmark(_)
            | Action::OceanSaveBookmark(_)
    )
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
/// normally used for typing. `lint_footguns` is false for mode tables --
/// bare keys there are the whole point of a mode (see the default `nav`
/// mode's bare `Escape = exit-mode`), only the always-active base
/// top-level bind table can silently steal a key from every other window.
pub(super) fn parse_keybind(
    combo: &str,
    action: &str,
    lint_footguns: bool,
    warnings: &mut Vec<String>,
) -> Option<Keybind> {
    let mut mods = Mods::default();
    let mut ordinary_keys = Vec::new();

    for part in combo.split('+') {
        match part.to_lowercase().as_str() {
            "super" | "logo" | "mod4" => mods.logo = true,
            "ctrl" | "control" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            other => ordinary_keys.push(other.to_string()),
        }
    }

    let Some(key_name) = ordinary_keys.pop() else {
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
    let mut held_keysyms = Vec::with_capacity(ordinary_keys.len());
    for held_name in ordinary_keys {
        let held = xkb::keysym_from_name(&held_name, xkb::KEYSYM_CASE_INSENSITIVE);
        if held.raw() == 0 {
            warnings.push(format!(
                "Unknown helper key \"{held_name}\" in keybind \"{combo}\", skipped"
            ));
            return None;
        }
        held_keysyms.push(held);
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
        && held_keysyms.is_empty()
        && is_typing_key(&key_name)
    {
        tracing::debug!(
            combo,
            "Bare global keybind intentionally captures this key from clients"
        );
    }

    Some(Keybind {
        mods,
        held_keysyms,
        keysym,
        action,
    })
}

/// Parses the modifier-only side of compositor mouse bindings. An empty
/// modifier is rejected deliberately: otherwise an ordinary click would
/// unexpectedly start moving or resizing a window.
pub(super) fn parse_modifiers(value: &str) -> Option<Mods> {
    let mut mods = Mods::default();
    let mut saw_modifier = false;

    for part in value.split('+') {
        match part.trim().to_lowercase().as_str() {
            "super" | "logo" | "mod4" => mods.logo = true,
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "mod1" => mods.alt = true,
            "shift" => mods.shift = true,
            _ => return None,
        }
        saw_modifier = true;
    }

    saw_modifier.then_some(mods)
}

/// Parses a `minimap.key`-style hold chord: modifiers plus exactly one
/// ordinary trigger key, no multi-key helper support (unlike
/// `parse_keybind`'s `held_keysyms`) -- a hold gesture only needs to know
/// its own single terminal key, not a chain of helper modifiers.
pub(super) fn parse_simple_chord(combo: &str) -> Option<(Mods, Keysym)> {
    let mut mods = Mods::default();
    let mut key_name = None;

    for part in combo.split('+') {
        match part.trim().to_lowercase().as_str() {
            "super" | "logo" | "mod4" => mods.logo = true,
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "mod1" => mods.alt = true,
            "shift" => mods.shift = true,
            other if key_name.is_none() => key_name = Some(other.to_string()),
            _ => return None,
        }
    }

    let keysym = xkb::keysym_from_name(&key_name?, xkb::KEYSYM_CASE_INSENSITIVE);
    (keysym.raw() != 0).then_some((mods, keysym))
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
/// string syntax used in binds, so anything a keybind can trigger is
/// IPC-addressable for free, including actions added by later phases.
pub(crate) fn parse_action(action: &str) -> Option<Action> {
    if let Some(cmd) = action.strip_prefix("spawn:") {
        return Some(Action::Spawn(cmd.to_string()));
    }
    if matches!(action, "workspace-up" | "focus-workspace-up") {
        return Some(Action::WorkspaceStep(-1));
    }
    if matches!(action, "workspace-down" | "focus-workspace-down") {
        return Some(Action::WorkspaceStep(1));
    }
    if let Some(n) = action.strip_prefix("workspace:") {
        return Some(Action::SwitchWorkspace(parse_workspace_ref(n)));
    }
    // Connector names never contain ':', so the last ':' splits output
    // from workspace.
    if let Some(rest) = action.strip_prefix("workspace-on:") {
        let (output, n) = rest.rsplit_once(':')?;
        if output.is_empty() || n.is_empty() {
            return None;
        }
        return Some(Action::SwitchWorkspaceOn(
            output.to_string(),
            parse_workspace_ref(n),
        ));
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
    if let Some(name) = action.strip_prefix("mode:") {
        return Some(Action::EnterMode(name.to_string()));
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
    if let Some(zone) = action.strip_prefix("snap:") {
        return match crate::snap::SnapZone::parse(zone) {
            Some(zone) => Some(Action::Snap(zone)),
            None => {
                tracing::warn!(zone, "Unknown snap zone in keybind, skipping");
                None
            }
        };
    }
    if let Some(name) = action.strip_prefix("ocean-bookmark:") {
        return (!name.trim().is_empty()).then(|| Action::OceanBookmark(name.trim().to_string()));
    }
    if let Some(name) = action.strip_prefix("ocean-save-bookmark:") {
        return (!name.trim().is_empty())
            .then(|| Action::OceanSaveBookmark(name.trim().to_string()));
    }
    if let Some(engine) = action.strip_prefix("engine:") {
        return match engine {
            "classic" => Some(Action::SetEngine(SpatialEngine::Classic)),
            "ocean" => Some(Action::SetEngine(SpatialEngine::Ocean)),
            "scroll" => Some(Action::SetEngine(SpatialEngine::Scroll)),
            _ => None,
        };
    }
    if let Some(direction) = action.strip_prefix("scroll-") {
        return match direction {
            "left" => Some(Action::ScrollView(Direction::Left)),
            "right" => Some(Action::ScrollView(Direction::Right)),
            "up" => Some(Action::ScrollView(Direction::Up)),
            "down" => Some(Action::ScrollView(Direction::Down)),
            _ => None,
        };
    }
    if let Some(id) = action.strip_prefix("focus-window:") {
        return id
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .map(Action::FocusWindow);
    }
    if let Some(name) = action.strip_prefix("focus-app:") {
        return (!name.trim().is_empty()).then(|| Action::FocusApp(name.trim().to_string()));
    }
    if let Some(name) = action.strip_prefix("close-app:") {
        return (!name.trim().is_empty()).then(|| Action::CloseApp(name.trim().to_string()));
    }
    if let Some(name) = action.strip_prefix("tag-window:") {
        return (!name.trim().is_empty()).then(|| Action::TagWindow(name.trim().to_string()));
    }
    if let Some(name) = action.strip_prefix("untag-window:") {
        return (!name.trim().is_empty()).then(|| Action::UntagWindow(name.trim().to_string()));
    }
    if let Some(name) = action.strip_prefix("toggle-tag:") {
        return (!name.trim().is_empty()).then(|| Action::ToggleTag(name.trim().to_string()));
    }
    match action {
        "exit-mode" => Some(Action::ExitMode),
        "master-grow" => Some(Action::GrowMaster),
        "master-shrink" => Some(Action::ShrinkMaster),
        "toggle-overview" => Some(Action::ToggleOverview),
        "sink-window" => Some(Action::SinkWindow),
        "dive" => Some(Action::Dive),
        "depth-next" => Some(Action::DepthNext),
        "depth-prev" | "depth-previous" => Some(Action::DepthPrevious),
        "depth-select" => Some(Action::DepthSelect),
        "depth-cancel" => Some(Action::DepthCancel),
        "depth-down" => Some(Action::DepthDown),
        "depth-up" => Some(Action::DepthUp),
        "ocean-pan-left" => Some(Action::OceanPan(Direction::Left)),
        "ocean-pan-right" => Some(Action::OceanPan(Direction::Right)),
        "ocean-pan-up" => Some(Action::OceanPan(Direction::Up)),
        "ocean-pan-down" => Some(Action::OceanPan(Direction::Down)),
        "ocean-zoom-in" => Some(Action::OceanZoomIn),
        "ocean-zoom-out" => Some(Action::OceanZoomOut),
        "ocean-zoom-reset" => Some(Action::OceanZoomReset),
        "ocean-center-focused" => Some(Action::OceanCenterFocused),
        "ocean-dredge-window" | "dredge-window" => Some(Action::OceanDredgeWindow),
        "ocean-surface-window" | "surface-window" => Some(Action::OceanSurfaceWindow),
        "close-window" => Some(Action::CloseWindow),
        "toggle-floating" => Some(Action::ToggleFloating),
        "toggle-fullscreen" => Some(Action::ToggleFullscreen),
        "toggle-border-fullscreen" | "toggle-maximize" => Some(Action::ToggleBorderFullscreen),
        "resize-to-monitor" => Some(Action::ResizeToMonitor),
        "toggle-pin" => Some(Action::TogglePin),
        "toggle-output-pin" => Some(Action::ToggleOutputPin),
        "toggle-scratchpad" => Some(Action::ToggleScratchpad(None)),
        "move-to-scratchpad" => Some(Action::MoveToScratchpad(None)),
        "toggle-pseudo-tile" => Some(Action::TogglePseudoTile),
        "toggle-float-ambient" => Some(Action::ToggleFloatAmbient),
        "toggle-float-anchor" => Some(Action::ToggleFloatAnchor),
        "raise-window" => Some(Action::RaiseWindow),
        "lower-window" => Some(Action::LowerWindow),
        "focus-urgent" => Some(Action::FocusUrgent),
        "reload-config" => Some(Action::ReloadConfig),
        "toggle-dpms" => Some(Action::ToggleDpms),
        "switch-layout" => Some(Action::SwitchLayout(LayoutSwitch::Next)),
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
        "move-left" => Some(Action::MoveFloating(Direction::Left)),
        "move-right" => Some(Action::MoveFloating(Direction::Right)),
        "move-up" => Some(Action::MoveFloating(Direction::Up)),
        "move-down" => Some(Action::MoveFloating(Direction::Down)),
        "group-left" => Some(Action::GroupDirection(Direction::Left)),
        "group-right" => Some(Action::GroupDirection(Direction::Right)),
        "group-up" => Some(Action::GroupDirection(Direction::Up)),
        "group-down" => Some(Action::GroupDirection(Direction::Down)),
        "ungroup" => Some(Action::Ungroup),
        "cycle-tab-next" => Some(Action::CycleTabForward),
        "cycle-tab-prev" => Some(Action::CycleTabBackward),
        "quit" => Some(Action::Quit),
        "consume-or-expel-left" => Some(Action::Column(ColumnAction::ConsumeOrExpel {
            forward: false,
        })),
        "consume-or-expel-right" => Some(Action::Column(ColumnAction::ConsumeOrExpel {
            forward: true,
        })),
        "consume-into-column" => Some(Action::Column(ColumnAction::ConsumeInto)),
        "expel-from-column" => Some(Action::Column(ColumnAction::Expel)),
        "switch-preset-width" => Some(Action::Column(ColumnAction::SwitchPresetWidth)),
        "switch-preset-width-back" => Some(Action::Column(ColumnAction::SwitchPresetWidthBack)),
        "maximize-column" => Some(Action::Column(ColumnAction::Maximize)),
        "expand-column" => Some(Action::Column(ColumnAction::Expand)),
        "center-column" => Some(Action::Column(ColumnAction::Center)),
        "focus-column-first" => Some(Action::Column(ColumnAction::FocusEnd { last: false })),
        "focus-column-last" => Some(Action::Column(ColumnAction::FocusEnd { last: true })),
        "focus-window-top" => Some(Action::Column(ColumnAction::FocusRowEnd { last: false })),
        "focus-window-bottom" => Some(Action::Column(ColumnAction::FocusRowEnd { last: true })),
        "focus-column-right-or-first" => {
            Some(Action::Column(ColumnAction::FocusWrap { forward: true }))
        }
        "focus-column-left-or-last" => {
            Some(Action::Column(ColumnAction::FocusWrap { forward: false }))
        }
        "move-column-first" => Some(Action::Column(ColumnAction::MoveToEnd { last: false })),
        "move-column-last" => Some(Action::Column(ColumnAction::MoveToEnd { last: true })),
        "toggle-column-tabbed" => Some(Action::Column(ColumnAction::ToggleTabbed)),
        other if other.starts_with("set-column-width:") => {
            crate::scroll::SizeChange::parse(&other["set-column-width:".len()..])
                .map(|change| Action::Column(ColumnAction::SetWidth(change)))
        }
        other if other.starts_with("switch-layout:") => {
            let target = &other["switch-layout:".len()..];
            if target.is_empty() {
                None
            } else if target == "next" {
                Some(Action::SwitchLayout(LayoutSwitch::Next))
            } else if target == "prev" || target == "previous" {
                Some(Action::SwitchLayout(LayoutSwitch::Previous))
            } else if let Ok(index) = target.parse::<u32>() {
                Some(Action::SwitchLayout(LayoutSwitch::Index(index)))
            } else {
                Some(Action::SwitchLayout(LayoutSwitch::Name(target.to_string())))
            }
        }
        other => {
            tracing::warn!(action = %other, "Unknown keybind action, skipping");
            None
        }
    }
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
