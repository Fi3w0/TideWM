//! Central compositor state and cross-subsystem coordination.

mod config_reload;
mod effects;
mod focus;
mod groups_depth;
mod init;
mod ipc;
mod ocean;
mod physics;
mod render;
mod scroll;
mod session;
mod spatial;
mod workspace;

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    time::Instant,
};

use smithay::{
    backend::{
        renderer::{
            element::{
                default_primary_scanout_output_compare,
                memory::MemoryRenderBuffer,
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::WaylandSurfaceRenderElement,
                RenderElementState,
            },
            gles::{GlesPixelProgram, GlesTexProgram},
            ImportAll, ImportMem,
        },
        session::libseat::LibSeatSession,
    },
    desktop::{PopupGrab, PopupManager, Window},
    input::{keyboard::Keysym, pointer::CursorImageStatus, Seat, SeatState},
    output::Output,
    reexports::{
        calloop::{LoopHandle, LoopSignal, RegistrationToken},
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Client, DisplayHandle,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Rectangle, Size},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufGlobal, DmabufState},
        foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelListState},
        fractional_scale::FractionalScaleManagerState,
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        image_capture_source::{
            ImageCaptureSourceState, OutputCaptureSourceState, ToplevelCaptureSourceState,
        },
        image_copy_capture::{ImageCopyCaptureState, Session as CaptureSession},
        input_method::InputMethodManagerState,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        pointer_gestures::PointerGesturesState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        security_context::SecurityContext,
        selection::{
            data_device::DataDeviceState, primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState,
        },
        session_lock::{LockSurface, SessionLockManagerState, SessionLocker},
        shell::{
            kde::decoration::KdeDecorationState,
            wlr_layer::WlrLayerShellState,
            xdg::{decoration::XdgDecorationState, XdgShellState},
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        tablet_manager::TabletManagerState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_activation::XdgActivationState,
        xdg_toplevel_icon::XdgToplevelIconManager,
    },
};

use crate::{
    capture::PendingCapture,
    config::{Config, Direction},
    grabs::GrabCompletion,
    layout::Layouts,
    toast::Toast,
};

// Render elements for whatever should be visible on an output while
// `session_lock` isn't `Unlocked`: a full-output blank fill, always, plus
// the registered `LockSurface`'s own content on top once it has one. Shared
// by both backends (see `Smallvil::lock_render_elements`), same pattern as
// `backend/udev.rs`'s own `OutputRenderElements` -- and nested inside that
// enum as its own `Lock` variant there, rather than duplicating the
// blank-vs-surface choice per backend.
smithay::backend::renderer::element::render_elements! {
    pub LockRenderElement<R> where R: ImportAll + ImportMem;
    Blank = SolidColorRenderElement,
    Surface = WaylandSurfaceRenderElement<R>,
}

/// The live spatial model. Step 8 of the architecture plan moves the
/// workspace and Ocean ownership state in here.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SpatialState {
    /// The running engine. Only `migrate_spatial_engine` changes it.
    pub(crate) engine: crate::config::SpatialEngine,
}

impl SpatialState {
    /// Classic finds windows under the pointer through `Space`; Scroll and
    /// Ocean through `render_placements`, which carry their view offsets.
    pub(crate) fn hit_tests_through_space(self) -> bool {
        self.engine.projection() == Some(crate::config::WorkspaceProjection::Classic)
    }
}

/// The per-frame ripple elements grouped by their configured
/// `RippleLayer`. Each backend's element-chain builder splices these
/// four groups into the right positions in its front-to-back list --
/// `AboveWindows` between chrome and windows, `BelowWindows` between
/// wallpaper and windows, `AboveAll` at the very front, `BelowAll` at
/// the very back. Empty groups just don't appear in the chain.
#[derive(Default)]
pub(crate) struct RippleLayers {
    pub above_all: Vec<crate::backend::udev::OutputRenderElements>,
    pub above_windows: Vec<crate::backend::udev::OutputRenderElements>,
    pub below_windows: Vec<crate::backend::udev::OutputRenderElements>,
    pub below_all: Vec<crate::backend::udev::OutputRenderElements>,
}

impl RippleLayers {
    fn push(&mut self, layer: crate::config::RippleLayer, element: crate::ripple::RippleElement) {
        let element = crate::backend::udev::OutputRenderElements::Ripple(element);
        match layer {
            crate::config::RippleLayer::AboveAll => self.above_all.push(element),
            crate::config::RippleLayer::AboveWindows => self.above_windows.push(element),
            crate::config::RippleLayer::BelowWindows => self.below_windows.push(element),
            crate::config::RippleLayer::BelowAll => self.below_all.push(element),
        }
    }
}

pub(crate) struct SnapPreview {
    output: String,
    rect: Rectangle<i32, Logical>,
    buffer: SolidColorBuffer,
}

pub struct Smallvil {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    /// PID of the spawned `xwayland-satellite` child, if xwayland is
    /// enabled and it started successfully. Every X11 application arrives
    /// at TideWM as one of satellite's own Wayland surfaces (see the
    /// `xwayland` module docs), so comparing a surface's client PID
    /// against this one is how `rule { xwayland = ... }` tells an X11
    /// window apart from a native Wayland one -- there's no XWayland
    /// protocol marker to introspect instead. Cleared by SIGCHLD handling
    /// if the satellite exits.
    pub(crate) xwayland_satellite_pid: Option<i32>,
    /// DISPLAY installed by the live satellite. Kept alongside its PID so
    /// child-exit cleanup removes only TideWM's own value.
    pub(crate) xwayland_display: Option<String>,

    /// Which backend is driving this session: `"winit"` (nested) or
    /// `"udev"` (standalone DRM/TTY). Set by `main.rs` after backend init;
    /// surfaced through the IPC `diagnostics` request so `tidectl report`
    /// can say which code path a bug ran on without guessing from env.
    pub(crate) backend_name: &'static str,
    pub(crate) classic_tile_drag: Option<WlSurface>,
    pub(crate) native_render_stats: HashMap<Output, crate::backend::perf::OutputRenderStats>,
    /// The session Lua state config evaluates into: config globals and
    /// section tables persist here after a load, which is what makes
    /// `tidectl eval` and `on "event"` handlers work.
    pub(crate) config_lua: mlua::Lua,
    /// Live-compositor facts exposed to config as the `tide` table.
    pub(crate) tide: crate::wave::TideInfo,
    /// Nesting guard for `on "event"` handlers (an action queued by a
    /// handler can emit another event, which runs handlers again).
    handler_depth: u32,
    /// Config parse/apply warnings from the last load or reload, kept so
    /// `tidectl report`'s quick check can show "config has N warnings"
    /// instead of the user having to reproduce them by saving the file.
    pub(crate) config_warnings: Vec<String>,

    pub config: Config,
    /// Event-driven watches for the main Wave file and its current include
    /// graph. Installed by `main` after backend initialization and updated
    /// transactionally after each successful reload.
    pub(crate) config_watcher: Option<crate::config::ConfigWatcher>,
    /// Every libinput device currently attached to the udev seat. Keeping
    /// the ref-counted handles lets config reload re-apply touchpad and
    /// per-device calibration settings without waiting for another hotplug.
    /// Always empty under winit.
    pub known_input_devices: Vec<smithay::reexports::input::Device>,
    /// Resolved libinput-device to live-output bindings. Rebuilt on device
    /// topology, output topology, and config changes so input events only do
    /// a hash lookup and clone an `Output` handle, with no per-event strings
    /// or geometry search.
    pub(crate) input_device_outputs:
        HashMap<smithay::reexports::input::Device, smithay::output::Output>,
    /// Active compositor-consumed touchpad gesture. `None` means gesture
    /// events continue through the ordinary client protocol path.
    pub(crate) compositor_gesture: Option<CompositorGesture>,
    pub toast: Option<Toast>,
    /// Persistent config diagnostic rendered as a reserved top panel.  It
    /// is independent of `toast`: reload confirmations and short debug
    /// notices keep using the existing popup path.
    pub config_error_overlay: Option<crate::error_overlay::ConfigErrorOverlay>,
    /// Lightweight compositor fallback behind the desktop. External
    /// layer-shell wallpaper clients naturally cover it.
    pub builtin_wallpaper: crate::wallpaper::BuiltinWallpaper,
    /// `Some` while the workspace overview (see `overview.rs`) is showing
    /// on the output it was built for (`Overview::output_name`); `None`
    /// otherwise. Built once when toggled on, not rebuilt every frame --
    /// see `toggle_overview`.
    pub overview: Option<crate::overview::Overview>,
    /// `Some` while the whole-world minimap (`minimap.rs`) is being held
    /// open on the output that triggered it. Built once at peek-open, not
    /// rebuilt per frame -- see `open_minimap_peek`/`close_minimap_peek`.
    pub(crate) minimap_peek: Option<crate::minimap::MinimapPeek>,
    /// Whether `config.minimap`'s trigger keysym is currently physically
    /// held down. Tracked separately from XKB modifier state because the
    /// trigger is an ordinary key, not a modifier -- mirrors
    /// `helper_keys_down`'s own press/release bookkeeping shape.
    pub(crate) minimap_trigger_down: bool,
    /// Mouse presses consumed by compositor-owned modal UI whose matching
    /// release must not be delivered to a client after that UI closes and
    /// pointer focus is restored. Entries are button codes, not device-
    /// specific assumptions; a fresh press clears a stale entry if a
    /// release was lost across a backend/session transition.
    pub(crate) suppressed_pointer_buttons: HashSet<u32>,
    /// Structural parked-window state for Classic workspaces. This remains
    /// empty unless the opt-in `classic_depth` feature is used.
    pub classic_depth: crate::classic_depth::ClassicDepthDeck,
    /// Live spatial model. `config.spatial_engine` is only what the config
    /// file asks for; this is what is running.
    pub(crate) spatial: SpatialState,
    /// Continuous world ownership, cameras, reefs, and bookmarks for the
    /// Ocean engine. Dormant while Classic is selected.
    pub ocean: crate::ocean::OceanSpace,
    /// Static title-card overlay for the currently open Classic deck.
    pub depth_deck_overlay: Option<crate::depth_deck::DepthDeckOverlay>,
    #[cfg(feature = "screencast")]
    pub(crate) screencast_picker: Option<crate::source_picker::SourcePicker>,
    /// `Some` when `show_welcome_hint` was still on at startup (`main.rs`).
    /// Built once, static content -- `should_show_welcome_hint` decides
    /// per-frame whether the render call sites actually draw it, this
    /// field just holds the texture so it isn't rebuilt every time.
    pub welcome_hint: Option<crate::welcome::WelcomeHint>,
    last_config_event: Instant,
    config_reload_timer_armed: bool,
    redraw_requests: crate::redraw::RedrawRequests,
    /// Optional backend wake-up used by native DRM to propagate damage as
    /// an event instead of waiting for a fixed polling interval. The winit
    /// backend keeps its host-refresh timer and leaves this unset.
    redraw_wakeup: Option<smithay::reexports::calloop::ping::Ping>,
    /// Timestamp of the last real pointer motion (absolute or relative),
    /// updated from `Smallvil::note_pointer_motion`. Drives
    /// `config.cursor_hide_after_ms`'s auto-hide check at render time
    /// (`backend/udev.rs`) -- authoritative regardless of why a given
    /// render happened to run.
    pub(crate) last_pointer_motion: Instant,
    /// Timestamp of the last real input activity (any keyboard/pointer/
    /// touch event reaching `process_input_event`). Drives the caustics
    /// idle-decay tiers: after `caustics.idle_after_ms[i]` of no input, the
    /// effective frame rate drops to `caustics.idle_fps[i]`, so an idle
    /// desktop's always-on overlay winds down over minutes instead of
    /// burning frames at full rate forever. Pointer motion alone is not
    /// enough -- keys and touch must reset it too.
    pub(crate) last_activity: Instant,
    /// Last XKB layout group + layout names announced over IPC, so
    /// `layout-changed` fires only on an actual change (the `grp:` xkb
    /// option changes the group inside `keyboard.input`, where no action
    /// runs). `None` until the first check.
    pub(crate) keyboard_layout_state: Option<(u32, Vec<String>)>,
    /// Whether a `cursor_hide_after_ms` wake-up timer is currently pending.
    /// `note_pointer_motion` only arms a new timer when this is `false`,
    /// rather than spawning a fresh calloop source on every single motion
    /// event (a real mouse can fire hundreds of those a second) -- the one
    /// live timer re-reads `last_pointer_motion` when it fires and
    /// reschedules itself further out if motion happened more recently
    /// than expected, the same self-extending `TimeoutAction::ToDuration`
    /// idiom `winit.rs`'s own redraw timer already uses.
    cursor_idle_timer_armed: bool,
    /// `Some(name)` while a `mode <name> { }` keybind table is active
    /// (`Action::EnterMode`/`ExitMode`, `input.rs`'s keybind-matching
    /// closure). `None` means the base top-level bind table is in effect.
    /// Not tied to focus or any other implicit event -- only an explicit
    /// `exit-mode` bind clears it, matching sway/Hyprland's own "mode"
    /// behavior.
    pub active_mode: Option<String>,
    /// Emergency table is separate from Waves and starts only after the
    /// deliberate Ctrl+Alt+Escape rescue chord. A successful config reload
    /// clears it. Ordinary non-XKB helper keys held for arbitrary chords are
    /// tracked here so their press/release never leaks into clients.
    pub(crate) rescue_keybinds_active: bool,
    pub(crate) helper_keys_down: HashSet<Keysym>,

    /// IPC event subscribers and the state they were last told about.
    pub(crate) ipc: ipc::IpcState,

