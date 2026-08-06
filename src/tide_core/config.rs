//! TideWM configuration loading, lowering, validation, and hot reload.

use std::{
    collections::{HashMap, HashSet},
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

use crate::wave;
use crate::waves;

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
    /// TideWM's own "fills the basin" mode (`layout::layout_cascade`):
    /// windows wrap into rows left to right, top to bottom, instead of
    /// BSP's recursive bisection or master's fixed master+stack split. The
    /// row count is chosen so the resulting grid shape (columns per row
    /// over row count) best matches the output's own aspect ratio, so a
    /// wide monitor gets wider rows and a tall one gets more of them.
    Cascade,
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

/// Spatial pressure propagation for BSP resize. The primary split always
/// follows the full pointer displacement; parallel ancestor splits receive
/// `falloff ^ tree_distance` of it, capped to `max_splits` total handles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConnectedVesselsConfig {
    pub enabled: bool,
    pub falloff: f32,
    pub max_splits: u8,
}

impl Default for ConnectedVesselsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            falloff: 0.5,
            max_splits: 4,
        }
    }
}

/// Optional lateral sway for dragged floating windows. Explicitly opt-in:
/// `enabled` defaults false and `water_effects` remains the master bypass.
/// A matching `rule { sway = true|false }` overrides this per app.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwayConfig {
    pub enabled: bool,
    /// Fraction of each horizontal drag delta converted into sway
    /// displacement. 0.0 freezes the effect, 1.0 follows the pointer.
    pub response: f32,
    /// Hard cap on lateral displacement, logical pixels.
    pub max_offset: f32,
    /// Oscillation frequency, hertz.
    pub frequency: f32,
    /// Exponential decay rate, per second. Higher settles back sooner.
    pub damping: f32,
}

impl Default for SwayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            response: 0.08,
            max_offset: 24.0,
            frequency: 1.6,
            damping: 3.0,
        }
    }
}

/// F1's three quality tiers. `Off` and `Light` behave exactly as the doc
/// comments below already describe (closed-form, render-only bob-and-drift
/// via `crate::float_physics::FloatPhysics`). `Full` is the "rigid-body
/// boxes with mass, buoyancy, collisions, and a wave field" tier from the
/// spatial roadmap: real velocity state (`crate::float_physics::FloatBody`)
/// stepped by a fixed-timestep integrator so floating windows can actually
/// exchange collision impulses, high-end only. `enabled = true|false` is
/// still accepted as a legacy alias for `light`/`off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatPhysicsTier {
    Off,
    Light,
    Full,
}

/// Continuous forcing for `full` tier's mass-spring bodies. Unlike
/// `light`'s per-window `toggle-float-ambient`, this is not a manual
/// per-window toggle -- it is inherent to choosing `full`, since a wave
/// field passing across every floating window is what makes the tier
/// "full" rather than just a heavier `light`. `enabled = false` still gets
/// mass and collisions, just no continuous forcing, so a `full`-tier body
/// decays to rest like `light` does once nothing is disturbing it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatPhysicsWaveConfig {
    pub enabled: bool,
    /// Vertical displacement the traveling wave pulls a body's spring
    /// anchor toward, logical pixels.
    pub amplitude: f32,
    /// Distance between wave crests, logical pixels.
    pub wavelength: f32,
    /// Wave travel speed, logical pixels per second.
    pub speed: f32,
}

impl Default for FloatPhysicsWaveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            amplitude: 10.0,
            wavelength: 400.0,
            speed: 60.0,
        }
    }
}

/// Cosmetic 2D bob-and-drift for floating windows (spatial roadmap F1).
/// Disturbance-kicked and render-only; `light` decays to rest the same way
/// `sway` does so an idle desktop still ticks zero frames, while `full`
/// only decays to rest when its `wave` sub-block is off (see
/// `FloatPhysicsTier`). Explicitly opt-in: `tier` defaults `off` and
/// `water_effects` remains the master bypass. A matching
/// `rule { float_physics = off|light|full }` overrides this per app. When
/// enabled for a window, it takes over from `sway` for that window (the
/// two never stack).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatPhysicsConfig {
    pub tier: FloatPhysicsTier,
    /// Fraction of each disturbance impulse converted into displacement
    /// (`light`) or velocity (`full`). 0.0 freezes the effect, 1.0 follows
    /// the impulse one-to-one.
    pub response: f32,
    /// Hard cap on the combined lateral+vertical envelope, logical pixels.
    pub max_offset: f32,
    /// `light`: oscillation frequency, hertz, shared by both axes. `full`:
    /// the same knob reused as the mass-spring's natural frequency
    /// (stiffness = `(2*pi*frequency)^2`), rather than adding a parallel
    /// "stiffness" field for one more tier of the same effect.
    pub frequency: f32,
    /// `light`: exponential decay rate, per second, higher settles back
    /// sooner. `full`: the same knob reused as the spring's linear drag
    /// coefficient (`drag = 2 * damping`).
    pub damping: f32,
    /// Fraction of an impulse's magnitude always added to the vertical term,
    /// so even a lateral disturbance produces a bob. 0.0 is lateral-only.
    /// `light` only -- `full`'s bob comes from the wave field instead.
    pub bob_ratio: f32,
    /// Radius around a disturbance within which nearby floating windows are
    /// also rocked, logical pixels. 0.0 limits kicks to the source window.
    pub radius: f32,
    /// When true, nearby floating windows inside `radius` share a scaled
    /// fraction of each disturbance. False disturbs only the source window.
    pub falloff: bool,
    /// Period of the dominant sine term in the ambient "sitting on water"
    /// offset (`toggle-float-ambient`, per-window), seconds. Amplitude
    /// reuses `max_offset` directly -- a continuous wave has no separate
    /// "impulse strength" the way a kick does. See `float_physics::
    /// ambient_sample` for the actual waveform. `light` only.
    pub ambient_period_s: f32,
    /// `full` only: collision bounciness, `0` perfectly inelastic (windows
    /// just stop pressing against each other) through `1` perfectly
    /// elastic (a full bounce).
    pub restitution: f32,
    /// `full` only: whether a body's rect bouncing off its home output's
    /// edge is part of the simulation, or whether floating windows can
    /// drift past the screen edge unimpeded (still bounded by
    /// `max_offset`, just with no wall to bounce off).
    pub bounce_off_edges: bool,
    /// `full` only: the continuous traveling-wave forcing. See
    /// `FloatPhysicsWaveConfig`.
    pub wave: FloatPhysicsWaveConfig,
}

impl Default for FloatPhysicsConfig {
    fn default() -> Self {
        // Seeded from sway's proven values; the final feel is the user's
        // nested tuning pass, not these defaults.
        Self {
            tier: FloatPhysicsTier::Off,
            response: 0.08,
            max_offset: 24.0,
            frequency: 1.6,
            damping: 3.0,
            bob_ratio: 0.6,
            radius: 256.0,
            falloff: true,
            ambient_period_s: 5.0,
            restitution: 0.3,
            bounce_off_edges: true,
            wave: FloatPhysicsWaveConfig::default(),
        }
    }
}

/// How the water-glass refraction distortion moves over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlassAnimation {
    /// The original fixed distortion: no time uniform, never ticks frames
    /// on its own.
    Static,
    /// The distortion is energized by disturbances -- the window moving,
    /// the backdrop behind it changing, a ripple passing underneath -- and
    /// settles back to still over `settle_ms`. An idle desktop with glass
    /// windows visible still ticks zero frames.
    Reactive,
    /// A constant slow drift, whether anything is moving or not. Ticks
    /// frames for as long as a water-glass window is visible, by design.
    Ambient,
}

/// Water-glass motion tuning (`water_glass { }`). The glass layer itself
/// is selected per window by the `glass` rule (or the legacy
/// `opacity < 1` trigger); this block only controls how the refraction
/// animates once selected. `water_effects` remains the master bypass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterGlassConfig {
    pub animation: GlassAnimation,
    /// Phase drift speed multiplier. `1.0` is the default rate.
    pub speed: f32,
    /// Distortion strength multiplier on the shader's built-in UV offset.
    pub amplitude: f32,
    /// Reactive-mode settle time after the last disturbance, milliseconds.
    pub settle_ms: u32,
}

impl Default for WaterGlassConfig {
    fn default() -> Self {
        Self {
            animation: GlassAnimation::Reactive,
            speed: 1.0,
            amplitude: 1.0,
            settle_ms: 1200,
        }
    }
}

/// Continuous lateral "swim" between workspaces (spatial roadmap S0).
/// Instead of the discrete one-shot switch (and its wave transition), a
/// horizontal trackpad swipe pans the viewport continuously: neighboring
/// spots slide in from the side, the logical anchor advances once the pan
/// crosses the halfway mark, and the camera springs back to rest on
/// release. The lateral axis stays a sequence of discrete tiling spots --
/// each spot is an ordinary `BspLayout`/master/cascade tree -- so logical
/// identity is still the `u32` workspace number; only the *visual* camera
/// offset is continuous. `water_effects` remains the master bypass: off
/// falls back to the ordinary discrete switch regardless of `enabled`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwimConfig {
    pub enabled: bool,
    /// Maximum neighboring workspace distance available to the render-only
    /// preview on each side of the anchor. Only strips which intersect the
    /// viewport are assembled; the windows remain logically hidden.
    pub neighbors: u8,
    /// Swipe-to-offset gain. `1.0` maps one `workspace_swipe_distance` of
    /// trackpad travel to one spot-width of camera motion; higher travels
    /// further per unit swipe.
    pub response: f32,
    /// Snap-back-to-rest animation length after the fingers lift, millis.
    pub snap_duration_ms: u32,
}

impl Default for SwimConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            neighbors: 1,
            response: 1.0,
            snap_duration_ms: 220,
        }
    }
}

/// Bioluminescent edge-glow compass for the Ocean engine (spatial roadmap
/// S5). A window outside the output camera's viewport leaves a soft glow
/// at the viewport edge in its direction: urgent windows glow in any
/// direction, physically deep windows glow below. Nearer is brighter, and
/// the cue fades to nothing at `max_distance`. Ambient render-only cues --
/// travel stays on the existing pan/zoom/bookmark/depth actions.
/// Ocean-only; `water_effects` remains the master bypass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompassConfig {
    pub enabled: bool,
    /// Glow color for off-screen urgent windows, any direction.
    pub urgent_color: [f32; 3],
    /// Glow color for windows below the viewport (sunk or lower reef).
    pub deep_color: [f32; 3],
    /// World-logical distance beyond the viewport edge at which a cue
    /// fades to nothing.
    pub max_distance: f32,
    /// Glow rect side, logical pixels.
    pub size: f32,
    /// Glow alpha at zero distance, fading linearly to `max_distance`.
    pub alpha: f32,
    /// Shape drawn for each cue.
    pub shape: crate::compass::CompassShape,
}

impl Default for CompassConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            urgent_color: [0.463, 0.945, 1.0],
            deep_color: [0.176, 0.439, 0.588],
            max_distance: 3000.0,
            size: 96.0,
            alpha: 0.85,
            shape: crate::compass::CompassShape::Circle,
        }
    }
}

/// Whole-world Ocean overview minimap (spatial roadmap S5's other half,
/// alongside the compass). Hold `key` to peek: a schematic map of every
/// window in the shared world plus every connected output's current camera
/// viewport, click a window or region to travel this output's camera there
/// and dismiss, or release without clicking to just dismiss. Ocean-only.
/// Deliberately *not* gated by `water_effects` -- unlike the compass (an
/// identity/bioluminescence effect), the minimap reads as navigation
/// utility, so it stays available with water off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MinimapConfig {
    pub enabled: bool,
    /// Modifiers that must be held for `keysym` to open the peek.
    pub mods: Mods,
    /// The trigger key itself. Held together with `mods`; releasing either
    /// closes the peek.
    pub keysym: Keysym,
    /// Visual baseline; individual colors below can still override it.
    pub preset: crate::minimap::MinimapPreset,
    /// `None` uses the preset's own default for each field.
    pub background_color: Option<[f32; 3]>,
    pub window_color: Option<[f32; 3]>,
    pub accent_color: Option<[f32; 3]>,
}

