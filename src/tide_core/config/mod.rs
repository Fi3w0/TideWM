//! TideWM configuration loading, lowering, validation, and hot reload.

use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use smithay::input::keyboard::{xkb, Keysym, ModifiersState, XkbConfig};
use smithay::reexports::calloop::channel::{self, Channel};

use crate::wave;
use crate::waves;

mod ebb;
use ebb::apply_ebb_block;
pub use ebb::{EbbConfig, EbbMode};

mod spatial;
use spatial::{
    apply_buoyancy_block, apply_classic_depth_block, apply_compass_block,
    apply_connected_vessels_block, apply_currents_block, apply_depth_block,
    apply_float_physics_block, apply_minimap_block, apply_ocean_block, apply_scroll_block,
    apply_snap_block, apply_sway_block, apply_swim_block, parse_layout_algorithm,
    parse_master_orientation, parse_split_bias, parse_viscosity, parse_workspace_gaps,
    parse_workspace_names,
};
pub use spatial::{
    BuoyancyConfig, ClassicDepthConfig, CompassConfig, CurrentsConfig, DepthBackgroundFit,
    DepthConfig, FloatPhysicsConfig, FloatPhysicsTier, LayoutAlgorithm, MasterOrientation,
    MinimapConfig, OceanConfig, SnapConfig, SpatialEngine, SpatialOwnership, SplitBias, SwayConfig,
    SwimConfig, VesselsConfig, WorkspaceProjection,
};
#[cfg(test)]
pub use spatial::{OceanBookmarkConfig, OceanPanButton, OceanReefConfig};

mod animation;
use animation::{
    apply_animations_block, apply_cascade_liquid_block, apply_ripple_block,
    apply_workspace_transition_block, parse_ripple_color, valid_ripple_preset_name,
};
#[cfg(test)]
use animation::{
    parse_ripple_preset_selection, parse_window_animation_curve, spring_curve_rejection,
    RippleSizeMode,
};
pub use animation::{
    CascadeLiquidConfig, CascadeLiquidPreset, RippleAnchor, RippleConfig, RippleEase, RippleLayer,
    RipplePreset, RipplePresetSelection, RippleShape, RippleTrigger, WindowAnimationConfig,
    WindowAnimationCurve, WindowAnimationEffect, WindowAnimationOrigin, WindowAnimationsConfig,
    WorkspaceAnimationConfig, WorkspaceAnimationStyle, WorkspaceTransitionConfig,
    WorkspaceTransitionDirectionMode, WorkspaceTransitionStyle,
};

mod decoration;
use decoration::{
    apply_border_block, apply_border_override_block, apply_caustics_block, apply_frost_block,
    apply_frost_override_block, apply_popup_block, apply_rounding_block,
    apply_rounding_override_block, apply_shadow_block, apply_shadow_override_block,
    apply_water_glass_block, parse_corner_radii,
};
pub use decoration::{
    BorderConfig, BorderOverrides, BorderPlacement, CausticsConfig, FrostConfig, FrostOverrides,
    GlassAnimation, PopupAnimation, PopupConfig, RoundingConfig, RoundingOverrides, ShadowConfig,
    ShadowOverrides, ToastStyle, WaterGlassConfig,
};

mod rules;
pub(crate) use rules::WindowMatchFacts;
use rules::{lower_layer_rule_block, lower_window_rule_block, lower_workspace_rule_block};
pub use rules::{GlassMode, LayerRule, WindowOpacity, WindowRule, WorkspaceRule};

mod devices;
#[cfg(test)]
use devices::apply_touchpad_block;
use devices::{
    apply_gpu_block, apply_input_block, apply_switch_events_block, apply_xwayland_block,
    lower_output_block, parse_adaptive_sync, parse_position, GpuConfigRaw, SwitchEventsRaw,
};
pub use devices::{
    parse_mode_str, AdaptiveSync, GpuConfig, GpuSelector, GpuVendor, InputConfig,
    InputDeviceConfig, OutputConfig, SwitchEventsConfig, TouchpadConfig, XwaylandConfig,
};

mod shaders;
pub(crate) use shaders::expand_shader_sources;
use shaders::{apply_shaders_block, lower_shader_block};
pub use shaders::{ShaderAssignment, ShaderDefinition, ShaderStage};

mod actions;
#[cfg(test)]
use actions::is_depth_action;
use actions::{depth_action_enabled, parse_keybind, parse_modifiers, parse_simple_chord};
pub(crate) use actions::{is_ocean_action, parse_action};
pub use actions::{Action, ColumnAction, Direction, Keybind, LayoutSwitch, Mods, WorkspaceRef};

mod values;
pub(crate) use values::validate_env_entry;
use values::{
    decode_hex_rgb, list_items, parse_bool, parse_duration_list, parse_duration_ms,
    parse_duration_ms_including_zero, parse_f32_clamped, parse_list_value, parse_rgba_color,
    parse_u32_list, set_bool, set_f64, set_i32, set_opt_bool,
};

mod files;
#[cfg(test)]
use files::load_raw_config;
use files::{config_path, load_raw_config_in};
#[cfg(test)]
use files::{ensure_private_owned_dir, event_affects_paths, watch_directories};
pub use files::{set_config_path_override, spawn_watcher, ConfigWatcher};

mod diff;
pub(crate) use diff::diff_entries;

mod defaults;
use defaults::{DEFAULT_CONFIG_WAVE, DEFAULT_EBB_WAVE, DEFAULT_KEYBINDS_WAVE};