    pub layout: Layouts,
    pub space: crate::tide_core::window_space::WindowSpace,
    /// Per-mapped-window base/active/inactive/fullscreen alpha multipliers
    /// resolved from window rules. Missing entries preserve client alpha.
    pub(crate) window_opacity: HashMap<WlSurface, crate::config::WindowOpacity>,
    /// Explicit captured-backdrop mode resolved from window rules. Missing
    /// entries preserve the original behavior: translucent floating windows
    /// use water refraction.
    pub(crate) window_glass_modes: HashMap<WlSurface, crate::config::GlassMode>,
    /// The last matching `rule { shader = ... }`, including an explicit
    /// `none` that opts out of the workspace default. Resolved per frame by
    /// `resolved_shader_assignment`, so toggling `shaders { enabled }`, a
    /// definition or a workspace default needs no per-window re-resolve.
    pub(crate) window_shader_assignments: HashMap<WlSurface, crate::config::ShaderAssignment>,
    /// Assignments from `layer_rule { shader }`, refreshed for each output's
    /// mapped layers during its capture pass and cleared on reload.
    pub(crate) layer_shader_assignments: HashMap<WlSurface, crate::config::ShaderAssignment>,
    /// New custom programs still allowed to compile in this output's frame,
    /// shared by the chain passes, the glass slots and any capture served
    /// before the next frame.
    pub(crate) custom_shader_compile_budget: usize,
    /// Last-good compiled program per custom effect definition.
    pub(crate) custom_shader_programs: crate::shader_effect::CustomShaderPrograms,
    /// `u_time`/`u_delta` clocks for windows currently drawing an effect.
    /// Evicted in `detach_mapped_toplevel`, pruned on reload, cleared on lock.
    pub(crate) custom_shader_instances: HashMap<WlSurface, crate::shader_effect::ShaderInstance>,
    /// Windows refused a custom effect by the instance or payload cap. They
    /// take their normal path until a later capture pass admits them.
    pub(crate) custom_shader_bypassed: HashSet<WlSurface>,
    pub(crate) custom_shader_bypasses: u64,
    /// Captured immediately before a visible frame and sampled by
    /// water/frost glass while building that same frame's elements. The
    /// window-sized texture is reused until its dimensions change. Evicted in
    /// `detach_mapped_toplevel` alongside `window_opacity`; render passes also
    /// drop it after its last output stops presenting the surface.
    pub(crate) backdrop_textures: HashMap<WlSurface, crate::backdrop::BackdropCapture>,
    /// Native alpha captures for blurred layer surfaces whose rule sets
    /// `ignore_alpha`. Kept separate from the backdrop so both textures can
    /// be sampled by the frost shader in one pass.
    pub(crate) layer_alpha_masks: HashMap<WlSurface, crate::backdrop::BackdropCapture>,
    /// Cached full-output solid fill for `layer_rule { dim_around = true }`,
    /// one per output -- same "own buffer per output, dedup only bumps its
    /// commit on real size/color change" shape as `lock_blank`. Pruned on
    /// output disconnect alongside it.
    pub(crate) layer_dim_buffers: HashMap<Output, SolidColorBuffer>,
    /// Per-window water-glass animation state (`water_glass::GlassAnim`):
    /// when the distortion was last disturbed and what frame state that
    /// disturbance was derived from. Only populated for windows taking the
    /// water branch of `glass_frame_elements` while the resolved
    /// `water_glass.animation` isn't `static`; evicted in
    /// `detach_mapped_toplevel` alongside `backdrop_textures`.
    pub(crate) glass_anim: HashMap<WlSurface, crate::water_glass::GlassAnim>,
    /// Compiled lazily on first use (needs a live renderer, unlike
    /// `toast::font()`'s process-global `OnceLock`); see
    /// `water_glass::water_glass_program`.
    pub(crate) water_glass_program: Option<GlesTexProgram>,
    /// Frost is a separate program over the same capture texture; compiled
    /// lazily only when a rule actually selects `glass = frost`.
    pub(crate) frost_glass_program: Option<GlesTexProgram>,
    /// Analytical window-shadow shader, compiled once on first use.
    pub(crate) shadow_program: Option<GlesPixelProgram>,
    /// Texture override used to clip main toplevel surface trees.
    pub(crate) rounded_surface_program: Option<GlesTexProgram>,
    /// Analytical solid/gradient window-border shader.
    pub(crate) border_program: Option<GlesPixelProgram>,
    /// Active impulse ripples (Phase R1, see `ripple.rs`). Each one lives
    /// on a specific output, decays over its own `duration`, and is
    /// `retain`-pruned once `finished()` inside `ripple_frame_elements`
    /// so this Vec never grows past the in-flight count. Capped at a
    /// small constant by `spawn_ripple` to bound the worst case (rapid
    /// window mapping shouldn't stack unboundedly), matching the same
    /// no-unbounded-growth steady-state discipline every other animation
    /// field in this struct obeys.
    pub(crate) ripples: Vec<crate::ripple::Ripple>,
    /// Compiled lazily on first use; see `ripple::ripple_program`.
    pub(crate) ripple_program: Option<GlesPixelProgram>,
    /// Short-lived visual state for mapped windows. Logical geometry and
    /// input move immediately; these maps only interpolate what is drawn.
    pub(crate) window_open_animations:
        HashMap<WlSurface, crate::window_animation::WindowVisualAnimation>,
    /// Cascade-only mapped-window pour clocks. Closing clocks move into the
    /// bounded snapshot records below so unmapping never retains a client.
    pub(crate) cascade_window_animations:
        HashMap<WlSurface, crate::cascade_transition::CascadeTransition>,
    /// Coalesces strip hit-test refreshes outside pointer-grab callbacks.
    scroll_pointer_refresh_pending: bool,
    pub(crate) window_move_animations:
        HashMap<WlSurface, crate::window_animation::WindowVisualAnimation>,
    /// Short-lived render-only followers for interactive move/resize.
    /// Logical geometry stays at the pointer target; one entry per active
    /// mapped window bounds this state without retaining motion history.
    pub(crate) window_viscosity: HashMap<WlSurface, crate::viscosity::ViscousMotion>,
    /// Lateral damped oscillation for dragged floating windows. One small
    /// closed-form record per mapped window; a settled entry finishes and
    /// is pruned, so idle windows never keep the frame pump alive.
    pub(crate) window_sway: HashMap<WlSurface, crate::sway::FloatingSway>,
    /// Cosmetic 2D bob-and-drift for floating windows (F1 `light`). One
    /// small closed-form record per disturbed window; a settled entry
    /// finishes and is pruned, so idle windows never keep the frame pump
    /// alive. When a window resolves float-physics-enabled it is tracked
    /// here instead of in `window_sway` -- the two never stack.
    pub(crate) window_float_physics: HashMap<WlSurface, crate::float_physics::FloatPhysics>,
    /// Windows with ambient "sitting on water" sway toggled on
    /// (`toggle-float-ambient`), keyed to the `Instant` it was toggled on --
    /// the time origin `float_physics::ambient_sample`'s `elapsed` measures
    /// from, sampled fresh in `base_window_visual_sample`. Unlike
    /// `window_float_physics`, entries here don't finish on their own --
    /// they persist until explicitly toggled off, deliberately keeping the
    /// frame pump alive the whole time. Independent of `window_float_physics`
    /// entirely: the two offsets simply add when both are active.
    pub(crate) window_float_ambient: HashMap<WlSurface, Instant>,
    /// One mass-spring-damper body per floating window using full physics.
    /// Continuous wave forcing and collisions require persistent velocity;
    /// `sync_float_physics_bodies` keeps membership aligned with live rules.
    pub(crate) window_float_bodies: HashMap<WlSurface, crate::float_physics::FloatBody>,
    /// Fixed-timestep accumulator for `update_float_physics_full`'s
    /// integrator, carried across ticks so a variable frame interval still
    /// advances the simulation in fixed, numerically stable steps. Clamped
    /// to a small multiple of the fixed step so a long stall (a hung
    /// nested host, a VT switch) can't dump a huge catch-up burst of
    /// substeps into a single tick.
    pub(crate) float_physics_accum: f64,
    /// Wall-clock timestamp of the previous `update_float_physics_full`
    /// call, used to derive the elapsed real time fed into the accumulator
    /// above.
    pub(crate) float_physics_last_tick: Instant,
    /// Fixed time origin the traveling wave field's phase is measured from
    /// (`float_physics::wave_target`'s `elapsed`). A single shared clock
    /// (not one per window, unlike `window_float_ambient`) because the
    /// wave is one continuous field every full-tier window samples by its
    /// own position, not N independent oscillators.
    pub(crate) float_physics_wave_clock: Instant,
    /// Ocean-only cosmetic current, one small phase clock per visible
    /// floating window. Records are paused (not reset) while focused or
    /// directly dragged, and cleared outside Ocean or when disabled.
    pub(crate) window_currents: HashMap<WlSurface, crate::currents::CurrentDrift>,
    /// Floating move grabs currently under direct manipulation. Kept
    /// separately from viscosity because viscosity may be disabled or may
    /// keep settling briefly after the physical grab ends.
    pub(crate) floating_dragging: HashSet<WlSurface>,
    /// One bounded analytical fill while a Classic floating drag is inside
    /// a configured edge/corner snap zone. Dropped on every grab teardown.
    pub(crate) snap_preview: Option<SnapPreview>,
    /// Render-only apparent weight, retained only for visible eligible
    /// floaters. Stable entries request no frames; `buoyancy_dirty` avoids
    /// rebuilding placements on idle backend maintenance ticks.
    pub(crate) window_buoyancy: HashMap<WlSurface, crate::buoyancy::BuoyancyState>,
    pub(crate) buoyancy_dirty: bool,
    pub(crate) buoyancy_camera_revision: u64,
    pub(crate) buoyancy_camera_was_moving: bool,
    /// Continuous lateral swim camera, one entry per output that has ever
    /// swum. The offset is purely visual (the logical workspace identity is
    /// still `Layouts::active`'s `u32`); at rest it is zero and the entry
    /// asks for no frames.
    pub(crate) swim_cameras: HashMap<String, crate::swim::SwimCamera>,
    /// Vertical workspace-slide camera, one entry per output while a
    /// `workspace-up`/`workspace-down` step settles. At rest the entry is
    /// removed; the logical workspace changes immediately, the camera only
    /// animates the already-applied switch.
    pub(crate) workspace_scroll_cameras: HashMap<String, crate::swim::WorkspaceScrollCamera>,
    /// Last already-imported surface tree for each visible mapped window.
    /// Texture handles are cloned, not framebuffer contents; the entry is
    /// transferred to the bounded close list on unmap.
    pub(crate) window_frame_snapshots:
        HashMap<WlSurface, crate::window_animation::WindowFrameSnapshot>,
    /// Detached windows retaining their last drawable GPU snapshot until
    /// the configured close animation finishes.
    pub(crate) closing_window_animations: Vec<crate::window_animation::ClosingWindowAnimation>,
    /// Per-mapped-window automatic attention depth. Entries are created on
    /// map and removed on unmap/destroy, so time-based sinking cannot grow
    /// state beyond the current mapped toplevel count.
    pub(crate) window_depths: HashMap<WlSurface, crate::depth::WindowDepth>,
    /// Shared artwork source, CPU frame and per-renderer texture imports.
    pub(crate) depth_background: crate::depth_background::DepthBackground,
    depth_background_timer: Option<RegistrationToken>,
    depth_background_deadline: Option<Instant>,
    /// With artwork these buffers contain only the optional title strip.
    /// Returning to live content or unmapping evicts a window's buffer.
    pub(crate) depth_schematics: HashMap<WlSurface, crate::depth::DepthSchematic>,
    /// Shared analytical cool-wash/urgent-border shader.
    pub(crate) depth_overlay_program: Option<GlesPixelProgram>,
    /// Throttles inactivity scans to 10Hz even when a backend ticks faster.
    depth_last_tick: Instant,
    /// Requested workspace target awaiting an outgoing-frame capture,
    /// keyed by output name. A HashMap deliberately makes this one slot
    /// per output: repeated switches replace the target instead of
    /// accumulating full-output work.
    pub(crate) pending_workspace_transitions: HashMap<String, u32>,
    /// At most one captured outgoing texture per output, dropped as soon
    /// as its wipe completes (see `workspace_transition.rs`).
    pub(crate) workspace_transitions:
        HashMap<String, crate::workspace_transition::WorkspaceTransition>,
    /// At most one non-water outgoing workspace snapshot per output.
    pub(crate) workspace_glides: HashMap<String, crate::workspace_transition::WorkspaceGlide>,
    /// Lazily compiled custom texture shader shared by every output's
    /// transition element.
    pub(crate) workspace_transition_program: Option<GlesTexProgram>,
    /// At most one analytical Classic depth switch per output.
    pub(crate) depth_transitions: HashMap<String, crate::depth_transition::DepthTransition>,
    pub(crate) depth_transition_program: Option<GlesPixelProgram>,
    /// One stable analytical canvas element per live Ocean output. No
    /// texture/framebuffer; the record only preserves damage identity.
    pub(crate) ocean_canvases: HashMap<String, crate::ocean_canvas::OceanCanvas>,
    pub(crate) ocean_canvas_program: Option<GlesPixelProgram>,
    /// Per-output ambient caustics state and one shared compiled shader.
    /// Engine-agnostic (works under both Classic and Ocean); gated by
    /// `water_effects` plus `caustics.enabled`. The phase only advances
    /// inside `caustics_frame_element`, so damage-driven idle desktops
    /// freeze the pattern at zero extra frames; `caustics.fps > 0` opts
    /// into deadline-driven constant motion.
    pub(crate) caustics: HashMap<String, crate::caustics::Caustics>,
    pub(crate) caustics_program: crate::caustics::CausticsProgramCache,
    /// Per-output compass state and one shared compiled shader (spatial
    /// roadmap S5). Ocean-only and gated by `water_effects`; no element is
    /// produced when nothing sits off-screen.
    pub(crate) compasses: HashMap<String, crate::compass::Compass>,
    pub(crate) compass_program: Option<GlesPixelProgram>,
    pub loop_handle: LoopHandle<'static, Smallvil>,
    pub loop_signal: LoopSignal,

