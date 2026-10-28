use super::*;

/// How the water-glass refraction distortion moves over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlassAnimation {
    /// The original fixed distortion: no time uniform, never ticks frames
    /// on its own.
    Static,
    /// The distortion is energized by disturbances -- the window moving,
    /// a ripple passing underneath -- and
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

/// Which shape TideWM's first-party toast/popup chrome draws. `Pill` is the
/// original rounded card (icon orb + "TIDEWM" label + message); `Banner` is
/// a long bar with a bottom progress line that fills in as the toast's
/// visible time elapses, then the whole thing fades -- opt-in via
/// `[popup] { style = banner }`. Loosely modeled on how Hyprland's own
/// config-warning notifications look and behave (own implementation, not
/// a port). `Pill` stays the default for now; every existing toast call
/// site already picks up whichever style is configured, since both read
/// through `UiTheme::from_config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastStyle {
    Pill,
    Banner,
    /// Square-friendly box with no static border: the accent outline
    /// traces clockwise around it over the popup's visible time.
    Outline,
    /// Borderless box whose countdown is a full-width line along its
    /// bottom edge. The default since 0.90.135.
    #[default]
    Underline,
}

/// How a timed popup enters and leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupAnimation {
    /// Appears in place, fades out.
    Fade,
    /// Slides in from the right screen edge and back out (Hyprland's
    /// notification motion). The default since 0.90.135.
    #[default]
    Slide,
}

/// Global tuning for the bounded single-pass frost shader. The backdrop
/// capture is already window-sized, so cost scales with translucent window
/// area rather than allocating another full-output buffer.
#[derive(Debug, Clone)]
pub struct FrostConfig {
    pub enabled: bool,
    /// Optical edge treatment: inward refraction plus a directional glass
    /// rim. Zero keeps the pre-liquid flat frost treatment.
    pub liquid: f32,
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
            liquid: 0.6,
            radius: 16.0,
            strength: 1.0,
            opacity: 1.0,
            saturation: 1.0,
            contrast: 0.92,
            brightness: 1.04,
            noise: 0.008,
            noise_scale: 1.0,
            vibrancy: 0.16,
            vibrancy_darkness: 0.35,
            tint_color: [142.0 / 255.0, 221.0 / 255.0, 1.0],
            tint_alpha: 0.04,
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
    pub liquid: Option<f32>,
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
            liquid: other.liquid.or(self.liquid),
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
            liquid: self.liquid.unwrap_or(base.liquid),
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
    /// Card shape. Defaults to `Underline`. See `ToastStyle`.
    pub style: ToastStyle,
    /// How long a timed popup stays fully visible before its fade, in
    /// milliseconds. Auto is 2400. Banner's countdown bar spans this.
    pub duration_ms: Option<u64>,
    /// Enter/leave motion. Defaults to `Slide`.
    pub animation: PopupAnimation,
    /// Slide in and out time for `animation = slide`, milliseconds. Auto
    /// is 600, Hyprland's notification animation length.
    pub slide_ms: Option<u64>,
    /// Countdown line thickness, pixels, for `banner` and `underline`.
    /// Auto is 3 for banner and 4 for underline.
    pub line_width: Option<f32>,
    /// Card height for the banner-layout styles, pixels. Auto is 46.
    pub height: Option<f32>,
    /// Minimum card length for the banner-layout styles, pixels. Auto is
    /// 320; never wider than the output allows.
    pub min_width: Option<f32>,
}

fn parse_glass_animation(value: &str) -> Option<GlassAnimation> {
    match value.trim().to_lowercase().as_str() {
        "static" | "off" | "none" => Some(GlassAnimation::Static),
        "reactive" | "disturbed" => Some(GlassAnimation::Reactive),
        "ambient" | "always" | "drift" => Some(GlassAnimation::Ambient),
        _ => None,
    }
}

pub(super) fn apply_water_glass_block(cfg: &mut WaterGlassConfig, body: &[waves::Entry]) {
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `glass` block, ignoring");
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
            other => tracing::warn!(key = %other, "Unknown key in `glass` block, ignoring"),
        }
    }
}

