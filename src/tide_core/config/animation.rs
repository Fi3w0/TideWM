use super::*;

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
    Spring(crate::visual::spring::SpringCurve),
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
    /// Maximum detached close snapshots retained at once. This limits tiny
    /// client windows that would otherwise evade an area-only budget.
    pub max_closing_snapshots: usize,
    /// Total retained close-snapshot pixels as a multiple of the live
    /// outputs' aggregate physical pixel area.
    pub close_snapshot_output_budget: f32,
    pub open: WindowAnimationConfig,
    pub close: WindowAnimationConfig,
    pub movement: WindowAnimationConfig,
    /// Non-water workspace motion, independent of `water_effects`.
    pub workspace: WorkspaceAnimationConfig,
    /// Render-only smoothing for pointer-driven move and resize.
    pub interactive: InteractiveAnimationConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAnimationStyle {
    Slide,
    SlideFade,
    Fade,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceAnimationConfig {
    pub enabled: bool,
    pub style: WorkspaceAnimationStyle,
    pub duration_ms: u32,
    pub curve: WindowAnimationCurve,
    /// Horizontal travel as a fraction of the live output width.
    pub travel: f32,
}

impl Default for WorkspaceAnimationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            style: WorkspaceAnimationStyle::SlideFade,
            duration_ms: 220,
            curve: WindowAnimationCurve::CubicBezier([0.16, 1.0, 0.3, 1.0]),
            travel: 0.2,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InteractiveAnimationConfig {
    pub enabled: bool,
    /// Exponential follower half-life in milliseconds. This is independent
    /// of output refresh cadence; elapsed wall time drives the curve.
    pub half_life_ms: u32,
}

impl Default for InteractiveAnimationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            half_life_ms: 28,
        }
    }
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
            max_closing_snapshots: 64,
            close_snapshot_output_budget: 2.0,
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
            workspace: WorkspaceAnimationConfig::default(),
            interactive: InteractiveAnimationConfig::default(),
        }
    }

    /// TideWM's calm default: fast exponential settling with one broad,
    /// low-amplitude sideways swell. It reads as water without making
    /// ordinary window management feel delayed.
    pub fn tide() -> Self {
        Self {
            enabled: true,
            slowdown: 1.0,
            max_closing_snapshots: 64,
            close_snapshot_output_budget: 2.0,
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
            workspace: WorkspaceAnimationConfig::default(),
            interactive: InteractiveAnimationConfig::default(),
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

    fn smooth_glide(
        open_ms: u32,
        close_ms: u32,
        movement_ms: u32,
        interactive_half_life_ms: u32,
        workspace_ms: u32,
        workspace_style: WorkspaceAnimationStyle,
        workspace_travel: f32,
    ) -> Self {
        let mut preset = Self::tide();
        for animation in [&mut preset.open, &mut preset.close, &mut preset.movement] {
            animation.effect = WindowAnimationEffect::Glide;
            animation.wave_amplitude = 0.0;
            animation.wave_cycles = 0.0;
            animation.wave_decay = 0.0;
            animation.curve = WindowAnimationCurve::CubicBezier([0.16, 1.0, 0.3, 1.0]);
        }
        preset.open.duration_ms = open_ms;
        preset.open.offset = (0, 18);
        preset.open.from_opacity = 0.15;
        preset.close.duration_ms = close_ms;
        preset.close.offset = (0, 12);
        preset.movement.duration_ms = movement_ms;
        preset.workspace = WorkspaceAnimationConfig {
            enabled: true,
            style: workspace_style,
            duration_ms: workspace_ms,
            curve: WindowAnimationCurve::CubicBezier([0.16, 1.0, 0.3, 1.0]),
            travel: workspace_travel,
        };
        preset.interactive = InteractiveAnimationConfig {
            enabled: true,
            half_life_ms: interactive_half_life_ms,
        };
        preset
    }

    pub fn silk() -> Self {
        Self::smooth_glide(
            170,
            140,
            200,
            28,
            220,
            WorkspaceAnimationStyle::SlideFade,
            0.20,
        )
    }

    pub fn snappy() -> Self {
        Self::smooth_glide(
            120,
            100,
            140,
            16,
            150,
            WorkspaceAnimationStyle::SlideFade,
            0.12,
        )
    }

    pub fn gentle() -> Self {
        Self::smooth_glide(
            240,
            200,
            260,
            42,
            280,
            WorkspaceAnimationStyle::SlideFade,
            0.16,
        )
    }

    pub fn cinematic() -> Self {
        Self::smooth_glide(
            320,
            260,
            340,
            52,
            360,
            WorkspaceAnimationStyle::SlideFade,
            0.65,
        )
    }

    pub fn minimal() -> Self {
        let mut preset =
            Self::smooth_glide(100, 90, 110, 14, 130, WorkspaceAnimationStyle::Fade, 0.0);
        preset.open.offset = (0, 0);
        preset.close.offset = (0, 0);
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

/// Shape of Cascade's per-window water front. `None` can disable pour and
/// drain independently without turning off the rest of TideWM's water
/// identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CascadeLiquidPreset {
    None,
    #[default]
    Wave,
    Trickle,
    Splash,
}

/// Liquid lifecycle applied only to tiled windows in Cascade workspaces.
/// Ordinary geometry/opacity animation remains enabled by default and may
/// be replaced explicitly when a pure reveal/recede is preferred.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CascadeLiquidConfig {
    pub pour: CascadeLiquidPreset,
    pub drain: CascadeLiquidPreset,
    pub replace_motion: bool,
    pub pour_duration_ms: u32,
    pub drain_duration_ms: u32,
    pub curve: RippleEase,
}

impl Default for CascadeLiquidConfig {
    fn default() -> Self {
        Self {
            pour: CascadeLiquidPreset::Wave,
            drain: CascadeLiquidPreset::Wave,
            replace_motion: false,
            pour_duration_ms: 240,
            drain_duration_ms: 210,
            curve: RippleEase::CubicOut,
        }
    }
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

pub(super) fn apply_animations_block(cfg: &mut WindowAnimationsConfig, body: &[waves::Entry]) {
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
            "silk" => *cfg = WindowAnimationsConfig::silk(),
            "snappy" => *cfg = WindowAnimationsConfig::snappy(),
            "gentle" => *cfg = WindowAnimationsConfig::gentle(),
            "cinematic" => *cfg = WindowAnimationsConfig::cinematic(),
            "minimal" => *cfg = WindowAnimationsConfig::minimal(),
            other => tracing::warn!(
                preset = %other,
                "Unknown animation preset; expected tide, wave, riptide, hypr-smooth, silk, snappy, gentle, cinematic, or minimal"
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
                "max_closing_snapshots" | "max-close-snapshots" => match value.parse::<usize>() {
                    Ok(value) if value <= 4096 => cfg.max_closing_snapshots = value,
                    _ => tracing::warn!(
                        value,
                        "Expected max_closing_snapshots from 0 to 4096, ignoring"
                    ),
                },
                "close_snapshot_output_budget" | "close-snapshot-output-budget" => {
                    match value.parse::<f32>() {
                        Ok(value) if value.is_finite() && (0.0..=16.0).contains(&value) => {
                            cfg.close_snapshot_output_budget = value
                        }
                        _ => tracing::warn!(
                            value,
                            "Expected close_snapshot_output_budget from 0 to 16, ignoring"
                        ),
                    }
                }
                other => tracing::warn!(
                    key = %other,
                    "Unknown assignment in `animations` block, ignoring"
                ),
            },
            waves::Entry::Block(keyword, header, child) => {
                if !header.trim().is_empty() {
                    tracing::warn!(header, "Animation sub-block headers are ignored");
                }
                match keyword.as_str() {
                    "open" | "window_open" | "window-open" => {
                        apply_window_animation_block(&mut cfg.open, child)
                    }
                    "close" | "window_close" | "window-close" => {
                        apply_window_animation_block(&mut cfg.close, child)
                    }
                    "move" | "movement" | "window_movement" | "window-movement" => {
                        apply_window_animation_block(&mut cfg.movement, child)
                    }
                    "workspace" | "workspaces" => {
                        apply_workspace_animation_block(&mut cfg.workspace, child)
                    }
                    "interactive" | "drag" | "resize" => {
                        apply_interactive_animation_block(&mut cfg.interactive, child)
                    }
                    other => tracing::warn!(
                        block = %other,
                        "Unknown animation sub-block, ignoring"
                    ),
                }
            }
            _ => tracing::warn!("Unexpected entry in `animations` block, ignoring"),
        }
    }
}