    // Smithay State
    pub compositor_state: CompositorState,
    pub(crate) drm_syncobj_state: Option<smithay::wayland::drm_syncobj::DrmSyncobjState>,
    /// Implicit and explicit fence watchers keyed by their Wayland client. Signalled
    /// sources remove themselves; disconnect cleanup removes the rest.
    pub(crate) dmabuf_blocker_sources: HashMap<ClientId, Vec<(RegistrationToken, WlSurface)>>,
    pub(crate) dmabuf_blocker_source_count: usize,
    pub xdg_shell_state: XdgShellState,
    /// `zxdg_decoration_manager_v1`: lets GTK/Qt clients ask whether to draw
    /// their own title bar (client-side decorations). TideWM enforces
    /// server-side (which, since TideWM itself draws no decorations, means
    /// no decorations at all) -- the tiling-WM convention (sway, Hyprland):
    /// a tiling layout wastes space under a client-drawn header bar, and
    /// floating windows focus/move/resize through Super-modifier actions
    /// rather than a title bar. The handler is no-op-shaped beyond always
    /// pinning `Mode::ServerSide`; see `handlers/mod.rs`.
    pub xdg_decoration_state: XdgDecorationState,
    /// `org_kde_kwin_server_decoration_manager`: KDE's older competing
    /// decoration protocol, same purpose as `xdg_decoration_state` for
    /// clients that speak this one instead (mostly Qt/KDE apps without
    /// xdg-decoration support). Default mode `Server`; `new_decoration`
    /// announces that policy once per decoration object, and repeat
    /// `request_mode` requests are ignored to prevent the same feedback loop
    /// the xdg handler avoids.
    pub kde_decoration_state: KdeDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    /// `wp-pointer-constraints-v1`: lets a client ask the compositor to
    /// lock the pointer in place (FPS look) or confine it to a region
    /// (strategy games, modal menus). The actual enforcement happens in
    /// `input.rs`'s `PointerMotion` arm -- this just hosts the global and
    /// the constraint store, which lives per-surface in surface state
    /// (Smithay's design, not on `Smallvil`).
    pub pointer_constraints_state: PointerConstraintsState,
    /// `wp-relative-pointer-manager-v1`: lets a client opt into receiving
    /// relative motion events (deltas, not absolute coordinates), what
    /// FPS games read to drive camera look. Purely additive on the input
    /// path: `input.rs`'s `PointerMotion` arm calls `pointer.relative_
    /// motion(...)` once per event, and any client that bound
    /// `zwp_relative_pointer_v1` gets the deltas; nothing about the
    /// regular motion path changes.
    pub relative_pointer_state: RelativePointerManagerState,
    pub seat_state: SeatState<Smallvil>,
    pub data_device_state: DataDeviceState,
    /// `wp-primary-selection-unstable-v1`: select-to-copy and middle-click
    /// paste, also exposed to wlr-data-control clipboard managers.
    pub primary_selection_state: PrimarySelectionState,
    /// `wlr-data-control-unstable-v1`, the protocol clipboard managers
    /// (cliphist, clipman, wl-clip-persist) actually use to read/write the
    /// clipboard without being the focused client -- `wl_data_device` alone
    /// only lets a focused client see the selection. Smithay's convenience
    /// module handles the protocol directly; no per-request handler logic
    /// needed here beyond the getter `DataControlHandler` requires (see
    /// `handlers/mod.rs`).
    pub data_control_state: DataControlState,
    /// `ext-session-lock-v1`: lets a privileged client (swaylock, a
    /// hyprlock-style daemon) take over every output and gate all
    /// window/layer input until it decides to unlock. See
    /// `handlers/mod.rs`'s `SessionLockHandler` impl (thin, adapts
    /// Smithay's handler shape) and `lock_session`/`unlock_session`/
    /// `register_lock_surface` below (the actual state machine).
    pub session_lock_state: SessionLockManagerState,
    pub(crate) session_lock: SessionLock,
    /// Client currently responsible for the fail-closed lock. If it dies
    /// without unlocking, TideWM terminates the compositor session so the
    /// login manager can recover; it never reveals the existing desktop.
    session_lock_client: Option<ClientId>,
    /// One `LockSurface` per output, registered via `register_lock_surface`
    /// as clients call `get_lock_surface`. Rendered full-screen in place of
    /// every window/layer-shell surface while `session_lock` isn't
    /// `Unlocked` -- see `lock_render_elements`.
    pub(crate) lock_surfaces: HashMap<Output, LockSurface>,
    /// Outputs that have rendered at least one locked (blanked, or real
    /// lock-surface) frame since the current `Locking` attempt began.
    /// `ext-session-lock-v1` requires *every* output to present a locked
    /// frame -- not just one -- before the `locked` event may be sent, or a
    /// second monitor could still be showing the unlocked desktop. Distinct
    /// from Smithay's own internal (and unrelated)
    /// `SessionLockManagerState::locked_outputs`, which only tracks
    /// duplicate `get_lock_surface` calls per output.
    pub(crate) locked_outputs: HashSet<Output>,
    /// Cached full-output blank fill shown behind (or instead of) a lock
    /// surface, one per output. `SolidColorBuffer` tracks its own commit
    /// counter and only bumps it when size/color actually changes -- same
    /// "don't rebuild every frame" shape as `Toast`/`WindowGroup::strip` --
    /// but that dedup only works per-buffer: a single shared buffer across
    /// every output would flip size on every call in a multi-monitor lock
    /// with differently-sized outputs, defeating the dedup and forcing a
    /// texture re-upload per output per frame. Pruned on output disconnect
    /// alongside `lock_surfaces`.
    pub(crate) lock_blank: HashMap<Output, SolidColorBuffer>,
    /// `xdg_activation_v1`: focus handoff on request (click a link, the
    /// browser window gets focus). Handler and token policy live in
    /// `handlers/mod.rs`; the grant path itself is
    /// `Smallvil::activate_toplevel`.
    pub xdg_activation_state: XdgActivationState,
    pub xdg_toplevel_icon_manager: XdgToplevelIconManager,
    /// `wp_single_pixel_buffer_manager_v1`: clients create 1x1 solid-color
    /// buffers without a SHM round-trip. No handler logic; Smithay's
    /// surface render element already knows how to draw these buffers, so
    /// advertising the global is the whole job.
    pub single_pixel_buffer_state: SinglePixelBufferState,
    /// `wp_presentation`: frame-presentation feedback for clients that
    /// want precise vsync timing (video players, benchmarks). Feedback is
    /// collected per frame by `take_presentation_feedback` and presented
    /// by each backend's render loop. `clock` is the MONOTONIC source the
    /// feedback timestamps are taken from.
    pub presentation_state: PresentationState,
    pub clock: Clock<Monotonic>,
    /// `zwp_keyboard_shortcuts_inhibit_v1`: lets a VM/remote-desktop
    /// client capture combos the compositor would otherwise intercept
    /// (Alt+Tab etc.) for its guest. The gate itself is in `input.rs`'s
    /// keyboard filter -- an active inhibitor on the keyboard-focused
    /// surface forwards everything instead of matching binds.
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    /// `zwp_pointer_gestures_v1`: touchpad swipe/pinch/hold for clients
    /// that bind it (browsers, map apps, image viewers). Pure global
    /// advertisement -- no handler trait exists; `input.rs` forwards the
    /// backend's libinput gesture events to the pointer handle, and
    /// Smithay's own machinery delivers them to whichever client created
    /// gesture objects. Kept so the `GlobalId` isn't dropped.
    #[allow(dead_code)]
    pub pointer_gestures_state: PointerGesturesState,
    /// `wp_cursor_shape_manager_v1`: lets a client (Qt6/GTK4 toolkits,
    /// which increasingly prefer this over drawing/positioning their own
    /// cursor surface) ask the compositor to show a named system cursor
    /// shape directly. No handler trait: Smithay's own `SetShape` dispatch
    /// calls the existing `SeatHandler::cursor_image` (`handlers/mod.rs`)
    /// the exact same way a client-set cursor surface already does, so
    /// this needed zero new compositor-side plumbing beyond `cursor.rs`
    /// actually rendering the requested shape instead of always the
    /// theme's default glyph (see `Theme::render_element`).
    #[allow(dead_code)]
    pub cursor_shape_manager_state: CursorShapeManagerState,
    /// `zwp_text_input_v3`: lets a text field (browser address bar,
    /// terminal search, GTK/Qt entry widgets) tell the compositor it wants
    /// an input method -- content type, surrounding text, cursor rect. No
    /// handler trait: Smithay wires text-input focus automatically off
    /// `WlSurface`'s own `KeyboardTarget` impl, which already fires from
    /// `reconcile_keyboard_focus`'s existing `keyboard.set_focus` call, so
    /// this needs nothing beyond the global itself.
    #[allow(dead_code)]
    pub text_input_manager_state: TextInputManagerState,
    /// `zwp_input_method_v2`: the other side of text-input, for an actual
    /// IME (fcitx5, ibus, an on-screen keyboard) to receive activation and
    /// surrounding-text state and reply with `commit_string`/preedit.
    /// `InputMethodHandler` below only has to place its popup surface
    /// (candidate window) -- everything else, including composing with an
    /// active keyboard grab so WM keybinds still take priority over IME
    /// input, is Smithay's own machinery. Sandboxed security-context
    /// clients cannot see this privileged global.
    #[allow(dead_code)]
    pub input_method_manager_state: InputMethodManagerState,
    /// `zwp_virtual_keyboard_v1`: lets a privileged client (an on-screen
    /// keyboard, ydotool/wtype-style tools, an IME's fallback path) inject
    /// synthetic key events. Each one is an input device: its keys go
    /// through `process_input_event` like real hardware, binds included
    /// (`handlers/virtual_keyboard.rs`). Like input-method, the global is
    /// hidden from sandboxed security-context clients.
    #[allow(dead_code)]
    pub virtual_keyboard_manager_state: VirtualKeyboardManagerState,
    /// A virtual keyboard's keymap currently on the seat keyboard, if any.
    pub(crate) virtual_keyboard_keymap:
        Option<crate::handlers::virtual_keyboard::VirtualKeyboardKeymap>,
    /// The configured keymap, saved while a virtual keyboard's replaces it.
    pub(crate) configured_keymap: Option<smithay::input::keyboard::KeymapFile>,
    /// `ext-foreign-toplevel-list-v1`: exposes every mapped toplevel to
    /// external tooling (bars, taskbars, window switchers) as a handle
    /// carrying title/app_id. One handle per mapped toplevel, created in
    /// `map_toplevel` and closed in `detach_mapped_toplevel` (so both an
    /// xdg unmap and role destruction retire it); title/app_id changes are
    /// pushed from `handle_commit`. Read-only by design: this protocol has
    /// no activation/close requests, so a bar that wants those needs
    /// xdg-activation on top.
    pub foreign_toplevel_list_state: ForeignToplevelListState,
    pub(crate) foreign_toplevels: HashMap<WlSurface, ForeignToplevelHandle>,
    /// Compositor-global numeric IDs for mapped toplevels. Wayland object
    /// IDs are only unique within one client, so they cannot safely identify
    /// a window across DBus/IPC when two applications both own `wl_surface@7`.
    pub(crate) foreign_toplevel_numeric_ids: HashMap<WlSurface, u64>,
    pub(crate) next_foreign_toplevel_numeric_id: u64,
    /// `wlr-foreign-toplevel-management-v1` (the older, bidirectional
    /// protocol that waybar's `wlr/taskbar` module and ags v1 hardcode
    /// against). Coexists with `foreign_toplevel_list_state` (the newer
    /// read-only `ext-foreign-toplevel-list-v1`); clients pick whichever
    /// they were written for. The two state machines are independent:
    /// this one is hand-rolled on `wayland-protocols-wlr` because
    /// Smithay 0.7 has no module for the older protocol, unlike the newer
    /// one. See `handlers/wlr_foreign_toplevel.rs`.
    pub wlr_foreign_toplevel_state:
        Option<crate::handlers::wlr_foreign_toplevel::WlrForeignToplevelState>,
    pub(crate) wlr_foreign_toplevels:
        HashMap<WlSurface, crate::handlers::wlr_foreign_toplevel::WlrForeignToplevelHandle>,
    /// `ext-workspace-v1`: workspace listing/activation for bars (waybar
    /// has no compositor-specific TideWM module, so this is the
    /// protocol-native alternative to polling `tidectl`). Hand-rolled,
    /// same reason as the toplevel/output-management protocols above --
    /// no Smithay module exists for it. See `handlers/ext_workspace.rs`
    /// for the object model and this first pass's deliberate scope.
    pub(crate) ext_workspace_state: crate::handlers::ext_workspace::ExtWorkspaceState,
    /// `wlr-output-management-unstable-v1`: kanshi/`wlr-randr`/wdisplays read
    /// output layout and can push position/transform/scale changes back.
    /// Hand-rolled, same reason as the toplevel-management protocol above --
    /// no Smithay module exists for it. See
    /// `handlers/wlr_output_management.rs` for what this first pass does and
    /// deliberately doesn't support yet.
    pub wlr_output_management_state:
        crate::handlers::wlr_output_management::WlrOutputManagementState,
    /// `wlr-output-power-management-unstable-v1`: on/off per output
    /// (wlogout-style tools, a QuickShell power widget). See
    /// `handlers/wlr_output_power_management.rs`.
    pub wlr_output_power_management_state:
        crate::handlers::wlr_output_power_management::WlrOutputPowerManagementState,
    /// Backend hook for the above: `Some` only under the udev backend
    /// (installed by `backend::udev::init_udev` once its device `Rc`
    /// exists), where turning a CRTC off/on threads through the same DRM
    /// surface the render loop drives. `None` under winit -- there's no
    /// real display to power down for a nested window, so a power request
    /// there is tracked as bookkeeping only (see the handler module).
    /// Returns whether the change actually applied.
    #[allow(clippy::type_complexity)]
    pub set_output_power: Option<Box<dyn FnMut(&Output, bool) -> bool>>,
    /// `zwlr_gamma_control_manager_v1`: night-light (wlsunset/gammastep).
    /// See `handlers/wlr_gamma_control.rs`.
    pub wlr_gamma_control_state: crate::handlers::wlr_gamma_control::WlrGammaControlState,
    /// Backend hooks for the above, both `Some` only under udev (`None`
    /// under winit -- a nested output has no real color pipeline to
    /// adjust). `gamma_size` reads the CRTC's LUT size; `set_gamma` applies
    /// a ramp (also used to reset to linear on control destroy).
    #[allow(clippy::type_complexity)]
    pub gamma_size: Option<Box<dyn FnMut(&Output) -> Option<u32>>>,
    #[allow(clippy::type_complexity)]
    pub set_gamma: Option<Box<dyn FnMut(&Output, &[u16], &[u16], &[u16]) -> bool>>,
    /// `wp_fractional_scale_manager_v1`: lets clients render at
    /// non-integer output scales (the `output { scale }` config knob
    /// already accepts fractions; without this global clients only ever
    /// saw the integer-rounded `wl_output` scale). Preferred scales are
    /// pushed to surfaces from `new_fractional_scale` (handlers/mod.rs)
    /// and refreshed by `set_window_fractional_scale` as windows are
    /// (re)placed on outputs.
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub popups: PopupManager,
    /// XDG popup roles which currently have no committed buffer. Smithay's
    /// `PopupManager` tracks role/tree lifetime, not protocol mapping, so a
    /// persistent popup role needs this separate signal across null-buffer
    /// unmap and later remap just like `unmapped_toplevels` below.
    pub(crate) unmapped_popup_surfaces: HashSet<WlSurface>,
    /// Active xdg-popup pointer/keyboard grab and the root shell surface it
    /// belongs to. The focus authority treats a live popup keyboard grab as
    /// the root retaining logical focus even while wl_keyboard targets a
    /// descendant popup surface.
    pub(crate) popup_grab: Option<PopupGrabState>,
    /// XDG toplevel roles that exist but do not currently have a committed
    /// buffer. Protocol mapping is deliberately tracked separately from
    /// `Space`: switching workspaces also removes windows from `Space`, but
    /// those windows remain mapped from the client's point of view.
    ///
    /// A toplevel starts here, moves into `Layouts`/`floating_workspace` on
    /// its first non-null buffer, and returns here when it commits a null
    /// buffer. Keeping the `Window` handle lets a later remap repeat the
    /// initial configure/map sequence without confusing it with destruction.
    pub unmapped_toplevels: HashMap<WlSurface, Window>,
    /// Layer-shell roles registered in an output's `LayerMap` which do not
    /// currently have a committed buffer. `LayerMap` registration is kept
    /// across unmap so the next initial configure can be arranged on the
    /// same output; this set is the separate protocol-mapping signal used
    /// for focus and commit transitions.
    pub unmapped_layer_surfaces: HashSet<WlSurface>,
    /// `wp_viewporter`: no handler logic of our own (`on_commit_buffer_handler`,
    /// already wired into `handlers/compositor.rs`, validates viewport state on
    /// every commit for us), but `xwayland-satellite` hard-requires the global
    /// to exist -- it panics on startup without it.
    pub viewporter_state: ViewporterState,
    /// `zwp-tablet-v2`: drawing-tablet/pen support. Real Smithay convenience
    /// module (unlike `wp-tearing-control-v1`, which had none) -- this just
    /// registers the global; per-device advertisement and per-event
    /// forwarding live in `tide_core/input.rs`'s `DeviceAdded`/`TabletTool*`
    /// handling, and `handlers/mod.rs`'s `TabletSeatHandler` impl.
    #[allow(dead_code)]
    pub tablet_manager_state: TabletManagerState,
    /// `ext-image-copy-capture-v1` (screenshot) protocol states, see
    /// `handlers/capture.rs`. Only output sources are supported.
    pub image_capture_source_state: ImageCaptureSourceState,
    pub output_capture_source_state: OutputCaptureSourceState,
    pub toplevel_capture_source_state: ToplevelCaptureSourceState,
    pub image_copy_capture_state: ImageCopyCaptureState,
    /// `wlr-screencopy-unstable-v1` global (grim's native protocol, the one
    /// with region capture), hand-rolled in `handlers/screencopy.rs`. Kept
    /// because dropping the `GlobalId` would remove the global.
    #[allow(dead_code)]
    pub wlr_screencopy_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// `wp-tearing-control-v1` global, hand-rolled in
    /// `handlers/tearing_control.rs`. Kept for the same reason as
    /// `wlr_screencopy_global` above.
    #[allow(dead_code)]
    pub tearing_control_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// `hyprland-toplevel-export-v1` global (per-window capture by wlr
    /// toplevel handle), hand-rolled in `handlers/hyprland_toplevel_export.rs`.
    /// Kept for the same reason as `wlr_screencopy_global` above.
    #[allow(dead_code)]
    pub hyprland_toplevel_export_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// Owned capture sessions. A `Session` stops itself on drop, so it must
    /// be kept as long as the client wants it; dead ones are filtered out on
    /// the backend cleanup ticks (`cleanup_capture`).
    pub(crate) capture_sessions: Vec<CaptureSession>,
    /// Validated capture requests waiting for a backend render loop (which
    /// owns the GL renderer) to produce pixels. See `capture.rs`.
    pub pending_captures: Vec<PendingCapture>,
    /// `zwp_idle_inhibit_manager_v1`: only has an observable effect through
    /// `idle_notifier_state.set_is_inhibited` below -- TideWM has no
    /// idle/DPMS/lock behavior of its own to suppress.
    pub idle_inhibit_manager_state: IdleInhibitManagerState,
    /// `ext-idle-notifier-v1`, for an external tool (a swayidle-style daemon)
    /// to watch idle/resume transitions and act on them (screen off, lock,
    /// suspend). `notify_activity` is driven from every real input event in
    /// `input.rs`; `set_is_inhibited` from `idle_inhibitors` below.
    pub idle_notifier_state: IdleNotifierState<Smallvil>,
    /// Live inhibitor count per surface. A client can create more than one
    /// `zwp_idle_inhibitor_v1` on the same surface -- Smithay calls
    /// `inhibit`/`uninhibit` per inhibitor *object*, not deduplicated by
    /// surface -- so a plain `HashSet` would drop the whole surface the
    /// moment any *one* of its inhibitors was destroyed, un-inhibiting while
    /// another inhibitor on the same surface is still alive. Whether this
    /// map is empty or not is the only thing that drives
    /// `idle_notifier_state`'s inhibited flag -- individual surfaces are
    /// never checked for visibility (matching most compositors' simplest-
    /// correct behavior; see `IdleInhibitHandler`'s own doc comment).
    pub idle_inhibitors: HashMap<WlSurface, usize>,

