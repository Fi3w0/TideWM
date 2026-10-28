use super::*;

/// Which tiling geometry a workspace uses (see `layout::Layouts::layout`).
/// Not `Deserialize` -- like `Action`, this is resolved from the same
/// string syntax binds/the `default_layout` config key use (see
/// `parse_layout_algorithm`), not deserialized directly, so an
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
    /// Classic floating-default (spatial roadmap Phase 4): the workspace's
    /// BSP tree exists exactly as it does under `Bsp` (`layout()` shares
    /// that arm below), but a window mapping here is converted to floating
    /// immediately, the same `implicit_float` conversion path a parented
    /// dialog already takes -- see `map_toplevel`'s `implicit_float`
    /// computation. `rule { tile = true }` still force-tiles a specific
    /// window, and `toggle-floating`/drag-to-tile still move a window
    /// between floating and this same tree by hand, exactly as under any
    /// other algorithm -- this variant only changes a new window's
    /// *default*, not the tree's capabilities.
    Floating,
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
pub struct VesselsConfig {
    pub enabled: bool,
    pub falloff: f32,
    pub max_splits: u8,
}

impl Default for VesselsConfig {
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
/// exchange collision impulses, high-end only.
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

/// Slow render-only drift for unfocused floating windows in Ocean. This is
/// explicitly opt-in because it keeps visible eligible windows animating at
/// rest; `water_effects` remains the master bypass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurrentsConfig {
    pub enabled: bool,
    /// Screen-space direction in degrees: 0 right, 90 down, 180 left,
    /// 270 up. Cardinal names are accepted by the Wave lowerer too.
    pub direction_degrees: f32,
    /// Maximum envelope in logical pixels.
    pub strength: f32,
    /// Duration of one smooth downstream eddy.
    pub period_ms: u32,
}

impl Default for CurrentsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            direction_degrees: 0.0,
            strength: 10.0,
            period_ms: 14_000,
        }
    }
}

/// Render-only apparent weight for floating windows in both spatial engines.
/// Stable windows consume no frames: a small per-window transition exists
/// only while focus/direct manipulation changes how far the floater sinks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BuoyancyConfig {
    pub enabled: bool,
    /// Weight inherited by a window without a matching `rule { weight }`.
    pub default_weight: f32,
    /// Downward screen-space displacement at weight 1, logical pixels.
    pub max_sink: f32,
    /// Approximate time for focus/drag transitions to settle.
    pub settle_ms: u32,
    /// Ocean-only reduction applied to currents and floating physics at
    /// weight 1. A value of 1 removes all flow response; 0 changes none.
    pub flow_reduction: f32,
}

impl Default for BuoyancyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_weight: 0.35,
            max_sink: 18.0,
            settle_ms: 240,
            flow_reduction: 0.65,
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

#[derive(Debug, Clone, PartialEq)]
pub struct SnapConfig {
    pub enabled: bool,
    pub preset: crate::snap::SnapPreset,
    /// Exact enabled-zone override. `None` uses the selected preset.
    pub zones: Option<Vec<crate::snap::SnapZone>>,
    pub distance: i32,
    /// `None` inherits the current output/workspace gap.
    pub gap: Option<i32>,
    pub preview: bool,
    pub preview_color: [f32; 3],
    pub preview_opacity: f32,
    /// Whether dropping dead-center on the top edge resizes to fill the
    /// whole usable area instead of the ordinary top-half snap. Independent
    /// of `preset` (neither halves nor quarters implies it) so it can be
    /// turned off on its own; an explicit `zones` list still overrides it
    /// either way.
    pub fullscreen: bool,
}

impl Default for SnapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            preset: crate::snap::SnapPreset::Quarters,
            zones: None,
            distance: 24,
            gap: None,
            preview: true,
            preview_color: [46.0 / 255.0, 199.0 / 255.0, 1.0],
            preview_opacity: 0.22,
            fullscreen: true,
        }
    }
}