fn apply_workspace_animation_block(cfg: &mut WorkspaceAnimationConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Workspace animation blocks contain assignments only, ignoring entry");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "style" | "effect" => {
                match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
                    "slide" => cfg.style = WorkspaceAnimationStyle::Slide,
                    "slide-fade" | "slidefade" => cfg.style = WorkspaceAnimationStyle::SlideFade,
                    "fade" => cfg.style = WorkspaceAnimationStyle::Fade,
                    _ => tracing::warn!(
                        value,
                        "Expected workspace style slide, slide-fade, or fade, ignoring"
                    ),
                }
            }
            "duration" => match parse_duration_ms(value) {
                Some(value) if (1..=10_000).contains(&value) => cfg.duration_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected workspace animation duration from 1 to 10000ms, ignoring"
                ),
            },
            "curve" | "ease" => match parse_window_animation_curve(value) {
                Some(value) => cfg.curve = value,
                None => match spring_curve_rejection(value) {
                    Some(reason) => tracing::warn!(value, reason, "Invalid workspace spring, ignoring"),
                    None => tracing::warn!(
                        value,
                        "Expected workspace easing, cubic-bezier(x1,y1,x2,y2), or spring(mass,stiffness,damping), ignoring"
                    ),
                },
            },
            "travel" | "distance" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
                    cfg.travel = value
                }
                _ => tracing::warn!(
                    value,
                    "Expected workspace travel from 0 to 1 of the live output width, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown workspace animation setting, ignoring"),
        }
    }
}