    pub seat: Seat<Self>,
    /// The window Tide intends to focus when no higher-priority layer owns
    /// the keyboard. Kept separately from actual seat focus so an Exclusive
    /// layer can preempt temporarily and then restore the exact window.
    window_focus: Option<WlSurface>,
    /// Explicit/first-map OnDemand layer focus. Unlike Exclusive layers it
    /// is not globally preemptive; a later window/empty click clears it.
    on_demand_layer_focus: Option<WlSurface>,
    /// Last target applied to both wl_keyboard focus and XDG Activated.
    /// Every mutation goes through `reconcile_keyboard_focus`.
    keyboard_focus: KeyboardFocusTarget,

    /// Only populated by the udev backend, which is also the only place
    /// the `zwp_linux_dmabuf_v1` global actually gets created (see
    /// `backend/udev.rs`). Under winit, `dmabuf_state` sits unused: no
    /// global means no client ever binds it, so `DmabufHandler` methods
    /// never fire.
    pub dmabuf_state: DmabufState,
    pub(crate) native_dmabuf_feedback: Option<crate::backend::udev::NativeDmabufFeedback>,
    pub dmabuf_global: Option<DmabufGlobal>,
    /// Shared multi-GPU manager and selected primary render node. The udev
    /// render loop and DMA-BUF handler borrow it only on the single calloop
    /// thread, so imports cannot race a frame render.
    pub(crate) udev_gpu: Option<crate::backend::udev::UdevGpu>,
    /// Only populated by the udev backend (`backend/udev.rs`), which is the
    /// only place VT switching means anything -- under winit a host
    /// compositor already owns that. `input.rs`'s VT-switch keybind
    /// detection calls `change_vt` on this when it's `Some`.
    pub session: Option<LibSeatSession>,

    /// Only read by the udev backend (see `cursor.rs`, `backend/udev.rs`).
    /// Under winit the host compositor draws the real cursor, so this is
    /// tracked but never rendered from.
    pub cursor_status: CursorImageStatus,
    /// Icon surface of the active client drag'n'drop, drawn at the pointer.
    pub dnd_icon: Option<WlSurface>,

    /// Loaded xcursor theme for `CursorImageStatus::Named`, only populated
    /// by the udev backend (`backend/udev.rs`, same pattern as `session`/
    /// `udev_gpu` above). `None` under winit, and also under udev if no
    /// theme could be loaded -- `cursor::fallback_glyph_element` covers that
    /// case, see `cursor.rs`.
    pub cursor_theme: Option<crate::cursor::Theme>,

    /// Windows currently fullscreen, keyed by surface. This is the
    /// authoritative "is this fullscreen right now" source `retile()` reads
    /// -- `ToplevelSurface::current_state()` only reflects the last surface
    /// the *client* acked, a round-trip behind `with_pending_state`, so it
    /// can't drive rendering decisions directly (same reason anvil keeps its
    /// own `FullscreenSurface` marker rather than relying on it).
    pub fullscreen: HashMap<WlSurface, FullscreenEntry>,

    /// Floating windows with a pending maximized placement. Kept separately
    /// from FullscreenEntry so fullscreen can temporarily suppress the
    /// protocol Maximized state and restore it on exit without losing the
    /// original normal floating rectangle.
    pub maximized: HashMap<WlSurface, MaximizedEntry>,

    /// (output, workspace) tag for every *floating* window. Tiled windows
    /// don't need one -- their workspace is implicit in which `Layouts` tree
    /// holds them -- but a floating window isn't tracked in `Layouts` at
    /// all, so `switch_workspace` needs this to know which floating windows
    /// belong to the workspace it's hiding or showing.
    pub floating_workspace: HashMap<WlSurface, FloatingTag>,

    /// Windows exempt from `switch_workspace`'s hide/show cycle -- stay
    /// mapped and visible no matter which workspace is active on their
    /// output. Always floating (`toggle_pin` un-tiles first if needed; a
    /// tiled pinned window has no coherent meaning when only one
    /// workspace's tree is ever rendered per output at a time).
    pub pinned: HashSet<WlSurface>,

    /// Per-output "workspace to return to" for `toggle_scratchpad`, so
    /// toggling the scratchpad off doesn't strand you on some fixed
    /// fallback workspace regardless of where you actually were.
    scratchpad_previous: HashMap<String, u32>,

    /// Windows hidden by swallowing (`WindowRule::swallow`), keyed by the
    /// *child* surface that took over their tile. Holds the actual
    /// `Window` handle, not just the surface -- a hidden window isn't in
    /// `space.elements()` at all, so the surface alone couldn't get it
    /// back (same lesson `FloatingTag` already encodes). Restored by
    /// `restore_swallowed` when the child unmaps or is destroyed.
    pub swallowed: HashMap<WlSurface, SwallowedWindow>,

    /// Per-surface map/unmap timing, used to detect the "flutter storm":
    /// a client that nulls its buffer and immediately maps again, over
    /// and over (a video player refusing a tiled size re-realizes its
    /// surface on every refusal). Each quick flip increments `flips`;
    /// once enough accumulate inside a short window the surface is marked
    /// `flutter_floated` and `map_toplevel` floats it instead of tiling,
    /// which is the one outcome that ends the storm. Pruned lazily to
    /// surfaces the compositor still tracks.
    pub lifecycle_flutter: HashMap<WlSurface, LifecycleFlutter>,

    /// Per-surface expected tiled size: the `state.size` most recently
    /// sent to a tiled window by `retile_with_viscosity`. Compared against
    /// the size the client actually commits, to catch the other half of
    /// the size-refusal class: a client that *stays mapped* (no map/unmap
    /// flip, so `lifecycle_flutter` never sees it) but renders at a
    /// different size than the tile it was given -- a video player with a
    /// locked aspect renders half-empty inside its slot forever. See
    /// `note_tiled_size_refusal`. Pruned alongside `lifecycle_flutter`.
    pub expected_tiled_size: HashMap<WlSurface, Size<i32, Logical>>,

    /// Consecutive commits whose size didn't match `expected_tiled_size`,
    /// per surface, with the time of the first such commit. Used by
    /// `note_tiled_size_refusal` to distinguish "client is still mid-
    /// resize" (a couple of stale commits, then it matches) from "client
    /// refuses this size outright" (never matches). Reset on a matching
    /// commit or a changed expected size.
    pub tiled_size_refusals: HashMap<WlSurface, TiledSizeRefusal>,

    /// Surfaces given up on after a `lifecycle_flutter` storm or a
    /// `tiled_size_refusals` streak: they float on every subsequent map,
    /// never tiled again.
    pub flutter_floated: HashSet<WlSurface>,

    /// `(app_id, title)` captured once at a surface's true first map, never
    /// overwritten by a later unmap/remap (the flutter-storm case) or a
    /// live `set_app_id`/`set_title` -- what `initial_class`/`initial_title`
    /// window-rule criteria match against. Populated in `map_toplevel`,
    /// pruned in `toplevel_destroyed` (not on ordinary unmap, since a
    /// remapped surface should keep its original spawn identity).
    pub initial_toplevel_identity: HashMap<WlSurface, (Option<String>, Option<String>)>,

    /// Last known floating size per app_id, for `rule { persistent_size =
    /// true }` (Hyprland). Recorded in `detach_mapped_toplevel` from a
    /// closing floating window, reapplied at the next map of the same
    /// app_id in `map_toplevel`. In-memory only, keyed by app_id rather
    /// than surface since the whole point is surviving past this window's
    /// own lifetime.
    pub remembered_floating_sizes: HashMap<String, (i32, i32)>,