pub(super) fn apply_caustics_block(cfg: &mut CausticsConfig, body: &[waves::Entry]) {
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

pub(super) fn apply_frost_block(cfg: &mut FrostConfig, body: &[waves::Entry]) {
    let mut overrides = FrostOverrides::default();
    apply_frost_override_block(&mut overrides, body);
    *cfg = overrides.apply_to(cfg);
}

pub(super) fn apply_frost_override_block(cfg: &mut FrostOverrides, body: &[waves::Entry]) {
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
            "liquid" | "liquid_strength" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => cfg.liquid = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(value, "Expected frost.liquid from 0 to 1, ignoring"),
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

pub(super) fn apply_shadow_block(cfg: &mut ShadowConfig, body: &[waves::Entry]) {
    let mut overrides = ShadowOverrides::default();
    apply_shadow_override_block(&mut overrides, body);
    *cfg = overrides.apply_to(cfg);
}

pub(super) fn apply_shadow_override_block(cfg: &mut ShadowOverrides, body: &[waves::Entry]) {
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

pub(super) fn apply_rounding_block(cfg: &mut RoundingConfig, body: &[waves::Entry]) {
    let mut overrides = RoundingOverrides::default();
    apply_rounding_override_block(&mut overrides, body);
    *cfg = overrides.apply_to(cfg);
}

pub(super) fn parse_corner_radii(value: &str) -> Option<[f32; 4]> {
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

pub(super) fn apply_rounding_override_block(cfg: &mut RoundingOverrides, body: &[waves::Entry]) {
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

pub(super) fn apply_border_block(cfg: &mut BorderConfig, body: &[waves::Entry]) {
    let mut overrides = BorderOverrides::default();
    apply_border_override_block(&mut overrides, body);
    *cfg = overrides.apply_to(cfg);
}

pub(super) fn apply_border_override_block(cfg: &mut BorderOverrides, body: &[waves::Entry]) {
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

pub(super) fn apply_popup_block(cfg: &mut PopupConfig, body: &[waves::Entry]) {
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
            "animation" | "transition" => {
                cfg.animation = match value.trim().to_ascii_lowercase().as_str() {
                    "fade" => PopupAnimation::Fade,
                    "slide" => PopupAnimation::Slide,
                    other => {
                        tracing::warn!(value = other, "Unknown popup.animation, using the default");
                        PopupAnimation::default()
                    }
                };
            }
            "height" => {
                cfg.height = parse_f32_clamped(value, 24.0, 96.0, "popup.height");
            }
            "min_width" | "min-width" | "width" => {
                cfg.min_width = parse_f32_clamped(value, 120.0, 4096.0, "popup.min_width");
            }
            "line_width" | "line-width" | "bar_width" => {
                cfg.line_width = parse_f32_clamped(value, 1.0, 16.0, "popup.line_width");
            }
            "slide_ms" | "slide_duration" => match parse_duration_ms(value) {
                Some(ms) if ms > 0 => cfg.slide_ms = Some((ms as u64).clamp(50, 3_000)),
                _ => tracing::warn!(
                    value,
                    "Expected a positive integer for popup.slide_ms, ignoring"
                ),
            },
            "duration_ms" | "duration" => match parse_duration_ms(value) {
                Some(ms) if ms > 0 => cfg.duration_ms = Some((ms as u64).clamp(300, 60_000)),
                _ => tracing::warn!(
                    value,
                    "Expected a positive integer for popup.duration_ms, ignoring"
                ),
            },
            "style" => {
                cfg.style = match value.trim().to_ascii_lowercase().as_str() {
                    "pill" | "card" => ToastStyle::Pill,
                    "banner" | "bar" | "strip" => ToastStyle::Banner,
                    "outline" | "frame" | "border" => ToastStyle::Outline,
                    "underline" | "line" => ToastStyle::Underline,
                    other => {
                        tracing::warn!(value = other, "Unknown popup.style, using the default");
                        ToastStyle::default()
                    }
                };
            }
            _ => tracing::warn!(key, "Unknown popup key, ignoring"),
        }
    }
}