impl Default for MinimapConfig {
    fn default() -> Self {
        // Deliberately not bare Super: every default keybind is
        // `Super+<key>`, so a bare-Super hold would fire the peek on every
        // ordinary keybind attempt. A real base key keeps release-tracking
        // to one keysym and doesn't collide with `pointer_modifier` (also
        // Super by default), whose own drag grabs would otherwise lose
        // pointer input for the hold's duration.
        let (mods, keysym) =
            parse_simple_chord("Super+Space").expect("built-in minimap default chord must parse");
        Self {
            enabled: true,
            mods,
            keysym,
            preset: crate::minimap::MinimapPreset::default(),
            background_color: None,
            window_color: None,
            accent_color: None,
        }
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

/// Spatial ownership model selected once when TideWM starts. Classic keeps
/// numbered per-output workspaces; Ocean owns one continuous world viewed by
/// per-output cameras. A reload never swaps this underneath live windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpatialEngine {
    #[default]
    Classic,
    Ocean,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OceanReefConfig {
    pub name: String,
    pub x: i32,
    pub y: i32,
    /// Omitted dimensions follow the largest real output viewport that uses
    /// Ocean; they are never guessed from a 1080p-style constant.
    pub width: Option<i32>,
    pub height: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OceanBookmarkConfig {
    pub name: String,
    pub x: f64,
    pub y: f64,
}

/// Pointer button used to grab otherwise-empty Ocean canvas. It is explicit
/// config state rather than compositor policy so Ocean does not permanently
/// reserve a mouse button the user cannot reclaim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OceanPanButton {
    Disabled,
    Left,
    Middle,
    Right,
}

impl OceanPanButton {
    pub(crate) fn matches(self, button: u32) -> bool {
        match self {
            Self::Disabled => false,
            Self::Left => button == 0x110,
            Self::Right => button == 0x111,
            Self::Middle => button == 0x112,
        }
    }
}

/// Startup shape and keyboard travel scale for the Ocean engine. Empty reef
/// and bookmark lists intentionally produce an output-sized `main` reef and
/// a `home` bookmark at the world origin, so selecting Ocean is sufficient
/// for a usable first launch.
#[derive(Debug, Clone, PartialEq)]
pub struct OceanConfig {
    pub camera_step: i32,
    /// Dragging a reef tile with the normal move/resize gesture detaches it
    /// into a freely placed world rectangle. Reefs remain available as local
    /// tiling zones through the ordinary toggle-floating action.
    pub freeform_windows: bool,
    /// Keeps tiled Ocean move drags inside the reef and reattaches floating
    /// windows released near an existing tile.
    pub smart_tiling: bool,
    /// Maximum screen-pixel distance for floating-to-tiled attachment.
    pub smart_tiling_snap_distance: i32,
    /// Preserve a floating window's current size when it is reattached.
    pub smart_tiling_preserve_size: bool,
    /// Empty-canvas camera grab. `Disabled` leaves every button untouched.
    pub canvas_pan_button: OceanPanButton,
    /// When true, the configured `pointer_modifier` must accompany the canvas
    /// button. False gives the Drift-style direct empty-canvas drag.
    pub canvas_pan_requires_modifier: bool,
    /// Structural depth actions can be disabled without giving up the
    /// continuous 2D canvas itself.
    pub depth_enabled: bool,
    /// Camera zoom is independent of decorative water effects.
    pub zoom_enabled: bool,
    pub modifier_zoom: bool,
    pub min_zoom: f64,
    pub max_zoom: f64,
    pub zoom_step: f64,
    /// Zero makes keyboard/bookmark camera movement immediate.
    pub camera_animation_ms: u64,
    /// Screen-pixel arc applied perpendicular to an animated camera move.
    /// Zero keeps a straight path.
    pub camera_sway: f64,
    /// World-anchored reference field behind windows. This can be disabled
    /// independently from camera movement, zoom, depth, and water effects.
    pub canvas_guides: bool,
    pub canvas_grid_size: i32,
    pub canvas_grid_alpha: f32,
    /// Center navigation point shown only after camera movement.
    pub canvas_marker: bool,
    pub canvas_marker_fade_ms: u64,
    pub reefs: Vec<OceanReefConfig>,
    pub bookmarks: Vec<OceanBookmarkConfig>,
}

impl Default for OceanConfig {
    fn default() -> Self {
        Self {
            camera_step: 480,
            freeform_windows: true,
            smart_tiling: true,
            smart_tiling_snap_distance: 64,
            smart_tiling_preserve_size: true,
            canvas_pan_button: OceanPanButton::Left,
            canvas_pan_requires_modifier: false,
            depth_enabled: true,
            zoom_enabled: true,
            modifier_zoom: true,
            min_zoom: 0.25,
            max_zoom: 2.0,
            zoom_step: 1.2,
            camera_animation_ms: 260,
            camera_sway: 18.0,
            canvas_guides: true,
            canvas_grid_size: 240,
            canvas_grid_alpha: 0.10,
            canvas_marker: true,
            canvas_marker_fade_ms: 4200,
            reefs: Vec::new(),
            bookmarks: Vec::new(),
        }
    }
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
    /// Classic: switches the output's active numbered/named workspace.
    /// Ocean has no workspace concept -- a numbered ref instead jumps to
    /// that app-slot (see `Smallvil::jump_to_app_slot`, `OceanSpace::
    /// app_slot`); a named ref keeps the older camera-bookmark-jump
    /// behavior (`OceanSpace::animate_to_bookmark`), since app-slots are
    /// inherently numbered.
    SwitchWorkspace(WorkspaceRef),
    MoveToWorkspace(WorkspaceRef),
    /// Swaps the current output's active workspace content with the named
    /// output's. No default keybind -- output names are machine-specific
    /// (see `[[output]]`), so there's nothing sensible to bind out of the box.
    SwapWorkspacesWithOutput(String),
    /// Switches which keybind table is consulted (sway/Hyprland's "mode"/
    /// "submap" idea): a temporary layer of binds on top of `[keybinds]`,
    /// entered by name and left active until an explicit `exit-mode`
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
    /// (the one on the current workspace when there is one), switching
    /// workspace / traveling the camera there first if needed -- the dock
    /// and taskbar's "smart open": running app, click icon, it comes to
    /// you. No-op when the app has no mapped window; the caller (a bar's
    /// pin) is expected to spawn it when it has none.
    FocusApp(String),
    /// Sends a graceful close request to every mapped window of the given
    /// app_id -- the dock's "quit app". Apps that confirm-and-close on
    /// their own handle it; anything stuck must be killed by the caller.
    CloseApp(String),
    Quit,
}

fn is_depth_action(action: &Action) -> bool {
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

fn depth_action_enabled(
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

pub struct Config {
    /// The merged entry list this config was lowered from (both formats
    /// produce the same `waves::Entry` shape), kept for the reload diff:
    /// `reload_config` compares the new file's entries against these to
    /// know what actually changed and skip the window-affecting re-apply
    /// battery on a no-op save.
    pub(crate) loaded_entries: Vec<waves::Entry>,
    pub terminal: String,
    /// Startup-only spatial ownership model. Hot reload keeps the old value
    /// until the next TideWM launch so live windows never change owners.
    pub spatial_engine: SpatialEngine,
    pub ocean: OceanConfig,
    /// Physically-held modifier required for compositor mouse move/resize.
    /// This is separate from keybinds because `$mod` is only a Waves
    /// variable, not global compositor state; the shipped config points
    /// both at `$mod`, while existing configs keep the Super default.
    pub pointer_modifier: Mods,
    /// Shows a one-time startup toast pointing a new user at
    /// `Super+Enter`. True on a freshly-generated config; delete the key
    /// (or set it `false`) to stop seeing it. Not read on reload -- only
    /// checked once, at startup (see `main.rs`).
    pub show_welcome_hint: bool,
    /// Shows the short success card after a hot reload. Parse failures and
    /// warnings remain visible regardless: hiding diagnostics would make a
    /// broken configuration harder to repair.
    pub show_config_reload_toast: bool,
    pub water_effects: bool,
    /// Strength of TideWM's render-only interactive move/resize damping.
    /// `1.0` is the default half-life multiplier, `0.0` disables it, and
    /// larger values settle more slowly. `water_effects` is the master
    /// bypass. Matching window rules may override this per app.
    pub viscosity: f64,
    /// BSP resize pressure propagated through parallel ancestor splits.
    /// `water_effects` is the master bypass; this block can independently
    /// restore the legacy one-split resize path.
    pub connected_vessels: ConnectedVesselsConfig,
    /// Optional lateral sway for dragged floating windows. Opt-in and
    /// bypassed by the `water_effects` master toggle like viscosity.
    pub sway: SwayConfig,
    /// Cosmetic 2D bob-and-drift for floating windows (spatial roadmap F1,
    /// `light` tier). Opt-in and bypassed by `water_effects`; takes over
    /// from `sway` per window when enabled.
    pub float_physics: FloatPhysicsConfig,
    /// Continuous lateral "swim" between workspaces (spatial roadmap S0).
    /// Bypassed by the `water_effects` master toggle; when off, workspace
    /// navigation is the ordinary discrete switch.
    pub swim: SwimConfig,
    /// Bioluminescent edge-glow compass for off-screen urgent/deep windows
    /// (spatial roadmap S5). Ocean-only; `water_effects` is the master
    /// bypass.
    pub compass: CompassConfig,
    /// Whole-world overview minimap (spatial roadmap S5). Ocean-only;
    /// deliberately independent of `water_effects` -- see its own doc.
    pub minimap: MinimapConfig,
    /// Window lifecycle and layout-motion animation timing. Logical state
    /// changes immediately; this controls only visual settling.
    pub animations: WindowAnimationsConfig,
    /// Directional captured workspace wipe. Kept separate from the master
    /// `water_effects` toggle so the transition can be disabled or tuned
    /// without suppressing water-glass and ripples.
    pub workspace_transition: WorkspaceTransitionConfig,
    /// Automatic visual depth/buoyancy for inactive windows (Phase R1).
    /// `water_effects` remains the master toggle; this block controls the
    /// depth model without disabling ripples, water-glass, or transitions.
    pub depth: DepthConfig,
    /// Structural per-workspace parking for the Classic spatial engine.
    /// Independent of automatic visual depth and opt-in by default.
    pub classic_depth: ClassicDepthConfig,
    /// Shared frosted-glass appearance. A window opts into this shader with
    /// `glass = frost` in its rule; water remains the compatibility default
    /// for translucent floating windows with no explicit glass choice.
    pub frost: FrostConfig,
    /// Water-glass refraction motion: static, disturbance-reactive
    /// (default), or constant ambient drift.
    pub water_glass: WaterGlassConfig,
    /// Ambient caustic light over the wallpaper, below windows.
    pub caustics: CausticsConfig,
    /// Analytical window shadows. Independent of `water_effects`: shadows
    /// are general compositor decoration, while the water master toggle
    /// only gates TideWM's water/glass/depth identity effects.
    pub shadow: ShadowConfig,
    /// Compositor-owned window geometry clipping. The same resolved
    /// radii feed surface clipping, borders, glass and shadows.
    pub rounding: RoundingConfig,
    /// Analytical solid/gradient window borders.
    pub border: BorderConfig,
    /// Optional manual override for the compositor's own popup chrome
    /// (config warning panel, toast). Auto by default -- see `PopupConfig`.
    pub popup: PopupConfig,
    /// Global ripple defaults (Phase R1, see `ripple.rs`). Per-app
    /// `rule { ripple { } }` overrides merge over these at resolve time
    /// (`merge_over`). Kept sparse; `resolve_ripple_config` layers the
    /// system defaults and any selected named preset underneath.
    pub ripple: RippleConfig,
    /// Reusable named visual bundles declared with
    /// `ripple_preset <name> { }`.
    pub ripple_presets: HashMap<String, RippleConfig>,
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
    /// Known-safe temporary table activated only by the compositor rescue
    /// chord. It never merges into normal Waves bindings.
    pub rescue_keybinds: Vec<Keybind>,
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
        let lua = mlua::Lua::new_with(
            mlua::StdLib::MATH | mlua::StdLib::STRING | mlua::StdLib::TABLE,
            mlua::LuaOptions::default(),
        )
        .expect("creating the config Lua state must not fail");
        Self::load_with_error_in(&lua, &wave::TideInfo::default())
    }

    /// [`Config::load_with_error`] on a caller-owned session Lua with the
    /// live-compositor facts (`tide` table): the runtime path, so
    /// hardware conditionals see the real machine and config globals
    /// persist for `tidectl eval` and `on` handlers.
    pub(crate) fn load_with_error_in(
        lua: &mlua::Lua,
        tide: &wave::TideInfo,
    ) -> (Self, Option<String>, Vec<String>) {
        let path = config_path();

        let (raw, error, include_warnings, entries) = if path.exists() {
            match load_raw_config_in(lua, tide, &path) {
                Ok((raw, include_warnings, entries)) => (raw, None, include_warnings, entries),
                Err(err) => {
                    tracing::warn!(%err, path = %path.display(), "Failed to parse config, using defaults");
                    (RawConfig::default(), Some(err), Vec::new(), Vec::new())
                }
            }
        } else {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(err) = fs::write(&path, DEFAULT_CONFIG_WAVE) {
                tracing::warn!(%err, path = %path.display(), "Failed to write default config");
            }
            // Guarded by existence, not just overwritten: a stray
            // `keybinds.wave` a user already has (from a previous partial
            // run, or hand-placed ahead of time) must not be clobbered
            // just because `config.wave` itself didn't exist yet.
            let keybinds_path = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("keybinds.wave");
            if !keybinds_path.exists() {
                if let Err(err) = fs::write(&keybinds_path, DEFAULT_KEYBINDS_WAVE) {
                    tracing::warn!(%err, path = %keybinds_path.display(), "Failed to write default keybinds");
                }
            }
            // Load through the real disk-resolving path (not an in-memory
            // parse of the constant) so `include "keybinds.wave"` above
            // actually resolves on this very first boot, not just on the
            // next reload.
            let (default, include_warnings, entries) = load_raw_config_in(lua, tide, &path)
                .unwrap_or_else(|err| {
                    tracing::error!(%err, "Built-in default Waves config failed to parse");
                    (RawConfig::default(), Vec::new(), Vec::new())
                });
            (default, None, include_warnings, entries)
        };

        let (mut config, mut warnings) = Self::from_raw(raw);
        warnings.extend(include_warnings);
        config.loaded_entries = entries;
        (config, error, warnings)
    }

    /// Re-read the config file for a hot-reload. Unlike `load`, this never
    /// writes a default file and reports a hard parse failure instead of
    /// silently falling back, so a reload with a typo in it doesn't quietly
    /// wipe out whatever the user had. The `Vec<String>` alongside a
    /// successful reload is the same dropped-keybind/footgun-lint
    /// diagnostics `from_raw` produces -- empty in the common case.
    pub fn reload() -> Result<(Self, Vec<String>), String> {
        let lua = mlua::Lua::new_with(
            mlua::StdLib::MATH | mlua::StdLib::STRING | mlua::StdLib::TABLE,
            mlua::LuaOptions::default(),
        )
        .map_err(|e| format!("failed to create config Lua state: {e}"))?;
        Self::reload_in(&lua, &wave::TideInfo::default())
    }

    /// [`Config::reload`] on the session Lua with live-compositor facts.
    pub(crate) fn reload_in(
        lua: &mlua::Lua,
        tide: &wave::TideInfo,
    ) -> Result<(Self, Vec<String>), String> {
        let (raw, include_warnings, entries) = load_raw_config_in(lua, tide, &config_path())?;
        let (mut config, mut warnings) = Self::from_raw(raw);
        warnings.extend(include_warnings);
        config.loaded_entries = entries;
        Ok((config, warnings))
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
        let ocean_selected = raw.spatial_engine.trim().eq_ignore_ascii_case("ocean");
        let classic_depth_enabled = raw.classic_depth.enabled;
        let ocean_depth_enabled = raw.ocean.depth_enabled;
        let keybinds = raw
            .keybinds
            .iter()
            .filter_map(|(combo, action)| parse_keybind(combo, action, true, &mut warnings))
            .filter(|bind| {
                depth_action_enabled(
                    &bind.action,
                    ocean_selected,
                    classic_depth_enabled,
                    ocean_depth_enabled,
                )
            })
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
                    .filter(|bind| {
                        depth_action_enabled(
                            &bind.action,
                            ocean_selected,
                            classic_depth_enabled,
                            ocean_depth_enabled,
                        )
                    })
                    .collect();
                (name.clone(), parsed)
            })
            .collect();

        let default_layout = parse_layout_algorithm(&raw.default_layout).unwrap_or_else(|| {
            if !raw.default_layout.is_empty() {
                tracing::warn!(value = %raw.default_layout, "Unknown layout, using bsp");
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
        let pointer_modifier = parse_modifiers(&raw.pointer_modifier).unwrap_or_else(|| {
            warnings.push(format!(
                "Invalid pointer_modifier \"{}\"; using Super",
                raw.pointer_modifier
            ));
            Mods {
                logo: true,
                ..Default::default()
            }
        });
        let spatial_engine = match raw.spatial_engine.trim().to_ascii_lowercase().as_str() {
            "classic" | "workspace" | "workspaces" => SpatialEngine::Classic,
            "ocean" | "canvas" => SpatialEngine::Ocean,
            other => {
                warnings.push(format!("Invalid spatial_engine \"{other}\"; using classic"));
                SpatialEngine::Classic
            }
        };
        let workspace_names = parse_workspace_names(&raw.workspace_names);
        let workspace_gaps = parse_workspace_gaps(&raw.workspace_gaps, &workspace_names);
        let rescue_keybinds = RawConfig::default()
            .keybinds
            .iter()
            .filter_map(|(combo, action)| parse_keybind(combo, action, false, &mut Vec::new()))
            .filter(|bind| !matches!(bind.action, Action::EnterSubmap(_)))
            .collect();

        let config = Self {
            loaded_entries: Vec::new(),
            terminal: raw.terminal,
            spatial_engine,
            ocean: raw.ocean,
            pointer_modifier,
            show_welcome_hint: raw.show_welcome_hint,
            show_config_reload_toast: raw.show_config_reload_toast,
            water_effects: raw.water_effects,
            viscosity: raw.viscosity.clamp(0.0, 4.0),
            connected_vessels: raw.connected_vessels,
            sway: raw.sway,
            float_physics: raw.float_physics,
            swim: raw.swim,
            compass: raw.compass,
            minimap: raw.minimap,
            animations: raw.animations,
            workspace_transition: raw.workspace_transition,
            depth: raw.depth,
            classic_depth: raw.classic_depth,
            frost: raw.frost,
            water_glass: raw.water_glass,
            caustics: raw.caustics,
            shadow: raw.shadow,
            rounding: raw.rounding,
            border: raw.border,
            popup: raw.popup,
            ripple: raw.ripple,
            ripple_presets: raw.ripple_presets,
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
            rescue_keybinds,
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

    /// Resolves system defaults, a selected named preset, global tuning, and
    /// per-app tuning in that order for one concrete trigger. Named presets
    /// may inherit another named preset; cycles and unknown names safely fall
    /// back to the configuration accumulated so far.
    pub(crate) fn resolve_ripple_config(
        &self,
        rule: Option<&RippleConfig>,
        trigger: RippleTrigger,
    ) -> RippleConfig {
        let mut stack = Vec::new();
        let global = self.apply_ripple_scope(
            RippleConfig::system_default(),
            &self.ripple,
            trigger,
            &mut stack,
        );
        match rule {
            Some(rule) => self.apply_ripple_scope(global, rule, trigger, &mut stack),
            None => global,
        }
    }

    fn apply_ripple_scope(
        &self,
        mut base: RippleConfig,
        scope: &RippleConfig,
        trigger: RippleTrigger,
        stack: &mut Vec<String>,
    ) -> RippleConfig {
        if let Some(selection) = scope.preset_for(trigger).cloned() {
            match selection {
                RipplePresetSelection::BuiltIn(preset) => {
                    base.preset = Some(RipplePresetSelection::BuiltIn(preset));
                }
                RipplePresetSelection::Named(name) => {
                    if stack.len() >= 16 || stack.contains(&name) {
                        tracing::warn!(preset = name, "Cyclic ripple preset inheritance, ignoring");
                    } else if let Some(named) = self.ripple_presets.get(&name) {
                        stack.push(name);
                        base = self.apply_ripple_scope(base, named, trigger, stack);
                        stack.pop();
                    } else {
                        tracing::warn!(preset = name, "Unknown named ripple preset, ignoring");
                    }
                }
            }
        }

        // The selection was expanded above. Merge only the scope's concrete
        // knobs now so local values override the reusable preset bundle.
        let mut tuning = scope.clone();
        tuning.preset = None;
        tuning.map_preset = None;
        tuning.focus_preset = None;
        tuning.urgent_preset = None;
        base.merge_over(&tuning)
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
        pid: Option<i32>,
        is_xwayland: bool,
    ) -> WindowRule {
        let mut effective = WindowRule::default();
        for rule in &self.window_rules {
            if !rule.matches(app_id, title, pid, is_xwayland) {
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
            effective.swallow |= rule.swallow;
            if rule.position.is_some() {
                effective.position = rule.position;
            }
            if rule.size.is_some() {
                effective.size = rule.size;
            }
            if rule.opacity.is_some() {
                effective.opacity = rule.opacity;
            }
            if rule.active_opacity.is_some() {
                effective.active_opacity = rule.active_opacity;
            }
            if rule.inactive_opacity.is_some() {
                effective.inactive_opacity = rule.inactive_opacity;
            }
            if rule.fullscreen_opacity.is_some() {
                effective.fullscreen_opacity = rule.fullscreen_opacity;
            }
            if rule.glass.is_some() {
                effective.glass = rule.glass;
            }
            if rule.viscosity.is_some() {
                effective.viscosity = rule.viscosity;
            }
            if rule.sway.is_some() {
                effective.sway = rule.sway;
            }
            if rule.float_physics.is_some() {
                effective.float_physics = rule.float_physics;
            }
            if rule.depth.is_some() {
                effective.depth = rule.depth;
            }
            if let Some(rule_frost) = &rule.frost {
                effective.frost = Some(match effective.frost.take() {
                    Some(existing) => existing.merge_over(rule_frost),
                    None => rule_frost.clone(),
                });
            }
            if let Some(rule_shadow) = &rule.shadow {
                effective.shadow = Some(match effective.shadow.take() {
                    Some(existing) => existing.merge_over(rule_shadow),
                    None => rule_shadow.clone(),
                });
            }
            if let Some(rule_rounding) = &rule.rounding {
                effective.rounding = Some(match effective.rounding.take() {
                    Some(existing) => existing.merge_over(rule_rounding),
                    None => rule_rounding.clone(),
                });
            }
            if let Some(rule_border) = &rule.border {
                effective.border = Some(match effective.border.take() {
                    Some(existing) => existing.merge_over(rule_border),
                    None => rule_border.clone(),
                });
            }
            if let Some(rule_ripple) = &rule.ripple {
                // Per-rule ripple overrides accumulate field-by-field:
                // an earlier matching rule sets some knobs, a later one
                // can layer more on top. Distinct from the "last wins"
                // of scalars above because a `ripple { }` sub-block is
                // itself a partial set, not a single value. Each
                // matched rule's sub-block merges over whatever the
                // previous matches left.
                effective.ripple = Some(match effective.ripple.take() {
                    Some(existing) => existing.merge_over(rule_ripple),
                    None => rule_ripple.clone(),
                });
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
    spatial_engine: String,
    ocean: OceanConfig,
    /// `super`, `alt`, `ctrl`, `shift`, or a `+`-joined combination.
    /// Resolved after Waves variable substitution in `Config::from_raw`.
    pointer_modifier: String,
    show_welcome_hint: bool,
    show_config_reload_toast: bool,
    water_effects: bool,
    viscosity: f64,
    connected_vessels: ConnectedVesselsConfig,
    sway: SwayConfig,
    float_physics: FloatPhysicsConfig,
    swim: SwimConfig,
    compass: CompassConfig,
    minimap: MinimapConfig,
    animations: WindowAnimationsConfig,
    workspace_transition: WorkspaceTransitionConfig,
    depth: DepthConfig,
    classic_depth: ClassicDepthConfig,
    frost: FrostConfig,
    water_glass: WaterGlassConfig,
    caustics: CausticsConfig,
    shadow: ShadowConfig,
    rounding: RoundingConfig,
    border: BorderConfig,
    popup: PopupConfig,
    /// Sparse global overrides, mutated by `apply_ripple_block`. Runtime
    /// resolution layers these over `RippleConfig::system_default()`.
    ripple: RippleConfig,
    ripple_presets: HashMap<String, RippleConfig>,
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
}

impl Default for RawConfig {
    fn default() -> Self {
        let mut keybinds = HashMap::new();
        keybinds.insert("Super+Return".to_string(), "spawn:kitty".to_string());
        keybinds.insert("Super+Q".to_string(), "close-window".to_string());
        keybinds.insert("Super+V".to_string(), "toggle-floating".to_string());
        keybinds.insert("Super+F".to_string(), "toggle-fullscreen".to_string());
        keybinds.insert(
            "Super+M".to_string(),
            "toggle-border-fullscreen".to_string(),
        );
        keybinds.insert("Super+Shift+M".to_string(), "resize-to-monitor".to_string());
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
        // own exit-mode bind -- not tied to focus or any other implicit
        // event. This one's a vim-motion focus-move mode (h/j/k/l with no
        // modifier held); a resize mode is the more common example
        // elsewhere, but needs a keyboard resize action this project
        // doesn't have yet.
        let mut nav_submap = HashMap::new();
        nav_submap.insert("h".to_string(), "focus-left".to_string());
        nav_submap.insert("l".to_string(), "focus-right".to_string());
        nav_submap.insert("k".to_string(), "focus-up".to_string());
        nav_submap.insert("j".to_string(), "focus-down".to_string());
        nav_submap.insert("Escape".to_string(), "exit-mode".to_string());
        let mut submaps = HashMap::new();
        submaps.insert("nav".to_string(), nav_submap);

        Self {
            terminal: "kitty".to_string(),
            spatial_engine: "classic".to_string(),
            ocean: OceanConfig::default(),
            pointer_modifier: "super".to_string(),
            // Deliberately false, not true: a real config.wave always ships
            // with `show_welcome_hint = true` written explicitly (see
            // DEFAULT_CONFIG_WAVE), so this default is only ever consulted
            // when a user deletes the key from an existing file -- and per
            // the on-screen hint's own "delete this to dismiss" advice
            // (welcome.rs), that must resolve to off, not back to on.
            show_welcome_hint: false,
            show_config_reload_toast: true,
            water_effects: true,
            viscosity: 1.0,
            connected_vessels: ConnectedVesselsConfig::default(),
            sway: SwayConfig::default(),
            float_physics: FloatPhysicsConfig::default(),
            swim: SwimConfig::default(),
            compass: CompassConfig::default(),
            minimap: MinimapConfig::default(),
            animations: WindowAnimationsConfig::default(),
            workspace_transition: WorkspaceTransitionConfig::default(),
            depth: DepthConfig::default(),
            classic_depth: ClassicDepthConfig::default(),
            frost: FrostConfig::default(),
            water_glass: WaterGlassConfig::default(),
            caustics: CausticsConfig::default(),
            shadow: ShadowConfig::default(),
            rounding: RoundingConfig::default(),
            border: BorderConfig::default(),
            popup: PopupConfig::default(),
            ripple: RippleConfig::default(),
            ripple_presets: HashMap::new(),
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
        }
    }
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
            unfocus_on_empty: false,
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
/// `title`/`pid`/`xwayland` (i3/sway's `for_window`, Hyprland's
/// `windowrule` idea) and apply
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
    /// Exact PID match (sway's `[pid=...]` criterion). Never `Some` unless
    /// a window's real client PID is known at match time; a dead/unknown
    /// client never satisfies this even if the rule would otherwise match.
    pub pid: Option<i32>,
    /// Tri-state XWayland match (Hyprland's `xwayland:1`/`xwayland:0`).
    /// `None` matches either kind of window; `Some(true)`/`Some(false)`
    /// requires the window to be (or not be) an X11 client running
    /// through `xwayland-satellite`. See `Smallvil::is_xwayland_surface`.
    pub is_xwayland: Option<bool>,
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
    /// Marks matching windows as swallowers (Hyprland's
    /// `misc:enable_swallow`, i3's window swallowing): a tiled window this
    /// rule matches gets hidden when a window whose process it spawned
    /// maps (PID ancestry via `/proc`), which takes over its tile; closing
    /// the child puts the swallower back in that exact slot. The classic
    /// use is `app_id = kitty, swallow = true` so a video/image viewer
    /// launched from a terminal replaces it instead of splitting it.
    /// Tiled swallowers only -- a floating terminal keeps both visible.
    pub swallow: bool,
    /// Per-window render alpha in the inclusive range 0.0..=1.0. Applied
    /// as a base multiplier to the complete surface tree (including
    /// subsurfaces and popups).
    pub opacity: Option<f32>,
    /// State-specific opacity multipliers. These multiply `opacity` when
    /// it is set, matching Hyprland's active/inactive/fullscreen model.
    /// Fullscreen wins over focus state.
    pub active_opacity: Option<f32>,
    pub inactive_opacity: Option<f32>,
    pub fullscreen_opacity: Option<f32>,
    /// Which captured-backdrop shader sits behind this floating window.
    /// Explicit modes also work with client-provided surface alpha, which
    /// keeps glyphs/foreground pixels opaque. Missing preserves the original
    /// behavior where compositor `opacity` below one implies water;
    /// `Plain` disables backdrop substitution while preserving `opacity`.
    pub glass: Option<GlassMode>,
    /// Per-window interactive move/resize damping. Last matching rule wins;
    /// `0.0` disables damping for the matched app.
    pub viscosity: Option<f64>,
    /// Per-app opt-in/out for floating sway. Last matching rule wins;
    /// unset falls back to the global `sway { enabled }` value.
    pub sway: Option<bool>,
    /// Per-app tier override for cosmetic float physics (F1). Last
    /// matching rule wins; unset falls back to the global
    /// `float_physics { tier }` value. When resolved `light` or `full`, it
    /// takes over from `sway` for that window -- the two never stack.
    pub float_physics: Option<FloatPhysicsTier>,
    /// Per-app buoyancy override for the automatic depth/attention system
    /// (Phase R1). `Some(false)` pins the matched window at tier zero
    /// forever -- it never dims or sinks regardless of inactivity, useful
    /// for a widget or player the user always wants to see live. `Some(true)`
    /// affirms the normal automatic behavior (mainly useful to override an
    /// earlier matching rule's `false`). Unset inherits the global
    /// `depth { enabled }` behavior. Last matching rule wins.
    pub depth: Option<bool>,
    /// Per-app frost overrides. Unset fields inherit the global
    /// `frost { }` block, and multiple matching rules merge field by field.
    pub frost: Option<FrostOverrides>,
    /// Per-app analytical-shadow overrides. Unset fields inherit the global
    /// `shadow { }` block; matching rule sub-blocks merge field by field.
    pub shadow: Option<ShadowOverrides>,
    /// Per-app rounded-geometry overrides. Unset fields inherit the global
    /// `rounding { }` block; matching rules merge field by field.
    pub rounding: Option<RoundingOverrides>,
    /// Per-app analytical-border overrides.
    pub border: Option<BorderOverrides>,
    /// Exact floating placement (top-left corner), `<x>x<y>` -- the same
    /// syntax `[[output]]`'s `position` already uses. No-op unless the
    /// window ends up floating (from `float`/`pin`/the auto-float
    /// heuristic), same "only means something once floating" restriction
    /// `pseudo_tile` has in reverse for tiled.
    pub position: Option<(i32, i32)>,
    /// Exact floating size, `<width>x<height>` (same syntax as `position`).
    pub size: Option<(i32, i32)>,
    /// Per-app ripple overrides; `None` fields inherit the global
    /// `ripple { }` block. Set `enabled = false` inside this sub-block to
    /// suppress ripples entirely for matching windows. See `RippleConfig`.
    pub ripple: Option<RippleConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlassMode {
    Water,
    Frost,
    Plain,
}

/// Resolved compositor opacity multipliers for one window. A missing entry
/// means the window stays at its client-provided opacity.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WindowOpacity {
    pub base: Option<f32>,
    pub active: Option<f32>,
    pub inactive: Option<f32>,
    pub fullscreen: Option<f32>,
}

impl WindowOpacity {
    pub fn from_rule(rule: &WindowRule) -> Option<Self> {
        let opacity = Self {
            base: rule.opacity,
            active: rule.active_opacity,
            inactive: rule.inactive_opacity,
            fullscreen: rule.fullscreen_opacity,
        };
        (opacity.base.is_some()
            || opacity.active.is_some()
            || opacity.inactive.is_some()
            || opacity.fullscreen.is_some())
        .then_some(opacity)
    }

    pub fn alpha(self, focused: bool, fullscreen: bool) -> f32 {
        let state = if fullscreen {
            self.fullscreen
        } else if focused {
            self.active
        } else {
            self.inactive
        };
        (self.base.unwrap_or(1.0) * state.unwrap_or(1.0)).clamp(0.0, 1.0)
    }
}

/// Global tuning for the bounded single-pass frost shader. The backdrop
/// capture is already window-sized, so cost scales with translucent window
/// area rather than allocating another full-output buffer.
#[derive(Debug, Clone)]
pub struct FrostConfig {
    pub enabled: bool,
    /// Sampling radius in physical pixels. Zero keeps the color treatment
    /// but bypasses diffusion.
    pub radius: f32,
    /// Mix between the original sharp capture and the diffused result.
    pub strength: f32,
    /// Opacity of the processed backdrop layer. Reducing this lets the
    /// undistorted desktop beneath it show through.
    pub opacity: f32,
    pub saturation: f32,
    pub contrast: f32,
    pub brightness: f32,
    /// Static grain used to reduce banding in smooth blur gradients.
    pub noise: f32,
    pub noise_scale: f32,
    /// Additional saturation, optionally biased toward darker pixels.
    pub vibrancy: f32,
    pub vibrancy_darkness: f32,
    pub tint_color: [f32; 3],
    pub tint_alpha: f32,
    /// Rounded clipping of the frost layer itself, in physical pixels.
    pub corner_radius: f32,
    pub corner_softness: f32,
}

impl Default for FrostConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            radius: 12.0,
            strength: 1.0,
            opacity: 1.0,
            saturation: 1.0,
            contrast: 1.0,
            brightness: 1.0,
            noise: 0.0,
            noise_scale: 1.0,
            vibrancy: 0.0,
            vibrancy_darkness: 0.0,
            tint_color: [142.0 / 255.0, 221.0 / 255.0, 1.0],
            tint_alpha: 0.0,
            corner_radius: 0.0,
            corner_softness: 1.0,
        }
    }
}

/// Ambient caustic light patterns over the wallpaper, below windows.
/// `water_effects` is the master bypass. `fps = 0` (default) animates only
/// on frames that are already being rendered for some other reason
/// (damage, an active animation elsewhere); a non-zero value opts into
/// constant motion at roughly that frame rate, keeping the wallpaper
/// "breathing" at the cost of a steady redraw.
#[derive(Debug, Clone, PartialEq)]
pub struct CausticsConfig {
    pub enabled: bool,
    /// Peak alpha contribution, 0..1.
    pub intensity: f32,
    /// Linear RGB tint of the light ridges.
    pub color: [f32; 3],
    /// Pattern size multiplier; higher packs more cells per output.
    pub scale: f32,
    /// Phase drift speed multiplier.
    pub speed: f32,
    /// Constant-motion frame rate, 0 for damage-piggyback. Clamped to a
    /// small range at parse time.
    pub fps: u32,
    /// Idle-decay frame rates: after `idle_after_ms[i]` milliseconds of no
    /// input activity, the effective frame rate drops to `idle_fps[i]`.
    /// Both lists are parallel, ascending in time, and empty means no
    /// decay (the current behavior). Defaults: 30fps after 5 minutes,
    /// 15fps after 10, 10fps after 30.
    pub idle_fps: Vec<u32>,
    pub idle_after_ms: Vec<u64>,
}

impl Default for CausticsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.35,
            color: [0.55, 0.85, 1.0],
            scale: 1.0,
            speed: 1.0,
            fps: 0,
            idle_fps: vec![30, 15, 10],
            idle_after_ms: vec![300_000, 600_000, 1_800_000],
        }
    }
}

/// Sparse per-window overrides for [`FrostConfig`]. Every `None` inherits
/// the global frost value; this makes a rule able to change one knob
/// without copying the complete global block.
#[derive(Debug, Clone, Default)]
pub struct FrostOverrides {
    pub enabled: Option<bool>,
    pub radius: Option<f32>,
    pub strength: Option<f32>,
    pub opacity: Option<f32>,
    pub saturation: Option<f32>,
    pub contrast: Option<f32>,
    pub brightness: Option<f32>,
    pub noise: Option<f32>,
    pub noise_scale: Option<f32>,
    pub vibrancy: Option<f32>,
    pub vibrancy_darkness: Option<f32>,
    pub tint_color: Option<[f32; 3]>,
    pub tint_alpha: Option<f32>,
    pub corner_radius: Option<f32>,
    pub corner_softness: Option<f32>,
}

impl FrostOverrides {
    pub fn merge_over(&self, other: &Self) -> Self {
        Self {
            enabled: other.enabled.or(self.enabled),
            radius: other.radius.or(self.radius),
            strength: other.strength.or(self.strength),
            opacity: other.opacity.or(self.opacity),
            saturation: other.saturation.or(self.saturation),
            contrast: other.contrast.or(self.contrast),
            brightness: other.brightness.or(self.brightness),
            noise: other.noise.or(self.noise),
            noise_scale: other.noise_scale.or(self.noise_scale),
            vibrancy: other.vibrancy.or(self.vibrancy),
            vibrancy_darkness: other.vibrancy_darkness.or(self.vibrancy_darkness),
            tint_color: other.tint_color.or(self.tint_color),
            tint_alpha: other.tint_alpha.or(self.tint_alpha),
            corner_radius: other.corner_radius.or(self.corner_radius),
            corner_softness: other.corner_softness.or(self.corner_softness),
        }
    }

    pub fn apply_to(&self, base: &FrostConfig) -> FrostConfig {
        FrostConfig {
            enabled: self.enabled.unwrap_or(base.enabled),
            radius: self.radius.unwrap_or(base.radius),
            strength: self.strength.unwrap_or(base.strength),
            opacity: self.opacity.unwrap_or(base.opacity),
            saturation: self.saturation.unwrap_or(base.saturation),
            contrast: self.contrast.unwrap_or(base.contrast),
            brightness: self.brightness.unwrap_or(base.brightness),
            noise: self.noise.unwrap_or(base.noise),
            noise_scale: self.noise_scale.unwrap_or(base.noise_scale),
            vibrancy: self.vibrancy.unwrap_or(base.vibrancy),
            vibrancy_darkness: self.vibrancy_darkness.unwrap_or(base.vibrancy_darkness),
            tint_color: self.tint_color.unwrap_or(base.tint_color),
            tint_alpha: self.tint_alpha.unwrap_or(base.tint_alpha),
            corner_radius: self.corner_radius.unwrap_or(base.corner_radius),
            corner_softness: self.corner_softness.unwrap_or(base.corner_softness),
        }
    }
}

/// Fixed-cost analytical window-shadow tuning. The terminology deliberately
/// accepts both niri's CSS-like model and Hyprland's model:
/// `softness`/`spread`/`draw_behind_window`, plus
/// `render_power`/`sharp`/`scale`.
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    pub enabled: bool,
    /// Soft falloff reach in logical pixels (`range`/`size` are aliases).
    pub softness: f32,
    /// CSS-style expansion before falloff; may be negative.
    pub spread: f32,
    /// Logical-pixel x/y offset.
    pub offset: (f32, f32),
    /// Scales the shadow's base rectangle around the window center.
    pub scale: f32,
    /// Falloff exponent; higher values concentrate the shadow at the edge.
    pub render_power: f32,
    /// Hard edge, bypassing the soft falloff.
    pub sharp: bool,
    /// If false, cut the actual window rectangle out of the shadow. This is
    /// the color-safe default for translucent/frosted windows.
    pub draw_behind_window: bool,
    pub color: [f32; 4],
    pub inactive_color: [f32; 4],
    /// TideWM extension: urgent windows can glow aqua without needing a
    /// separate border implementation.
    pub urgent_color: [f32; 4],
    pub opacity: f32,
    pub inactive_opacity: f32,
    pub urgent_opacity: f32,
    /// Shadow geometry rounding in logical pixels. This becomes the shared
    /// compositor corner radius when the next R2 rounded-corners slice lands.
    pub corner_radius: f32,
    /// Limit shadows to floating windows. Off matches compositor-wide
    /// Hyprland/niri behavior; useful to enable when tiled gaps are narrow.
    pub floating_only: bool,
    /// Fullscreen shadows are normally invisible beyond the output and just
    /// consume fill rate, so they default off but remain selectable.
    pub fullscreen: bool,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            softness: 28.0,
            spread: 2.0,
            offset: (0.0, 8.0),
            scale: 1.0,
            render_power: 2.0,
            sharp: false,
            // Do not put a dark/color filter under translucent windows.
            draw_behind_window: false,
            color: [4.0 / 255.0, 14.0 / 255.0, 19.0 / 255.0, 122.0 / 255.0],
            inactive_color: [3.0 / 255.0, 8.0 / 255.0, 12.0 / 255.0, 77.0 / 255.0],
            urgent_color: [46.0 / 255.0, 199.0 / 255.0, 1.0, 184.0 / 255.0],
            opacity: 1.0,
            inactive_opacity: 1.0,
            urgent_opacity: 1.0,
            corner_radius: 0.0,
            floating_only: false,
            fullscreen: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ShadowOverrides {
    pub enabled: Option<bool>,
    pub softness: Option<f32>,
    pub spread: Option<f32>,
    pub offset_x: Option<f32>,
    pub offset_y: Option<f32>,
    pub scale: Option<f32>,
    pub render_power: Option<f32>,
    pub sharp: Option<bool>,
    pub draw_behind_window: Option<bool>,
    pub color: Option<[f32; 4]>,
    pub inactive_color: Option<[f32; 4]>,
    pub urgent_color: Option<[f32; 4]>,
    pub opacity: Option<f32>,
    pub inactive_opacity: Option<f32>,
    pub urgent_opacity: Option<f32>,
    pub corner_radius: Option<f32>,
    pub floating_only: Option<bool>,
    pub fullscreen: Option<bool>,
}

impl ShadowOverrides {
    pub fn merge_over(&self, other: &Self) -> Self {
        Self {
            enabled: other.enabled.or(self.enabled),
            softness: other.softness.or(self.softness),
            spread: other.spread.or(self.spread),
            offset_x: other.offset_x.or(self.offset_x),
            offset_y: other.offset_y.or(self.offset_y),
            scale: other.scale.or(self.scale),
            render_power: other.render_power.or(self.render_power),
            sharp: other.sharp.or(self.sharp),
            draw_behind_window: other.draw_behind_window.or(self.draw_behind_window),
            color: other.color.or(self.color),
            inactive_color: other.inactive_color.or(self.inactive_color),
            urgent_color: other.urgent_color.or(self.urgent_color),
            opacity: other.opacity.or(self.opacity),
            inactive_opacity: other.inactive_opacity.or(self.inactive_opacity),
            urgent_opacity: other.urgent_opacity.or(self.urgent_opacity),
            corner_radius: other.corner_radius.or(self.corner_radius),
            floating_only: other.floating_only.or(self.floating_only),
            fullscreen: other.fullscreen.or(self.fullscreen),
        }
    }

    pub fn apply_to(&self, base: &ShadowConfig) -> ShadowConfig {
        ShadowConfig {
            enabled: self.enabled.unwrap_or(base.enabled),
            softness: self.softness.unwrap_or(base.softness),
            spread: self.spread.unwrap_or(base.spread),
            offset: (
                self.offset_x.unwrap_or(base.offset.0),
                self.offset_y.unwrap_or(base.offset.1),
            ),
            scale: self.scale.unwrap_or(base.scale),
            render_power: self.render_power.unwrap_or(base.render_power),
            sharp: self.sharp.unwrap_or(base.sharp),
            draw_behind_window: self.draw_behind_window.unwrap_or(base.draw_behind_window),
            color: self.color.unwrap_or(base.color),
            inactive_color: self.inactive_color.unwrap_or(base.inactive_color),
            urgent_color: self.urgent_color.unwrap_or(base.urgent_color),
            opacity: self.opacity.unwrap_or(base.opacity),
            inactive_opacity: self.inactive_opacity.unwrap_or(base.inactive_opacity),
            urgent_opacity: self.urgent_opacity.unwrap_or(base.urgent_opacity),
            corner_radius: self.corner_radius.unwrap_or(base.corner_radius),
            floating_only: self.floating_only.unwrap_or(base.floating_only),
            fullscreen: self.fullscreen.unwrap_or(base.fullscreen),
        }
    }
}

/// Rounded client geometry shared by clipping and compositor decoration.
/// Radii use CSS order: top-left, top-right, bottom-right, bottom-left.
#[derive(Debug, Clone)]
pub struct RoundingConfig {
    pub enabled: bool,
    pub radii: [f32; 4],
    /// Superellipse exponent. 2 is a circle; higher values produce
    /// Hyprland-style squarer corners while keeping the same radius.
    pub power: f32,
    /// Physical-pixel antialias width.
    pub antialias: f32,
    /// Clip the actual toplevel surface tree to the rounded geometry.
    pub clip: bool,
    pub floating_only: bool,
    pub fullscreen: bool,
}

impl Default for RoundingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            radii: [12.0; 4],
            power: 2.0,
            antialias: 1.0,
            clip: true,
            floating_only: false,
            fullscreen: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RoundingOverrides {
    pub enabled: Option<bool>,
    pub top_left: Option<f32>,
    pub top_right: Option<f32>,
    pub bottom_right: Option<f32>,
    pub bottom_left: Option<f32>,
    pub power: Option<f32>,
    pub antialias: Option<f32>,
    pub clip: Option<bool>,
    pub floating_only: Option<bool>,
    pub fullscreen: Option<bool>,
}

impl RoundingOverrides {
    pub fn merge_over(&self, other: &Self) -> Self {
        Self {
            enabled: other.enabled.or(self.enabled),
            top_left: other.top_left.or(self.top_left),
            top_right: other.top_right.or(self.top_right),
            bottom_right: other.bottom_right.or(self.bottom_right),
            bottom_left: other.bottom_left.or(self.bottom_left),
            power: other.power.or(self.power),
            antialias: other.antialias.or(self.antialias),
            clip: other.clip.or(self.clip),
            floating_only: other.floating_only.or(self.floating_only),
            fullscreen: other.fullscreen.or(self.fullscreen),
        }
    }

    pub fn apply_to(&self, base: &RoundingConfig) -> RoundingConfig {
        RoundingConfig {
            enabled: self.enabled.unwrap_or(base.enabled),
            radii: [
                self.top_left.unwrap_or(base.radii[0]),
                self.top_right.unwrap_or(base.radii[1]),
                self.bottom_right.unwrap_or(base.radii[2]),
                self.bottom_left.unwrap_or(base.radii[3]),
            ],
            power: self.power.unwrap_or(base.power),
            antialias: self.antialias.unwrap_or(base.antialias),
            clip: self.clip.unwrap_or(base.clip),
            floating_only: self.floating_only.unwrap_or(base.floating_only),
            fullscreen: self.fullscreen.unwrap_or(base.fullscreen),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BorderPlacement {
    Outside,
    Center,
    Inside,
}

/// Fixed-cost rounded border with independent focus-state gradients.
#[derive(Debug, Clone)]
pub struct BorderConfig {
    pub enabled: bool,
    pub width: f32,
    pub placement: BorderPlacement,
    pub active_from: [f32; 4],
    pub active_to: [f32; 4],
    pub inactive_from: [f32; 4],
    pub inactive_to: [f32; 4],
    pub urgent_from: [f32; 4],
    pub urgent_to: [f32; 4],
    pub angle: f32,
    pub opacity: f32,
    pub inactive_opacity: f32,
    pub urgent_opacity: f32,
    pub animate: bool,
    pub animate_focused: bool,
    pub animate_inactive: bool,
    pub animate_urgent: bool,
    /// Keep the inactive border visible. Turning this off gives a classic
    /// focus-ring-only look without affecting urgent borders.
    pub inactive_enabled: bool,
    /// Gradient rotation in degrees per second.
    pub animation_speed: f32,
    /// Optional sine-wave brightness modulation.
    pub pulse_amount: f32,
    pub pulse_speed: f32,
    /// Added to the rounding radius before the placement expansion.
    pub radius_offset: f32,
    pub antialias: f32,
    pub floating_only: bool,
    pub fullscreen: bool,
}

impl Default for BorderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            width: 2.0,
            placement: BorderPlacement::Outside,
            active_from: [46.0 / 255.0, 199.0 / 255.0, 1.0, 0.92],
            active_to: [92.0 / 255.0, 1.0, 210.0 / 255.0, 0.92],
            inactive_from: [20.0 / 255.0, 58.0 / 255.0, 72.0 / 255.0, 0.62],
            inactive_to: [8.0 / 255.0, 28.0 / 255.0, 38.0 / 255.0, 0.62],
            urgent_from: [94.0 / 255.0, 232.0 / 255.0, 1.0, 1.0],
            urgent_to: [71.0 / 255.0, 117.0 / 255.0, 1.0, 1.0],
            angle: 135.0,
            opacity: 1.0,
            inactive_opacity: 1.0,
            urgent_opacity: 1.0,
            animate: false,
            animate_focused: true,
            animate_inactive: true,
            animate_urgent: true,
            inactive_enabled: true,
            animation_speed: 28.0,
            pulse_amount: 0.0,
            pulse_speed: 1.0,
            radius_offset: 0.0,
            antialias: 1.0,
            floating_only: false,
            fullscreen: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BorderOverrides {
    pub enabled: Option<bool>,
    pub width: Option<f32>,
    pub placement: Option<BorderPlacement>,
    pub active_from: Option<[f32; 4]>,
    pub active_to: Option<[f32; 4]>,
    pub inactive_from: Option<[f32; 4]>,
    pub inactive_to: Option<[f32; 4]>,
    pub urgent_from: Option<[f32; 4]>,
    pub urgent_to: Option<[f32; 4]>,
    pub angle: Option<f32>,
    pub opacity: Option<f32>,
    pub inactive_opacity: Option<f32>,
    pub urgent_opacity: Option<f32>,
    pub animate: Option<bool>,
    pub animate_focused: Option<bool>,
    pub animate_inactive: Option<bool>,
    pub animate_urgent: Option<bool>,
    pub inactive_enabled: Option<bool>,
    pub animation_speed: Option<f32>,
    pub pulse_amount: Option<f32>,
    pub pulse_speed: Option<f32>,
    pub radius_offset: Option<f32>,
    pub antialias: Option<f32>,
    pub floating_only: Option<bool>,
    pub fullscreen: Option<bool>,
}

impl BorderOverrides {
    pub fn merge_over(&self, other: &Self) -> Self {
        Self {
            enabled: other.enabled.or(self.enabled),
            width: other.width.or(self.width),
            placement: other.placement.or(self.placement),
            active_from: other.active_from.or(self.active_from),
            active_to: other.active_to.or(self.active_to),
            inactive_from: other.inactive_from.or(self.inactive_from),
            inactive_to: other.inactive_to.or(self.inactive_to),
            urgent_from: other.urgent_from.or(self.urgent_from),
            urgent_to: other.urgent_to.or(self.urgent_to),
            angle: other.angle.or(self.angle),
            opacity: other.opacity.or(self.opacity),
            inactive_opacity: other.inactive_opacity.or(self.inactive_opacity),
            urgent_opacity: other.urgent_opacity.or(self.urgent_opacity),
            animate: other.animate.or(self.animate),
            animate_focused: other.animate_focused.or(self.animate_focused),
            animate_inactive: other.animate_inactive.or(self.animate_inactive),
            animate_urgent: other.animate_urgent.or(self.animate_urgent),
            inactive_enabled: other.inactive_enabled.or(self.inactive_enabled),
            animation_speed: other.animation_speed.or(self.animation_speed),
            pulse_amount: other.pulse_amount.or(self.pulse_amount),
            pulse_speed: other.pulse_speed.or(self.pulse_speed),
            radius_offset: other.radius_offset.or(self.radius_offset),
            antialias: other.antialias.or(self.antialias),
            floating_only: other.floating_only.or(self.floating_only),
            fullscreen: other.fullscreen.or(self.fullscreen),
        }
    }

    pub fn apply_to(&self, base: &BorderConfig) -> BorderConfig {
        BorderConfig {
            enabled: self.enabled.unwrap_or(base.enabled),
            width: self.width.unwrap_or(base.width),
            placement: self.placement.unwrap_or(base.placement),
            active_from: self.active_from.unwrap_or(base.active_from),
            active_to: self.active_to.unwrap_or(base.active_to),
            inactive_from: self.inactive_from.unwrap_or(base.inactive_from),
            inactive_to: self.inactive_to.unwrap_or(base.inactive_to),
            urgent_from: self.urgent_from.unwrap_or(base.urgent_from),
            urgent_to: self.urgent_to.unwrap_or(base.urgent_to),
            angle: self.angle.unwrap_or(base.angle),
            opacity: self.opacity.unwrap_or(base.opacity),
            inactive_opacity: self.inactive_opacity.unwrap_or(base.inactive_opacity),
            urgent_opacity: self.urgent_opacity.unwrap_or(base.urgent_opacity),
            animate: self.animate.unwrap_or(base.animate),
            animate_focused: self.animate_focused.unwrap_or(base.animate_focused),
            animate_inactive: self.animate_inactive.unwrap_or(base.animate_inactive),
            animate_urgent: self.animate_urgent.unwrap_or(base.animate_urgent),
            inactive_enabled: self.inactive_enabled.unwrap_or(base.inactive_enabled),
            animation_speed: self.animation_speed.unwrap_or(base.animation_speed),
            pulse_amount: self.pulse_amount.unwrap_or(base.pulse_amount),
            pulse_speed: self.pulse_speed.unwrap_or(base.pulse_speed),
            radius_offset: self.radius_offset.unwrap_or(base.radius_offset),
            antialias: self.antialias.unwrap_or(base.antialias),
            floating_only: self.floating_only.unwrap_or(base.floating_only),
            fullscreen: self.fullscreen.unwrap_or(base.fullscreen),
        }
    }
}

/// Optional manual override for compositor-owned popup chrome (the config
/// warning panel and toast). Every field defaults to `None` -- "auto":
/// `UiTheme::from_config` already derives a full theme for this UI (panel
/// tint, accent gradient, readable text, radius) from `[border]`/
/// `[rounding]`, so first-party UI matches whatever the user has already
/// configured for window decoration without a second palette to keep in
/// sync. Set a field here only to pin that one piece away from the theme;
/// everything else keeps auto-following it.
#[derive(Debug, Clone, Copy, Default)]
pub struct PopupConfig {
    /// Pixels. Auto matches `border.width`.
    pub border_width: Option<f32>,
    /// Auto matches the active/urgent accent gradient window borders use.
    pub border_color: Option<[f32; 4]>,
    /// Pixels. Auto matches the average `[rounding]` radius.
    pub radius: Option<f32>,
}

/// Which built-in shape a ripple draws. Multiple shapes can stack
/// concurrently (the `shapes = ring square` config syntax), each
/// producing its own per-frame render element sharing the same center,
/// size, and alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RippleShape {
    /// Thin annulus expanding from the center. The classic droplet ring.
    #[default]
    Ring,
    /// Square outline (Chebyshev-distance falloff) -- the same bounding
    /// square, drawn as four edge segments instead of a circle.
    Square,
    /// Ring plus a small filled disc at the center that fades faster
    /// than the ring, mimicking the impact crater a real droplet leaves
    /// for a moment after the ring has begun to radiate.
    Droplet,
    /// Plus-sign / cross. Two perpendicular bars meeting at the center,
    /// each fading at the bounding-square edge.
    Cross,
}

/// Polished analytical ripple appearances. `Legacy` preserves the original
/// independently-stackable geometric shapes; the other presets are one
/// fixed-cost shader element regardless of their internal visual detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RipplePreset {
    /// Layered concentric rings, a soft impact crater, and an aqua glow.
    #[default]
    WaterDrop,
    /// Organic oscillating membrane. `jiggle` and `giggle` are aliases.
    Jelly,
    /// Double translucent membrane with a moving glossy highlight.
    Bubble,
    /// Lobed impact crown with analytical spray peaks.
    Splash,
    /// Several offset wave bands flowing through each other.
    Tide,
    /// Original `ring`, `square`, `droplet`, and `cross` shape renderer.
    Legacy,
}

/// A ripple appearance selector can name a built-in shader style or a
/// reusable `ripple_preset <name> { }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipplePresetSelection {
    BuiltIn(RipplePreset),
    Named(String),
}

/// How the final ripple radius is derived from the triggering window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RippleSizeMode {
    /// Use `peak_radius` directly in logical pixels.
    #[default]
    Fixed,
    /// Half the window diagonal; scale 1.0 reaches its corners.
    Window,
    Width,
    Height,
    MinDimension,
    MaxDimension,
}

/// Which Wayland/WM event causes a ripple. Multiple triggers can be
/// enabled at once (the `triggers = map focus` config syntax); an
/// empty `triggers` list means "use the global default triggers."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum RippleTrigger {
    /// A window's first real buffer mapping (the moment it appears on
    /// screen). The default trigger if none are listed.
    #[default]
    Map,
    /// Keyboard focus moving from one window to another.
    Focus,
    /// An xdg-protocol urgent hint (a window wanting attention without
    /// stealing focus, e.g. an IM reply arriving).
    Urgent,
}

/// Where a ripple's center sits relative to the trigger window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RippleAnchor {
    /// The trigger window's geometric center. The default.
    #[default]
    Center,
    /// The current pointer position when the ripple is spawned -- a
    /// window-map ripple then appears where the click that spawned it
    /// happened (or wherever the cursor was on focus change).
    Cursor,
    Top,
    Bottom,
    Left,
    Right,
    /// Point on the window perimeter closest to the current pointer.
    NearestEdge,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Z-order layer a ripple renders in. Picks where in each backend's
/// render-element chain the ripple elements get inserted -- the chains
/// are front-to-back, index 0 topmost (this codebase's standing
/// convention, see AGENT.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RippleLayer {
    /// Above all windows, below chrome (toast/overview/picker/tab-strip).
    /// The default; reads as a window-level effect.
    #[default]
    AboveWindows,
    /// Below all windows, just above the wallpaper. Reads as a
    /// background-level effect rather than an interaction cue.
    BelowWindows,
    /// Above everything, including chrome. Use when a ripple should
    /// never be occluded by a bar or picker.
    AboveAll,
    /// Below everything, including the wallpaper. Rarely useful but
    /// symmetric with `AboveAll`.
    BelowAll,
}

/// Easing used by lifecycle and layout-motion animations. Cubic Bézier
/// stores CSS-compatible `(x1, y1, x2, y2)` control points; x is solved
/// numerically so the curve remains a function of elapsed time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowAnimationCurve {
    Linear,
    QuadOut,
    CubicOut,
    ExpOut,
    CubicInOut,
    CubicBezier([f32; 4]),
}