    /// Named scratchpads (Hyprland's named "special workspaces"):
    /// each name is lazily assigned its own reserved workspace number from
    /// `NAMED_SCRATCHPAD_BASE` upward on first use, then behaves exactly
    /// like the unnamed scratchpad does -- same `switch_workspace` hide/show
    /// machinery, no parallel data structure. Session-scoped: numbers are
    /// allocated in first-use order, which only matters to IPC consumers,
    /// and those should key on the `scratchpad` name field instead.
    scratchpad_named: HashMap<String, u32>,

    /// Per-output previously-active workspace, updated on every real
    /// `switch_workspace` call -- drives `config.workspace_auto_back_and_forth`
    /// (niri's own feature of the same name). Separate from
    /// `scratchpad_previous` above: that one only tracks the scratchpad's
    /// own toggle, this tracks every ordinary workspace switch.
    workspace_previous: HashMap<String, u32>,
    /// Output that received the last `workspace-up`/`workspace-down` step.
    /// Used only while no visible window holds focus (an empty workspace):
    /// `primary_output` then falls back to the pointer, which may be on the
    /// other monitor and would step the wrong screen on the way back.
    workspace_step_output: Option<String>,

    /// Tiled windows that keep `config.pseudo_tile_scale` of their tile's
    /// size instead of filling it -- a rect override `retile()` applies,
    /// same shape as the fullscreen override. Stays in its `Layouts` slot
    /// throughout, unlike floating: only the rendered rect changes.
    pub pseudo_tiled: HashSet<WlSurface>,

    /// Windows asking for attention while they didn't have a fresh enough
    /// claim to steal focus outright -- set from a stale-serial
    /// xdg-activation request (see `Smallvil::mark_urgent`,
    /// `handlers/mod.rs`'s `request_activation`), cleared by focusing the
    /// window through any path (`reconcile_keyboard_focus`) or explicitly
    /// via `focus-urgent` (`Smallvil::focus_urgent`).
    pub urgent: HashSet<WlSurface>,

    /// Last time each urgent window's repeat pulse fired, keyed by surface.
    /// Reconciled against `urgent` inside `update_urgent_pulses` itself
    /// (entries for surfaces no longer urgent are dropped there), so the
    /// several paths that clear `urgent` -- focus, unmap, `focus-urgent` --
    /// don't each have to remember to keep this map in sync. Same
    /// reconcile-at-the-tick pattern the workspace-switch `Space` cache
    /// staleness bugs taught: one authoritative point, not N call sites.
    pub(crate) urgent_pulse_last: HashMap<WlSurface, Instant>,
    /// Throttle for `update_urgent_pulses`, same shape as
    /// `depth_last_tick`'s 10Hz gate for `update_window_depths`.
    urgent_pulse_tick: Instant,

    /// Arbitrary string tags carried by a window (Hyprland tags/sway
    /// marks), keyed by surface. No entry means "no tags" -- an untagged
    /// window (the overwhelming common case) costs nothing beyond the
    /// `HashMap`'s own empty state. Populated from a matching rule's
    /// `add_tag` at first map (`map_toplevel`) and from `tag-window`/
    /// `untag-window` at any time; read live by `resolve_window_rules_for`
    /// for the `rule { tag = [...] }` match criterion. Cleared per-surface
    /// in `detach_mapped_toplevel`, same lifecycle as `urgent`/`pinned`.
    pub window_tags: HashMap<WlSurface, HashSet<String>>,

    /// Output-pin bindings (`rule { pin_output }`, `toggle-output-pin`),
    /// keyed by surface, valued by connector name. No entry means "not
    /// pinned" -- the common case costs nothing. Kept independent of
    /// whether the named output is currently connected: a disconnect
    /// leaves the entry alone (the window falls back like any other, but
    /// still remembers where it belongs), and reconnecting that output
    /// recalls the window via `recall_pinned_windows_to`. Cleared
    /// per-surface in `detach_mapped_toplevel`, same lifecycle as
    /// `window_tags`.
    pub output_pins: HashMap<WlSurface, String>,

    /// (output, workspace) pairs that have already fired their
    /// `workspace_rule { on_created_empty }` command (see
    /// `apply_workspace_switch`). TideWM's numbered workspaces always
    /// exist as addressable slots -- there is no real create/destroy
    /// lifecycle to hook a one-shot event on the way Hyprland can -- so
    /// this set is what makes the closest honest analog to that a
    /// once-per-process-lifetime event instead of firing on every later
    /// visit to an empty workspace. Pruned per-output in
    /// `remove_workspace_transition_output` on disconnect, same
    /// "connector name might mean something else on replug" reasoning as
    /// that function's other entries.
    workspace_created_empty_fired: HashSet<(String, u32)>,

    /// Most-recently-focused-first order of every window that has ever
    /// held keyboard focus, updated from `reconcile_keyboard_focus`'s own
    /// activation-change block (except while `cycling_focus` is set, see
    /// that field) and pruned in `detach_mapped_toplevel`. Drives
    /// `cycle_focus`'s Alt-Tab ordering; a window not yet in this list
    /// (freshly mapped but never focused) falls back to `Space`'s own
    /// order, appended after every known entry.
    pub(crate) focus_history: Vec<WlSurface>,
    /// Set for the duration of `cycle_focus`'s own `focus_window` call, so
    /// `reconcile_keyboard_focus` skips its normal `focus_history` reorder.
    /// Without this, cycling itself would move whatever it just focused to
    /// the front of the MRU list, which the *next* press then reads back as
    /// "current" -- degenerating into an A/B two-window oscillation instead
    /// of a real cycle, since the third-oldest window's position keeps
    /// getting pushed out from under the walk before it's ever reached.
    /// Found by manual trace, not observed live -- `cargo test` cannot
    /// catch this class of bug (needs a real multi-press keybind sequence).
    cycling_focus: bool,

    /// Laptop lid closed right now, per libinput's `Switch::Lid`
    /// (`SwitchState::On` = closed). Tracked separately from the
    /// configured `lid_close`/`lid_open` actions so an event whose state
    /// matches what we already have is a no-op -- libinput sometimes
    /// re-emits the current state after a suspend/resume cycle, and
    /// without this dedup that would re-fire the action every wake.
    /// Defaults to `false` (open); libinput does not report initial switch
    /// state at seat-assign time, so a lid already closed when TideWM
    /// starts won't be reflected here until the first real toggle. Same
    /// shape as `is_tablet_mode` below.
    pub is_lid_closed: bool,

    /// Device is in tablet mode right now, per libinput's
    /// `Switch::TabletMode` (`SwitchState::On` = tablet mode). Same
    /// dedup-vs-libinput-resume rationale as `is_lid_closed`.
    pub is_tablet_mode: bool,

    /// Windows sharing one `Layouts` leaf, tab-strip style: only the
    /// `active` member actually occupies the leaf (and so is mapped/
    /// visible) at any time, following this codebase's own established
    /// pattern for "windows share a slot, only one shows" (see
    /// `fullscreen`/`maximized`/`pseudo_tiled`/`floating_workspace` above)
    /// rather than restructuring `layout::Node::Leaf` to hold more than one
    /// `Window`. A flat `Vec` plus a linear scan (`group_of`) rather than a
    /// second reverse-index map kept in sync with it -- groups are few and
    /// small, and this codebase has hit the "two structures drift apart"
    /// bug class more than once already (`FloatingTag` staleness, the
    /// `workspace_of`/`window_of` fix). One source of truth.
    pub groups: Vec<WindowGroup>,
    /// Stable AccessKit node IDs for group/tab UI. Allocated monotonically
    /// instead of deriving from vector indices or per-client Wayland object
    /// numbers, both of which can collide or be reused.
    next_ui_node_id: u64,

    /// `org.gnome.Mutter.ScreenCast` DBus service handle. `None` until
    /// `main.rs` assigns it (after backend init, since the initial output
    /// snapshot needs real outputs to exist) and always `None` when the
    /// `screencast` feature is off. See `src/screencast/mod.rs`.
    #[cfg(feature = "screencast")]
    pub screencast: Option<crate::screencast::ScreencastState>,

    /// `org.freedesktop.a11y.KeyboardMonitor` DBus service handle. `None`
    /// until `main.rs` assigns it and always `None` when the
    /// `accessibility` feature is off. See `src/accessibility/mod.rs`.
    #[cfg(feature = "accessibility")]
    pub accessibility: Option<crate::accessibility::AccessibilityState>,
    /// Coalesces high-frequency redraw causes (pointer motion, client
    /// damage) into bounded accessibility snapshot work.
    #[cfg(feature = "accessibility")]
    accessibility_sync_timer_armed: bool,
}

/// One member of a `WindowGroup`.
/// What `emit_state_change_events` last reported.
#[derive(Clone)]
struct IpcSeenState {
    outputs: HashMap<String, u64>,
    mode: Option<String>,
    engine: &'static str,
    scroll: HashMap<String, u64>,
}

fn engine_name(engine: crate::config::SpatialEngine) -> &'static str {
    match engine {
        crate::config::SpatialEngine::Classic => "classic",
        crate::config::SpatialEngine::Ocean => "ocean",
        crate::config::SpatialEngine::Scroll => "scroll",
    }
}

pub struct GroupMember {
    pub ui_node_id: u64,
    pub surface: WlSurface,
    /// Only meaningful while this member is *not* the group's active one --
    /// the tree/`space` are authoritative for the active member instead.
    /// Same convention as `FloatingTag::rect` being stale while its window
    /// is visible: a parked member is unmapped (`space.unmap_elem`), so
    /// (per the same gotcha `FloatingTag` exists for) it isn't in
    /// `space.elements()` to re-find later -- this is the only place its
    /// `Window` handle survives while parked.
    pub parked_window: Option<Window>,
}

pub struct WindowGroup {
    pub ui_node_id: u64,
    /// Classic tree owner used for activation and reinsertion. Ocean groups
    /// derive their live reef from the active leaf; these fields are cached
    /// only for UI affinity and refreshed when migrating back to Classic.
    pub output: String,
    pub workspace: u32,
    /// Tab order. Always at least 2 -- a "group" of one collapses back to a
    /// plain tile (see `Smallvil::ungroup`).
    pub members: Vec<GroupMember>,
    /// Index into `members` of whichever one currently occupies the tree
    /// leaf.
    pub active: usize,
    /// Cached tab-strip texture (see `tab_strip.rs`), same model as
    /// `Toast`: built once, not every frame. `None` forces a rebuild on the
    /// next `tab_strip_elements` pass -- set whenever membership/active
    /// changes; a plain leaf-width change (an output resize, a sibling
    /// split drag) is instead caught by comparing against `strip_width`
    /// there, no separate invalidation needed for that case.
    strip: Option<MemoryRenderBuffer>,
    strip_width: i32,
}

pub struct FloatingTag {
    /// A hidden window isn't mapped, so it isn't in `space.elements()`
    /// either -- this is the only place its `Window` handle survives while
    /// its workspace isn't the active one, needed to map it again later.
    pub window: Window,
    pub output: String,
    pub workspace: u32,
    /// Last known position+size, refreshed every time `switch_workspace`
    /// hides this window, and read back to restore it exactly when its
    /// workspace becomes active again. Stale while the window is actually
    /// visible (`space`'s own position is authoritative then), but that's
    /// fine since it's only ever read right after a hide.
    pub rect: Rectangle<i32, Logical>,
}

pub struct FullscreenEntry {
    /// Output name (matching `Layouts`' own convention of a stable `String`
    /// rather than holding the `Output` type itself).
    pub output: String,
    /// The window's rect immediately before it went fullscreen, so it can be
    /// restored exactly. Applied only when the window is floating at
    /// unfullscreen time. It may remain populated while temporarily tiled
    /// so a floating -> tiled -> floating round trip does not discard the
    /// original rect; a window that stays tiled falls back through its
    /// intact `Layouts` slot instead.
    pub restore_rect: Option<Rectangle<i32, Logical>>,
    /// Pinning is output-local and workspace-independent, while fullscreen is
    /// owned by one workspace/output transaction. Suspend the pin during
    /// fullscreen and restore it only if the window exits still floating.
    pub was_pinned: bool,
    /// Whether the *most recent* `wants_pinned` toggle in `toggle_pin` was
    /// the one that floated this window (it was tiled at the time). Turning
    /// pin back off needs to know whether to undo that specific float --
    /// re-tiling a window that was already floating on its own before it
    /// got pinned would be wrong, so `was_pinned` alone (which only says
    /// "is pin currently wanted") isn't enough information by itself.
    pub pin_floated_it: bool,
}

pub struct MaximizedEntry {
    pub output: String,
    pub restore_rect: Rectangle<i32, Logical>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum KeyboardFocusTarget {
    None,
    Window(WlSurface),
    Layer(WlSurface),
    Lock(WlSurface),
}

/// Session lock state machine: `Unlocked` -> `Locking` -> `Locked` ->
/// `Unlocked`. Entered via `SessionLockHandler::lock` (`handlers/mod.rs`),
/// which calls `Smallvil::lock_session` below.
pub(crate) enum SessionLock {
    Unlocked,
    /// Lock requested; waiting for every output to render at least one
    /// locked frame (see `locked_outputs`) before confirming via
    /// `SessionLocker::lock()`.
    Locking(SessionLocker),
    Locked,
}

/// What caused a focus change. Only navigation origins may move a Scroll
/// viewport; pointer origins must leave the strip exactly where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusOrigin {
    /// Keyboard actions, IPC (`focus-window`/`focus-app`), xdg-activation,
    /// notification clicks, map/close/workspace repairs, depth recall.
    Navigation,
    /// Pointer hover, press, touch contact or tablet tip. The pointer is
    /// already on the app, so the viewport must not move under it.
    Pointer,
}

/// Pointer space is surface space. A placement -- and `Space::element_location`
/// -- locates the window's `xdg_surface` *geometry*, and a client may declare
/// that geometry with a non-zero offset inside its surface. GTK reserves a CSD
/// shadow margin there (Firefox reports one; Qt/kitty report zero), and the
/// renderer already places the surface at `rect.loc - geometry().loc`, so any
/// input mapping built from a placement has to subtract the same margin or
/// every hover and click shifts by it. `Space::element_under` is the exception:
/// it returns the render location (surface origin) directly.
fn surface_origin(window: &Window, rect_loc: Point<i32, Logical>) -> Point<i32, Logical> {
    rect_loc - window.geometry().loc
}