/// Spatial ownership model. Classic keeps numbered per-output workspaces;
/// Ocean uses continuous world rectangles and per-output cameras, with optional
/// shared visibility. Reloading the selection migrates live windows in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpatialEngine {
    #[default]
    Classic,
    Ocean,
    Scroll,
}

/// Who owns window placement: numbered per-output workspaces (Classic and
/// Scroll), or one continuous Ocean world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialOwnership {
    Workspaces,
    Ocean,
}

/// How a workspace shows its tiles: the Classic layout algorithms, or the
/// Scroll strip. Ocean has no workspaces, so no projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceProjection {
    Classic,
    Scroll,
}

impl SpatialEngine {
    pub fn ownership(self) -> SpatialOwnership {
        match self {
            Self::Classic | Self::Scroll => SpatialOwnership::Workspaces,
            Self::Ocean => SpatialOwnership::Ocean,
        }
    }

    pub fn projection(self) -> Option<WorkspaceProjection> {
        match self {
            Self::Classic => Some(WorkspaceProjection::Classic),
            Self::Scroll => Some(WorkspaceProjection::Scroll),
            Self::Ocean => None,
        }
    }
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
    /// Opt in to rendering another output's windows through this camera.
    /// Independent output membership is the default; world geometry stays
    /// continuous in either mode.
    pub shared_canvas: bool,
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
            shared_canvas: false,
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

/// Automatic attention-distance depth for mapped windows. A window stays at
/// the surface until `sink_after_ms` elapses without focus/input, then moves
/// down one visual tier every `tier_interval_ms`. Focusing or typing into it
/// returns it to tier zero immediately.
#[derive(Debug, Clone)]
pub struct DepthConfig {
    pub enabled: bool,
    pub background: Option<PathBuf>,
    pub background_fit: DepthBackgroundFit,
    pub show_title: bool,
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
            // Opt-in: the blue wash / title cards surprise people who didn't ask for them.
            enabled: false,
            background: None,
            background_fit: DepthBackgroundFit::Cover,
            show_title: true,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DepthBackgroundFit {
    #[default]
    Cover,
    Contain,
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

pub(super) fn apply_snap_block(cfg: &mut SnapConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `snap` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "preset" => match value.trim().to_ascii_lowercase().as_str() {
                "halves" => cfg.preset = crate::snap::SnapPreset::Halves,
                "quarters" | "windows" => cfg.preset = crate::snap::SnapPreset::Quarters,
                _ => tracing::warn!(value, "Expected snap preset halves or quarters, ignoring"),
            },
            "zones" => match parse_list_value(value) {
                Some(values) => {
                    let zones: Option<Vec<_>> = values
                        .iter()
                        .map(|value| crate::snap::SnapZone::parse(value))
                        .collect();
                    match zones {
                        Some(zones) if !zones.is_empty() => cfg.zones = Some(zones),
                        _ => tracing::warn!(
                            value,
                            "Expected a non-empty list of snap zones, ignoring"
                        ),
                    }
                }
                None => tracing::warn!(value, "Expected a list of snap zones, ignoring"),
            },
            "distance" => match value.parse::<i32>() {
                Ok(value) => cfg.distance = value.clamp(0, 512),
                Err(_) => tracing::warn!(value, "Expected snap distance from 0 to 512, ignoring"),
            },
            "gap" => match value.trim() {
                "workspace" | "inherit" | "auto" => cfg.gap = None,
                value => match value.parse::<i32>() {
                    Ok(value) => cfg.gap = Some(value.clamp(0, 256)),
                    Err(_) => tracing::warn!(
                        value,
                        "Expected snap gap from 0 to 256 or workspace, ignoring"
                    ),
                },
            },
            "preview" => set_bool(&mut cfg.preview, key, value),
            "fullscreen" => set_bool(&mut cfg.fullscreen, key, value),
            "preview_color" | "color" => match parse_ripple_color(value) {
                Some(color) => cfg.preview_color = color,
                None => tracing::warn!(
                    value,
                    "Expected snap preview_color as #RRGGBB/rgb(...), ignoring"
                ),
            },
            "preview_opacity" | "opacity" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.preview_opacity = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected snap preview_opacity from 0 to 1, ignoring"),
            },
            other => tracing::warn!(key = %other, "Unknown key in `snap` block, ignoring"),
        }
    }
}