/// Extra trajectory layered over the normal eased start-to-end path.
/// `Tide` makes one broad sideways swell; `Wave` makes a configurable
/// decaying oscillation. Both return exactly to the logical target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAnimationEffect {
    Glide,
    Tide,
    Wave,
}

/// Lifecycle travel source. `NearestEdge` matches Hyprland's unforced
/// `slide`: choose the shortest midpoint-to-output-edge distance, then
/// place the window just beyond that edge for open/close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAnimationOrigin {
    Offset,
    NearestEdge,
    Top,
    Right,
    Bottom,
    Left,
}

/// One visual transition. `offset` is the lifecycle travel distance:
/// opening starts there and settles to zero, closing starts at zero and
/// travels there. Movement derives its offset from old/new layout geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowAnimationConfig {
    pub enabled: bool,
    /// Interpolate the outer window size during layout movement. Lifecycle
    /// transitions currently use position/opacity only.
    pub animate_size: bool,
    pub duration_ms: u32,
    pub curve: WindowAnimationCurve,
    /// Separate alpha clock/easing. `None` follows `duration_ms`/`curve`.
    pub opacity_duration_ms: Option<u32>,
    pub opacity_curve: Option<WindowAnimationCurve>,
    pub offset: (i32, i32),
    pub from_opacity: f32,
    pub to_opacity: f32,
    pub origin: WindowAnimationOrigin,
    pub effect: WindowAnimationEffect,
    pub wave_amplitude: f32,
    pub wave_cycles: f32,
    pub wave_decay: f32,
}

/// Product-wide window animation controls. A single slowdown multiplier is
/// useful for visual tuning and accessibility; values below one speed up.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowAnimationsConfig {
    pub enabled: bool,
    pub slowdown: f32,
    pub open: WindowAnimationConfig,
    pub close: WindowAnimationConfig,
    pub movement: WindowAnimationConfig,
}

impl Default for WindowAnimationsConfig {
    fn default() -> Self {
        Self::tide()
    }
}

impl WindowAnimationsConfig {
    pub fn hypr_smooth() -> Self {
        Self {
            enabled: true,
            slowdown: 1.0,
            open: WindowAnimationConfig {
                enabled: true,
                animate_size: false,
                duration_ms: 300,
                curve: WindowAnimationCurve::CubicBezier([0.05, 0.9, 0.1, 1.1]),
                opacity_duration_ms: Some(400),
                opacity_curve: Some(WindowAnimationCurve::CubicBezier([0.65, 0.05, 0.36, 1.0])),
                offset: (0, 42),
                from_opacity: 0.0,
                to_opacity: 1.0,
                origin: WindowAnimationOrigin::NearestEdge,
                effect: WindowAnimationEffect::Glide,
                wave_amplitude: 0.0,
                wave_cycles: 1.0,
                wave_decay: 1.5,
            },
            close: WindowAnimationConfig {
                enabled: true,
                animate_size: false,
                duration_ms: 300,
                curve: WindowAnimationCurve::CubicBezier([0.65, 0.05, 0.36, 1.0]),
                opacity_duration_ms: Some(400),
                opacity_curve: Some(WindowAnimationCurve::CubicBezier([0.65, 0.05, 0.36, 1.0])),
                offset: (0, 30),
                from_opacity: 1.0,
                to_opacity: 0.0,
                origin: WindowAnimationOrigin::NearestEdge,
                effect: WindowAnimationEffect::Glide,
                wave_amplitude: 0.0,
                wave_cycles: 1.0,
                wave_decay: 1.5,
            },
            movement: WindowAnimationConfig {
                enabled: true,
                animate_size: true,
                duration_ms: 400,
                curve: WindowAnimationCurve::CubicBezier([0.65, 0.05, 0.36, 1.0]),
                opacity_duration_ms: None,
                opacity_curve: None,
                offset: (0, 0),
                from_opacity: 1.0,
                to_opacity: 1.0,
                origin: WindowAnimationOrigin::Offset,
                effect: WindowAnimationEffect::Glide,
                wave_amplitude: 0.0,
                wave_cycles: 1.0,
                wave_decay: 1.5,
            },
        }
    }

    /// TideWM's calm default: fast exponential settling with one broad,
    /// low-amplitude sideways swell. It reads as water without making
    /// ordinary window management feel delayed.
    pub fn tide() -> Self {
        Self {
            enabled: true,
            slowdown: 1.0,
            open: WindowAnimationConfig {
                enabled: true,
                animate_size: false,
                duration_ms: 190,
                curve: WindowAnimationCurve::CubicBezier([0.16, 1.0, 0.3, 1.0]),
                opacity_duration_ms: None,
                opacity_curve: None,
                offset: (0, 24),
                from_opacity: 0.28,
                to_opacity: 1.0,
                origin: WindowAnimationOrigin::Offset,
                effect: WindowAnimationEffect::Tide,
                wave_amplitude: 4.0,
                wave_cycles: 0.5,
                wave_decay: 2.2,
            },
            close: WindowAnimationConfig {
                enabled: true,
                animate_size: false,
                duration_ms: 160,
                curve: WindowAnimationCurve::CubicOut,
                opacity_duration_ms: None,
                opacity_curve: None,
                offset: (0, 18),
                from_opacity: 1.0,
                to_opacity: 0.0,
                origin: WindowAnimationOrigin::Offset,
                effect: WindowAnimationEffect::Tide,
                wave_amplitude: 2.5,
                wave_cycles: 0.5,
                wave_decay: 2.0,
            },
            movement: WindowAnimationConfig {
                enabled: true,
                animate_size: true,
                duration_ms: 190,
                curve: WindowAnimationCurve::CubicBezier([0.16, 1.0, 0.3, 1.0]),
                opacity_duration_ms: None,
                opacity_curve: None,
                offset: (0, 0),
                from_opacity: 1.0,
                to_opacity: 1.0,
                origin: WindowAnimationOrigin::Offset,
                effect: WindowAnimationEffect::Tide,
                wave_amplitude: 1.25,
                wave_cycles: 0.5,
                wave_decay: 2.4,
            },
        }
    }

    /// More visible water motion for users who want the animation itself
    /// to be part of the desktop's personality.
    pub fn wave() -> Self {
        let mut preset = Self::tide();
        preset.open.duration_ms = 280;
        preset.open.offset = (0, 34);
        preset.open.effect = WindowAnimationEffect::Wave;
        preset.open.wave_amplitude = 15.0;
        preset.open.wave_cycles = 1.15;
        preset.open.wave_decay = 1.35;
        preset.close.duration_ms = 230;
        preset.close.curve = WindowAnimationCurve::CubicOut;
        preset.close.offset = (0, 28);
        preset.close.effect = WindowAnimationEffect::Wave;
        preset.close.wave_amplitude = 12.0;
        preset.close.wave_cycles = 1.0;
        preset.close.wave_decay = 1.2;
        preset.movement.duration_ms = 250;
        preset.movement.effect = WindowAnimationEffect::Wave;
        preset.movement.wave_amplitude = 6.0;
        preset.movement.wave_cycles = 0.85;
        preset
    }

    /// A short, sharper breaker: strong motion but less wall-clock delay.
    pub fn riptide() -> Self {
        let mut preset = Self::wave();
        preset.open.duration_ms = 155;
        preset.open.offset = (0, 38);
        preset.open.wave_amplitude = 9.0;
        preset.open.wave_cycles = 0.85;
        preset.close.duration_ms = 135;
        preset.close.offset = (0, 30);
        preset.close.wave_amplitude = 7.0;
        preset.close.wave_cycles = 0.8;
        preset.movement.duration_ms = 165;
        preset.movement.wave_amplitude = 3.0;
        preset.movement.wave_cycles = 0.7;
        preset
    }
}

/// Easing function for the ripple's radius/alpha progression. Picked
/// once at spawn time and applied to both the radius and the alpha
/// curve, so changing this single knob reshapes the whole feel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RippleEase {
    /// Constant velocity. Visually mechanical, not water-like -- mostly
    /// useful as a baseline for comparison when tuning other shapes.
    Linear,
    /// Fast start, decelerating toward peak radius. Default; matches the
    /// physical-feel of a droplet impact's wavefront radiating outward
    /// against increasing circumference.
    #[default]
    CubicOut,
    /// Slow start and slow end, fast through the middle.
    CubicInOut,
    /// Slightly softer than `CubicOut`.
    QuadOut,
    /// Very fast start, near-instant deceleration. Aggressive, can read
    /// as "snappy" rather than "watery" -- included because it suits
    /// non-ring shapes (a cross or square outline) better than the
    /// softer water easing does.
    ExpOut,
}

/// Tuning for the captured directional workspace wipe. Niri's current
/// per-animation shape informed the `enabled` + duration + curve split;
/// TideWM adds wave-specific geometry knobs because its transition is a
/// custom shader rather than a camera translation.
#[derive(Debug, Clone)]
pub struct WorkspaceTransitionConfig {
    pub enabled: bool,
    /// `Water` floods the output with animated water, foam, and spray;
    /// `Glow` retains the original colored boundary wipe.
    pub style: WorkspaceTransitionStyle,
    pub duration_ms: u32,
    /// Multiplier over `duration_ms`: 2.0 is twice as fast, 0.5 half.
    pub speed: f32,
    pub curve: RippleEase,
    /// Automatic follows workspace-number direction; fixed modes make
    /// every switch travel the same way.
    pub direction: WorkspaceTransitionDirectionMode,
    /// Captures both desktops and slides them together under the wave.
    /// Off by default so the second full-output texture is opt-in.
    pub workspace_motion: bool,
    /// Delay after the wave begins before desktop motion starts.
    pub workspace_motion_delay_ms: u32,
    /// Horizontal displacement of the wipe boundary, in physical pixels.
    pub wave_amplitude: f32,
    /// Number of sine cycles from the top of the output to the bottom.
    pub wave_frequency: f32,
    /// Half-width of the soft cross-fade boundary, in physical pixels.
    pub edge_width: f32,
    /// Main water color, or the colored core tint in `Glow` style.
    pub color: [f32; 3],
    pub wave_size: f32,
    pub wave_alpha: f32,
    pub glow_size: f32,
    pub glow_alpha: f32,
    /// Water-style shading scale and off-screen travel margin, in physical
    /// pixels.
    pub water_depth: f32,
    pub water_alpha: f32,
    pub foam_color: [f32; 3],
    pub foam_size: f32,
    pub foam_alpha: f32,
    /// Density/opacity multiplier for procedural droplets ahead of the crest.
    pub spray_amount: f32,
    /// Strength of secondary wave harmonics and animated body streaks.
    pub turbulence: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceTransitionStyle {
    #[default]
    Water,
    Glow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceTransitionDirectionMode {
    #[default]
    Auto,
    LeftToRight,
    RightToLeft,
}

impl Default for WorkspaceTransitionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            style: WorkspaceTransitionStyle::Water,
            duration_ms: 520,
            speed: 1.0,
            curve: RippleEase::CubicInOut,
            direction: WorkspaceTransitionDirectionMode::Auto,
            workspace_motion: false,
            workspace_motion_delay_ms: 150,
            wave_amplitude: 34.0,
            wave_frequency: 3.0,
            edge_width: 18.0,
            color: [142.0 / 255.0, 221.0 / 255.0, 1.0],
            wave_size: 10.0,
            wave_alpha: 0.9,
            glow_size: 46.0,
            glow_alpha: 0.25,
            water_depth: 260.0,
            water_alpha: 0.88,
            foam_color: [232.0 / 255.0, 252.0 / 255.0, 1.0],
            foam_size: 18.0,
            foam_alpha: 0.95,
            spray_amount: 0.7,
            turbulence: 0.7,
        }
    }
}

/// Automatic attention-distance depth for mapped windows. A window stays at
/// the surface until `sink_after_ms` elapses without focus/input, then moves
/// down one visual tier every `tier_interval_ms`. Focusing or typing into it
/// returns it to tier zero immediately.
#[derive(Debug, Clone)]
pub struct DepthConfig {
    pub enabled: bool,
    pub sink_after_ms: u32,
    pub tier_interval_ms: u32,
    /// Bounded visual depth. Tier one keeps live content; tier two and below
    /// use the schematic title-card representation.
    pub max_tier: u8,
    /// Live-content opacity at tier one before the cool overlay is applied.
    pub tier_one_alpha: f32,
    pub cool_color: [f32; 3],
    pub cool_alpha: f32,
    pub schematic_color: [f32; 3],
    pub schematic_alpha: f32,
    pub border_color: [f32; 3],
    pub urgent_color: [f32; 3],
    pub urgent_alpha: f32,
}

impl Default for DepthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sink_after_ms: 30_000,
            tier_interval_ms: 30_000,
            max_tier: 2,
            tier_one_alpha: 0.78,
            cool_color: [45.0 / 255.0, 112.0 / 255.0, 150.0 / 255.0],
            cool_alpha: 0.24,
            schematic_color: [16.0 / 255.0, 35.0 / 255.0, 48.0 / 255.0],
            schematic_alpha: 0.9,
            border_color: [82.0 / 255.0, 166.0 / 255.0, 198.0 / 255.0],
            urgent_color: [118.0 / 255.0, 241.0 / 255.0, 1.0],
            urgent_alpha: 0.95,
        }
    }
}

/// Explicit structural depth for the Classic spatial engine. This stays a
/// separate switch from [`DepthConfig`]: users may want visual cooling with
/// no parked windows, a deck with plain rendering, both, or neither.
#[derive(Debug, Clone)]
pub struct ClassicDepthConfig {
    pub enabled: bool,
    pub animation: bool,
    pub animation_duration_ms: u32,
    pub wave_color: [f32; 3],
    pub wave_alpha: f32,
}

impl Default for ClassicDepthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            animation: true,
            animation_duration_ms: 420,
            wave_color: [62.0 / 255.0, 196.0 / 255.0, 224.0 / 255.0],
            wave_alpha: 0.72,
        }
    }
}

/// A complete ripple configuration surface: every knob a user can set.
/// Sparse copies live in `Config::ripple`, named presets, and
/// each `WindowRule::ripple` with only the fields the user explicitly
/// set. `Config::resolve_ripple_config` layers them over `system_default`.
#[derive(Debug, Clone, Default)]
pub struct RippleConfig {
    /// Master enable. `None` in a per-rule config means "use the global
    /// value"; `Some(false)` is how a rule disables ripples for a
    /// specific app.
    pub enabled: Option<bool>,
    /// Base polished appearance. Trigger-specific values below override it.
    pub preset: Option<RipplePresetSelection>,
    pub map_preset: Option<RipplePresetSelection>,
    pub focus_preset: Option<RipplePresetSelection>,
    pub urgent_preset: Option<RipplePresetSelection>,
    /// Whether automatic focus during a window's first map should also emit
    /// the focus preset. Off by default so the map preset is the transaction's
    /// single visual cue; opt in when deliberate effect stacking is desired.
    pub focus_on_map: Option<bool>,
    /// Repeat the urgent ripple until the window is focused or its urgent
    /// hint clears, instead of firing once when the hint is set. Each pulse
    /// is identical (no decay). Only meaningful for the urgent trigger.
    pub urgent_repeat: Option<bool>,
    /// Milliseconds between urgent-repeat pulses. Clamped to a 100ms floor
    /// at resolve time so a pathological value can't turn the pulse into a
    /// per-frame spawn loop.
    pub urgent_repeat_interval_ms: Option<u32>,
    /// Shape(s) to draw. Empty in a per-rule config means "inherit the
    /// global shapes." Used by `preset = legacy`; assigning `shapes`
    /// automatically selects that preset for compatibility.
    pub shapes: Vec<RippleShape>,
    /// Tint as linear RGB in `[0.0, 1.0]`. Parsed from CSS-style hex
    /// (`#RRGGBB` / `#RRGGBBAA`, the alpha silently ignored for now --
    /// transparency has its own `peak_alpha` knob).
    pub color: Option<[f32; 3]>,
    /// Second gradient/highlight color used by polished presets.
    pub secondary_color: Option<[f32; 3]>,
    pub peak_radius: Option<f32>,
    pub size_mode: Option<RippleSizeMode>,
    /// Multiplier applied to a window-derived radius.
    pub size_scale: Option<f32>,
    pub min_radius: Option<f32>,
    pub max_radius: Option<f32>,
    /// Ring/outline half-width in logical pixels.
    pub thickness: Option<f32>,
    pub duration_ms: Option<u32>,
    /// Maximum alpha reached at spawn, in `[0.0, 1.0]`. Separate from
    /// `color`'s alpha channel so a single hex color can be reused
    /// across ripple intensities.
    pub peak_alpha: Option<f32>,
    /// Soft halo intensity around preset geometry.
    pub glow: Option<f32>,
    /// Organic displacement strength. Most visible on `jelly` and `tide`.
    pub wobble: Option<f32>,
    /// Amount of inner rings, spray, highlights, or secondary bands.
    pub detail: Option<f32>,
    pub ease: Option<RippleEase>,
    pub anchor: Option<RippleAnchor>,
    /// Position along a side anchor, `0` at left/top and `1` at right/bottom.
    pub edge_position: Option<f32>,
    /// Signed outward distance from a side/nearest-edge anchor.
    pub edge_offset: Option<f32>,
    /// Logical-pixel offset from the anchor point, `<dx>x<dy>`. Lets a
    /// rule shift the ripple without redefining the anchor itself.
    pub offset: Option<(i32, i32)>,
    pub layer: Option<RippleLayer>,
    /// Which events trigger ripples in this scope. Empty in a per-rule
    /// config means "inherit the global triggers"; empty in the global
    /// config means `[Map]` (the conservative default, not the whole
    /// event surface -- ripples on every focus change can feel busy).
    pub triggers: Vec<RippleTrigger>,
}

impl RippleConfig {
    /// The values used when the user has no `ripple { }` block at all.
    /// Every field `Some`, so per-rule merges always have a real value
    /// to fall back to.
    pub fn system_default() -> Self {
        Self {
            enabled: Some(true),
            preset: Some(RipplePresetSelection::BuiltIn(RipplePreset::WaterDrop)),
            map_preset: None,
            focus_preset: None,
            urgent_preset: None,
            focus_on_map: Some(false),
            urgent_repeat: Some(true),
            urgent_repeat_interval_ms: Some(1500),
            shapes: vec![RippleShape::Ring],
            color: Some([0.55, 0.85, 1.0]),
            secondary_color: Some([0.91, 0.98, 1.0]),
            peak_radius: Some(220.0),
            size_mode: Some(RippleSizeMode::Fixed),
            size_scale: Some(1.0),
            min_radius: Some(24.0),
            max_radius: Some(2048.0),
            thickness: Some(8.0),
            duration_ms: Some(650),
            peak_alpha: Some(0.88),
            glow: Some(0.55),
            wobble: Some(0.7),
            detail: Some(0.8),
            ease: Some(RippleEase::CubicOut),
            anchor: Some(RippleAnchor::Center),
            edge_position: Some(0.5),
            edge_offset: Some(0.0),
            offset: Some((0, 0)),
            layer: Some(RippleLayer::AboveWindows),
            triggers: vec![RippleTrigger::Map],
        }
    }

    /// Merge `other` (per-rule, sparse) onto `self` (global, fully
    /// populated). `Some` fields in `other` win; `None` inherits. Vec
    /// fields (`shapes`, `triggers`) replace wholesale if non-empty,
    /// else inherit -- consistent with how the rest of the config
    /// treats "explicit list vs unset."
    pub fn merge_over(&self, other: &RippleConfig) -> RippleConfig {
        RippleConfig {
            enabled: other.enabled.or(self.enabled),
            preset: other.preset.clone().or_else(|| self.preset.clone()),
            map_preset: other.map_preset.clone().or_else(|| self.map_preset.clone()),
            focus_preset: other
                .focus_preset
                .clone()
                .or_else(|| self.focus_preset.clone()),
            urgent_preset: other
                .urgent_preset
                .clone()
                .or_else(|| self.urgent_preset.clone()),
            focus_on_map: other.focus_on_map.or(self.focus_on_map),
            urgent_repeat: other.urgent_repeat.or(self.urgent_repeat),
            urgent_repeat_interval_ms: other
                .urgent_repeat_interval_ms
                .or(self.urgent_repeat_interval_ms),
            shapes: if other.shapes.is_empty() {
                self.shapes.clone()
            } else {
                other.shapes.clone()
            },
            color: other.color.or(self.color),
            secondary_color: other.secondary_color.or(self.secondary_color),
            peak_radius: other.peak_radius.or(self.peak_radius),
            size_mode: other.size_mode.or(self.size_mode),
            size_scale: other.size_scale.or(self.size_scale),
            min_radius: other.min_radius.or(self.min_radius),
            max_radius: other.max_radius.or(self.max_radius),
            thickness: other.thickness.or(self.thickness),
            duration_ms: other.duration_ms.or(self.duration_ms),
            peak_alpha: other.peak_alpha.or(self.peak_alpha),
            glow: other.glow.or(self.glow),
            wobble: other.wobble.or(self.wobble),
            detail: other.detail.or(self.detail),
            ease: other.ease.or(self.ease),
            anchor: other.anchor.or(self.anchor),
            edge_position: other.edge_position.or(self.edge_position),
            edge_offset: other.edge_offset.or(self.edge_offset),
            offset: other.offset.or(self.offset),
            layer: other.layer.or(self.layer),
            triggers: if other.triggers.is_empty() {
                self.triggers.clone()
            } else {
                other.triggers.clone()
            },
        }
    }

    /// Whether this config wants a ripple on the given trigger.
    pub fn fires_on(&self, trigger: RippleTrigger) -> bool {
        let active = if self.triggers.is_empty() {
            &[RippleTrigger::Map][..]
        } else {
            &self.triggers[..]
        };
        active.contains(&trigger)
    }

    /// Resolves a trigger-specific appearance over the base preset.
    pub fn preset_for(&self, trigger: RippleTrigger) -> Option<&RipplePresetSelection> {
        let specific = match trigger {
            RippleTrigger::Map => self.map_preset.as_ref(),
            RippleTrigger::Focus => self.focus_preset.as_ref(),
            RippleTrigger::Urgent => self.urgent_preset.as_ref(),
        };
        specific.or(self.preset.as_ref())
    }

    /// Built-in style after named-preset resolution. An unresolved name is
    /// never fatal and falls back to the product default.
    pub fn built_in_preset(&self) -> RipplePreset {
        match self.preset.as_ref() {
            Some(RipplePresetSelection::BuiltIn(preset)) => *preset,
            _ => RipplePreset::WaterDrop,
        }
    }

    /// Resolves the configured sizing mode against one triggering window.
    pub fn radius_for_window(&self, width: f32, height: f32) -> f32 {
        let width = width.max(0.0);
        let height = height.max(0.0);
        let scale = self.size_scale.unwrap_or(1.0);
        let radius = match self.size_mode.unwrap_or(RippleSizeMode::Fixed) {
            RippleSizeMode::Fixed => self.peak_radius.unwrap_or(220.0),
            RippleSizeMode::Window => width.hypot(height) * 0.5 * scale,
            RippleSizeMode::Width => width * 0.5 * scale,
            RippleSizeMode::Height => height * 0.5 * scale,
            RippleSizeMode::MinDimension => width.min(height) * 0.5 * scale,
            RippleSizeMode::MaxDimension => width.max(height) * 0.5 * scale,
        };
        let min = self.min_radius.unwrap_or(24.0).max(0.0);
        let max = self.max_radius.unwrap_or(2048.0).max(1.0);
        radius.clamp(min.min(max), min.max(max))
    }
}