/// Surface origin for a camera-scaled placement. Ocean renders a window's
/// surface tree around its *geometry* origin and then scales it by the camera
/// zoom; a client geometry offset therefore has to be scaled with it. Keeping
/// this beside `surface_origin` makes the input and renderer transforms use
/// the same coordinates.
fn surface_origin_for_view_parts(
    rect_loc: Point<i32, Logical>,
    geometry_loc: Point<i32, Logical>,
    view_scale: f64,
) -> Point<f64, Logical> {
    let scale = view_scale.max(0.05);
    Point::from((
        rect_loc.x as f64 - geometry_loc.x as f64 * scale,
        rect_loc.y as f64 - geometry_loc.y as f64 * scale,
    ))
}

fn surface_origin_for_view(
    window: &Window,
    rect_loc: Point<i32, Logical>,
    view_scale: f64,
) -> Point<f64, Logical> {
    surface_origin_for_view_parts(rect_loc, window.geometry().loc, view_scale)
}

/// Camera zoom is a compositor-side scale. Smithay's WlPointer applies a
/// client scale to the local coordinate after `PointerHandle` subtracts the
/// surface origin, so Ocean supplies the inverse zoom for the duration of one
/// pointer event. Zoom 1 (and every non-Ocean engine) needs no override.
fn client_scale_for_view_scale(view_scale: f64) -> Option<f64> {
    let scale = view_scale.max(0.05);
    if (scale - 1.0).abs() < 0.0001 {
        None
    } else {
        Some(1.0 / scale)
    }
}

/// Gesture streams captured by compositor bindings instead of forwarded via
/// wp-pointer-gestures. A stream is claimed at Begin and remains owned until
/// End so clients never receive a partial gesture.
pub(crate) enum CompositorGesture {
    Scroll {
        output: String,
        workspace: u32,
        initial_offset: f64,
    },
    Swipe {
        workspace_fallback: bool,
        /// Whether this gesture is being driven live by the swim camera
        /// (decided once at Begin: `workspace_fallback` and swim enabled).
        /// `Update` drags the camera per-event instead of only accumulating
        /// `delta_x`/`delta_y`, and `End` releases the camera's spring
        /// instead of resolving a one-shot discrete switch.
        swim: bool,
        delta_x: f64,
        delta_y: f64,
    },
    Pinch {
        scale: f64,
    },
    /// `pointer_modifier` + `modifier_pan_fingers` touchpad swipe: a real
    /// `PointerGrab` (the same types the mouse move/pan path uses) is
    /// already active by the time this is stored, driven by synthetic
    /// `MotionEvent`s and ended by `unset_grab` -- never a real button
    /// release. See `Smallvil::start_gesture_modifier_move`.
    ModifierMove {
        last_location: Point<f64, Logical>,
        completion: GrabCompletion,
    },
}

/// Per-tick, per-window context for F1 `full` tier's physics step:
/// resolved once in `Smallvil::update_float_physics_full`, reused across
/// every fixed-timestep substep. `anchor`/`output_bounds` are
/// `(x, y, w, h)` in global logical coordinates -- plain tuples rather than
/// `Rectangle<f64, Logical>` since every consumer does raw arithmetic on
/// the four components anyway (`float_physics::resolve_collision`/
/// `resolve_edge_collision` take the same shape).
struct FloatBodyContext {
    surface: WlSurface,
    anchor: (f64, f64, f64, f64),
    mass: f64,
    output_bounds: Option<(f64, f64, f64, f64)>,
}

pub(crate) struct PopupGrabState {
    pub(crate) root: WlSurface,
    pub(crate) grab: PopupGrab<Smallvil>,
    pub(crate) has_keyboard_grab: bool,
}

impl KeyboardFocusTarget {
    fn surface(&self) -> Option<&WlSurface> {
        match self {
            Self::None => None,
            Self::Window(surface) | Self::Layer(surface) | Self::Lock(surface) => Some(surface),
        }
    }
}

impl FullscreenEntry {
    /// Records the first useful windowed rect without overwriting an older
    /// floating rect. The latter must survive a floating -> tiled -> floating
    /// round trip performed while the fullscreen override is still active.
    fn remember_restore_rect(&mut self, rect: Rectangle<i32, Logical>) {
        self.restore_rect.get_or_insert(rect);
    }

    /// Moves the fullscreen override with the workspace content that owns it.
    /// A saved floating rect uses global logical coordinates, so it needs the
    /// same translation as the window itself when monitor origins differ.
    fn move_to_output(&mut self, output: String, delta: Option<Point<i32, Logical>>) {
        self.output = output;
        if let (Some(rect), Some(delta)) = (&mut self.restore_rect, delta) {
            rect.loc += delta;
        }
    }
}

impl MaximizedEntry {
    fn move_to_output(&mut self, output: String, delta: Option<Point<i32, Logical>>) {
        self.output = output;
        if let Some(delta) = delta {
            self.restore_rect.loc += delta;
        }
    }
}

/// Reserved workspace number for the scratchpad -- never reachable via the
/// default `Super+1..9,0`/`Super+Shift+1..9,0` keybinds (which only ever
/// address 1-10), so it stays inert on every output until something
/// explicitly moves a window there via `Smallvil::move_to_scratchpad`.
const SCRATCHPAD_WORKSPACE: u32 = 0;

/// Base of the reserved range named scratchpads allocate workspace numbers
/// from (`NAMED_SCRATCHPAD_BASE..=u32::MAX`, so 4096 names). High enough
/// that no numbered `workspace:N` bind plausibly collides, same "inert
/// unless explicitly addressed" reasoning as `SCRATCHPAD_WORKSPACE`.
const NAMED_SCRATCHPAD_BASE: u32 = u32::MAX - 4095;

/// Whether `workspace` is any scratchpad's reserved number, unnamed or named.
pub(crate) fn is_scratchpad_workspace(workspace: u32) -> bool {
    workspace == SCRATCHPAD_WORKSPACE || workspace >= NAMED_SCRATCHPAD_BASE
}

/// A window hidden because a child window it spawned swallowed it
/// (see `WindowRule::swallow` and `Smallvil::swallowed`).
pub struct SwallowedWindow {
    pub surface: WlSurface,
    pub window: Window,
    /// Last authoritative Classic owner before the window was hidden. If
    /// the child closes while no output is live, restoring into this
    /// dormant tree lets output adoption recover it on reconnect.
    pub output: String,
    pub workspace: u32,
}

/// Per-surface map/unmap flutter tracking (see `Smallvil::lifecycle_flutter`).
#[derive(Default)]
pub struct LifecycleFlutter {
    /// When the surface last unmapped, if it unmapped recently.
    pub last_unmapped_at: Option<std::time::Instant>,
    /// Consecutive map-after-recent-unmap flips inside the flutter window.
    pub flips: u32,
}

/// Per-surface tiled-size refusal tracking (see `Smallvil::tiled_size_refusals`).
#[derive(Default)]
pub struct TiledSizeRefusal {
    /// Consecutive commits whose committed size didn't match the expected
    /// tiled size. Reset to 0 the moment one does.
    pub refusals: u32,
    /// When the current streak started, so a slow-but-compliant client
    /// (which takes a couple of frames to catch up to a new configure)
    /// is never mistaken for a refuser.
    pub first_refusal_at: Option<std::time::Instant>,
}

fn monitor_resized_rect(
    current: Rectangle<i32, Logical>,
    output_size: Size<i32, Logical>,
    gap: i32,
    constraints: crate::window_size::FloatingSizeConstraints,
) -> Rectangle<i32, Logical> {
    use crate::window_size::{anchored_rect, SizeAnchor};
    let size = constraints
        .clamp(crate::layout::inset(Rectangle::new(Point::default(), output_size), gap).size);
    anchored_rect(current, size, SizeAnchor::Center, SizeAnchor::Center)
}

fn prefer_presentation_output<'a>(
    current: &'a Output,
    current_state: &'a RenderElementState,
    next: &'a Output,
    next_state: &'a RenderElementState,
    current_can_show_window: bool,
) -> &'a Output {
    if current_can_show_window {
        default_primary_scanout_output_compare(current, current_state, next, next_state)
    } else {
        next
    }
}

fn keyboard_resized_size(
    current: Size<i32, Logical>,
    direction: Direction,
    step: i32,
    constraints: crate::window_size::FloatingSizeConstraints,
) -> Size<i32, Logical> {
    let mut width = current.w;
    let mut height = current.h;
    match direction {
        Direction::Right => width = width.saturating_add(step),
        Direction::Down => height = height.saturating_add(step),
        Direction::Left => width = width.saturating_sub(step),
        Direction::Up => height = height.saturating_sub(step),
    }
    constraints.clamp_dimensions(width, height)
}

/// The `keyboard_move_floating` step vector for `direction`: `step` logical
/// pixels toward that edge, zero on the other axis.
fn direction_delta(direction: Direction, step: i32) -> Point<i32, Logical> {
    match direction {
        Direction::Left => (-step, 0),
        Direction::Right => (step, 0),
        Direction::Up => (0, -step),
        Direction::Down => (0, step),
    }
    .into()
}

fn is_window(window: &Window, surface: &WlSurface) -> bool {
    window
        .toplevel()
        .map(|t| t.wl_surface() == surface)
        .unwrap_or(false)
}

fn selected_glass_mode(
    explicit: Option<crate::config::GlassMode>,
    compositor_opacity: Option<f32>,
    frost_enabled: bool,
) -> Option<crate::config::GlassMode> {
    match explicit {
        Some(crate::config::GlassMode::Plain) => None,
        Some(crate::config::GlassMode::Frost) if frost_enabled => {
            Some(crate::config::GlassMode::Frost)
        }
        Some(crate::config::GlassMode::Frost) => None,
        Some(crate::config::GlassMode::Water) => Some(crate::config::GlassMode::Water),
        None if compositor_opacity.is_some_and(|alpha| alpha < 1.0) => {
            // Backward compatibility: before `glass` existed, opacity below
            // one was itself the water-glass trigger.
            Some(crate::config::GlassMode::Water)
        }
        None => None,
    }
}

/// Pure index bookkeeping for `leave_group`: given a group's member count
/// and active index *before* removal, and the position being removed,
/// returns the active index to use *after* removal, and whether the group
/// should dissolve entirely (one member left doesn't meaningfully form a
/// group anymore). Kept separate from `leave_group`'s `Window`/`Layouts`
/// side effects so this indexing logic -- the part actually easy to get
/// subtly wrong -- has a plain unit test.
fn group_removal_outcome(len: usize, active: usize, removed_pos: usize) -> (usize, bool) {
    let was_active = removed_pos == active;
    let mut new_active = active;
    if removed_pos < active {
        new_active -= 1;
    }
    let new_len = len - 1;
    if new_len <= 1 {
        return (0, true);
    }
    if was_active {
        new_active %= new_len;
    }
    (new_active, false)
}

/// Pure owner retag for `swap_workspaces`: `Layouts::swap_active` moves the
/// whole tree keyed `(output, workspace)` into the other output's active
/// slot, so a group whose owner matches either side of the swap follows its
/// tree. Returns `None` when the group's workspace did not participate --
/// a hidden workspace's tree never moves. Kept separate from
/// `swap_workspaces`'s `Space`/fullscreen side effects so the matching
/// logic has a plain unit test.
fn swapped_group_owner(
    output: &str,
    workspace: u32,
    name_a: &str,
    ws_a: u32,
    name_b: &str,
    ws_b: u32,
) -> Option<(String, u32)> {
    if output == name_a && workspace == ws_a {
        Some((name_b.to_string(), ws_b))
    } else if output == name_b && workspace == ws_b {
        Some((name_a.to_string(), ws_a))
    } else {
        None
    }
}

fn directional_score(
    from: Rectangle<i32, Logical>,
    candidate: Rectangle<i32, Logical>,
    direction: Direction,
) -> Option<f64> {
    let from_center = Point::<f64, Logical>::from((
        f64::from(from.loc.x) + f64::from(from.size.w) / 2.0,
        f64::from(from.loc.y) + f64::from(from.size.h) / 2.0,
    ));
    let candidate_center = Point::<f64, Logical>::from((
        f64::from(candidate.loc.x) + f64::from(candidate.size.w) / 2.0,
        f64::from(candidate.loc.y) + f64::from(candidate.size.h) / 2.0,
    ));
    let (primary, off_axis) = match direction {
        Direction::Left => (
            from_center.x - candidate_center.x,
            from_center.y - candidate_center.y,
        ),
        Direction::Right => (
            candidate_center.x - from_center.x,
            from_center.y - candidate_center.y,
        ),
        Direction::Up => (
            from_center.y - candidate_center.y,
            from_center.x - candidate_center.x,
        ),
        Direction::Down => (
            candidate_center.y - from_center.y,
            from_center.x - candidate_center.x,
        ),
    };
    (primary > 0.0).then_some(primary + off_axis.abs() * 2.0)
}

/// `loc + delta` with per-axis saturation. Output positions arrive as
/// full-range i32 (config `output { }`, `zwlr_output_manager` wire
/// values) and `delta` is a difference of two stored positions, so plain
/// `i32` addition on adversarial values would panic in debug and wrap in
/// release.
fn saturating_translate(
    loc: Point<i32, Logical>,
    delta: Point<i32, Logical>,
) -> Point<i32, Logical> {
    (loc.x.saturating_add(delta.x), loc.y.saturating_add(delta.y)).into()
}

fn ocean_island_origins(
    outputs: &[(String, Rectangle<i32, Logical>)],
    max_workspaces: &std::collections::HashMap<String, u32>,
    gap: i32,
) -> std::collections::HashMap<String, i32> {
    let mut next_x = 0_i32;
    let mut origins = std::collections::HashMap::new();
    for (name, geometry) in outputs {
        origins.insert(name.clone(), next_x);
        let stride = geometry.size.w.saturating_add(gap).max(1);
        let workspaces = max_workspaces.get(name).copied().unwrap_or(1).max(1);
        let span = i64::from(stride) * i64::from(workspaces);
        next_x = i64::from(next_x)
            .saturating_add(span)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    }
    origins
}