pub(super) fn apply_connected_vessels_block(cfg: &mut VesselsConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `vessels` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "falloff" | "damping" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.falloff = value.clamp(0.0, 1.0),
                _ => tracing::warn!(value, "Expected vessels.falloff from 0 to 1, ignoring"),
            },
            "max_splits" | "depth" => match value.parse::<u8>() {
                Ok(value) if (1..=8).contains(&value) => cfg.max_splits = value,
                _ => tracing::warn!(value, "Expected vessels.max_splits from 1 to 8, ignoring"),
            },
            other => {
                tracing::warn!(key = %other, "Unknown key in `vessels` block, ignoring")
            }
        }
    }
}

pub(super) fn apply_sway_block(cfg: &mut SwayConfig, body: &[waves::Entry]) {
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

pub(super) fn apply_float_physics_block(cfg: &mut FloatPhysicsConfig, body: &[waves::Entry]) {
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) => match key.as_str() {
                "tier" => match value.trim().to_ascii_lowercase().as_str() {
                    "off" => cfg.tier = FloatPhysicsTier::Off,
                    "light" => cfg.tier = FloatPhysicsTier::Light,
                    "full" => cfg.tier = FloatPhysicsTier::Full,
                    _ => tracing::warn!(
                        value,
                        "Expected float_physics.tier: off light full, ignoring"
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

pub(super) fn apply_currents_block(cfg: &mut CurrentsConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `currents` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "direction" => {
                let cardinal = match value.trim().to_ascii_lowercase().as_str() {
                    "right" | "east" => Some(0.0),
                    "down" | "south" => Some(90.0),
                    "left" | "west" => Some(180.0),
                    "up" | "north" => Some(270.0),
                    _ => None,
                };
                match cardinal.or_else(|| value.parse::<f32>().ok().filter(|v| v.is_finite())) {
                    Some(value) => cfg.direction_degrees = value.rem_euclid(360.0),
                    None => tracing::warn!(
                        value,
                        "Expected currents.direction as right/down/left/up or degrees, ignoring"
                    ),
                }
            }
            "strength" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.strength = value.clamp(0.0, 64.0),
                _ => tracing::warn!(value, "Expected currents.strength from 0 to 64, ignoring"),
            },
            "period" => match parse_duration_ms(value) {
                Some(value) if (1_000..=120_000).contains(&value) => cfg.period_ms = value,
                _ => tracing::warn!(value, "Expected currents.period from 1s to 120s, ignoring"),
            },
            other => tracing::warn!(key = %other, "Unknown key in `currents` block, ignoring"),
        }
    }
}

pub(super) fn apply_buoyancy_block(cfg: &mut BuoyancyConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `buoyancy` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "default_weight" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.default_weight = value.clamp(0.0, 1.0),
                _ => tracing::warn!(
                    value,
                    "Expected buoyancy.default_weight from 0 to 1, ignoring"
                ),
            },
            "max_sink" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.max_sink = value.clamp(0.0, 64.0),
                _ => tracing::warn!(value, "Expected buoyancy.max_sink from 0 to 64, ignoring"),
            },
            "settle" => match parse_duration_ms(value) {
                Some(value) if value <= 5_000 => cfg.settle_ms = value,
                _ => tracing::warn!(value, "Expected buoyancy.settle from 0ms to 5s, ignoring"),
            },
            "flow_reduction" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.flow_reduction = value.clamp(0.0, 1.0),
                _ => tracing::warn!(
                    value,
                    "Expected buoyancy.flow_reduction from 0 to 1, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `buoyancy` block, ignoring"),
        }
    }
}