impl WindowRule {
    /// A rule with no identifying criterion at all never matches anything,
    /// rather than silently matching every window -- a blank rule is far
    /// more likely to be a config mistake than an intentional "match all".
    pub(crate) fn matches(
        &self,
        app_id: Option<&str>,
        title: Option<&str>,
        pid: Option<i32>,
        is_xwayland: bool,
    ) -> bool {
        if self.app_id.is_none()
            && self.title.is_none()
            && self.app_id_regex.is_none()
            && self.title_regex.is_none()
            && self.pid.is_none()
            && self.is_xwayland.is_none()
        {
            return false;
        }
        if let Some(want) = &self.app_id {
            let Some(app_id) = app_id else { return false };
            // Case-insensitive: app_ids are conventionally lowercase but
            // real clients vary (and xwayland-satellite synthesizes them
            // from WM_CLASS, which is free-form). A rule written as `mpv`
            // silently missing a client reporting `MPV` is a footgun.
            if !app_id.eq_ignore_ascii_case(want) {
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
        if let Some(want) = self.pid {
            if pid != Some(want) {
                return false;
            }
        }
        if let Some(want) = self.is_xwayland {
            if want != is_xwayland {
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

/// Reads `path` as config, resolving `include` statements first (see
/// `waves::resolve`), then lowers the merged entry list into a
/// `RawConfig`. The single entry point both `Config::load` and
/// `Config::reload` use so they stay consistent about how includes are
/// resolved.
///
/// The Wave engine is the only grammar. A parse error reports the Wave
/// parser's message with file/line detail.
#[cfg(test)]
fn load_raw_config(path: &Path) -> Result<(RawConfig, Vec<String>, Vec<waves::Entry>), String> {
    let lua = mlua::Lua::new_with(
        mlua::StdLib::MATH | mlua::StdLib::STRING | mlua::StdLib::TABLE,
        mlua::LuaOptions::default(),
    )
    .map_err(|e| {
        format!(
            "in file {}: failed to create Lua state: {e}",
            path.display()
        )
    })?;
    load_raw_config_in(&lua, &wave::TideInfo::default(), path)
}

/// [`load_raw_config`] on a caller-owned session Lua with live-compositor
/// facts: the runtime path, so hardware conditionals
/// (`if tide.backend == "udev" then ...`) see the real machine and the
/// resulting globals persist for `tidectl eval` and `on` handlers.
fn load_raw_config_in(
    lua: &mlua::Lua,
    tide: &wave::TideInfo,
    path: &Path,
) -> Result<(RawConfig, Vec<String>, Vec<waves::Entry>), String> {
    let (entries, warnings) = wave::resolve_with_lua(lua, tide, path)?;

    let raw = lower_entries(&entries);
    Ok((raw, warnings, entries))
}

/// Lowers a fully-merged Waves entry list into a `RawConfig`, starting
/// from its defaults and overwriting whatever was actually present.
/// Unknown keys/blocks warn and are ignored rather than failing the whole
/// config -- same forgiving convention TOML loading always used (a typo
/// shouldn't take down a working session).
/// Parses the Wave engine's serialized list form (`["a", "b"]`) into
/// items; returns `None` for anything that is not a list.
fn parse_list_value(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    if !value.starts_with('[') || !value.ends_with(']') {
        return None;
    }
    let inner = &value[1..value.len() - 1];
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                items.push(current.trim().to_string());
                current.clear();
            }
            '\\' if in_quotes => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            _ => current.push(c),
        }
    }
    items.push(current.trim().to_string());
    let items: Vec<String> = items.into_iter().filter(|s| !s.is_empty()).collect();
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

/// Parses a duration value into milliseconds: a bare number (the legacy
/// `_ms` key spelling) or a unit-suffixed literal (`600ms`, `1.5s`,
/// `90m`) as the Wave engine's duration values serialize to. Returns
/// `None` for zero/negative or unparseable values.
fn parse_duration_ms(value: &str) -> Option<u32> {
    let value = value.trim();
    let (num, scale) = if let Some(num) = value.strip_suffix("ms") {
        (num, 1.0)
    } else if let Some(num) = value.strip_suffix('s') {
        (num, 1000.0)
    } else if let Some(num) = value.strip_suffix('m') {
        (num, 60_000.0)
    } else {
        (value, 1.0)
    };
    let n = num.trim().parse::<f64>().ok()?;
    let ms = (n * scale).round();
    u32::try_from(ms as i64).ok().filter(|ms| *ms > 0)
}

/// What changed between two merged entry lists (W3's reload diff).
/// Computed at the entry level, so it is grammar-agnostic: old and new
/// Wave configs produce the same `waves::Entry` shape.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ConfigDiff {
    /// Top-level keys whose value changed: (key, old, new). Added keys
    /// come through with an empty old, removed with an empty new.
    pub keys_changed: Vec<(String, String, String)>,
    pub binds_added: Vec<(String, String)>,
    pub binds_removed: Vec<(String, String)>,
    /// Combos whose action list changed: (combo, old actions joined, new).
    pub binds_changed: Vec<(String, String, String)>,
    /// Blocks whose bodies changed: (keyword, header).
    pub blocks_changed: Vec<(String, String)>,
}

impl ConfigDiff {
    pub fn is_empty(&self) -> bool {
        self.keys_changed.is_empty()
            && self.binds_added.is_empty()
            && self.binds_removed.is_empty()
            && self.binds_changed.is_empty()
            && self.blocks_changed.is_empty()
    }

    /// A one-line human summary for the reload log: "3 keys, binds
    /// +1 -1 ~2, blocks: border, input".
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.keys_changed.is_empty() {
            let keys: Vec<&str> = self
                .keys_changed
                .iter()
                .map(|(k, _, _)| k.as_str())
                .collect();
            parts.push(format!(
                "{} key{} changed ({})",
                keys.len(),
                if keys.len() == 1 { "" } else { "s" },
                keys.join(", ")
            ));
        }
        if !self.binds_added.is_empty()
            || !self.binds_removed.is_empty()
            || !self.binds_changed.is_empty()
        {
            parts.push(format!(
                "binds +{} -{} ~{}",
                self.binds_added.len(),
                self.binds_removed.len(),
                self.binds_changed.len()
            ));
        }
        if !self.blocks_changed.is_empty() {
            let keywords: Vec<&str> = self
                .blocks_changed
                .iter()
                .map(|(k, _)| k.as_str())
                .collect();
            parts.push(format!("blocks changed ({})", keywords.join(", ")));
        }
        if parts.is_empty() {
            "nothing changed".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Diffs two merged entry lists per section: top-level keys, binds, and
/// blocks. Variables and includes are deliberately skipped: they are
/// substitution/resolution machinery, not configuration.
pub(crate) fn diff_entries(old: &[waves::Entry], new: &[waves::Entry]) -> ConfigDiff {
    let old_keys = collect_assigns(old);
    let new_keys = collect_assigns(new);
    let mut keys_changed = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for key in old_keys.keys().chain(new_keys.keys()) {
        if !seen.insert(key) {
            continue; // present in both lists, already compared
        }
        let o = old_keys.get(key).cloned().unwrap_or_default();
        let n = new_keys.get(key).cloned().unwrap_or_default();
        if o != n {
            keys_changed.push((key.clone(), o, n));
        }
    }
    keys_changed.sort();

    let old_binds = collect_binds(old);
    let new_binds = collect_binds(new);
    let mut binds_added = Vec::new();
    let mut binds_removed = Vec::new();
    let mut binds_changed = Vec::new();
    for (combo, actions) in &new_binds {
        match old_binds.get(combo) {
            None => {
                for a in actions {
                    binds_added.push((combo.clone(), a.clone()));
                }
            }
            Some(old_actions) if old_actions != actions => {
                binds_changed.push((combo.clone(), old_actions.join(" "), actions.join(" ")));
            }
            Some(_) => {}
        }
    }
    for (combo, actions) in &old_binds {
        if !new_binds.contains_key(combo) {
            for a in actions {
                binds_removed.push((combo.clone(), a.clone()));
            }
        }
    }
    binds_added.sort();
    binds_removed.sort();
    binds_changed.sort();

    let old_blocks = collect_blocks(old);
    let new_blocks = collect_blocks(new);
    let mut blocks_changed: Vec<(String, String)> = Vec::new();
    let mut seen_blocks = std::collections::BTreeSet::new();
    for key in old_blocks.keys().chain(new_blocks.keys()) {
        if seen_blocks.insert(key) && old_blocks.get(key) != new_blocks.get(key) {
            blocks_changed.push(key.clone());
        }
    }
    blocks_changed.sort();

    ConfigDiff {
        keys_changed,
        binds_added,
        binds_removed,
        binds_changed,
        blocks_changed,
    }
}

fn collect_assigns(entries: &[waves::Entry]) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for entry in entries {
        if let waves::Entry::Assign(key, value) = entry {
            map.insert(key.clone(), value.clone()); // last write wins, as in lowering
        }
    }
    map
}

fn collect_binds(entries: &[waves::Entry]) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut map: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        if let waves::Entry::Bind(combo, action) = entry {
            map.entry(combo.clone()).or_default().push(action.clone());
        }
    }
    map
}

fn collect_blocks(
    entries: &[waves::Entry],
) -> std::collections::BTreeMap<(String, String), Vec<waves::Entry>> {
    let mut map: std::collections::BTreeMap<(String, String), Vec<waves::Entry>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        if let waves::Entry::Block(keyword, header, body) = entry {
            map.entry((keyword.clone(), header.clone()))
                .or_default()
                .push(waves::Entry::Block(
                    keyword.clone(),
                    header.clone(),
                    body.clone(),
                ));
        }
    }
    map
}

fn lower_entries(entries: &[waves::Entry]) -> RawConfig {
    let mut raw = RawConfig::default();
    // A parsed Waves file is authoritative. Defaults belong in the
    // generated default file and the explicit rescue layer, never merged
    // invisibly underneath a user's bindings.
    raw.keybinds.clear();
    raw.submaps.clear();
    for entry in entries {
        match entry {
            waves::Entry::VarDef(_, _) => {
                // Variables are resolved by the Wave engine at emit time;
                // the entry exists for the model and merge policy only.
            }
            waves::Entry::Bind(combo, action) => {
                raw.keybinds.insert(combo.clone(), action.clone());
            }
            // Already resolved away by `waves::resolve` before this ever runs.
            waves::Entry::Include(_) => {}
            // Handlers live as live Lua functions in the session state
            // (`_handlers`, registered by the `_on` environment
            // function); the entry exists for the model and diagnostics.
            waves::Entry::Handler(_, _) => {}
            waves::Entry::Assign(key, value) => apply_top_level_assign(&mut raw, key, value),
            waves::Entry::Block(keyword, header, body) => {
                apply_top_level_block(&mut raw, keyword, header, body)
            }
        }
    }
    raw
}

/// Is `key` a recognized top-level config key (see
/// `apply_top_level_assign`)? Used to tell a computation block like
/// `theme { }` (whose keys are unknown on purpose) apart from a typo'd
/// real block.
fn is_known_top_level_key(key: &str) -> bool {
    matches!(
        key,
        "terminal"
            | "engine"
            | "drag_modifier"
            | "welcome_hint"
            | "reload_toast"
            | "water_effects"
            | "viscosity"
            | "cursor_always_visible"
            | "cursor_hide_after"
            | "auto_back_and_forth"
            | "workspace_name"
            | "gaps"
            | "workspace_gaps"
            | "layout"
            | "master_side"
            | "split_bias"
            | "pseudo_tile_scale"
            | "spawn"
    )
}

fn apply_top_level_assign(raw: &mut RawConfig, key: &str, value: &str) {
    match key {
        "terminal" => raw.terminal = value.to_string(),
        "spatial_engine" | "engine" | "wm_mode" => raw.spatial_engine = value.to_string(),
        "pointer_modifier" | "mouse_modifier" | "drag_modifier" => {
            raw.pointer_modifier = value.to_string()
        }
        "welcome_hint" => set_bool(&mut raw.show_welcome_hint, key, value),
        "reload_toast" => set_bool(&mut raw.show_config_reload_toast, key, value),
        "water_effects" => set_bool(&mut raw.water_effects, key, value),
        "viscosity" => match parse_viscosity(value) {
            Some(value) => raw.viscosity = value,
            None => tracing::warn!(value, "Expected finite viscosity from 0.0 to 4.0, ignoring"),
        },
        "cursor_always_visible" => set_bool(&mut raw.cursor_always_visible, key, value),
        "cursor_hide_after" => {
            if let Some(ms) = parse_duration_ms(value) {
                raw.cursor_hide_after_ms = ms as i32;
            } else {
                tracing::warn!(
                    value,
                    "Expected a duration like 2s or 2000ms for cursor_hide_after, ignoring"
                );
            }
        }
        "auto_back_and_forth" => set_bool(&mut raw.workspace_auto_back_and_forth, key, value),
        "gaps" => set_i32(&mut raw.gaps, key, value),
        "layout" => raw.default_layout = value.to_string(),
        "master_side" => raw.master_orientation = value.to_string(),
        "split_bias" => raw.bsp_split_bias = value.to_string(),
        "workspace_name" => raw.workspace_names.push(value.to_string()),
        "workspace_gaps" => raw.workspace_gaps.push(value.to_string()),
        "pseudo_tile_scale" => set_f64(&mut raw.pseudo_tile_scale, key, value),
        // List-shaped, not scalar -- accumulates because `waves::merge_into`
        // already let every occurrence of this one key through instead of
        // deduping to the last (see `waves::assign_is_multi`).
        "spawn" => match parse_list_value(value) {
            Some(items) => raw.spawn_at_startup.extend(items),
            None => raw.spawn_at_startup.push(value.to_string()),
        },
        other => tracing::warn!(key = %other, value, "Unknown config key, ignoring"),
    }
}

fn apply_top_level_block(raw: &mut RawConfig, keyword: &str, header: &str, body: &[waves::Entry]) {
    match keyword {
        "input" => apply_input_block(&mut raw.input, body),
        "xwayland" => apply_xwayland_block(&mut raw.xwayland, body),
        "transition" => apply_workspace_transition_block(&mut raw.workspace_transition, body),
        "vessels" => apply_connected_vessels_block(&mut raw.connected_vessels, body),
        "sway" => apply_sway_block(&mut raw.sway, body),
        "physics" => apply_float_physics_block(&mut raw.float_physics, body),
        "swim" => apply_swim_block(&mut raw.swim, body),
        "compass" => apply_compass_block(&mut raw.compass, body),
        "minimap" => apply_minimap_block(&mut raw.minimap, body),
        "ocean" => apply_ocean_block(&mut raw.ocean, body),
        "animations" => apply_animations_block(&mut raw.animations, body),
        "depth" => apply_depth_block(&mut raw.depth, body),
        "depth_deck" => apply_classic_depth_block(&mut raw.classic_depth, body),
        "frost" => apply_frost_block(&mut raw.frost, body),
        "glass" => apply_water_glass_block(&mut raw.water_glass, body),
        "caustics" => apply_caustics_block(&mut raw.caustics, body),
        "shadow" => apply_shadow_block(&mut raw.shadow, body),
        "rounding" => apply_rounding_block(&mut raw.rounding, body),
        "border" => apply_border_block(&mut raw.border, body),
        "popup" => apply_popup_block(&mut raw.popup, body),
        "ripple" => apply_ripple_block(&mut raw.ripple, body),
        "ripple_preset" => {
            let name = header.trim().to_lowercase();
            if !valid_ripple_preset_name(&name) {
                tracing::warn!(
                    header,
                    "A `ripple_preset` block needs an alphanumeric, dash, or underscore name"
                );
                return;
            }
            let preset = raw.ripple_presets.entry(name).or_default();
            apply_ripple_block(preset, body);
        }
        "output" => raw.outputs.push(lower_output_block(header, body)),
        "rule" => raw.window_rules.push(lower_window_rule_block(body)),
        "layer_rule" => raw.layer_rules.push(lower_layer_rule_block(body)),
        "mode" => {
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
        other => {
            // A computation block (the Wave palette's `theme { }`) has no
            // config meaning: its values feed expressions through section
            // globals, so it is silently ignored. A block that tries to
            // set known keys is a real typo and still warns.
            let computation_only = body.iter().all(|entry| {
                matches!(entry, waves::Entry::Assign(key, _) if !is_known_top_level_key(key))
                    || matches!(entry, waves::Entry::Block(k, _, _) if k == other)
            });
            if !computation_only {
                tracing::warn!(keyword = %other, "Unknown config block, ignoring");
            }
        }
    }
}

fn apply_input_block(input: &mut InputConfig, body: &[waves::Entry]) {
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

fn apply_animations_block(cfg: &mut WindowAnimationsConfig, body: &[waves::Entry]) {
    // Presets establish a complete baseline first, regardless of where the
    // assignment appears in the block. Every explicit setting below then
    // remains an override, which is much less surprising while live-tuning.
    if let Some(preset) = body.iter().rev().find_map(|entry| match entry {
        waves::Entry::Assign(key, value) if key == "preset" => Some(value.as_str()),
        _ => None,
    }) {
        match preset
            .trim()
            .to_ascii_lowercase()
            .replace('_', "-")
            .as_str()
        {
            "tide" | "calm-tide" | "default" => *cfg = WindowAnimationsConfig::tide(),
            "wave" | "rolling-wave" => *cfg = WindowAnimationsConfig::wave(),
            "riptide" | "breaker" => *cfg = WindowAnimationsConfig::riptide(),
            "hypr" | "hyprland" | "hypr-smooth" => *cfg = WindowAnimationsConfig::hypr_smooth(),
            other => tracing::warn!(
                preset = %other,
                "Unknown animation preset; expected tide, wave, riptide, or hypr-smooth"
            ),
        }
    }

    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) => match key.as_str() {
                "preset" => {}
                "enabled" => match parse_bool(value) {
                    Some(value) => cfg.enabled = value,
                    None => tracing::warn!(
                        value,
                        "Expected `true` or `false` for animations.enabled, ignoring"
                    ),
                },
                "slowdown" | "speed_scale" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() && value > 0.0 => {
                        cfg.slowdown = value.clamp(0.1, 10.0)
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected animations.slowdown from 0.1 to 10, ignoring"
                    ),
                },
                other => tracing::warn!(
                    key = %other,
                    "Unknown assignment in `animations` block, ignoring"
                ),
            },
            waves::Entry::Block(keyword, header, child) => {
                if !header.trim().is_empty() {
                    tracing::warn!(header, "Animation sub-block headers are ignored");
                }
                let target = match keyword.as_str() {
                    "open" | "window_open" | "window-open" => &mut cfg.open,
                    "close" | "window_close" | "window-close" => &mut cfg.close,
                    "move" | "movement" | "window_movement" | "window-movement" => {
                        &mut cfg.movement
                    }
                    other => {
                        tracing::warn!(
                            block = %other,
                            "Unknown animation sub-block, ignoring"
                        );
                        continue;
                    }
                };
                apply_window_animation_block(target, child);
            }
            _ => tracing::warn!("Unexpected entry in `animations` block, ignoring"),
        }
    }
}

fn apply_window_animation_block(cfg: &mut WindowAnimationConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Animation sub-blocks contain assignments only, ignoring entry");
            continue;
        };
        match key.as_str() {
            "enabled" => match parse_bool(value) {
                Some(value) => cfg.enabled = value,
                None => tracing::warn!(
                    value,
                    "Expected `true` or `false` for animation enabled, ignoring"
                ),
            },
            "animate_size" | "animate-size" | "resize" | "size" => match parse_bool(value) {
                Some(value) => cfg.animate_size = value,
                None => tracing::warn!(
                    value,
                    "Expected `true` or `false` for animate_size, ignoring"
                ),
            },
            "duration" => match parse_duration_ms(value) {
                Some(value) if (1..=10_000).contains(&value) => cfg.duration_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected animation duration from 1 to 10000ms, ignoring"
                ),
            },
            "curve" | "ease" => match parse_window_animation_curve(value) {
                Some(value) => cfg.curve = value,
                None => tracing::warn!(
                    value,
                    "Expected a built-in easing or cubic-bezier(x1,y1,x2,y2), ignoring"
                ),
            },
            "opacity_duration" => match parse_duration_ms(value) {
                Some(value) if (1..=10_000).contains(&value) => {
                    cfg.opacity_duration_ms = Some(value)
                }
                _ => tracing::warn!(
                    value,
                    "Expected opacity animation duration from 1 to 10000ms, ignoring"
                ),
            },
            "opacity_curve" | "fade_curve" | "opacity_ease" => {
                match parse_window_animation_curve(value) {
                    Some(value) => cfg.opacity_curve = Some(value),
                    None => tracing::warn!(
                        value,
                        "Expected an opacity easing or cubic-bezier(x1,y1,x2,y2), ignoring"
                    ),
                }
            }
            "offset" | "travel" => match parse_position(value) {
                Some(value) => cfg.offset = value,
                None => tracing::warn!(value, "Expected animation offset as <x>x<y>, ignoring"),
            },
            "from_opacity" | "opacity_from" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.from_opacity = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected from_opacity from 0 to 1, ignoring"),
            },
            "to_opacity" | "opacity_to" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.to_opacity = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected to_opacity from 0 to 1, ignoring"),
            },
            "origin" | "slide_from" | "slide-from" | "direction" => {
                match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
                    "offset" | "fixed" => cfg.origin = WindowAnimationOrigin::Offset,
                    "nearest" | "nearest-edge" | "auto" => {
                        cfg.origin = WindowAnimationOrigin::NearestEdge
                    }
                    "top" => cfg.origin = WindowAnimationOrigin::Top,
                    "right" => cfg.origin = WindowAnimationOrigin::Right,
                    "bottom" => cfg.origin = WindowAnimationOrigin::Bottom,
                    "left" => cfg.origin = WindowAnimationOrigin::Left,
                    _ => tracing::warn!(
                        value,
                        "Expected animation origin offset, nearest-edge, top, right, bottom, or left, ignoring"
                    ),
                }
            }
            "effect" | "motion" => {
                match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
                    "glide" | "none" => cfg.effect = WindowAnimationEffect::Glide,
                    "tide" | "swell" => cfg.effect = WindowAnimationEffect::Tide,
                    "wave" | "rolling-wave" => cfg.effect = WindowAnimationEffect::Wave,
                    _ => tracing::warn!(
                        value,
                        "Expected animation effect glide, tide, or wave, ignoring"
                    ),
                }
            }
            "wave_amplitude" | "amplitude" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.wave_amplitude = value.clamp(0.0, 512.0),
                _ => tracing::warn!(
                    value,
                    "Expected wave_amplitude from 0 to 512 logical pixels, ignoring"
                ),
            },
            "wave_cycles" | "cycles" | "frequency" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.wave_cycles = value.clamp(0.0, 8.0),
                _ => tracing::warn!(value, "Expected wave_cycles from 0 to 8, ignoring"),
            },
            "wave_decay" | "decay" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.wave_decay = value.clamp(0.0, 8.0),
                _ => tracing::warn!(value, "Expected wave_decay from 0 to 8, ignoring"),
            },
            other => tracing::warn!(
                key = %other,
                "Unknown animation setting, ignoring"
            ),
        }
    }
}

fn parse_window_animation_curve(value: &str) -> Option<WindowAnimationCurve> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "linear" => return Some(WindowAnimationCurve::Linear),
        "quad-out" | "ease-out-quad" => return Some(WindowAnimationCurve::QuadOut),
        "cubic-out" | "ease-out-cubic" => return Some(WindowAnimationCurve::CubicOut),
        "exp-out" | "expo-out" | "ease-out-expo" => {
            return Some(WindowAnimationCurve::ExpOut);
        }
        "cubic-in-out" | "ease-in-out-cubic" => {
            return Some(WindowAnimationCurve::CubicInOut);
        }
        _ => {}
    }

    let inner = normalized
        .strip_prefix("cubic-bezier(")?
        .strip_suffix(')')?;
    let values: Vec<f32> = inner
        .split([',', ' '])
        .filter(|part| !part.is_empty())
        .map(str::parse::<f32>)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() != 4 || values.iter().any(|value| !value.is_finite()) {
        return None;
    }
    if !(0.0..=1.0).contains(&values[0]) || !(0.0..=1.0).contains(&values[2]) {
        return None;
    }
    Some(WindowAnimationCurve::CubicBezier([
        values[0], values[1], values[2], values[3],
    ]))
}

fn apply_workspace_transition_block(cfg: &mut WorkspaceTransitionConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `workspace_transition` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "style" => match value.as_str() {
                "water" => cfg.style = WorkspaceTransitionStyle::Water,
                "glow" => cfg.style = WorkspaceTransitionStyle::Glow,
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.style: water glow, ignoring"
                ),
            },
            "duration" => match parse_duration_ms(value) {
                Some(value) if (50..=5000).contains(&value) => cfg.duration_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected transition.duration from 50 to 5000ms, ignoring"
                ),
            },
            "speed" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.1..=10.0).contains(&value) => {
                    cfg.speed = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.speed from 0.1 to 10, ignoring"
                ),
            },
            "curve" | "ease" => match parse_ease(value) {
                Some(curve) => cfg.curve = curve,
                None => tracing::warn!(
                    value,
                    "Expected one of: linear cubic-out cubic-in-out quad-out exp-out, ignoring"
                ),
            },
            "direction" => match value.as_str() {
                "auto" => cfg.direction = WorkspaceTransitionDirectionMode::Auto,
                "left-to-right" | "ltr" => {
                    cfg.direction = WorkspaceTransitionDirectionMode::LeftToRight
                }
                "right-to-left" | "rtl" => {
                    cfg.direction = WorkspaceTransitionDirectionMode::RightToLeft
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.direction: auto left-to-right right-to-left, ignoring"
                ),
            },
            "workspace_motion" | "move_workspaces" => {
                set_bool(&mut cfg.workspace_motion, key, value)
            }
            "workspace_motion_delay" => match parse_duration_ms(value) {
                Some(value) if value <= 5000 => cfg.workspace_motion_delay_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.workspace_motion_delay_ms from 0 to 5000, ignoring"
                ),
            },
            "wave_amplitude" | "amplitude" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=500.0).contains(&value) => {
                    cfg.wave_amplitude = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.wave_amplitude from 0 to 500, ignoring"
                ),
            },
            "wave_frequency" | "frequency" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=20.0).contains(&value) => {
                    cfg.wave_frequency = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.wave_frequency from 0 to 20, ignoring"
                ),
            },
            "edge_width" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.5..=250.0).contains(&value) => {
                    cfg.edge_width = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.edge_width from 0.5 to 250, ignoring"
                ),
            },
            "color" => match parse_ripple_color(value) {
                Some(color) => cfg.color = color,
                None => tracing::warn!(
                    value,
                    "Expected a transition hex color (RRGGBB, quoted #RRGGBB, or rgb(...)), ignoring"
                ),
            },
            "wave_size" | "size" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=250.0).contains(&value) => {
                    cfg.wave_size = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.wave_size from 0 to 250, ignoring"
                ),
            },
            "wave_alpha" | "alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
                    cfg.wave_alpha = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.wave_alpha from 0 to 1, ignoring"
                ),
            },
            "glow_size" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=500.0).contains(&value) => {
                    cfg.glow_size = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.glow_size from 0 to 500, ignoring"
                ),
            },
            "glow_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
                    cfg.glow_alpha = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.glow_alpha from 0 to 1, ignoring"
                ),
            },
            "water_depth" | "depth" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (1.0..=2000.0).contains(&value) => {
                    cfg.water_depth = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.water_depth from 1 to 2000, ignoring"
                ),
            },
            "water_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
                    cfg.water_alpha = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.water_alpha from 0 to 1, ignoring"
                ),
            },
            "foam_color" => match parse_ripple_color(value) {
                Some(color) => cfg.foam_color = color,
                None => tracing::warn!(
                    value,
                    "Expected a foam hex color (RRGGBB, quoted #RRGGBB, or rgb(...)), ignoring"
                ),
            },
            "foam_size" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=250.0).contains(&value) => {
                    cfg.foam_size = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.foam_size from 0 to 250, ignoring"
                ),
            },
            "foam_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
                    cfg.foam_alpha = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.foam_alpha from 0 to 1, ignoring"
                ),
            },
            "spray_amount" | "spray" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
                    cfg.spray_amount = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.spray_amount from 0 to 1, ignoring"
                ),
            },
            "turbulence" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=2.0).contains(&value) => {
                    cfg.turbulence = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace_transition.turbulence from 0 to 2, ignoring"
                ),
            },
            other => tracing::warn!(
                key = %other,
                "Unknown key in `workspace_transition` block, ignoring"
            ),
        }
    }
}

fn apply_connected_vessels_block(cfg: &mut ConnectedVesselsConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `connected_vessels` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "falloff" | "damping" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.falloff = value.clamp(0.0, 1.0),
                _ => tracing::warn!(
                    value,
                    "Expected connected_vessels.falloff from 0 to 1, ignoring"
                ),
            },
            "max_splits" | "depth" => match value.parse::<u8>() {
                Ok(value) if (1..=8).contains(&value) => cfg.max_splits = value,
                _ => tracing::warn!(
                    value,
                    "Expected connected_vessels.max_splits from 1 to 8, ignoring"
                ),
            },
            other => {
                tracing::warn!(key = %other, "Unknown key in `connected_vessels` block, ignoring")
            }
        }
    }
}

fn apply_sway_block(cfg: &mut SwayConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `sway` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "response" | "gain" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.response = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected sway.response from 0 to 1, ignoring"),
            },
            "max_offset" | "amplitude" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.max_offset = value.clamp(0.0, 128.0),
                _ => tracing::warn!(value, "Expected sway.max_offset from 0 to 128, ignoring"),
            },
            "frequency" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.frequency = value.clamp(0.1, 10.0),
                _ => tracing::warn!(value, "Expected sway.frequency from 0.1 to 10, ignoring"),
            },
            "damping" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.damping = value.clamp(0.1, 20.0),
                _ => tracing::warn!(value, "Expected sway.damping from 0.1 to 20, ignoring"),
            },
            other => tracing::warn!(key = %other, "Unknown key in `sway` block, ignoring"),
        }
    }
}

fn apply_float_physics_block(cfg: &mut FloatPhysicsConfig, body: &[waves::Entry]) {
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) => match key.as_str() {
                "tier" => match value.trim().to_ascii_lowercase().as_str() {
                    "off" | "false" => cfg.tier = FloatPhysicsTier::Off,
                    "light" | "true" => cfg.tier = FloatPhysicsTier::Light,
                    "full" => cfg.tier = FloatPhysicsTier::Full,
                    _ => tracing::warn!(
                        value,
                        "Expected float_physics.tier: off light full, ignoring"
                    ),
                },
                // Legacy alias from before `full` existed: a plain on/off
                // switch mapped to `light`/`off`.
                "enabled" => match parse_bool(value) {
                    Some(true) => cfg.tier = FloatPhysicsTier::Light,
                    Some(false) => cfg.tier = FloatPhysicsTier::Off,
                    None => tracing::warn!(
                        value,
                        "Expected `true` or `false` for float_physics.enabled, ignoring"
                    ),
                },
                "response" | "gain" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => cfg.response = value.clamp(0.0, 1.0),
                    _ => {
                        tracing::warn!(
                            value,
                            "Expected float_physics.response from 0 to 1, ignoring"
                        )
                    }
                },
                "max_offset" | "amplitude" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => cfg.max_offset = value.clamp(0.0, 128.0),
                    _ => tracing::warn!(
                        value,
                        "Expected float_physics.max_offset from 0 to 128, ignoring"
                    ),
                },
                "frequency" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => cfg.frequency = value.clamp(0.1, 10.0),
                    _ => tracing::warn!(
                        value,
                        "Expected float_physics.frequency from 0.1 to 10, ignoring"
                    ),
                },
                "damping" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => cfg.damping = value.clamp(0.1, 20.0),
                    _ => {
                        tracing::warn!(
                            value,
                            "Expected float_physics.damping from 0.1 to 20, ignoring"
                        )
                    }
                },
                "bob_ratio" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => cfg.bob_ratio = value.clamp(0.0, 2.0),
                    _ => {
                        tracing::warn!(
                            value,
                            "Expected float_physics.bob_ratio from 0 to 2, ignoring"
                        )
                    }
                },
                "radius" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => cfg.radius = value.clamp(0.0, 2048.0),
                    _ => {
                        tracing::warn!(
                            value,
                            "Expected float_physics.radius from 0 to 2048, ignoring"
                        )
                    }
                },
                "falloff" => set_bool(&mut cfg.falloff, key, value),
                "ambient_period_s" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => cfg.ambient_period_s = value.clamp(0.5, 60.0),
                    _ => tracing::warn!(
                        value,
                        "Expected float_physics.ambient_period_s from 0.5 to 60, ignoring"
                    ),
                },
                "restitution" | "bounciness" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => cfg.restitution = value.clamp(0.0, 1.0),
                    _ => tracing::warn!(
                        value,
                        "Expected float_physics.restitution from 0 to 1, ignoring"
                    ),
                },
                "bounce_off_edges" => set_bool(&mut cfg.bounce_off_edges, key, value),
                other => {
                    tracing::warn!(key = %other, "Unknown key in `float_physics` block, ignoring")
                }
            },
            waves::Entry::Block(keyword, header, wave_body) if keyword == "wave" => {
                if !header.trim().is_empty() {
                    tracing::warn!(header, "float_physics.wave block headers are ignored");
                }
                apply_float_physics_wave_block(&mut cfg.wave, wave_body);
            }
            waves::Entry::Block(keyword, ..) => {
                tracing::warn!(block = %keyword, "Unknown block in `float_physics`, ignoring");
            }
            _ => tracing::warn!("Unexpected entry in `float_physics` block, ignoring"),
        }
    }
}

fn apply_float_physics_wave_block(cfg: &mut FloatPhysicsWaveConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `float_physics.wave` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "amplitude" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.amplitude = value.clamp(0.0, 128.0),
                _ => tracing::warn!(
                    value,
                    "Expected float_physics.wave.amplitude from 0 to 128, ignoring"
                ),
            },
            "wavelength" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && value > 0.0 => {
                    cfg.wavelength = value.clamp(10.0, 4096.0)
                }
                _ => tracing::warn!(
                    value,
                    "Expected float_physics.wave.wavelength from 10 to 4096, ignoring"
                ),
            },
            "speed" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.speed = value.clamp(-2000.0, 2000.0),
                _ => tracing::warn!(
                    value,
                    "Expected float_physics.wave.speed from -2000 to 2000, ignoring"
                ),
            },
            other => tracing::warn!(
                key = %other,
                "Unknown key in `float_physics.wave` block, ignoring"
            ),
        }
    }
}

fn parse_glass_animation(value: &str) -> Option<GlassAnimation> {
    match value.trim().to_lowercase().as_str() {
        "static" | "off" | "none" => Some(GlassAnimation::Static),
        "reactive" | "disturbed" => Some(GlassAnimation::Reactive),
        "ambient" | "always" | "drift" => Some(GlassAnimation::Ambient),
        _ => None,
    }
}

fn apply_water_glass_block(cfg: &mut WaterGlassConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `water_glass` block, ignoring");
            continue;
        };
        match key.as_str() {
            "animation" | "mode" => match parse_glass_animation(value) {
                Some(animation) => cfg.animation = animation,
                None => tracing::warn!(
                    value,
                    "Expected static, reactive, or ambient for water_glass.animation, ignoring"
                ),
            },
            "speed" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.speed = value.clamp(0.0, 8.0),
                _ => tracing::warn!(value, "Expected water_glass.speed from 0 to 8, ignoring"),
            },
            "amplitude" | "strength" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.amplitude = value.clamp(0.0, 4.0),
                _ => {
                    tracing::warn!(
                        value,
                        "Expected water_glass.amplitude from 0 to 4, ignoring"
                    )
                }
            },
            "settle_ms" | "settle" => match parse_duration_ms(value) {
                Some(value) => cfg.settle_ms = value.clamp(100, 10_000),
                _ => tracing::warn!(
                    value,
                    "Expected water_glass.settle_ms from 100 to 10000, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `water_glass` block, ignoring"),
        }
    }
}

/// Parses a space-separated list of non-negative integers. `None` when any
/// token fails to parse. Used by the caustics idle-decay tiers.
fn parse_u32_list(value: &str) -> Option<Vec<u32>> {
    let items = parse_list_value(value)
        .unwrap_or_else(|| value.split_whitespace().map(str::to_string).collect());
    let list: Vec<u32> = items
        .iter()
        .map(|token| token.parse::<u32>().ok())
        .collect::<Option<Vec<_>>>()?;
    (!list.is_empty()).then_some(list)
}

/// A list of duration values: either the Wave engine's serialized list
/// form (`[600ms, 1s]`) or the legacy whitespace-separated form. Each
/// item goes through [`parse_duration_ms`].
fn parse_duration_list(value: &str) -> Option<Vec<u64>> {
    let items = parse_list_value(value)
        .unwrap_or_else(|| value.split_whitespace().map(str::to_string).collect());
    let list: Vec<u64> = items
        .iter()
        .map(|item| parse_duration_ms(item).map(u64::from))
        .collect::<Option<Vec<_>>>()?;
    (!list.is_empty()).then_some(list)
}

fn apply_caustics_block(cfg: &mut CausticsConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `caustics` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "intensity" | "strength" | "alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.intensity = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected caustics.intensity from 0 to 1, ignoring"),
            },
            "color" => match parse_ripple_color(value) {
                Some(c) => cfg.color = c,
                None => tracing::warn!(value, "Expected a hex color for caustics.color, ignoring"),
            },
            "scale" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.scale = value.clamp(0.1, 8.0),
                _ => tracing::warn!(value, "Expected caustics.scale from 0.1 to 8, ignoring"),
            },
            "speed" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.speed = value.clamp(0.0, 8.0),
                _ => tracing::warn!(value, "Expected caustics.speed from 0 to 8, ignoring"),
            },
            "fps" => match value.parse::<u32>() {
                Ok(value) => cfg.fps = value.clamp(0, 60),
                _ => tracing::warn!(value, "Expected caustics.fps from 0 to 60, ignoring"),
            },
            "idle_fps" => match parse_u32_list(value) {
                Some(list) => cfg.idle_fps = list,
                None => tracing::warn!(
                    value,
                    "Expected space-separated integers for caustics.idle_fps, ignoring"
                ),
            },
            "idle_after" => match parse_duration_list(value) {
                Some(list) => cfg.idle_after_ms = list,
                None => tracing::warn!(
                    value,
                    "Expected space-separated integers for caustics.idle_after_ms, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `caustics` block, ignoring"),
        }
    }
}

fn apply_swim_block(cfg: &mut SwimConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `swim` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "neighbors" | "window" => match value.parse::<u8>() {
                Ok(value) => cfg.neighbors = value.clamp(1, 4),
                _ => tracing::warn!(value, "Expected swim.neighbors from 1 to 4, ignoring"),
            },
            "response" | "gain" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.response = value.clamp(0.1, 4.0),
                _ => tracing::warn!(value, "Expected swim.response from 0.1 to 4, ignoring"),
            },
            "snap_duration_ms" | "snap_ms" => match parse_duration_ms(value) {
                Some(value) => cfg.snap_duration_ms = value.min(2000),
                _ => tracing::warn!(
                    value,
                    "Expected swim.snap_duration from 0 to 2000ms, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `swim` block, ignoring"),
        }
    }
}