fn ocean_classic_slot<'a>(
    world_rect: Rectangle<i32, Logical>,
    slots: &'a [(String, u32, Rectangle<i32, Logical>)],
    owner: Option<&str>,
) -> Option<&'a (String, u32, Rectangle<i32, Logical>)> {
    slots
        .iter()
        .filter(|(output, _, _)| owner.is_none_or(|owner| owner == output))
        .min_by_key(|(_, _, rect)| squared_center_distance(world_rect, *rect))
}

fn ocean_reef_x(base_x: i32, workspace: u32, width: i32, gap: i32) -> i32 {
    let stride = i64::from(width.saturating_add(gap).max(1));
    let offset = i64::from(workspace.saturating_sub(1)).saturating_mul(stride);
    i64::from(base_x)
        .saturating_add(offset)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn classic_rect_to_ocean(
    classic: Rectangle<i32, Logical>,
    output: Rectangle<i32, Logical>,
    reef: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (
            reef.loc
                .x
                .saturating_add(classic.loc.x.saturating_sub(output.loc.x)),
            reef.loc
                .y
                .saturating_add(classic.loc.y.saturating_sub(output.loc.y)),
        )
            .into(),
        classic.size,
    )
}

fn ocean_rect_to_classic(
    world: Rectangle<i32, Logical>,
    reef: Rectangle<i32, Logical>,
    output: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let x = output
        .loc
        .x
        .saturating_add(world.loc.x.saturating_sub(reef.loc.x));
    let y = output
        .loc
        .y
        .saturating_add(world.loc.y.saturating_sub(reef.loc.y));
    let max_x = output
        .loc
        .x
        .saturating_add(output.size.w.saturating_sub(world.size.w.max(1)).max(0));
    let max_y = output
        .loc
        .y
        .saturating_add(output.size.h.saturating_sub(world.size.h.max(1)).max(0));
    Rectangle::new(
        (
            x.clamp(output.loc.x.min(max_x), output.loc.x.max(max_x)),
            y.clamp(output.loc.y.min(max_y), output.loc.y.max(max_y)),
        )
            .into(),
        world.size,
    )
}

fn squared_center_distance(a: Rectangle<i32, Logical>, b: Rectangle<i32, Logical>) -> u128 {
    let center_x = |rect: Rectangle<i32, Logical>| {
        i64::from(rect.loc.x)
            .saturating_mul(2)
            .saturating_add(i64::from(rect.size.w))
    };
    let center_y = |rect: Rectangle<i32, Logical>| {
        i64::from(rect.loc.y)
            .saturating_mul(2)
            .saturating_add(i64::from(rect.size.h))
    };
    let dx = center_x(a).abs_diff(center_x(b)) as u128;
    let dy = center_y(a).abs_diff(center_y(b)) as u128;
    dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
}

/// Stronger first-map/untile clamp. Reuse the ordinary rescue first, then
/// fully contain each axis that fits; an unavoidably oversized client keeps
/// its leading edge accessible. Wide arithmetic handles negative outputs.
fn fit_floating_rect_visible(
    rect: Rectangle<i32, Logical>,
    bounds: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let mut rect = clamp_rect_visible(rect, bounds);
    let axis = |value: i32, origin: i32, extent: i32, size: i32| {
        let last = (i64::from(origin) + i64::from(extent) - i64::from(size))
            .max(i64::from(origin))
            .min(i64::from(i32::MAX));
        i64::from(value).clamp(i64::from(origin), last) as i32
    };
    rect.loc.x = axis(rect.loc.x, bounds.loc.x, bounds.size.w, rect.size.w);
    rect.loc.y = axis(rect.loc.y, bounds.loc.y, bounds.size.h, rect.size.h);
    rect
}

fn initial_floating_rect(
    bounds: Rectangle<i32, Logical>,
    position: Option<Point<i32, Logical>>,
    size: Size<i32, Logical>,
    constraints: crate::window_size::FloatingSizeConstraints,
) -> Rectangle<i32, Logical> {
    // Client/rule minima win over the output cap, just as for resizing.
    let size = constraints.clamp((size.w.min(bounds.size.w), size.h.min(bounds.size.h)).into());
    let centered_axis = |origin: i32, extent: i32, size: i32| {
        (i64::from(origin) + i64::from((extent - size).max(0)) / 2)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    let loc = position.unwrap_or_else(|| {
        (
            centered_axis(bounds.loc.x, bounds.size.w, size.w),
            centered_axis(bounds.loc.y, bounds.size.h, size.h),
        )
            .into()
    });
    fit_floating_rect_visible(Rectangle::new(loc, size), bounds)
}

/// Keeps at least a small, grabbable part of a floating window inside the
/// target working area after an output move. Raw origin translation can put
/// almost the entire window off-screen when the destination is smaller.
pub(crate) fn clamp_rect_visible(
    mut rect: Rectangle<i32, Logical>,
    bounds: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    const MIN_VISIBLE: i32 = 32;
    let visible_w = MIN_VISIBLE
        .min(rect.size.w.max(1))
        .min(bounds.size.w.max(1));
    let visible_h = MIN_VISIBLE
        .min(rect.size.h.max(1))
        .min(bounds.size.h.max(1));
    let clamp_axis = |value: i32, origin: i32, extent: i32, window: i32, visible: i32| {
        let min = i64::from(origin) - i64::from(window) + i64::from(visible);
        let max = i64::from(origin) + i64::from(extent) - i64::from(visible);
        let low = min.min(max).max(i64::from(i32::MIN));
        let high = min.max(max).min(i64::from(i32::MAX));
        i64::from(value).clamp(low, high) as i32
    };
    rect.loc.x = clamp_axis(
        rect.loc.x,
        bounds.loc.x,
        bounds.size.w,
        rect.size.w,
        visible_w,
    );
    rect.loc.y = clamp_axis(
        rect.loc.y,
        bounds.loc.y,
        bounds.size.h,
        rect.size.h,
        visible_h,
    );
    rect
}

pub(crate) fn nearest_point_in_output_rects(
    pos: Point<f64, Logical>,
    rects: impl IntoIterator<Item = Rectangle<i32, Logical>>,
) -> Option<Point<f64, Logical>> {
    let mut nearest: Option<(f64, Point<f64, Logical>)> = None;
    for rect in rects {
        if rect.size.w <= 0 || rect.size.h <= 0 {
            continue;
        }
        let min_x = f64::from(rect.loc.x);
        let min_y = f64::from(rect.loc.y);
        let max_x = (i64::from(rect.loc.x) + i64::from(rect.size.w)) as f64;
        let max_y = (i64::from(rect.loc.y) + i64::from(rect.size.h)) as f64;
        if pos.x >= min_x && pos.y >= min_y && pos.x < max_x && pos.y < max_y {
            return Some(pos);
        }

        let candidate = Point::from((
            pos.x.clamp(min_x, max_x.next_down()),
            pos.y.clamp(min_y, max_y.next_down()),
        ));
        let dx = candidate.x - pos.x;
        let dy = candidate.y - pos.y;
        let distance = dx * dx + dy * dy;
        if nearest
            .as_ref()
            .is_none_or(|(best, _)| distance.total_cmp(best).is_lt())
        {
            nearest = Some((distance, candidate));
        }
    }
    nearest.map(|(_, point)| point)
}

fn stable_output_by_name<'a>(outputs: impl Iterator<Item = &'a Output>) -> Option<&'a Output> {
    outputs.min_by(|left, right| left.name().cmp(&right.name()))
}