pub(super) fn apply_swim_block(cfg: &mut SwimConfig, body: &[waves::Entry]) {
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

pub(super) fn apply_compass_block(cfg: &mut CompassConfig, body: &[waves::Entry]) {
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

pub(super) fn apply_minimap_block(cfg: &mut MinimapConfig, body: &[waves::Entry]) {
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

pub(super) fn apply_depth_block(cfg: &mut DepthConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `depth` block, ignoring");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "background" => {
                cfg.background = if value.is_empty() || value == "none" {
                    None
                } else {
                    Some(PathBuf::from(value))
                };
            }
            "background_fit" => match value.as_str() {
                "cover" => cfg.background_fit = DepthBackgroundFit::Cover,
                "contain" => cfg.background_fit = DepthBackgroundFit::Contain,
                _ => tracing::warn!(
                    value,
                    "Expected depth.background_fit cover or contain, ignoring"
                ),
            },
            "show_title" => set_bool(&mut cfg.show_title, key, value),
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

pub(super) fn apply_classic_depth_block(cfg: &mut ClassicDepthConfig, body: &[waves::Entry]) {
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

pub(super) fn apply_scroll_block(cfg: &mut crate::scroll::ScrollConfig, body: &[waves::Entry]) {
    use crate::scroll::{Orientation, WorkspaceMode};
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            continue;
        };
        match key.as_str() {
            "workspaces" => match value.as_str() {
                "separate" => cfg.workspaces = WorkspaceMode::Separate,
                "sections" => cfg.workspaces = WorkspaceMode::Sections,
                _ => tracing::warn!(value, "Expected scroll.workspaces separate or sections"),
            },
            "orientation" => match value.as_str() {
                "horizontal" => cfg.orientation = Orientation::Horizontal,
                "vertical" => cfg.orientation = Orientation::Vertical,
                _ => tracing::warn!(value, "Expected scroll.orientation horizontal or vertical"),
            },
            "tile_proportion" => match value.parse::<f64>() {
                Ok(value) if value.is_finite() && (0.1..=1.0).contains(&value) => {
                    cfg.tile_proportion = value
                }
                _ => tracing::warn!(value, "Expected scroll.tile_proportion from 0.1 to 1.0"),
            },
            "gesture_distance" => match value.parse::<f64>() {
                Ok(value) if value.is_finite() && value > 0.0 => cfg.gesture_distance = value,
                _ => tracing::warn!(value, "Expected a positive finite scroll.gesture_distance"),
            },
            "center_focused" => match value.as_str() {
                "never" | "false" => cfg.center_focused = crate::scroll::CenterMode::Never,
                "always" | "true" => cfg.center_focused = crate::scroll::CenterMode::Always,
                "on-overflow" => cfg.center_focused = crate::scroll::CenterMode::OnOverflow,
                _ => tracing::warn!(
                    value,
                    "Expected scroll.center_focused never, always or on-overflow"
                ),
            },
            "float_anchor" => match value.as_str() {
                "screen" => cfg.float_anchor = crate::scroll::FloatAnchor::Screen,
                "strip" => cfg.float_anchor = crate::scroll::FloatAnchor::Strip,
                _ => tracing::warn!(value, "Expected scroll.float_anchor screen or strip"),
            },
            "float_rescue" => set_bool(&mut cfg.float_rescue, "scroll.float_rescue", value),
            "stack" => match value.as_str() {
                "fit" => cfg.stack = crate::scroll::StackMode::Fit,
                "scroll" => cfg.stack = crate::scroll::StackMode::Scroll,
                "hybrid" => cfg.stack = crate::scroll::StackMode::Hybrid,
                _ => tracing::warn!(value, "Expected scroll.stack fit, scroll or hybrid"),
            },
            "stack_height" => match value.parse::<f64>() {
                Ok(height) if height.is_finite() && (0.1..=1.0).contains(&height) => {
                    cfg.stack_height = height
                }
                _ => tracing::warn!(value, "Expected scroll.stack_height from 0.1 to 1.0"),
            },
            "stack_min_height" => match value.parse::<f64>() {
                Ok(height) if height.is_finite() && (0.1..=1.0).contains(&height) => {
                    cfg.stack_min_height = height
                }
                _ => tracing::warn!(value, "Expected scroll.stack_min_height from 0.1 to 1.0"),
            },
            "workspace_scroll" => {
                set_bool(&mut cfg.workspace_scroll, "scroll.workspace_scroll", value)
            }
            "workspace_scroll_duration" => match parse_duration_ms(value) {
                Some(ms) if (1..=10_000).contains(&ms) => cfg.workspace_scroll_duration_ms = ms,
                _ => tracing::warn!(
                    value,
                    "Expected scroll.workspace_scroll_duration from 1 to 10000ms"
                ),
            },
            "always_center_single" => set_bool(
                &mut cfg.always_center_single,
                "scroll.always_center_single",
                value,
            ),
            "preset_widths" => {
                let widths: Option<Vec<f64>> = list_items(value)
                    .iter()
                    .map(|item| {
                        item.parse::<f64>()
                            .ok()
                            .filter(|w| w.is_finite() && (0.1..=1.0).contains(w))
                    })
                    .collect();
                match widths.as_deref().and_then(crate::scroll::PresetWidths::new) {
                    Some(widths) => cfg.preset_widths = widths,
                    _ => tracing::warn!(
                        value,
                        "Expected scroll.preset_widths as a list of 1 to 16 widths from 0.1 to 1.0"
                    ),
                }
            }
            _ => tracing::warn!(key, "Unknown scroll setting"),
        }
    }
}

pub(super) fn apply_ocean_block(cfg: &mut OceanConfig, body: &[waves::Entry]) {
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) if key == "shared_canvas" => {
                set_bool(&mut cfg.shared_canvas, "ocean.shared_canvas", value)
            }
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

pub(super) fn parse_viscosity(value: &str) -> Option<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 4.0))
}