fn apply_compass_block(cfg: &mut CompassConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `compass` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "urgent_color" | "urgent" => {
                if let Some(color) = parse_ripple_color(value) {
                    cfg.urgent_color = color;
                } else {
                    tracing::warn!(
                        value,
                        "Expected compass.urgent_color as #RRGGBB/rgb(...), ignoring"
                    );
                }
            }
            "deep_color" | "deep" => {
                if let Some(color) = parse_ripple_color(value) {
                    cfg.deep_color = color;
                } else {
                    tracing::warn!(
                        value,
                        "Expected compass.deep_color as #RRGGBB/rgb(...), ignoring"
                    );
                }
            }
            "max_distance" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && value > 0.0 => {
                    cfg.max_distance = value.min(50_000.0)
                }
                _ => tracing::warn!(value, "Expected compass.max_distance > 0, ignoring"),
            },
            "size" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && value > 0.0 => {
                    cfg.size = value.clamp(8.0, 1024.0)
                }
                _ => tracing::warn!(value, "Expected compass.size from 8 to 1024, ignoring"),
            },
            "alpha" | "peak_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.alpha = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected compass.alpha from 0 to 1, ignoring"),
            },
            "shape" => match crate::compass::CompassShape::parse(value) {
                Some(shape) => cfg.shape = shape,
                None => tracing::warn!(
                    value,
                    "Expected compass.shape as circle/arrow/chevron/ring/diamond, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `compass` block, ignoring"),
        }
    }
}

fn apply_minimap_block(cfg: &mut MinimapConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `minimap` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "key" => match parse_simple_chord(value) {
                Some((mods, keysym)) => {
                    cfg.mods = mods;
                    cfg.keysym = keysym;
                }
                None => tracing::warn!(
                    value,
                    "Expected minimap.key as a single modifier+key chord (e.g. \"Super+Space\"), ignoring"
                ),
            },
            "preset" => match crate::minimap::MinimapPreset::parse(value) {
                Some(preset) => cfg.preset = preset,
                None => tracing::warn!(
                    value,
                    "Expected minimap.preset as plain/bioluminescent/glass, ignoring"
                ),
            },
            "background_color" | "background" => match parse_ripple_color(value) {
                Some(color) => cfg.background_color = Some(color),
                None => tracing::warn!(
                    value,
                    "Expected minimap.background_color as #RRGGBB/rgb(...), ignoring"
                ),
            },
            "window_color" | "window" => match parse_ripple_color(value) {
                Some(color) => cfg.window_color = Some(color),
                None => tracing::warn!(
                    value,
                    "Expected minimap.window_color as #RRGGBB/rgb(...), ignoring"
                ),
            },
            "accent_color" | "accent" => match parse_ripple_color(value) {
                Some(color) => cfg.accent_color = Some(color),
                None => tracing::warn!(
                    value,
                    "Expected minimap.accent_color as #RRGGBB/rgb(...), ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `minimap` block, ignoring"),
        }
    }
}

fn apply_depth_block(cfg: &mut DepthConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `depth` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "sink_after_ms" | "delay_ms" => match parse_duration_ms(value) {
                Some(value) if value <= 86_400_000 => cfg.sink_after_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected depth.sink_after_ms from 0 to 86400000, ignoring"
                ),
            },
            "tier_interval_ms" | "interval_ms" => match parse_duration_ms(value) {
                Some(value) if (1..=86_400_000).contains(&value) => cfg.tier_interval_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected depth.tier_interval_ms from 1 to 86400000, ignoring"
                ),
            },
            "max_tier" | "tiers" => match value.parse::<u8>() {
                Ok(value) if (1..=8).contains(&value) => cfg.max_tier = value,
                _ => tracing::warn!(value, "Expected depth.max_tier from 1 to 8, ignoring"),
            },
            "tier_one_alpha" | "live_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.tier_one_alpha = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected depth.tier_one_alpha from 0 to 1, ignoring"),
            },
            "cool_color" => match parse_ripple_color(value) {
                Some(value) => cfg.cool_color = value,
                None => {
                    tracing::warn!(value, "Expected a hex color for depth.cool_color, ignoring")
                }
            },
            "cool_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.cool_alpha = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected depth.cool_alpha from 0 to 1, ignoring"),
            },
            "schematic_color" => match parse_ripple_color(value) {
                Some(value) => cfg.schematic_color = value,
                None => tracing::warn!(
                    value,
                    "Expected a hex color for depth.schematic_color, ignoring"
                ),
            },
            "schematic_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.schematic_alpha = value.clamp(0.0, 1.0),
                _ => tracing::warn!(
                    value,
                    "Expected depth.schematic_alpha from 0 to 1, ignoring"
                ),
            },
            "border_color" => match parse_ripple_color(value) {
                Some(value) => cfg.border_color = value,
                None => tracing::warn!(
                    value,
                    "Expected a hex color for depth.border_color, ignoring"
                ),
            },
            "urgent_color" => match parse_ripple_color(value) {
                Some(value) => cfg.urgent_color = value,
                None => tracing::warn!(
                    value,
                    "Expected a hex color for depth.urgent_color, ignoring"
                ),
            },
            "urgent_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.urgent_alpha = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected depth.urgent_alpha from 0 to 1, ignoring"),
            },
            other => tracing::warn!(key = %other, "Unknown key in `depth` block, ignoring"),
        }
    }
}

fn apply_classic_depth_block(cfg: &mut ClassicDepthConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `classic_depth` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "animation" | "animate" => set_bool(&mut cfg.animation, key, value),
            "animation_duration_ms" => match parse_duration_ms(value) {
                Some(value) if value <= 3000 => cfg.animation_duration_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected classic_depth.animation_duration_ms from 0 to 3000, ignoring"
                ),
            },
            "wave_color" | "color" => match parse_ripple_color(value) {
                Some(value) => cfg.wave_color = value,
                None => tracing::warn!(
                    value,
                    "Expected a hex color for classic_depth.wave_color, ignoring"
                ),
            },
            "wave_alpha" | "alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.wave_alpha = value.clamp(0.0, 1.0),
                _ => tracing::warn!(
                    value,
                    "Expected classic_depth.wave_alpha from 0 to 1, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `classic_depth` block, ignoring"),
        }
    }
}

fn apply_ocean_block(cfg: &mut OceanConfig, body: &[waves::Entry]) {
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) if key == "freeform_windows" => {
                set_bool(&mut cfg.freeform_windows, "ocean.freeform_windows", value)
            }
            waves::Entry::Assign(key, value) if key == "smart_tiling" => {
                set_bool(&mut cfg.smart_tiling, "ocean.smart_tiling", value)
            }
            waves::Entry::Assign(key, value) if key == "smart_tiling_snap_distance" => {
                match value.parse::<i32>() {
                    Ok(value) if (0..=512).contains(&value) => {
                        cfg.smart_tiling_snap_distance = value
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected ocean.smart_tiling_snap_distance from 0 to 512, ignoring"
                    ),
                }
            }
            waves::Entry::Assign(key, value) if key == "smart_tiling_preserve_size" => set_bool(
                &mut cfg.smart_tiling_preserve_size,
                "ocean.smart_tiling_preserve_size",
                value,
            ),
            waves::Entry::Assign(key, value) if key == "canvas_pan_button" => {
                cfg.canvas_pan_button = match value.trim().to_ascii_lowercase().as_str() {
                    "none" | "disabled" | "off" => OceanPanButton::Disabled,
                    "left" | "primary" => OceanPanButton::Left,
                    "middle" => OceanPanButton::Middle,
                    "right" | "secondary" => OceanPanButton::Right,
                    _ => {
                        tracing::warn!(
                            value,
                            "Expected ocean.canvas_pan_button left, middle, right, or none; ignoring"
                        );
                        cfg.canvas_pan_button
                    }
                }
            }
            waves::Entry::Assign(key, value) if key == "canvas_pan_requires_modifier" => set_bool(
                &mut cfg.canvas_pan_requires_modifier,
                "ocean.canvas_pan_requires_modifier",
                value,
            ),
            waves::Entry::Assign(key, value) if key == "camera_step" => {
                match value.parse::<i32>() {
                    Ok(value) if (32..=8192).contains(&value) => cfg.camera_step = value,
                    _ => tracing::warn!(
                        value,
                        "Expected ocean.camera_step from 32 to 8192, ignoring"
                    ),
                }
            }
            waves::Entry::Assign(key, value) if key == "depth_enabled" => {
                set_bool(&mut cfg.depth_enabled, "ocean.depth_enabled", value)
            }
            waves::Entry::Assign(key, value) if key == "zoom_enabled" => {
                set_bool(&mut cfg.zoom_enabled, "ocean.zoom_enabled", value)
            }
            waves::Entry::Assign(key, value) if key == "modifier_zoom" => {
                set_bool(&mut cfg.modifier_zoom, "ocean.modifier_zoom", value)
            }
            waves::Entry::Assign(key, value) if key == "min_zoom" => match value.parse::<f64>() {
                Ok(value) if value.is_finite() && (0.05..=8.0).contains(&value) => {
                    cfg.min_zoom = value
                }
                _ => tracing::warn!(value, "Expected ocean.min_zoom from 0.05 to 8.0, ignoring"),
            },
            waves::Entry::Assign(key, value) if key == "max_zoom" => match value.parse::<f64>() {
                Ok(value) if value.is_finite() && (0.05..=8.0).contains(&value) => {
                    cfg.max_zoom = value
                }
                _ => tracing::warn!(value, "Expected ocean.max_zoom from 0.05 to 8.0, ignoring"),
            },
            waves::Entry::Assign(key, value) if key == "zoom_step" => match value.parse::<f64>() {
                Ok(value) if value.is_finite() && (1.01..=3.0).contains(&value) => {
                    cfg.zoom_step = value
                }
                _ => tracing::warn!(value, "Expected ocean.zoom_step from 1.01 to 3.0, ignoring"),
            },
            waves::Entry::Assign(key, value) if key == "camera_animation_ms" => {
                match value.parse::<u64>() {
                    Ok(value) if value <= 5000 => cfg.camera_animation_ms = value,
                    _ => tracing::warn!(
                        value,
                        "Expected ocean.camera_animation_ms from 0 to 5000, ignoring"
                    ),
                }
            }
            waves::Entry::Assign(key, value) if key == "camera_sway" => {
                match value.parse::<f64>() {
                    Ok(value) if value.is_finite() && (0.0..=256.0).contains(&value) => {
                        cfg.camera_sway = value
                    }
                    _ => {
                        tracing::warn!(value, "Expected ocean.camera_sway from 0 to 256, ignoring")
                    }
                }
            }
            waves::Entry::Assign(key, value) if key == "canvas_guides" => {
                set_bool(&mut cfg.canvas_guides, "ocean.canvas_guides", value)
            }
            waves::Entry::Assign(key, value) if key == "canvas_grid_size" => {
                match value.parse::<i32>() {
                    Ok(value) if (32..=8192).contains(&value) => cfg.canvas_grid_size = value,
                    _ => tracing::warn!(
                        value,
                        "Expected ocean.canvas_grid_size from 32 to 8192, ignoring"
                    ),
                }
            }
            waves::Entry::Assign(key, value) if key == "canvas_grid_alpha" => {
                match value.parse::<f32>() {
                    Ok(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
                        cfg.canvas_grid_alpha = value
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected ocean.canvas_grid_alpha from 0 to 1, ignoring"
                    ),
                }
            }
            waves::Entry::Assign(key, value) if key == "canvas_marker" => {
                set_bool(&mut cfg.canvas_marker, "ocean.canvas_marker", value)
            }
            waves::Entry::Assign(key, value) if key == "canvas_marker_fade_ms" => {
                match value.parse::<u64>() {
                    Ok(value) if value <= 30_000 => cfg.canvas_marker_fade_ms = value,
                    _ => tracing::warn!(
                        value,
                        "Expected ocean.canvas_marker_fade_ms from 0 to 30000, ignoring"
                    ),
                }
            }
            waves::Entry::Block(keyword, header, entries) if keyword == "reef" => {
                if let Some(reef) = lower_ocean_reef(header, entries) {
                    if let Some(existing) = cfg.reefs.iter_mut().find(|item| item.name == reef.name)
                    {
                        *existing = reef;
                    } else {
                        cfg.reefs.push(reef);
                    }
                }
            }
            waves::Entry::Block(keyword, header, entries) if keyword == "bookmark" => {
                if let Some(bookmark) = lower_ocean_bookmark(header, entries) {
                    if let Some(existing) = cfg
                        .bookmarks
                        .iter_mut()
                        .find(|item| item.name == bookmark.name)
                    {
                        *existing = bookmark;
                    } else {
                        cfg.bookmarks.push(bookmark);
                    }
                }
            }
            _ => tracing::warn!("Unexpected entry in `ocean` block, ignoring"),
        }
    }
    if cfg.min_zoom > cfg.max_zoom {
        std::mem::swap(&mut cfg.min_zoom, &mut cfg.max_zoom);
    }
}

fn lower_ocean_reef(header: &str, body: &[waves::Entry]) -> Option<OceanReefConfig> {
    let name = header.trim();
    if name.is_empty() {
        tracing::warn!("An ocean reef needs a name, ignoring");
        return None;
    }
    let mut reef = OceanReefConfig {
        name: name.to_string(),
        x: 0,
        y: 0,
        width: None,
        height: None,
    };
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!(reef = name, "Unexpected reef entry, ignoring");
            continue;
        };
        match (key.as_str(), value.parse::<i32>()) {
            ("x", Ok(parsed)) => reef.x = parsed,
            ("y", Ok(parsed)) => reef.y = parsed,
            ("width", Ok(parsed)) if parsed > 0 => reef.width = Some(parsed),
            ("height", Ok(parsed)) if parsed > 0 => reef.height = Some(parsed),
            ("x" | "y" | "width" | "height", _) => {
                tracing::warn!(
                    key,
                    value,
                    reef = name,
                    "Expected a valid integer reef value"
                )
            }
            (other, _) => {
                tracing::warn!(key = other, reef = name, "Unknown reef key, ignoring")
            }
        }
    }
    Some(reef)
}

fn lower_ocean_bookmark(header: &str, body: &[waves::Entry]) -> Option<OceanBookmarkConfig> {
    let name = header.trim();
    if name.is_empty() {
        tracing::warn!("An ocean bookmark needs a name, ignoring");
        return None;
    }
    let mut bookmark = OceanBookmarkConfig {
        name: name.to_string(),
        x: 0.0,
        y: 0.0,
    };
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!(bookmark = name, "Unexpected bookmark entry, ignoring");
            continue;
        };
        let target = match key.as_str() {
            "x" => &mut bookmark.x,
            "y" => &mut bookmark.y,
            other => {
                tracing::warn!(
                    key = other,
                    bookmark = name,
                    "Unknown bookmark key, ignoring"
                );
                continue;
            }
        };
        match value.parse::<f64>() {
            Ok(parsed) if parsed.is_finite() => *target = parsed,
            _ => tracing::warn!(
                key,
                value,
                bookmark = name,
                "Expected finite bookmark value"
            ),
        }
    }
    Some(bookmark)
}

fn apply_frost_block(cfg: &mut FrostConfig, body: &[waves::Entry]) {
    let mut overrides = FrostOverrides::default();
    apply_frost_override_block(&mut overrides, body);
    *cfg = overrides.apply_to(cfg);
}

fn apply_frost_override_block(cfg: &mut FrostOverrides, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `frost` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => match parse_bool(value) {
                Some(value) => cfg.enabled = Some(value),
                None => tracing::warn!(
                    value,
                    "Expected frost.enabled to be true or false, ignoring"
                ),
            },
            "radius" | "blur_radius" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.radius = Some(value.clamp(0.0, 64.0)),
                _ => tracing::warn!(value, "Expected frost.radius from 0 to 64, ignoring"),
            },
            "strength" | "blur_strength" | "frost" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.strength = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected frost.strength from 0 to 1, ignoring"),
            },
            "opacity" | "glass_opacity" | "background_opacity" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.opacity = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected frost.opacity from 0 to 1, ignoring"),
            },
            "saturation" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.saturation = Some(value.clamp(0.0, 2.0)),
                _ => tracing::warn!(value, "Expected frost.saturation from 0 to 2, ignoring"),
            },
            "contrast" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.contrast = Some(value.clamp(0.0, 2.0)),
                _ => tracing::warn!(value, "Expected frost.contrast from 0 to 2, ignoring"),
            },
            "brightness" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.brightness = Some(value.clamp(0.0, 2.0)),
                _ => tracing::warn!(value, "Expected frost.brightness from 0 to 2, ignoring"),
            },
            "noise" | "grain" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.noise = Some(value.clamp(0.0, 0.25)),
                _ => tracing::warn!(value, "Expected frost.noise from 0 to 0.25, ignoring"),
            },
            "noise_scale" | "grain_scale" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.noise_scale = Some(value.clamp(0.25, 16.0)),
                _ => {
                    tracing::warn!(
                        value,
                        "Expected frost.noise_scale from 0.25 to 16, ignoring"
                    )
                }
            },
            "vibrancy" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.vibrancy = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected frost.vibrancy from 0 to 1, ignoring"),
            },
            "vibrancy_darkness" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => {
                    cfg.vibrancy_darkness = Some(value.clamp(0.0, 1.0))
                }
                _ => tracing::warn!(
                    value,
                    "Expected frost.vibrancy_darkness from 0 to 1, ignoring"
                ),
            },
            "tint_color" | "color" => match parse_ripple_color(value) {
                Some(value) => cfg.tint_color = Some(value),
                None => {
                    tracing::warn!(value, "Expected a hex color for frost.tint_color, ignoring")
                }
            },
            "tint_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.tint_alpha = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected frost.tint_alpha from 0 to 1, ignoring"),
            },
            "corner_radius" | "rounding" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.corner_radius = Some(value.clamp(0.0, 256.0)),
                _ => {
                    tracing::warn!(
                        value,
                        "Expected frost.corner_radius from 0 to 256, ignoring"
                    )
                }
            },
            "corner_softness" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => {
                    cfg.corner_softness = Some(value.clamp(0.25, 8.0))
                }
                _ => tracing::warn!(
                    value,
                    "Expected frost.corner_softness from 0.25 to 8, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `frost` block, ignoring"),
        }
    }
}

fn apply_shadow_block(cfg: &mut ShadowConfig, body: &[waves::Entry]) {
    let mut overrides = ShadowOverrides::default();
    apply_shadow_override_block(&mut overrides, body);
    *cfg = overrides.apply_to(cfg);
}

fn apply_shadow_override_block(cfg: &mut ShadowOverrides, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `shadow` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => match parse_bool(value) {
                Some(value) => cfg.enabled = Some(value),
                None => tracing::warn!(
                    value,
                    "Expected shadow.enabled to be true or false, ignoring"
                ),
            },
            "softness" | "range" | "size" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.softness = Some(value.clamp(0.0, 256.0)),
                _ => tracing::warn!(value, "Expected shadow.softness from 0 to 256, ignoring"),
            },
            "spread" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.spread = Some(value.clamp(-128.0, 256.0)),
                _ => tracing::warn!(value, "Expected shadow.spread from -128 to 256, ignoring"),
            },
            "offset" => match parse_shadow_offset(value) {
                Some((x, y)) => {
                    cfg.offset_x = Some(x);
                    cfg.offset_y = Some(y);
                }
                None => tracing::warn!(
                    value,
                    "Expected shadow.offset as <x>x<y>, `x y`, or `{{x, y}}`, ignoring"
                ),
            },
            "offset_x" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => {
                    cfg.offset_x = Some(value.clamp(-512.0, 512.0));
                }
                _ => tracing::warn!(value, "Expected a finite shadow.offset_x, ignoring"),
            },
            "offset_y" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => {
                    cfg.offset_y = Some(value.clamp(-512.0, 512.0));
                }
                _ => tracing::warn!(value, "Expected a finite shadow.offset_y, ignoring"),
            },
            "scale" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.scale = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected shadow.scale from 0 to 1, ignoring"),
            },
            "render_power" | "falloff_power" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.render_power = Some(value.clamp(1.0, 8.0)),
                _ => tracing::warn!(value, "Expected shadow.render_power from 1 to 8, ignoring"),
            },
            "sharp" => match parse_bool(value) {
                Some(value) => cfg.sharp = Some(value),
                None => {
                    tracing::warn!(value, "Expected shadow.sharp to be true or false, ignoring")
                }
            },
            "draw_behind_window" | "draw_behind" => match parse_bool(value) {
                Some(value) => cfg.draw_behind_window = Some(value),
                None => tracing::warn!(
                    value,
                    "Expected shadow.draw_behind_window to be true or false, ignoring"
                ),
            },
            // Hyprland's historical inverse spelling.
            "ignore_window" => match parse_bool(value) {
                Some(value) => cfg.draw_behind_window = Some(!value),
                None => tracing::warn!(
                    value,
                    "Expected shadow.ignore_window to be true or false, ignoring"
                ),
            },
            "color" | "active_color" => match parse_rgba_color(value) {
                Some(value) => cfg.color = Some(value),
                None => tracing::warn!(value, "Expected a shadow RGBA color, ignoring"),
            },
            "inactive_color" | "color_inactive" => match parse_rgba_color(value) {
                Some(value) => cfg.inactive_color = Some(value),
                None => tracing::warn!(value, "Expected an inactive shadow RGBA color, ignoring"),
            },
            "urgent_color" | "color_urgent" => match parse_rgba_color(value) {
                Some(value) => cfg.urgent_color = Some(value),
                None => tracing::warn!(value, "Expected an urgent shadow RGBA color, ignoring"),
            },
            "opacity" | "active_opacity" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.opacity = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected shadow.opacity from 0 to 1, ignoring"),
            },
            "inactive_opacity" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => {
                    cfg.inactive_opacity = Some(value.clamp(0.0, 1.0))
                }
                _ => tracing::warn!(
                    value,
                    "Expected shadow.inactive_opacity from 0 to 1, ignoring"
                ),
            },
            "urgent_opacity" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.urgent_opacity = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(
                    value,
                    "Expected shadow.urgent_opacity from 0 to 1, ignoring"
                ),
            },
            "corner_radius" | "rounding" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.corner_radius = Some(value.clamp(0.0, 256.0)),
                _ => tracing::warn!(
                    value,
                    "Expected shadow.corner_radius from 0 to 256, ignoring"
                ),
            },
            "floating_only" => match parse_bool(value) {
                Some(value) => cfg.floating_only = Some(value),
                None => tracing::warn!(
                    value,
                    "Expected shadow.floating_only to be true or false, ignoring"
                ),
            },
            "fullscreen" | "fullscreen_enabled" => match parse_bool(value) {
                Some(value) => cfg.fullscreen = Some(value),
                None => tracing::warn!(
                    value,
                    "Expected shadow.fullscreen to be true or false, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `shadow` block, ignoring"),
        }
    }
}

fn parse_shadow_offset(value: &str) -> Option<(f32, f32)> {
    let trimmed = value
        .trim()
        .trim_start_matches(['{', '[', '('])
        .trim_end_matches(['}', ']', ')']);
    let parts: Vec<&str> = if trimmed.contains('x') {
        trimmed.splitn(2, 'x').collect()
    } else if trimmed.contains(',') {
        trimmed.splitn(2, ',').collect()
    } else {
        trimmed.split_whitespace().collect()
    };
    if parts.len() != 2 {
        return None;
    }
    let x = parts[0].trim().parse::<f32>().ok()?;
    let y = parts[1].trim().parse::<f32>().ok()?;
    (x.is_finite() && y.is_finite()).then_some((x.clamp(-512.0, 512.0), y.clamp(-512.0, 512.0)))
}

/// Parses the body of a `ripple { }` block. Used both for the global
/// block (mutating `RawConfig::ripple` in place) and for a per-rule
/// `rule { ripple { } }` sub-block (mutating a fresh `RippleConfig` and
/// stashing it on the rule). Empty fields stay `None`, which is what
/// `merge_over` later reads as "inherit from the layer below."
fn apply_ripple_block(cfg: &mut RippleConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `ripple` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => match parse_bool(value) {
                Some(v) => cfg.enabled = Some(v),
                None => tracing::warn!(value, "Expected `true` or `false` for ripple.enabled, ignoring"),
            },
            "preset" | "style" => match parse_ripple_preset_selection(value) {
                Some(preset) => cfg.preset = Some(preset),
                None => tracing::warn!(
                    value,
                    "Expected a built-in or named ripple preset, ignoring"
                ),
            },
            "map_preset" | "map_style" => match parse_ripple_preset_selection(value) {
                Some(preset) => cfg.map_preset = Some(preset),
                None => tracing::warn!(value, "Unknown ripple.map_preset, ignoring"),
            },
            "focus_preset" | "focus_style" => match parse_ripple_preset_selection(value) {
                Some(preset) => cfg.focus_preset = Some(preset),
                None => tracing::warn!(value, "Unknown ripple.focus_preset, ignoring"),
            },
            "urgent_preset" | "urgent_style" => match parse_ripple_preset_selection(value) {
                Some(preset) => cfg.urgent_preset = Some(preset),
                None => tracing::warn!(value, "Unknown ripple.urgent_preset, ignoring"),
            },
            "focus_on_map" | "stack_focus_on_map" => match parse_bool(value) {
                Some(v) => cfg.focus_on_map = Some(v),
                None => tracing::warn!(
                    value,
                    "Expected `true` or `false` for ripple.focus_on_map, ignoring"
                ),
            },
            "urgent_repeat" | "urgent_pulse" => match parse_bool(value) {
                Some(v) => cfg.urgent_repeat = Some(v),
                None => tracing::warn!(
                    value,
                    "Expected `true` or `false` for ripple.urgent_repeat, ignoring"
                ),
            },
            "urgent_repeat_interval_ms" | "urgent_interval_ms" | "urgent_interval" => {
                match parse_duration_ms(value) {
                    Some(v) if v > 0 => cfg.urgent_repeat_interval_ms = Some(v.clamp(100, 60_000)),
                    _ => tracing::warn!(
                        value,
                        "Expected a positive integer for ripple.urgent_repeat_interval_ms, ignoring"
                    ),
                }
            }
            "shape" | "shapes" | "form" => match parse_shapes(value) {
                Some(shapes) if !shapes.is_empty() => {
                    cfg.shapes = shapes;
                    cfg.preset = Some(RipplePresetSelection::BuiltIn(RipplePreset::Legacy));
                }
                _ => tracing::warn!(value, "Expected one or more of: ring square droplet cross, ignoring"),
            },
            "color" => match parse_ripple_color(value) {
                Some(c) => cfg.color = Some(c),
                None => tracing::warn!(value, "Expected a hex color (#RRGGBB or #RRGGBBAA), ignoring"),
            },
            "secondary_color" | "accent_color" | "highlight_color" => {
                match parse_ripple_color(value) {
                    Some(c) => cfg.secondary_color = Some(c),
                    None => tracing::warn!(
                        value,
                        "Expected a hex color for ripple.secondary_color, ignoring"
                    ),
                }
            }
            "peak_radius" | "radius" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() && v > 0.0 => cfg.peak_radius = Some(v),
                _ => tracing::warn!(value, "Expected a positive number for ripple.peak_radius, ignoring"),
            },
            "size_mode" | "radius_mode" | "scale_mode" => {
                match parse_ripple_size_mode(value) {
                    Some(mode) => cfg.size_mode = Some(mode),
                    None => tracing::warn!(
                        value,
                        "Expected fixed, window, width, height, min, or max for ripple.size_mode, ignoring"
                    ),
                }
            }
            "size_scale" | "radius_scale" | "window_scale" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() && v > 0.0 => {
                    cfg.size_scale = Some(v.clamp(0.01, 8.0))
                }
                _ => tracing::warn!(value, "Expected ripple.size_scale from 0.01 to 8, ignoring"),
            },
            "min_radius" | "minimum_radius" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() && v >= 0.0 => {
                    cfg.min_radius = Some(v.clamp(0.0, 8192.0))
                }
                _ => tracing::warn!(value, "Expected ripple.min_radius from 0 to 8192, ignoring"),
            },
            "max_radius" | "maximum_radius" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() && v > 0.0 => {
                    cfg.max_radius = Some(v.clamp(1.0, 8192.0))
                }
                _ => tracing::warn!(value, "Expected ripple.max_radius from 1 to 8192, ignoring"),
            },
            "thickness" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() && v > 0.0 => cfg.thickness = Some(v),
                _ => tracing::warn!(value, "Expected a positive number for ripple.thickness, ignoring"),
            },
            "duration" => match parse_duration_ms(value) {
                Some(v) if v > 0 => cfg.duration_ms = Some(v),
                _ => tracing::warn!(value, "Expected a positive integer for ripple.duration_ms, ignoring"),
            },
            "peak_alpha" | "alpha" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() => cfg.peak_alpha = Some(v.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected a number from 0.0 to 1.0 for ripple.peak_alpha, ignoring"),
            },
            "glow" | "glow_strength" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() => cfg.glow = Some(v.clamp(0.0, 2.0)),
                _ => tracing::warn!(value, "Expected ripple.glow from 0 to 2, ignoring"),
            },
            "wobble" | "jiggle" | "distortion" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() => cfg.wobble = Some(v.clamp(0.0, 2.0)),
                _ => tracing::warn!(value, "Expected ripple.wobble from 0 to 2, ignoring"),
            },
            "detail" | "complexity" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() => cfg.detail = Some(v.clamp(0.0, 2.0)),
                _ => tracing::warn!(value, "Expected ripple.detail from 0 to 2, ignoring"),
            },
            "ease" => match parse_ease(value) {
                Some(e) => cfg.ease = Some(e),
                None => tracing::warn!(value, "Expected one of: linear cubic-out cubic-in-out quad-out exp-out, ignoring"),
            },
            "anchor" => match parse_anchor(value) {
                Some(a) => cfg.anchor = Some(a),
                None => tracing::warn!(value, "Expected center, cursor, a side, nearest-edge, or a corner for ripple.anchor, ignoring"),
            },
            "edge_position" | "edge_pos" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() => cfg.edge_position = Some(v.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected ripple.edge_position from 0 to 1, ignoring"),
            },
            "edge_offset" | "edge_distance" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() => {
                    cfg.edge_offset = Some(v.clamp(-4096.0, 4096.0))
                }
                _ => tracing::warn!(value, "Expected a number for ripple.edge_offset, ignoring"),
            },
            "offset" => match parse_position(value) {
                Some(o) => cfg.offset = Some(o),
                None => tracing::warn!(value, "Expected <dx>x<dy> for ripple.offset, ignoring"),
            },
            "layer" => match parse_layer(value) {
                Some(l) => cfg.layer = Some(l),
                None => tracing::warn!(value, "Expected one of: above-windows below-windows above-all below-all, ignoring"),
            },
            "triggers" => match parse_triggers(value) {
                Some(triggers) if !triggers.is_empty() => cfg.triggers = triggers,
                _ => tracing::warn!(value, "Expected one or more of: map focus urgent, ignoring"),
            },
            other => tracing::warn!(key = %other, "Unknown key in `ripple` block, ignoring"),
        }
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Parses a hex color: `RRGGBB` / `RRGGBBAA` bare, or `#RRGGBB` /
/// `#RRGGBBAA` (the alpha byte is silently ignored for now; `peak_alpha`
/// is the dedicated transparency knob).
///
/// Returns linear-space RGB in `[0.0, 1.0]`.
fn parse_ripple_color(value: &str) -> Option<[f32; 3]> {
    let v = value
        .trim()
        .strip_prefix('#')
        .unwrap_or_else(|| value.trim());
    decode_hex_rgb(v)
}

fn decode_hex_rgb(hex: &str) -> Option<[f32; 3]> {
    let hex = match hex.len() {
        6 => hex,
        8 => &hex[..6],
        _ => return None,
    };
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
}

/// Shadow colors keep their alpha channel, unlike ripple/frost colors which
/// have a separate dedicated alpha knob. Accepts bare/`#` hex and the
/// legacy `0xAARRGGBB` form.
fn parse_rgba_color(value: &str) -> Option<[f32; 4]> {
    let value = value
        .trim()
        .strip_prefix('#')
        .unwrap_or_else(|| value.trim());
    if let Some(hex) = value.strip_prefix("0x") {
        if hex.len() != 8 {
            return None;
        }
        let a = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let r = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let g = u8::from_str_radix(&hex[4..6], 16).ok()?;
        let b = u8::from_str_radix(&hex[6..8], 16).ok()?;
        return Some([r, g, b, a].map(|channel| channel as f32 / 255.0));
    }
    decode_hex_rgba(value)
}

fn decode_hex_rgba(hex: &str) -> Option<[f32; 4]> {
    let (rgb, alpha) = match hex.len() {
        6 => (hex, 255),
        8 => (&hex[..6], u8::from_str_radix(&hex[6..8], 16).ok()?),
        _ => return None,
    };
    let color = decode_hex_rgb(rgb)?;
    Some([color[0], color[1], color[2], alpha as f32 / 255.0])
}

fn apply_rounding_block(cfg: &mut RoundingConfig, body: &[waves::Entry]) {
    let mut overrides = RoundingOverrides::default();
    apply_rounding_override_block(&mut overrides, body);
    *cfg = overrides.apply_to(cfg);
}