pub struct Config {
    /// The merged entry list this config was lowered from (both formats
    /// produce the same `waves::Entry` shape), kept for the reload diff:
    /// `reload_config` compares the new file's entries against these to
    /// know what actually changed and skip the window-affecting re-apply
    /// battery on a no-op save.
    pub(crate) loaded_entries: Vec<waves::Entry>,
    /// Main config plus every include path reached by the authoritative Wave
    /// evaluation, including missing targets and canonical symlink targets.
    /// The hot-reload watcher follows this exact graph instead of assuming
    /// every include lives below the main config directory.
    pub(crate) source_paths: Vec<PathBuf>,
    pub terminal: String,
    /// Startup-only spatial ownership model. Hot reload keeps the old value
    /// until the next TideWM launch so live windows never change owners.
    pub spatial_engine: SpatialEngine,
    pub ocean: OceanConfig,
    pub scroll: crate::scroll::ScrollConfig,
    pub ebb: EbbConfig,
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
    /// Classic floating-window edge/corner snap behavior and preview.
    pub snap: SnapConfig,
    /// Embedded aqua fallback wallpaper on/off. See `RawConfig::builtin_wallpaper`.
    pub builtin_wallpaper: bool,
    /// Strength of TideWM's render-only interactive move/resize damping.
    /// `1.0` is the default half-life multiplier, `0.0` disables it, and
    /// larger values settle more slowly. `water_effects` is the master
    /// bypass. Matching window rules may override this per app.
    pub viscosity: f64,
    /// Linear downscale factor for the per-window backdrop capture that
    /// feeds frost glass, water glass, and layer-shell blur (`backdrop.rs`,
    /// shared by every reader of `backdrop_textures`). `1` (default)
    /// captures at the window's native physical size, unchanged from
    /// before this knob existed. `2` or `4` allocate a texture with
    /// 1/4 or 1/16 the area -- real VRAM savings on a machine running many
    /// glass windows at once (see report.md's P-13/P-14 audit entries) --
    /// at the cost of a softer captured image once magnified back up to
    /// the window's size. This changes how the identity features look, so
    /// it defaults to the unchanged behavior rather than picking a value;
    /// see AGENT.md's render-roadmap note on agreeing feel changes with
    /// the maintainer before shipping a new default. Clamped to `1..=4`,
    /// the same range `caustics.rs`'s own precedent (`CAUSTICS_DOWNSCALE`)
    /// uses for its pattern texture.
    pub backdrop_capture_scale: i32,
    /// BSP resize pressure propagated through parallel ancestor splits.
    /// `water_effects` is the master bypass; this block can independently
    /// restore the legacy one-split resize path.
    pub vessels: VesselsConfig,
    /// Optional lateral sway for dragged floating windows. Opt-in and
    /// bypassed by the `water_effects` master toggle like viscosity.
    pub sway: SwayConfig,
    /// Cosmetic 2D bob-and-drift for floating windows (spatial roadmap F1,
    /// `light` tier). Opt-in and bypassed by `water_effects`; takes over
    /// from `sway` per window when enabled.
    pub float_physics: FloatPhysicsConfig,
    /// Ocean-only downstream drift for unfocused floating windows. Opt-in,
    /// render-only, and paused while a window is focused or dragged.
    pub currents: CurrentsConfig,
    /// Apparent weight for floating windows in Classic and Ocean. Opt-in and
    /// render-only; Ocean additionally attenuates currents/physics by weight.
    pub buoyancy: BuoyancyConfig,
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
    /// Cascade-layout window lifecycle water. Pour/drain run only for tiled
    /// windows owned by a Cascade workspace and obey `water_effects`.
    pub cascade: CascadeLiquidConfig,
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
    /// for translucent windows with no explicit glass choice. Tiled and
    /// floating placements share the same rule semantics.
    pub frost: FrostConfig,
    /// Water-glass refraction motion: static, disturbance-reactive
    /// (default), or constant ambient drift.
    pub water_glass: WaterGlassConfig,
    /// Master switch for custom `.frag` effects (`shaders { enabled }`),
    /// off by default and independent of `water_effects`.
    pub shaders_enabled: bool,
    /// Named custom effects that parsed and passed the definition caps.
    pub shader_definitions: HashMap<String, ShaderDefinition>,
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
    /// Permit KMS cursor planes, with automatic software fallback.
    pub hardware_cursor: bool,
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
    /// When set, `1..=N` are advertised as existing on every output even
    /// while empty -- by the IPC `workspaces` query and the
    /// `ext-workspace-v1` protocol (`Smallvil::advertised_workspaces`,
    /// the shared definition both use). `None` (the default) keeps the
    /// original behavior: only a workspace with a window, or the one
    /// currently active, is reported. Workspaces themselves stay lazily
    /// created either way -- this only affects what gets *advertised* to
    /// external tooling like a waybar workspace strip, not `Layouts`'
    /// own on-demand tree creation.
    pub workspace_count: Option<u32>,
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
    pub spawn: Vec<String>,
    /// Global adaptive-sync default; a `output { }`'s own `adaptive_sync`
    /// beats it, same override shape `gaps`/`scale`/etc. already use. See
    /// `AdaptiveSync`'s own doc for what actually happens with this today.
    pub adaptive_sync: AdaptiveSync,
    pub outputs: Vec<OutputConfig>,
    /// Startup-only DRM GPU ownership and render-node policy. The nested
    /// backend is initialized before config hot reload can replace hardware,
    /// so changes take effect on the next TideWM start.
    pub gpu: GpuConfig,
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
    /// Per-workspace-number overrides (layout default, decoration base,
    /// `on_created_empty`). See `WorkspaceRule`, `Config::resolve_workspace_rule`.
    pub workspace_rules: Vec<WorkspaceRule>,
    /// Named alternate keybind tables (`mode <name> { }`), each parsed the
    /// same way top-level binds are. See `Action::EnterMode`.
    pub modes: HashMap<String, Vec<Keybind>>,
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
        let lua = wave::new_lua().expect("creating the config Lua state must not fail");
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

