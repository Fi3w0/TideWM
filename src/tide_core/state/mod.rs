//! Central compositor state and cross-subsystem coordination.

mod config_reload;
mod effects;
mod focus;
mod init;
mod ipc;
mod render;
mod scroll;
mod session;
mod workspace;

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        renderer::{
            element::{
                default_primary_scanout_output_compare,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                utils::RescaleRenderElement,
                AsRenderElements, Id, Kind, RenderElementState, RenderElementStates,
            },
            gles::{GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture},
            ImportAll, ImportMem,
        },
        session::libseat::LibSeatSession,
    },
    desktop::{
        self, layer_map_for_output,
        space::SpaceRenderElements,
        utils::{
            send_frames_surface_tree, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, under_from_surface_tree,
            update_surface_primary_scanout_output, OutputPresentationFeedback,
        },
        PopupGrab, PopupManager, PopupUngrabStrategy, Window, WindowSurfaceType,
    },
    input::{
        keyboard::Keysym,
        pointer::{CursorImageStatus, MotionEvent, PointerHandle},
        Seat, SeatState,
    },
    output::Output,
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            LoopHandle, LoopSignal, RegistrationToken,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Client, DisplayHandle, Resource,
        },
    },
    utils::{Clock, Logical, Monotonic, Physical, Point, Rectangle, Scale, Size, SERIAL_COUNTER},
    wayland::{
        compositor::{get_parent, with_states, CompositorClientState, CompositorState},
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufGlobal, DmabufState},
        foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelListState},
        fractional_scale::{with_fractional_scale, FractionalScaleManagerState},
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        image_capture_source::{
            ImageCaptureSourceState, OutputCaptureSourceState, ToplevelCaptureSourceState,
        },
        image_copy_capture::{ImageCopyCaptureState, Session as CaptureSession},
        input_method::InputMethodManagerState,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
        output::OutputManagerState,
        pointer_constraints::{
            with_pointer_constraint, PointerConstraint, PointerConstraintsState,
        },
        pointer_gestures::PointerGesturesState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        security_context::{SecurityContext, SecurityContextState},
        selection::{
            data_device::DataDeviceState, primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState,
        },
        session_lock::{LockSurface, SessionLockManagerState, SessionLocker},
        shell::{
            kde::decoration::KdeDecorationState,
            wlr_layer::{KeyboardInteractivity, Layer as WlrLayer, WlrLayerShellState},
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

use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_manager_v1::WpTearingControlManagerV1;
use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use crate::{
    capture::PendingCapture,
    config::{Config, Direction, WorkspaceRef},
    grabs::GrabCompletion,
    layout::Layouts,
    toast::{Toast, ToastKind},
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

    /// Long-lived IPC subscribe connections, keyed by a process-unique id
    /// assigned at subscribe time. Empty in the steady state where no
    /// widget is connected, which makes `emit_ipc_event` a cheap
    /// early-return on every state change. See `ipc::IpcSubscriber` for
    /// the per-connection fields and lifecycle.
    pub(crate) ipc_subscribers: HashMap<usize, crate::ipc::IpcSubscriber>,
    /// Last state `emit_state_change_events` reported, `None` until a
    /// subscriber or handler first needs it.
    ipc_seen: Option<IpcSeenState>,
    /// One-shot retry timer for subscriber bytes that did not fit in the
    /// kernel socket buffer. `None` during the normal fully-drained state, so
    /// IPC adds no periodic wakeup to an idle compositor.
    pub(crate) ipc_flush_token: Option<RegistrationToken>,
    /// Next subscriber id. Monotonic across the process lifetime rather
    /// than recycled, so a stale reference inside an in-flight calloop
    /// callback can't accidentally target a fresh subscriber that reused
    /// the same id.
    pub(crate) next_ipc_subscriber_id: usize,

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

impl Smallvil {
    /// Whether continuous lateral swim is active: the master water toggle
    /// and the swim toggle both on. Off falls back to the ordinary discrete
    /// workspace switch, and the camera offset collapses to zero.
    pub(crate) fn swim_enabled(&self) -> bool {
        self.config.spatial_engine == crate::config::SpatialEngine::Classic
            && self.config.water_effects
            && self.config.swim.enabled
    }

    /// Gathers every fact `WindowRule::matches`/`matches_initial` need for
    /// one surface (identity, PID, XWayland-ness, current urgency, spawn-time
    /// identity, startup-window-ness) and folds the matching
    /// `rule { }` entries into one effective rule. The single
    /// chokepoint every other resolver in this file goes through, so a new
    /// match criterion only needs wiring here once instead of at each call
    /// site.
    pub(crate) fn resolve_window_rules_for(
        &self,
        surface: &WlSurface,
    ) -> crate::config::WindowRule {
        self.resolve_window_rules_on_workspace(surface, self.workspace_of_surface(surface))
    }

    /// Safe during a pointer grab: rules and committed hints require no
    /// pointer-handle query or backend/output assumption.
    pub(crate) fn floating_size_constraints_for(
        &self,
        surface: &WlSurface,
    ) -> crate::window_size::FloatingSizeConstraints {
        self.floating_size_constraints_with_rule(surface, &self.resolve_window_rules_for(surface))
    }

    fn floating_size_constraints_with_rule(
        &self,
        surface: &WlSurface,
        rule: &crate::config::WindowRule,
    ) -> crate::window_size::FloatingSizeConstraints {
        let (min, max) = smithay::wayland::compositor::with_states(surface, |states| {
            use smithay::wayland::shell::xdg::SurfaceCachedState;
            let mut guard = states.cached_state.get::<SurfaceCachedState>();
            let data = guard.current();
            (data.min_size, data.max_size)
        });
        crate::window_size::FloatingSizeConstraints::from_hints(min, max, rule)
    }

    pub(crate) fn enforce_floating_size_constraints(&mut self, surface: &WlSurface) {
        self.enforce_floating_size_constraints_with_rule(
            surface,
            &self.resolve_window_rules_for(surface),
        );
    }

    fn enforce_floating_size_constraints_with_rule(
        &mut self,
        surface: &WlSurface,
        rule: &crate::config::WindowRule,
    ) {
        if !self.is_floating(surface)
            || self.fullscreen.contains_key(surface)
            || self.maximized.contains_key(surface)
            || crate::grabs::resize_grab::ongoing(surface)
        {
            return;
        }
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        let current = self
            .ocean
            .floating_rect(surface)
            .map(|rect| rect.size)
            .or_else(|| {
                self.floating_workspace
                    .get(surface)
                    .map(|tag| tag.rect.size)
            })
            .unwrap_or_else(|| window.geometry().size);
        let requested = window
            .toplevel()
            .and_then(|toplevel| toplevel.with_pending_state(|state| state.size))
            .unwrap_or(current);
        let constrained = self
            .floating_size_constraints_with_rule(surface, rule)
            .clamp(requested);
        if constrained != requested {
            self.apply_floating_placement(surface, None, Some((constrained.w, constrained.h)));
        }
    }

    fn interactive_motion_half_life(&self, surface: &WlSurface) -> Duration {
        if self.config.animations.enabled && self.config.animations.interactive.enabled {
            return Duration::from_millis(self.config.animations.interactive.half_life_ms as u64);
        }
        if !self.config.water_effects {
            return Duration::ZERO;
        }
        // Preserve the original water-viscosity feel for existing configs.
        // This is a user-facing animation baseline, not a display-cadence or
        // hardware assumption; the follower itself integrates elapsed time.
        let viscosity = self
            .resolve_window_rules_for(surface)
            .viscosity
            .unwrap_or(self.config.viscosity)
            .clamp(0.0, 4.0);
        Duration::from_secs_f64(0.030 * viscosity)
    }

    pub(crate) fn connected_resize_handles(
        &self,
        hit: &crate::layout::SplitHit,
    ) -> Vec<crate::layout::SplitResizeHandle> {
        let config = self.config.vessels;
        let (falloff, max_splits) = if self.config.water_effects && config.enabled {
            (config.falloff, config.max_splits)
        } else {
            // The primary split is still returned, preserving ordinary BSP
            // resize when the water identity or this mechanic is disabled.
            (0.0, 1)
        };
        self.layout
            .connected_resize_handles(hit, falloff, max_splits)
    }

    /// Retargets one mapped window's render-only interactive follower.
    /// Returns `false` when viscosity is bypassed, allowing a tiled resize
    /// to fall back to the ordinary layout-movement animation.
    pub(crate) fn retarget_window_viscosity(
        &mut self,
        surface: &WlSurface,
        target: Rectangle<i32, Logical>,
    ) -> bool {
        let half_life = self.interactive_motion_half_life(surface);
        // Interactive input owns the visual geometry from this point. The
        // generic movement tween must not remain underneath it even when
        // viscosity is disabled and the pointer path becomes immediate.
        self.window_move_animations.remove(surface);
        if half_life.is_zero() {
            self.window_viscosity.remove(surface);
            return false;
        }
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return false;
        };
        let Some(location) = self.space.element_location(&window) else {
            return false;
        };
        let live = Rectangle::new(location, window.geometry().size).to_f64();
        let target = target.to_f64();
        if let Some(motion) = self.window_viscosity.get_mut(surface) {
            motion.retarget(target, half_life);
        } else {
            self.window_viscosity.insert(
                surface.clone(),
                crate::viscosity::ViscousMotion::new(live, target, half_life),
            );
        }
        // The generic movement tween and the physical follower must never
        // both offset the same window. Interactive viscosity owns it until
        // it settles, then a later non-interactive retile may start a fresh
        // ordinary movement animation.
        self.request_redraw();
        true
    }

    fn sway_enabled_for_surface(&self, surface: &WlSurface) -> bool {
        if !self.config.water_effects {
            return false;
        }
        self.resolve_window_rules_for(surface)
            .sway
            .unwrap_or(self.config.sway.enabled)
    }

    /// `true` when this window's resolved rule pins it buoyant (`depth =
    /// false`), exempting it from the automatic inactivity sink regardless
    /// of the global `depth { enabled }` setting.
    fn depth_exempt(&self, surface: &WlSurface) -> bool {
        self.resolve_window_rules_for(surface).depth == Some(false)
    }

    /// Feeds one horizontal floating-drag delta into the window's lateral
    /// sway. Called from the move grab's motion path; a no-op when the
    /// water identity, the mechanic, or this window's rule disables it.
    pub(crate) fn sway_kick(&mut self, surface: &WlSurface, delta_x: f64) {
        if delta_x.abs() < f64::EPSILON || !self.sway_enabled_for_surface(surface) {
            return;
        }
        let config = self.config.sway;
        let response = config.response as f64;
        let max_offset = config.max_offset as f64;
        match self.window_sway.get_mut(surface) {
            Some(sway) => sway.kick(delta_x, response, max_offset),
            None => {
                self.window_sway.insert(
                    surface.clone(),
                    crate::sway::FloatingSway::kicked(
                        (delta_x * response).clamp(-max_offset, max_offset),
                        config.frequency as f64,
                        config.damping as f64,
                    ),
                );
            }
        }
        self.request_redraw();
    }

    /// Resolves the exclusive floating-physics tier used instead of sway.
    pub(crate) fn float_physics_tier_for_surface(
        &self,
        surface: &WlSurface,
    ) -> crate::config::FloatPhysicsTier {
        if !self.config.water_effects {
            return crate::config::FloatPhysicsTier::Off;
        }
        self.resolve_window_rules_for(surface)
            .float_physics
            .unwrap_or(self.config.float_physics.tier)
    }

    /// Convenience for call sites that only care whether F1 is active at
    /// all (either tier), not which one.
    pub(crate) fn float_physics_enabled_for_surface(&self, surface: &WlSurface) -> bool {
        self.float_physics_tier_for_surface(surface) != crate::config::FloatPhysicsTier::Off
    }

    /// Applies a 2D disturbance at the resolved physics tier. A kick also
    /// refreshes depth attention so the window cannot sink mid-motion.
    pub(crate) fn float_physics_kick(&mut self, surface: &WlSurface, impulse: (f64, f64)) {
        if impulse.0.abs() < f64::EPSILON && impulse.1.abs() < f64::EPSILON {
            return;
        }
        let config = self.config.float_physics;
        match self.float_physics_tier_for_surface(surface) {
            crate::config::FloatPhysicsTier::Off => return,
            crate::config::FloatPhysicsTier::Light => {
                let response = config.response as f64;
                let max_offset = config.max_offset as f64;
                let bob_ratio = config.bob_ratio as f64;
                match self.window_float_physics.get_mut(surface) {
                    Some(physics) => physics.kick(impulse, response, max_offset, bob_ratio),
                    None => {
                        self.window_float_physics.insert(
                            surface.clone(),
                            crate::float_physics::FloatPhysics::kicked(
                                impulse,
                                response,
                                max_offset,
                                bob_ratio,
                                config.frequency as f64,
                                config.damping as f64,
                            ),
                        );
                    }
                }
            }
            crate::config::FloatPhysicsTier::Full => {
                self.window_float_bodies
                    .entry(surface.clone())
                    .or_insert_with(crate::float_physics::FloatBody::at_rest)
                    .kick(impulse, config.response as f64);
            }
        }
        self.note_depth_attention(surface);
        self.request_redraw();
    }

    /// Spreads a disturbance across output floaters with the configured
    /// distance falloff from `point`.
    pub(crate) fn float_physics_kick_near(
        &mut self,
        output: &Output,
        point: (f64, f64),
        impulse: (f64, f64),
    ) {
        if !self.config.water_effects {
            return;
        }
        // `render_placements` (Classic) reads `Space::elements_for_output`,
        // a per-output cache normally kept fresh by each backend's render
        // loop calling `space.refresh()` once a frame -- see
        // `refocus_after_hide`'s identical comment. Both call sites here
        // (a window mapping, a workspace-transition wave) run synchronously
        // right after their own `map_element`/`apply_workspace_switch`,
        // before that frame's render pass, so without this the cache is
        // stale and this always sees zero placements for a window just
        // mapped or just switched in. Cheap and idempotent to call an extra
        // time here.
        self.refresh_space();
        let Some(placements) = self.render_placements(output) else {
            return;
        };
        let radius = self.config.float_physics.radius as f64;
        let falloff = self.config.float_physics.falloff;
        for placement in &placements {
            if !placement.is_floating() {
                continue;
            }
            let Some(surface) = placement.surface() else {
                continue;
            };
            // Not `placement.rect`: a just-mapped window's `PlacedWindow`
            // still carries `window.geometry()`'s pre-commit (often
            // zero) size, the same buffer-commit lag `spawn_ripple`
            // documents -- collapsing its "center" to its top-left corner
            // and pushing it outside its own kick radius. This is the
            // same already-resolved-rect helper `point` itself came from
            // at the call site, so a window kicking itself always
            // measures at distance zero.
            let Some(center) = self.window_center_for_kick(surface) else {
                continue;
            };
            let Some(fraction) = crate::float_physics::falloff_kick(point, center, radius) else {
                continue;
            };
            if !falloff && fraction < 1.0 {
                continue;
            }
            self.float_physics_kick(surface, (impulse.0 * fraction, impulse.1 * fraction));
        }
    }

    /// Best-effort window center in global logical coordinates, used as a
    /// disturbance origin. Mirrors `spawn_ripple`'s own preference order:
    /// the floating tag's already-resolved target rect (set before a
    /// first-map floating conversion's own buffer commit lands) over the
    /// window's possibly still-stale committed geometry.
    pub(crate) fn window_center_for_kick(&self, surface: &WlSurface) -> Option<(f64, f64)> {
        let rect = self.window_rect_for_kick(surface)?;
        Some((
            rect.loc.x as f64 + rect.size.w as f64 / 2.0,
            rect.loc.y as f64 + rect.size.h as f64 / 2.0,
        ))
    }

    /// The rect half of `window_center_for_kick`, also used by F1 `full`
    /// tier's per-tick collision pass, which needs real width/height for
    /// AABB overlap tests, not just a center point.
    pub(crate) fn window_rect_for_kick(
        &self,
        surface: &WlSurface,
    ) -> Option<Rectangle<i32, Logical>> {
        self.floating_workspace
            .get(surface)
            .map(|tag| tag.rect)
            .or_else(|| {
                let window = self.mapped_toplevel_window(surface)?;
                self.space
                    .element_location(&window)
                    .map(|loc| Rectangle::new(loc, window.geometry().size))
            })
    }

    fn is_floating(&self, surface: &WlSurface) -> bool {
        self.floating_workspace.contains_key(surface) || self.ocean.floating_rect(surface).is_some()
    }

    /// Toggles ambient "sitting on water" sway for `surface`: once on, a
    /// continuous sine-wave offset (`float_physics::ambient_sample`, sampled
    /// fresh every frame in `base_window_visual_sample`) runs until toggled
    /// off again, independent of the drag/map/wave kick sources -- it never
    /// settles the way a kick does, by design. Floating windows only -- a
    /// tiled window has no offset render slot to sway in without visibly
    /// drifting off its own tile boundary, so this is a silent no-op on one.
    pub(crate) fn toggle_float_ambient(&mut self, surface: &WlSurface) {
        if !self.is_floating(surface) {
            return;
        }
        if self.window_float_ambient.remove(surface).is_some() {
            self.request_redraw();
            return;
        }
        // The value is the time origin `ambient_sample`'s `elapsed` measures
        // from, not a due time -- there's nothing to schedule.
        self.window_float_ambient
            .insert(surface.clone(), Instant::now());
        self.request_redraw();
    }

    /// Maintains all qualifying full-tier bodies while collision/wave
    /// simulation is active, and prunes the set completely once idle.
    fn sync_float_physics_bodies(&mut self) {
        if !self.config.water_effects {
            self.window_float_bodies.clear();
            return;
        }
        let stale: Vec<WlSurface> = self
            .window_float_bodies
            .keys()
            .filter(|surface| {
                !self.is_floating(surface)
                    || self.float_physics_tier_for_surface(surface)
                        != crate::config::FloatPhysicsTier::Full
            })
            .cloned()
            .collect();
        for surface in stale {
            self.window_float_bodies.remove(&surface);
        }
        if !crate::float_physics::bodies_need_simulation(
            self.config.float_physics.wave.enabled,
            self.window_float_bodies.values(),
        ) {
            return;
        }

        let mut candidates: Vec<WlSurface> = self.floating_workspace.keys().cloned().collect();
        candidates.extend(self.ocean.floating_surfaces().cloned());
        for surface in candidates {
            if self.float_physics_tier_for_surface(&surface)
                == crate::config::FloatPhysicsTier::Full
            {
                self.window_float_bodies
                    .entry(surface)
                    .or_insert_with(crate::float_physics::FloatBody::at_rest);
            }
        }
    }

    /// Resolves one body's logical anchor, area-derived mass, and edge bounds
    /// once per outer tick for reuse across fixed substeps.
    fn float_body_context(&self, surface: &WlSurface) -> Option<FloatBodyContext> {
        let rect = self.window_rect_for_kick(surface)?;
        let anchor = (
            rect.loc.x as f64,
            rect.loc.y as f64,
            rect.size.w as f64,
            rect.size.h as f64,
        );
        const MIN_MASS: f64 = 1.0;
        let mass = (anchor.2 * anchor.3).max(MIN_MASS);
        let output_bounds = match self.config.spatial_engine {
            crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll => {
                self.mapped_toplevel_window(surface).and_then(|window| {
                    let output = self.output_for_window(&window)?;
                    let geo = self.space.output_geometry(&output)?;
                    Some((
                        geo.loc.x as f64,
                        geo.loc.y as f64,
                        geo.size.w as f64,
                        geo.size.h as f64,
                    ))
                })
            }
            crate::config::SpatialEngine::Ocean => self
                .ocean
                .entry_output(surface)
                .and_then(|name| self.output_by_name(name).map(|output| (name, output)))
                .and_then(|(name, output)| {
                    let viewport = self.space.output_geometry(&output)?.size;
                    Some(self.ocean.viewport_world_bounds(name, viewport))
                }),
        };
        Some(FloatBodyContext {
            surface: surface.clone(),
            anchor,
            mass,
            output_bounds,
        })
    }

    /// Advances the full floating-physics simulation from either backend's
    /// live frame clock. A fixed-timestep accumulator with capped catch-up
    /// keeps integration stable across variable output cadence.
    #[allow(clippy::mutable_key_type)] // WlSurface identity is stable across body updates.
    pub(crate) fn update_float_physics_full(&mut self) {
        const FIXED_DT: f64 = 1.0 / 120.0;
        const MAX_SUBSTEPS: u32 = 8;

        let now = Instant::now();
        let elapsed = now
            .saturating_duration_since(self.float_physics_last_tick)
            .as_secs_f64();
        self.float_physics_last_tick = now;

        if self.floating_workspace.is_empty() && self.window_float_bodies.is_empty() {
            self.float_physics_accum = 0.0;
            return;
        }
        self.sync_float_physics_bodies();
        if self.window_float_bodies.is_empty() {
            self.float_physics_accum = 0.0;
            return;
        }

        self.float_physics_accum =
            (self.float_physics_accum + elapsed).min(FIXED_DT * MAX_SUBSTEPS as f64);

        let contexts: Vec<FloatBodyContext> = self
            .window_float_bodies
            .keys()
            .filter_map(|surface| self.float_body_context(surface))
            .collect();

        let mut steps = 0;
        while self.float_physics_accum >= FIXED_DT && steps < MAX_SUBSTEPS {
            self.float_physics_accum -= FIXED_DT;
            steps += 1;
            self.step_float_physics_full(&contexts, FIXED_DT);
        }
        let changed = contexts
            .iter()
            .map(|context| context.surface.clone())
            .collect();
        self.request_surface_redraws(&changed);

        // Passive at-rest bodies stay present while another body moves so
        // the collision pass can hit them. Once all motion ends, remove the
        // set atomically; recreating settled entries one-by-one here was the
        // source of the permanent idle redraw loop.
        if !crate::float_physics::bodies_need_simulation(
            self.config.float_physics.wave.enabled,
            self.window_float_bodies.values(),
        ) {
            self.window_float_bodies.clear();
        }
    }

    fn currents_enabled(&self) -> bool {
        self.config.water_effects
            && self.config.spatial_engine == crate::config::SpatialEngine::Ocean
            && self.config.currents.enabled
            && self.config.currents.strength > f32::EPSILON
    }

    /// Marks the exact floating window being directly manipulated. Move
    /// grabs call this from their own motion/unset callbacks; it does not
    /// query the pointer or seat, avoiding the re-entrant grab deadlock class
    /// documented elsewhere in this state module.
    pub(crate) fn set_floating_dragging(&mut self, surface: &WlSurface, dragging: bool) {
        if dragging {
            self.floating_dragging.insert(surface.clone());
        } else {
            self.floating_dragging.remove(surface);
        }
        self.buoyancy_dirty = true;
        self.request_redraw();
    }

    fn snap_eligible_surface(&self, surface: &WlSurface) -> bool {
        if !self.config.spatial_engine.uses_workspaces()
            || !self.config.snap.enabled
            || !matches!(self.session_lock, SessionLock::Unlocked)
            || self.layout.contains(surface)
            || !self.floating_workspace.contains_key(surface)
            || self.fullscreen.contains_key(surface)
            || self.maximized.contains_key(surface)
        {
            return false;
        }
        self.window_is_visible(surface)
    }

    pub(crate) fn snap_target_at(
        &self,
        surface: &WlSurface,
        point: Point<f64, Logical>,
    ) -> Option<crate::snap::SnapTarget> {
        if !self.snap_eligible_surface(surface) {
            return None;
        }
        let output = self.output_for_point(point)?;
        let output_geometry = self.space.output_geometry(&output)?;
        let zone = crate::snap::zone_at(point, output_geometry, &self.config.snap)?;
        self.snap_target_for_zone(surface, &output, zone)
    }

    /// Resolve policy and geometry at the destination for both input paths.
    /// Safe inside pointer callbacks: output and coordinates come from the
    /// caller, never from a query of the pointer handle.
    fn snap_target_for_zone(
        &self,
        surface: &WlSurface,
        output: &Output,
        zone: crate::snap::SnapZone,
    ) -> Option<crate::snap::SnapTarget> {
        if !self.snap_eligible_surface(surface)
            || !crate::snap::zone_enabled(zone, &self.config.snap)
        {
            return None;
        }
        let output_name = output.name();
        let workspace = self.layout.active_workspace(&output_name);
        let rule = self.resolve_window_rules_on_workspace(surface, Some(workspace));
        // A per-window decision keeps precedence over the destination
        // workspace. Otherwise, crossing an output must honor the active
        // workspace there rather than the workspace where the grab began.
        if !rule
            .snap
            .or(self.config.resolve_workspace_rule(workspace).snap)
            .unwrap_or(true)
        {
            return None;
        }
        let area = self.output_tiling_area(output)?;
        let gap = self
            .config
            .snap
            .gap
            .unwrap_or_else(|| self.gaps_for(&output_name, workspace));
        Some(crate::snap::SnapTarget {
            output: output_name,
            workspace,
            zone,
            rect: crate::snap::constrained_target_rect(
                area,
                zone,
                gap,
                self.floating_size_constraints_with_rule(surface, &rule),
            ),
        })
    }

    pub(crate) fn set_snap_preview(&mut self, target: Option<&crate::snap::SnapTarget>) {
        if !self.config.snap.preview {
            if self.snap_preview.take().is_some() {
                self.request_redraw();
            }
            return;
        }
        let Some(target) = target else {
            if self.snap_preview.take().is_some() {
                self.request_redraw();
            }
            return;
        };
        let color = [
            self.config.snap.preview_color[0],
            self.config.snap.preview_color[1],
            self.config.snap.preview_color[2],
            1.0,
        ];
        match &mut self.snap_preview {
            Some(preview) => {
                preview.output.clone_from(&target.output);
                preview.rect = target.rect;
                preview.buffer.update(target.rect.size, color);
            }
            None => {
                self.snap_preview = Some(SnapPreview {
                    output: target.output.clone(),
                    rect: target.rect,
                    buffer: SolidColorBuffer::new(target.rect.size, color),
                });
            }
        }
        self.request_redraw();
    }

    /// Shows (or with `None` hides) a drag hint rectangle through the
    /// snap-preview element, in the snap preview's color and opacity.
    pub(crate) fn set_drag_preview(&mut self, preview: Option<(String, Rectangle<i32, Logical>)>) {
        let Some((output, rect)) = preview else {
            if self.snap_preview.take().is_some() {
                self.request_redraw();
            }
            return;
        };
        let color = [
            self.config.snap.preview_color[0],
            self.config.snap.preview_color[1],
            self.config.snap.preview_color[2],
            1.0,
        ];
        let unchanged = self
            .snap_preview
            .as_ref()
            .is_some_and(|p| p.output == output && p.rect == rect);
        if unchanged {
            return;
        }
        match &mut self.snap_preview {
            Some(preview) => {
                preview.output = output;
                preview.rect = rect;
                preview.buffer.update(rect.size, color);
            }
            None => {
                self.snap_preview = Some(SnapPreview {
                    output,
                    rect,
                    buffer: SolidColorBuffer::new(rect.size, color),
                });
            }
        }
        self.request_redraw();
    }

    pub(crate) fn apply_snap_target(
        &mut self,
        window: &Window,
        target: &crate::snap::SnapTarget,
    ) -> bool {
        let Some(surface) = window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
        else {
            return false;
        };
        let Some(output) = self.output_by_name(&target.output) else {
            return false;
        };
        // Revalidate against current config, workspace, and output geometry.
        let Some(target) = self.snap_target_for_zone(&surface, &output, target.zone) else {
            return false;
        };
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| state.size = Some(target.rect.size));
            toplevel.send_pending_configure();
        }
        self.set_window_fractional_scale(window, &output);
        self.space
            .map_element(window.clone(), target.rect.loc, false);
        if let Some(tag) = self.floating_workspace.get_mut(&surface) {
            tag.output.clone_from(&target.output);
            tag.workspace = target.workspace;
            tag.rect = target.rect;
        }
        self.refresh_window_opacity_and_glass_for(&surface);
        self.retarget_window_viscosity(&surface, target.rect);
        self.request_redraw();
        true
    }

    /// Advances Ocean currents from the existing backend clock. Only visible
    /// floating placements retain a record, keeping both state and continuous
    /// redraw work proportional to what can actually be seen.
    #[allow(clippy::mutable_key_type)] // WlSurface-keyed bounded visibility set
    pub(crate) fn update_currents(&mut self) {
        if !self.currents_enabled() || self.floating_workspace.is_empty() {
            self.window_currents.clear();
            return;
        }

        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let mut seen = HashSet::new();
        let mut visible = Vec::new();
        for output in outputs {
            let Some(placements) = self.render_placements(&output) else {
                continue;
            };
            for placement in placements {
                if !placement.is_floating()
                    || placement.surface().is_some_and(|surface| {
                        self.fullscreen.contains_key(surface)
                            || self.ocean.is_screen_pinned(surface)
                    })
                {
                    continue;
                }
                let Some(surface) = placement.surface().cloned() else {
                    continue;
                };
                if !seen.insert(surface.clone()) {
                    continue;
                }
                // Stable for the record's lifetime and cheap to derive.
                // Different homes decorrelate neighboring windows without a
                // random-number dependency or global unbounded seed counter.
                let rect = placement.rect;
                let phase = (rect.loc.x as f64 * 0.013
                    + rect.loc.y as f64 * 0.021
                    + rect.size.w as f64 * 0.003
                    + rect.size.h as f64 * 0.005)
                    .rem_euclid(std::f64::consts::TAU);
                visible.push((surface, phase));
            }
        }

        self.window_currents
            .retain(|surface, _| seen.contains(surface));
        let focused = self.focused_window_surface();
        let now = Instant::now();
        let mut changed = HashSet::new();
        for (surface, phase) in visible {
            let paused =
                focused.as_ref() == Some(&surface) || self.floating_dragging.contains(&surface);
            let drift = self
                .window_currents
                .entry(surface.clone())
                .or_insert_with(|| crate::currents::CurrentDrift::new(now, phase, paused));
            drift.tick(now, paused);
            if drift.needs_frame() {
                changed.insert(surface);
            }
        }
        self.request_surface_redraws(&changed);
    }

    fn buoyancy_enabled(&self) -> bool {
        self.config.water_effects && self.config.buoyancy.enabled
    }

    /// Synchronizes visible floating windows only when ownership/config/focus
    /// changed, then ticks just the short focus/drag transitions. A stable
    /// weighted desktop therefore performs no placement walk and asks for no
    /// frames on idle maintenance ticks.
    #[allow(clippy::mutable_key_type)] // WlSurface-keyed bounded visibility set
    pub(crate) fn update_buoyancy(&mut self) {
        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            let revision = self.ocean.camera_revision();
            let moving = self.ocean.has_active_camera_motion();
            self.buoyancy_dirty |= revision != self.buoyancy_camera_revision
                || moving
                || moving != self.buoyancy_camera_was_moving;
            self.buoyancy_camera_revision = revision;
            self.buoyancy_camera_was_moving = moving;
        }
        if !self.buoyancy_enabled() {
            self.window_buoyancy.clear();
            self.buoyancy_dirty = false;
            return;
        }

        let transitioning = self
            .window_buoyancy
            .values()
            .any(crate::buoyancy::BuoyancyState::needs_frame);
        if !self.buoyancy_dirty && !transitioning {
            return;
        }

        if self.buoyancy_dirty {
            let outputs: Vec<Output> = self.space.outputs().cloned().collect();
            let mut candidates = HashSet::new();
            for output in outputs {
                let Some(placements) = self.render_placements(&output) else {
                    continue;
                };
                for placement in placements {
                    if !placement.replacement_eligible() || !placement.is_floating() {
                        continue;
                    }
                    let Some(surface) = placement.surface() else {
                        continue;
                    };
                    if self.fullscreen.contains_key(surface) || self.window_is_pinned(surface) {
                        continue;
                    }
                    candidates.insert(surface.clone());
                }
            }

            let mut eligible = HashSet::new();
            let now = Instant::now();
            for surface in candidates {
                let weight = self
                    .resolve_window_rules_for(&surface)
                    .weight
                    .unwrap_or(self.config.buoyancy.default_weight)
                    .clamp(0.0, 1.0) as f64;
                if weight <= f64::EPSILON {
                    continue;
                }
                eligible.insert(surface.clone());
                self.window_buoyancy
                    .entry(surface)
                    .and_modify(|state| state.set_configured_weight(weight))
                    .or_insert_with(|| crate::buoyancy::BuoyancyState::new(now, weight));
            }
            self.window_buoyancy
                .retain(|surface, _| eligible.contains(surface));
            self.buoyancy_dirty = false;
        }

        let focused = self.focused_window_surface();
        let now = Instant::now();
        let settle = self.config.buoyancy.settle_ms as f64 / 1000.0;
        let mut changed = HashSet::new();
        for (surface, state) in &mut self.window_buoyancy {
            let paused =
                focused.as_ref() == Some(surface) || self.floating_dragging.contains(surface);
            let target = if paused {
                0.0
            } else {
                state.configured_weight()
            };
            state.tick(now, target, settle);
            if state.needs_frame() {
                changed.insert(surface.clone());
            }
        }
        self.request_surface_redraws(&changed);
    }

    /// One fixed `dt` physics substep across every F1 `full` body: wave/
    /// spring forcing, then pairwise inter-window collisions, then
    /// home-output edge bounces -- all velocity-only updates layered on
    /// top of the position integration `step_body` already did for this
    /// substep. Bodies are pulled into a plain local `Vec` for the
    /// duration (a `HashMap` can't safely hand out two simultaneous `&mut`
    /// entries for an arbitrary pair of keys without extra ceremony this
    /// realistically single-digit window count doesn't need) and written
    /// back at the end.
    fn step_float_physics_full(&mut self, contexts: &[FloatBodyContext], dt: f64) {
        let cfg = self.config.float_physics;
        let wave = cfg.wave;
        // Reuses `light` tier's two feel knobs as a damped spring's
        // stiffness/drag instead of adding a parallel pair for one more
        // tier -- see `FloatPhysicsConfig::frequency`/`damping`'s own doc
        // comments.
        let stiffness = (std::f64::consts::TAU * cfg.frequency as f64).powi(2);
        let drag = 2.0 * cfg.damping as f64;
        let elapsed = self.float_physics_wave_clock.elapsed().as_secs_f64();

        let mut bodies: Vec<crate::float_physics::FloatBody> = contexts
            .iter()
            .map(|ctx| {
                self.window_float_bodies
                    .get(&ctx.surface)
                    .copied()
                    .unwrap_or_default()
            })
            .collect();

        for (i, ctx) in contexts.iter().enumerate() {
            let target = if wave.enabled {
                crate::float_physics::wave_target(
                    ctx.anchor.0 + ctx.anchor.2 / 2.0,
                    elapsed,
                    wave.amplitude as f64,
                    wave.wavelength as f64,
                    wave.speed as f64,
                )
            } else {
                (0.0, 0.0)
            };
            crate::float_physics::step_body(
                &mut bodies[i],
                target,
                stiffness,
                drag,
                cfg.max_offset as f64,
                dt,
            );
        }

        for i in 0..contexts.len() {
            for j in (i + 1)..contexts.len() {
                let a_rect = (
                    contexts[i].anchor.0 + bodies[i].offset.0,
                    contexts[i].anchor.1 + bodies[i].offset.1,
                    contexts[i].anchor.2,
                    contexts[i].anchor.3,
                );
                let b_rect = (
                    contexts[j].anchor.0 + bodies[j].offset.0,
                    contexts[j].anchor.1 + bodies[j].offset.1,
                    contexts[j].anchor.2,
                    contexts[j].anchor.3,
                );
                let (left, right) = bodies.split_at_mut(j);
                crate::float_physics::resolve_collision(
                    a_rect,
                    b_rect,
                    &mut left[i].velocity,
                    &mut right[0].velocity,
                    contexts[i].mass,
                    contexts[j].mass,
                    cfg.restitution as f64,
                );
            }
        }

        if cfg.bounce_off_edges {
            for (i, ctx) in contexts.iter().enumerate() {
                if let Some(bounds) = ctx.output_bounds {
                    let rect = (
                        ctx.anchor.0 + bodies[i].offset.0,
                        ctx.anchor.1 + bodies[i].offset.1,
                        ctx.anchor.2,
                        ctx.anchor.3,
                    );
                    crate::float_physics::resolve_edge_collision(
                        rect,
                        &mut bodies[i].velocity,
                        bounds,
                        cfg.restitution as f64,
                    );
                }
            }
        }

        for (ctx, body) in contexts.iter().zip(bodies) {
            self.window_float_bodies.insert(ctx.surface.clone(), body);
        }
    }

    pub(crate) fn start_window_open_animation(&mut self, surface: &WlSurface) {
        self.closing_window_animations
            .retain(|closing| closing.surface != *surface);
        let cascade = self
            .cascade_liquid_applies(surface)
            .then(|| crate::cascade_transition::CascadeTransition::pour(&self.config.cascade))
            .flatten();
        if let Some(cascade) = cascade {
            self.cascade_window_animations
                .insert(surface.clone(), cascade);
        } else {
            self.cascade_window_animations.remove(surface);
        }
        let animations = self.config.animations.clone();
        let ordinary = animations.enabled
            && animations.open.enabled
            && !(self.cascade_window_animations.contains_key(surface)
                && self.config.cascade.replace_motion);
        if !ordinary {
            self.window_open_animations.remove(surface);
            if self.cascade_window_animations.contains_key(surface) {
                self.request_redraw();
            }
            return;
        }
        let offset = self
            .window_lifecycle_offset(surface, &animations.open)
            .unwrap_or_else(|| {
                Point::from((
                    animations.open.offset.0 as f64,
                    animations.open.offset.1 as f64,
                ))
            });
        self.window_open_animations.insert(
            surface.clone(),
            crate::window_animation::WindowVisualAnimation::open(
                &animations.open,
                animations.slowdown,
                offset,
            ),
        );
        self.request_redraw();
    }

    fn cascade_liquid_applies(&self, surface: &WlSurface) -> bool {
        if !self.config.water_effects || !self.config.spatial_engine.uses_workspaces() {
            return false;
        }
        let (Some(output), Some(workspace)) = (
            self.layout.output_of(surface),
            self.layout.workspace_of(surface),
        ) else {
            return false;
        };
        self.layout.algorithm(output, workspace) == crate::config::LayoutAlgorithm::Cascade
    }

    fn window_lifecycle_offset(
        &self,
        surface: &WlSurface,
        config: &crate::config::WindowAnimationConfig,
    ) -> Option<Point<f64, Logical>> {
        let window = self.mapped_toplevel_window(surface)?;
        let output = self
            .rendered_output_for_surface(surface)
            .or_else(|| self.output_for_window(&window))?;
        let output_rect = self.space.output_geometry(&output)?;
        let window_rect = if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            self.rendered_rect_on_output(surface, &output)?
        } else {
            self.tiled_rect_for_surface(surface)
                .or_else(|| self.floating_workspace.get(surface).map(|tag| tag.rect))
                .or_else(|| self.space.element_geometry(&window))?
        };
        Some(crate::window_animation::lifecycle_offset(
            config,
            window_rect,
            output_rect,
        ))
    }

    /// Starts/re-targets visual movement while preserving the current
    /// on-screen position. Input and layout already use `new_location`.
    fn start_window_move_animation(
        &mut self,
        surface: &WlSurface,
        old_rect: Rectangle<i32, Logical>,
        new_rect: Rectangle<i32, Logical>,
    ) {
        let animations = &self.config.animations;
        if !animations.enabled || !animations.movement.enabled {
            self.window_move_animations.remove(surface);
            return;
        }
        let current = self
            .window_move_animations
            .get(surface)
            .filter(|animation| !animation.finished())
            .map(|animation| animation.sample());
        let current_offset = current
            .map(|sample| sample.offset)
            .unwrap_or_else(|| Point::from((0.0, 0.0)));
        let current_size = current
            .and_then(|sample| sample.size)
            .unwrap_or_else(|| old_rect.size.to_f64());
        let from_offset = Point::from((
            (f64::from(old_rect.loc.x) - f64::from(new_rect.loc.x)) + current_offset.x,
            (f64::from(old_rect.loc.y) - f64::from(new_rect.loc.y)) + current_offset.y,
        ));
        let size_changed = animations.movement.animate_size
            && ((current_size.w - new_rect.size.w as f64).abs() >= 0.01
                || (current_size.h - new_rect.size.h as f64).abs() >= 0.01);
        if from_offset.x.abs() < 0.01 && from_offset.y.abs() < 0.01 && !size_changed {
            return;
        }
        self.window_move_animations.insert(
            surface.clone(),
            crate::window_animation::WindowVisualAnimation::movement(
                &animations.movement,
                animations.slowdown,
                from_offset,
                current_size,
                new_rect.size.to_f64(),
            ),
        );
        let scroll_output = (self.config.spatial_engine == crate::config::SpatialEngine::Scroll)
            .then(|| {
                self.layout
                    .output_of(surface)
                    .and_then(|name| self.output_by_name(name))
            })
            .flatten();
        if let Some(output) = scroll_output {
            self.request_output_redraw(&output);
        } else {
            self.request_redraw();
        }
    }

    /// Transfers the last drawable surface snapshot into a short close
    /// animation. Smithay's null-buffer commit still proceeds normally.
    pub(crate) fn start_window_close_animation(&mut self, surface: &WlSurface) {
        let animations = self.config.animations.clone();
        let cascade = self
            .cascade_liquid_applies(surface)
            .then(|| crate::cascade_transition::CascadeTransition::drain(&self.config.cascade))
            .flatten();
        let ordinary_enabled = animations.enabled
            && animations.close.enabled
            && !(cascade.is_some() && self.config.cascade.replace_motion);
        if !ordinary_enabled && cascade.is_none() {
            return;
        }
        let offset = self
            .window_lifecycle_offset(surface, &animations.close)
            .unwrap_or_else(|| {
                Point::from((
                    animations.close.offset.0 as f64,
                    animations.close.offset.1 as f64,
                ))
            });
        let Some(snapshot) = self.window_frame_snapshots.remove(surface) else {
            return;
        };
        let pixel_budget = self.close_snapshot_pixel_budget();
        let snapshot_pixels = snapshot.estimated_pixels();
        let count_budget = animations.max_closing_snapshots;
        self.window_open_animations.remove(surface);
        self.cascade_window_animations.remove(surface);
        self.window_move_animations.remove(surface);
        self.window_viscosity.remove(surface);
        self.closing_window_animations
            .retain(|closing| closing.surface != *surface && !closing.finished());
        let retained_pixels: Vec<u64> = self
            .closing_window_animations
            .iter()
            .map(|closing| closing.snapshot.estimated_pixels())
            .collect();
        let Some(evictions) = crate::window_animation::close_snapshot_evictions(
            &retained_pixels,
            snapshot_pixels,
            count_budget,
            pixel_budget,
        ) else {
            return;
        };
        self.closing_window_animations.drain(..evictions);
        self.closing_window_animations
            .push(crate::window_animation::ClosingWindowAnimation::new(
                surface.clone(),
                snapshot,
                ordinary_enabled.then(|| {
                    crate::window_animation::WindowVisualAnimation::close(
                        &animations.close,
                        animations.slowdown,
                        offset,
                    )
                }),
                cascade,
            ));
        self.request_redraw();
    }

    /// Converts the configured output-area multiple into a live physical
    /// pixel budget. Mode sizes come from Smithay's current outputs, so this
    /// scales with nested, HiDPI, rotated, and multi-output sessions without
    /// assuming any particular monitor resolution.
    fn close_snapshot_pixel_budget(&self) -> u64 {
        let output_pixels =
            self.space
                .outputs()
                .filter_map(Output::current_mode)
                .fold(0_u64, |total, mode| {
                    let width = u64::try_from(mode.size.w.max(0)).unwrap_or(u64::MAX);
                    let height = u64::try_from(mode.size.h.max(0)).unwrap_or(u64::MAX);
                    total.saturating_add(width.saturating_mul(height))
                });
        ((output_pixels as f64) * f64::from(self.config.animations.close_snapshot_output_budget))
            .round()
            .clamp(0.0, u64::MAX as f64) as u64
    }

    pub(crate) fn closing_window_frame_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<crate::backend::udev::OutputRenderElements> {
        let output_scale = output.current_scale().fractional_scale();
        let scale = Scale::from(output_scale);
        let needs_cascade = self
            .closing_window_animations
            .iter()
            .any(|closing| closing.snapshot.output == output.name() && closing.cascade.is_some());
        let cascade_program = needs_cascade
            .then(|| {
                crate::decoration::rounded_surface_program(
                    &mut self.rounded_surface_program,
                    renderer,
                )
            })
            .flatten();
        let mut result = Vec::new();
        for closing in self
            .closing_window_animations
            .iter_mut()
            .filter(|closing| closing.snapshot.output == output.name())
        {
            result.extend(
                closing
                    .frame_elements(scale, cascade_program.clone())
                    .into_iter()
                    .map(crate::backend::udev::OutputRenderElements::WindowSnapshot),
            );
        }
        result
    }

    fn depth_live_alpha(&self, surface: &WlSurface) -> f32 {
        if !self.config.water_effects || !self.config.depth.enabled {
            return 1.0;
        }
        self.window_depths
            .get(surface)
            .filter(|depth| depth.tier() == 1)
            .map(|_| self.config.depth.tier_one_alpha)
            .unwrap_or(1.0)
    }

    /// Records direct attention for a mapped window. The next frame renders
    /// it at the surface immediately; its inactivity clock restarts from now.
    pub(crate) fn note_depth_attention(&mut self, surface: &WlSurface) {
        let Some(depth) = self.window_depths.get_mut(surface) else {
            return;
        };
        if depth.note_attention() {
            self.depth_schematics.remove(surface);
            self.request_redraw();
            // The periodic `update_window_depths` tick is the only other
            // place tier changes, and it emits this same event -- without
            // it here, a subscriber sees a window sink but never sees it
            // resurface, since by the next tick `old_tier` and the new
            // tier are already equal (both zero) and no change is detected.
            self.emit_ipc_event(crate::ipc::IpcEvent::DepthChanged {
                surface: surface.clone(),
                tier: 0,
            });
        }
    }

    /// Advances every mapped window's analytical inactivity tier. Both
    /// backends call this from their already-bounded frame timer, so no
    /// per-window calloop source or unbounded timer registry is needed.
    pub(crate) fn update_window_depths(&mut self) {
        if self.depth_last_tick.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.depth_last_tick = Instant::now();
        let enabled = self.config.water_effects && self.config.depth.enabled;
        // Resolved separately, before the mutable loop below: `depth_exempt`
        // needs `&self` (it reads surface identity + config), which can't
        // run while `window_depths` is mutably borrowed. Skipped entirely
        // when depth is globally off, since every window takes the
        // `reset_disabled` branch regardless of exemption in that case.
        // `depth_exempt` is intentionally resolved before the mutable walk,
        // but membership in that result must stay constant-time: this runs
        // at 10 Hz and the old Vec made the two full window walks quadratic.
        #[allow(clippy::mutable_key_type)]
        let exempt: HashSet<WlSurface> = if enabled {
            self.window_depths
                .keys()
                .filter(|surface| self.depth_exempt(surface))
                .cloned()
                .collect()
        } else {
            HashSet::new()
        };
        let focused = self.focused_window_surface();
        let mut changed = false;
        // Collect per-surface tier transitions while we still hold the
        // mutable borrow on `window_depths`, then emit after the loop --
        // `emit_ipc_event` needs `&mut self` and can't run mid-iteration.
        let mut tier_changes: Vec<(WlSurface, u8)> = Vec::new();
        for (surface, depth) in self.window_depths.iter_mut() {
            let old_tier = depth.tier();
            let focused_now = focused.as_ref() == Some(surface);
            let local_changed = if focused_now {
                // Focus is an ongoing form of attention, not just the event
                // that granted it. Keep the timer fresh and stay on surface.
                depth.update(&self.config.depth, true)
            } else if enabled && !exempt.contains(surface) {
                depth.update(&self.config.depth, false)
            } else {
                depth.reset_disabled()
            };
            changed |= local_changed;
            if local_changed && depth.tier() != old_tier {
                tier_changes.push((surface.clone(), depth.tier()));
            }
        }
        if !enabled {
            self.depth_schematics.clear();
        }
        self.poll_depth_background();
        if changed {
            self.request_redraw();
        }
        for (surface, tier) in tier_changes {
            self.emit_ipc_event(crate::ipc::IpcEvent::DepthChanged { surface, tier });
        }
    }

    /// Re-fires the urgent ripple for every still-urgent window whose
    /// resolved ripple config has `urgent_repeat` on, once per
    /// `urgent_repeat_interval_ms`. Both backends call this from their
    /// already-bounded frame timer alongside `update_window_depths`, so
    /// no per-window calloop source is needed; the 100ms gate below keeps
    /// the tick itself cheap. Each pulse is identical (flat repeat, the
    /// agreed design -- no decay), and each window holds at most one
    /// active ripple at a time as long as the interval exceeds the ripple
    /// lifetime, so pulsing can't starve the 16-ripple cap.
    ///
    /// `urgent_pulse_last` is reconciled against `urgent` here rather
    /// than at every clear site: focus, unmap, and `focus-urgent` all
    /// remove from `urgent` through different paths, and a stale entry
    /// left behind would only waste a lookup, never fire -- but dropping
    /// it here keeps the map bounded by the urgent set's actual size.
    pub(crate) fn update_urgent_pulses(&mut self) {
        if self.urgent_pulse_tick.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.urgent_pulse_tick = Instant::now();
        self.urgent_pulse_last
            .retain(|surface, _| self.urgent.contains(surface));
        if !self.config.water_effects || self.urgent.is_empty() {
            return;
        }
        let due: Vec<WlSurface> = self
            .urgent
            .iter()
            .filter(|surface| {
                let rule = self.resolve_window_rules_for(surface);
                let cfg = self.config.resolve_ripple_config(
                    rule.ripple.as_ref(),
                    crate::config::RippleTrigger::Urgent,
                );
                if cfg.enabled == Some(false)
                    || !cfg.urgent_repeat.unwrap_or(true)
                    || !cfg.fires_on(crate::config::RippleTrigger::Urgent)
                {
                    return false;
                }
                let interval = cfg.urgent_repeat_interval_ms.unwrap_or(1500).max(100) as u64;
                self.urgent_pulse_last
                    .get(*surface)
                    .is_none_or(|last| last.elapsed() >= Duration::from_millis(interval))
            })
            .cloned()
            .collect();
        for surface in due {
            // Stamped before the spawn attempt on purpose: a spawn that
            // no-ops (hidden workspace, ripple cap reached) must not be
            // retried on every 100ms tick until it succeeds.
            self.urgent_pulse_last
                .insert(surface.clone(), Instant::now());
            self.spawn_ripple(&surface, crate::config::RippleTrigger::Urgent);
        }
    }

    pub(crate) fn apply_depth_settings(
        &mut self,
        next: crate::config::DepthConfig,
        reload_background: bool,
    ) {
        if reload_background || next.background != self.config.depth.background {
            self.depth_background.reset();
            for depth in self.window_depths.values_mut() {
                depth.artwork_id = smithay::backend::renderer::element::Id::new();
            }
        }
        self.config.depth = next;
        self.depth_schematics.clear();
        for depth in self.window_depths.values_mut() {
            depth.visual_changed();
        }
        self.depth_last_tick = Instant::now() - Duration::from_millis(100);
        self.update_window_depths();
        self.request_redraw();
    }

    /// One decoder wakeup/deadline for the entire artwork, regardless of window count.
    fn poll_depth_background(&mut self) {
        let enabled = self.config.water_effects && self.config.depth.enabled;
        let path = enabled
            .then(|| self.config.depth.background.clone())
            .flatten();
        let visible = path.is_some()
            && self.depth_background.can_present()
            && matches!(self.session_lock, SessionLock::Unlocked)
            && self.space.outputs().any(|output| {
                if !self.wlr_output_power_management_state.is_on(output) {
                    return false;
                }
                let Some(view) = self.space.output_geometry(output) else {
                    return false;
                };
                self.render_placements(output).is_some_and(|placements| {
                    placements.iter().any(|p| {
                        p.replacement_eligible()
                            && p.rect.overlaps(view)
                            && p.surface()
                                .and_then(|s| self.window_depths.get(s))
                                .is_some_and(|depth| depth.tier() >= 2)
                    })
                })
            });
        let now = Instant::now();
        let old_status = self.depth_background.status();
        self.depth_background
            .configure(path.as_deref(), visible, now);
        if self.depth_background.poll(now) {
            self.request_redraw();
        }
        if self.depth_background.status() != old_status {
            self.emit_ipc_event(crate::ipc::IpcEvent::DepthBackgroundChanged);
        }
        let deadline = self.depth_background.deadline();
        if deadline != self.depth_background_deadline {
            if let Some(token) = self.depth_background_timer.take() {
                self.loop_handle.remove(token);
            }
            self.depth_background_deadline = deadline;
            if let Some(deadline) = deadline {
                self.depth_background_timer = self
                    .loop_handle
                    .insert_source(Timer::from_deadline(deadline), |_, _, state| {
                        state.depth_background_timer = None;
                        state.depth_background_deadline = None;
                        state.poll_depth_background();
                        TimeoutAction::Drop
                    })
                    .ok();
            }
        }
    }

    /// Builds the depth-specific replacement/overlay elements for one output.
    /// The returned surface list contains only deep windows whose schematic
    /// replacement was successfully built, so callers can safely omit those
    /// live surfaces from `desktop_render_elements`.
    pub(crate) fn depth_frame_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        placements: &[crate::placement::PlacedWindow],
    ) -> (
        HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>>,
        Vec<WlSurface>,
    ) {
        self.poll_depth_background();
        if !self.config.water_effects || !self.config.depth.enabled {
            return (HashMap::new(), Vec::new());
        }
        let Some(output_geo) = self.space.output_geometry(output) else {
            return (HashMap::new(), Vec::new());
        };
        let visible: Vec<(WlSurface, Rectangle<i32, Logical>)> = placements
            .iter()
            .filter(|placement| placement.replacement_eligible())
            .filter_map(|placement| {
                let surface = placement.surface()?.clone();
                let mut rect = placement.rect;
                let visual = self.placement_visual_sample(placement);
                rect.loc += Point::from((
                    visual.offset.x.round() as i32,
                    visual.offset.y.round() as i32,
                ));
                rect.size = visual.rounded_size_or(placement.window.geometry().size);
                if !rect.overlaps(output_geo) {
                    return None;
                }
                Some((
                    surface,
                    Rectangle::new(rect.loc - output_geo.loc, rect.size),
                ))
            })
            .collect();
        let program =
            crate::depth::depth_overlay_program(&mut self.depth_overlay_program, renderer);
        #[allow(clippy::mutable_key_type)]
        let mut layers = HashMap::new();
        let mut replaced = Vec::new();

        for (surface, area) in visible {
            let mut elements = Vec::new();
            let Some(depth) = self.window_depths.get(&surface) else {
                continue;
            };
            let tier = depth.tier();
            let urgent = self.urgent.contains(&surface);

            let artwork = if tier >= 2 {
                self.depth_background.render_element(
                    renderer,
                    area,
                    self.config.depth.background_fit,
                    output.current_scale().fractional_scale(),
                    depth.artwork_id.clone(),
                )
            } else {
                None
            };
            let has_artwork = artwork.is_some();
            if let Some(program) = program
                .as_ref()
                .filter(|_| tier == 1 || urgent || has_artwork)
            {
                elements.push(crate::backend::udev::OutputRenderElements::DepthOverlay(
                    crate::depth::DepthOverlayElement::new(
                        depth,
                        area,
                        program.clone(),
                        &self.config.depth,
                        urgent,
                        has_artwork,
                    ),
                ));
            }

            if tier < 2 {
                self.depth_schematics.remove(&surface);
                if !elements.is_empty() {
                    layers.insert(surface, elements);
                }
                continue;
            }

            let size = (area.size.w.max(1), area.size.h.max(1));
            // Artwork only needs a short title strip, never a window-sized CPU copy.
            if !has_artwork || self.config.depth.show_title {
                let rebuild = self.depth_schematics.get(&surface).is_none_or(|schematic| {
                    !schematic.matches(size, tier, &self.config.depth, has_artwork)
                });
                if rebuild {
                    // Identity-change callbacks invalidate this cache; avoid
                    // cloning/rasterizing a title on every GIF frame.
                    let title = if self.config.depth.show_title {
                        crate::tab_strip::window_title(&surface)
                    } else {
                        String::new()
                    };
                    self.depth_schematics.insert(
                        surface.clone(),
                        crate::depth::DepthSchematic::build(
                            size,
                            &title,
                            tier,
                            &self.config.depth,
                            has_artwork,
                        ),
                    );
                }
                let Some(element) = self.depth_schematics.get(&surface).and_then(|schematic| {
                    schematic.render_element(
                        renderer,
                        (
                            area.loc.x as f64 * output.current_scale().fractional_scale(),
                            area.loc.y as f64 * output.current_scale().fractional_scale(),
                        ),
                    )
                }) else {
                    continue;
                };
                elements.push(crate::backend::udev::OutputRenderElements::Composited(
                    element,
                ));
            } else {
                self.depth_schematics.remove(&surface);
            }
            if let Some(artwork) = artwork {
                elements.push(crate::backend::udev::OutputRenderElements::DepthImage(
                    artwork,
                ));
                let alpha = self.config.depth.schematic_alpha;
                let color = self.config.depth.schematic_color;
                elements.push(crate::backend::udev::OutputRenderElements::Dim(
                    SolidColorRenderElement::new(
                        depth.artwork_id.namespaced(1),
                        area.to_physical_precise_round(output.current_scale().fractional_scale()),
                        depth.commit,
                        [color[0] * alpha, color[1] * alpha, color[2] * alpha, alpha],
                        Kind::Unspecified,
                    ),
                ));
            }
            layers.insert(surface.clone(), elements);
            replaced.push(surface);
        }

        (layers, replaced)
    }

    /// Scroll input follows the same animated rectangle as the rendered strip.
    /// Other engines retain their existing camera/input policy.
    fn placement_input_rect(
        &self,
        placement: &crate::placement::PlacedWindow,
    ) -> Rectangle<i32, Logical> {
        if self.config.spatial_engine == crate::config::SpatialEngine::Scroll {
            let visual = self.placement_visual_sample(placement);
            Rectangle::new(
                placement.rect.loc
                    + Point::from((
                        visual.offset.x.round() as i32,
                        visual.offset.y.round() as i32,
                    )),
                visual.rounded_size_or(placement.rect.size),
            )
        } else {
            crate::placement::translated_rect(placement.rect, placement.view_offset)
        }
    }

    /// The output containing `pos`, if any. Overlap is resolved by stable
    /// output name so pointer ownership does not depend on `Space`'s map
    /// iteration order. Used for click, drag, focus, and placement targets.
    pub(crate) fn output_for_point(&self, pos: Point<f64, Logical>) -> Option<Output> {
        stable_output_by_name(self.space.output_under(pos)).cloned()
    }

    /// `Space::refresh` with TideWM's output ownership applied first, so a
    /// window never enters an output it is not drawn on. Use this instead
    /// of calling `self.space.refresh()` directly.
    pub(crate) fn refresh_space(&mut self) {
        for window in self.space.elements() {
            self.space
                .set_output_owner(window, self.space_output_owner(window).as_ref());
        }
        self.space.refresh();
    }

    /// The single output a window may enter, or `None` for geometric
    /// membership. Scroll tiles and strip floaters are positions on one
    /// output's strip: off-screen parts can overlap a neighbouring output's
    /// rectangle without ever being drawn there.
    fn space_output_owner(&self, window: &Window) -> Option<Output> {
        if self.config.spatial_engine != crate::config::SpatialEngine::Scroll {
            return None;
        }
        let surface = window.toplevel()?.wl_surface();
        let name = if self.layout.contains(surface) {
            self.layout.output_of(surface)?
        } else if self.scroll_strip_floater(surface) {
            self.floating_workspace.get(surface)?.output.as_str()
        } else {
            return None;
        };
        self.output_by_name(name)
    }

    /// Ocean's independent membership, or the output containing the largest
    /// share of the window's current presentation geometry.
    /// Output overlap storage inside Smithay is a HashMap, so taking its first
    /// entry made straddling-window ownership nondeterministic across runs.
    /// Ties use the stable output name. `None` if the window is not visible.
    pub(crate) fn output_for_window(&self, window: &Window) -> Option<Output> {
        if self.config.spatial_engine == crate::config::SpatialEngine::Scroll {
            if let Some(output) = window
                .toplevel()
                .and_then(|t| self.layout.output_of(t.wl_surface()))
                .and_then(|name| self.output_by_name(name))
            {
                return Some(output);
            }
        }
        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            let surface = window.toplevel()?.wl_surface();
            let owner = || {
                self.ocean
                    .entry_output(surface)
                    .and_then(|name| self.output_by_name(name))
            };
            // Space contains Ocean world coordinates, not desktop output
            // coordinates. Comparing those rectangles can change the
            // preferred scale on every commit and corrupt output affinity.
            return if self.config.ocean.shared_canvas {
                self.rendered_output_for_surface(surface).or_else(owner)
            } else {
                owner().or_else(|| self.rendered_output_for_surface(surface))
            };
        }
        let rect = self.space.element_geometry(window)?;
        self.space
            .outputs()
            .filter_map(|output| {
                let intersection = self.space.output_geometry(output)?.intersection(rect)?;
                let area = i64::from(intersection.size.w) * i64::from(intersection.size.h);
                Some((output.clone(), area))
            })
            .max_by(|(output_a, area_a), (output_b, area_b)| {
                area_a
                    .cmp(area_b)
                    .then_with(|| output_b.name().cmp(&output_a.name()))
            })
            .map(|(output, _)| output)
    }

    /// Resolves a spawn-only rule output. Persistent output pins deliberately
    /// bypass this helper: their value is always a real connector name.
    pub(crate) fn initial_rule_output(&self, name: &str) -> Option<Output> {
        match name {
            "focused" | "camera" | "current" => {
                self.focused_output().or_else(|| self.primary_output())
            }
            name => self.output_by_name(name),
        }
    }

    /// Chooses the output for an action with no stronger spatial hint. With
    /// focus-follows-mouse, prefer the pointer output unless an exclusive
    /// layer owns focus; otherwise prefer focused-window ownership.
    pub(crate) fn primary_output(&self) -> Option<Output> {
        let focused_output = self.focused_output();
        let pointer_output = self
            .seat
            .get_pointer()
            .and_then(|p| self.output_for_point(p.current_location()));
        let first_output = || self.space.outputs().next().cloned();
        let prefer_pointer =
            self.config.input.focus_follows_mouse && self.exclusive_layer().is_none();
        if prefer_pointer {
            pointer_output.or(focused_output).or_else(first_output)
        } else {
            focused_output.or(pointer_output).or_else(first_output)
        }
    }

    fn focus_window_with_ripple(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
        animate_handoff: bool,
        origin: FocusOrigin,
    ) {
        if origin == FocusOrigin::Navigation {
            if let Some(surface) = surface.as_ref() {
                self.reveal_scroll_window(surface);
            }
        }
        let previous_focus = self.window_focus.clone();
        self.on_demand_layer_focus = None;
        self.window_focus = surface.filter(|surface| self.window_is_visible(surface));
        if let Some(surface) = self.window_focus.clone() {
            self.note_depth_attention(&surface);
        }
        self.reconcile_keyboard_focus(serial);

        // Focus-change ripple: only on a user-visible transition between two
        // real windows, not on the very first focus (from None), focus being
        // dropped (to None), or a map transaction. A freshly mapped window
        // receives focus and its map ripple in the same transaction; stacking
        // a focus ripple there makes two presets fight over the same frame.
        if let Some(new_surface) = self.window_focus.clone() {
            if animate_handoff
                && previous_focus.as_ref() != Some(&new_surface)
                && previous_focus.is_some()
            {
                self.spawn_ripple(&new_surface, crate::config::RippleTrigger::Focus);
            }
        }
    }

    /// Commits a visible floating window's live Space placement back into
    /// its durable ownership tag after an interactive move. In particular,
    /// crossing an output transfers it to that output's active workspace;
    /// otherwise later workspace actions would still treat it as belonging
    /// to the output where the drag began.
    ///
    /// `dropped` is true only at the end of an interactive move. A
    /// strip-anchored Scroll floater otherwise keeps its output: scrolling
    /// can leave its rect mostly over a neighbor's coordinates without it
    /// being shown there, and a plain commit must not hand it over.
    pub(crate) fn sync_visible_floating_window(&mut self, window: &Window, dropped: bool) {
        let Some(surface) = window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
        else {
            return;
        };
        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            if self.fullscreen.contains_key(&surface)
                || self.maximized.contains_key(&surface)
                || self.ocean.floating_rect(&surface).is_none()
                || !self.window_is_visible(&surface)
            {
                return;
            }
            let Some(rect) = self.space.element_geometry(window) else {
                return;
            };
            self.ocean.set_floating_rect(&surface, rect);
            // Grab teardown already holds the pointer borrow; resolve output
            // from window geometry instead of calling `primary_output()`.
            if let Some(output) = self.output_for_window(window) {
                self.set_window_fractional_scale(window, &output);
                self.ocean.set_entry_output(&surface, output.name());
            }
            self.enforce_floating_size_constraints(&surface);
            return;
        }
        if self.fullscreen.contains_key(&surface)
            || self.maximized.contains_key(&surface)
            || !self.floating_workspace.contains_key(&surface)
            || !self.window_is_visible(&surface)
        {
            return;
        }
        let Some(rect) = self.space.element_geometry(window) else {
            return;
        };
        // Grab teardown holds the pointer mutex. If the window overlaps no
        // output, retain its live owner or use a stable remaining output;
        // primary_output() would re-enter that mutex through its fallback.
        let keep_strip_owner = !dropped && self.scroll_strip_floater(&surface);
        let owner = (!keep_strip_owner)
            .then(|| self.output_for_window(window))
            .flatten()
            .or_else(|| {
                self.floating_workspace
                    .get(&surface)
                    .and_then(|tag| self.output_by_name(&tag.output))
            })
            .or_else(|| stable_output_by_name(self.space.outputs()).cloned())
            .map(|output| {
                // The window may have just been dragged onto a different
                // output; its surfaces need to hear about that output's scale.
                self.set_window_fractional_scale(window, &output);
                let name = output.name();
                let workspace = self.layout.active_workspace(&name);
                (name, workspace)
            });

        // A strip floater is drawn only on its owner, so a drop straddling
        // two outputs would leave part of it clipped: slide it fully onto
        // the owner (as far as it fits).
        let mut rect = rect;
        if dropped && self.scroll_strip_floater(&surface) {
            if let Some(bounds) = owner
                .as_ref()
                .and_then(|(name, _)| self.output_by_name(name))
                .and_then(|output| self.space.output_geometry(&output))
            {
                let fit = |loc: i32, size: i32, origin: i32, extent: i32| {
                    loc.clamp(origin, (origin + extent - size).max(origin))
                };
                let loc = Point::from((
                    fit(rect.loc.x, rect.size.w, bounds.loc.x, bounds.size.w),
                    fit(rect.loc.y, rect.size.h, bounds.loc.y, bounds.size.h),
                ));
                if loc != rect.loc {
                    rect.loc = loc;
                    self.space.map_element(window.clone(), loc, false);
                }
            }
        }
        let Some(tag) = self.floating_workspace.get_mut(&surface) else {
            // The floating tag can vanish mid-commit if an unmap raced this
            // sync; never crash the compositor over a stale rect write.
            return;
        };
        tag.rect = rect;
        if let Some((output, workspace)) = owner {
            tag.output = output;
            tag.workspace = workspace;
        }
        self.refresh_window_opacity_and_glass_for(&surface);
    }

    /// Reattaches an Ocean floater when its final world rectangle is close to
    /// an existing reef tile. The pointer position is supplied by the grab's
    /// last motion event so release handling never re-enters the pointer
    /// handle while Smithay is dispatching the button callback.
    pub(crate) fn smart_attach_ocean_floating(
        &mut self,
        surface: &WlSurface,
        pointer_location: Point<f64, Logical>,
    ) -> bool {
        if self.config.spatial_engine != crate::config::SpatialEngine::Ocean
            || !self.config.ocean.smart_tiling
            || !self.ocean.floating_rect(surface).is_some()
        {
            return false;
        }
        let Some(output) = self.output_for_point(pointer_location) else {
            return false;
        };
        let output_name = output.name();
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return false;
        };
        let pointer_view = pointer_location - output_geo.loc.to_f64();
        // Require pointer containment; a radius scaled by the moving window
        // would make large floaters attach on nearly every overlap.
        let Some(target) = self.ocean.tiled_target_at_view(
            surface,
            &output_name,
            pointer_view,
            self.config.gaps,
            self.config.bsp_split_bias,
        ) else {
            return false;
        };
        let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size) else {
            return false;
        };
        if !self.ocean.make_tiled_with_size(
            surface,
            &output_name,
            viewport,
            Some(&target),
            self.config.ocean.smart_tiling_preserve_size,
            self.config.gaps,
            self.config.bsp_split_bias,
        ) {
            return false;
        }
        self.retile();
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
        true
    }

    /// Fits the focused Ocean floater to its live output minus configured
    /// gaps and live floating limits, preserving its world-space center.
    /// This action deliberately keeps its existing zoom-independent sizing.
    pub(crate) fn resize_to_monitor(&mut self) {
        if self.config.spatial_engine != crate::config::SpatialEngine::Ocean {
            return;
        }
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        if self.fullscreen.contains_key(&surface) || self.maximized.contains_key(&surface) {
            return;
        }
        let Some(current) = self.ocean.floating_rect(&surface) else {
            return;
        };
        let Some(output_name) = self.ocean.entry_output(&surface).map(str::to_string) else {
            return;
        };
        let Some(output) = self.output_by_name(&output_name) else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let rect = monitor_resized_rect(
            current,
            output_geo.size,
            self.config.gaps,
            self.floating_size_constraints_for(&surface),
        );
        self.apply_floating_placement(
            &surface,
            Some((rect.loc.x, rect.loc.y)),
            Some((rect.size.w, rect.size.h)),
        );
        self.retile();
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
    }

    /// `Action::SwitchWorkspace`'s Ocean branch for a numbered ref: centers
    /// the camera on and focuses `index`'s (1-based) app-slot window --
    /// "like focus but faster" than panning manually. No apps open at all
    /// jumps to the world origin specifically, not any particular reef or
    /// bookmark. A slot number past however many apps are actually open is
    /// a no-op, the same way focusing an empty workspace wouldn't do
    /// anything either.
    pub(crate) fn jump_to_app_slot(&mut self, output: &Output, index: usize) {
        let Some(viewport) = self.space.output_geometry(output).map(|geo| geo.size) else {
            return;
        };
        let animation = Duration::from_millis(self.config.ocean.camera_animation_ms);
        match self
            .ocean
            .app_slot_on_output(&output.name(), index)
            .cloned()
        {
            Some(surface) => {
                if let Some(rect) =
                    self.ocean
                        .world_rect(&surface, self.config.gaps, self.config.bsp_split_bias)
                {
                    self.ocean.center_on_rect(
                        &output.name(),
                        viewport,
                        rect,
                        animation,
                        self.config.ocean.camera_sway,
                    );
                }
                self.focus_window(Some(surface.clone()), SERIAL_COUNTER.next_serial());
                self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged { surface });
                self.request_redraw();
            }
            None if !self.ocean.has_open_apps() => {
                self.ocean.animate_to_point(
                    &output.name(),
                    crate::ocean::OceanPoint::default(),
                    animation,
                    self.config.ocean.camera_sway,
                );
                self.request_redraw();
            }
            None => {}
        }
    }

    /// Translates every floating window tagged to `output_name` by `delta`
    /// -- called when wlr-output-management (kanshi, wdisplays, ...) moves
    /// an output's logical position. `retile()` already repositions tiled
    /// windows for free (their tree is recomputed against the output's
    /// fresh area on the very next call); floating windows have no such
    /// automatic step, so without this they'd keep their old absolute
    /// coordinates after the output moved out from under them -- possibly
    /// landing on a different output, or off-screen entirely -- even
    /// though the client requesting the move was told it succeeded.
    ///
    /// Mirrors `swap_workspaces`' own visible-vs-hidden handling: a visible
    /// window's real position lives in `space` right now (`tag.rect` is
    /// stale until the next hide, same as `sync_visible_floating_window`'s
    /// doc comment explains), so that's what gets read and re-mapped; a
    /// hidden window has no `space` presence to read, so `tag.rect` itself
    /// -- the only place its position survives -- is what moves.
    pub(crate) fn translate_floating_windows_on_output(
        &mut self,
        output_name: &str,
        delta: Point<i32, Logical>,
    ) {
        let surfaces: Vec<WlSurface> = self
            .floating_workspace
            .iter()
            .filter(|(_, tag)| tag.output == output_name)
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in surfaces {
            self.translate_floating_window(&surface, delta);
        }
    }

    /// Moves one floating window by `delta`, live position if visible,
    /// remembered one if hidden; see `translate_floating_windows_on_output`.
    fn translate_floating_window(&mut self, surface: &WlSurface, delta: Point<i32, Logical>) {
        let surface = surface.clone();
        let output_override =
            self.fullscreen.contains_key(&surface) || self.maximized.contains_key(&surface);
        if output_override {
            // The live Space rect is the fullscreen/maximized override,
            // not windowed geometry. Retile will derive that override
            // again from the output's new live geometry; only translate
            // the durable windowed restore records here.
            if let Some(tag) = self.floating_workspace.get_mut(&surface) {
                tag.rect.loc = saturating_translate(tag.rect.loc, delta);
            }
        } else if self.window_is_visible(&surface) {
            let window = self
                .floating_workspace
                .get(&surface)
                .unwrap()
                .window
                .clone();
            let Some(mut rect) = self.space.element_geometry(&window) else {
                return;
            };
            rect.loc = saturating_translate(rect.loc, delta);
            self.space.map_element(window, rect.loc, false);
            self.floating_workspace.get_mut(&surface).unwrap().rect = rect;
        } else if let Some(tag) = self.floating_workspace.get_mut(&surface) {
            tag.rect.loc = saturating_translate(tag.rect.loc, delta);
        }
        if let Some(entry) = self.fullscreen.get_mut(&surface) {
            if let Some(rect) = &mut entry.restore_rect {
                rect.loc = saturating_translate(rect.loc, delta);
            }
        }
        if let Some(entry) = self.maximized.get_mut(&surface) {
            entry.restore_rect.loc = saturating_translate(entry.restore_rect.loc, delta);
        }
    }

    fn topmost_ocean_surface(&self, preferred_output: Option<&str>) -> Option<WlSurface> {
        let mut outputs: Vec<Output> = self.space.outputs().cloned().collect();
        outputs.sort_by_key(Output::name);
        if let Some(preferred) = preferred_output {
            outputs.sort_by_key(|output| output.name() != preferred);
        }
        outputs.into_iter().find_map(|output| {
            self.render_placements(&output)?
                .into_iter()
                .find_map(|placement| placement.surface().cloned())
        })
    }

    fn window_at_layout_position(&self, pos: Point<f64, Logical>) -> Option<WlSurface> {
        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            return self.window_under(pos).and_then(|(window, _)| {
                window
                    .toplevel()
                    .map(|toplevel| toplevel.wl_surface().clone())
            });
        }
        let live = self.space.element_under(pos).and_then(|(window, _)| {
            window
                .toplevel()
                .map(|toplevel| toplevel.wl_surface().clone())
        });
        if live.as_ref().is_some_and(|surface| {
            !self.layout.contains(surface) || self.fullscreen.contains_key(surface)
        }) {
            return live;
        }

        let tiled = self.output_for_point(pos).and_then(|output| {
            let workspace = self.layout.active_workspace(&output.name());
            let area = self.output_tiling_area(&output)?;
            self.layout
                .layout(
                    &output.name(),
                    workspace,
                    area,
                    self.gaps_for(&output.name(), workspace),
                )
                .into_iter()
                .find(|(_, rect)| rect.contains(pos.to_i32_round()))
                .and_then(|(window, _)| {
                    window
                        .toplevel()
                        .map(|toplevel| toplevel.wl_surface().clone())
                })
        });
        tiled.or(live)
    }

    /// Prunes completed animations and reports whether another frame is
    /// needed even when no client or input event produced damage.
    pub fn has_active_animation(&mut self) -> bool {
        // Completion cleanup must not depend on an output actually being
        // rendered. A DPMS-off/minimized output can skip its frame path;
        // pruning here still releases the full-output transition texture
        // on the event-loop tick that notices the animation ended.
        self.workspace_transitions
            .retain(|_, transition| !transition.finished());
        self.workspace_glides
            .retain(|_, transition| !transition.finished());
        self.depth_transitions
            .retain(|_, transition| !transition.finished());
        self.window_open_animations
            .retain(|_, animation| !animation.finished());
        self.cascade_window_animations
            .retain(|_, animation| !animation.finished());
        let scroll_was_moving = self.config.spatial_engine == crate::config::SpatialEngine::Scroll
            && !self.window_move_animations.is_empty();
        self.window_move_animations
            .retain(|_, animation| !animation.finished());
        if scroll_was_moving {
            self.queue_scroll_pointer_refresh();
        }
        self.window_viscosity.retain(|_, motion| !motion.finished());
        self.window_sway.retain(|_, sway| !sway.finished());
        self.window_float_physics
            .retain(|_, physics| !physics.finished());
        let stale_ambient: Vec<WlSurface> = self
            .window_float_ambient
            .keys()
            .filter(|surface| !self.is_floating(surface))
            .cloned()
            .collect();
        for surface in stale_ambient {
            self.window_float_ambient.remove(&surface);
        }
        self.swim_cameras.retain(|_, camera| !camera.at_rest());
        if self
            .workspace_scroll_cameras
            .values()
            .any(crate::swim::WorkspaceScrollCamera::at_rest)
            && matches!(self.session_lock, SessionLock::Unlocked)
        {
            // The camera samples its final zero offset on the tick that
            // finishes it. Ask for one more frame before dropping the entry
            // so the last presented frame is not left a fraction of a
            // workspace off.
            self.request_redraw();
        }
        self.workspace_scroll_cameras
            .retain(|_, camera| !camera.at_rest());
        let camera_was_active = self.ocean.has_active_camera_motion();
        self.ocean.prune_completed_camera_motions();
        if camera_was_active && !self.ocean.has_active_camera_motion() {
            self.queue_pointer_focus_refresh();
        }
        self.closing_window_animations
            .retain(|closing| !closing.finished());
        self.ripples.retain(|ripple| !ripple.finished());
        // Desktop content is invisible behind a session lock. Suppress the
        // animation predicate so the fast timer, per-CRTC dirty re-arm, and
        // global redraw request all stand down; lock surfaces render through
        // their own commit path. Pruning above still runs so finished
        // animations are cleaned up regardless of lock state.
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            return false;
        }
        self.toast
            .as_ref()
            .is_some_and(|toast| toast.needs_continued_redraw())
            || !self.ripples.is_empty()
            || !self.workspace_transitions.is_empty()
            || !self.workspace_glides.is_empty()
            || !self.depth_transitions.is_empty()
            || !self.window_open_animations.is_empty()
            || !self.cascade_window_animations.is_empty()
            || !self.window_move_animations.is_empty()
            || !self.window_viscosity.is_empty()
            || !self.window_sway.is_empty()
            || !self.window_float_physics.is_empty()
            // Deliberately not pruned by a `finished()` check like the
            // physics map above -- ambient entries persist until
            // `toggle_float_ambient` removes them, so their mere presence
            // (not decay state) is what should keep the pump alive.
            || !self.window_float_ambient.is_empty()
            // Pruned inside `update_float_physics_full` itself (which also
            // needs to know whether the wave field is still forcing a
            // given body, not just whether it's numerically settled), not
            // here -- see that function's own retain call.
            || !self.window_float_bodies.is_empty()
            || self
                .window_currents
                .values()
                .any(crate::currents::CurrentDrift::needs_frame)
            || self
                .window_buoyancy
                .values()
                .any(crate::buoyancy::BuoyancyState::needs_frame)
            || !self.swim_cameras.is_empty()
            || !self.workspace_scroll_cameras.is_empty()
            || self.ocean.has_active_camera_motion()
            || (self.config.spatial_engine == crate::config::SpatialEngine::Ocean
                && self.config.ocean.canvas_marker
                && self.ocean_canvases.values().any(|canvas| {
                    canvas.marker_active(Duration::from_millis(
                        self.config.ocean.canvas_marker_fade_ms,
                    ))
                }))
            || !self.closing_window_animations.is_empty()
            || self.animated_borders_possible()
            || self.glass_animation_active()
            || self.caustics_active()
    }

    /// Continue only the animations present in this output's actual scene.
    /// The backend passes its frame-owned placements, including previews;
    /// durable workspace ownership and Space overlap are not camera visibility.
    /// Completion cleanup remains global in `has_active_animation`.
    pub(crate) fn output_has_active_animation(
        &self,
        output: &Output,
        placements: &[crate::placement::PlacedWindow],
    ) -> bool {
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            return false;
        }
        let name = output.name();
        self.toast
            .as_ref()
            .is_some_and(|toast| toast.needs_continued_redraw())
            || self.ripples.iter().any(|ripple| ripple.output == name)
            || self.workspace_transitions.contains_key(&name)
            || self.workspace_glides.contains_key(&name)
            || self.depth_transitions.contains_key(&name)
            || self.swim_cameras.contains_key(&name)
            || self.workspace_scroll_cameras.contains_key(&name)
            || self.ocean.has_active_camera_motion_on(&name)
            || (self.config.spatial_engine == crate::config::SpatialEngine::Ocean
                && self.config.ocean.canvas_marker
                && self.ocean_canvases.get(&name).is_some_and(|canvas| {
                    canvas.marker_active(Duration::from_millis(
                        self.config.ocean.canvas_marker_fade_ms,
                    ))
                }))
            || self
                .closing_window_animations
                .iter()
                .any(|closing| closing.snapshot.output == name)
            || placements
                .iter()
                .filter_map(|placed| placed.surface())
                .any(|surface| {
                    self.window_open_animations.contains_key(surface)
                        || self.cascade_window_animations.contains_key(surface)
                        || self.window_move_animations.contains_key(surface)
                        || self.window_viscosity.contains_key(surface)
                        || self.window_sway.contains_key(surface)
                        || self.window_float_physics.contains_key(surface)
                        || self.window_float_ambient.contains_key(surface)
                        || self.window_float_bodies.contains_key(surface)
                        || self
                            .window_currents
                            .get(surface)
                            .is_some_and(crate::currents::CurrentDrift::needs_frame)
                        || self
                            .window_buoyancy
                            .get(surface)
                            .is_some_and(crate::buoyancy::BuoyancyState::needs_frame)
                        || self.surface_has_animated_border(surface)
                        || self.surface_has_glass_animation(surface)
                })
            || self
                .caustics_redraw_delay_for_output(output)
                .is_some_and(|delay| delay.is_zero())
    }

    /// Whether any water-glass window's refraction is currently animating
    /// and needs the frame pump kept alive: always in `ambient` mode, only
    /// during the post-disturbance settle tail in `reactive` mode, never
    /// in `static` mode. Entries are also cross-checked against the water
    /// glass branch's own eligibility (backdrop captured, mode resolves to
    /// water) so a window hot-reloaded to frost or plain glass can't keep
    /// the compositor ticking through its leftover entry.
    fn glass_animation_active(&self) -> bool {
        self.glass_anim
            .keys()
            .any(|surface| self.surface_has_glass_animation(surface))
    }

    fn surface_has_glass_animation(&self, surface: &WlSurface) -> bool {
        if !self.config.water_effects
            || !self.backdrop_textures.contains_key(surface)
            || matches!(
                self.window_glass_modes.get(surface),
                Some(crate::config::GlassMode::Frost) | Some(crate::config::GlassMode::Plain)
            )
        {
            return false;
        }
        let Some(anim) = self.glass_anim.get(surface) else {
            return false;
        };
        match self.config.water_glass.animation {
            crate::config::GlassAnimation::Static => false,
            crate::config::GlassAnimation::Ambient => true,
            crate::config::GlassAnimation::Reactive => {
                anim.envelope(self.config.water_glass.settle_ms) > 0.0
            }
        }
    }

    /// Popups remain unclipped above compositor decoration; only the
    /// toplevel's own surface/subsurface tree is rounded.
    #[allow(clippy::too_many_arguments)]
    fn window_surface_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        window: &Window,
        surface: &WlSurface,
        render_location: Point<i32, Logical>,
        visual_size: Size<i32, Logical>,
        alpha: f32,
        rounded_program: Option<GlesTexProgram>,
        floating: bool,
        fullscreen: bool,
    ) -> (
        Vec<crate::backend::udev::OutputRenderElements>,
        Vec<crate::backend::udev::OutputRenderElements>,
    ) {
        let output_scale = output.current_scale().fractional_scale();
        let scale = Scale::from(output_scale);
        let physical_location = render_location.to_physical_precise_round(output_scale);
        let natural_size = window.geometry().size;
        let resize_scale = Scale::from((
            visual_size.w as f64 / natural_size.w.max(1) as f64,
            visual_size.h as f64 / natural_size.h.max(1) as f64,
        ));
        let resize_active =
            (resize_scale.x - 1.0).abs() > 0.0001 || (resize_scale.y - 1.0).abs() > 0.0001;
        let physical_anchor =
            (render_location + window.geometry().loc).to_physical_precise_round(output_scale);
        let mut popups = Vec::new();
        for (popup, popup_offset) in PopupManager::popups_for_surface(surface) {
            let offset = (window.geometry().loc + popup_offset - popup.geometry().loc)
                .to_physical_precise_round(scale);
            for element in render_elements_from_surface_tree(
                renderer,
                popup.wl_surface(),
                physical_location + offset,
                scale,
                alpha,
                Kind::Unspecified,
            ) {
                if resize_active {
                    popups.push(crate::backend::udev::OutputRenderElements::AnimatedSurface(
                        RescaleRenderElement::from_element(element, physical_anchor, resize_scale),
                    ));
                } else {
                    popups.push(crate::backend::udev::OutputRenderElements::Space(
                        SpaceRenderElements::Surface(element),
                    ));
                }
            }
        }

        let rounding = self.rounding_config_for_surface(surface);
        let clip = self.rounding_applies(&rounding, floating, fullscreen);
        let physical_rect = Rectangle::new(render_location + window.geometry().loc, natural_size)
            .to_physical_precise_round(output_scale);
        let radii = rounding.radii.map(|radius| radius * output_scale as f32);
        let raw_main = render_elements_from_surface_tree(
            renderer,
            surface,
            physical_location,
            scale,
            alpha,
            Kind::Unspecified,
        );
        let cascade = self
            .cascade_window_animations
            .get(surface)
            .map(crate::cascade_transition::CascadeTransition::sample);
        let retain_close_snapshot = (self.config.animations.enabled
            && self.config.animations.close.enabled)
            || (self.cascade_liquid_applies(surface)
                && self.config.cascade.drain != crate::config::CascadeLiquidPreset::None);
        if retain_close_snapshot {
            if let Some(snapshot) = crate::window_animation::WindowFrameSnapshot::capture(
                output.name(),
                physical_anchor,
                physical_rect,
                scale,
                resize_scale,
                &raw_main,
            ) {
                self.window_frame_snapshots
                    .insert(surface.clone(), snapshot);
            }
        }
        let mut main = Vec::with_capacity(raw_main.len());
        for element in raw_main {
            let surface_id = smithay::backend::renderer::element::Element::id(&element).clone();
            if let (true, Some(program)) = (clip || cascade.is_some(), rounded_program.clone()) {
                let rounded = crate::decoration::RoundedSurfaceElement::new(
                    element,
                    program,
                    physical_rect,
                    if clip { radii } else { [0.0; 4] },
                    rounding.power,
                    rounding.antialias * output_scale as f32,
                    scale,
                    cascade,
                );
                if resize_active {
                    main.push(
                        crate::backend::udev::OutputRenderElements::AnimatedRoundedSurface(
                            crate::backend::presentation::SurfaceRenderIdentity::new(
                                RescaleRenderElement::from_element(
                                    rounded,
                                    physical_anchor,
                                    resize_scale,
                                ),
                                surface_id,
                            ),
                        ),
                    );
                } else {
                    main.push(crate::backend::udev::OutputRenderElements::RoundedSurface(
                        crate::backend::presentation::SurfaceRenderIdentity::new(
                            rounded, surface_id,
                        ),
                    ));
                }
            } else if resize_active {
                main.push(crate::backend::udev::OutputRenderElements::AnimatedSurface(
                    RescaleRenderElement::from_element(element, physical_anchor, resize_scale),
                ));
            } else {
                main.push(crate::backend::udev::OutputRenderElements::Space(
                    SpaceRenderElements::Surface(element),
                ));
            }
        }
        (popups, main)
    }

    /// A shaded window's or layer's last stage, or `None` when it has no
    /// drawable effect this frame (unassigned, over budget, no program yet,
    /// or a chain that hasn't rendered). `window` carries a window's visual
    /// opacity and output scale for its rounding; layers draw unrounded at
    /// full opacity, masked by their `ignore_alpha` capture when set.
    fn custom_shader_last_stage(
        &mut self,
        renderer: &mut GlesRenderer,
        surface: &WlSurface,
        (capture_id, capture_version, capture_texture): (
            smithay::backend::renderer::element::Id,
            usize,
            &GlesTexture,
        ),
        physical_rect: Rectangle<i32, Physical>,
        window: Option<(f32, f32)>,
        alpha_mask: Option<&(GlesTexture, usize, f32)>,
    ) -> Option<crate::shader_effect::CustomShaderElement> {
        if self.custom_shader_bypassed.contains(surface) {
            return None;
        }
        let workspace = self.workspace_of_surface(surface);
        let (name, definition) = surface_shader(
            &self.config,
            &self.layer_shader_assignments,
            &self.window_shader_assignments,
            surface,
            workspace,
        )?;
        let stages = &definition.stages;
        let index = stages.len().checked_sub(1)?;
        let lookup = self.custom_shader_programs.lookup(
            renderer,
            name,
            index,
            stages.len(),
            &stages[index],
            &mut self.custom_shader_compile_budget,
        );
        // The last stage reads the backdrop or the chain's outputs; a chain
        // that hasn't rendered can't draw yet.
        let instance = self.custom_shader_instances.get(surface);
        let targets = instance.map(|instance| instance.targets()).unwrap_or(&[]);
        let chain_version = instance.and_then(|instance| instance.chain_version());
        let resolved = lookup.program.as_ref().and_then(|program| {
            if index > 0 && chain_version.is_none() {
                return None;
            }
            let input = crate::shader_effect::stage_input(index, stages, targets, capture_texture)?;
            let textures =
                crate::shader_effect::stage_textures(program, stages, targets, capture_texture)?;
            let base = crate::shader_effect::with_mask_version(
                chain_version.unwrap_or(capture_version as u64),
                alpha_mask.map(|(_, version, threshold)| (*version, *threshold)),
            );
            let content_version =
                crate::shader_effect::stage_content_version(base, &stages[index], program);
            Some((input, textures, content_version))
        });
        if lookup.deferred {
            self.request_redraw();
        }
        if let Some(message) = lookup.failure {
            self.queue_shader_failure(message);
        }
        let (program, (input, textures, content_version)) = lookup.program.zip(resolved)?;
        let (corner_radii, rounding_power, antialias, opacity) = match window {
            Some((opacity, output_scale)) => {
                let rounding = self.rounding_config_for_surface(surface);
                let corner_radii = if rounding.enabled {
                    rounding.radii.map(|radius| radius * output_scale)
                } else {
                    [0.0; 4]
                };
                (
                    corner_radii,
                    rounding.power,
                    rounding.antialias * output_scale,
                    opacity,
                )
            }
            None => ([0.0; 4], 2.0, 0.0, 1.0),
        };
        let commit = crate::shader_effect::custom_shader_commit(
            content_version,
            program.generation,
            (physical_rect.size.w, physical_rect.size.h),
            &program.params,
            corner_radii,
            rounding_power,
            antialias,
            opacity,
        );
        let instance = self
            .custom_shader_instances
            .entry(surface.clone())
            .or_insert_with(crate::shader_effect::ShaderInstance::new);
        // A chain sampled its clock when its stages rendered.
        let timing = if chain_version.is_some() {
            instance.timing()
        } else {
            instance.sample(commit)
        };
        let mut element = crate::shader_effect::CustomShaderElement::new(
            capture_id,
            commit,
            input,
            physical_rect,
            program,
            corner_radii,
            rounding_power,
            antialias,
            opacity,
            timing,
        )
        .with_textures(textures);
        if let Some((mask, _, threshold)) = alpha_mask {
            element = element.with_alpha_mask(mask.clone(), *threshold);
        }
        Some(element)
    }

    pub(crate) fn depth_transition_frame_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Option<crate::backend::udev::OutputRenderElements> {
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            return None;
        }
        let output_name = output.name();
        if self
            .depth_transitions
            .get(&output_name)
            .is_some_and(|transition| transition.finished())
        {
            self.depth_transitions.remove(&output_name);
            return None;
        }
        let area = Rectangle::from_size(self.space.output_geometry(output)?.size);
        let program = crate::depth_transition::depth_transition_program(
            &mut self.depth_transition_program,
            renderer,
        )?;
        self.depth_transitions
            .get_mut(&output_name)
            .map(|transition| {
                crate::backend::udev::OutputRenderElements::DepthTransition(
                    transition.frame_element(program, area),
                )
            })
    }

    pub(crate) fn ocean_canvas_frame_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Option<crate::backend::udev::OutputRenderElements> {
        if !matches!(self.session_lock, SessionLock::Unlocked)
            || self.config.spatial_engine != crate::config::SpatialEngine::Ocean
        {
            return None;
        }
        let output_name = output.name();
        let area = Rectangle::from_size(self.space.output_geometry(output)?.size);
        let camera = self.ocean.camera(&output_name);
        let origin = [camera.origin.x as f32, camera.origin.y as f32];
        let zoom = camera.zoom as f32;
        let grid_size = self.config.ocean.canvas_grid_size as f32;
        let opacity = if self.config.ocean.canvas_guides {
            self.config.ocean.canvas_grid_alpha
        } else {
            0.0
        };
        let marker_fade = Duration::from_millis(self.config.ocean.canvas_marker_fade_ms);
        let marker_alpha = self
            .ocean_canvases
            .entry(output_name.clone())
            .or_default()
            .note_camera(origin, zoom, self.config.ocean.canvas_marker, marker_fade);
        if opacity <= 0.0 && marker_alpha <= 0.0 {
            return None;
        }
        let program =
            crate::ocean_canvas::ocean_canvas_program(&mut self.ocean_canvas_program, renderer)?;
        let element = self
            .ocean_canvases
            .entry(output_name)
            .or_default()
            .frame_element(
                program,
                area,
                crate::ocean_canvas::OceanCanvasSample {
                    origin,
                    zoom,
                    grid_size,
                    color: [0.25, 0.78, 0.92],
                    opacity,
                    marker_alpha,
                },
            );
        Some(crate::backend::udev::OutputRenderElements::OceanCanvas(
            element,
        ))
    }

    /// Builds the unlocked wallpaper-level caustics element. Zero FPS advances
    /// only with other damage; positive FPS uses the per-output deadline.
    pub(crate) fn caustics_frame_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Option<crate::backend::udev::OutputRenderElements> {
        if !matches!(self.session_lock, SessionLock::Unlocked)
            || !self.config.water_effects
            || !self.config.caustics.enabled
            || self.config.caustics.intensity <= 0.0
        {
            return None;
        }
        let area = Rectangle::from_size(self.space.output_geometry(output)?.size);
        let output_scale = output.current_scale().fractional_scale();
        let effective_fps = self.caustics_effective_fps();
        let retry_cadence = if effective_fps == 0 {
            crate::backend::udev::live_output_refresh_period(output)?
        } else {
            Duration::from_secs_f64(1.0 / f64::from(effective_fps))
        };
        let program =
            crate::caustics::caustics_program(&mut self.caustics_program, renderer, retry_cadence)?;
        let output_name = output.name();
        let advance = if self.config.caustics.fps == 0 {
            true
        } else if effective_fps == 0 {
            false
        } else {
            self.caustics
                .get(&output_name)
                .and_then(|caustics| caustics.next_frame_in(effective_fps))
                .is_none_or(|delay| delay.is_zero())
        };
        let element = self
            .caustics
            .entry(output_name)
            .or_default()
            .frame_element(
                renderer,
                program,
                area,
                output_scale,
                &self.config.caustics,
                crate::caustics::CausticsFrameTiming {
                    advance,
                    retry_cadence,
                },
            )?;
        Some(crate::backend::udev::OutputRenderElements::Caustics(
            element,
        ))
    }

    /// The frame rate the caustics overlay should currently run at:
    /// `caustics.fps` when the user is active, stepping down through the
    /// configured idle tiers as minutes pass without input. Empty tier
    /// lists (or a short list) leave the rate at `fps` for the remaining
    /// idle time. Both lists are parallel and ascending in time. A tier
    /// of 0 means "stop pumping entirely" -- `caustics_active` treats it
    /// like `fps = 0` (frozen, zero extra frames), never as a 1Hz zombie.
    fn caustics_effective_fps(&self) -> u32 {
        let cfg = &self.config.caustics;
        let mut fps = cfg.fps;
        if cfg.idle_fps.is_empty() || cfg.idle_after_ms.is_empty() {
            return fps;
        }
        let idle_ms = self.last_activity.elapsed().as_millis() as u64;
        for (after_ms, tier_fps) in cfg.idle_after_ms.iter().zip(&cfg.idle_fps) {
            if idle_ms >= *after_ms {
                fps = (*tier_fps).min(fps);
            }
        }
        fps
    }

    pub(crate) fn caustics_redraw_delay(&self) -> Option<Duration> {
        self.space
            .outputs()
            .filter_map(|output| self.caustics_redraw_delay_for_output(output))
            .min()
    }

    pub(crate) fn caustics_redraw_delay_for_output(&self, output: &Output) -> Option<Duration> {
        let cfg = &self.config.caustics;
        if !matches!(self.session_lock, SessionLock::Unlocked)
            || !self.config.water_effects
            || !cfg.enabled
            || cfg.intensity <= 0.0
            || cfg.fps == 0
            || cfg.speed <= 0.0
        {
            return None;
        }
        let fps = self.caustics_effective_fps();
        if fps == 0 {
            return None;
        }
        let period = Duration::from_secs_f64(1.0 / f64::from(fps));
        let program_retry = self.caustics_program.retry_in(period);
        Some(
            self.caustics
                .get(&output.name())
                .and_then(|caustics| caustics.next_frame_in(fps))
                .unwrap_or(period)
                .max(program_retry),
        )
    }

    fn caustics_active(&self) -> bool {
        self.caustics_redraw_delay()
            .is_some_and(|delay| delay.is_zero())
    }

    /// Builds render-only Ocean edge cues for off-viewport urgent/deep windows.
    /// An empty cue set creates no element or idle work.
    pub(crate) fn compass_frame_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<crate::backend::udev::OutputRenderElements> {
        if !matches!(self.session_lock, SessionLock::Unlocked)
            || self.config.spatial_engine != crate::config::SpatialEngine::Ocean
            || !self.config.water_effects
            || !self.config.compass.enabled
        {
            return Vec::new();
        }
        let Some(output_geo) = self.space.output_geometry(output) else {
            return Vec::new();
        };
        let output_name = output.name();
        let camera = self.ocean.camera(&output_name);
        let zoom = camera.zoom.max(0.05);
        let viewport = Rectangle::new(
            Point::from((camera.origin.x, camera.origin.y)),
            Size::from((
                output_geo.size.w as f64 / zoom,
                output_geo.size.h as f64 / zoom,
            )),
        );
        let cfg = self.config.compass;
        let windows: Vec<(Point<f64, Logical>, crate::compass::CueKind)> = self
            .ocean
            .world_layouts(self.config.gaps, self.config.bsp_split_bias)
            .into_iter()
            .filter_map(|(window, rect, _)| {
                let surface = window.toplevel()?.wl_surface().clone();
                if !self.ocean.visible_on_output(&surface, &output_name) {
                    return None;
                }
                let kind = if self.urgent.contains(&surface) {
                    crate::compass::CueKind::Urgent
                } else {
                    crate::compass::CueKind::Deep
                };
                Some((
                    Point::from((
                        rect.loc.x as f64 + rect.size.w as f64 / 2.0,
                        rect.loc.y as f64 + rect.size.h as f64 / 2.0,
                    )),
                    kind,
                ))
            })
            .collect();
        let cues = crate::compass::compute_cues(
            viewport,
            zoom,
            &windows,
            &crate::compass::CompassParams {
                urgent_color: cfg.urgent_color,
                deep_color: cfg.deep_color,
                max_distance: cfg.max_distance,
                peak_alpha: cfg.alpha,
                size: cfg.size,
                shape: cfg.shape,
            },
        );
        let Some(program) = crate::compass::compass_program(&mut self.compass_program, renderer)
        else {
            return Vec::new();
        };
        let compass = self.compasses.entry(output_name).or_default();
        compass
            .frame_elements(program, cues)
            .into_iter()
            .map(crate::backend::udev::OutputRenderElements::Compass)
            .collect()
    }

    /// Opens the Ocean minimap from current `world_layouts`. Active pointer
    /// grabs suppress it because consuming the matching release would strand
    /// their commit; popup grabs are safe and remain eligible. Callers only
    /// intercept the trigger if this method actually opens the peek.
    pub(crate) fn open_minimap_peek(&mut self, output: &Output) {
        if self.minimap_peek.is_some()
            || self.config.spatial_engine != crate::config::SpatialEngine::Ocean
            || !self.config.minimap.enabled
            || !matches!(self.session_lock, SessionLock::Unlocked)
            || (self.popup_grab.is_none()
                && self
                    .seat
                    .get_pointer()
                    .is_some_and(|pointer| pointer.is_grabbed()))
        {
            return;
        }
        let Some(output_geometry) = self.space.output_geometry(output) else {
            return;
        };
        let output_name = output.name();
        let pointer_location = self
            .seat
            .get_pointer()
            .map(|pointer| pointer.current_location())
            .unwrap_or_default();

        let windows: Vec<(Rectangle<i32, Logical>, String, bool)> = self
            .ocean
            .world_layouts(self.config.gaps, self.config.bsp_split_bias)
            .into_iter()
            .filter_map(|(window, rect, _kind)| {
                let surface = window.toplevel()?.wl_surface().clone();
                if !self.ocean.visible_on_output(&surface, &output_name) {
                    return None;
                }
                let urgent = self.urgent.contains(&surface);
                Some((rect, crate::tab_strip::window_title(&surface), urgent))
            })
            .collect();
        let reef_rects = self.ocean.reef_rects_on_output(&output_name);
        let viewports: Vec<(String, Rectangle<i32, Logical>)> = self
            .space
            .outputs()
            .filter_map(|candidate| {
                if !self.config.ocean.shared_canvas && candidate.name() != output_name {
                    return None;
                }
                let geo = self.space.output_geometry(candidate)?;
                let camera = self.ocean.camera(&candidate.name());
                let zoom = camera.zoom.max(0.05);
                let size = Size::from((
                    (geo.size.w as f64 / zoom).round() as i32,
                    (geo.size.h as f64 / zoom).round() as i32,
                ));
                let loc = Point::from((
                    camera.origin.x.round() as i32,
                    camera.origin.y.round() as i32,
                ));
                Some((candidate.name(), Rectangle::new(loc, size)))
            })
            .collect();

        let style = crate::minimap::MinimapStyle::resolve(
            self.config.minimap.preset,
            self.config.minimap.background_color,
            self.config.minimap.window_color,
            self.config.minimap.accent_color,
        );
        self.minimap_peek = Some(crate::minimap::MinimapPeek::build(
            output_name,
            output_geometry,
            pointer_location,
            style,
            &windows,
            &reef_rects,
            &viewports,
        ));
        self.request_redraw();
    }

    /// Dismisses the minimap peek without traveling, if one is open.
    /// Idempotent -- both the chord-release path and the post-click path
    /// in `minimap_click_travel` may call this for the same peek.
    pub(crate) fn close_minimap_peek(&mut self) {
        if self.minimap_peek.take().is_some() {
            self.refresh_pointer_focus();
            self.request_redraw();
        }
    }

    /// Live pointer position feed for the open peek, so a click can resolve
    /// a world point without reading `current_location()` from inside a
    /// grab/button dispatch -- see `MinimapPeek::last_location`'s own doc
    /// for why that specific call shape is what froze this compositor
    /// twice before (`TileMoveGrab` 0.15.1, `sync_visible_floating_window`).
    pub(crate) fn update_minimap_pointer(&mut self, location: Point<f64, Logical>) {
        if let Some(peek) = self.minimap_peek.as_mut() {
            peek.set_last_location(location);
        }
    }

    /// Resolves the peek's last known pointer position to a world point,
    /// travels the triggering output's Ocean camera there, and dismisses.
    /// A no-op if no peek is open (nothing left to click).
    pub(crate) fn minimap_click_travel(&mut self) {
        let Some(peek) = self.minimap_peek.take() else {
            return;
        };
        let world = peek.world_point_at_last_location();
        let output_name = peek.output_name().to_string();
        let Some(viewport) = self
            .space
            .outputs()
            .find(|candidate| candidate.name() == output_name)
            .and_then(|candidate| self.space.output_geometry(candidate))
            .map(|geo| geo.size)
        else {
            self.refresh_pointer_focus();
            self.request_redraw();
            return;
        };
        let target = Rectangle::new(
            Point::from((world.x.round() as i32, world.y.round() as i32)),
            Size::from((0, 0)),
        );
        self.ocean.center_on_rect(
            &output_name,
            viewport,
            target,
            Duration::from_millis(self.config.ocean.camera_animation_ms),
            self.config.ocean.camera_sway,
        );
        self.refresh_pointer_focus();
        self.request_redraw();
    }

    /// Off-screen render element for the open minimap peek, filtered to the
    /// output it was built for -- same raw `Option<MemoryRenderBufferRenderElement<_>>`
    /// shape as `overview.rs`'s own `render_element`, so both backends'
    /// existing `overview_element`-style `.chain()` assembly can carry it
    /// too, wrapped by the same single `OutputRenderElements::Composited`
    /// map at the end -- no new element type needed.
    pub(crate) fn minimap_frame_element(
        &self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let Some(peek) = self
            .minimap_peek
            .as_ref()
            .filter(|peek| peek.output_name() == output.name())
        else {
            return Vec::new();
        };
        let mut elements = Vec::with_capacity(2);
        // Front-to-back, index 0 topmost: the cursor glyph must come first
        // so it draws *over* the map, not beneath it -- the map's panel is
        // translucent, and a cursor behind it showed as a faint ghost (the
        // same element-order class as the session-lock bug).
        let scale = output.current_scale().fractional_scale();
        let output_loc = self
            .space
            .output_geometry(output)
            .map(|geo| geo.loc)
            .unwrap_or_else(|| Point::from((0, 0)));
        if let Some(cursor) = peek.cursor_element(renderer, output_loc, scale) {
            elements.push(cursor);
        }
        if let Some(map) = peek.render_element(renderer) {
            elements.push(map);
        }
        elements
    }

    /// Spawns an impulse ripple (`ripple.rs`, Phase R1) on `trigger`,
    /// centered per the resolved `ripple { }` config (and any per-rule
    /// `ripple { }` override matched by `surface`'s app_id/title). The
    /// Map, focus-change, and urgent triggers all reuse this path.
    ///
    /// No-op when:
    /// - `water_effects` is off (the master identity toggle),
    /// - the resolved ripple config's `enabled` is `false` (e.g. a rule
    ///   set `ripple = none` for this app),
    /// - the resolved config's `triggers` doesn't include `trigger`,
    /// - the window has no current position (not yet mapped, on an
    ///   inactive workspace),
    /// - the ripple cap is reached (bounding the worst-case cost of
    ///   rapid window mapping so `self.ripples` can't grow unboundedly).
    pub(crate) fn spawn_ripple(
        &mut self,
        surface: &WlSurface,
        trigger: crate::config::RippleTrigger,
    ) {
        use crate::config::RippleAnchor;
        if !self.config.water_effects {
            return;
        }
        const MAX_ACTIVE_RIPPLES: usize = 16;
        if self.ripples.iter().filter(|r| !r.finished()).count() >= MAX_ACTIVE_RIPPLES {
            return;
        }
        // Resolve the per-window-rule ripple override and merge over the
        // global `ripple { }` block. `resolve_window_rules` returns a
        // WindowRule whose `ripple` field is `Some(...)` only if at
        // least one matching rule declared a ripple sub-block (or the
        // `ripple = none` shorthand).
        let rule = self.resolve_window_rules_for(surface);
        let mut cfg = self
            .config
            .resolve_ripple_config(rule.ripple.as_ref(), trigger);
        if cfg.enabled == Some(false) || !cfg.fires_on(trigger) {
            return;
        }

        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        // A Classic tiled window belongs to its layout's output even while
        // its committed size still overlaps a neighbor.
        let layout_owner = (self.config.spatial_engine.uses_workspaces())
            .then(|| self.layout.output_of(surface))
            .flatten()
            .and_then(|name| self.output_by_name(name));
        let Some(output) = layout_owner
            .or_else(|| self.rendered_output_for_surface(surface))
            .or_else(|| self.output_for_window(&window))
        else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        // A window's `window.geometry()` can lag one client
        // round-trip behind retile()'s own just-computed target rect:
        // retile() only *sends* a configure proposing the new size, it
        // doesn't block on the client acking and committing a matching
        // buffer -- so reading geometry() synchronously right after (as
        // the map trigger does, straight after retile() in map_toplevel)
        // sees the window's pre-tile size at its already-updated post-tile
        // location. Confirmed live: a freshly mapped tiled window's map
        // ripple anchored at (8, 8) -- the gap offset, i.e. loc with a
        // near-zero size -- while a later ripple on the same settled
        // window anchored at its real center. Same "space reflects where
        // this is being moved to, not its real slot" gap
        // `TileMoveGrab::drop` already hit; its own doc comment is why
        // this reads from the retained placement state instead when possible.
        // A first-map floating conversion has the same lag: its
        // `FloatingTag::rect` already records the configure target while the
        // committed buffer may still have the old size. Use that rect for an
        // ordinary floater so its map ripple lands at the settled center,
        // including an exact rule-provided position/size.
        // Skipped for fullscreen/pseudo-tiled windows, whose rect
        // `retile()` overrides after reading it from `layout()` -- rarer,
        // and reusing the plain path here is a no-op change for them, not
        // a regression.
        let win = if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            self.rendered_rect_on_output(surface, &output)
        } else {
            self.layout
                .workspace_of(surface)
                .filter(|_| {
                    !self.fullscreen.contains_key(surface) && !self.pseudo_tiled.contains(surface)
                })
                .zip(self.output_tiling_area(&output))
                .and_then(|(workspace, area)| {
                    self.layout
                        .layout(
                            &output.name(),
                            workspace,
                            area,
                            self.gaps_for(&output.name(), workspace),
                        )
                        .into_iter()
                        .find(|(w, _)| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
                        .map(|(_, rect)| rect)
                })
                .or_else(|| {
                    (!self.fullscreen.contains_key(surface)
                        && !self.maximized.contains_key(surface))
                    .then(|| self.floating_workspace.get(surface).map(|tag| tag.rect))
                    .flatten()
                })
                .or_else(|| {
                    self.space
                        .element_location(&window)
                        .map(|loc| Rectangle::new(loc, window.geometry().size))
                })
        };
        let Some(win) = win else {
            return;
        };
        // Ripple geometry is output-local. Ocean's authoritative placement
        // above already includes its camera/pin transform.
        let win = Rectangle::new(win.loc - output_geo.loc, win.size);
        cfg.peak_radius = Some(cfg.radius_for_window(win.size.w as f32, win.size.h as f32));
        let anchor = cfg.anchor.unwrap_or(RippleAnchor::Center);
        let (dx, dy) = cfg.offset.unwrap_or((0, 0));
        let pointer_local = self
            .seat
            .get_pointer()
            .map(|p| p.current_location())
            .map(|g| Point::from((g.x - output_geo.loc.x as f64, g.y - output_geo.loc.y as f64)));
        let base = crate::ripple::anchor_point(
            win,
            anchor,
            pointer_local,
            cfg.edge_position.unwrap_or(0.5),
            cfg.edge_offset.unwrap_or(0.0),
        );
        let center = Point::from((base.x + dx as f64, base.y + dy as f64));
        tracing::debug!(
            trigger = ?trigger,
            output = output.name(),
            center = ?center,
            preset = ?cfg.preset,
            shapes = ?cfg.shapes,
            color = ?cfg.color,
            layer = ?cfg.layer,
            "spawning ripple"
        );
        self.ripples
            .push(crate::ripple::Ripple::new(output.name(), center, cfg));
        self.request_redraw();
    }

    /// Back-compat alias for the first call site, kept while focus/urgent
    /// triggers are being wired up -- new call sites should use
    /// `spawn_ripple` with an explicit trigger directly.
    pub(crate) fn spawn_window_map_ripple(&mut self, surface: &WlSurface) {
        self.spawn_ripple(surface, crate::config::RippleTrigger::Map);
    }

    /// Builds, for every not-yet-finished ripple on `output`, a
    /// `RippleElement` ready to prepend into a frame's render-element
    /// list. Prunes finished ripples as a side effect, so this Vec stays
    /// bounded by the in-flight count between frames even if a caller
    /// forgets to drain it. Lazily compiles the shader on first call.
    ///
    /// Ripples are grouped by their configured `layer` (`RippleLayer`)
    /// so the backend element-chain builders can splice each group into
    /// the right z-position: `AboveWindows` ripples go over windows but
    /// under chrome, `BelowWindows` go between wallpaper and windows,
    /// `AboveAll` goes at the very front, `BelowAll` at the very back.
    pub(crate) fn ripple_frame_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> RippleLayers {
        let mut layers = RippleLayers::default();
        if !self.config.water_effects || self.ripples.is_empty() {
            return layers;
        }
        self.ripples.retain(|r| !r.finished());
        if self.ripples.is_empty() {
            return layers;
        }
        let Some(program) = crate::ripple::ripple_program(&mut self.ripple_program, renderer)
        else {
            return layers;
        };
        let output_name = output.name();
        for ripple in self.ripples.iter_mut() {
            if ripple.output != output_name {
                continue;
            }
            for element in crate::ripple::RippleElement::from_ripple(ripple, program.clone()) {
                layers.push(ripple.layer(), element);
            }
        }
        layers
    }

    pub(crate) fn tiled_rect_for_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<Rectangle<i32, Logical>> {
        let output_name = self.layout.output_of(surface)?;
        let workspace = self.layout.workspace_of(surface)?;
        let output = self.output_by_name(output_name)?;
        let area = self.output_tiling_area(&output)?;
        let rect = self
            .layout
            .layout(
                output_name,
                workspace,
                area,
                self.gaps_for(output_name, workspace),
            )
            .into_iter()
            .find_map(|(window, rect)| is_window(&window, surface).then_some(rect))?;
        Some(if self.pseudo_tiled.contains(surface) {
            crate::layout::scale_centered(rect, self.config.pseudo_tile_scale)
        } else {
            rect
        })
    }

    /// Clamps `pos` into the union of every mapped output. Needed for
    /// relative pointer motion (a real mouse's delta accumulates onto the
    /// last known position with nothing else bounding it) -- absolute motion
    /// doesn't need this, it's already transformed against one output's
    /// geometry directly. Rectangle right/bottom edges are exclusive, so
    /// the nearest valid representable point is used when motion reaches an
    /// outer edge or a gap in an L-shaped layout.
    pub(crate) fn clamp_to_outputs(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        nearest_point_in_output_rects(
            pos,
            self.space
                .outputs()
                .filter_map(|output| self.space.output_geometry(output)),
        )
        .unwrap_or(pos)
    }

    /// Recomputes the tiling layout for every output and applies it: sends
    /// each tiled window its new size (`send_pending_configure`) and
    /// updates its position in `space`. Position takes effect immediately;
    /// visible size catches up once the client commits a matching buffer,
    /// same one-frame-lag tradeoff `resize_grab` already makes.
    ///
    /// Each output tiles independently -- see `layout::Layouts` -- so this
    /// just loops every mapped output and applies that output's own tree
    /// into that output's own (exclusive-zone-adjusted) area.
    ///
    /// A fullscreen window (`self.fullscreen`) keeps its `Layouts` slot the
    /// whole time it's fullscreen -- see `handlers/xdg_shell.rs` -- so this
    /// is also where that's reconciled: its slot's rect gets overridden to
    /// the *full* output geometry (ignoring gaps and exclusive zones, unlike
    /// every other rect here) rather than removed from the tree and
    /// reinserted later. A pseudo-tiled window (`self.pseudo_tiled`, see
    /// `toggle_pseudo_tile`) gets the same treatment on a smaller scale:
    /// its slot's rect is shrunk to `config.pseudo_tile_scale` of itself,
    /// centered, rather than filling the tile. Fullscreen always wins if a
    /// window is somehow both.
    pub fn retile(&mut self) {
        self.retile_with_viscosity(false, None);
    }

    /// Interactive tiled resize/drop path. It shares every layout and
    /// protocol-state operation with `retile`, changing only the short-lived
    /// render follower chosen for geometry that moved under the pointer.
    pub(crate) fn retile_viscous(&mut self) {
        self.retile_with_viscosity(true, None);
    }

    /// Same as `retile`, but withholds the tiled-size configure for one
    /// surface. `map_toplevel` calls this instead of `retile` when a
    /// window rule (float/pin/maximize/pseudo-tile/the auto-float
    /// heuristic) is about to immediately re-tile it again: without this,
    /// the client received a tiled-size configure here and a
    /// differently-sized one moments later from that conversion, and a
    /// terminal visibly re-flows its text grid between the two. Bookkeeping
    /// (`space.map_element`, staged pending state) still runs as normal --
    /// `toggle_floating`/`toggle_pseudo_tile` need the former to find the
    /// window at all, and the latter is harmless since the conversion's own
    /// `with_pending_state` call overwrites it before anything is sent.
    pub(crate) fn retile_skip_first_configure(&mut self, skip: &WlSurface) {
        self.retile_with_viscosity(false, Some(skip));
    }

    fn retile_ocean(&mut self, skip_configure_for: Option<&WlSurface>) {
        self.ocean
            .set_shared_canvas(self.config.ocean.shared_canvas);
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let Some(seed_output) = outputs.first() else {
            return;
        };
        let Some(seed_geo) = self.space.output_geometry(seed_output) else {
            return;
        };
        self.ocean
            .ensure_default_reef(&seed_output.name(), seed_geo.size);
        self.ocean
            .clamp_screen_pins(&seed_output.name(), seed_geo.size);
        for output in outputs.iter().skip(1) {
            if let Some(geometry) = self.space.output_geometry(output) {
                self.ocean
                    .ensure_default_reef(&output.name(), geometry.size);
                self.ocean.clamp_screen_pins(&output.name(), geometry.size);
            }
        }

        let tiled = self
            .ocean
            .tiled_layouts(self.config.gaps, self.config.bsp_split_bias);
        for (window, rect) in &tiled {
            if let Some(toplevel) = window.toplevel() {
                let target_size = self
                    .fullscreen
                    .get(toplevel.wl_surface())
                    .and_then(|entry| self.output_by_name(&entry.output))
                    .and_then(|output| self.space.output_geometry(&output))
                    .map(|geometry| geometry.size)
                    .unwrap_or(rect.size);
                toplevel.with_pending_state(|state| state.size = Some(target_size));
                if skip_configure_for != Some(toplevel.wl_surface()) {
                    toplevel.send_pending_configure();
                }
            }
        }

        for (window, rect, _) in self.ocean.world_layouts_from_tiled(tiled) {
            let output = self
                .output_for_window(&window)
                .unwrap_or_else(|| seed_output.clone());
            self.set_window_fractional_scale(&window, &output);
            // Space remains a protocol/input cache in Ocean. The shared
            // renderer uses world rect + camera translation, never this
            // presentation coordinate as spatial ownership.
            self.space.map_element(window, rect.loc, false);
        }

        self.request_redraw();
    }

    fn retile_with_viscosity(&mut self, interactive: bool, skip_configure_for: Option<&WlSurface>) {
        self.retile_filtered(interactive, skip_configure_for, None);
    }

    fn retile_filtered(
        &mut self,
        interactive: bool,
        skip_configure_for: Option<&WlSurface>,
        scoped_output: Option<&str>,
    ) {
        self.layout.scroll.config = (self.config.spatial_engine
            == crate::config::SpatialEngine::Scroll)
            .then_some(self.config.scroll);
        self.layout.scroll.fullscreen_extents =
            if self.config.spatial_engine == crate::config::SpatialEngine::Scroll {
                self.fullscreen
                    .iter()
                    .filter_map(|(surface, entry)| {
                        if !self.layout.contains(surface) {
                            return None;
                        }
                        let output = self.output_by_name(&entry.output)?;
                        let geometry = self.space.output_geometry(&output)?;
                        let extent = match self.config.scroll.orientation {
                            crate::scroll::Orientation::Horizontal => geometry.size.w,
                            crate::scroll::Orientation::Vertical => geometry.size.h,
                        };
                        Some((surface.clone(), f64::from(extent)))
                    })
                    .collect()
            } else {
                HashMap::new()
            };
        self.buoyancy_dirty = true;
        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            self.retile_ocean(skip_configure_for);
            return;
        }
        let outputs: Vec<Output> = self
            .space
            .outputs()
            .filter(|output| scoped_output.is_none_or(|name| output.name() == name))
            .cloned()
            .collect();
        for output in &outputs {
            let Some(area) = self.output_tiling_area(output) else {
                continue;
            };
            let full_output_geo = self.space.output_geometry(output);
            let workspace = self.layout.active_workspace(&output.name());

            let gaps = self.gaps_for(&output.name(), workspace);
            self.layout
                .prepare_scroll(&output.name(), workspace, area, gaps);
            self.anchor_scroll_floaters(&output.name(), workspace);
            self.rescue_scroll_floaters(&output.name(), workspace, area, gaps);
            let window_count = self.layout.window_count(&output.name(), workspace);
            let target_aspect = area.size.w as f32 / (area.size.h as f32).max(1.0);
            self.layout.refresh_cascade_state(
                &output.name(),
                workspace,
                window_count,
                target_aspect,
            );
            for (window, mut rect) in self.layout.layout(&output.name(), workspace, area, gaps) {
                if let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) {
                    if let (Some(entry), Some(full)) =
                        (self.fullscreen.get(&surface), full_output_geo)
                    {
                        if entry.output == output.name() {
                            if self.config.spatial_engine == crate::config::SpatialEngine::Scroll {
                                let gap = gaps
                                    .max(0)
                                    .min(area.size.w.min(area.size.h).saturating_sub(1) / 2);
                                let translation = match self.config.scroll.orientation {
                                    crate::scroll::Orientation::Horizontal => (
                                        i64::from(rect.loc.x)
                                            - i64::from(area.loc.x)
                                            - i64::from(gap),
                                        0,
                                    ),
                                    crate::scroll::Orientation::Vertical => (
                                        0,
                                        i64::from(rect.loc.y)
                                            - i64::from(area.loc.y)
                                            - i64::from(gap),
                                    ),
                                };
                                let x = (i64::from(full.loc.x) + translation.0).clamp(
                                    i64::from(i32::MIN),
                                    i64::from(i32::MAX - full.size.w.max(1)),
                                ) as i32;
                                let y = (i64::from(full.loc.y) + translation.1).clamp(
                                    i64::from(i32::MIN),
                                    i64::from(i32::MAX - full.size.h.max(1)),
                                ) as i32;
                                rect = Rectangle::new((x, y).into(), full.size);
                            } else {
                                rect = full;
                            }
                        }
                    } else if self.pseudo_tiled.contains(&surface) {
                        rect = crate::layout::scale_centered(rect, self.config.pseudo_tile_scale);
                    }
                }
                tracing::trace!(?rect, output = output.name(), "Tiling window");
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.size = Some(rect.size);
                    });
                    if skip_configure_for != Some(toplevel.wl_surface()) {
                        toplevel.send_pending_configure();
                    }
                    // Remember the size this tile promises the client, so
                    // `note_tiled_size_refusal` can tell a client that is
                    // still catching up to it from one that will never take
                    // it. A changed expected size also resets any running
                    // refusal streak: a fresh configure deserves a fresh
                    // grace period.
                    let surface = toplevel.wl_surface().clone();
                    let changed = self
                        .expected_tiled_size
                        .insert(surface.clone(), rect.size)
                        .is_some_and(|old| old != rect.size);
                    if changed {
                        self.tiled_size_refusals.remove(&surface);
                    }
                }
                if let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) {
                    if let Some(old_location) = self.space.element_location(&window) {
                        let old_rect = Rectangle::new(old_location, window.geometry().size);
                        if old_rect != rect {
                            let viscous = (interactive
                                || self.window_viscosity.contains_key(&surface))
                                && self.retarget_window_viscosity(&surface, rect);
                            if matches!(
                                self.compositor_gesture,
                                Some(CompositorGesture::Scroll { .. })
                            ) {
                                self.window_move_animations.remove(&surface);
                            } else if !viscous {
                                self.start_window_move_animation(&surface, old_rect, rect);
                            }
                        }
                    }
                }
                self.set_window_fractional_scale(&window, output);
                self.space.map_element(window, rect.loc, false);
            }

            // A *floating* fullscreen window isn't in `self.layout` at all,
            // so the loop above never sees it -- map it to the full output
            // geometry directly.
            if let Some(full) = full_output_geo {
                let floating_fullscreen: Vec<Window> = self
                    .space
                    .elements()
                    .filter(|w| {
                        w.toplevel().is_some_and(|t| {
                            let surface = t.wl_surface();
                            !self.layout.contains(surface)
                                && self
                                    .fullscreen
                                    .get(surface)
                                    .is_some_and(|e| e.output == output.name())
                        })
                    })
                    .cloned()
                    .collect();
                for window in floating_fullscreen {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|state| {
                            state.states.set(xdg_toplevel::State::Fullscreen);
                            state.states.unset(xdg_toplevel::State::Maximized);
                            state.states.unset(xdg_toplevel::State::Resizing);
                            state.size = Some(full.size);
                        });
                        toplevel.send_pending_configure();
                    }
                    self.set_window_fractional_scale(&window, output);
                    self.space.map_element(window, full.loc, false);
                }
            }

            // Floating maximized windows also live outside Layouts. Keep
            // them reconciled to the current non-exclusive tiling area so a
            // bar/output geometry change cannot leave stale size/location.
            let maximized_rect = crate::layout::inset(area, gaps);
            let floating_maximized: Vec<Window> = self
                .space
                .elements()
                .filter(|window| {
                    window.toplevel().is_some_and(|toplevel| {
                        let surface = toplevel.wl_surface();
                        !self.layout.contains(surface)
                            && !self.fullscreen.contains_key(surface)
                            && self
                                .maximized
                                .get(surface)
                                .is_some_and(|entry| entry.output == output.name())
                    })
                })
                .cloned()
                .collect();
            for window in floating_maximized {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.states.set(xdg_toplevel::State::Maximized);
                        state.states.unset(xdg_toplevel::State::Fullscreen);
                        state.states.unset(xdg_toplevel::State::Resizing);
                        state.size = Some(maximized_rect.size);
                    });
                    toplevel.send_pending_configure();
                }
                self.set_window_fractional_scale(&window, output);
                self.space.map_element(window, maximized_rect.loc, false);
            }

            // Floating ownership survives inactive workspaces, unlike
            // `Space`. Reconcile every tagged window so hidden floaters and
            // saved fullscreen/maximize geometry cannot retain a stale scale
            // or become unreachable after the logical output shrinks.
            if let Some(full) = full_output_geo {
                let floating: Vec<(WlSurface, Window)> = self
                    .floating_workspace
                    .iter()
                    .filter(|(_, tag)| tag.output == output.name())
                    .map(|(surface, tag)| (surface.clone(), tag.window.clone()))
                    .collect();
                for (surface, window) in floating {
                    // A shown window takes the scale of the output it is
                    // actually displayed on. A pinned window's tag can
                    // still name the output it was pinned on (see
                    // `toggle_pin`), and using it made a pinned window on
                    // another monitor flip to that monitor's scale on
                    // every retile -- visible as content briefly drawn
                    // too small during workspace switches.
                    let scale_output = self
                        .output_for_window(&window)
                        .unwrap_or_else(|| output.clone());
                    self.set_window_fractional_scale(&window, &scale_output);
                    let bounds = self.floating_bounds(&surface, full);

                    if let Some(tag) = self.floating_workspace.get_mut(&surface) {
                        tag.rect = clamp_rect_visible(tag.rect, bounds);
                    }
                    if let Some(entry) = self.fullscreen.get_mut(&surface) {
                        if let Some(rect) = &mut entry.restore_rect {
                            *rect = clamp_rect_visible(*rect, full);
                        }
                    }
                    if let Some(entry) = self.maximized.get_mut(&surface) {
                        entry.restore_rect = clamp_rect_visible(entry.restore_rect, full);
                    }

                    if !self.fullscreen.contains_key(&surface)
                        && !self.maximized.contains_key(&surface)
                        && self.window_is_visible(&surface)
                    {
                        let current = self
                            .space
                            .element_geometry(&window)
                            .or_else(|| self.floating_workspace.get(&surface).map(|tag| tag.rect));
                        if let Some(current) = current {
                            let clamped = clamp_rect_visible(current, bounds);
                            if let Some(tag) = self.floating_workspace.get_mut(&surface) {
                                tag.rect = clamped;
                            }
                            if clamped.loc != current.loc {
                                self.space.map_element(window, clamped.loc, false);
                            }
                        }
                    }
                }
            }
        }

        // Space::map_element always re-raises the element it touches (even
        // with activate: false), so the loop above just knocked every
        // floating window behind the tiled layer it re-stacked. Floating
        // windows should stay on top, normal WM convention, so restore that
        // (preserving floating windows' own relative order) every time.
        let floating: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                w.toplevel()
                    .map(|t| !self.layout.contains(t.wl_surface()))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for window in floating {
            self.space.raise_element(&window, false);
        }

        // Same invariant one level up: a fullscreen window (tiled or
        // floating) should sit above every other window on its output,
        // including floating ones, so it needs its own re-raise pass after
        // the one above.
        let fullscreen: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                w.toplevel()
                    .is_some_and(|t| self.fullscreen.contains_key(t.wl_surface()))
            })
            .cloned()
            .collect();
        for window in fullscreen {
            self.space.raise_element(&window, false);
        }

        self.queue_scroll_pointer_refresh();
        #[cfg(debug_assertions)]
        self.assert_state_invariants();
        self.request_redraw();
    }

    #[cfg(debug_assertions)]
    fn assert_state_invariants(&self) {
        for surface in &self.pinned {
            debug_assert!(
                self.floating_workspace.contains_key(surface),
                "pinned window must remain floating"
            );
            debug_assert!(
                !self.fullscreen.contains_key(surface),
                "fullscreen must suspend actual pin membership"
            );
        }
        for surface in &self.pseudo_tiled {
            debug_assert!(
                self.layout.contains(surface),
                "pseudo-tiled window must remain in Layouts"
            );
        }
        for (surface, maximized) in &self.maximized {
            debug_assert!(
                self.floating_workspace.contains_key(surface),
                "maximized restore state only applies to floating windows"
            );
            debug_assert_eq!(
                self.floating_workspace.get(surface).map(|tag| &tag.output),
                Some(&maximized.output),
                "maximized output must match floating ownership"
            );
        }

        for surface in self.foreign_toplevels.keys() {
            debug_assert!(
                !self.unmapped_toplevels.contains_key(surface),
                "foreign-toplevel handle must not outlive the mapped state"
            );
        }

        let mut fullscreen_outputs = HashSet::new();
        for (surface, entry) in &self.fullscreen {
            debug_assert!(
                fullscreen_outputs.insert(&entry.output),
                "at most one fullscreen window may own an output"
            );
            if !self.unmapped_toplevels.contains_key(surface) {
                if let Some(owner) = self.preferred_output_for_toplevel(surface) {
                    debug_assert_eq!(
                        owner, entry.output,
                        "fullscreen output must match mapped ownership"
                    );
                }
            }
        }
    }

    /// Toggles `surface` between tiled and floating. A window not tracked by
    /// `self.layout` is, by definition, floating (this is also what gates
    /// `move_request`/`resize_request` in `handlers/xdg_shell.rs`), so there's
    /// no separate floating-window set to keep in sync.
    ///
    /// Untiling keeps the window's current geometry (no jump); retiling
    /// snaps it into whatever slot the layout gives it, same as any other
    /// tiled window.
    pub fn toggle_floating(&mut self, surface: &WlSurface) {
        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            let was_tiled = self.ocean.is_tiled(surface);
            let changed = if was_tiled {
                self.ocean
                    .make_floating(surface, self.config.gaps, self.config.bsp_split_bias)
            } else {
                let Some(output) = self.primary_output() else {
                    return;
                };
                let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size) else {
                    return;
                };
                let focused = self.focused_window_surface();
                self.ocean.make_tiled(
                    surface,
                    &output.name(),
                    viewport,
                    focused.as_ref(),
                    self.config.gaps,
                    self.config.bsp_split_bias,
                )
            };
            if changed {
                self.buoyancy_dirty = true;
                if !was_tiled {
                    self.window_float_ambient.remove(surface);
                    self.window_currents.remove(surface);
                    self.floating_dragging.remove(surface);
                    self.window_buoyancy.remove(surface);
                }
                self.retile();
                if was_tiled {
                    self.enforce_floating_size_constraints(surface);
                    self.rescue_toggled_floating(surface);
                }
                self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
                    surface: surface.clone(),
                });
            }
            return;
        }
        let Some(window) = self
            .space
            .elements()
            .find(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == surface)
                    .unwrap_or(false)
            })
            .cloned()
        else {
            return;
        };

        let was_tiled = self.layout.contains(surface);
        if was_tiled {
            // `space` currently contains the fullscreen override geometry,
            // not the underlying tile geometry. If this is the first time a
            // tiled fullscreen window becomes floating, recover its normal
            // tile rect before removing it from the tree. Niri likewise
            // preserves separate windowed/floating geometry instead of
            // treating the fullscreen viewport as a restoration size.
            let normal_tile_rect = self.layout.output_of(surface).and_then(|output_name| {
                let workspace = self.layout.workspace_of(surface)?;
                let output = self.output_by_name(output_name)?;
                let area = self.output_tiling_area(&output)?;
                self.layout
                    .layout(
                        output_name,
                        workspace,
                        area,
                        self.gaps_for(output_name, workspace),
                    )
                    .into_iter()
                    .find_map(|(candidate, rect)| is_window(&candidate, surface).then_some(rect))
            });

            // It's tiled, so by construction it's on its output's *active*
            // workspace (a hidden workspace's windows are never mapped) --
            // tag it with that before removing it from the tree, since
            // `switch_workspace` needs this tag for a floating window (a
            // tiled one's workspace is implicit in tree membership instead).
            if let Some(output) = self.layout.output_of(surface).map(str::to_string) {
                let workspace = self.layout.active_workspace(&output);
                // Prefer an existing pre-fullscreen floating rect (it may be
                // surviving a floating -> tiled -> floating round trip), then
                // the underlying tile, and only finally the mapped geometry.
                // The final fallback can be fullscreen-sized, but is better
                // than losing the window entirely if output geometry vanished.
                let rect = self
                    .fullscreen
                    .get(surface)
                    .and_then(|entry| entry.restore_rect)
                    .or_else(|| {
                        (self.pseudo_tiled.contains(surface)
                            && !self.fullscreen.contains_key(surface))
                        .then(|| self.space.element_geometry(&window))
                        .flatten()
                    })
                    .or(normal_tile_rect)
                    .or_else(|| self.space.element_geometry(&window))
                    .unwrap_or_else(|| Rectangle::new(Point::default(), window.geometry().size));
                if let Some(entry) = self.fullscreen.get_mut(surface) {
                    entry.remember_restore_rect(rect);
                }
                self.floating_workspace.insert(
                    surface.clone(),
                    FloatingTag {
                        window: window.clone(),
                        output,
                        workspace,
                        rect,
                    },
                );
            }
            // Pseudo-tiling is a tiled placement mode, not a dormant flag to
            // report on a floating/pinned window or silently reactivate later.
            self.pseudo_tiled.remove(surface);
            self.layout.remove(surface);
            self.space.raise_element(&window, false);
        } else {
            // Tile on the output containing the floater, with the action
            // output as fallback when it is fully outside all outputs.
            let Some(output) = self
                .output_for_window(&window)
                .or_else(|| self.primary_output())
            else {
                return;
            };
            let focused = self.focused_window_surface();
            let workspace = self.layout.active_workspace(&output.name());
            // Pinning requires floating state. Explicitly unpin before the
            // inverse transition so `pinned => floating` remains true even
            // when toggle_floating is invoked directly on a pinned window.
            self.pinned.remove(surface);
            if let Some(entry) = self.fullscreen.get_mut(surface) {
                entry.was_pinned = false;
            }
            let was_maximized = self.maximized.remove(surface).is_some();
            crate::grabs::resize_grab::cancel(surface);
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    if was_maximized {
                        state.states.unset(xdg_toplevel::State::Maximized);
                    }
                    state.states.unset(xdg_toplevel::State::Resizing);
                });
            }
            self.layout
                .insert(&output.name(), workspace, window, focused.as_ref());
            // No longer floating, so no longer needs its own workspace tag.
            self.floating_workspace.remove(surface);
            self.window_float_ambient.remove(surface);
            self.window_currents.remove(surface);
            self.floating_dragging.remove(surface);
            self.window_buoyancy.remove(surface);
            self.buoyancy_dirty = true;
        }

        self.buoyancy_dirty = true;
        self.retile();
        if was_tiled {
            self.enforce_floating_size_constraints(surface);
            self.rescue_toggled_floating(surface);
        }
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
    }

    /// Converts a just-mapped tiled window on a *hidden* Classic workspace
    /// to floating. The interactive `toggle_floating` path cannot do this:
    /// it looks the window up in `space.elements()` (a hidden workspace's
    /// windows are never mapped) and tags the float with the output's
    /// *active* workspace rather than the tree it actually came from. The
    /// window handle comes from `Layouts`, the rect from the slot the tree
    /// would have given it, and nothing touches `Space` -- the window
    /// becomes visible through `apply_workspace_switch`'s ordinary
    /// incoming-floating map when its workspace is shown.
    pub(crate) fn float_hidden_tiled_window(
        &mut self,
        surface: &WlSurface,
        output_name: &str,
        workspace: u32,
    ) {
        let Some(window) = self.layout.window_of(surface) else {
            return;
        };
        let rect = self
            .output_by_name(output_name)
            .and_then(|output| self.output_tiling_area(&output))
            .and_then(|area| {
                self.layout
                    .layout(
                        output_name,
                        workspace,
                        area,
                        self.gaps_for(output_name, workspace),
                    )
                    .into_iter()
                    .find_map(|(candidate, rect)| is_window(&candidate, surface).then_some(rect))
            })
            .unwrap_or_else(|| Rectangle::new(Point::default(), window.geometry().size));
        self.pseudo_tiled.remove(surface);
        self.layout.remove(surface);
        self.floating_workspace.insert(
            surface.clone(),
            FloatingTag {
                window,
                output: output_name.to_string(),
                workspace,
                rect,
            },
        );
        self.buoyancy_dirty = true;
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
    }

    /// Toggles `surface` pseudo-tiled: stays in its `Layouts` slot (unlike
    /// floating), but `retile()` shrinks the rect it actually renders at
    /// to `config.pseudo_tile_scale` of the tile, centered within it,
    /// instead of filling it. No-op for a window that isn't tiled --
    /// pseudo-tiling only has meaning as a rect override on a real tile.
    pub fn toggle_pseudo_tile(&mut self, surface: &WlSurface) {
        if !self.layout.contains(surface) {
            return;
        }
        if !self.pseudo_tiled.remove(surface) {
            self.pseudo_tiled.insert(surface.clone());
        }
        self.retile();
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
    }

    /// Moves every window owned by `from_output` onto `to_output`, preserving
    /// workspace numbers. This runs while both outputs are still mapped, so
    /// floating restore rectangles can be translated between their global
    /// logical origins before the disconnected output disappears.
    ///
    /// A migrated workspace is not necessarily `to_output`'s active one. In
    /// that case its windows are explicitly unmapped here and remain
    /// reachable via `workspace:<N>`; leaving an element mapped at the dead
    /// output's old coordinates would make it visible on the wrong workspace
    /// and could leave keyboard focus pointing at unreachable content.
    pub(crate) fn migrate_output_windows(&mut self, from_output: &str, to_output: &str) {
        #[cfg(feature = "screencast")]
        if self
            .screencast_picker
            .as_ref()
            .is_some_and(|picker| picker.output_name() == from_output)
        {
            // The modal UI cannot remain usable after its output vanishes.
            // Dropping it sends a portal cancellation response.
            self.screencast_picker.take();
        }
        let from_geometry = self
            .output_by_name(from_output)
            .and_then(|output| self.space.output_geometry(&output));
        let to_output_handle = self.output_by_name(to_output);
        let to_geometry = to_output_handle
            .as_ref()
            .and_then(|output| self.space.output_geometry(output));
        let to_bounds = to_output_handle
            .as_ref()
            .and_then(|output| self.output_tiling_area(output))
            .or(to_geometry);
        let delta = match (from_geometry, to_geometry) {
            (Some(from), Some(to)) => Some(to.loc - from.loc),
            _ => None,
        };
        let active_workspace = self.layout.active_workspace(to_output);

        // Fullscreen is exclusive per output. If content from the removed
        // output is arriving, demote the destination's current owner before
        // retagging the source; otherwise debug builds trip the invariant and
        // release builds retain two contradictory protocol owners.
        let moving_fullscreen = self
            .fullscreen
            .iter()
            .find(|(_, entry)| entry.output == from_output)
            .map(|(surface, _)| surface.clone());
        if let Some(moving_surface) = moving_fullscreen.as_ref() {
            let destination_owner = self
                .fullscreen
                .iter()
                .find(|(surface, entry)| *surface != moving_surface && entry.output == to_output)
                .map(|(surface, _)| surface.clone());
            if let Some(surface) = destination_owner {
                let toplevel = self
                    .mapped_toplevel_window(&surface)
                    .and_then(|window| window.toplevel().cloned());
                if let Some(toplevel) = toplevel {
                    self.do_unfullscreen(&toplevel);
                } else {
                    self.fullscreen.remove(&surface);
                }
            }
        }
        if let Some(surface) = moving_fullscreen.as_ref() {
            if let Some(entry) = self.fullscreen.get_mut(surface) {
                entry.move_to_output(to_output.to_string(), delta);
                if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, to_bounds) {
                    *rect = clamp_rect_visible(*rect, bounds);
                }
            }
        }

        let moving_maximized: Vec<WlSurface> = self
            .maximized
            .iter()
            .filter(|(_, entry)| entry.output == from_output)
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in &moving_maximized {
            if let Some(entry) = self.maximized.get_mut(surface) {
                entry.move_to_output(to_output.to_string(), delta);
                if let Some(bounds) = to_bounds {
                    entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
                }
            }
        }

        // A deck entry has neither a Space placement nor a Layouts leaf.
        self.classic_depth.migrate_output(from_output, to_output);

        let workspaces: Vec<u32> = self
            .layout
            .populated_workspaces()
            .into_iter()
            .filter(|(name, _)| name == from_output)
            .map(|(_, workspace)| workspace)
            .collect();
        for workspace in workspaces {
            for window in self.layout.windows_in(from_output, workspace) {
                let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) else {
                    continue;
                };
                self.layout.remove(&surface);
                self.layout
                    .insert(to_output, workspace, window.clone(), None);

                if workspace == active_workspace {
                    if let Some(output) = &to_output_handle {
                        self.set_window_fractional_scale(&window, output);
                    }
                } else {
                    self.space.unmap_elem(&window);
                }
            }
        }

        // Parked group members need their durable ownership moved explicitly.
        for group in &mut self.groups {
            if group.output == from_output {
                group.output = to_output.to_string();
            }
        }

        let floating_surfaces: Vec<WlSurface> = self
            .floating_workspace
            .iter()
            .filter(|(_, tag)| tag.output == from_output)
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in floating_surfaces {
            let was_visible = self.window_is_visible(&surface);
            let live_rect = was_visible
                .then(|| {
                    let window = self.floating_workspace.get(&surface)?.window.clone();
                    self.space.element_geometry(&window)
                })
                .flatten();

            let (window, workspace, loc) = {
                let tag = self.floating_workspace.get_mut(&surface).unwrap();
                if !self.fullscreen.contains_key(&surface) && !self.maximized.contains_key(&surface)
                {
                    if let Some(rect) = live_rect {
                        tag.rect = rect;
                    }
                }
                tag.output = to_output.to_string();
                if let Some(delta) = delta {
                    tag.rect.loc += delta;
                }
                if let Some(bounds) = to_bounds {
                    tag.rect = clamp_rect_visible(tag.rect, bounds);
                }
                (tag.window.clone(), tag.workspace, tag.rect.loc)
            };

            if self.pinned.contains(&surface) || workspace == active_workspace {
                self.space.map_element(window.clone(), loc, false);
                if let Some(output) = &to_output_handle {
                    self.set_window_fractional_scale(&window, output);
                }
            } else {
                self.space.unmap_elem(&window);
            }
        }

        self.scratchpad_previous.remove(from_output);
        self.workspace_previous.remove(from_output);

        // Retile reconciles Classic placements. Ocean's camera renderer also
        // needs the client-side protocol size updated immediately because its
        // visual fit can otherwise hide the stale buffer size after hotplug.
        if let (Some(surface), Some(output_geo)) = (moving_fullscreen, to_geometry) {
            if let Some(toplevel) = self
                .mapped_toplevel_window(&surface)
                .and_then(|window| window.toplevel().cloned())
            {
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.states.unset(xdg_toplevel::State::Maximized);
                    state.states.unset(xdg_toplevel::State::Resizing);
                    state.size = Some(output_geo.size);
                });
                toplevel.send_pending_configure();
            }
        }
        if let Some(area) = to_bounds {
            let gaps = if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
                self.config.gaps
            } else {
                self.gaps_for(to_output, active_workspace)
            };
            let rect = crate::layout::inset(area, gaps);
            for surface in moving_maximized {
                if self.fullscreen.contains_key(&surface) {
                    continue;
                }
                if let Some(toplevel) = self
                    .mapped_toplevel_window(&surface)
                    .and_then(|window| window.toplevel().cloned())
                {
                    toplevel.with_pending_state(|state| {
                        state.states.set(xdg_toplevel::State::Maximized);
                        state.states.unset(xdg_toplevel::State::Fullscreen);
                        state.states.unset(xdg_toplevel::State::Resizing);
                        state.size = Some(rect.size);
                    });
                    toplevel.send_pending_configure();
                }
            }
        }
    }

    /// Completes a cross-monitor tile drop onto the destination's visible
    /// tree. Group tabs travel together; pointer callbacks supply the target
    /// and never need to lock/query the pointer again during release.
    pub(crate) fn move_tiled_window_to_output(
        &mut self,
        surface: &WlSurface,
        to: &Output,
        target: Option<&WlSurface>,
    ) -> bool {
        if !self.window_is_visible(surface)
            || self.fullscreen.contains_key(surface)
            || self.maximized.contains_key(surface)
        {
            return false;
        }
        let Some(geometry) = self.space.output_geometry(to) else {
            return false;
        };
        let name = to.name();
        let workspace = self.layout.active_workspace(&name);
        let window = match self.config.spatial_engine {
            crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll => {
                let Some(window) = self.layout.window_of(surface) else {
                    return false;
                };
                let target = target.filter(|target| {
                    self.layout.output_of(target) == Some(name.as_str())
                        && self.layout.workspace_of(target) == Some(workspace)
                });
                self.layout.remove(surface);
                self.layout.insert(&name, workspace, window.clone(), target);
                window
            }
            crate::config::SpatialEngine::Ocean => {
                let Some(window) =
                    self.ocean
                        .move_tiled_to_output(surface, &name, geometry.size, target)
                else {
                    return false;
                };
                window
            }
        };
        let members = if let Some(index) = self.group_of(surface) {
            let group = &mut self.groups[index];
            group.output = name.clone();
            group.workspace = workspace;
            group.strip = None;
            group
                .members
                .iter()
                .map(|member| (member.surface.clone(), member.parked_window.clone()))
                .collect::<Vec<_>>()
        } else {
            vec![(surface.clone(), None)]
        };
        self.set_window_fractional_scale(&window, to);
        for (member, parked) in members {
            if let Some(pin) = self.output_pins.get_mut(&member) {
                *pin = name.clone();
            }
            if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
                self.ocean.set_entry_output(&member, name.clone());
            }
            if let Some(parked) = parked {
                self.set_window_fractional_scale(&parked, to);
            }
            self.refresh_window_opacity_and_glass_for(&member);
            self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged { surface: member });
        }
        self.refresh_ext_workspaces();
        true
    }

    /// Moves exactly `surface` to `to`, wherever it currently lives --
    /// the single-surface counterpart to `migrate_output_windows`'s bulk
    /// per-output move, reusing the same idioms (tiled: `Layouts::remove`
    /// and `insert`, letting the caller's `retile()` reconcile Space the same
    /// way `move_to_workspace` does; floating: retag + translate by the
    /// output-origin delta + explicit map/unmap; fullscreen/maximized:
    /// `move_to_output` plus the same per-output exclusivity demotion
    /// `migrate_output_windows`/`swap_workspaces` already take). Used by
    /// `recall_pinned_windows_to`. No-op if `surface` isn't currently tiled
    /// or floating anywhere, or is already on `to`. Acknowledged gap: unlike
    /// `migrate_output_windows`, this doesn't re-send a fullscreen client's
    /// configure at the destination output's exact size -- a recalled
    /// fullscreen-pinned window keeps its old buffer size until some later
    /// event (focus change, an explicit fullscreen toggle) triggers a
    /// resize. Narrow enough (pinned *and* fullscreen *and* mid-recall) not
    /// to be worth the extra plumbing here; revisit if it's ever hit for
    /// real.
    fn move_window_to_output(&mut self, surface: &WlSurface, to: &Output) {
        let to_name = to.name();
        let from_name = self
            .layout
            .output_of(surface)
            .map(str::to_string)
            .or_else(|| {
                self.floating_workspace
                    .get(surface)
                    .map(|tag| tag.output.clone())
            });
        let Some(from_name) = from_name else {
            return;
        };
        if from_name == to_name {
            return;
        }
        let from_geometry = self
            .output_by_name(&from_name)
            .and_then(|output| self.space.output_geometry(&output));
        let to_geometry = self.space.output_geometry(to);
        let to_bounds = self.output_tiling_area(to).or(to_geometry);
        let delta = match (from_geometry, to_geometry) {
            (Some(from), Some(geo)) => Some(geo.loc - from.loc),
            _ => None,
        };

        // Fullscreen is exclusive per output -- demote whatever's already
        // fullscreen on the destination first, same precaution
        // `migrate_output_windows`/`swap_workspaces` take.
        if self.fullscreen.contains_key(surface) {
            let destination_owner = self
                .fullscreen
                .iter()
                .find(|(other, entry)| *other != surface && entry.output == to_name)
                .map(|(other, _)| other.clone());
            if let Some(owner) = destination_owner {
                let toplevel = self
                    .mapped_toplevel_window(&owner)
                    .and_then(|window| window.toplevel().cloned());
                if let Some(toplevel) = toplevel {
                    self.do_unfullscreen(&toplevel);
                } else {
                    self.fullscreen.remove(&owner);
                }
            }
        }
        if let Some(entry) = self.fullscreen.get_mut(surface) {
            entry.move_to_output(to_name.clone(), delta);
            if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, to_bounds) {
                *rect = clamp_rect_visible(*rect, bounds);
            }
        }
        if let Some(entry) = self.maximized.get_mut(surface) {
            entry.move_to_output(to_name.clone(), delta);
            if let Some(bounds) = to_bounds {
                entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
            }
        }

        if let Some(window) = self.layout.window_of(surface) {
            let workspace = self
                .layout
                .workspace_of(surface)
                .unwrap_or_else(|| self.layout.active_workspace(&from_name));
            self.layout.remove(surface);
            self.layout
                .insert(&to_name, workspace, window.clone(), None);
            if workspace == self.layout.active_workspace(&to_name) {
                self.set_window_fractional_scale(&window, to);
            } else {
                self.space.unmap_elem(&window);
            }
        } else if self.floating_workspace.contains_key(surface) {
            let was_visible = self.window_is_visible(surface);
            let live_rect = was_visible
                .then(|| {
                    let window = self.floating_workspace.get(surface)?.window.clone();
                    self.space.element_geometry(&window)
                })
                .flatten();
            let (window, workspace, loc) = {
                let tag = self.floating_workspace.get_mut(surface).unwrap();
                if !self.fullscreen.contains_key(surface) && !self.maximized.contains_key(surface) {
                    if let Some(rect) = live_rect {
                        tag.rect = rect;
                    }
                }
                tag.output = to_name.clone();
                if let Some(delta) = delta {
                    tag.rect.loc += delta;
                }
                if let Some(bounds) = to_bounds {
                    tag.rect = clamp_rect_visible(tag.rect, bounds);
                }
                (tag.window.clone(), tag.workspace, tag.rect.loc)
            };
            if self.pinned.contains(surface) || workspace == self.layout.active_workspace(&to_name)
            {
                self.space.map_element(window.clone(), loc, false);
                self.set_window_fractional_scale(&window, to);
            } else {
                self.space.unmap_elem(&window);
            }
        }
    }

    /// Adopt durable ownership left behind by a zero-output interval. A
    /// connector may return under a different kernel name (for example after
    /// moving the cable), so waiting for the old name would strand the whole
    /// Classic session indefinitely.
    pub(crate) fn adopt_orphaned_output_windows(&mut self, to_output: &str) {
        let live_outputs: HashSet<String> = self.space.outputs().map(Output::name).collect();
        let mut owned_outputs: HashSet<String> = self
            .layout
            .populated_workspaces()
            .into_iter()
            .map(|(output, _)| output)
            .collect();
        owned_outputs.extend(
            self.floating_workspace
                .values()
                .map(|tag| tag.output.clone()),
        );
        owned_outputs.extend(self.classic_depth.output_names().map(str::to_string));
        owned_outputs.extend(self.fullscreen.values().map(|entry| entry.output.clone()));
        owned_outputs.extend(self.maximized.values().map(|entry| entry.output.clone()));
        owned_outputs.extend(self.groups.iter().map(|group| group.output.clone()));

        let mut orphaned: Vec<String> = owned_outputs
            .into_iter()
            .filter(|output| output != to_output && !live_outputs.contains(output))
            .collect();
        orphaned.sort();
        for output in orphaned {
            tracing::info!(
                from = output,
                to = to_output,
                "Adopting orphaned output ownership"
            );
            self.migrate_output_windows(&output, to_output);
        }
    }

    // -- Spatial-engine migration (S6) -----------------------------------------

    /// Translates live ownership between Classic workspaces and Ocean reefs.
    /// Shared window/protocol tags survive; pins change representation. State
    /// with no counterpart is dropped after restoring any parked windows.
    pub(crate) fn migrate_spatial_engine(&mut self, target: crate::config::SpatialEngine) -> bool {
        if target == self.config.spatial_engine {
            return true;
        }
        if self.activation_blocked()
            || self.classic_tile_drag.is_some()
            || self.compositor_gesture.is_some()
            || self
                .seat
                .get_pointer()
                .is_some_and(|pointer| pointer.is_grabbed())
        {
            tracing::warn!(
                ?target,
                "cannot change spatial engine during lock or an active grab"
            );
            return false;
        }
        if self
            .space
            .outputs()
            .all(|output| self.space.output_geometry(output).is_none())
        {
            tracing::warn!(
                ?target,
                "cannot migrate spatial engine without a live output"
            );
            return false;
        }
        tracing::info!(
            from = ?self.config.spatial_engine,
            to = ?target,
            "migrating spatial engine"
        );
        // `Space` is a render/input cache, not the ownership model. Clear
        // every old-engine placement first so the new engine can rebuild only
        // what should be visible; Classic retile intentionally does not walk
        // inactive workspaces to unmap stale Ocean placements.
        let mapped: Vec<Window> = self.space.elements().cloned().collect();
        // `rule { on_workspace }` is a live match criterion feeding cached
        // opacity/glass, and this migration is the one place a window's
        // workspace-number-or-lack-thereof changes wholesale (Classic
        // numbers vs. Ocean's no-workspace-concept) -- re-resolve everything
        // that was visible going in once the migration lands below.
        let migrated_surfaces: Vec<WlSurface> = mapped
            .iter()
            .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone()))
            .collect();
        for window in mapped {
            self.space.unmap_elem(&window);
        }
        self.refresh_space();
        match (self.config.spatial_engine, target) {
            (crate::config::SpatialEngine::Ocean, _) => self.migrate_ocean_to_classic(),
            (_, crate::config::SpatialEngine::Ocean) => self.migrate_classic_to_ocean(),
            _ => {} // Classic and Scroll share durable workspace membership.
        }
        self.config.spatial_engine = target;
        self.pending_workspace_transitions.clear();
        self.workspace_transitions.clear();
        self.workspace_glides.clear();
        self.swim_cameras.clear();
        // Rebuild Space as the new engine's render/input cache.
        self.retile_with_viscosity(false, None);
        // Classic retile maps trees only; restore visible migrated floaters.
        if target.uses_workspaces() {
            let floating: Vec<(WlSurface, Rectangle<i32, Logical>, Window)> = self
                .floating_workspace
                .iter()
                .map(|(surface, tag)| (surface.clone(), tag.rect, tag.window.clone()))
                .collect();
            for (surface, rect, window) in floating {
                let Some(tag) = self.floating_workspace.get(&surface) else {
                    continue;
                };
                if self.pinned.contains(&surface)
                    || self.layout.active_workspace(&tag.output) == tag.workspace
                {
                    self.space.map_element(window.clone(), rect.loc, false);
                    if let Some(output) = self.output_by_name(&tag.output) {
                        self.set_window_fractional_scale(&window, &output);
                    }
                }
            }
        }
        for surface in &migrated_surfaces {
            self.refresh_window_opacity_and_glass_for(surface);
        }
        if let Some(focus) = self.window_focus.clone() {
            self.reveal_scroll_window(&focus);
        }
        self.sync_tide();
        self.refresh_ext_workspaces();
        self.emit_ipc_event(crate::ipc::IpcEvent::ConfigReloaded);
        self.request_redraw();
        true
    }

    /// Classic -> Ocean. Every output gets a disjoint world island and each
    /// populated workspace becomes a reef on that island's lateral line. The
    /// camera points at the previously-active workspace's reef, and floating
    /// windows translate from output-global to reef-local world coordinates.
    fn migrate_classic_to_ocean(&mut self) {
        // Depth-deck windows have no Ocean counterpart: recall them to
        // their workspace tiles first so the tree move carries them. The
        // deck picker overlay is Classic UI and must not linger.
        for entry in self.classic_depth.drain() {
            self.layout
                .insert(&entry.output, entry.workspace, entry.window.clone(), None);
        }
        self.depth_deck_overlay = None;
        self.classic_depth.close();

        let output_viewports: Vec<(String, Rectangle<i32, Logical>)> = self
            .space
            .outputs()
            .filter_map(|output| Some((output.name(), self.space.output_geometry(output)?)))
            .collect();

        // Snapshot the active workspace per output before draining.
        let active: std::collections::HashMap<String, u32> = output_viewports
            .iter()
            .map(|(name, _)| (name.clone(), self.layout.active_workspace(name)))
            .collect();

        // Drain the trees first (each is unique per (output, workspace)).
        let mut trees = self.layout.drain_for_migration();
        let floating: Vec<(WlSurface, FloatingTag)> = self.floating_workspace.drain().collect();
        let reef_gap = self.config.gaps.max(0);

        // Assign disjoint islands before placing per-output reef sequences.
        let mut max_workspaces = std::collections::HashMap::new();
        for (output_name, _) in &output_viewports {
            let max_workspace = trees
                .iter()
                .filter(|(name, _, _)| name == output_name)
                .map(|(_, workspace, _)| *workspace)
                .chain(
                    floating
                        .iter()
                        .filter(|(_, tag)| &tag.output == output_name)
                        .map(|(_, tag)| tag.workspace),
                )
                .chain(active.get(output_name).copied())
                .max()
                .unwrap_or(1);
            max_workspaces.insert(output_name.clone(), max_workspace);
        }
        let island_x = ocean_island_origins(&output_viewports, &max_workspaces, reef_gap);

        // Clear Classic-only pseudo-tile state; groups and pin semantics migrate.
        self.pseudo_tiled.clear();

        for (output_name, geometry) in &output_viewports {
            let base_x = island_x.get(output_name).copied().unwrap_or(0);
            let viewport = geometry.size;
            // Move this output's trees into reefs, workspace N at
            // X = (N-1) * stride on the lateral line.
            let mut i = 0;
            while i < trees.len() {
                if trees[i].0 != *output_name {
                    i += 1;
                    continue;
                }
                let (_, workspace, tree) = trees.remove(i);
                let reef_x = ocean_reef_x(base_x, workspace, viewport.w, reef_gap);
                let reef_rect = Rectangle::new(Point::from((reef_x, 0)), viewport);
                let reef_name = self
                    .config
                    .workspace_names
                    .iter()
                    .find_map(|(name, number)| (*number == workspace).then(|| name.clone()))
                    .unwrap_or_else(|| format!("ws-{workspace}"));
                self.ocean
                    .push_migrated_reef(reef_name, reef_rect, tree, output_name);
            }
            // Point the camera at the previously-active workspace's reef.
            let active_ws = active.get(output_name).copied().unwrap_or(1);
            let cam_x = f64::from(base_x)
                + (active_ws as f64 - 1.0) * (viewport.w as f64 + f64::from(reef_gap));
            self.ocean
                .set_camera_origin(output_name, crate::ocean::OceanPoint { x: cam_x, y: 0.0 });
        }

        // Floating windows translate to world coordinates around their
        // workspace's reef; pinned windows additionally get an Ocean
        // screen pin so the pin semantic survives the crossing. The
        // cameras were set above, which `pin_to_screen` needs to compute
        // its viewport anchor.
        for (surface, tag) in floating {
            let output = output_viewports
                .iter()
                .find(|(name, _)| name == &tag.output);
            let Some((output_name, output_geometry)) = output else {
                // Preserve real geometry instead of guessing another
                // output's dimensions if this output disappeared.
                self.ocean
                    .push_migrated_floating(surface, tag.window, tag.rect, &tag.output);
                continue;
            };
            let base_x = island_x.get(output_name).copied().unwrap_or(0);
            let reef_x = ocean_reef_x(base_x, tag.workspace, output_geometry.size.w, reef_gap);
            let reef_rect = Rectangle::new((reef_x, 0).into(), output_geometry.size);
            let world_rect = classic_rect_to_ocean(tag.rect, *output_geometry, reef_rect);
            let was_pinned = self.pinned.contains(&surface);
            self.ocean
                .push_migrated_floating(surface.clone(), tag.window, world_rect, output_name);
            if was_pinned {
                self.ocean.pin_to_screen(&surface, output_name);
            }
        }
        // `pinned` is a Classic-only set; Ocean represents pin status via
        // screen pins. Clear it so the debug-build invariant (pinned
        // windows stay in `floating_workspace`) holds under Ocean, where
        // that map is drained.
        self.pinned.clear();

        // Workspace-switch bookkeeping is Classic-only.
        self.scratchpad_previous.clear();
        self.workspace_previous.clear();
    }

    /// Ocean -> Classic. Reefs sorted left-to-right become workspaces 1..N
    /// on their owner in independent mode, or the nearest camera in shared
    /// mode. The active workspace follows each camera. Floating windows
    /// keep their connected output and select its nearest reef; screen pins
    /// keep their viewport rectangle and re-enter the Classic `pinned` set.
    fn migrate_ocean_to_classic(&mut self) {
        let (reefs, floating, cameras, entry_outputs, pinned_surfaces) =
            self.ocean.drain_for_classic();

        let output_viewports: std::collections::HashMap<String, Rectangle<i32, Logical>> = self
            .space
            .outputs()
            .filter_map(|output| Some((output.name(), self.space.output_geometry(output)?)))
            .collect();
        let output_names: Vec<String> = self
            .space
            .outputs()
            .filter(|output| output_viewports.contains_key(&output.name()))
            .map(|output| output.name())
            .collect();

        // Sort reefs left-to-right so workspace numbering is stable.
        let mut sorted_reefs = reefs;
        sorted_reefs.sort_by_key(|(_, rect, _)| rect.loc.x);

        let camera_center = |output: &str| {
            let camera = cameras.get(output).copied().unwrap_or_default();
            let viewport = output_viewports
                .get(output)
                .map(|rect| rect.size)
                .unwrap_or_default();
            crate::ocean::OceanPoint {
                x: camera.origin.x + viewport.w as f64 / camera.zoom.max(0.05) / 2.0,
                y: camera.origin.y + viewport.h as f64 / camera.zoom.max(0.05) / 2.0,
            }
        };

        // Nearest-output helper for one reef. Ocean is two-dimensional:
        // vertically separated cameras must not tie solely on X.
        let nearest_output = |rect: &Rectangle<i32, Logical>| -> String {
            let center_x = rect.loc.x as f64 + rect.size.w as f64 / 2.0;
            let center_y = rect.loc.y as f64 + rect.size.h as f64 / 2.0;
            output_names
                .iter()
                .min_by(|a, b| {
                    let cam_a = camera_center(a);
                    let cam_b = camera_center(b);
                    let dist_a = (cam_a.x - center_x).powi(2) + (cam_a.y - center_y).powi(2);
                    let dist_b = (cam_b.x - center_x).powi(2) + (cam_b.y - center_y).powi(2);
                    dist_a.total_cmp(&dist_b)
                })
                .cloned()
                .unwrap_or_default()
        };

        // Workspace counter per output, the workspace number each output's
        // camera is currently looking at (for active selection), and the
        // reef rects assigned so floating windows can find their nearest
        // workspace afterward.
        let mut ws_counters: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        let mut nearest_ws: std::collections::HashMap<String, (f64, u32)> =
            std::collections::HashMap::new();
        let mut reef_slots: Vec<(String, u32, Rectangle<i32, Logical>)> = Vec::new();

        for (_, rect, tree) in sorted_reefs {
            let target_output = (!self.config.ocean.shared_canvas)
                .then(|| {
                    tree.windows().iter().find_map(|window| {
                        entry_outputs
                            .get(window.toplevel()?.wl_surface())
                            .filter(|name| output_viewports.contains_key(*name))
                            .cloned()
                    })
                })
                .flatten()
                .unwrap_or_else(|| nearest_output(&rect));
            let ws = ws_counters.entry(target_output.clone()).or_insert(1);
            self.layout
                .insert_migrated_tree(target_output.clone(), *ws, tree);
            let center_x = rect.loc.x as f64 + rect.size.w as f64 / 2.0;
            let center_y = rect.loc.y as f64 + rect.size.h as f64 / 2.0;
            let camera = camera_center(&target_output);
            let dist = (camera.x - center_x).powi(2) + (camera.y - center_y).powi(2);
            match nearest_ws.get(&target_output) {
                Some((best, _)) if *best <= dist => {}
                _ => {
                    nearest_ws.insert(target_output.clone(), (dist, *ws));
                }
            }
            reef_slots.push((target_output, *ws, rect));
            *ws += 1;
        }

        // Set active workspaces from the cameras.
        for output_name in &output_names {
            let active = nearest_ws.get(output_name).map(|(_, ws)| *ws).unwrap_or(1);
            self.layout.set_active_workspace(output_name, active);
        }

        // Restrict nearest-reef selection to the floater's connected output.
        // An output with only floaters must not lose them to another's reef.
        for (surface, window, world_rect) in floating {
            let preferred_output = pinned_surfaces
                .get(&surface)
                .map(|(output, _)| output)
                .or_else(|| entry_outputs.get(&surface))
                .filter(|output| output_viewports.contains_key(*output));
            let nearest_slot = ocean_classic_slot(
                world_rect,
                &reef_slots,
                preferred_output.map(String::as_str),
            );
            let (target_output, ws, reef_rect) = if let Some((output, ws, rect)) = nearest_slot {
                (output.clone(), *ws, *rect)
            } else {
                let output = preferred_output
                    .cloned()
                    .unwrap_or_else(|| nearest_output(&world_rect));
                let Some(viewport) = output_viewports.get(&output).copied() else {
                    tracing::warn!(%output, "preserving floating world geometry without output geometry");
                    let output = entry_outputs.get(&surface).cloned().unwrap_or(output);
                    self.floating_workspace.insert(
                        surface,
                        FloatingTag {
                            window,
                            output,
                            workspace: 1,
                            rect: world_rect,
                        },
                    );
                    continue;
                };
                let camera = cameras.get(&output).copied().unwrap_or_default();
                (
                    output,
                    1,
                    Rectangle::new(
                        (camera.origin.x as i32, camera.origin.y as i32).into(),
                        viewport.size,
                    ),
                )
            };
            let Some(output_geometry) = output_viewports.get(&target_output).copied() else {
                tracing::warn!(%target_output, "preserving floating world geometry without output geometry");
                let output = entry_outputs
                    .get(&surface)
                    .cloned()
                    .unwrap_or(target_output);
                self.floating_workspace.insert(
                    surface,
                    FloatingTag {
                        window,
                        output,
                        workspace: ws,
                        rect: world_rect,
                    },
                );
                continue;
            };
            let local_rect = if let Some((_, pin_rect)) = pinned_surfaces.get(&surface) {
                ocean_rect_to_classic(
                    *pin_rect,
                    Rectangle::new(Point::default(), output_geometry.size),
                    output_geometry,
                )
            } else {
                ocean_rect_to_classic(world_rect, reef_rect, output_geometry)
            };
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(local_rect.size));
                toplevel.send_pending_configure();
            }
            self.floating_workspace.insert(
                surface,
                FloatingTag {
                    window,
                    output: target_output.clone(),
                    workspace: ws,
                    rect: local_rect,
                },
            );
        }

        // Screen-pinned windows re-enter the Classic `pinned` set (their
        // floating entries were created just above; Classic's invariant is
        // that pinned windows stay floating, which holds).
        for surface in pinned_surfaces.into_keys() {
            if self.floating_workspace.contains_key(&surface) {
                self.pinned.insert(surface);
            }
        }

        // Ocean groups identify their reef through the active leaf rather
        // than a Classic workspace key. Once reefs have been assigned to
        // Classic trees, refresh the cached owner used for activation,
        // accessibility, and later ungroup operations.
        for group in &mut self.groups {
            let active_surface = &group.members[group.active].surface;
            if let (Some(output), Some(workspace)) = (
                self.layout.output_of(active_surface),
                self.layout.workspace_of(active_surface),
            ) {
                group.output = output.to_string();
                group.workspace = workspace;
            }
        }

        // Remaining Ocean-only entry-output hints have no Classic counterpart.
    }

    /// Tiling work area expressed in the active engine's placement coordinates.
    fn floating_placement_area(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let area = self.output_tiling_area(output)?;
        if self.config.spatial_engine != crate::config::SpatialEngine::Ocean {
            return Some(area);
        }
        let geometry = self.space.output_geometry(output)?;
        let camera = self.ocean.camera(&output.name());
        let zoom = camera.zoom.max(0.05);
        Some(Rectangle::new(
            (
                (camera.origin.x + f64::from(area.loc.x - geometry.loc.x) / zoom).round() as i32,
                (camera.origin.y + f64::from(area.loc.y - geometry.loc.y) / zoom).round() as i32,
            )
                .into(),
            (
                (f64::from(area.size.w) / zoom).round().max(1.0) as i32,
                (f64::from(area.size.h) / zoom).round().max(1.0) as i32,
            )
                .into(),
        ))
    }

    /// New floats are placed in visible output coordinates, never at their
    /// transient tile/Scroll-strip location. Interactive toggles do not use
    /// this centering policy.
    pub(crate) fn place_initial_floating(
        &mut self,
        surface: &WlSurface,
        output: &Output,
        workspace: u32,
        rule: &crate::config::WindowRule,
        size: Option<(i32, i32)>,
    ) {
        let Some(bounds) = self.floating_placement_area(output) else {
            return;
        };
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        let constraints = self.floating_size_constraints_with_rule(surface, rule);
        // Keep explicit geometry exact wherever it fits the work area. Only
        // automatic natural-size placement reserves the usual workspace gap.
        let area = if rule.position.is_some() || rule.size.is_some() {
            bounds
        } else {
            crate::layout::inset(bounds, self.gaps_for(&output.name(), workspace))
        };
        let rect = initial_floating_rect(
            area,
            rule.position.map(Point::from),
            size.map(Size::from).unwrap_or(window.geometry().size),
            constraints,
        );
        self.apply_floating_placement(
            surface,
            Some((rect.loc.x, rect.loc.y)),
            Some((rect.size.w, rect.size.h)),
        );
    }

    /// Rescue only an off-screen interactive untile. A visible window keeps
    /// its existing location and size.
    fn rescue_toggled_floating(&mut self, surface: &WlSurface) {
        if self.fullscreen.contains_key(surface) || self.maximized.contains_key(surface) {
            return;
        }
        let owner = self
            .floating_workspace
            .get(surface)
            .map(|tag| tag.output.as_str())
            .or_else(|| self.ocean.entry_output(surface));
        let Some(output) = owner.and_then(|name| self.output_by_name(name)) else {
            return;
        };
        let Some(bounds) = self.floating_placement_area(&output) else {
            return;
        };
        let current = self
            .ocean
            .floating_rect(surface)
            .or_else(|| self.floating_workspace.get(surface).map(|tag| tag.rect));
        if let Some(current) = current {
            // A tile entirely within the output must not jump on toggle.
            let rect = fit_floating_rect_visible(current, bounds);
            if rect != current {
                self.apply_floating_placement(
                    surface,
                    Some((rect.loc.x, rect.loc.y)),
                    Some((rect.size.w, rect.size.h)),
                );
            }
        }
    }

    /// Applies a `[[rule]]`-specified exact position/size to a floating
    /// window, either of which may be unset (keep whatever `toggle_floating`
    /// already placed it at for that axis). No-op if `surface` isn't
    /// actually floating -- position/size only mean something once it is,
    /// same restriction `pseudo_tile` has in reverse for tiled. Mirrors the
    /// same configure+`map_element` shape `retile()`'s own tiled-window loop
    /// and the interactive resize/move grabs already use, not a new pattern.
    pub(crate) fn apply_floating_placement(
        &mut self,
        surface: &WlSurface,
        position: Option<(i32, i32)>,
        size: Option<(i32, i32)>,
    ) {
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            let Some(current) = self.ocean.floating_rect(surface) else {
                return;
            };
            let rect = Rectangle::new(
                position.map(Point::from).unwrap_or(current.loc),
                self.floating_size_constraints_for(surface)
                    .clamp(size.map(Size::from).unwrap_or(current.size)),
            );
            let pending = window
                .toplevel()
                .and_then(|toplevel| toplevel.with_pending_state(|state| state.size));
            if rect.loc == current.loc
                && !crate::window_size::needs_size_configure(current.size, rect.size, pending)
            {
                return;
            }
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(rect.size));
                toplevel.send_pending_configure();
            }
            self.ocean.set_floating_rect(surface, rect);
            self.space.map_element(window, rect.loc, false);
            self.request_redraw();
            return;
        }
        if !self.floating_workspace.contains_key(surface) {
            return;
        }
        // A floating window on a hidden workspace isn't in `Space`; its
        // durable tag rect is the only geometry it has.
        let Some(current) = self
            .space
            .element_geometry(&window)
            .or_else(|| self.floating_workspace.get(surface).map(|tag| tag.rect))
        else {
            return;
        };
        let loc = position.map(Point::from).unwrap_or(current.loc);
        let size = self
            .floating_size_constraints_for(surface)
            .clamp(size.map(Size::from).unwrap_or(current.size));
        let rect = Rectangle::new(loc, size);
        let pending = window
            .toplevel()
            .and_then(|toplevel| toplevel.with_pending_state(|state| state.size));
        if rect.loc == current.loc
            && !crate::window_size::needs_size_configure(current.size, rect.size, pending)
        {
            return;
        }

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = Some(rect.size);
            });
            toplevel.send_pending_configure();
        }
        if self.window_is_visible(surface) {
            self.space.map_element(window, rect.loc, false);
        }
        if let Some(tag) = self.floating_workspace.get_mut(surface) {
            tag.rect = rect;
        }
        self.request_redraw();
    }

    fn spatial_is_tiled(&self, surface: &WlSurface) -> bool {
        match self.config.spatial_engine {
            crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll => {
                self.layout.contains(surface)
            }
            crate::config::SpatialEngine::Ocean => self.ocean.is_tiled(surface),
        }
    }

    fn spatial_tiled_window(&self, surface: &WlSurface) -> Option<Window> {
        match self.config.spatial_engine {
            crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll => {
                self.layout.window_of(surface)
            }
            crate::config::SpatialEngine::Ocean => self.ocean.window(surface),
        }
    }

    fn spatial_tiles_share_owner(&self, first: &WlSurface, second: &WlSurface) -> bool {
        match self.config.spatial_engine {
            crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll => {
                self.layout.output_of(first) == self.layout.output_of(second)
                    && self.layout.workspace_of(first) == self.layout.workspace_of(second)
            }
            crate::config::SpatialEngine::Ocean => self.ocean.tiled_same_reef(first, second),
        }
    }

    fn detach_tiled_group_member(&mut self, surface: &WlSurface) -> Option<Window> {
        match self.config.spatial_engine {
            crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll => {
                let window = self.layout.window_of(surface)?;
                self.layout.remove(surface);
                Some(window)
            }
            crate::config::SpatialEngine::Ocean => self.ocean.detach_tiled_for_group(surface),
        }
    }

    fn replace_tiled_group_leaf(&mut self, old: &WlSurface, new_window: &Window) -> bool {
        match self.config.spatial_engine {
            crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll => {
                if !self.layout.contains(old) {
                    return false;
                }
                self.layout.replace_leaf(old, new_window);
                true
            }
            crate::config::SpatialEngine::Ocean => {
                self.ocean.replace_tiled_group_leaf(old, new_window)
            }
        }
    }

    /// Which group (if any) `surface` is a member of, active or parked.
    pub fn group_of(&self, surface: &WlSurface) -> Option<usize> {
        self.groups
            .iter()
            .position(|g| g.members.iter().any(|m| &m.surface == surface))
    }

    fn allocate_ui_node_id(&mut self) -> u64 {
        let id = self.next_ui_node_id;
        self.next_ui_node_id = self.next_ui_node_id.wrapping_add(1).max(1_000);
        id
    }

    /// Groups the focused tiled window with its neighbor in `direction` --
    /// i3/sway's "tabbed container" idea: both end up sharing one
    /// Classic-tree or Ocean-reef leaf, cycled via `cycle_tab`. Reuses the same
    /// neighbor lookup `swap_direction` already does, and only ever finds a
    /// *tiled* neighbor (a floating window nearest in that direction isn't
    /// a valid group target, same restriction `swap_direction` applies).
    /// Fullscreen and pseudo-tiled windows are excluded from grouping
    /// entirely for now -- a deliberate v1 scope-out, not an oversight:
    /// both are rect overrides on a tile's rendered size, and neither
    /// interaction with a shared tab slot has been worked out yet.
    pub fn group_direction(&mut self, direction: Direction) {
        let Some(focused) = self.focused_window_surface() else {
            return;
        };
        if !self.spatial_is_tiled(&focused)
            || self.fullscreen.contains_key(&focused)
            || self.pseudo_tiled.contains(&focused)
        {
            return;
        }
        let Some(current) = self
            .space
            .elements()
            .find(|w| is_window(w, &focused))
            .cloned()
        else {
            return;
        };
        let Some(neighbor) = self.neighbor_in_direction(&current, direction) else {
            return;
        };
        let Some(neighbor_surface) = neighbor.toplevel().map(|t| t.wl_surface().clone()) else {
            return;
        };
        if !self.spatial_is_tiled(&neighbor_surface)
            || self.fullscreen.contains_key(&neighbor_surface)
            || self.pseudo_tiled.contains(&neighbor_surface)
        {
            return;
        }
        self.group_with(&focused, &neighbor_surface);
    }

    /// Merges `b` into `a`'s tiled slot as a new parked tab. Both must
    /// already be tiled in the same spatial owner; `b`'s former leaf
    /// collapses exactly like a normal close, since the BSP removal
    /// removes it from the tree the same way either way. Merging two
    /// windows that are *both* already in (different) groups is out of
    /// scope for now -- no-op rather than picking a side to discard.
    pub fn group_with(&mut self, a: &WlSurface, b: &WlSurface) {
        if self.group_of(b).is_some() {
            tracing::debug!("group-with: target is itself already grouped, skipping");
            return;
        }
        let existing_group = self.group_of(a);
        let anchor = existing_group
            .map(|idx| {
                self.groups[idx].members[self.groups[idx].active]
                    .surface
                    .clone()
            })
            .unwrap_or_else(|| a.clone());
        if !self.spatial_is_tiled(&anchor)
            || !self.spatial_is_tiled(b)
            || !self.spatial_tiles_share_owner(&anchor, b)
        {
            return;
        }
        let new_group_owner = if existing_group.is_none() {
            let owner = match self.config.spatial_engine {
                crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll => {
                    self.layout.output_of(&anchor).map(|output| {
                        let workspace = self
                            .layout
                            .workspace_of(&anchor)
                            .unwrap_or_else(|| self.layout.active_workspace(output));
                        (output.to_string(), workspace)
                    })
                }
                crate::config::SpatialEngine::Ocean => {
                    self.ocean.entry_output(&anchor).map(|name| {
                        let output = name.to_string();
                        let workspace = self.layout.active_workspace(&output);
                        (output, workspace)
                    })
                }
            };
            let Some(owner) = owner else { return };
            Some(owner)
        } else {
            None
        };
        let Some(b_window) = self.detach_tiled_group_member(b) else {
            return;
        };
        self.space.unmap_elem(&b_window);

        if let Some(idx) = existing_group {
            let ui_node_id = self.allocate_ui_node_id();
            self.groups[idx].members.push(GroupMember {
                ui_node_id,
                surface: b.clone(),
                parked_window: Some(b_window),
            });
            self.groups[idx].strip = None;
        } else {
            let group_ui_node_id = self.allocate_ui_node_id();
            let a_ui_node_id = self.allocate_ui_node_id();
            let b_ui_node_id = self.allocate_ui_node_id();
            let (output, workspace) = new_group_owner.expect("new groups resolve an owner");
            self.groups.push(WindowGroup {
                ui_node_id: group_ui_node_id,
                output,
                workspace,
                members: vec![
                    GroupMember {
                        ui_node_id: a_ui_node_id,
                        surface: anchor,
                        parked_window: None,
                    },
                    GroupMember {
                        ui_node_id: b_ui_node_id,
                        surface: b.clone(),
                        parked_window: Some(b_window),
                    },
                ],
                active: 0,
                strip: None,
                strip_width: 0,
            });
        }
        // b's leaf is gone from the tree, so the group's slot just grew;
        // without this the old geometry stays on screen (dead hole where b
        // was) until something else happens to retile.
        self.retile();
        self.request_redraw();
    }

    /// Removes the member at `pos` in group `idx` (`surface` is that
    /// member's own surface), promoting the next member -- wrapping,
    /// following tab order -- into the leaf if the removed one was active,
    /// or dissolving the group entirely if only one member remains
    /// afterward. Returns the removed member's `Window` handle if one was
    /// available: a parked member's is held directly, an active member's
    /// is fetched from the tree before it's overwritten below. The caller
    /// decides what to do with it -- `ungroup` gives it its own new tile,
    /// close-cleanup (`leave_group_on_close`) does nothing further, since
    /// the window is already being destroyed/unmapped by then.
    fn leave_group(&mut self, idx: usize, surface: &WlSurface) -> Option<Window> {
        let pos = self.groups[idx]
            .members
            .iter()
            .position(|m| &m.surface == surface)?;
        let was_active = pos == self.groups[idx].active;
        let active_window = was_active
            .then(|| self.spatial_tiled_window(surface))
            .flatten();
        if was_active && active_window.is_none() {
            tracing::warn!("group active member has no spatial leaf, leaving group unchanged");
            return None;
        }

        let (new_active, dissolves) =
            group_removal_outcome(self.groups[idx].members.len(), self.groups[idx].active, pos);
        let removed = self.groups[idx].members.remove(pos);
        let removed_window = active_window.or(removed.parked_window);

        if dissolves {
            let last_parked = self.groups[idx].members[0].parked_window.take();
            self.groups.remove(idx);
            if let Some(window) = last_parked {
                if !self.replace_tiled_group_leaf(surface, &window) {
                    tracing::warn!("group dissolve lost its spatial leaf replacement");
                }
            }
            self.retile();
            return removed_window;
        }

        self.groups[idx].active = new_active;
        self.groups[idx].strip = None;
        if was_active {
            if let Some(window) = self.groups[idx].members[new_active].parked_window.take() {
                if !self.replace_tiled_group_leaf(surface, &window) {
                    tracing::warn!("group tab promotion lost its spatial leaf replacement");
                }
            }
        }
        self.retile();
        removed_window
    }

    /// Removes `surface` from its group, if grouped. The window keeps
    /// existing (unlike `leave_group_on_close`), so it becomes its own
    /// ordinary tile again. Classic uses its tree fallback; Ocean inserts
    /// beside the surviving member so the tab cannot jump to another reef.
    pub fn ungroup(&mut self, surface: &WlSurface) {
        let Some(idx) = self.group_of(surface) else {
            return;
        };
        let output = self.groups[idx].output.clone();
        let workspace = self.groups[idx].workspace;
        let anchor = self.groups[idx]
            .members
            .iter()
            .find(|member| &member.surface != surface)
            .map(|member| member.surface.clone());
        let Some(window) = self.leave_group(idx, surface) else {
            return;
        };
        match self.config.spatial_engine {
            crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll => {
                self.layout.insert(&output, workspace, window, None);
            }
            crate::config::SpatialEngine::Ocean => {
                let inserted = anchor
                    .as_ref()
                    .is_some_and(|anchor| self.ocean.insert_tiled_next_to(anchor, window));
                if !inserted {
                    tracing::warn!("ungroup could not find the surviving Ocean reef");
                    return;
                }
            }
        }
        self.retile();
        self.request_redraw();
    }

    /// If `surface` belongs to a window group, leaves it (promoting the
    /// next tab or dissolving the group, as `leave_group` describes)
    /// instead of letting `detach_mapped_toplevel`'s ordinary spatial
    /// removal collapse its leaf. No-op for an
    /// ungrouped window. Treats a temporary (null-buffer) unmap the same as
    /// permanent destruction for group membership -- a deliberate v1
    /// scope-out: a parked member being independently hidden-then-remapped
    /// while still "in" the group is its own can of worms this first pass
    /// doesn't open.
    pub(crate) fn leave_group_on_close(&mut self, surface: &WlSurface) {
        if let Some(idx) = self.group_of(surface) {
            self.leave_group(idx, surface);
        }
    }

    /// Cycles the focused window's group to the next (`forward`) or
    /// previous tab, wrapping. No-op if the focused window isn't grouped.
    /// The demoted tab is unmapped and parked exactly like `group_with`
    /// parks a freshly merged one; the promoted tab is mapped back at the
    /// leaf's rect by the `retile()` below -- `retile()` only ever
    /// positions whatever's currently in the tree, so without an explicit
    /// unmap here the demoted window would otherwise stay stuck mapped at
    /// its stale rect (same gotcha `switch_workspace` already has to work
    /// around for hidden floating windows).
    pub fn cycle_tab(&mut self, forward: bool) {
        let Some(focused) = self.focused_window_surface() else {
            return;
        };
        let Some(idx) = self.group_of(&focused) else {
            return;
        };
        let old_active = self.groups[idx].active;
        let len = self.groups[idx].members.len();
        let new_active = if forward {
            (old_active + 1) % len
        } else {
            (old_active + len - 1) % len
        };
        self.group_activate_tab(idx, new_active);
    }

    /// Makes `new_active` the tab occupying group `idx`'s shared leaf,
    /// parking the previously active one. The demoted tab is unmapped and
    /// parked exactly like `group_with` parks a freshly merged one; the
    /// promoted tab is mapped back at the leaf's rect by the `retile()`
    /// below -- `retile()` only ever positions whatever's currently in the
    /// tree, so without an explicit unmap here the demoted window would
    /// otherwise stay stuck mapped at its stale rect (same gotcha
    /// `switch_workspace` already has to work around for hidden floating
    /// windows).
    fn group_activate_tab(&mut self, idx: usize, new_active: usize) {
        let old_active = self.groups[idx].active;
        if new_active == old_active {
            return;
        }

        let old_surface = self.groups[idx].members[old_active].surface.clone();
        let Some(old_window) = self.spatial_tiled_window(&old_surface) else {
            return;
        };
        let Some(new_window) = self.groups[idx].members[new_active].parked_window.take() else {
            return;
        };

        if !self.replace_tiled_group_leaf(&old_surface, &new_window) {
            self.groups[idx].members[new_active].parked_window = Some(new_window);
            return;
        }
        self.space.unmap_elem(&old_window);
        self.groups[idx].members[old_active].parked_window = Some(old_window);
        self.groups[idx].active = new_active;
        self.groups[idx].strip = None;
        self.retile();

        let new_surface = self.groups[idx].members[new_active].surface.clone();
        self.focus_window(Some(new_surface), SERIAL_COUNTER.next_serial());
        // `reconcile_keyboard_focus` (triggered by `focus_window` above)
        // only refreshes wlr-foreign-toplevel state for the old/new
        // *keyboard* focus targets. The demoted tab (`old_surface`) is
        // only one of those if it also happened to hold actual keyboard
        // focus before this call -- a group's visible tab and the seat's
        // keyboard focus are independent (the user can click away to a
        // different window entirely without switching tabs), so a demoted
        // tab that wasn't focused would otherwise never get told anything
        // changed. Explicit and safe to call unconditionally: `send_state`
        // (see wlr_foreign_toplevel.rs) already short-circuits when its
        // computed state bytes match what was last broadcast.
        self.refresh_wlr_toplevel_state(&old_surface);
        self.request_redraw();
    }

    /// Marks `surface` urgent instead of stealing focus for it -- the
    /// downgrade path for a stale-serial xdg-activation request (see
    /// `request_activation`). A no-op if the window isn't actually mapped;
    /// urgency only means something for a window the user could plausibly
    /// switch to.
    pub(crate) fn mark_urgent(&mut self, surface: &WlSurface) {
        if self.intended_window_surface().as_ref() == Some(surface) {
            return;
        }
        if self.mapped_toplevel_window(surface).is_none() {
            return;
        }
        self.urgent.insert(surface.clone());
        if let Some(depth) = self.window_depths.get_mut(surface) {
            depth.visual_changed();
        }
        // `rule { urgent = ... }` is the one match criterion that can flip
        // after map time -- re-resolve this window's opacity/glass now so
        // a matching rule takes effect immediately rather than waiting for
        // a reload. Viscosity/sway/depth/border/shadow/rounding/ripple all
        // already resolve fresh on every use (see
        // `Smallvil::resolve_window_rules_for`'s call sites), so they need
        // no equivalent nudge here.
        self.refresh_window_opacity_and_glass_for(surface);
        // The immediate spawn is the first pulse; `update_urgent_pulses`
        // re-fires it every `urgent_repeat_interval_ms` until the hint
        // clears. A no-op if the window's workspace is hidden, since
        // spawn_ripple needs a real on-screen location -- the repeat tick
        // keeps trying, so the pulse starts up on its own once the
        // workspace becomes visible again.
        self.urgent_pulse_last
            .insert(surface.clone(), Instant::now());
        self.spawn_ripple(surface, crate::config::RippleTrigger::Urgent);
        self.emit_ipc_event(crate::ipc::IpcEvent::UrgentChanged {
            surface: surface.clone(),
            urgent: true,
        });
        self.request_redraw();
    }

    /// Focuses whichever window is currently marked urgent, if any --
    /// reuses `activate_toplevel`'s own workspace-switch/group-promote/
    /// floating-raise logic rather than duplicating it, since "focus this
    /// window" means the same thing regardless of why. `activate_toplevel`
    /// itself already clears `urgent` (via `reconcile_keyboard_focus`), so
    /// there's nothing left to clear here.
    pub fn focus_urgent(&mut self) {
        let Some(surface) = self.urgent.iter().next().cloned() else {
            return;
        };
        self.activate_toplevel(&surface);
    }

    /// Adds `tag` to `surface`'s tag set (`tag-window:<name>`) -- the
    /// runtime counterpart to `rule { add_tag = [...] }`. Idempotent, like
    /// `HashSet::insert` itself. `tag` is the one match criterion besides
    /// `urgent` that can flip after map time, so a matching rule's
    /// opacity/glass takes effect immediately rather than waiting for the
    /// next natural resolve -- same precedent as `mark_urgent`.
    pub(crate) fn tag_window(&mut self, surface: &WlSurface, tag: &str) {
        let inserted = self
            .window_tags
            .entry(surface.clone())
            .or_default()
            .insert(tag.to_string());
        if inserted {
            self.refresh_window_opacity_and_glass_for(surface);
            self.request_redraw();
        }
    }

    /// Removes `tag` from `surface`'s tag set (`untag-window:<name>`),
    /// dropping the map entry entirely once its last tag is gone so an
    /// untagged window costs nothing, same invariant `window_tags` keeps
    /// everywhere else.
    pub(crate) fn untag_window(&mut self, surface: &WlSurface, tag: &str) {
        let Some(tags) = self.window_tags.get_mut(surface) else {
            return;
        };
        if !tags.remove(tag) {
            return;
        }
        if tags.is_empty() {
            self.window_tags.remove(surface);
        }
        self.refresh_window_opacity_and_glass_for(surface);
        self.request_redraw();
    }

    /// Focuses a window carrying `tag`, cycling to the next one carrying
    /// it on repeated presses (`toggle-tag:<name>`) -- same MRU-then-
    /// arbitrary-order idea as `cycle_focus`, scoped to just the tagged
    /// subset, and reusing `activate_toplevel` so a match on a hidden
    /// workspace switches there first (`focus_urgent`'s own precedent).
    /// No-op if nothing carries the tag. `cycling_focus` suppresses the
    /// MRU reorder for the duration, the same guard `cycle_focus` uses --
    /// without it, the next press would read back whatever this one just
    /// focused as "current" and oscillate between two windows instead of
    /// visiting every tagged one in turn.
    pub(crate) fn toggle_tag(&mut self, tag: &str) {
        let mut windows: Vec<WlSurface> = self
            .window_tags
            .iter()
            .filter(|(_, tags)| tags.contains(tag))
            .map(|(surface, _)| surface.clone())
            .collect();
        // A tag can outlive the window it was on if some future caller
        // forgets the `detach_mapped_toplevel` cleanup this relies on --
        // filtering here means a stale entry gets skipped once rather than
        // wedging every future press on a dead surface `activate_toplevel`
        // can never focus (its own `mapped_toplevel_window` guard would
        // silently no-op, leaving `current_index` unchanged).
        windows.retain(|s| self.mapped_toplevel_window(s).is_some());
        if windows.is_empty() {
            return;
        }
        let mut ordered: Vec<WlSurface> = self
            .focus_history
            .iter()
            .filter(|s| windows.contains(s))
            .cloned()
            .collect();
        for surface in &windows {
            if !ordered.contains(surface) {
                ordered.push(surface.clone());
            }
        }
        let current = self.focused_window_surface();
        let current_index = current
            .as_ref()
            .and_then(|s| ordered.iter().position(|o| o == s));
        let next_index = match current_index {
            Some(i) => (i + 1) % ordered.len(),
            None => 0,
        };
        let next = ordered[next_index].clone();
        self.cycling_focus = true;
        self.activate_toplevel(&next);
        self.cycling_focus = false;
    }

    /// Turns `focused`'s Scroll column into one tab group (tabs in column
    /// order, `focused` stays the visible tab), or a tab group back into a
    /// stacked column in tab order, where the group was.
    fn toggle_column_tabbed(&mut self, focused: &WlSurface) {
        if self.fullscreen.contains_key(focused) {
            return;
        }
        if let Some(idx) = self.group_of(focused) {
            let members: Vec<WlSurface> = self.groups[idx]
                .members
                .iter()
                .map(|member| member.surface.clone())
                .collect();
            let leaf = members[self.groups[idx].active].clone();
            for member in members.iter().filter(|member| **member != leaf) {
                self.ungroup(member);
            }
            let (Some(output), Some(workspace)) = (
                self.layout.output_of(&leaf).map(str::to_owned),
                self.layout.workspace_of(&leaf),
            ) else {
                return;
            };
            let mut columns = self.layout.scroll_columns(&output, workspace);
            let Some(at) = columns.iter().position(|c| c.windows.contains(&leaf)) else {
                return;
            };
            let width = columns[at].width;
            for column in &mut columns {
                column.windows.retain(|s| !members.contains(s));
            }
            columns[at].windows = members
                .iter()
                .filter(|s| self.layout.contains(s))
                .cloned()
                .collect();
            columns[at].width = width;
            columns.retain(|c| !c.windows.is_empty());
            self.layout.set_scroll_columns(&output, workspace, columns);
            self.focus_window(Some(focused.clone()), SERIAL_COUNTER.next_serial());
            self.scroll_columns_changed(focused);
            return;
        }
        let (Some(output), Some(workspace)) = (
            self.layout.output_of(focused).map(str::to_owned),
            self.layout.workspace_of(focused),
        ) else {
            return;
        };
        let Some(column) = self
            .layout
            .scroll_columns(&output, workspace)
            .into_iter()
            .find(|c| c.windows.contains(focused))
        else {
            return;
        };
        if column.windows.len() < 2
            || column
                .windows
                .iter()
                .any(|s| self.group_of(s).is_some() || self.fullscreen.contains_key(s))
        {
            return;
        }
        let head = column.windows[0].clone();
        for other in &column.windows[1..] {
            self.group_with(&head, other);
        }
        if let Some(idx) = self.group_of(&head) {
            if let Some(position) = self.groups[idx]
                .members
                .iter()
                .position(|m| &m.surface == focused)
            {
                self.group_activate_tab(idx, position);
            }
        }
        self.scroll_columns_changed(focused);
    }

    /// Looks up protocol-mapped windows without depending on current visibility.
    pub(crate) fn window_surface_by_id(&self, id: u64) -> Option<WlSurface> {
        self.foreign_toplevel_numeric_ids
            .iter()
            .find(|(surface, candidate)| {
                **candidate == id && self.mapped_toplevel_window(surface).is_some()
            })
            .map(|(surface, _)| surface.clone())
    }

    /// Invalidates every compositor-owned texture whose pixels include this
    /// toplevel's title. Called only when the authoritative xdg-shell title
    /// differs from the value already announced to foreign-toplevel clients.
    pub(crate) fn invalidate_window_title_ui(&mut self, surface: &WlSurface) {
        self.depth_schematics.remove(surface);
        for group in &mut self.groups {
            if group
                .members
                .iter()
                .any(|member| &member.surface == surface)
            {
                group.strip = None;
            }
        }
    }

    /// Builds visible group tab strips at their active placement. Cached pixels
    /// rebuild only after invalidation or a live width change.
    pub(crate) fn tab_strip_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        scene: &[crate::placement::PlacedWindow],
    ) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let output_name = output.name();
        // Groups are few; a flat vector avoids using Wayland objects (whose
        // liveness flag is interior-mutable) as hash keys for transient
        // per-frame placement data.
        let mut placements: Vec<(WlSurface, Rectangle<i32, Logical>)> = Vec::new();
        for placement in scene {
            let Some(surface) = placement.surface().cloned() else {
                continue;
            };
            let rect = crate::placement::translated_rect(placement.rect, placement.view_offset);
            placements.push((surface, rect));
        }

        // Render elements take output-local physical positions; placements
        // are global logical rects.
        let Some(output_geo) = self.space.output_geometry(output) else {
            return Vec::new();
        };
        let scale = output.current_scale().fractional_scale();
        let mut elements = Vec::new();
        for group in &mut self.groups {
            if self.config.spatial_engine.uses_workspaces() && group.output != output_name {
                continue;
            }
            let active_surface = &group.members[group.active].surface;
            let Some(rect) = placements
                .iter()
                .find_map(|(surface, rect)| (surface == active_surface).then_some(*rect))
            else {
                continue;
            };

            if group.strip.is_none() || group.strip_width != rect.size.w {
                group.strip = None;
                let titles: Vec<String> = group
                    .members
                    .iter()
                    .map(|m| crate::tab_strip::window_title(&m.surface))
                    .collect();
                if let Some((buffer, width)) =
                    crate::tab_strip::build_buffer(&titles, group.active, rect.size.w)
                {
                    group.strip = Some(buffer);
                    group.strip_width = width;
                }
            }

            if let Some(buffer) = &group.strip {
                let location = (
                    f64::from(rect.loc.x - output_geo.loc.x) * scale,
                    f64::from(rect.loc.y - output_geo.loc.y) * scale,
                );
                if let Some(element) = crate::tab_strip::render_element(buffer, renderer, location)
                {
                    elements.push(element);
                }
            }
        }
        elements
    }

    /// Switches the current output's active workspace to `algorithm` (see
    /// `Action::SetLayout`). `Layouts` keeps the exact same tree either way
    /// -- membership, insertion order, groups, swap/focus-direction are all
    /// tree-shape-based rather than geometry-based, so none of that needs
    /// to know which algorithm is active. Only the rects `retile()`
    /// computes for this workspace change.
    pub fn set_layout_algorithm(&mut self, algorithm: crate::config::LayoutAlgorithm) {
        let Some(output) = self.primary_output() else {
            return;
        };
        let workspace = self.layout.active_workspace(&output.name());
        if self.layout.window_count(&output.name(), workspace) == 0 {
            return;
        }
        self.layout
            .set_algorithm(&output.name(), workspace, algorithm);
        self.retile();
    }

    /// Nudges the current output's active workspace's master/stack ratio
    /// (see `Action::GrowMaster`/`ShrinkMaster`). A visible no-op while BSP
    /// is active -- `layout::Layouts::layout` doesn't consult the ratio in
    /// that mode -- but still recorded against this (output, workspace), so
    /// switching to master later immediately reflects whatever was last set.
    pub fn adjust_master_ratio(&mut self, delta: f32) {
        let Some(output) = self.primary_output() else {
            return;
        };
        let workspace = self.layout.active_workspace(&output.name());
        if self.layout.window_count(&output.name(), workspace) == 0 {
            return;
        }
        self.layout
            .adjust_master_ratio(&output.name(), workspace, delta);
        self.retile();
    }

    /// Resizes the focused window without a pointer grab. Floating windows
    /// change in 24-logical-pixel steps; BSP windows drive the nearest split
    /// and its connected parallel ancestors while preserving the 5% safety
    /// clamp.
    pub fn keyboard_resize(&mut self, direction: Direction) {
        const STEP: i32 = 24;
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        let Some(window) = self.mapped_toplevel_window(&surface) else {
            return;
        };
        if self.fullscreen.contains_key(&surface) || self.maximized.contains_key(&surface) {
            return;
        }

        if self.config.spatial_engine == crate::config::SpatialEngine::Scroll
            && self.layout.contains(&surface)
        {
            let Some(owner) = self.layout.output_of(&surface).map(str::to_owned) else {
                return;
            };
            let Some(output) = self.output_by_name(&owner) else {
                return;
            };
            let Some(area) = self.output_tiling_area(&output) else {
                return;
            };
            let (along, _) = self.scroll_axis_step(direction);
            if !along {
                // Across the strip: the window's share of a shared column.
                let Some((column, _)) = self.layout.scroll_position(&surface) else {
                    return;
                };
                let Some(workspace) = self.layout.workspace_of(&surface) else {
                    return;
                };
                let rows = self.layout.scroll_columns(&owner, workspace)[column]
                    .windows
                    .len();
                if !crate::scroll::row_resize_enabled(rows, self.config.scroll.stack) {
                    return;
                }
                let cross = match self.config.scroll.orientation {
                    crate::scroll::Orientation::Horizontal => window.geometry().size.h,
                    crate::scroll::Orientation::Vertical => window.geometry().size.w,
                }
                .max(1);
                let delta = if matches!(direction, Direction::Right | Direction::Down) {
                    STEP
                } else {
                    -STEP
                };
                let weight = self.layout.scroll.weight(&surface)
                    * f64::from((cross + delta).max(1))
                    / f64::from(cross);
                self.layout.scroll.set_weight(&surface, weight);
                self.retile();
                self.reveal_scroll_window(&surface);
                self.request_redraw();
                return;
            }
            let extent = match self.config.scroll.orientation {
                crate::scroll::Orientation::Horizontal => area.size.w,
                crate::scroll::Orientation::Vertical => area.size.h,
            }
            .max(1);
            let delta = if matches!(direction, Direction::Right | Direction::Down) {
                STEP
            } else {
                -STEP
            };
            let proportion =
                self.layout.scroll.proportion(&surface) + f64::from(delta) / f64::from(extent);
            self.layout.scroll.resize(surface.clone(), proportion);
            self.retile();
            self.reveal_scroll_window(&surface);
            self.request_redraw();
            return;
        }

        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            if let Some(mut rect) = self.ocean.floating_rect(&surface) {
                rect.size = keyboard_resized_size(
                    rect.size,
                    direction,
                    STEP,
                    self.floating_size_constraints_for(&surface),
                );
                self.ocean.set_floating_rect(&surface, rect);
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| state.size = Some(rect.size));
                    toplevel.send_pending_configure();
                }
                self.retile();
                return;
            }

            let axis = match direction {
                Direction::Left | Direction::Right => crate::layout::Axis::Horizontal,
                Direction::Up | Direction::Down => crate::layout::Axis::Vertical,
            };
            let delta = if matches!(direction, Direction::Right | Direction::Down) {
                f64::from(STEP)
            } else {
                f64::from(-STEP)
            };
            if self
                .ocean
                .resize_tiled_by(&surface, axis, delta, self.config.bsp_split_bias)
            {
                self.retile();
            }
            return;
        }

        if !self.layout.contains(&surface) {
            // `Window::geometry().loc` is surface-local (usually 0,0), not
            // the element's global position in `Space`. Reusing it here
            // would teleport every keyboard-resized floater to the output's
            // origin.
            let location = self.space.element_location(&window).unwrap_or_default();
            let mut geometry = window.geometry();
            // Accumulate key repeats/batched actions against the latest
            // requested size, as niri's floating actions use expected_size.
            // The committed bbox can lag several configures behind input.
            let requested = window
                .toplevel()
                .and_then(|toplevel| toplevel.with_pending_state(|state| state.size))
                .unwrap_or(geometry.size);
            geometry.size = keyboard_resized_size(
                requested,
                direction,
                STEP,
                self.floating_size_constraints_for(&surface),
            );
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(geometry.size));
                toplevel.send_pending_configure();
            }
            self.space.map_element(window, location, true);
            if let Some(tag) = self.floating_workspace.get_mut(&surface) {
                tag.rect.loc = location;
                tag.rect.size = geometry.size;
            }
            self.request_redraw();
            return;
        }

        let Some(output_name) = self.layout.output_of(&surface).map(str::to_owned) else {
            return;
        };
        let Some(output) = self.output_by_name(&output_name) else {
            return;
        };
        let workspace = self
            .layout
            .workspace_of(&surface)
            .unwrap_or_else(|| self.layout.active_workspace(&output_name));
        let Some(area) = self.output_tiling_area(&output) else {
            return;
        };
        let wanted_axis = match direction {
            Direction::Left | Direction::Right => crate::layout::Axis::Horizontal,
            Direction::Up | Direction::Down => crate::layout::Axis::Vertical,
        };
        let Some(hit) = self
            .layout
            .resize_splits(&output_name, workspace, area, &surface)
            .into_iter()
            .find(|hit| hit.axis == wanted_axis)
        else {
            return;
        };
        let handles = self.connected_resize_handles(&hit);
        let delta_pixels = if matches!(direction, Direction::Right | Direction::Down) {
            f64::from(STEP)
        } else {
            f64::from(-STEP)
        };
        for handle in handles {
            let Some(new_ratio) = handle.ratio_for_delta(delta_pixels) else {
                continue;
            };
            self.layout.set_ratio(
                &handle.hit.output,
                handle.hit.workspace,
                &handle.hit.path,
                new_ratio,
            );
        }
        self.retile();
    }

    /// Keyboard-driven move for the focused *floating* window only (spatial
    /// roadmap Phase 4's escape hatch alongside `toggle-floating`; see
    /// `Action::MoveFloating`). A no-op on a tiled window, mirroring
    /// `keyboard_resize`'s per-engine floating/tiled split just above.
    pub fn keyboard_move_floating(&mut self, direction: Direction) {
        const STEP: i32 = 24;
        let Some(surface) = self.focused_window_surface() else {
            return;
        };

        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            let Some(mut rect) = self.ocean.floating_rect(&surface) else {
                return;
            };
            rect.loc = saturating_translate(rect.loc, direction_delta(direction, STEP));
            self.ocean.set_floating_rect(&surface, rect);
            self.retile();
            return;
        }

        if self.layout.contains(&surface) {
            return;
        }
        let Some(window) = self.mapped_toplevel_window(&surface) else {
            return;
        };
        // Same reasoning `translate_floating_windows_on_output` documents:
        // the live `Space` rect (not `Window::geometry().loc`, surface-local
        // and usually `0,0`) is the window's real global position, and
        // `floating_workspace`'s own `rect` must move with it or a later
        // hidden-workspace/restore read (workspace switch, fullscreen
        // restore) would snap the window back to its pre-move position.
        let Some(mut rect) = self.space.element_geometry(&window) else {
            return;
        };
        rect.loc = saturating_translate(rect.loc, direction_delta(direction, STEP));
        self.space.map_element(window, rect.loc, false);
        if let Some(tag) = self.floating_workspace.get_mut(&surface) {
            tag.rect = rect;
        }
        self.retile();
    }

    /// Keyboard parity for Classic drag-to-snap. The focused floater uses
    /// the same usable-area, gap, rule, and target geometry as a pointer drop.
    pub fn keyboard_snap(&mut self, zone: crate::snap::SnapZone) {
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        if !self.snap_eligible_surface(&surface) {
            return;
        }
        let Some(window) = self.mapped_toplevel_window(&surface) else {
            return;
        };
        let Some(output) = self.output_for_window(&window) else {
            return;
        };
        let Some(target) = self.snap_target_for_zone(&surface, &output, zone) else {
            return;
        };
        self.apply_snap_target(&window, &target);
    }

    /// Parks the focused ordinary tiled window below its current Classic
    /// workspace. Grouped and rect-override windows are deliberately excluded
    /// from v1 so parking cannot silently break another ownership model.
    pub fn sink_window(&mut self) {
        if !self.config.classic_depth.enabled {
            return;
        }
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        if !self.layout.contains(&surface)
            || self.group_of(&surface).is_some()
            || self.fullscreen.contains_key(&surface)
            || self.maximized.contains_key(&surface)
            || self.pseudo_tiled.contains(&surface)
            || self.pinned.contains(&surface)
        {
            return;
        }
        let (Some(output), Some(workspace), Some(window)) = (
            self.layout.output_of(&surface).map(str::to_string),
            self.layout.workspace_of(&surface),
            self.layout.window_of(&surface),
        ) else {
            return;
        };

        self.layout.remove(&surface);
        self.space.unmap_elem(&window);
        self.classic_depth
            .park(crate::classic_depth::DepthDeckEntry {
                surface: surface.clone(),
                window,
                output: output.clone(),
                workspace,
            });
        self.close_depth_deck();
        self.retile();
        self.refocus_after_hide(&output);
        self.refresh_wlr_toplevel_state(&surface);
        self.start_depth_transition(&output, true);
        self.request_redraw();
    }

    /// Opens (or closes) the current Classic workspace's deck. It is a static
    /// compositor overlay and does not steal keyboard focus from the tile that
    /// will be used as the default swap target.
    pub fn dive(&mut self) {
        if !self.config.classic_depth.enabled {
            return;
        }
        if self.classic_depth.view().is_some() {
            self.close_depth_deck();
            return;
        }
        self.overview = None;
        let Some(output) = self.primary_output() else {
            return;
        };
        let output_name = output.name();
        let workspace = self.layout.active_workspace(&output_name);
        if self.classic_depth.open(output_name, workspace) {
            self.rebuild_depth_deck_overlay();
            self.request_redraw();
        }
    }

    pub fn cycle_depth_deck(&mut self, delta: isize) {
        if !self.config.classic_depth.enabled || !self.classic_depth.cycle(delta) {
            return;
        }
        self.rebuild_depth_deck_overlay();
        self.request_redraw();
    }

    pub fn close_depth_deck(&mut self) {
        let changed = self.classic_depth.close() | self.depth_deck_overlay.take().is_some();
        if changed {
            self.request_redraw();
        }
    }

    /// Direct, workspace-like depth movement. Repeated Down walks newest to
    /// oldest through the current workspace's deck; Up walks the same ring in
    /// reverse. The focused tile stays geometrically fixed throughout.
    pub fn switch_depth(&mut self, down: bool) {
        if !self.config.classic_depth.enabled {
            return;
        }
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        if !self.layout.contains(&surface)
            || self.group_of(&surface).is_some()
            || self.fullscreen.contains_key(&surface)
            || self.maximized.contains_key(&surface)
            || self.pseudo_tiled.contains(&surface)
            || self.pinned.contains(&surface)
        {
            return;
        }
        let (Some(output), Some(workspace), Some(old_window)) = (
            self.layout.output_of(&surface).map(str::to_string),
            self.layout.workspace_of(&surface),
            self.layout.window_of(&surface),
        ) else {
            return;
        };

        if self
            .classic_depth
            .entries_for(&output, workspace)
            .next()
            .is_none()
        {
            if down && self.layout.window_count(&output, workspace) > 1 {
                self.sink_window();
            }
            return;
        }

        let Some(selected) = self.classic_depth.rotate(
            &output,
            workspace,
            crate::classic_depth::DepthDeckEntry {
                surface: surface.clone(),
                window: old_window.clone(),
                output: output.clone(),
                workspace,
            },
            down,
        ) else {
            return;
        };
        self.close_depth_deck();
        self.layout.replace_leaf(&surface, &selected.window);
        self.space.unmap_elem(&old_window);
        self.retile();
        self.note_depth_attention(&selected.surface);
        self.focus_window(Some(selected.surface.clone()), SERIAL_COUNTER.next_serial());
        self.refresh_wlr_toplevel_state(&surface);
        self.refresh_wlr_toplevel_state(&selected.surface);
        self.start_depth_transition(&output, down);
        self.request_redraw();
    }

    /// Recalls the selected deep window. A compatible focused tile is replaced
    /// exactly and becomes the new occupant of that same deck slot; otherwise
    /// the selected window is inserted as an ordinary tile.
    pub fn select_depth_deck(&mut self) {
        if !self.config.classic_depth.enabled {
            return;
        }
        let Some(view) = self.classic_depth.view().cloned() else {
            return;
        };
        let swap_surface = self.focused_window_surface().filter(|surface| {
            self.layout.output_of(surface) == Some(view.output.as_str())
                && self.layout.workspace_of(surface) == Some(view.workspace)
                && self.group_of(surface).is_none()
                && !self.fullscreen.contains_key(surface)
                && !self.maximized.contains_key(surface)
                && !self.pseudo_tiled.contains(surface)
                && !self.pinned.contains(surface)
        });

        let selected = if let Some(surface) = swap_surface {
            let Some(old_window) = self.layout.window_of(&surface) else {
                return;
            };
            let Some(selected) =
                self.classic_depth
                    .swap_selected(crate::classic_depth::DepthDeckEntry {
                        surface: surface.clone(),
                        window: old_window.clone(),
                        output: view.output.clone(),
                        workspace: view.workspace,
                    })
            else {
                return;
            };
            self.layout.replace_leaf(&surface, &selected.window);
            self.space.unmap_elem(&old_window);
            self.refresh_wlr_toplevel_state(&surface);
            selected
        } else {
            let Some(selected) = self.classic_depth.take_selected() else {
                return;
            };
            self.layout.insert(
                &selected.output,
                selected.workspace,
                selected.window.clone(),
                None,
            );
            selected
        };

        self.classic_depth.close();
        self.depth_deck_overlay = None;
        self.retile();
        self.note_depth_attention(&selected.surface);
        self.focus_window(Some(selected.surface.clone()), SERIAL_COUNTER.next_serial());
        self.refresh_wlr_toplevel_state(&selected.surface);
        self.start_depth_transition(&selected.output, false);
        self.request_redraw();
    }

    fn rebuild_depth_deck_overlay(&mut self) {
        let Some(view) = self.classic_depth.view().cloned() else {
            self.depth_deck_overlay = None;
            return;
        };
        let Some(output) = self.output_by_name(&view.output) else {
            self.close_depth_deck();
            return;
        };
        let Some(output_geometry) = self.space.output_geometry(&output) else {
            self.close_depth_deck();
            return;
        };
        let titles: Vec<String> = self
            .classic_depth
            .entries_for(&view.output, view.workspace)
            .map(|entry| crate::tab_strip::window_title(&entry.surface))
            .collect();
        self.depth_deck_overlay = Some(crate::depth_deck::DepthDeckOverlay::build(
            view.output,
            view.workspace,
            &titles,
            view.selected,
            (output_geometry.size.w, output_geometry.size.h),
        ));
    }

    /// Hot-disabling Classic depth must never strand clients in hidden
    /// structural state. Restore every parked window to its owning tree; only
    /// active workspaces are remapped by the normal `retile()` pass.
    fn restore_all_depth_deck_windows(&mut self) {
        let entries = self.classic_depth.drain();
        self.depth_deck_overlay = None;
        for entry in entries {
            self.layout
                .insert(&entry.output, entry.workspace, entry.window, None);
        }
    }

    fn start_depth_transition(&mut self, output: &str, down: bool) {
        let config = &self.config.classic_depth;
        if !config.enabled || !config.animation || config.animation_duration_ms == 0 {
            return;
        }
        self.depth_transitions.insert(
            output.to_string(),
            crate::depth_transition::DepthTransition::new(
                if down {
                    crate::depth_transition::DepthTransitionDirection::Down
                } else {
                    crate::depth_transition::DepthTransitionDirection::Up
                },
                Duration::from_millis(config.animation_duration_ms as u64),
                config.wave_color,
                config.wave_alpha,
            ),
        );
    }

    /// Ocean directional focus projects every world rectangle through the
    /// current camera before comparing direction, so on-screen and off-screen
    /// candidates share one coordinate space. Screen pins substitute their
    /// real viewport placement instead of their dormant world rectangle.
    fn ocean_focus_target(
        &self,
        focused: &WlSurface,
        output: &Output,
        direction: Direction,
    ) -> Option<(Window, Rectangle<i32, Logical>, bool)> {
        let output_geo = self.space.output_geometry(output)?;
        let visible = self.render_placements(output)?;
        let layouts = self
            .ocean
            .world_layouts(self.config.gaps, self.config.bsp_split_bias);
        let current = layouts.iter().find(|(window, _, _)| {
            window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == focused)
        })?;
        let camera = self.ocean.camera(&output.name());
        let current_view = visible
            .iter()
            .find(|placement| placement.surface() == Some(focused))
            .map(|placement| placement.rect)
            .unwrap_or_else(|| crate::ocean::world_to_view_rect(current.1, camera, output_geo.loc));
        let (window, rect, _) = layouts
            .iter()
            .filter_map(|(window, rect, _)| {
                let surface = window.toplevel().map(|t| t.wl_surface())?;
                if surface == focused
                    || !self.ocean.visible_on_output(surface, &output.name())
                    || self
                        .fullscreen
                        .get(surface)
                        .is_some_and(|entry| entry.output != output.name())
                {
                    return None;
                }
                let visible_rect = visible
                    .iter()
                    .find(|placement| placement.surface() == Some(surface))
                    .map(|placement| placement.rect);
                let view_rect = match visible_rect {
                    Some(rect) => rect,
                    None if self.ocean.is_screen_pinned(surface) => return None,
                    None => crate::ocean::world_to_view_rect(*rect, camera, output_geo.loc),
                };
                let score = directional_score(current_view, view_rect, direction)?;
                Some((window.clone(), *rect, score))
            })
            .min_by(|(_, _, a), (_, _, b)| a.total_cmp(b))?;
        let surface = window.toplevel()?.wl_surface();
        let already_visible = visible
            .iter()
            .any(|placement| placement.surface() == Some(surface));
        Some((window, rect, already_visible))
    }
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
    match (engine, first_workspace) {
        (
            crate::config::SpatialEngine::Classic | crate::config::SpatialEngine::Scroll,
            Some(workspace),
        ) => i64::from(workspace),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