fn parse_corner_radii(value: &str) -> Option<[f32; 4]> {
    let values = value
        .trim_matches(|c| matches!(c, '[' | ']' | '{' | '}'))
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .map(str::parse::<f32>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let clamp = |value: f32| value.clamp(0.0, 512.0);
    match values.as_slice() {
        [one] if one.is_finite() => Some([clamp(*one); 4]),
        [tl, tr, br, bl] if values.iter().all(|value| value.is_finite()) => {
            Some([clamp(*tl), clamp(*tr), clamp(*br), clamp(*bl)])
        }
        _ => None,
    }
}

fn apply_rounding_override_block(cfg: &mut RoundingOverrides, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `rounding` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => match parse_bool(value) {
                Some(value) => cfg.enabled = Some(value),
                None => tracing::warn!(value, "Expected rounding.enabled boolean, ignoring"),
            },
            "radius" | "radii" | "geometry_corner_radius" | "geometry-corner-radius" => {
                match parse_corner_radii(value) {
                    Some([tl, tr, br, bl]) => {
                        cfg.top_left = Some(tl);
                        cfg.top_right = Some(tr);
                        cfg.bottom_right = Some(br);
                        cfg.bottom_left = Some(bl);
                    }
                    None => tracing::warn!(
                        value,
                        "Expected one radius or four CSS-order radii, ignoring"
                    ),
                }
            }
            "top_left" | "top-left" => {
                cfg.top_left = parse_f32_clamped(value, 0.0, 512.0, "rounding.top_left");
            }
            "top_right" | "top-right" => {
                cfg.top_right = parse_f32_clamped(value, 0.0, 512.0, "rounding.top_right");
            }
            "bottom_right" | "bottom-right" => {
                cfg.bottom_right = parse_f32_clamped(value, 0.0, 512.0, "rounding.bottom_right");
            }
            "bottom_left" | "bottom-left" => {
                cfg.bottom_left = parse_f32_clamped(value, 0.0, 512.0, "rounding.bottom_left");
            }
            "power" | "rounding_power" => {
                cfg.power = parse_f32_clamped(value, 1.0, 10.0, "rounding.power");
            }
            "antialias" | "corner_softness" => {
                cfg.antialias = parse_f32_clamped(value, 0.0, 8.0, "rounding.antialias");
            }
            "clip" | "clip_to_geometry" | "clip-to-geometry" => match parse_bool(value) {
                Some(value) => cfg.clip = Some(value),
                None => tracing::warn!(value, "Expected rounding.clip boolean, ignoring"),
            },
            "floating_only" => match parse_bool(value) {
                Some(value) => cfg.floating_only = Some(value),
                None => tracing::warn!(value, "Expected rounding.floating_only boolean, ignoring"),
            },
            "fullscreen" => match parse_bool(value) {
                Some(value) => cfg.fullscreen = Some(value),
                None => tracing::warn!(value, "Expected rounding.fullscreen boolean, ignoring"),
            },
            _ => tracing::warn!(key, "Unknown rounding key, ignoring"),
        }
    }
}

fn apply_border_block(cfg: &mut BorderConfig, body: &[waves::Entry]) {
    let mut overrides = BorderOverrides::default();
    apply_border_override_block(&mut overrides, body);
    *cfg = overrides.apply_to(cfg);
}

fn parse_f32_clamped(value: &str, min: f32, max: f32, key: &str) -> Option<f32> {
    match value.parse::<f32>() {
        Ok(value) if value.is_finite() => Some(value.clamp(min, max)),
        _ => {
            tracing::warn!(value, key, "Expected a finite numeric value, ignoring");
            None
        }
    }
}

fn apply_border_override_block(cfg: &mut BorderOverrides, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `border` block, ignoring");
            continue;
        };
        let color = |value: &str, key: &str| {
            parse_rgba_color(value).or_else(|| {
                tracing::warn!(value, key, "Expected an RGBA border color, ignoring");
                None
            })
        };
        match key.as_str() {
            "enabled" => match parse_bool(value) {
                Some(value) => cfg.enabled = Some(value),
                None => tracing::warn!(value, "Expected border.enabled boolean, ignoring"),
            },
            "width" | "size" | "border_size" => {
                cfg.width = parse_f32_clamped(value, 0.0, 64.0, "border.width");
            }
            "placement" | "position" => {
                cfg.placement = match value.trim().to_ascii_lowercase().as_str() {
                    "outside" | "outer" => Some(BorderPlacement::Outside),
                    "center" | "centered" => Some(BorderPlacement::Center),
                    "inside" | "inner" => Some(BorderPlacement::Inside),
                    _ => {
                        tracing::warn!(value, "Expected border placement outside|center|inside");
                        None
                    }
                }
            }
            "color" | "active_color" | "active_from" => cfg.active_from = color(value, key),
            "color_to" | "active_color_to" | "active_to" => cfg.active_to = color(value, key),
            "inactive_color" | "inactive_from" => cfg.inactive_from = color(value, key),
            "inactive_color_to" | "inactive_to" => cfg.inactive_to = color(value, key),
            "urgent_color" | "urgent_from" => cfg.urgent_from = color(value, key),
            "urgent_color_to" | "urgent_to" => cfg.urgent_to = color(value, key),
            "angle" | "gradient_angle" => {
                cfg.angle = parse_f32_clamped(value, -3600.0, 3600.0, "border.angle");
            }
            "opacity" | "active_opacity" => {
                cfg.opacity = parse_f32_clamped(value, 0.0, 1.0, "border.opacity");
            }
            "inactive_opacity" => {
                cfg.inactive_opacity =
                    parse_f32_clamped(value, 0.0, 1.0, "border.inactive_opacity");
            }
            "urgent_opacity" => {
                cfg.urgent_opacity = parse_f32_clamped(value, 0.0, 1.0, "border.urgent_opacity");
            }
            "animate" | "animated" => match parse_bool(value) {
                Some(value) => cfg.animate = Some(value),
                None => tracing::warn!(value, "Expected border.animate boolean, ignoring"),
            },
            "animate_focused" | "animate_active" | "animate_on_focus" => match parse_bool(value) {
                Some(value) => cfg.animate_focused = Some(value),
                None => tracing::warn!(value, "Expected border.animate_focused boolean, ignoring"),
            },
            "animate_inactive" | "animate_unfocused" => match parse_bool(value) {
                Some(value) => cfg.animate_inactive = Some(value),
                None => tracing::warn!(value, "Expected border.animate_inactive boolean, ignoring"),
            },
            "animate_urgent" => match parse_bool(value) {
                Some(value) => cfg.animate_urgent = Some(value),
                None => tracing::warn!(value, "Expected border.animate_urgent boolean, ignoring"),
            },
            "inactive_enabled" | "show_inactive" => match parse_bool(value) {
                Some(value) => cfg.inactive_enabled = Some(value),
                None => tracing::warn!(value, "Expected border.inactive_enabled boolean, ignoring"),
            },
            "focus_only" | "active_only" => match parse_bool(value) {
                Some(value) => cfg.inactive_enabled = Some(!value),
                None => tracing::warn!(value, "Expected border.focus_only boolean, ignoring"),
            },
            "animation_speed" | "rotation_speed" => {
                cfg.animation_speed =
                    parse_f32_clamped(value, -720.0, 720.0, "border.animation_speed");
            }
            "pulse_amount" | "pulse" => {
                cfg.pulse_amount = parse_f32_clamped(value, 0.0, 1.0, "border.pulse_amount");
            }
            "pulse_speed" => {
                cfg.pulse_speed = parse_f32_clamped(value, 0.0, 20.0, "border.pulse_speed");
            }
            "radius_offset" => {
                cfg.radius_offset = parse_f32_clamped(value, -128.0, 128.0, "border.radius_offset");
            }
            "antialias" | "softness" => {
                cfg.antialias = parse_f32_clamped(value, 0.0, 8.0, "border.antialias");
            }
            "floating_only" => match parse_bool(value) {
                Some(value) => cfg.floating_only = Some(value),
                None => tracing::warn!(value, "Expected border.floating_only boolean, ignoring"),
            },
            "fullscreen" => match parse_bool(value) {
                Some(value) => cfg.fullscreen = Some(value),
                None => tracing::warn!(value, "Expected border.fullscreen boolean, ignoring"),
            },
            _ => tracing::warn!(key, "Unknown border key, ignoring"),
        }
    }
}

fn apply_popup_block(cfg: &mut PopupConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `popup` block, ignoring");
            continue;
        };
        match key.as_str() {
            "border_width" | "border-width" => {
                cfg.border_width = parse_f32_clamped(value, 0.0, 8.0, "popup.border_width");
            }
            "border_color" | "border-color" => {
                cfg.border_color = parse_rgba_color(value).or_else(|| {
                    tracing::warn!(
                        value,
                        "Expected popup.border_color as an RGBA color, ignoring"
                    );
                    None
                });
            }
            "radius" => {
                cfg.radius = parse_f32_clamped(value, 0.0, 64.0, "popup.radius");
            }
            _ => tracing::warn!(key, "Unknown popup key, ignoring"),
        }
    }
}

fn parse_shapes(value: &str) -> Option<Vec<RippleShape>> {
    let mut out = Vec::new();
    let tokens = parse_list_value(value)
        .unwrap_or_else(|| value.split_whitespace().map(str::to_string).collect());
    for tok in tokens {
        match tok.as_str() {
            "ring" => out.push(RippleShape::Ring),
            "square" => out.push(RippleShape::Square),
            "droplet" => out.push(RippleShape::Droplet),
            "cross" => out.push(RippleShape::Cross),
            _ => return None,
        }
    }
    Some(out)
}

fn parse_ripple_preset_selection(value: &str) -> Option<RipplePresetSelection> {
    let name = value.trim().to_lowercase();
    let built_in = match name.as_str() {
        "water-drop" | "waterdrop" | "drop" | "aqua" => Some(RipplePreset::WaterDrop),
        "jelly" | "jiggle" | "giggle" | "wobble" => Some(RipplePreset::Jelly),
        "bubble" => Some(RipplePreset::Bubble),
        "splash" | "crown" => Some(RipplePreset::Splash),
        "tide" | "waves" | "wave" => Some(RipplePreset::Tide),
        "legacy" | "shapes" => Some(RipplePreset::Legacy),
        _ => None,
    };
    built_in
        .map(RipplePresetSelection::BuiltIn)
        .or_else(|| valid_ripple_preset_name(&name).then_some(RipplePresetSelection::Named(name)))
}

fn valid_ripple_preset_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn parse_ripple_size_mode(value: &str) -> Option<RippleSizeMode> {
    match value.trim().to_lowercase().as_str() {
        "fixed" | "pixels" | "px" => Some(RippleSizeMode::Fixed),
        "window" | "diagonal" => Some(RippleSizeMode::Window),
        "width" => Some(RippleSizeMode::Width),
        "height" => Some(RippleSizeMode::Height),
        "min" | "minimum" | "shortest" | "min-dimension" => Some(RippleSizeMode::MinDimension),
        "max" | "maximum" | "longest" | "max-dimension" => Some(RippleSizeMode::MaxDimension),
        _ => None,
    }
}

fn parse_triggers(value: &str) -> Option<Vec<RippleTrigger>> {
    let mut out = Vec::new();
    for tok in value.split_whitespace() {
        match tok {
            "map" => out.push(RippleTrigger::Map),
            "focus" => out.push(RippleTrigger::Focus),
            "urgent" => out.push(RippleTrigger::Urgent),
            _ => return None,
        }
    }
    Some(out)
}

fn parse_anchor(value: &str) -> Option<RippleAnchor> {
    match value {
        "center" => Some(RippleAnchor::Center),
        "cursor" => Some(RippleAnchor::Cursor),
        "top" | "top-edge" => Some(RippleAnchor::Top),
        "bottom" | "bottom-edge" => Some(RippleAnchor::Bottom),
        "left" | "left-edge" => Some(RippleAnchor::Left),
        "right" | "right-edge" => Some(RippleAnchor::Right),
        "nearest-edge" | "nearest" | "edge" => Some(RippleAnchor::NearestEdge),
        "topleft" | "top-left" => Some(RippleAnchor::TopLeft),
        "topright" | "top-right" => Some(RippleAnchor::TopRight),
        "bottomleft" | "bottom-left" => Some(RippleAnchor::BottomLeft),
        "bottomright" | "bottom-right" => Some(RippleAnchor::BottomRight),
        _ => None,
    }
}

fn parse_layer(value: &str) -> Option<RippleLayer> {
    match value {
        "above-windows" => Some(RippleLayer::AboveWindows),
        "below-windows" => Some(RippleLayer::BelowWindows),
        "above-all" => Some(RippleLayer::AboveAll),
        "below-all" => Some(RippleLayer::BelowAll),
        _ => None,
    }
}

fn parse_ease(value: &str) -> Option<RippleEase> {
    match value {
        "linear" => Some(RippleEase::Linear),
        "cubic-out" => Some(RippleEase::CubicOut),
        "cubic-in-out" => Some(RippleEase::CubicInOut),
        "quad-out" => Some(RippleEase::QuadOut),
        "exp-out" => Some(RippleEase::ExpOut),
        _ => None,
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
        match entry {
            waves::Entry::Assign(key, value) => match key.as_str() {
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
                "pid" => match value.parse() {
                    Ok(n) => rule.pid = Some(n),
                    Err(_) => tracing::warn!(value, "Expected an integer PID, ignoring"),
                },
                "xwayland" | "is_xwayland" => set_opt_bool(&mut rule.is_xwayland, key, value),
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
                "swallow" => set_bool(&mut rule.swallow, key, value),
                "opacity" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => rule.opacity = Some(value.clamp(0.0, 1.0)),
                    _ => {
                        tracing::warn!(value, "Expected a finite opacity from 0.0 to 1.0, ignoring")
                    }
                },
                "active_opacity" | "focused_opacity" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => {
                        rule.active_opacity = Some(value.clamp(0.0, 1.0))
                    }
                    _ => tracing::warn!(value, "Expected active_opacity from 0.0 to 1.0, ignoring"),
                },
                "inactive_opacity" | "unfocused_opacity" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => {
                        rule.inactive_opacity = Some(value.clamp(0.0, 1.0))
                    }
                    _ => {
                        tracing::warn!(value, "Expected inactive_opacity from 0.0 to 1.0, ignoring")
                    }
                },
                "fullscreen_opacity" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => {
                        rule.fullscreen_opacity = Some(value.clamp(0.0, 1.0))
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected fullscreen_opacity from 0.0 to 1.0, ignoring"
                    ),
                },
                "glass" | "glass_mode" => match value.as_str() {
                    "water" => rule.glass = Some(GlassMode::Water),
                    "frost" => rule.glass = Some(GlassMode::Frost),
                    "none" | "plain" => rule.glass = Some(GlassMode::Plain),
                    _ => tracing::warn!(
                        value,
                        "Expected a rule glass mode: water frost none, ignoring"
                    ),
                },
                "viscosity" => match parse_viscosity(value) {
                    Some(value) => rule.viscosity = Some(value),
                    None => {
                        tracing::warn!(value, "Expected rule viscosity from 0.0 to 4.0, ignoring")
                    }
                },
                "sway" => set_opt_bool(&mut rule.sway, key, value),
                "float_physics" => match value.as_str() {
                    "off" | "false" => rule.float_physics = Some(FloatPhysicsTier::Off),
                    "light" | "true" => rule.float_physics = Some(FloatPhysicsTier::Light),
                    "full" => rule.float_physics = Some(FloatPhysicsTier::Full),
                    _ => tracing::warn!(
                        value,
                        "Expected a rule float_physics tier: off light full, ignoring"
                    ),
                },
                "depth" => set_opt_bool(&mut rule.depth, key, value),
                "shadow" => match value.as_str() {
                    "true" | "on" => {
                        rule.shadow
                            .get_or_insert_with(ShadowOverrides::default)
                            .enabled = Some(true);
                    }
                    "false" | "off" | "none" => {
                        rule.shadow
                            .get_or_insert_with(ShadowOverrides::default)
                            .enabled = Some(false);
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected rule shadow to be true/false/on/off/none, ignoring"
                    ),
                },
                "rounding" => {
                    let overrides = rule.rounding.get_or_insert_with(RoundingOverrides::default);
                    match value.trim().to_ascii_lowercase().as_str() {
                        "true" | "on" => overrides.enabled = Some(true),
                        "false" | "off" | "none" => overrides.enabled = Some(false),
                        _ => match parse_corner_radii(value) {
                            Some([tl, tr, br, bl]) => {
                                overrides.enabled = Some(true);
                                overrides.top_left = Some(tl);
                                overrides.top_right = Some(tr);
                                overrides.bottom_right = Some(br);
                                overrides.bottom_left = Some(bl);
                            }
                            None => tracing::warn!(
                                value,
                                "Expected rule rounding on|off or one/four radii, ignoring"
                            ),
                        },
                    }
                }
                "clip_to_geometry" | "clip-to-geometry" => match parse_bool(value) {
                    Some(value) => {
                        rule.rounding
                            .get_or_insert_with(RoundingOverrides::default)
                            .clip = Some(value);
                    }
                    None => tracing::warn!(value, "Expected clip_to_geometry boolean, ignoring"),
                },
                "border" => match value.as_str() {
                    "true" | "on" => {
                        rule.border
                            .get_or_insert_with(BorderOverrides::default)
                            .enabled = Some(true);
                    }
                    "false" | "off" | "none" => {
                        rule.border
                            .get_or_insert_with(BorderOverrides::default)
                            .enabled = Some(false);
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected rule border true/false/on/off/none, ignoring"
                    ),
                },
                "position" => match parse_position(value) {
                    Some(pos) => rule.position = Some(pos),
                    None => {
                        tracing::warn!(value, "Expected <x>x<y> for a rule's position, ignoring")
                    }
                },
                "size" => match parse_position(value) {
                    Some(size) => rule.size = Some(size),
                    None => tracing::warn!(
                        value,
                        "Expected <width>x<height> for a rule's size, ignoring"
                    ),
                },
                "ripple" if value == "none" => {
                    // Shorthand for a rule that matches the window but
                    // suppresses any ripple on it. Equivalent to a full
                    // `ripple { enabled = false }` sub-block.
                    rule.ripple = Some(RippleConfig {
                        enabled: Some(false),
                        ..Default::default()
                    });
                }
                other => tracing::warn!(key = %other, "Unknown key in `rule` block, ignoring"),
            },
            waves::Entry::Block(keyword, _, ripple_body) if keyword == "ripple" => {
                // Per-app ripple overrides. Start from a fresh empty
                // `RippleConfig` (every field `None`) so unset fields
                // inherit the global `ripple { }` block via `merge_over`.
                let mut overrides = RippleConfig::default();
                apply_ripple_block(&mut overrides, ripple_body);
                rule.ripple = Some(overrides);
            }
            waves::Entry::Block(keyword, _, frost_body) if keyword == "frost" => {
                let mut overrides = FrostOverrides::default();
                apply_frost_override_block(&mut overrides, frost_body);
                rule.frost = Some(overrides);
            }
            waves::Entry::Block(keyword, _, shadow_body) if keyword == "shadow" => {
                let mut overrides = ShadowOverrides::default();
                apply_shadow_override_block(&mut overrides, shadow_body);
                rule.shadow = Some(overrides);
            }
            waves::Entry::Block(keyword, _, rounding_body)
                if keyword == "rounding" || keyword == "corners" =>
            {
                let mut overrides = RoundingOverrides::default();
                apply_rounding_override_block(&mut overrides, rounding_body);
                rule.rounding = Some(overrides);
            }
            waves::Entry::Block(keyword, _, border_body) if keyword == "border" => {
                let mut overrides = BorderOverrides::default();
                apply_border_override_block(&mut overrides, border_body);
                rule.border = Some(overrides);
            }
            _ => tracing::warn!("Unexpected entry in `rule` block, ignoring"),
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

fn parse_viscosity(value: &str) -> Option<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 4.0))
}