fn apply_interactive_animation_block(cfg: &mut InteractiveAnimationConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Interactive animation blocks contain assignments only, ignoring entry");
            continue;
        };
        match key.as_str() {
            "enabled" => set_bool(&mut cfg.enabled, key, value),
            "half_life" | "half-life" => match parse_duration_ms(value) {
                Some(value) if value <= 10_000 => cfg.half_life_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected interactive half_life from 0 to 10000ms, ignoring"
                ),
            },
            other => {
                tracing::warn!(key = %other, "Unknown interactive animation setting, ignoring")
            }
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
                None => match spring_curve_rejection(value) {
                    Some(reason) => tracing::warn!(value, reason, "Invalid animation spring, ignoring"),
                    None => tracing::warn!(
                        value,
                        "Expected built-in easing, cubic-bezier(x1,y1,x2,y2), or spring(mass,stiffness,damping), ignoring"
                    ),
                },
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
                    None => match spring_curve_rejection(value) {
                        Some(reason) => {
                            tracing::warn!(value, reason, "Invalid opacity spring, ignoring")
                        }
                        None => tracing::warn!(
                            value,
                            "Expected opacity easing, cubic-bezier(x1,y1,x2,y2), or spring(mass,stiffness,damping), ignoring"
                        ),
                    },
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

/// Why a `spring(...)` curve was refused, for its warning. `None` for a
/// value that isn't a spring or is a valid one.
pub(super) fn spring_curve_rejection(value: &str) -> Option<&'static str> {
    let normalized = value.trim().to_ascii_lowercase();
    let inner = normalized
        .strip_prefix("spring(")?
        .strip_suffix(')')
        .unwrap_or_default();
    let values: Option<Vec<f64>> = inner
        .split(',')
        .map(|value| value.trim().parse::<f64>().ok())
        .collect();
    match values.as_deref() {
        Some([mass, stiffness, damping]) => {
            crate::visual::spring::SpringCurve::try_new(*mass, *stiffness, *damping).err()
        }
        _ => Some("spring needs three numbers: spring(mass, stiffness, damping)"),
    }
}