        let (raw, error, include_warnings, entries, source_paths) = if path.exists() {
            match load_raw_config_in(lua, tide, &path) {
                Ok((raw, include_warnings, entries, source_paths)) => {
                    (raw, None, include_warnings, entries, source_paths)
                }
                Err(err) => {
                    tracing::warn!(%err, path = %path.display(), "Failed to parse config, using defaults");
                    (
                        RawConfig::default(),
                        Some(err),
                        Vec::new(),
                        Vec::new(),
                        vec![path.clone()],
                    )
                }
            }
        } else {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let ebb_path = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("ebb.wave");
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&ebb_path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    if let Err(error) = file.write_all(DEFAULT_EBB_WAVE.as_bytes()) {
                        tracing::warn!(%error,"Failed to write default Ebb config");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => tracing::warn!(%error,"Failed to create default Ebb config"),
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
            let (default, include_warnings, entries, source_paths) =
                load_raw_config_in(lua, tide, &path).unwrap_or_else(|err| {
                    tracing::error!(%err, "Built-in default Waves config failed to parse");
                    (
                        RawConfig::default(),
                        Vec::new(),
                        Vec::new(),
                        vec![path.clone()],
                    )
                });
            (default, None, include_warnings, entries, source_paths)
        };

        let (mut config, mut warnings) = Self::from_raw(raw);
        warnings.extend(include_warnings);
        config.loaded_entries = entries;
        config.source_paths = source_paths;
        warnings.extend(config.load_shader_sources(&path));
        (config, error, warnings)
    }

    /// Re-read the config file for a hot-reload. Unlike `load`, this never
    /// writes a default file and reports a hard parse failure instead of
    /// silently falling back, so a reload with a typo in it doesn't quietly
    /// wipe out whatever the user had. The `Vec<String>` alongside a
    /// successful reload is the same dropped-keybind/footgun-lint
    /// diagnostics `from_raw` produces -- empty in the common case.
    pub fn reload() -> Result<(Self, Vec<String>), String> {
        let lua = wave::new_lua()?;
        Self::reload_in(&lua, &wave::TideInfo::default())
    }

    /// [`Config::reload`] on the session Lua with live-compositor facts.
    pub(crate) fn reload_in(
        lua: &mlua::Lua,
        tide: &wave::TideInfo,
    ) -> Result<(Self, Vec<String>), String> {
        let path = config_path();
        let (raw, include_warnings, entries, source_paths) = load_raw_config_in(lua, tide, &path)?;
        let (mut config, mut warnings) = Self::from_raw(raw);
        warnings.extend(include_warnings);
        config.loaded_entries = entries;
        config.source_paths = source_paths;
        warnings.extend(config.load_shader_sources(&path));
        Ok((config, warnings))
    }

    /// Evaluates a reload in a fresh Lua sandbox and returns the complete
    /// runtime alongside its lowered config. The caller swaps both only on
    /// success, keeping the active handlers/globals intact after any parse,
    /// include, or evaluation failure.
    pub(crate) fn reload_staged(
        tide: &wave::TideInfo,
    ) -> Result<(mlua::Lua, Self, Vec<String>), String> {
        Self::reload_staged_from(&config_path(), tide)
    }

    fn reload_staged_from(
        path: &Path,
        tide: &wave::TideInfo,
    ) -> Result<(mlua::Lua, Self, Vec<String>), String> {
        let lua = wave::new_lua()?;
        let (raw, include_warnings, entries, source_paths) = load_raw_config_in(&lua, tide, path)?;
        let (mut config, mut warnings) = Self::from_raw(raw);
        warnings.extend(include_warnings);
        config.loaded_entries = entries;
        config.source_paths = source_paths;
        warnings.extend(config.load_shader_sources(path));
        Ok((lua, config, warnings))
    }