fn set_opt_f64(field: &mut Option<f64>, key: &str, value: &str) {
    match value.parse() {
        Ok(n) => *field = Some(n),
        Err(_) => tracing::warn!(key, value, "Expected a number, ignoring"),
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
/// submap's bare `Escape = exit-mode`), only the always-active base
/// `[keybinds]` table can silently steal a key from every other window.
fn parse_keybind(
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
fn parse_modifiers(value: &str) -> Option<Mods> {
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
fn parse_simple_chord(combo: &str) -> Option<(Mods, Keysym)> {
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
    if let Some(name) = action.strip_prefix("mode:") {
        return Some(Action::EnterSubmap(name.to_string()));
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
    if let Some(name) = action.strip_prefix("ocean-bookmark:") {
        return (!name.trim().is_empty()).then(|| Action::OceanBookmark(name.trim().to_string()));
    }
    if let Some(name) = action.strip_prefix("ocean-save-bookmark:") {
        return (!name.trim().is_empty())
            .then(|| Action::OceanSaveBookmark(name.trim().to_string()));
    }
    if let Some(name) = action.strip_prefix("focus-app:") {
        return (!name.trim().is_empty()).then(|| Action::FocusApp(name.trim().to_string()));
    }
    if let Some(name) = action.strip_prefix("close-app:") {
        return (!name.trim().is_empty()).then(|| Action::CloseApp(name.trim().to_string()));
    }
    match action {
        "exit-mode" => Some(Action::ExitSubmap),
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
        "toggle-scratchpad" => Some(Action::ToggleScratchpad(None)),
        "move-to-scratchpad" => Some(Action::MoveToScratchpad(None)),
        "toggle-pseudo-tile" => Some(Action::TogglePseudoTile),
        "toggle-float-ambient" => Some(Action::ToggleFloatAmbient),
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

/// Parses `"bsp"`/`"master"`/`"cascade"` into a `LayoutAlgorithm`. Shared by
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
        "cascade" => Some(LayoutAlgorithm::Cascade),
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
            tracing::warn!(
                entry,
                "workspace_gaps needs a workspace and a pixel value, ignoring"
            );
            continue;
        };
        let number = match workspace.parse::<u32>() {
            Ok(n) => Some(n),
            Err(_) => names.get(workspace.trim()).copied(),
        };
        let (Some(number), Ok(pixels)) = (number, pixels.trim().parse::<i32>()) else {
            tracing::warn!(
                entry,
                "Invalid workspace or pixel value in workspace_gaps, ignoring"
            );
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
# Full reference, every action string, and the whole protocol matrix:
# DOCUMENTATION.md in the TideWM repo.
#
# Locked out by a bad edit? Ctrl+Alt+Escape turns on a small always-safe
# keybind set (terminal, close, quit, floating, fullscreen) until the next
# reload or restart.

# ~~~~~~~~~~~~~~~~~ the core ~~~~~~~~~~~~~~~~~

include "keybinds.wave"

@mod = SUPER                    # one modifier, everything hangs off it

# The first-boot hint card on the empty desktop. Set false or delete this
# line entirely to keep it from coming back.
welcome_hint = true

terminal = wave(kitty, alacritty, foot, xterm)
drag_modifier = mod              # left-drag moves a floating window; right-drag resizes
engine = classic                 # classic (workspaces) or ocean (infinite canvas), live-switchable
reload_toast = true

# ~~~~~~~~~~~~~~~~~ the water ~~~~~~~~~~~~~~~~~

# Ripples, wave transitions, glass, viscosity, sway, and everything else
# that makes this TideWM instead of any other tiling WM. One toggle, plus
# a lot of dials that already have good defaults. The full catalog
# (frost/shadow/rounding/border/ripple/depth/ocean/...) is in
# DOCUMENTATION.md, not repeated here.
water_effects = true
viscosity = 1.0                  # 0 turns off drag/resize settling, higher settles slower

# ~~~~~~~~~~~~~~~~~ the layout ~~~~~~~~~~~~~~~~~

gaps = 8
layout = bsp                     # bsp, master, or cascade

# ~~~~~~~~~~~~~~~~~ input ~~~~~~~~~~~~~~~~~

input {
    repeat_delay = 200
    repeat_rate = 25
    focus_follows_mouse = true

    # xkb_layout = us
    # xkb_options = grp:alt_shift_toggle

    touchpad {
        # tap_to_click = true
        # natural_scroll = true
    }
}

# ~~~~~~~~~~~~~~~~~ x11 apps ~~~~~~~~~~~~~~~~~

xwayland {
    enabled = true
    path = xwayland-satellite
}

# ~~~~~~~~~~~~~~~~~ monitors ~~~~~~~~~~~~~~~~~

# TideWM picks a sensible mode/scale per monitor on its own; set this only
# to override one.
# output eDP-1 {
#     mode = 1920x1080@60
#     position = 0x0
#     scale = 1.0
# }

# ~~~~~~~~~~~~~~~~~ autostart & environment ~~~~~~~~~~~~~~~~~

# spawn = [waybar, "swaybg -i ~/wallpaper.png"]

# env {
#     XCURSOR_THEME = Adwaita
# }

# switch_events {
#     lid_close = spawn:systemctl suspend
# }

# ~~~~~~~~~~~~~~~~~ window rules ~~~~~~~~~~~~~~~~~

# Per-app placement and appearance, matched on app_id and/or title (regex
# allowed). Every field a rule can set is in DOCUMENTATION.md.
# rule {
#     app_id = pavucontrol
#     float = true
# }
"#;

/// The keybind half of the shipped default, `include`d by
/// `DEFAULT_CONFIG_WAVE` -- a real split (not just an example in a
/// comment) so a fresh install looks like the two-file layout this
/// project's own docs already recommend. Mirrors `RawConfig::default()`'s
/// hardcoded fallback bind-for-bind (see the `default_layout_keybinds_...`
/// and `default_submap_...` tests below, which assert both agree) --
/// `RawConfig::default()` is the ultimate hard fallback used when no
/// config exists at all or parsing fails outright, so the two must never
/// silently diverge.
const DEFAULT_KEYBINDS_WAVE: &str = r#"# Keybinds. Everything below hangs off @mod (config.wave), so rebinding
# your primary modifier is a one-line change, not a find-and-replace.

# ~~~~~~~~~~~~~~~~~ apps ~~~~~~~~~~~~~~~~~

bind $mod+Return  { spawn:kitty }

# Most setups also want a launcher and a file manager, but both assume a
# tool this config can't verify you have installed -- uncomment yours:
# bind $mod+D  { "spawn:wofi --show drun" }
# bind $mod+E  { spawn:thunar }

# ~~~~~~~~~~~~~~~~~ windows ~~~~~~~~~~~~~~~~~

bind $mod+Q  { close-window }
bind $mod+Shift+Q  { quit }

bind $mod+V  { toggle-floating }
bind $mod+F  { toggle-fullscreen }
bind $mod+M  { toggle-border-fullscreen }
bind $mod+Shift+M  { resize-to-monitor }
bind $mod+P  { toggle-pin }
bind $mod+Shift+P  { toggle-pseudo-tile }

# Focus and swap, vim-style
bind $mod+H  { focus-left }
bind $mod+L  { focus-right }
bind $mod+K  { focus-up }
bind $mod+J  { focus-down }
bind $mod+Shift+H  { swap-left }
bind $mod+Shift+L  { swap-right }
bind $mod+Shift+K  { swap-up }
bind $mod+Shift+J  { swap-down }
bind $mod+Tab  { cycle-focus }

# ~~~~~~~~~~~~~~~~~ tiling ~~~~~~~~~~~~~~~~~

bind $mod+W  { layout:bsp }
bind $mod+Shift+W  { layout:master }
bind $mod+Ctrl+Minus  { master-shrink }
bind $mod+Ctrl+Equal  { master-grow }
bind $mod+O  { toggle-overview }

# Group windows into one tabbed slot
bind $mod+Ctrl+H  { group-left }
bind $mod+Ctrl+L  { group-right }
bind $mod+Ctrl+K  { group-up }
bind $mod+Ctrl+J  { group-down }
bind $mod+Shift+G  { ungroup }
bind $mod+BracketRight  { cycle-tab-next }
bind $mod+BracketLeft  { cycle-tab-prev }

bind $mod+Minus  { toggle-scratchpad }
bind $mod+Shift+Minus  { move-to-scratchpad }

# ~~~~~~~~~~~~~~~~~ workspaces ~~~~~~~~~~~~~~~~~

bind $mod+1  { workspace:1 }
bind $mod+2  { workspace:2 }
bind $mod+3  { workspace:3 }
bind $mod+4  { workspace:4 }
bind $mod+5  { workspace:5 }
bind $mod+6  { workspace:6 }
bind $mod+7  { workspace:7 }
bind $mod+8  { workspace:8 }
bind $mod+9  { workspace:9 }
bind $mod+0  { workspace:10 }
bind $mod+Shift+1  { move-to-workspace:1 }
bind $mod+Shift+2  { move-to-workspace:2 }
bind $mod+Shift+3  { move-to-workspace:3 }
bind $mod+Shift+4  { move-to-workspace:4 }
bind $mod+Shift+5  { move-to-workspace:5 }
bind $mod+Shift+6  { move-to-workspace:6 }
bind $mod+Shift+7  { move-to-workspace:7 }
bind $mod+Shift+8  { move-to-workspace:8 }
bind $mod+Shift+9  { move-to-workspace:9 }
bind $mod+Shift+0  { move-to-workspace:10 }

# ~~~~~~~~~~~~~~~~~ submaps ~~~~~~~~~~~~~~~~~

# A submap (sway/Hyprland's "mode"): a temporary keybind layer, entered
# below, left active until its own exit -- not tied to focus.
bind $mod+N  { submap:nav }
mode nav {
    bind h  { focus-left }
    bind l { focus-right }
    bind k { focus-up }
    bind j { focus-down }
    bind Escape { exit-mode }
}

# ~~~~~~~~~~~~~~~~~ ocean's camera ~~~~~~~~~~~~~~~~~

# Off by default: these chords are ones apps use themselves (Ctrl+arrows
# is word-jump in every editor), and the actions only do anything once
# spatial_engine = ocean in config.wave anyway. Uncomment when you switch.
# bind Ctrl+Left { ocean-pan-left }
# bind Ctrl+Right { ocean-pan-right }
# bind Ctrl+Up = ocean-pan-up
# bind Ctrl+Down = ocean-pan-down
# bind Ctrl+I = ocean-zoom-in
# bind Ctrl+O = ocean-zoom-out
# bind Ctrl+0 = ocean-zoom-reset
# bind $mod+Ctrl+D  { sink-window }
# bind $mod+Ctrl+Shift+D  { ocean-dredge-window }
# bind $mod+Ctrl+Shift+U  { ocean-surface-window }

# ~~~~~~~~~~~~~~~~~ media keys ~~~~~~~~~~~~~~~~~

# Commented out since they assume tools this config can't verify you have
# installed.
# bind Print  { "spawn:grim ~/screenshot.png" }
# bind XF86AudioRaiseVolume = spawn:pactl set-sink-volume @DEFAULT_SINK@ +5%
# bind XF86AudioLowerVolume = spawn:pactl set-sink-volume @DEFAULT_SINK@ -5%
# bind XF86AudioMute = spawn:pactl set-sink-mute @DEFAULT_SINK@ toggle
# bind XF86MonBrightnessUp = spawn:brightnessctl set 10%+
# bind XF86MonBrightnessDown = spawn:brightnessctl set 10%-
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
                "web 16".to_string(), // name resolves via workspace_name
                "nope 4".to_string(), // unknown name, skipped
                "2".to_string(),      // no pixel value, skipped
                "2 lots".to_string(), // bad pixel value, skipped
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
        assert!(by_app_id.matches(Some("firefox"), None, None, false));
        assert!(!by_app_id.matches(Some("firefox-nightly"), None, None, false));
        assert!(!by_app_id.matches(None, Some("Firefox"), None, false));

        let by_title = WindowRule {
            title: Some("Picture-in-Picture".to_string()),
            ..Default::default()
        };
        assert!(by_title.matches(None, Some("Video - picture-in-picture"), None, false));
        assert!(!by_title.matches(None, Some("Video"), None, false));
        assert!(!by_title.matches(None, None, None, false));

        let both = WindowRule {
            app_id: Some("firefox".to_string()),
            title: Some("pip".to_string()),
            ..Default::default()
        };
        assert!(!both.matches(Some("firefox"), Some("normal tab"), None, false));
        assert!(both.matches(Some("firefox"), Some("Video - PIP"), None, false));
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
        assert!(!blank.matches(Some("anything"), Some("anything"), None, false));
        assert!(!blank.matches(None, None, None, false));
    }

    #[test]
    fn window_rule_regexes_compile_once_and_invalid_patterns_are_ignored() {
        let rule = WindowRule {
            app_id_regex: Some(regex::Regex::new(r"^(org\.)?mozilla\.firefox$").unwrap()),
            title_regex: Some(regex::Regex::new("(?i)private browsing").unwrap()),
            ..Default::default()
        };
        assert!(rule.matches(
            Some("org.mozilla.firefox"),
            Some("Private Browsing"),
            None,
            false
        ));
        assert!(!rule.matches(Some("kitty"), Some("Private Browsing"), None, false));

        let invalid =
            lower_window_rule_block(&[waves::Entry::Assign("app_id_regex".into(), "[".into())]);
        assert!(invalid.app_id_regex.is_none());
        assert!(!invalid.matches(Some("anything"), None, None, false));
    }

    #[test]
    fn window_rule_matches_by_pid_and_xwayland_alone() {
        // Neither field needs app_id/title alongside it -- `rule { pid =
        // ... }` and `rule { xwayland = ... }` are each a complete,
        // standalone identifying criterion, same as `app_id`/`title`.
        let by_pid = lower_window_rule_block(&[waves::Entry::Assign("pid".into(), "1234".into())]);
        assert!(by_pid.matches(None, None, Some(1234), false));
        assert!(!by_pid.matches(None, None, Some(5678), false));
        assert!(!by_pid.matches(None, None, None, false));

        let xwayland_only =
            lower_window_rule_block(&[waves::Entry::Assign("xwayland".into(), "true".into())]);
        assert!(xwayland_only.matches(None, None, None, true));
        assert!(!xwayland_only.matches(None, None, None, false));

        let native_only =
            lower_window_rule_block(&[waves::Entry::Assign("xwayland".into(), "false".into())]);
        assert!(native_only.matches(None, None, None, false));
        assert!(!native_only.matches(None, None, None, true));

        // Combines with app_id like any other criterion (AND, not OR).
        let combined = WindowRule {
            app_id: Some("firefox".to_string()),
            pid: Some(42),
            ..Default::default()
        };
        assert!(combined.matches(Some("firefox"), None, Some(42), false));
        assert!(!combined.matches(Some("firefox"), None, Some(99), false));
        assert!(!combined.matches(Some("chromium"), None, Some(42), false));
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
                waves::Entry::Assign("modifier_pan_fingers".into(), "2".into()),
            ],
        );
        assert_eq!(touchpad.gesture_swipe_fingers, Some(3));
        assert!(matches!(touchpad.swipe_left, Some(Action::ToggleOverview)));
        assert_eq!(touchpad.gesture_pinch_fingers, Some(4));
        assert!(matches!(touchpad.pinch_out, Some(Action::CloseWindow)));
        assert_eq!(touchpad.modifier_pan_fingers, Some(2));

        apply_touchpad_block(
            &mut touchpad,
            &[waves::Entry::Assign(
                "modifier_pan_fingers".into(),
                "1".into(),
            )],
        );
        assert_eq!(
            touchpad.modifier_pan_fingers,
            Some(2),
            "below the 2-finger minimum is ignored, not silently accepted"
        );
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
            loaded_entries: Vec::new(),
            terminal: String::new(),
            spatial_engine: SpatialEngine::Classic,
            ocean: OceanConfig::default(),
            pointer_modifier: Mods {
                logo: true,
                ..Default::default()
            },
            show_welcome_hint: false,
            show_config_reload_toast: true,
            water_effects: true,
            viscosity: 1.0,
            connected_vessels: ConnectedVesselsConfig::default(),
            sway: SwayConfig::default(),
            float_physics: FloatPhysicsConfig::default(),
            swim: SwimConfig::default(),
            compass: CompassConfig::default(),
            minimap: MinimapConfig::default(),
            animations: WindowAnimationsConfig::default(),
            workspace_transition: WorkspaceTransitionConfig::default(),
            depth: DepthConfig::default(),
            classic_depth: ClassicDepthConfig::default(),
            frost: FrostConfig::default(),
            water_glass: WaterGlassConfig::default(),
            caustics: CausticsConfig::default(),
            shadow: ShadowConfig::default(),
            rounding: RoundingConfig::default(),
            border: BorderConfig::default(),
            popup: PopupConfig::default(),
            ripple: RippleConfig::system_default(),
            ripple_presets: HashMap::new(),
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
            rescue_keybinds: Vec::new(),
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
        let effective = config.resolve_window_rules(Some("kitty"), None, None, false);
        assert_eq!(effective.workspace, Some(5)); // later match overrides earlier
        assert!(effective.float); // set by the first match, not unset by the second
        assert!(effective.pin); // set by the second match

        config.window_rules.clear();
        let none_matched = config.resolve_window_rules(Some("kitty"), None, None, false);
        assert_eq!(none_matched.workspace, None);
        assert!(!none_matched.float);
    }

    #[test]
    fn ocean_selector_reefs_bookmarks_and_actions_parse_without_resolution_defaults() {
        let entries = wave_entries(
            "spatial_engine = ocean\n\
             ocean {\n\
                 camera_step = 720\n\
                 depth_enabled = false\n\
                 zoom_enabled = true\n\
                 modifier_zoom = false\n\
                 min_zoom = 0.4\n\
                 max_zoom = 2.5\n\
                 zoom_step = 1.3\n\
                 camera_animation_ms = 340\n\
                 camera_sway = 24\n\
                 canvas_guides = false\n\
                 canvas_grid_size = 320\n\
                 canvas_grid_alpha = 0.18\n\
                 canvas_marker = false\n\
                 canvas_marker_fade_ms = 5100\n\
                 reef main {\n\
                     x = -400\n\
                     y = 250\n\
                 }\n\
                 reef deep {\n\
                     x = 0\n\
                     y = 4000\n\
                     width = 3440\n\
                     height = 1440\n\
                 }\n\
                 bookmark code {\n\
                     x = 1234.5\n\
                     y = -80\n\
                 }\n\
             }\n\
             bind Super+Ctrl+H { ocean-pan-left }\n\
             bind Super+Ctrl+I { ocean-zoom-in }\n\
             bind Super+Ctrl+D { sink-window }\n\
             bind Super+Ctrl+Shift+D { ocean-dredge-window }\n\
             bind Super+Ctrl+Shift+U { ocean-surface-window }\n\
             bind Super+1 { ocean-bookmark:code }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;

        assert_eq!(config.spatial_engine, SpatialEngine::Ocean);
        assert_eq!(config.ocean.camera_step, 720);
        assert!(!config.ocean.depth_enabled);
        assert!(config.ocean.zoom_enabled);
        assert!(!config.ocean.modifier_zoom);
        assert_eq!(config.ocean.min_zoom, 0.4);
        assert_eq!(config.ocean.max_zoom, 2.5);
        assert_eq!(config.ocean.zoom_step, 1.3);
        assert_eq!(config.ocean.camera_animation_ms, 340);
        assert_eq!(config.ocean.camera_sway, 24.0);
        assert!(!config.ocean.canvas_guides);
        assert_eq!(config.ocean.canvas_grid_size, 320);
        assert_eq!(config.ocean.canvas_grid_alpha, 0.18);
        assert!(!config.ocean.canvas_marker);
        assert_eq!(config.ocean.canvas_marker_fade_ms, 5100);
        assert_eq!(config.ocean.reefs.len(), 2);
        assert_eq!(config.ocean.reefs[0].width, None);
        assert_eq!(config.ocean.reefs[0].height, None);
        assert_eq!(config.ocean.reefs[1].width, Some(3440));
        assert_eq!(config.ocean.reefs[1].height, Some(1440));
        assert_eq!(config.ocean.bookmarks[0].name, "code");
        assert_eq!(config.ocean.bookmarks[0].x, 1234.5);
        assert!(config
            .keybinds
            .iter()
            .any(|bind| { matches!(bind.action, Action::OceanPan(Direction::Left)) }));
        assert!(config
            .keybinds
            .iter()
            .any(|bind| matches!(bind.action, Action::OceanZoomIn)));
        assert!(config.keybinds.iter().all(|bind| !matches!(
            bind.action,
            Action::SinkWindow | Action::OceanDredgeWindow | Action::OceanSurfaceWindow
        )));
        assert!(config
            .keybinds
            .iter()
            .any(|bind| { matches!(&bind.action, Action::OceanBookmark(name) if name == "code") }));
    }

    #[test]
    fn parsed_keybinds_are_authoritative_and_never_merge_hidden_defaults() {
        let dir = TestDir::new("mod-frees-super");
        let main = dir.write(
            "config.wave",
            "@mod = ALT\n\
             bind $mod+Q { close-window }\n\
             bind Super+F { toggle-fullscreen }\n",
        );

        let (raw, _, _) = load_raw_config(&main).expect("should parse");
        // Only what the file declared exists. No default table is merged
        // underneath it, regardless of which modifier variables it uses.
        assert!(!raw.keybinds.contains_key("Super+Q"));
        assert_eq!(
            raw.keybinds.get("ALT+Q").map(String::as_str),
            Some("close-window")
        );
        // A Super+X the user wrote out themselves is a deliberate dual-bind
        // and must survive even though $mod is off Super.
        assert_eq!(
            raw.keybinds.get("Super+F").map(String::as_str),
            Some("toggle-fullscreen")
        );
        assert!(!raw.keybinds.contains_key("Super+Tab"));
    }

    #[test]
    fn load_raw_config_accepts_wave_syntax_end_to_end() {
        let config = "@mod = SUPER\n\
                   terminal = kitty\n\
                   gaps = 8\n\
                   color = #8EDDFF\n\
                   border {\n\
                       width = 2\n\
                   }\n\
                   input {\n\
                       xkb_layout = us\n\
                   }\n\
                   bind $mod+Q { close-window }\n\
                   spawn = [waybar]\n";

        let dir = TestDir::new("wave-syntax-end-to-end");
        let (raw, warnings, _) =
            load_raw_config(&dir.write("config.wave", config)).expect("should evaluate");

        assert!(warnings.is_empty(), "should not warn: {warnings:?}");
        assert_eq!(raw.gaps, 8);
        assert_eq!(raw.terminal, "kitty");
        assert_eq!(raw.spawn_at_startup, vec!["waybar".to_string()]);
        assert_eq!(
            raw.keybinds.get("SUPER+Q").map(String::as_str),
            Some("close-window")
        );
        assert!(!raw.keybinds.contains_key("$mod+Q"));
    }

    #[test]
    fn load_raw_config_new_syntax_error_is_authoritative() {
        // @mod is new-only syntax, so the Wave parser's error must
        // surface. A fallback to the old grammar would silently misparse
        // `gaps = $extra` as a raw string.
        let dir = TestDir::new("wave-error-authoritative");
        let main = dir.write(
            "config.wave",
            "@mod = SUPER\nbind $mod+Q { close-window }\ngaps = $extra\n",
        );
        let err = load_raw_config(&main).expect_err("new-syntax errors must not fall back");
        assert!(err.contains("`$extra` is not defined"), "{err}");
    }

    #[test]
    fn diff_entries_reports_changed_keys_binds_and_blocks() {
        use waves::Entry;
        let old = vec![
            Entry::VarDef("mod".into(), "SUPER".into()),
            Entry::Assign("gaps".into(), "8".into()),
            Entry::Assign("terminal".into(), "kitty".into()),
            Entry::Bind("SUPER+Q".into(), "close-window".into()),
            Entry::Bind("SUPER+F".into(), "toggle-fullscreen".into()),
            Entry::Block(
                "border".into(),
                "".into(),
                vec![Entry::Assign("width".into(), "2".into())],
            ),
        ];
        let new = vec![
            Entry::VarDef("mod".into(), "SUPER".into()),
            Entry::Assign("gaps".into(), "12".into()),
            Entry::Bind("SUPER+Q".into(), "close-window".into()),
            Entry::Bind("SUPER+F".into(), "toggle-floating".into()),
            Entry::Bind("SUPER+D".into(), "spawn:rofi".into()),
            Entry::Block(
                "border".into(),
                "".into(),
                vec![Entry::Assign("width".into(), "3".into())],
            ),
        ];

        let diff = diff_entries(&old, &new);
        assert_eq!(
            diff.keys_changed,
            vec![
                ("gaps".to_string(), "8".to_string(), "12".to_string()),
                ("terminal".to_string(), "kitty".to_string(), String::new()),
            ]
        );
        assert_eq!(
            diff.binds_added,
            vec![("SUPER+D".to_string(), "spawn:rofi".to_string())]
        );
        assert_eq!(diff.binds_removed, vec![]);
        assert_eq!(
            diff.binds_changed,
            vec![(
                "SUPER+F".to_string(),
                "toggle-fullscreen".to_string(),
                "toggle-floating".to_string()
            )]
        );
        assert_eq!(
            diff.blocks_changed,
            vec![("border".to_string(), String::new())]
        );
        assert!(!diff.is_empty());
        let summary = diff.summary();
        assert!(summary.contains("2 keys changed"), "{summary}");
        assert!(summary.contains("binds +1 -0 ~1"), "{summary}");
        assert!(summary.contains("blocks changed (border)"), "{summary}");
    }

    #[test]
    fn diff_entries_is_empty_for_identical_lists() {
        use waves::Entry;
        let entries = vec![
            Entry::Assign("gaps".into(), "8".into()),
            Entry::Bind("SUPER+Q".into(), "close-window".into()),
        ];
        let diff = diff_entries(&entries, &entries);
        assert!(diff.is_empty());
        assert_eq!(diff.summary(), "nothing changed");
    }

    #[test]
    fn reload_of_unchanged_file_diffs_empty_and_change_is_detected() {
        fn load_from(path: &Path) -> Config {
            let (raw, _, entries) = load_raw_config(path).expect("should parse");
            let (mut config, _) = Config::from_raw(raw);
            config.loaded_entries = entries;
            config
        }
        let dir = TestDir::new("wave-reload-diff");
        let main = dir.write("config.wave", "gaps = 8\nterminal = kitty\n");
        let first = load_from(&main);
        let second = load_from(&main);
        assert!(diff_entries(&first.loaded_entries, &second.loaded_entries).is_empty());

        dir.write("config.wave", "gaps = 12\nterminal = kitty\n");
        let third = load_from(&main);
        let diff = diff_entries(&first.loaded_entries, &third.loaded_entries);
        assert_eq!(
            diff.keys_changed,
            vec![("gaps".to_string(), "8".to_string(), "12".to_string())]
        );
    }

    #[test]
    fn duration_values_parse_to_ms_everywhere() {
        assert_eq!(parse_duration_ms("600"), Some(600));
        assert_eq!(parse_duration_ms("600ms"), Some(600));
        assert_eq!(parse_duration_ms("1.5s"), Some(1500));
        assert_eq!(parse_duration_ms("2m"), Some(120_000));
        assert_eq!(parse_duration_ms("0"), None);
        assert_eq!(parse_duration_ms("nope"), None);

        let dir = TestDir::new("wave-durations");
        let main = dir.write(
            "config.wave",
            "cursor_hide_after = 2s\n\
             transition {\n\
                 duration = 600ms\n\
                 workspace_motion_delay = 150ms\n\
             }\n",
        );
        let (raw, _, _) = load_raw_config(&main).expect("should parse");
        assert_eq!(raw.cursor_hide_after_ms, 2000);
        let transition = &raw.workspace_transition;
        assert_eq!(transition.duration_ms, 600);
        assert_eq!(transition.workspace_motion_delay_ms, 150);
    }

    #[test]
    fn spawn_list_lowers_to_multiple_entries() {
        let dir = TestDir::new("wave-spawn-list");
        let main = dir.write(
            "config.wave",
            "spawn = [waybar, \"swaybg -i ~/wallpaper.png -m fill\"]\n",
        );
        let (raw, _, _) = load_raw_config(&main).expect("should parse");
        assert_eq!(
            raw.spawn_at_startup,
            vec![
                "waybar".to_string(),
                "swaybg -i ~/wallpaper.png -m fill".to_string()
            ]
        );
        // legacy scalar spelling still works
        let dir = TestDir::new("wave-spawn-scalar");
        let main = dir.write("config.wave", "spawn = [waybar]\n");
        let (raw, _, _) = load_raw_config(&main).expect("should parse");
        assert_eq!(raw.spawn_at_startup, vec!["waybar".to_string()]);
    }

    #[test]
    fn wave_palette_lowers_end_to_end() {
        let dir = TestDir::new("wave-palette");
        let main = dir.write(
            "config.wave",
            "theme {\n\
                 primary = #8EDDFF\n\
                 deep = primary.darken(0.35)\n\
                 highlight = primary.lighten(0.15)\n\
             }\n\
             border {\n\
                 active_from = theme.primary\n\
                 active_to = theme.deep\n\
             }\n",
        );
        let (raw, warnings, _) = load_raw_config(&main).expect("should parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        let border = &raw.border;
        assert_eq!(border.active_from[0], 0x8E as f32 / 255.0);
        assert_eq!(border.active_from[1], 0xDD as f32 / 255.0);
        assert_eq!(border.active_from[2], 0xFF as f32 / 255.0);
        // deep = 8EDDFF * 0.65 -> 5C90A6
        assert_eq!(border.active_to[0], 0x5C as f32 / 255.0);
        assert_eq!(border.active_to[1], 0x90 as f32 / 255.0);
        assert_eq!(border.active_to[2], 0xA6 as f32 / 255.0);
    }

    #[test]
    fn hardware_conditionals_see_the_tide_table() {
        let lua = mlua::Lua::new_with(
            mlua::StdLib::MATH | mlua::StdLib::STRING | mlua::StdLib::TABLE,
            mlua::LuaOptions::default(),
        )
        .unwrap();
        let config = "@mod = SUPER\n\
                      if tide.backend == \"udev\" and tide.gpu.vendor == \"nvidia\" then\n\
                          udev {\n\
                              disable_overlay_planes = true\n\
                          }\n\
                      end\n\
                      gaps = 8\n";
        let dir = TestDir::new("wave-tide");
        let main = dir.write("config.wave", config);

        // Nvidia on the real backend: the workaround applies.
        let tide = wave::TideInfo {
            backend: "udev",
            gpu_vendor: "nvidia",
            ..Default::default()
        };
        let (raw, warnings, _) = load_raw_config_in(&lua, &tide, &main).expect("should parse");
        assert!(warnings.is_empty(), "{warnings:?}");

        // AMD: the conditional is false, so no udev block and no warning.
        let tide = wave::TideInfo {
            backend: "udev",
            gpu_vendor: "amd",
            ..Default::default()
        };
        let (raw_amd, _, _) = load_raw_config_in(&lua, &tide, &main).expect("should parse");
        assert_eq!(raw_amd.gaps, raw.gaps);
    }

    #[test]
    fn config_globals_persist_on_the_session_lua_for_eval() {
        let lua = mlua::Lua::new_with(
            mlua::StdLib::MATH | mlua::StdLib::STRING | mlua::StdLib::TABLE,
            mlua::LuaOptions::default(),
        )
        .unwrap();
        let tide = wave::TideInfo {
            backend: "winit",
            ..Default::default()
        };
        let dir = TestDir::new("wave-eval-globals");
        let main = dir.write(
            "config.wave",
            "@mod = SUPER\nterminal = wave(sh)\ntheme {\n    primary = #8EDDFF\n}\n",
        );
        load_raw_config_in(&lua, &tide, &main).expect("should parse");
        // After the load, the session Lua still answers queries:
        // @mod is a global, theme is a section table, wave() resolved.
        assert_eq!(
            lua.load("mod").eval::<String>().expect("mod global"),
            "SUPER"
        );
        // Section tables and colors survive as values on the session Lua.
        let primary = lua.load("theme.primary").eval::<mlua::Value>().unwrap();
        assert_eq!(
            wave::lua_value_to_json(primary).expect("color as JSON"),
            serde_json::json!("8EDDFF")
        );
        // The tide table carries the loader's facts.
        assert_eq!(lua.load("tide.backend").eval::<String>().unwrap(), "winit");
    }

    #[test]
    fn rename_map_canonical_and_legacy_names_lower_identically() {
        let dir = TestDir::new("wave-renames");
        let new = dir.write(
            "config.wave",
            "engine = classic\n\
             drag_modifier = SUPER\n\
             welcome_hint = true\n\
             reload_toast = true\n\
             auto_back_and_forth = true\n\
             layout = master\n\
             master_side = top\n\
             split_bias = horizontal\n\
             transition {\n\
                 duration = 600ms\n\
             }\n\
             vessels {\n\
                 enabled = true\n\
             }\n\
             glass {\n\
                 tint_alpha = 0.1\n\
             }\n\
             physics {\n\
                 tier = light\n\
             }\n\
             mode nav {\n\
                 bind h { focus-left }\n\
             }\n",
        );
        let (raw_new, warnings, _) = load_raw_config(&new).expect("new names should parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(raw_new.spatial_engine, "classic");
        assert_eq!(raw_new.pointer_modifier, "SUPER");
        assert!(raw_new.show_welcome_hint);
        assert!(raw_new.show_config_reload_toast);
        assert!(raw_new.workspace_auto_back_and_forth);
        assert_eq!(raw_new.default_layout, "master");
        assert_eq!(raw_new.master_orientation, "top");
        assert_eq!(raw_new.bsp_split_bias, "horizontal");
        assert!(raw_new.submaps.contains_key("nav"));

        // Legacy spellings are gone: they warn as unknown keys and are
        // ignored, never silently applied.
        let dir = TestDir::new("wave-renames-rejected");
        let old = dir.write(
            "config.wave",
            "spatial_engine = classic\n\
             default_layout = master\n",
        );
        let (raw, _, _) = load_raw_config(&old).expect("config still loads");
        assert!(
            raw.default_layout.is_empty(),
            "legacy default_layout must be ignored"
        );
        assert_eq!(
            raw.spatial_engine, "classic",
            "the default engine must still apply"
        );
    }

    #[test]
    fn mode_actions_alias_submap_actions() {
        let parsed = |s: &str| parse_action(s).expect("should parse");
        assert!(matches!(parsed("mode:nav"), Action::EnterSubmap(n) if n == "nav"));
        assert!(matches!(parsed("exit-mode"), Action::ExitSubmap));
    }

    #[test]
    fn wave_function_resolves_to_the_first_real_command() {
        // /bin/sh always exists on any system that can even run this test
        // suite; "definitely-not-a-real-binary" never will. First match
        // wins over an earlier miss, not just the first candidate overall.
        let dir = TestDir::new("wave-fallback");
        let main = dir.write(
            "config.wave",
            "terminal = wave(\"definitely-not-a-real-binary\", \"/bin/sh\", \"kitty\")\n",
        );

        let (raw, _, _) = load_raw_config(&main).expect("should parse");
        assert_eq!(raw.terminal, "/bin/sh");
    }

    /// Sets up an isolated directory under the system temp dir for a single
    /// test, cleaned up on drop -- these tests exercise real file I/O
    /// (`load_raw_config` resolving `include` paths relative to the file
    /// doing the including), which in-memory AST construction can't cover.
    /// Evaluates a Wave config to its entry list (the line-based
    /// `waves::parse` was removed with the old grammar).
    fn wave_entries(s: &str) -> Vec<waves::Entry> {
        wave::evaluate(s, Path::new("test.wave")).expect("config should evaluate")
    }

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
        dir.write("keybinds.wave", "bind Super+Q { close-window }\n");
        let main = dir.write(
            "config.wave",
            "include \"keybinds.wave\"\nterminal = kitty\nbind Super+F { toggle-fullscreen }\n",
        );

        let (raw, _, _) = load_raw_config(&main).expect("include chain should resolve");

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

        let (raw, _, _) = load_raw_config(&main).unwrap();
        assert_eq!(raw.gaps, 12);
    }

    #[test]
    fn load_raw_config_skips_a_missing_include_instead_of_failing_the_whole_load() {
        let dir = TestDir::new("missing-include");
        let main = dir.write(
            "config.wave",
            "include \"does-not-exist.wave\"\nterminal = kitty\n",
        );

        let (raw, warnings, _) =
            load_raw_config(&main).expect("a bad include must not fail the top-level file");
        assert_eq!(raw.terminal, "kitty");
        // The failure must reach the caller, not just the log -- this is
        // what the compositor's warning panel reads to know something's
        // wrong (see `Config::load_with_error`/`Config::reload`).
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("does-not-exist.wave"));
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

        let (raw, warnings, _) =
            load_raw_config(&a).expect("a cycle is skipped with a warning, not a hard failure");
        assert_eq!(raw.terminal, "kitty");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("include cycle"));
    }

    /// Writes both `DEFAULT_CONFIG_WAVE` and `DEFAULT_KEYBINDS_WAVE` to a
    /// real temp directory and loads them through `load_raw_config`, the
    /// exact code path a real first boot uses (see `load_with_error`) --
    /// needed for real disk I/O now that the shipped default genuinely
    /// `include`s a second file, unlike the old single-file default that
    /// `waves::parse` alone could handle in memory.
    fn parse_default_config() -> Config {
        // Every caller shares one PID, so a fixed name would collide across
        // the many test functions that call this concurrently -- thread id
        // is stable within one call but unique across the parallel test
        // threads that actually race on it.
        let dir = TestDir::new(&format!("default-config-{:?}", std::thread::current().id()));
        dir.write("keybinds.wave", DEFAULT_KEYBINDS_WAVE);
        let main = dir.write("config.wave", DEFAULT_CONFIG_WAVE);
        let (raw, _, _) =
            load_raw_config(&main).expect("the shipped default must parse and resolve");
        Config::from_raw(raw).0
    }

    #[test]
    fn pointer_modifier_fallback_and_generated_default_both_use_super() {
        assert_eq!(
            Config::from_raw(RawConfig::default()).0.pointer_modifier,
            Mods {
                logo: true,
                ..Default::default()
            }
        );
        assert_eq!(
            parse_default_config().pointer_modifier,
            Mods {
                logo: true,
                ..Default::default()
            }
        );

        let entries = wave_entries("@mod = ALT\npointer_modifier = mod .. \"+SHIFT\"\n");
        let raw = lower_entries(&entries);
        assert_eq!(
            Config::from_raw(raw).0.pointer_modifier,
            Mods {
                alt: true,
                shift: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn ocean_direct_manipulation_and_reload_card_are_user_configurable() {
        let entries = wave_entries(
            "reload_toast = false\n\
             ocean {\n\
                 freeform_windows = false\n\
                 smart_tiling = true\n\
                 smart_tiling_snap_distance = 96\n\
                 smart_tiling_preserve_size = false\n\
                 canvas_pan_button = middle\n\
                 canvas_pan_requires_modifier = true\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert!(!config.show_config_reload_toast);
        assert!(!config.ocean.freeform_windows);
        assert!(config.ocean.smart_tiling);
        assert_eq!(config.ocean.smart_tiling_snap_distance, 96);
        assert!(!config.ocean.smart_tiling_preserve_size);
        assert_eq!(config.ocean.canvas_pan_button, OceanPanButton::Middle);
        assert!(config.ocean.canvas_pan_requires_modifier);

        let defaults = parse_default_config();
        assert!(defaults.show_config_reload_toast);
        assert!(defaults.ocean.freeform_windows);
        assert!(defaults.ocean.smart_tiling);
        assert_eq!(defaults.ocean.smart_tiling_snap_distance, 64);
        assert!(defaults.ocean.smart_tiling_preserve_size);
        assert_eq!(defaults.ocean.canvas_pan_button, OceanPanButton::Left);
        assert!(!defaults.ocean.canvas_pan_requires_modifier);
    }

    #[test]
    fn viscosity_parses_clamps_and_uses_the_last_matching_rule() {
        let entries = wave_entries(
            "viscosity = 1.75\n\
             rule {\n\
             app_id = kitty\n\
             viscosity = 0.4\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             viscosity = 9\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert_eq!(config.viscosity, 1.75);
        assert_eq!(
            config
                .resolve_window_rules(Some("kitty"), None, None, false)
                .viscosity,
            Some(4.0)
        );
        assert_eq!(
            config
                .resolve_window_rules(Some("foot"), None, None, false)
                .viscosity,
            None
        );
        assert_eq!(parse_default_config().viscosity, 1.0);
    }

    #[test]
    fn connected_vessels_block_parses_clamps_and_matches_generated_defaults() {
        let entries = wave_entries(
            "vessels {\n\
             enabled = false\n\
             falloff = 1.7\n\
             max_splits = 7\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert!(!config.connected_vessels.enabled);
        assert_eq!(config.connected_vessels.falloff, 1.0);
        assert_eq!(config.connected_vessels.max_splits, 7);

        let defaults = parse_default_config().connected_vessels;
        assert!(defaults.enabled);
        assert_eq!(defaults.falloff, 0.5);
        assert_eq!(defaults.max_splits, 4);
    }

    #[test]
    fn swim_block_parses_clamps_and_matches_generated_defaults() {
        let entries = wave_entries(
            "swim {\n\
             enabled = true\n\
             neighbors = 9\n\
             response = 12.0\n\
             snap_duration_ms = 99999\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert!(config.swim.enabled);
        assert_eq!(config.swim.neighbors, 4);
        assert_eq!(config.swim.response, 4.0);
        assert_eq!(config.swim.snap_duration_ms, 2000);

        let defaults = parse_default_config().swim;
        assert!(!defaults.enabled);
        assert_eq!(defaults.neighbors, 1);
        assert_eq!(defaults.response, 1.0);
        assert_eq!(defaults.snap_duration_ms, 220);
    }

    #[test]
    fn sway_block_parses_clamps_rules_and_matches_generated_defaults() {
        let entries = wave_entries(
            "sway {\n\
             enabled = true\n\
             response = 2.5\n\
             max_offset = 999\n\
             frequency = 0\n\
             damping = 42\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             sway = false\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert!(config.sway.enabled);
        assert_eq!(config.sway.response, 1.0);
        assert_eq!(config.sway.max_offset, 128.0);
        assert_eq!(config.sway.frequency, 0.1);
        assert_eq!(config.sway.damping, 20.0);
        assert_eq!(
            config
                .resolve_window_rules(Some("kitty"), None, None, false)
                .sway,
            Some(false)
        );
        assert_eq!(
            config
                .resolve_window_rules(Some("foot"), None, None, false)
                .sway,
            None
        );

        let defaults = parse_default_config().sway;
        assert!(!defaults.enabled);
        assert_eq!(defaults, SwayConfig::default());
    }

    #[test]
    fn float_physics_block_parses_clamps_rules_and_matches_generated_defaults() {
        let entries = wave_entries(
            "physics {\n\
             tier = full\n\
             response = 2.5\n\
             max_offset = 999\n\
             frequency = 0\n\
             damping = 42\n\
             bob_ratio = -1\n\
             radius = 99999\n\
             falloff = false\n\
             ambient_period_s = 999\n\
             restitution = 5\n\
             bounce_off_edges = false\n\
             wave {\n\
             enabled = false\n\
             amplitude = 999\n\
             wavelength = 1\n\
             speed = 99999\n\
             }\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             float_physics = off\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert_eq!(config.float_physics.tier, FloatPhysicsTier::Full);
        assert_eq!(config.float_physics.response, 1.0);
        assert_eq!(config.float_physics.max_offset, 128.0);
        assert_eq!(config.float_physics.frequency, 0.1);
        assert_eq!(config.float_physics.damping, 20.0);
        assert_eq!(config.float_physics.bob_ratio, 0.0);
        assert_eq!(config.float_physics.radius, 2048.0);
        assert!(!config.float_physics.falloff);
        assert_eq!(config.float_physics.ambient_period_s, 60.0);
        assert_eq!(config.float_physics.restitution, 1.0);
        assert!(!config.float_physics.bounce_off_edges);
        assert!(!config.float_physics.wave.enabled);
        assert_eq!(config.float_physics.wave.amplitude, 128.0);
        assert_eq!(config.float_physics.wave.wavelength, 10.0);
        assert_eq!(config.float_physics.wave.speed, 2000.0);
        assert_eq!(
            config
                .resolve_window_rules(Some("kitty"), None, None, false)
                .float_physics,
            Some(FloatPhysicsTier::Off)
        );
        assert_eq!(
            config
                .resolve_window_rules(Some("foot"), None, None, false)
                .float_physics,
            None
        );

        // Legacy true/false aliases still map onto light/off, both globally
        // and per rule.
        let legacy = wave_entries(
            "physics {\n\
             enabled = true\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             float_physics = true\n\
             }\n",
        );
        let legacy_config = Config::from_raw(lower_entries(&legacy)).0;
        assert_eq!(legacy_config.float_physics.tier, FloatPhysicsTier::Light);
        assert_eq!(
            legacy_config
                .resolve_window_rules(Some("kitty"), None, None, false)
                .float_physics,
            Some(FloatPhysicsTier::Light)
        );

        let defaults = parse_default_config().float_physics;
        assert_eq!(defaults.tier, FloatPhysicsTier::Off);
        assert_eq!(defaults, FloatPhysicsConfig::default());
    }

    #[test]
    fn water_glass_block_parses_clamps_and_matches_generated_defaults() {
        let entries = wave_entries(
            "glass {\n\
             animation = ambient\n\
             speed = 99\n\
             amplitude = -1\n\
             settle_ms = 5\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert_eq!(config.water_glass.animation, GlassAnimation::Ambient);
        assert_eq!(config.water_glass.speed, 8.0);
        assert_eq!(config.water_glass.amplitude, 0.0);
        assert_eq!(config.water_glass.settle_ms, 100);

        let defaults = parse_default_config().water_glass;
        assert_eq!(defaults.animation, GlassAnimation::Reactive);
        assert_eq!(defaults, WaterGlassConfig::default());
    }

    #[test]
    fn caustics_block_parses_clamps_and_matches_generated_defaults() {
        let entries = wave_entries(
            "caustics {\n\
             enabled = true\n\
             intensity = 5\n\
             color = 8CDDFF\n\
             scale = 0\n\
             speed = -2\n\
             fps = 999\n\
             idle_fps = [20, 10]\n\
             idle_after = [600000, 1200000]\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert!(config.caustics.enabled);
        assert_eq!(config.caustics.intensity, 1.0);
        assert_eq!(config.caustics.color, parse_ripple_color("8CDDFF").unwrap());
        assert_eq!(config.caustics.scale, 0.1);
        assert_eq!(config.caustics.speed, 0.0);
        assert_eq!(config.caustics.fps, 60);
        assert_eq!(config.caustics.idle_fps, vec![20, 10]);
        assert_eq!(config.caustics.idle_after_ms, vec![600000, 1200000]);

        let defaults = parse_default_config().caustics;
        assert!(!defaults.enabled);
        assert_eq!(defaults, CausticsConfig::default());
        assert_eq!(defaults.idle_fps, vec![30, 15, 10]);
        assert_eq!(defaults.idle_after_ms, vec![300000, 600000, 1800000]);
    }

    #[test]
    fn rule_depth_override_uses_the_last_matching_rule() {
        let entries = wave_entries(
            "rule {\n\
             app_id = kitty\n\
             depth = false\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             depth = true\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        // Two matching rules for the same app: the later one wins, same
        // fold rule every other Option<bool> rule field uses.
        assert_eq!(
            config
                .resolve_window_rules(Some("kitty"), None, None, false)
                .depth,
            Some(true)
        );
        assert_eq!(
            config
                .resolve_window_rules(Some("foot"), None, None, false)
                .depth,
            None
        );
    }

    #[test]
    fn invalid_pointer_modifier_warns_and_safely_falls_back_to_super() {
        let raw = RawConfig {
            pointer_modifier: "none".to_string(),
            ..Default::default()
        };
        let (config, warnings) = Config::from_raw(raw);
        assert_eq!(
            config.pointer_modifier,
            Mods {
                logo: true,
                ..Default::default()
            }
        );
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("Invalid pointer_modifier")));
    }

    #[test]
    fn mod_left_at_super_or_undefined_produces_no_leftover_super_lint() {
        let (_, warnings) = Config::from_raw(RawConfig::default());
        assert!(!warnings
            .iter()
            .any(|w| w.contains("$mod is set away from Super")));

        let raw = RawConfig {
            ..Default::default()
        };
        let (_, warnings) = Config::from_raw(raw);
        assert!(!warnings
            .iter()
            .any(|w| w.contains("$mod is set away from Super")));
    }

    #[test]
    fn mod_freed_from_super_with_every_bind_rewritten_has_nothing_left_to_warn_about() {
        let mut keybinds = HashMap::new();
        keybinds.insert("ALT+Return".to_string(), "spawn:kitty".to_string());
        let raw = RawConfig {
            keybinds,
            pointer_modifier: "ALT".to_string(),
            ..Default::default()
        };
        let (_, warnings) = Config::from_raw(raw);
        assert!(
            !warnings
                .iter()
                .any(|w| w.contains("$mod is set away from Super")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn workspace_transition_block_parses_every_tuning_knob() {
        let entries = wave_entries(
            "transition {\n\
             enabled = false\n\
             style = glow\n\
             duration = 900ms\n\
             speed = 1.75\n\
             curve = exp-out\n\
             direction = left-to-right\n\
             workspace_motion = true\n\
             workspace_motion_delay = 225ms\n\
             wave_amplitude = 72.5\n\
             wave_frequency = 4.5\n\
             edge_width = 26\n\
             color = #FF0088\n\
             wave_size = 14\n\
             wave_alpha = 0.8\n\
             glow_size = 64\n\
             glow_alpha = 0.35\n\
             water_depth = 330\n\
             water_alpha = 0.96\n\
             foam_color = EEF9FF\n\
             foam_size = 24\n\
             foam_alpha = 0.88\n\
             spray_amount = 0.6\n\
             turbulence = 1.2\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        let transition = config.workspace_transition;

        assert!(!transition.enabled);
        assert_eq!(transition.style, WorkspaceTransitionStyle::Glow);
        assert_eq!(transition.duration_ms, 900);
        assert_eq!(transition.speed, 1.75);
        assert_eq!(transition.curve, RippleEase::ExpOut);
        assert_eq!(
            transition.direction,
            WorkspaceTransitionDirectionMode::LeftToRight
        );
        assert!(transition.workspace_motion);
        assert_eq!(transition.workspace_motion_delay_ms, 225);
        assert_eq!(transition.wave_amplitude, 72.5);
        assert_eq!(transition.wave_frequency, 4.5);
        assert_eq!(transition.edge_width, 26.0);
        assert_eq!(transition.color, [1.0, 0.0, 136.0 / 255.0]);
        assert_eq!(transition.wave_size, 14.0);
        assert_eq!(transition.wave_alpha, 0.8);
        assert_eq!(transition.glow_size, 64.0);
        assert_eq!(transition.glow_alpha, 0.35);
        assert_eq!(transition.water_depth, 330.0);
        assert_eq!(transition.water_alpha, 0.96);
        assert_eq!(transition.foam_color, [238.0 / 255.0, 249.0 / 255.0, 1.0]);
        assert_eq!(transition.foam_size, 24.0);
        assert_eq!(transition.foam_alpha, 0.88);
        assert_eq!(transition.spray_amount, 0.6);
        assert_eq!(transition.turbulence, 1.2);
    }

    #[test]
    fn window_animation_blocks_parse_bezier_offsets_and_opacity() {
        let entries = wave_entries(
            "animations {\n\
             preset = wave\n\
             enabled = true\n\
             slowdown = 1.5\n\
             open {\n\
             duration = 240ms\n\
             curve = \"cubic-bezier(0.1,0.8,0.2,1.1)\"\n\
             opacity_duration = 410ms\n\
             opacity_curve = cubic-in-out\n\
             offset = -12x32\n\
             from_opacity = 0.2\n\
             to_opacity = 0.95\n\
             origin = nearest-edge\n\
             effect = tide\n\
             wave_amplitude = 22\n\
             wave_cycles = 1.75\n\
             wave_decay = 2.25\n\
             }\n\
             close {\n\
             enabled = false\n\
             curve = ease-out-quad\n\
             }\n\
             movement {\n\
             animate_size = false\n\
             duration = 360\n\
             curve = exp-out\n\
             }\n\
             }\n",
        );
        let animations = Config::from_raw(lower_entries(&entries)).0.animations;

        assert!(animations.enabled);
        assert_eq!(animations.slowdown, 1.5);
        assert_eq!(animations.open.duration_ms, 240);
        assert_eq!(
            animations.open.curve,
            WindowAnimationCurve::CubicBezier([0.1, 0.8, 0.2, 1.1])
        );
        assert_eq!(animations.open.opacity_duration_ms, Some(410));
        assert_eq!(
            animations.open.opacity_curve,
            Some(WindowAnimationCurve::CubicInOut)
        );
        assert_eq!(animations.open.offset, (-12, 32));
        assert_eq!(animations.open.from_opacity, 0.2);
        assert_eq!(animations.open.to_opacity, 0.95);
        assert_eq!(animations.open.origin, WindowAnimationOrigin::NearestEdge);
        assert_eq!(animations.open.effect, WindowAnimationEffect::Tide);
        assert_eq!(animations.open.wave_amplitude, 22.0);
        assert_eq!(animations.open.wave_cycles, 1.75);
        assert_eq!(animations.open.wave_decay, 2.25);
        assert!(!animations.close.enabled);
        assert_eq!(animations.close.curve, WindowAnimationCurve::QuadOut);
        assert_eq!(animations.close.effect, WindowAnimationEffect::Wave);
        assert_eq!(animations.movement.duration_ms, 360);
        assert_eq!(animations.movement.curve, WindowAnimationCurve::ExpOut);
        assert!(!animations.movement.animate_size);
    }

    #[test]
    fn frost_block_and_per_window_glass_mode_parse() {
        let entries = wave_entries(
            "frost {\n\
             enabled = false\n\
             radius = 24\n\
             strength = 0.75\n\
             opacity = 0.8\n\
             saturation = 1.25\n\
             contrast = 0.9\n\
             brightness = 0.8\n\
             noise = 0.03\n\
             noise_scale = 2\n\
             vibrancy = 0.4\n\
             vibrancy_darkness = 0.6\n\
             tint_color = 88CCFF\n\
             tint_alpha = 0.2\n\
             corner_radius = 16\n\
             corner_softness = 1.5\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             opacity = 0.7\n\
             active_opacity = 1.0\n\
             inactive_opacity = 0.75\n\
             fullscreen_opacity = 0.95\n\
             glass = frost\n\
             frost {\n\
             radius = 32\n\
             opacity = 0.65\n\
             tint_alpha = 0.0\n\
             noise = 0.02\n\
             }\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;

        assert!(!config.frost.enabled);
        assert_eq!(config.frost.radius, 24.0);
        assert_eq!(config.frost.strength, 0.75);
        assert_eq!(config.frost.opacity, 0.8);
        assert_eq!(config.frost.saturation, 1.25);
        assert_eq!(config.frost.contrast, 0.9);
        assert_eq!(config.frost.brightness, 0.8);
        assert_eq!(config.frost.noise, 0.03);
        assert_eq!(config.frost.noise_scale, 2.0);
        assert_eq!(config.frost.vibrancy, 0.4);
        assert_eq!(config.frost.vibrancy_darkness, 0.6);
        assert_eq!(config.frost.tint_color, [136.0 / 255.0, 204.0 / 255.0, 1.0]);
        assert_eq!(config.frost.tint_alpha, 0.2);
        assert_eq!(config.frost.corner_radius, 16.0);
        assert_eq!(config.frost.corner_softness, 1.5);
        let rule = config.resolve_window_rules(Some("kitty"), None, None, false);
        assert_eq!(rule.opacity, Some(0.7));
        assert_eq!(rule.active_opacity, Some(1.0));
        assert_eq!(rule.inactive_opacity, Some(0.75));
        assert_eq!(rule.fullscreen_opacity, Some(0.95));
        assert_eq!(rule.glass, Some(GlassMode::Frost));
        let frost = rule.frost.unwrap().apply_to(&config.frost);
        assert_eq!(frost.radius, 32.0);
        assert_eq!(frost.opacity, 0.65);
        assert_eq!(frost.tint_alpha, 0.0);
        assert_eq!(frost.noise, 0.02);
        assert_eq!(frost.contrast, 0.9);
    }

    #[test]
    fn window_opacity_multiplies_base_and_focus_state_with_fullscreen_priority() {
        let opacity = WindowOpacity {
            base: Some(0.8),
            active: Some(1.0),
            inactive: Some(0.6),
            fullscreen: Some(0.95),
        };
        assert_eq!(opacity.alpha(true, false), 0.8);
        assert!((opacity.alpha(false, false) - 0.48).abs() < f32::EPSILON);
        assert!((opacity.alpha(false, true) - 0.76).abs() < f32::EPSILON);
    }

    #[test]
    fn shadow_block_and_per_window_overrides_parse_and_merge() {
        let entries = wave_entries(
            "shadow {\n\
             enabled = false\n\
             range = 42\n\
             spread = -3\n\
             offset = 4x-6\n\
             scale = 0.9\n\
             render_power = 4\n\
             sharp = true\n\
             ignore_window = false\n\
             color = 11223380\n\
             color_inactive = 0x40102030\n\
             urgent_color = 00CCFFC0\n\
             opacity = 0.8\n\
             inactive_opacity = 0.5\n\
             urgent_opacity = 0.95\n\
             corner_radius = 18\n\
             floating_only = true\n\
             fullscreen = true\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             shadow {\n\
             enabled = true\n\
             softness = 24\n\
             offset_y = 12\n\
             draw_behind_window = false\n\
             }\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             shadow {\n\
             spread = 5\n\
             inactive_opacity = 0.7\n\
             }\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;

        assert!(!config.shadow.enabled);
        assert_eq!(config.shadow.softness, 42.0);
        assert_eq!(config.shadow.spread, -3.0);
        assert_eq!(config.shadow.offset, (4.0, -6.0));
        assert_eq!(config.shadow.scale, 0.9);
        assert_eq!(config.shadow.render_power, 4.0);
        assert!(config.shadow.sharp);
        assert!(config.shadow.draw_behind_window);
        assert_eq!(
            config.shadow.color,
            [17.0 / 255.0, 34.0 / 255.0, 51.0 / 255.0, 128.0 / 255.0]
        );
        assert_eq!(
            config.shadow.inactive_color,
            [16.0 / 255.0, 32.0 / 255.0, 48.0 / 255.0, 64.0 / 255.0]
        );
        assert_eq!(config.shadow.opacity, 0.8);
        assert_eq!(config.shadow.inactive_opacity, 0.5);
        assert_eq!(config.shadow.urgent_opacity, 0.95);
        assert_eq!(config.shadow.corner_radius, 18.0);
        assert!(config.shadow.floating_only);
        assert!(config.shadow.fullscreen);

        let resolved = config.resolve_window_rules(Some("kitty"), None, None, false);
        let shadow = resolved.shadow.unwrap().apply_to(&config.shadow);
        assert!(shadow.enabled);
        assert_eq!(shadow.softness, 24.0);
        assert_eq!(shadow.spread, 5.0);
        // Per-axis sparse override keeps the global X offset.
        assert_eq!(shadow.offset, (4.0, 12.0));
        assert_eq!(shadow.inactive_opacity, 0.7);
        assert!(!shadow.draw_behind_window);
        assert_eq!(shadow.render_power, 4.0);
    }

    #[test]
    fn rounding_and_border_blocks_parse_and_merge_per_window() {
        let entries = wave_entries(
            "rounding {\n\
             radius = [18, 14, 10, 6]\n\
             power = 2.5\n\
             antialias = 1.25\n\
             clip = true\n\
             floating_only = true\n\
             }\n\
             border {\n\
             width = 3.5\n\
             placement = center\n\
             active_from = 2EC7FFEE\n\
             active_to = 5CFFD2EE\n\
             inactive_color = 10203080\n\
             urgent_color_to = 4775FFFF\n\
             angle = 120\n\
             animate = true\n\
             animate_focused = true\n\
             animate_inactive = false\n\
             animate_urgent = true\n\
             inactive_enabled = true\n\
             animation_speed = 42\n\
             pulse_amount = 0.2\n\
             pulse_speed = 1.5\n\
             radius_offset = 2\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             rounding {\n\
             top_right = 22\n\
             floating_only = false\n\
             }\n\
             border {\n\
             placement = inside\n\
             inactive_opacity = 0.45\n\
             }\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert_eq!(config.rounding.radii, [18.0, 14.0, 10.0, 6.0]);
        assert_eq!(config.rounding.power, 2.5);
        assert_eq!(config.rounding.antialias, 1.25);
        assert!(config.rounding.clip);
        assert!(config.rounding.floating_only);
        assert_eq!(config.border.width, 3.5);
        assert_eq!(config.border.placement, BorderPlacement::Center);
        assert!(config.border.animate);
        assert!(config.border.animate_focused);
        assert!(!config.border.animate_inactive);
        assert!(config.border.animate_urgent);
        assert!(config.border.inactive_enabled);
        assert_eq!(config.border.animation_speed, 42.0);
        assert_eq!(config.border.pulse_amount, 0.2);

        let resolved = config.resolve_window_rules(Some("kitty"), None, None, false);
        let rounding = resolved.rounding.unwrap().apply_to(&config.rounding);
        let border = resolved.border.unwrap().apply_to(&config.border);
        assert_eq!(rounding.radii, [18.0, 22.0, 10.0, 6.0]);
        assert!(!rounding.floating_only);
        assert_eq!(border.placement, BorderPlacement::Inside);
        assert_eq!(border.inactive_opacity, 0.45);
        assert_eq!(border.active_to, config.border.active_to);
    }

    #[test]
    fn popup_theme_is_auto_by_default_and_pinned_by_explicit_overrides() {
        // Auto: nothing set, so the popup theme just follows border.width
        // (clamped) and the accent gradient -- no [popup] block at all.
        let auto = Config::from_raw(RawConfig::default()).0;
        let auto_theme = crate::ui_theme::UiTheme::from_config(&auto);
        assert_eq!(auto_theme.border_width, auto.border.width.clamp(1.0, 4.0));
        assert_eq!(
            auto_theme.popup_accent(false, 0.5),
            auto_theme.accent(false, 0.5)
        );

        let entries = wave_entries(
            "popup {\n\
             border_width = 3\n\
             border_color = FF0000\n\
             radius = 20\n\
             }\n",
        );
        let pinned = Config::from_raw(lower_entries(&entries)).0;
        assert_eq!(pinned.popup.border_width, Some(3.0));
        assert_eq!(pinned.popup.radius, Some(20.0));

        let theme = crate::ui_theme::UiTheme::from_config(&pinned);
        assert_eq!(theme.border_width, 3.0);
        assert_eq!(theme.radius, 20);
        // The color pin ignores the gradient position entirely -- same
        // flat color no matter where along the border it's sampled.
        assert_eq!(theme.popup_accent(false, 0.0), [255, 0, 0]);
        assert_eq!(theme.popup_accent(false, 1.0), [255, 0, 0]);
        assert_eq!(theme.popup_accent(true, 0.5), [255, 0, 0]);
    }

    #[test]
    fn glass_mode_is_last_match_wins_and_plain_is_explicit() {
        let entries = wave_entries(
            "rule {\n app_id = kitty\n glass = frost\n }\n\
             rule {\n app_id = kitty\n glass = none\n }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;

        assert_eq!(
            config
                .resolve_window_rules(Some("kitty"), None, None, false)
                .glass,
            Some(GlassMode::Plain)
        );
        assert_eq!(
            config
                .resolve_window_rules(Some("foot"), None, None, false)
                .glass,
            None
        );
    }

    #[test]
    fn workspace_transition_defaults_match_generated_config() {
        for config in [
            Config::from_raw(RawConfig::default()).0,
            parse_default_config(),
        ] {
            let transition = config.workspace_transition;
            assert!(transition.enabled);
            assert_eq!(transition.style, WorkspaceTransitionStyle::Water);
            assert_eq!(transition.duration_ms, 520);
            assert_eq!(transition.speed, 1.0);
            assert_eq!(transition.curve, RippleEase::CubicInOut);
            assert_eq!(transition.direction, WorkspaceTransitionDirectionMode::Auto);
            assert!(!transition.workspace_motion);
            assert_eq!(transition.workspace_motion_delay_ms, 150);
            assert_eq!(transition.wave_amplitude, 34.0);
            assert_eq!(transition.wave_frequency, 3.0);
            assert_eq!(transition.edge_width, 18.0);
            assert_eq!(transition.color, [142.0 / 255.0, 221.0 / 255.0, 1.0]);
            assert_eq!(transition.wave_size, 10.0);
            assert_eq!(transition.wave_alpha, 0.9);
            assert_eq!(transition.glow_size, 46.0);
            assert_eq!(transition.glow_alpha, 0.25);
            assert_eq!(transition.water_depth, 260.0);
            assert_eq!(transition.water_alpha, 0.88);
            assert_eq!(transition.foam_color, [232.0 / 255.0, 252.0 / 255.0, 1.0]);
            assert_eq!(transition.foam_size, 18.0);
            assert_eq!(transition.foam_alpha, 0.95);
            assert_eq!(transition.spray_amount, 0.7);
            assert_eq!(transition.turbulence, 0.7);
        }
    }

    #[test]
    fn depth_block_parses_every_tuning_knob() {
        let entries = wave_entries(
            "depth {\n\
             enabled = false\n\
             sink_after_ms = 1200\n\
             tier_interval_ms = 800\n\
             max_tier = 4\n\
             tier_one_alpha = 0.61\n\
             cool_color = 123456\n\
             cool_alpha = 0.33\n\
             schematic_color = 102030\n\
             schematic_alpha = 0.75\n\
             border_color = AABBCC\n\
             urgent_color = FFEEDD\n\
             urgent_alpha = 0.84\n\
             }\n",
        );
        let depth = Config::from_raw(lower_entries(&entries)).0.depth;
        assert!(!depth.enabled);
        assert_eq!(depth.sink_after_ms, 1200);
        assert_eq!(depth.tier_interval_ms, 800);
        assert_eq!(depth.max_tier, 4);
        assert_eq!(depth.tier_one_alpha, 0.61);
        assert_eq!(depth.cool_color, [18.0 / 255.0, 52.0 / 255.0, 86.0 / 255.0]);
        assert_eq!(depth.cool_alpha, 0.33);
        assert_eq!(
            depth.schematic_color,
            [16.0 / 255.0, 32.0 / 255.0, 48.0 / 255.0]
        );
        assert_eq!(depth.schematic_alpha, 0.75);
        assert_eq!(
            depth.border_color,
            [170.0 / 255.0, 187.0 / 255.0, 204.0 / 255.0]
        );
        assert_eq!(depth.urgent_color, [1.0, 238.0 / 255.0, 221.0 / 255.0]);
        assert_eq!(depth.urgent_alpha, 0.84);
    }

    #[test]
    fn depth_defaults_match_generated_config() {
        for config in [
            Config::from_raw(RawConfig::default()).0,
            parse_default_config(),
        ] {
            let depth = config.depth;
            assert!(depth.enabled);
            assert_eq!(depth.sink_after_ms, 30_000);
            assert_eq!(depth.tier_interval_ms, 30_000);
            assert_eq!(depth.max_tier, 2);
            assert_eq!(depth.tier_one_alpha, 0.78);
            assert_eq!(depth.cool_alpha, 0.24);
            assert_eq!(depth.schematic_alpha, 0.9);
            assert_eq!(depth.urgent_alpha, 0.95);
        }
    }

    #[test]
    fn classic_depth_is_independently_opt_in() {
        let entries = wave_entries(
            "depth_deck {\n\
             enabled = true\n\
             animation = false\n\
             animation_duration_ms = 610\n\
             wave_color = 123456\n\
             wave_alpha = 0.44\n\
             }\n\
             bind Super+D { depth-down }\n\
             bind Super+Shift+D { depth-up }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert!(config.classic_depth.enabled);
        assert!(!config.classic_depth.animation);
        assert_eq!(config.classic_depth.animation_duration_ms, 610);
        assert_eq!(
            config.classic_depth.wave_color,
            [18.0 / 255.0, 52.0 / 255.0, 86.0 / 255.0]
        );
        assert_eq!(config.classic_depth.wave_alpha, 0.44);
        assert!(config
            .keybinds
            .iter()
            .any(|bind| matches!(bind.action, Action::DepthDown)));
        assert!(config
            .keybinds
            .iter()
            .any(|bind| matches!(bind.action, Action::DepthUp)));
        assert!(!RawConfig::default().classic_depth.enabled);
        let default = parse_default_config();
        assert!(!default.classic_depth.enabled);
        assert!(default
            .keybinds
            .iter()
            .all(|bind| !is_depth_action(&bind.action)));
    }

    #[test]
    fn default_submap_parses_from_both_the_in_memory_and_written_defaults() {
        // Two independently hand-maintained representations of the same
        // default (see `RawConfig::default()` and `DEFAULT_KEYBINDS_WAVE`'s
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
        // a duplicate `bind` for the same combo in DEFAULT_KEYBINDS_WAVE
        // (last one silently wins there, unlike a literal duplicate TOML
        // key, which used to fail the parse loudly -- worth a second look
        // at the template by eye if this test ever breaks unexpectedly).
        for (config, main) in [
            (
                Config::from_raw(RawConfig::default()).0,
                Mods {
                    logo: true,
                    ..Default::default()
                },
            ),
            (
                parse_default_config(),
                Mods {
                    logo: true,
                    ..Default::default()
                },
            ),
        ] {
            let find = |key: &str, mods: Mods| {
                config.keybinds.iter().find(|b| {
                    b.keysym == xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE)
                        && b.mods == mods
                })
            };
            let main_shift = Mods {
                alt: main.alt,
                logo: main.logo,
                shift: true,
                ..Default::default()
            };
            let main_ctrl = Mods {
                alt: main.alt,
                logo: main.logo,
                ctrl: true,
                ..Default::default()
            };

            assert!(matches!(
                find("w", main).map(|b| &b.action),
                Some(Action::SetLayout(LayoutAlgorithm::Bsp))
            ));
            assert!(matches!(
                find("w", main_shift).map(|b| &b.action),
                Some(Action::SetLayout(LayoutAlgorithm::Master))
            ));
            assert!(matches!(
                find("minus", main_ctrl).map(|b| &b.action),
                Some(Action::ShrinkMaster)
            ));
            assert!(matches!(
                find("equal", main_ctrl).map(|b| &b.action),
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
    fn shipped_default_config_files_never_trip_the_footgun_lint() {
        // Same check as above, but through the real written text
        // (`DEFAULT_CONFIG_WAVE` + `DEFAULT_KEYBINDS_WAVE`, resolved via
        // `load_raw_config` exactly like a real first boot) rather than
        // the separate Rust-literal `RawConfig::default()` -- catches a
        // problem in the shipped *text* specifically, like a genuinely
        // missing include, that the Rust-side default can't.
        let dir = TestDir::new(&format!(
            "default-config-files-{:?}",
            std::thread::current().id()
        ));
        dir.write("keybinds.wave", DEFAULT_KEYBINDS_WAVE);
        let main = dir.write("config.wave", DEFAULT_CONFIG_WAVE);
        let (raw, include_warnings, _) =
            load_raw_config(&main).expect("the shipped default must parse and resolve");
        let (_, mut warnings) = Config::from_raw(raw);
        warnings.extend(include_warnings);
        assert!(
            warnings.is_empty(),
            "the shipped default config produced diagnostics: {warnings:?}"
        );
    }

    #[test]
    fn bare_typing_key_in_base_keybinds_is_allowed_without_policy_warning() {
        let mut raw = RawConfig::default();
        raw.keybinds
            .insert("Return".to_string(), "spawn:kitty".to_string());
        let (config, warnings) = Config::from_raw(raw);

        // Waves describes exactly what the user requested, even when that
        // deliberately captures a typing key globally.
        let bound = config.keybinds.iter().any(|b| {
            b.keysym == xkb::keysym_from_name("Return", xkb::KEYSYM_CASE_INSENSITIVE)
                && b.mods == Mods::default()
        });
        assert!(bound, "the bare bind should still be applied");
        assert!(
            warnings.is_empty(),
            "unexpected bare-key policy: {warnings:?}"
        );
    }

    #[test]
    fn bare_typing_key_in_a_submap_is_not_flagged() {
        // Submaps rely on bare keys by design (the default `nav` submap's
        // own Escape = exit-mode) -- only the always-active base table
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
    fn ordinary_keys_can_be_held_as_user_defined_helpers() {
        let entries = wave_entries(
            "@sub = P\n\
             bind $sub+Ctrl+H { focus-left }\n\
             bind F { toggle-fullscreen }\n",
        );
        let raw = lower_entries(&entries);
        let (config, warnings) = Config::from_raw(raw);
        assert!(warnings.is_empty());

        let helper = config
            .keybinds
            .iter()
            .find(|bind| matches!(bind.action, Action::FocusDirection(Direction::Left)))
            .unwrap();
        assert_eq!(
            helper.keysym,
            xkb::keysym_from_name("h", xkb::KEYSYM_CASE_INSENSITIVE)
        );
        assert_eq!(
            helper.held_keysyms,
            vec![xkb::keysym_from_name("p", xkb::KEYSYM_CASE_INSENSITIVE)]
        );
        assert!(helper.mods.ctrl);
        assert!(config.keybinds.iter().any(|bind| {
            bind.keysym == xkb::keysym_from_name("f", xkb::KEYSYM_CASE_INSENSITIVE)
                && bind.held_keysyms.is_empty()
                && bind.mods == Mods::default()
                && matches!(bind.action, Action::ToggleFullscreen)
        }));
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

    #[test]
    fn ripple_color_accepts_hash_and_bare_forms() {
        let parse = parse_ripple_color;
        assert_eq!(parse("00FFFF"), Some([0.0, 1.0, 1.0]));
        assert_eq!(parse("#00FFFF"), Some([0.0, 1.0, 1.0]));
        assert_eq!(parse("00FFFF80"), Some([0.0, 1.0, 1.0]));
        // the Hyprland-style rgb()/rgba() text forms are gone with the
        // old grammar
        assert_eq!(parse("rgb(0,255,255)"), None);
        assert_eq!(parse("rgba(00FFFF, 128)"), None);
    }

    #[test]
    fn ripple_presets_parse_with_trigger_specific_styles_and_tuning() {
        assert_eq!(
            parse_ripple_preset_selection("water-drop"),
            Some(RipplePresetSelection::BuiltIn(RipplePreset::WaterDrop))
        );
        assert_eq!(
            parse_ripple_preset_selection("jiggle"),
            Some(RipplePresetSelection::BuiltIn(RipplePreset::Jelly))
        );
        assert_eq!(
            parse_ripple_preset_selection("giggle"),
            Some(RipplePresetSelection::BuiltIn(RipplePreset::Jelly))
        );

        let entries = wave_entries(
            "ripple {\n\
             preset = tide\n\
             map_preset = splash\n\
             focus_preset = jelly\n\
             urgent_preset = bubble\n\
             focus_on_map = true\n\
             secondary_color = CBA6F7\n\
             glow = 1.2\n\
             wobble = 0.9\n\
             detail = 1.1\n\
             triggers = [map, focus, urgent]\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert_eq!(
            config.ripple.preset,
            Some(RipplePresetSelection::BuiltIn(RipplePreset::Tide))
        );
        assert_eq!(
            config.ripple.preset_for(RippleTrigger::Map),
            Some(&RipplePresetSelection::BuiltIn(RipplePreset::Splash))
        );
        assert_eq!(
            config.ripple.preset_for(RippleTrigger::Focus),
            Some(&RipplePresetSelection::BuiltIn(RipplePreset::Jelly))
        );
        assert_eq!(
            config.ripple.preset_for(RippleTrigger::Urgent),
            Some(&RipplePresetSelection::BuiltIn(RipplePreset::Bubble))
        );
        assert_eq!(config.ripple.focus_on_map, Some(true));
        assert_eq!(config.ripple.secondary_color, parse_ripple_color("CBA6F7"));
        assert_eq!(config.ripple.glow, Some(1.2));
        assert_eq!(config.ripple.wobble, Some(0.9));
        assert_eq!(config.ripple.detail, Some(1.1));
    }

    #[test]
    fn urgent_repeat_parses_and_merges_with_clamped_interval() {
        let entries = wave_entries(
            "ripple {\n\
             urgent_repeat = false\n\
             urgent_repeat_interval_ms = 10\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert_eq!(config.ripple.urgent_repeat, Some(false));
        // 10ms is below the 100ms floor; clamped at parse time.
        assert_eq!(config.ripple.urgent_repeat_interval_ms, Some(100));

        let resolved = config.resolve_ripple_config(None, RippleTrigger::Urgent);
        assert_eq!(resolved.urgent_repeat, Some(false));
        assert_eq!(resolved.urgent_repeat_interval_ms, Some(100));

        // Unset inherits the system default (repeat on, 1500ms).
        let default = RippleConfig::system_default();
        assert_eq!(default.urgent_repeat, Some(true));
        assert_eq!(default.urgent_repeat_interval_ms, Some(1500));
    }

    #[test]
    fn assigning_legacy_shapes_selects_the_compatibility_preset() {
        let entries = wave_entries("ripple {\nshapes = [ring, square]\n}\n");
        let config = Config::from_raw(lower_entries(&entries)).0;
        assert_eq!(
            config.ripple.preset,
            Some(RipplePresetSelection::BuiltIn(RipplePreset::Legacy))
        );
        assert_eq!(
            config.ripple.shapes,
            vec![RippleShape::Ring, RippleShape::Square]
        );
    }

    #[test]
    fn named_ripple_presets_inherit_and_keep_local_adjustments() {
        let entries = wave_entries(
            "ripple_preset ocean-base {\n\
             preset = tide\n\
             color = 89B4FA\n\
             size_mode = window\n\
             size_scale = 0.8\n\
             min_radius = 90\n\
             max_radius = 900\n\
             anchor = right\n\
             edge_position = 0.25\n\
             edge_offset = 12\n\
             }\n\
             ripple_preset ocean-wide {\n\
             preset = ocean-base\n\
             detail = 1.4\n\
             }\n\
             ripple_preset ocean-jelly {\n\
             preset = jelly\n\
             wobble = 1.4\n\
             }\n\
             ripple {\n\
             map_preset = ocean-wide\n\
             focus_preset = ocean-jelly\n\
             color = CBA6F7\n\
             triggers = [map, focus]\n\
             }\n",
        );
        let config = Config::from_raw(lower_entries(&entries)).0;

        let map = config.resolve_ripple_config(None, RippleTrigger::Map);
        assert_eq!(map.built_in_preset(), RipplePreset::Tide);
        assert_eq!(map.color, parse_ripple_color("CBA6F7"));
        assert_eq!(map.size_mode, Some(RippleSizeMode::Window));
        assert_eq!(map.anchor, Some(RippleAnchor::Right));
        assert_eq!(map.edge_position, Some(0.25));
        assert_eq!(map.edge_offset, Some(12.0));
        assert_eq!(map.detail, Some(1.4));
        assert!((map.radius_for_window(600.0, 800.0) - 400.0).abs() < 0.01);

        let focus = config.resolve_ripple_config(None, RippleTrigger::Focus);
        assert_eq!(focus.built_in_preset(), RipplePreset::Jelly);
        assert_eq!(focus.wobble, Some(1.4));
    }

    #[test]
    fn ripple_window_size_modes_and_radius_clamps_are_deterministic() {
        let base = RippleConfig {
            size_scale: Some(1.0),
            min_radius: Some(40.0),
            max_radius: Some(500.0),
            ..RippleConfig::system_default()
        };
        for (mode, expected) in [
            (RippleSizeMode::Window, 500.0),
            (RippleSizeMode::Width, 300.0),
            (RippleSizeMode::Height, 400.0),
            (RippleSizeMode::MinDimension, 300.0),
            (RippleSizeMode::MaxDimension, 400.0),
        ] {
            let cfg = RippleConfig {
                size_mode: Some(mode),
                ..base.clone()
            };
            assert!((cfg.radius_for_window(600.0, 800.0) - expected).abs() < 0.01);
        }
    }

    #[test]
    fn shadow_color_keeps_alpha_across_hex_forms() {
        let parse = parse_rgba_color;
        assert_eq!(parse("00FFFF"), Some([0.0, 1.0, 1.0, 1.0]));
        assert_eq!(parse("#00FFFF"), Some([0.0, 1.0, 1.0, 1.0]));
        assert_eq!(parse("00FFFF80"), Some([0.0, 1.0, 1.0, 0.5019608]));
        // 0x form is AARRGGBB
        assert_eq!(parse("0x8000FFFF"), Some([0.0, 1.0, 1.0, 0.5019608]));
        // the Hyprland-style rgb()/rgba() text forms are gone with the
        // old grammar
        assert_eq!(parse("rgb(0,255,255)"), None);
    }
}