fn tide_workspace_value(engine: crate::config::SpatialEngine, first_workspace: Option<u32>) -> i64 {
    match (engine.ownership(), first_workspace) {
        (crate::config::SpatialOwnership::Workspaces, Some(workspace)) => i64::from(workspace),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::renderer::element::RenderElementStates;
    use smithay::output::{PhysicalProperties, Subpixel};

    #[test]
    fn initial_float_centers_within_reserved_area_and_preserves_valid_position() {
        let area = Rectangle::new((-1500, 80).into(), (1400, 900).into());
        let constraints = crate::window_size::FloatingSizeConstraints::from_hints(
            (0, 0).into(),
            (0, 0).into(),
            &crate::config::WindowRule::default(),
        );
        let size = (600, 400).into();
        assert_eq!(
            initial_floating_rect(area, None, size, constraints),
            Rectangle::new((-1100, 330).into(), size)
        );
        let position = (-1420, 120).into();
        assert_eq!(
            initial_floating_rect(area, Some(position), size, constraints),
            Rectangle::new(position, size)
        );
        assert_eq!(
            initial_floating_rect(
                area,
                Some((3907, 9000).into()),
                (5000, 4000).into(),
                constraints
            ),
            area
        );
    }

    #[test]
    fn initial_float_respects_client_minimum_even_when_larger_than_output() {
        let area = Rectangle::new((100, 80).into(), (700, 500).into());
        let constraints = crate::window_size::FloatingSizeConstraints::from_hints(
            (900, 600).into(),
            (0, 0).into(),
            &crate::config::WindowRule::default(),
        );
        assert_eq!(
            initial_floating_rect(area, None, (1200, 900).into(), constraints),
            Rectangle::new(area.loc, (900, 600).into())
        );
    }

    #[test]
    fn untile_clamp_preserves_visible_rect_and_rescues_offscreen_without_resizing() {
        let bounds = Rectangle::new((1200, 30).into(), (1000, 700).into());
        let visible = Rectangle::new((1250, 90).into(), (600, 400).into());
        assert_eq!(fit_floating_rect_visible(visible, bounds), visible);
        let offscreen = Rectangle::new((3907, -9000).into(), visible.size);
        assert_eq!(
            fit_floating_rect_visible(offscreen, bounds),
            Rectangle::new((1600, 30).into(), visible.size)
        );
        let huge = Rectangle::new((3907, -9000).into(), (3000, 2000).into());
        assert_eq!(
            fit_floating_rect_visible(huge, bounds),
            Rectangle::new(bounds.loc, huge.size)
        );
    }

    #[test]
    fn presentation_moves_to_the_first_new_frame_even_with_a_slower_destination() {
        use smithay::backend::renderer::element::{
            Id, PrimaryScanoutOutput, RenderElementPresentationState,
        };
        let make_output = |name: &str, refresh| {
            let output = Output::new(
                name.into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                    serial_number: "test".into(),
                },
            );
            output.change_current_state(
                Some(smithay::output::Mode {
                    size: (1000, 800).into(),
                    refresh,
                }),
                None,
                None,
                None,
            );
            output
        };
        let left = make_output("left", 170_013);
        let right = make_output("right", 71_347);
        let id = Id::new();
        let states = RenderElementStates {
            states: HashMap::from([(
                id.clone(),
                RenderElementState {
                    visible_area: 1000,
                    presentation_state: RenderElementPresentationState::Rendering { reason: None },
                    needs_capture: false,
                },
            )]),
        };
        let mut primary = PrimaryScanoutOutput::default();
        assert!(primary.current_output().is_none());
        primary.update_from_render_element_states(
            id.clone(),
            &left,
            None,
            &states,
            default_primary_scanout_output_compare,
        );
        // Ordinary overlap keeps Smithay's existing refresh/area preference.
        primary.update_from_render_element_states(
            id.clone(),
            &right,
            None,
            &states,
            |a, sa, b, sb| prefer_presentation_output(a, sa, b, sb, true),
        );
        assert_eq!(primary.current_output(), Some(left));
        // The source no longer intersects the window; its older render state
        // cannot defer the first destination frame, even at a lower rate.
        primary.update_from_render_element_states(
            id.clone(),
            &right,
            None,
            &states,
            |a, sa, b, sb| prefer_presentation_output(a, sa, b, sb, false),
        );
        assert_eq!(primary.current_output(), Some(right.clone()));
        primary.update_from_render_element_states(
            id,
            &right,
            None,
            &RenderElementStates::default(),
            default_primary_scanout_output_compare,
        );
        assert!(primary.current_output().is_none());
    }

    #[test]
    fn monitor_resize_intersects_live_limits_and_keeps_the_world_center() {
        let constraints = crate::window_size::FloatingSizeConstraints::from_hints(
            (0, 700).into(),
            (1200, 0).into(),
            &crate::config::WindowRule {
                max_width: Some(900),
                ..Default::default()
            },
        );
        let current = Rectangle::new((-2000, 4500).into(), (300, 200).into());
        assert_eq!(
            monitor_resized_rect(current, (1600, 600).into(), 10, constraints),
            Rectangle::new((-2300, 4250).into(), (900, 700).into())
        );
    }

    #[test]
    fn monitor_resize_uses_live_logical_dimensions_and_safe_center_arithmetic() {
        let constraints = crate::window_size::FloatingSizeConstraints::from_hints(
            (0, 0).into(),
            (0, 0).into(),
            &crate::config::WindowRule::default(),
        );
        let current = Rectangle::new((i32::MAX - 10, i32::MIN + 10).into(), (200, 100).into());
        let result = monitor_resized_rect(current, (601, 1001).into(), 8, constraints);
        assert_eq!(result.size, (585, 985).into());
        assert_eq!(result.loc.x, i32::MAX - 202);
        assert_eq!(result.loc.y, i32::MIN);
        assert_eq!(
            monitor_resized_rect(current, (1, 1).into(), i32::MAX, constraints).size,
            (1, 1).into()
        );
    }

    #[test]
    fn keyboard_resize_obeys_rules_client_limits_and_overflow_bounds() {
        let constraints = crate::window_size::FloatingSizeConstraints::from_hints(
            (200, 100).into(),
            (900, 700).into(),
            &crate::config::WindowRule {
                max_width: Some(500),
                min_height: Some(250),
                ..Default::default()
            },
        );
        assert_eq!(
            keyboard_resized_size((490, 300).into(), Direction::Right, 24, constraints),
            (500, 300).into()
        );
        assert_eq!(
            keyboard_resized_size((300, 260).into(), Direction::Up, 24, constraints),
            (300, 250).into()
        );
        assert_eq!(
            keyboard_resized_size((220, 300).into(), Direction::Left, i32::MAX, constraints),
            (200, 300).into()
        );
        assert_eq!(
            keyboard_resized_size((i32::MAX, 300).into(), Direction::Right, 24, constraints),
            (500, 300).into()
        );
    }

    fn named_output(name: &str) -> Output {
        Output::new(
            name.to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".to_string(),
                model: "test".to_string(),
                serial_number: "test".to_string(),
            },
        )
    }

    #[test]
    fn saturating_translate_never_overflows() {
        let loc: Point<i32, Logical> = (i32::MAX - 10, i32::MIN + 10).into();
        let delta: Point<i32, Logical> = (100, -100).into();
        let out = saturating_translate(loc, delta);
        assert_eq!(out.x, i32::MAX);
        assert_eq!(out.y, i32::MIN);

        let ordinary: Point<i32, Logical> = (40, -7).into();
        let step: Point<i32, Logical> = (12, 5).into();
        assert_eq!(saturating_translate(ordinary, step), (52, -2).into());
    }

    #[test]
    fn direction_delta_steps_one_axis() {
        let delta: Point<i32, Logical> = (-24, 0).into();
        assert_eq!(direction_delta(Direction::Left, 24), delta);
        assert_eq!(direction_delta(Direction::Right, 24), (24, 0).into());
        assert_eq!(direction_delta(Direction::Up, 24), (0, -24).into());
        assert_eq!(direction_delta(Direction::Down, 24), (0, 24).into());
    }

    #[test]
    fn overlapping_output_priority_is_name_stable() {
        let alpha = named_output("DP-1");
        let beta = named_output("HDMI-A-1");

        assert_eq!(
            stable_output_by_name([&beta, &alpha].into_iter()).map(Output::name),
            Some("DP-1".to_string())
        );
        assert_eq!(
            stable_output_by_name([&alpha, &beta].into_iter()).map(Output::name),
            Some("DP-1".to_string())
        );
    }

    #[test]
    fn tide_workspace_is_zero_without_classic_output_ownership() {
        use crate::config::SpatialEngine;

        assert_eq!(tide_workspace_value(SpatialEngine::Classic, Some(7)), 7);
        assert_eq!(tide_workspace_value(SpatialEngine::Classic, None), 0);
        assert_eq!(tide_workspace_value(SpatialEngine::Ocean, Some(7)), 0);
        assert_eq!(tide_workspace_value(SpatialEngine::Ocean, None), 0);
    }

    #[test]
    fn directional_score_filters_axis_and_penalizes_off_axis_distance() {
        let from = Rectangle::new((100, 100).into(), (80, 60).into());
        let aligned = Rectangle::new((220, 110).into(), (40, 40).into());
        let diagonal = Rectangle::new((190, 260).into(), (40, 40).into());

        let aligned_score = directional_score(from, aligned, Direction::Right).unwrap();
        let diagonal_score = directional_score(from, diagonal, Direction::Right).unwrap();
        assert!(aligned_score < diagonal_score);
        assert!(directional_score(from, aligned, Direction::Left).is_none());
    }

    #[test]
    fn directional_score_handles_world_coordinate_edges() {
        let left = Rectangle::new((i32::MIN, -17).into(), (503, 281).into());
        let right = Rectangle::new((i32::MAX - 346, 29).into(), (346, 199).into());

        let score = directional_score(left, right, Direction::Right).unwrap();
        assert!(score.is_finite());
        assert!(score > f64::from(i32::MAX));
    }

    #[test]
    fn swapped_group_owner_follows_only_the_participating_trees() {
        // A group on output A's active workspace moves to output B's, and
        // vice versa.
        assert_eq!(
            swapped_group_owner("DP-1", 1, "DP-1", 1, "HDMI-A-1", 2),
            Some(("HDMI-A-1".to_string(), 2))
        );
        assert_eq!(
            swapped_group_owner("HDMI-A-1", 2, "DP-1", 1, "HDMI-A-1", 2),
            Some(("DP-1".to_string(), 1))
        );
        // A group parked on a hidden workspace of either output did not
        // have its tree moved and keeps its owner.
        assert_eq!(
            swapped_group_owner("DP-1", 3, "DP-1", 1, "HDMI-A-1", 2),
            None
        );
        assert_eq!(
            swapped_group_owner("HDMI-A-1", 1, "DP-1", 1, "HDMI-A-1", 2),
            None
        );
        // Both outputs on the same workspace number still swap by output.
        assert_eq!(
            swapped_group_owner("DP-1", 1, "DP-1", 1, "HDMI-A-1", 1),
            Some(("HDMI-A-1".to_string(), 1))
        );
    }

    #[test]
    fn ocean_islands_do_not_overlap_between_outputs() {
        let outputs = vec![
            (
                "left".to_string(),
                Rectangle::new((-300, 40).into(), (1111, 777).into()),
            ),
            (
                "right".to_string(),
                Rectangle::new((811, -90).into(), (937, 613).into()),
            ),
        ];
        let max_workspaces =
            std::collections::HashMap::from([("left".to_string(), 3), ("right".to_string(), 2)]);

        let gap = 37;
        let origins = ocean_island_origins(&outputs, &max_workspaces, gap);
        assert_eq!(origins["left"], 0);
        assert_eq!(origins["right"], 3 * (1111 + gap));
    }

    #[test]
    fn floating_geometry_round_trips_across_offset_output_and_reef() {
        let output = Rectangle::new((-430, 275).into(), (1373, 829).into());
        let reef = Rectangle::new((8000, 0).into(), output.size);
        let classic = Rectangle::new((-310, 355).into(), (641, 479).into());

        let world = classic_rect_to_ocean(classic, output, reef);
        assert_eq!(world.loc, (8120, 80).into());
        assert_eq!(ocean_rect_to_classic(world, reef, output), classic);
    }

    #[test]
    fn returning_float_keeps_its_monitor_even_when_only_the_other_monitor_has_tiles() {
        let world = Rectangle::new((2100, 100).into(), (600, 400).into());
        let qhd = (
            "qhd".to_string(),
            1,
            Rectangle::new((0, 0).into(), (2048, 1152).into()),
        );
        let fhd = (
            "fhd".to_string(),
            3,
            Rectangle::new((7000, 0).into(), (1920, 1080).into()),
        );
        let slots = vec![qhd.clone(), fhd];
        assert_eq!(
            ocean_classic_slot(world, &slots, Some("fhd")).unwrap().0,
            "fhd"
        );
        assert!(ocean_classic_slot(world, std::slice::from_ref(&qhd), Some("fhd")).is_none());
        assert_eq!(ocean_classic_slot(world, &[qhd], None).unwrap().0, "qhd");
    }

    #[test]
    fn nearest_reef_uses_both_axes() {
        let window = Rectangle::new((100, 900).into(), (100, 100).into());
        let same_x_far_y = Rectangle::new((100, 0).into(), (100, 100).into());
        let near_both = Rectangle::new((300, 800).into(), (100, 100).into());

        assert!(
            squared_center_distance(window, near_both)
                < squared_center_distance(window, same_x_far_y)
        );
    }

    #[test]
    fn fullscreen_restore_rect_keeps_the_original_windowed_geometry() {
        let original = Rectangle::new((10, 20).into(), (640, 480).into());
        let later_tile = Rectangle::new((100, 200).into(), (1280, 720).into());
        let mut entry = FullscreenEntry {
            output: "DP-1".to_owned(),
            restore_rect: Some(original),
            was_pinned: false,
            pin_floated_it: false,
        };

        entry.remember_restore_rect(later_tile);

        assert_eq!(entry.restore_rect, Some(original));
    }

    #[test]
    fn explicit_glass_supports_client_alpha_without_whole_window_opacity() {
        use crate::config::GlassMode;

        assert_eq!(
            selected_glass_mode(Some(GlassMode::Frost), None, true),
            Some(GlassMode::Frost)
        );
        assert_eq!(
            selected_glass_mode(Some(GlassMode::Water), None, true),
            Some(GlassMode::Water)
        );
        assert_eq!(
            selected_glass_mode(None, Some(0.7), true),
            Some(GlassMode::Water)
        );
        assert_eq!(
            selected_glass_mode(Some(GlassMode::Plain), Some(0.7), true),
            None
        );
        assert_eq!(
            selected_glass_mode(Some(GlassMode::Frost), None, false),
            None
        );
    }

    #[test]
    fn moving_fullscreen_translates_its_saved_floating_rect() {
        let original = Rectangle::new((10, 20).into(), (640, 480).into());
        let mut entry = FullscreenEntry {
            output: "DP-1".to_owned(),
            restore_rect: Some(original),
            was_pinned: false,
            pin_floated_it: false,
        };

        entry.move_to_output("HDMI-A-1".to_owned(), Some((1920, -40).into()));

        assert_eq!(entry.output, "HDMI-A-1");
        assert_eq!(entry.restore_rect.unwrap().loc, (1930, -20).into());
    }

    #[test]
    fn output_move_keeps_a_grabbable_part_of_large_floater_visible() {
        let bounds = Rectangle::new((1920, 0).into(), (800, 600).into());
        let far_offscreen = Rectangle::new((4000, 2000).into(), (1200, 900).into());

        let clamped = clamp_rect_visible(far_offscreen, bounds);

        assert_eq!(clamped.loc, (2688, 568).into());
    }

    #[test]
    fn visible_rect_clamp_handles_coordinate_edges() {
        let low_bounds = Rectangle::new((i32::MIN, i32::MIN).into(), (347, 199).into());
        let high_bounds =
            Rectangle::new((i32::MAX - 347, i32::MAX - 199).into(), (347, 199).into());
        let window = Rectangle::new((0, 0).into(), (503, 281).into());

        assert_eq!(
            clamp_rect_visible(window, low_bounds).loc,
            (i32::MIN + 315, i32::MIN + 167).into()
        );
        assert_eq!(
            clamp_rect_visible(window, high_bounds).loc,
            (i32::MAX - 818, i32::MAX - 448).into()
        );
    }

    #[test]
    fn group_removal_promotes_next_member_wrapping() {
        // [m0, m1, m2], active m1 (index 1) closes -> m2 (now index 1)
        // becomes active.
        assert_eq!(group_removal_outcome(3, 1, 1), (1, false));
        // [m0, m1, m2], active m0 (index 0) closes -> m1 (now index 0).
        assert_eq!(group_removal_outcome(3, 0, 0), (0, false));
        // [m0, m1, m2], active m2 (last, index 2) closes -> wraps to m0.
        assert_eq!(group_removal_outcome(3, 2, 2), (0, false));
    }

    #[test]
    fn group_removal_of_non_active_member_keeps_active_pointed_at_same_window() {
        // [m0, m1, m2, m3], active m3 (index 3); removing m0 (earlier,
        // non-active) shifts active's index down to stay pointed at m3.
        assert_eq!(group_removal_outcome(4, 3, 0), (2, false));
        // Removing a later, non-active member leaves active's index alone.
        assert_eq!(group_removal_outcome(4, 1, 3), (1, false));
    }

    #[test]
    fn group_removal_dissolves_a_two_member_group_regardless_of_which_side_closes() {
        assert_eq!(group_removal_outcome(2, 0, 0), (0, true));
        assert_eq!(group_removal_outcome(2, 0, 1), (0, true));
    }

    #[test]
    fn ocean_camera_scale_inverts_pointer_coordinates() {
        assert_eq!(client_scale_for_view_scale(1.0), None);
        assert_eq!(client_scale_for_view_scale(2.0), Some(0.5));
        assert_eq!(client_scale_for_view_scale(0.5), Some(2.0));
        // Keep the existing lower clamp shared with placement math.
        assert_eq!(client_scale_for_view_scale(0.0), Some(20.0));
    }

    #[test]
    fn ocean_camera_scales_a_csd_geometry_offset_with_the_window() {
        let origin =
            surface_origin_for_view_parts(Point::from((100, 200)), Point::from((19, 17)), 2.0);
        assert_eq!(origin, Point::from((62.0, 166.0)));
        assert_eq!(
            surface_origin_for_view_parts(Point::from((100, 200)), Point::from((19, 17)), 1.0),
            Point::from((81.0, 183.0))
        );
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Present only for clients accepted through a
    /// `wp_security_context_v1` listener. Privileged global filters use
    /// this marker; the metadata is also retained for diagnostics/policy.
    pub security_context: Option<SecurityContext>,
    pub(crate) disconnect_sender: Option<smithay::reexports::calloop::channel::Sender<ClientId>>,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, client_id: ClientId, _reason: DisconnectReason) {
        if let Some(sender) = &self.disconnect_sender {
            let _ = sender.send(client_id);
        }
    }
}

/// Only clients connected to TideWM's ordinary socket are trusted with
/// desktop-control protocols. A sandboxed application still receives the
/// regular compositor/shell/input globals, but cannot become the session
/// locker or IME, inject keys, or scrape/control the clipboard globally.
pub(crate) fn trusted_client(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_none_or(|state| state.security_context.is_none())
}

/// A surface's effect: a layer's `layer_rule { shader }` assignment, or a
/// window's rule assignment and workspace default. Takes the fields it reads
/// so render code can resolve while holding other `Smallvil` state mutably.
#[allow(clippy::mutable_key_type)] // WlSurface keys, same as every other map here
fn surface_shader<'a>(
    config: &'a crate::config::Config,
    layers: &'a HashMap<WlSurface, crate::config::ShaderAssignment>,
    windows: &'a HashMap<WlSurface, crate::config::ShaderAssignment>,
    surface: &WlSurface,
    workspace: Option<u32>,
) -> Option<(&'a str, &'a crate::config::ShaderDefinition)> {
    match layers.get(surface) {
        Some(assignment) => config.shader_for(Some(assignment), None),
        None => config.shader_for(windows.get(surface), workspace),
    }
}