pub(super) fn parse_window_animation_curve(value: &str) -> Option<WindowAnimationCurve> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    if let Some(inner) = normalized
        .strip_prefix("spring(")
        .and_then(|v| v.strip_suffix(')'))
    {
        let values = inner
            .split(',')
            .map(|v| v.trim().parse::<f64>())
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        if let [mass, stiffness, damping] = values.as_slice() {
            return crate::visual::spring::SpringCurve::new(*mass, *stiffness, *damping)
                .map(WindowAnimationCurve::Spring);
        }
        return None;
    }
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

pub(super) fn apply_cascade_liquid_block(cfg: &mut CascadeLiquidConfig, body: &[waves::Entry]) {
    fn preset(value: &str) -> Option<CascadeLiquidPreset> {
        match value {
            "none" => Some(CascadeLiquidPreset::None),
            "wave" => Some(CascadeLiquidPreset::Wave),
            "trickle" => Some(CascadeLiquidPreset::Trickle),
            "splash" => Some(CascadeLiquidPreset::Splash),
            _ => None,
        }
    }

    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `cascade` block, ignoring");
            continue;
        };
        match key.as_str() {
            "pour" => match preset(value) {
                Some(value) => cfg.pour = value,
                None => tracing::warn!(
                    value,
                    "Expected cascade.pour: wave trickle splash none, ignoring"
                ),
            },
            "drain" => match preset(value) {
                Some(value) => cfg.drain = value,
                None => tracing::warn!(
                    value,
                    "Expected cascade.drain: wave trickle splash none, ignoring"
                ),
            },
            "replace_motion" => set_bool(&mut cfg.replace_motion, key, value),
            "pour_duration" => match parse_duration_ms(value) {
                Some(value) if (50..=5000).contains(&value) => cfg.pour_duration_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected cascade.pour_duration from 50 to 5000ms, ignoring"
                ),
            },
            "drain_duration" => match parse_duration_ms(value) {
                Some(value) if (50..=5000).contains(&value) => cfg.drain_duration_ms = value,
                _ => tracing::warn!(
                    value,
                    "Expected cascade.drain_duration from 50 to 5000ms, ignoring"
                ),
            },
            "curve" | "ease" => match parse_ease(value) {
                Some(value) => cfg.curve = value,
                None => tracing::warn!(
                    value,
                    "Expected one of: linear cubic-out cubic-in-out quad-out exp-out, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `cascade` block, ignoring"),
        }
    }
}

pub(super) fn apply_workspace_transition_block(cfg: &mut WorkspaceTransitionConfig, body: &[waves::Entry]) {
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
            "workspace_motion_delay" => match parse_duration_ms_including_zero(value) {
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

/// Parses the body of a `ripple { }` block. Used both for the global
/// block (mutating `RawConfig::ripple` in place) and for a per-rule
/// `rule { ripple { } }` sub-block (mutating a fresh `RippleConfig` and
/// stashing it on the rule). Empty fields stay `None`, which is what
/// `merge_over` later reads as "inherit from the layer below."
pub(super) fn apply_ripple_block(cfg: &mut RippleConfig, body: &[waves::Entry]) {
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

/// Parses a hex color: `RRGGBB` / `RRGGBBAA` bare, or `#RRGGBB` /
/// `#RRGGBBAA` (the alpha byte is silently ignored for now; `peak_alpha`
/// is the dedicated transparency knob).
///
/// Returns linear-space RGB in `[0.0, 1.0]`.
pub(super) fn parse_ripple_color(value: &str) -> Option<[f32; 3]> {
    let v = value
        .trim()
        .strip_prefix('#')
        .unwrap_or_else(|| value.trim());
    decode_hex_rgb(v)
}

fn parse_shapes(value: &str) -> Option<Vec<RippleShape>> {
    let mut out = Vec::new();
    let tokens = list_items(value);
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

pub(super) fn parse_ripple_preset_selection(value: &str) -> Option<RipplePresetSelection> {
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

pub(super) fn valid_ripple_preset_name(name: &str) -> bool {
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
    let tokens = list_items(value);
    for tok in tokens {
        match tok.as_str() {
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