    /// Reads, checks and wraps every definition's `.frag` files. Relative
    /// paths resolve from the main config's directory (lowered entries carry
    /// no per-include provenance). Each path joins `source_paths`, so the
    /// watcher reloads on a shader edit, deletion or late creation exactly
    /// like an include. A failed stage keeps its definition with no source,
    /// which leaves any last-good program to the renderer's cache.
    fn load_shader_sources(&mut self, config_path: &Path) -> Vec<String> {
        let base_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        if let Some(background) = &mut self.depth.background {
            *background = waves::resolve_include_path(base_dir, &background.to_string_lossy());
            if !self.source_paths.contains(background) {
                self.source_paths.push(background.clone());
            }
        }
        let mut warnings = Vec::new();
        let mut loaded_bytes = 0usize;
        let mut names: Vec<&String> = self.shader_definitions.keys().collect();
        names.sort_unstable();
        let names: Vec<String> = names.into_iter().cloned().collect();
        for name in names {
            let Some(definition) = self.shader_definitions.get_mut(&name) else {
                continue;
            };
            for (index, stage) in definition.stages.iter_mut().enumerate() {
                stage.path = waves::resolve_include_path(base_dir, stage.file.trim());
                if !self.source_paths.contains(&stage.path) {
                    self.source_paths.push(stage.path.clone());
                }
                let loaded =
                    crate::shader_effect::read_fragment_file(&stage.path).and_then(|source| {
                        crate::shader_effect::validate_fragment_contract(&source)?;
                        let wrapped = crate::shader_effect::wrap_texture_fragment(
                            &source,
                            &stage.params,
                            &stage.textures,
                        );
                        if loaded_bytes + wrapped.len() > crate::shader_effect::MAX_CANDIDATE_BYTES
                        {
                            return Err(format!(
                                "all shader sources together exceed {} bytes",
                                crate::shader_effect::MAX_CANDIDATE_BYTES
                            ));
                        }
                        loaded_bytes += wrapped.len();
                        Ok(wrapped)
                    });
                match loaded {
                    Ok(wrapped) => stage.source = Some(Arc::from(wrapped)),
                    Err(reason) => {
                        stage.source = None;
                        warnings.push(format!(
                            "Shader {name:?} stage {} ({}): {reason}",
                            index + 1,
                            stage.path.display()
                        ));
                    }
                }
            }
        }
        warnings.extend(expand_shader_sources(&mut self.shader_definitions));
        warnings
    }

    /// The path being watched/loaded, so callers can set up a file watcher on it.
    pub fn path() -> PathBuf {
        config_path()
    }