/// Parses `"bsp"`/`"master"`/`"cascade"`/`"floating"` into a
/// `LayoutAlgorithm`. Shared by `"layout:<name>"` keybind actions and the
/// top-level `default_layout` config key; callers differ in what they do
/// with an unrecognized name (a keybind drops the whole bind, same as any
/// other bad action string; the top-level default warns and falls back to
/// `Bsp` instead of failing the whole config over one typo, matching
/// `pseudo_tile_scale`'s clamp-rather-than-reject precedent for a plain
/// scalar setting).
pub(super) fn parse_layout_algorithm(s: &str) -> Option<LayoutAlgorithm> {
    match s {
        "bsp" => Some(LayoutAlgorithm::Bsp),
        "master" => Some(LayoutAlgorithm::Master),
        "cascade" => Some(LayoutAlgorithm::Cascade),
        "floating" => Some(LayoutAlgorithm::Floating),
        _ => None,
    }
}

pub(super) fn parse_master_orientation(s: &str) -> Option<MasterOrientation> {
    match s {
        "left" => Some(MasterOrientation::Left),
        "right" => Some(MasterOrientation::Right),
        "top" => Some(MasterOrientation::Top),
        "bottom" => Some(MasterOrientation::Bottom),
        _ => None,
    }
}

pub(super) fn parse_split_bias(s: &str) -> Option<SplitBias> {
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
pub(super) fn parse_workspace_gaps(raw: &[String], names: &HashMap<String, u32>) -> HashMap<u32, i32> {
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

pub(super) fn parse_workspace_names(raw: &[String]) -> HashMap<String, u32> {
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