    /// Returns the parsed config plus any diagnostics worth showing on the
    /// compositor-owned panel (dropped keybind entries, footgun lints) --
    /// see `parse_keybind`. Empty when nothing needs a second look.
    fn from_raw(mut raw: RawConfig) -> (Self, Vec<String>) {
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
        let modes = raw
            .modes
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
        let adaptive_sync = parse_adaptive_sync(&raw.adaptive_sync).unwrap_or_else(|| {
            if !raw.adaptive_sync.is_empty() {
                tracing::warn!(value = %raw.adaptive_sync, "Unknown adaptive_sync, using off");
            }
            AdaptiveSync::Off
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
                "Invalid drag_modifier \"{}\"; using Super",
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
            "scroll" => SpatialEngine::Scroll,
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
            .filter(|bind| !matches!(bind.action, Action::EnterMode(_)))
            .collect();
        warnings.append(&mut raw.shader_warnings);
        warnings.append(&mut raw.ebb_warnings);
        let env = raw
            .env
            .into_iter()
            .filter_map(|(key, value)| match validate_env_entry(&key, &value) {
                Ok(()) => Some((key, value)),
                Err(reason) => {
                    warnings.push(format!("Invalid env key {key:?}: {reason}; entry ignored"));
                    None
                }
            })
            .collect();

        let config = Self {
            loaded_entries: Vec::new(),
            source_paths: Vec::new(),
            terminal: raw.terminal,
            spatial_engine,
            ocean: raw.ocean,
            scroll: raw.scroll,
            ebb: raw.ebb,
            pointer_modifier,
            show_welcome_hint: raw.show_welcome_hint,
            show_config_reload_toast: raw.show_config_reload_toast,
            water_effects: raw.water_effects,
            snap: raw.snap,
            builtin_wallpaper: raw.builtin_wallpaper,
            viscosity: raw.viscosity.clamp(0.0, 4.0),
            backdrop_capture_scale: raw.backdrop_capture_scale.clamp(1, 4),
            vessels: raw.vessels,
            sway: raw.sway,
            float_physics: raw.float_physics,
            currents: raw.currents,
            buoyancy: raw.buoyancy,
            swim: raw.swim,
            compass: raw.compass,
            minimap: raw.minimap,
            animations: raw.animations,
            cascade: raw.cascade,
            workspace_transition: raw.workspace_transition,
            depth: raw.depth,
            classic_depth: raw.classic_depth,
            frost: raw.frost,
            water_glass: raw.water_glass,
            shaders_enabled: raw.shaders_enabled,
            shader_definitions: raw.shader_definitions,
            caustics: raw.caustics,
            shadow: raw.shadow,
            rounding: raw.rounding,
            border: raw.border,
            popup: raw.popup,
            ripple: raw.ripple,
            ripple_presets: raw.ripple_presets,
            cursor_always_visible: raw.cursor_always_visible,
            hardware_cursor: raw.hardware_cursor,
            cursor_hide_after_ms: raw.cursor_hide_after_ms,
            workspace_auto_back_and_forth: raw.workspace_auto_back_and_forth,
            workspace_count: raw.workspace_count,
            workspace_names,
            workspace_gaps,
            gaps: raw.gaps,
            default_layout,
            adaptive_sync,
            master_orientation,
            bsp_split_bias,
            pseudo_tile_scale: raw.pseudo_tile_scale.clamp(0.05, 1.0),
            keybinds,
            rescue_keybinds,
            input: raw.input,
            xwayland: raw.xwayland,
            spawn: raw.spawn,
            outputs: raw.outputs,
            gpu: GpuConfig::from_raw(raw.gpu, &mut warnings),
            switch_events: SwitchEventsConfig::from_raw(raw.switch_events),
            window_rules: raw.window_rules,
            layer_rules: raw.layer_rules,
            workspace_rules: raw.workspace_rules,
            modes,
            env,
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

    /// Folds every `rule { }` entry matching `app_id`/`title` into
    /// one effective rule: later matching entries override an earlier
    /// one's `workspace`/`output` (last match wins, so a more specific
    /// rule can sit after a general one and win), while `float`/
    /// `pseudo_tile`/`pin` accumulate (any match sets it, never unsets --
    /// booleans have no "leave alone" state to fall back to). Called once
    /// per newly-mapped window, not per frame, so folding rather than
    /// caching is fine.
    pub(crate) fn resolve_window_rules(&self, facts: WindowMatchFacts) -> WindowRule {
        let mut effective = WindowRule::default();
        for rule in &self.window_rules {
            // `matches`/`matches_initial` each vacuously pass when a rule
            // sets none of the fields they check, so a rule using only
            // spawn-time criteria (say, just `initial_class`) must skip the
            // live-fact check entirely rather than calling `matches` with
            // no live criteria to test -- `matches`'s own blank-rule guard
            // would then reject it outright regardless of `app_id`/`title`.
            let has_live = rule.has_live_criteria();
            let has_initial = rule.has_initial_criteria();
            let has_tag = rule.has_tag_criteria();
            let has_on_workspace = rule.has_on_workspace_criteria();
            if !has_live && !has_initial && !has_tag && !has_on_workspace {
                continue;
            }
            if has_live
                && !rule.matches(
                    facts.app_id,
                    facts.title,
                    facts.pid,
                    facts.is_xwayland,
                    facts.urgent,
                )
            {
                continue;
            }
            if has_initial
                && !rule.matches_initial(
                    facts.initial_app_id,
                    facts.initial_title,
                    facts.at_startup,
                )
            {
                continue;
            }
            if has_tag && !rule.matches_tags(facts.tags) {
                continue;
            }
            if has_on_workspace && !rule.matches_on_workspace(facts.on_workspace) {
                continue;
            }
            if rule.workspace.is_some() {
                effective.workspace = rule.workspace;
            }
            if rule.output.is_some() {
                effective.output = rule.output.clone();
            }
            if rule.pin_output.is_some() {
                effective.pin_output = rule.pin_output.clone();
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
            if rule.shader.is_some() {
                effective.shader = rule.shader.clone();
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
            if rule.weight.is_some() {
                effective.weight = rule.weight;
            }
            if rule.ebb.is_some() {
                effective.ebb = rule.ebb;
            }
            if rule.depth.is_some() {
                effective.depth = rule.depth;
            }
            if rule.snap.is_some() {
                effective.snap = rule.snap;
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
            effective.persistent_size |= rule.persistent_size;
            if rule.min_width.is_some() {
                effective.min_width = rule.min_width;
            }
            if rule.max_width.is_some() {
                effective.max_width = rule.max_width;
            }
            if rule.min_height.is_some() {
                effective.min_height = rule.min_height;
            }
            if rule.max_height.is_some() {
                effective.max_height = rule.max_height;
            }
            if rule.scroll_factor.is_some() {
                effective.scroll_factor = rule.scroll_factor;
            }
            if !rule.add_tag.is_empty() {
                effective.add_tag.extend(rule.add_tag.iter().cloned());
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

    /// Last-match-wins render order for `namespace` within its own wlr
    /// layer stratum, skipping matches that don't set `z_order` at all
    /// (so an unrelated later rule matching the same namespace for a
    /// different effect can't reset it back to the default).
    pub(crate) fn layer_z_order(&self, namespace: &str) -> Option<i32> {
        self.layer_rules
            .iter()
            .rev()
            .filter(|rule| rule.matches(namespace))
            .find_map(|rule| rule.z_order)
    }

    /// Last matching `dim_around = true` rule for `namespace`, resolved to
    /// its overlay alpha (its own `dim_amount`, or a fixed default). `None`
    /// when nothing matching dims. Not an `.any()` fold like
    /// `layer_blocks_capture`'s bare bool: the amount has to come from one
    /// authoritative rule, not several that might disagree.
    pub(crate) fn layer_dim_around(&self, namespace: &str) -> Option<f32> {
        self.layer_rules
            .iter()
            .rev()
            .find(|rule| rule.dim_around && rule.matches(namespace))
            .map(|rule| rule.dim_amount.unwrap_or(0.35))
    }

    /// Whether any `[[layer_rule]]` matching `namespace` sets
    /// `above_lock_screen` -- same "did any of them say yes" fold as
    /// `layer_blocks_capture`, since this is a bare bool.
    pub(crate) fn layer_above_lock_screen(&self, namespace: &str) -> bool {
        self.layer_rules
            .iter()
            .any(|rule| rule.above_lock_screen && rule.matches(namespace))
    }

    /// Whether any `[[layer_rule]]` matching `namespace` sets `blur` --
    /// same any-of fold as `layer_blocks_capture`/`layer_above_lock_screen`.
    /// Callers must still check `self.frost.enabled` separately: this is
    /// the per-namespace opt-in, not the global decoration master switch.
    pub(crate) fn layer_blur(&self, namespace: &str) -> bool {
        self.layer_rules
            .iter()
            .any(|rule| rule.blur && rule.matches(namespace))
    }

    /// The custom effect a layer's last matching `layer_rule { shader }`
    /// names, `None` for `shader = none` or no matching assignment.
    pub(crate) fn layer_shader(&self, namespace: &str) -> Option<&str> {
        let assignment = self
            .layer_rules
            .iter()
            .rev()
            .filter(|rule| rule.matches(namespace))
            .find_map(|rule| rule.shader.as_ref());
        ShaderAssignment::resolve(assignment, None)
    }

    /// Last matching alpha threshold for a blurred layer. An unset value
    /// leaves the full negotiated layer geometry blurred.
    pub(crate) fn layer_ignore_alpha(&self, namespace: &str) -> Option<f32> {
        self.layer_rules
            .iter()
            .rev()
            .filter(|rule| rule.matches(namespace))
            .find_map(|rule| rule.ignore_alpha)
    }

    /// The `layout` field of every `workspace_rule { }` that set one,
    /// keyed by workspace number -- feeds `Layouts::set_workspace_algorithm_overrides`
    /// directly from `Smallvil::new`/`reload_config`, the same "resolve
    /// once here, not per `retile()`" shape `default_layout` already uses.
    pub(crate) fn workspace_algorithm_overrides(&self) -> HashMap<u32, LayoutAlgorithm> {
        let mut map = HashMap::new();
        for rule in &self.workspace_rules {
            if let (Some(workspace), Some(algorithm)) = (rule.workspace, rule.layout) {
                map.insert(workspace, algorithm);
            }
        }
        map
    }

    /// `resolve_workspace_rule(workspace).shader` without the fold's clones,
    /// for the per-frame shader resolution.
    pub(crate) fn workspace_shader(&self, workspace: u32) -> Option<&ShaderAssignment> {
        self.workspace_rules
            .iter()
            .rev()
            .filter(|rule| rule.workspace == Some(workspace))
            .find_map(|rule| rule.shader.as_ref())
    }

    /// The definition a window draws with: its rule's assignment, else its
    /// workspace's default, live against the master switch. Takes the
    /// window's own assignment rather than the window so render code can
    /// call it while holding other state mutably.
    pub(crate) fn shader_for<'a>(
        &'a self,
        assignment: Option<&'a ShaderAssignment>,
        workspace: Option<u32>,
    ) -> Option<(&'a str, &'a ShaderDefinition)> {
        if !self.shaders_enabled {
            return None;
        }
        let name = ShaderAssignment::resolve(
            assignment,
            workspace.and_then(|workspace| self.workspace_shader(workspace)),
        )?;
        self.shader_definitions
            .get_key_value(name)
            .map(|(name, definition)| (name.as_str(), definition))
    }

    /// Folds every `workspace_rule { }` naming `workspace` into one
    /// effective rule, last-match-wins per field -- same fold shape as
    /// `resolve_window_rules`, just against a single numeric match instead
    /// of several identity criteria.
    pub(crate) fn resolve_workspace_rule(&self, workspace: u32) -> WorkspaceRule {
        let mut effective = WorkspaceRule::default();
        for rule in &self.workspace_rules {
            if rule.workspace != Some(workspace) {
                continue;
            }
            if rule.layout.is_some() {
                effective.layout = rule.layout;
            }
            if rule.border.is_some() {
                effective.border = rule.border.clone();
            }
            if rule.rounding.is_some() {
                effective.rounding = rule.rounding.clone();
            }
            if rule.shadow.is_some() {
                effective.shadow = rule.shadow.clone();
            }
            if rule.snap.is_some() {
                effective.snap = rule.snap;
            }
            if rule.shader.is_some() {
                effective.shader = rule.shader.clone();
            }
            if rule.on_created_empty.is_some() {
                effective.on_created_empty = rule.on_created_empty.clone();
            }
        }
        effective
    }
}

#[derive(Debug)]
struct RawConfig {
    terminal: String,
    spatial_engine: String,
    ocean: OceanConfig,
    scroll: crate::scroll::ScrollConfig,
    ebb: EbbConfig,
    ebb_warnings: Vec<String>,
    /// `super`, `alt`, `ctrl`, `shift`, or a `+`-joined combination.
    /// Resolved after Waves variable substitution in `Config::from_raw`.
    pointer_modifier: String,
    show_welcome_hint: bool,
    show_config_reload_toast: bool,
    water_effects: bool,
    snap: SnapConfig,
    /// Whether the embedded 4K aqua fallback wallpaper is decoded and drawn.
    /// Default true (the water identity's backdrop); false skips the decode
    /// and GPU texture entirely so neither CPU nor VRAM is paid for it -- a
    /// layer-shell wallpaper (swaybg/swww/hyprpaper) renders above it
    /// anyway. Read each frame in `wallpaper_element`, so a hot reload to
    /// false stops drawing it; an already-imported texture is dropped on the
    /// next renderer-context replacement.
    builtin_wallpaper: bool,
    viscosity: f64,
    backdrop_capture_scale: i32,
    vessels: VesselsConfig,
    sway: SwayConfig,
    float_physics: FloatPhysicsConfig,
    currents: CurrentsConfig,
    buoyancy: BuoyancyConfig,
    swim: SwimConfig,
    compass: CompassConfig,
    minimap: MinimapConfig,
    animations: WindowAnimationsConfig,
    cascade: CascadeLiquidConfig,
    workspace_transition: WorkspaceTransitionConfig,
    depth: DepthConfig,
    classic_depth: ClassicDepthConfig,
    frost: FrostConfig,
    water_glass: WaterGlassConfig,
    shaders_enabled: bool,
    shader_definitions: HashMap<String, ShaderDefinition>,
    /// Definition problems for the error panel. A rejected definition is
    /// left out entirely instead of rendering something other than asked.
    shader_warnings: Vec<String>,
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
    hardware_cursor: bool,
    cursor_hide_after_ms: i32,
    workspace_auto_back_and_forth: bool,
    workspace_count: Option<u32>,
    gaps: i32,
    /// `"bsp"`/`"master"`, resolved via `parse_layout_algorithm` in
    /// `Config::from_raw`. Raw string (not `LayoutAlgorithm` itself) for
    /// the same reason `switch_events`/`keybinds` store strings -- so a
    /// bad value warns and falls back rather than failing the whole
    /// config parse.
    default_layout: String,
    /// `"off"`/`"on"`/`"on-demand"`, resolved via `parse_adaptive_sync` --
    /// same raw-string-then-resolve shape as `default_layout` above.
    adaptive_sync: String,
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
    /// `spawn` (see `waves::assign_is_multi`), not a scalar --
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
    spawn: Vec<String>,
    outputs: Vec<OutputConfig>,
    gpu: GpuConfigRaw,
    switch_events: SwitchEventsRaw,
    window_rules: Vec<WindowRule>,
    layer_rules: Vec<LayerRule>,
    workspace_rules: Vec<WorkspaceRule>,
    modes: HashMap<String, HashMap<String, String>>,
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
        keybinds.insert("Super+N".to_string(), "mode:nav".to_string());

        // A mode (sway/Hyprland's idea): a temporary alternate keybind
        // table, entered above via Super+N, left active until its own
        // exit-mode bind -- not tied to focus or any other implicit event.
        // This one's a vim-motion focus-move mode (h/j/k/l with no
        // modifier held).
        let mut nav_mode = HashMap::new();
        nav_mode.insert("h".to_string(), "focus-left".to_string());
        nav_mode.insert("l".to_string(), "focus-right".to_string());
        nav_mode.insert("k".to_string(), "focus-up".to_string());
        nav_mode.insert("j".to_string(), "focus-down".to_string());
        nav_mode.insert("Escape".to_string(), "exit-mode".to_string());
        let mut modes = HashMap::new();
        modes.insert("nav".to_string(), nav_mode);

        Self {
            terminal: "kitty".to_string(),
            spatial_engine: "classic".to_string(),
            ocean: OceanConfig::default(),
            scroll: crate::scroll::ScrollConfig::default(),
            ebb: EbbConfig::default(),
            ebb_warnings: Vec::new(),
            pointer_modifier: "super".to_string(),
            backdrop_capture_scale: 1,
            // Deliberately false, not true: a real config.wave always ships
            // with `show_welcome_hint = true` written explicitly (see
            // DEFAULT_CONFIG_WAVE), so this default is only ever consulted
            // when a user deletes the key from an existing file -- and per
            // the on-screen hint's own "delete this to dismiss" advice
            // (welcome.rs), that must resolve to off, not back to on.
            show_welcome_hint: false,
            show_config_reload_toast: true,
            water_effects: true,
            snap: SnapConfig::default(),
            builtin_wallpaper: true,
            viscosity: 1.0,
            vessels: VesselsConfig::default(),
            sway: SwayConfig::default(),
            float_physics: FloatPhysicsConfig::default(),
            currents: CurrentsConfig::default(),
            buoyancy: BuoyancyConfig::default(),
            swim: SwimConfig::default(),
            compass: CompassConfig::default(),
            minimap: MinimapConfig::default(),
            animations: WindowAnimationsConfig::default(),
            cascade: CascadeLiquidConfig::default(),
            workspace_transition: WorkspaceTransitionConfig::default(),
            depth: DepthConfig::default(),
            classic_depth: ClassicDepthConfig::default(),
            frost: FrostConfig::default(),
            water_glass: WaterGlassConfig::default(),
            shaders_enabled: false,
            shader_definitions: HashMap::new(),
            shader_warnings: Vec::new(),
            caustics: CausticsConfig::default(),
            shadow: ShadowConfig::default(),
            rounding: RoundingConfig::default(),
            border: BorderConfig::default(),
            popup: PopupConfig::default(),
            ripple: RippleConfig::default(),
            ripple_presets: HashMap::new(),
            cursor_always_visible: false,
            hardware_cursor: true,
            cursor_hide_after_ms: 0,
            workspace_auto_back_and_forth: false,
            workspace_count: None,
            gaps: 8,
            default_layout: String::new(),
            adaptive_sync: String::new(),
            master_orientation: String::new(),
            bsp_split_bias: String::new(),
            workspace_names: Vec::new(),
            workspace_gaps: Vec::new(),
            pseudo_tile_scale: 0.7,
            keybinds,
            input: InputConfig::default(),
            xwayland: XwaylandConfig::default(),
            spawn: Vec::new(),
            outputs: Vec::new(),
            gpu: GpuConfigRaw::default(),
            switch_events: SwitchEventsRaw::default(),
            window_rules: Vec::new(),
            layer_rules: Vec::new(),
            workspace_rules: Vec::new(),
            modes,
            env: HashMap::new(),
        }
    }
}

fn lower_entries(entries: &[waves::Entry]) -> RawConfig {
    let mut raw = RawConfig::default();
    // A parsed Waves file is authoritative. Defaults belong in the
    // generated default file and the explicit rescue layer, never merged
    // invisibly underneath a user's bindings.
    raw.keybinds.clear();
    raw.modes.clear();
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
            | "builtin_wallpaper"
            | "viscosity"
            | "backdrop_capture_scale"
            | "cursor_always_visible"
            | "hardware_cursor"
            | "cursor_hide_after"
            | "auto_back_and_forth"
            | "workspace_count"
            | "workspace_name"
            | "gaps"
            | "workspace_gaps"
            | "layout"
            | "adaptive_sync"
            | "master_side"
            | "split_bias"
            | "pseudo_tile_scale"
            | "spawn"
    )
}

fn apply_top_level_assign(raw: &mut RawConfig, key: &str, value: &str) {
    match key {
        "terminal" => raw.terminal = value.to_string(),
        "engine" => raw.spatial_engine = value.to_string(),
        "drag_modifier" => raw.pointer_modifier = value.to_string(),
        "welcome_hint" => set_bool(&mut raw.show_welcome_hint, key, value),
        "reload_toast" => set_bool(&mut raw.show_config_reload_toast, key, value),
        "water_effects" => set_bool(&mut raw.water_effects, key, value),
        "builtin_wallpaper" => set_bool(&mut raw.builtin_wallpaper, key, value),
        "viscosity" => match parse_viscosity(value) {
            Some(value) => raw.viscosity = value,
            None => tracing::warn!(value, "Expected finite viscosity from 0.0 to 4.0, ignoring"),
        },
        "backdrop_capture_scale" => set_i32(&mut raw.backdrop_capture_scale, key, value),
        "hardware_cursor" => set_bool(&mut raw.hardware_cursor, key, value),
        "cursor_always_visible" => set_bool(&mut raw.cursor_always_visible, key, value),
        "cursor_hide_after" => {
            // `0` is documented as "never hide", so zero must parse.
            if let Some(ms) = parse_duration_ms_including_zero(value) {
                raw.cursor_hide_after_ms = ms as i32;
            } else {
                tracing::warn!(
                    value,
                    "Expected a duration like 2s or 2000ms for cursor_hide_after, ignoring"
                );
            }
        }
        "auto_back_and_forth" => set_bool(&mut raw.workspace_auto_back_and_forth, key, value),
        "workspace_count" => match value.parse::<u32>() {
            Ok(0) => raw.workspace_count = None,
            Ok(n @ 1..=64) => raw.workspace_count = Some(n),
            Ok(_) => tracing::warn!(
                value,
                "workspace_count must be 0 (off) or 1 to 64, ignoring"
            ),
            Err(err) => tracing::warn!(value, %err, "Expected an integer, ignoring"),
        },
        "gaps" => set_i32(&mut raw.gaps, key, value),
        "layout" => raw.default_layout = value.to_string(),
        "adaptive_sync" | "vrr" | "variable_refresh_rate" => raw.adaptive_sync = value.to_string(),
        "master_side" => raw.master_orientation = value.to_string(),
        "split_bias" => raw.bsp_split_bias = value.to_string(),
        "workspace_name" => raw.workspace_names.push(value.to_string()),
        "workspace_gaps" => raw.workspace_gaps.push(value.to_string()),
        "pseudo_tile_scale" => set_f64(&mut raw.pseudo_tile_scale, key, value),
        // List-shaped, not scalar -- accumulates because `waves::merge_into`
        // already let every occurrence of this one key through instead of
        // deduping to the last (see `waves::assign_is_multi`).
        "spawn" => match parse_list_value(value) {
            Some(items) => raw.spawn.extend(items),
            None => raw.spawn.push(value.to_string()),
        },
        other => tracing::warn!(key = %other, value, "Unknown config key, ignoring"),
    }
}

fn apply_top_level_block(raw: &mut RawConfig, keyword: &str, header: &str, body: &[waves::Entry]) {
    match keyword {
        "ebb" => apply_ebb_block(raw, body, false),
        "ebb_advanced" => apply_ebb_block(raw, body, true),
        "input" => apply_input_block(&mut raw.input, body),
        "xwayland" => apply_xwayland_block(&mut raw.xwayland, body),
        "transition" => apply_workspace_transition_block(&mut raw.workspace_transition, body),
        "snap" => apply_snap_block(&mut raw.snap, body),
        "shaders" => apply_shaders_block(raw, body),
        "shader" => lower_shader_block(raw, header, body),
        "vessels" => apply_connected_vessels_block(&mut raw.vessels, body),
        "sway" => apply_sway_block(&mut raw.sway, body),
        "physics" => apply_float_physics_block(&mut raw.float_physics, body),
        "currents" => apply_currents_block(&mut raw.currents, body),
        "buoyancy" => apply_buoyancy_block(&mut raw.buoyancy, body),
        "swim" => apply_swim_block(&mut raw.swim, body),
        "compass" => apply_compass_block(&mut raw.compass, body),
        "minimap" => apply_minimap_block(&mut raw.minimap, body),
        "ocean" => apply_ocean_block(&mut raw.ocean, body),
        "scroll" => apply_scroll_block(&mut raw.scroll, body),
        "animations" => apply_animations_block(&mut raw.animations, body),
        "cascade" => apply_cascade_liquid_block(&mut raw.cascade, body),
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
        "gpu" => apply_gpu_block(&mut raw.gpu, body),
        "rule" => raw.window_rules.push(lower_window_rule_block(body)),
        "layer_rule" => raw.layer_rules.push(lower_layer_rule_block(body)),
        "workspace_rule" => raw.workspace_rules.push(lower_workspace_rule_block(body)),
        "mode" => {
            let name = header.trim();
            if name.is_empty() {
                tracing::warn!("`mode` block needs a name, ignoring");
                return;
            }
            let binds = raw.modes.entry(name.to_string()).or_default();
            for entry in body {
                match entry {
                    waves::Entry::Bind(combo, action) => {
                        binds.insert(combo.clone(), action.clone());
                    }
                    _ => tracing::warn!(
                        name,
                        "A `mode` block may only contain `bind` statements, ignoring an entry"
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

#[cfg(test)]
mod tests;
