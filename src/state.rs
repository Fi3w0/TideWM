use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsString,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                utils::RescaleRenderElement,
                AsRenderElements, Kind, RenderElementStates,
            },
            gles::{GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture},
            utils::CommitCounter,
            ImportAll, ImportMem,
        },
        session::libseat::LibSeatSession,
    },
    desktop::{
        self, layer_map_for_output,
        space::SpaceRenderElements,
        utils::{
            send_frames_surface_tree, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, under_from_surface_tree, OutputPresentationFeedback,
        },
        PopupGrab, PopupManager, PopupUngrabStrategy, Space, Window, WindowSurfaceType,
    },
    input::{
        keyboard::XkbConfig,
        pointer::{CursorImageStatus, MotionEvent, PointerHandle},
        Seat, SeatState,
    },
    output::Output,
    reexports::{
        calloop::{
            generic::Generic,
            timer::{TimeoutAction, Timer},
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Client, Display, DisplayHandle, Resource,
        },
    },
    utils::{Clock, Logical, Monotonic, Physical, Point, Rectangle, Scale, Size, SERIAL_COUNTER},
    wayland::{
        compositor::{get_parent, CompositorClientState, CompositorState},
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
        pointer_constraints::{with_pointer_constraint, PointerConstraintsState},
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
        socket::ListeningSocketSource,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_activation::XdgActivationState,
        xdg_toplevel_icon::XdgToplevelIconManager,
    },
};

use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use crate::{
    capture::PendingCapture,
    config::{Config, Direction, WorkspaceRef},
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

pub struct Smallvil {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub config: Config,
    /// Every touchpad-class device `apply_touchpad_config` has been run
    /// against (populated on `DeviceAdded`, pruned on `DeviceRemoved` --
    /// see `backend/udev.rs`), so `reload_config` can re-apply a live
    /// `[input.touchpad]` edit to hardware that's already connected.
    /// libinput's `Device` is cheaply `Clone` (ref-counted), so this holds
    /// owned handles rather than re-deriving them some other way. Always
    /// empty under winit: the nested backend never reports a real libinput
    /// device.
    pub known_touchpads: Vec<smithay::reexports::input::Device>,
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
    #[cfg(feature = "screencast")]
    pub(crate) screencast_picker: Option<crate::source_picker::SourcePicker>,
    /// `Some` when `show_welcome_hint` was still on at startup (`main.rs`).
    /// Built once, static content -- `should_show_welcome_hint` decides
    /// per-frame whether the render call sites actually draw it, this
    /// field just holds the texture so it isn't rebuilt every time.
    pub welcome_hint: Option<crate::welcome::WelcomeHint>,
    last_config_event: Instant,
    config_reload_timer_armed: bool,
    needs_redraw: bool,
    /// Timestamp of the last real pointer motion (absolute or relative),
    /// updated from `Smallvil::note_pointer_motion`. Drives
    /// `config.cursor_hide_after_ms`'s auto-hide check at render time
    /// (`backend/udev.rs`) -- authoritative regardless of why a given
    /// render happened to run.
    pub(crate) last_pointer_motion: Instant,
    /// Whether a `cursor_hide_after_ms` wake-up timer is currently pending.
    /// `note_pointer_motion` only arms a new timer when this is `false`,
    /// rather than spawning a fresh calloop source on every single motion
    /// event (a real mouse can fire hundreds of those a second) -- the one
    /// live timer re-reads `last_pointer_motion` when it fires and
    /// reschedules itself further out if motion happened more recently
    /// than expected, the same self-extending `TimeoutAction::ToDuration`
    /// idiom `winit.rs`'s own redraw timer already uses.
    cursor_idle_timer_armed: bool,
    /// `Some(name)` while a `[submap.<name>]` keybind table is active
    /// (`Action::EnterSubmap`/`ExitSubmap`, `input.rs`'s keybind-matching
    /// closure). `None` means the base `[keybinds]` table is in effect.
    /// Not tied to focus or any other implicit event -- only an explicit
    /// `exit-submap` bind clears it, matching sway/Hyprland's own "mode"
    /// behavior.
    pub active_submap: Option<String>,

    /// Long-lived IPC subscribe connections, keyed by a process-unique id
    /// assigned at subscribe time. Empty in the steady state where no
    /// widget is connected, which makes `emit_ipc_event` a cheap
    /// early-return on every state change. See `ipc::IpcSubscriber` for
    /// the per-connection fields and lifecycle.
    pub(crate) ipc_subscribers: HashMap<usize, crate::ipc::IpcSubscriber>,
    /// Next subscriber id. Monotonic across the process lifetime rather
    /// than recycled, so a stale reference inside an in-flight calloop
    /// callback can't accidentally target a fresh subscriber that reused
    /// the same id.
    pub(crate) next_ipc_subscriber_id: usize,

    pub layout: Layouts,
    pub space: Space<Window>,
    /// Per-mapped-window base/active/inactive/fullscreen alpha multipliers
    /// resolved from window rules. Missing entries preserve client alpha.
    pub(crate) window_opacity: HashMap<WlSurface, crate::config::WindowOpacity>,
    /// Explicit captured-backdrop mode resolved from window rules. Missing
    /// entries preserve the original behavior: translucent floating windows
    /// use water refraction.
    pub(crate) window_glass_modes: HashMap<WlSurface, crate::config::GlassMode>,
    /// Captured immediately before a visible frame and sampled by
    /// water/frost glass while building that same frame's elements. The
    /// window-sized texture is reused until its dimensions change. Evicted in
    /// `detach_mapped_toplevel` alongside `window_opacity`, or this grows
    /// for the life of the session.
    pub(crate) backdrop_textures: HashMap<WlSurface, crate::backdrop::BackdropCapture>,
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
    /// Cached title-card buffers for tier-two-and-deeper windows. A window
    /// returning to live content or unmapping evicts its buffer immediately.
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
    /// Lazily compiled custom texture shader shared by every output's
    /// transition element.
    pub(crate) workspace_transition_program: Option<GlesTexProgram>,
    pub loop_handle: LoopHandle<'static, Smallvil>,
    pub loop_signal: LoopSignal,

    // Smithay State
    pub compositor_state: CompositorState,
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
    /// xdg-decoration support). Default mode `Server`, and `request_mode`
    /// re-affirms it -- same enforcement shape as the xdg variant.
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
    /// surface forwards everything instead of matching `[keybinds]`.
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
    /// synthetic key events as if from real hardware. Requests deliver
    /// straight to the wl_keyboard of whatever surface actually has
    /// keyboard focus -- deliberately bypassing `input.rs`'s own filter
    /// closure and any active grab, per protocol design, so an injected
    /// `Super+<key>` is delivered as a literal keypress rather than
    /// triggering a WM keybind. Like input-method, the global is hidden
    /// from sandboxed security-context clients.
    #[allow(dead_code)]
    pub virtual_keyboard_manager_state: VirtualKeyboardManagerState,
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
    /// non-integer output scales (the `[[output]] scale` config knob
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
    pub dmabuf_global: Option<DmabufGlobal>,
    /// Shared with `backend::udev::DeviceData` so both the render loop and
    /// `DmabufHandler::dmabuf_imported` (dispatched from client requests,
    /// never re-entrantly with a render) can independently borrow it.
    pub udev_renderer: Option<Rc<RefCell<GlesRenderer>>>,
    /// Only populated by the udev backend (`backend/udev.rs`), which is the
    /// only place VT switching means anything -- under winit a host
    /// compositor already owns that. `input.rs`'s VT-switch keybind
    /// detection calls `change_vt` on this when it's `Some`.
    pub session: Option<LibSeatSession>,

    /// Only read by the udev backend (see `cursor.rs`, `backend/udev.rs`).
    /// Under winit the host compositor draws the real cursor, so this is
    /// tracked but never rendered from.
    pub cursor_status: CursorImageStatus,

    /// Loaded xcursor theme for `CursorImageStatus::Named`, only populated
    /// by the udev backend (`backend/udev.rs`, same pattern as `session`/
    /// `udev_renderer` above). `None` under winit, and also under udev if no
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
    /// Which (output, workspace) tree's leaf this group occupies -- needed
    /// to re-insert a parked member as its own tile again on `ungroup`
    /// (`Layouts::insert` takes both explicitly).
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

/// Gesture streams captured by compositor bindings instead of forwarded via
/// wp-pointer-gestures. A stream is claimed at Begin and remains owned until
/// End so clients never receive a partial gesture.
pub(crate) enum CompositorGesture {
    Swipe {
        workspace_fallback: bool,
        delta_x: f64,
        delta_y: f64,
    },
    Pinch {
        scale: f64,
    },
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
}

impl Smallvil {
    pub(crate) fn screencast_picker_element(
        &self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        #[cfg(feature = "screencast")]
        {
            self.screencast_picker
                .as_ref()
                .filter(|picker| picker.output_name() == output.name())
                .and_then(|picker| picker.render_element(renderer))
        }
        #[cfg(not(feature = "screencast"))]
        {
            let _ = (output, renderer);
            None
        }
    }

    /// Builds the desktop stack explicitly, allowing both per-window alpha
    /// and fullscreen placement to differ from Smithay's all-or-nothing
    /// `space_render_elements` helper. The returned vector is front-to-back.
    /// Fullscreen windows cover layer-shell Top/Overlay surfaces as requested;
    /// regular windows retain the protocol order above Bottom/Background.
    /// `skip` omits each listed surface's own elements from the result --
    /// used by backdrop capture (`backdrop.rs`, Phase R0.5) to render
    /// "everything behind window X" without X itself in the way, and by
    /// water-glass (`water_glass.rs`, Phase R1) to pull every eligible
    /// window out of its normal z-slot so it can be reinserted (its own
    /// element plus a water-glass layer) at a caller-chosen position. A
    /// slice rather than one surface since more than one window can be
    /// water-glass-eligible on the same output at once. Built into the
    /// same real walk every other caller uses (`&[]`) rather than
    /// filtering an already-built list by index afterward: this list
    /// mixes windows and layer-shell surfaces across two
    /// fullscreen-ordering passes, and index arithmetic against that
    /// composition is exactly the kind of off-by-one this project's
    /// front-to-back element-order convention has already been burned by
    /// once (see session-lock's element-order bug, AGENT.md).
    fn window_visual_sample(&self, surface: &WlSurface) -> crate::window_animation::VisualSample {
        let mut sample = crate::window_animation::VisualSample::default();
        if let Some(open) = self.window_open_animations.get(surface) {
            let current = open.sample();
            sample.offset += current.offset;
            sample.opacity *= current.opacity;
        }
        if let Some(viscosity) = self.window_viscosity.get(surface) {
            if let Some(window) = self.mapped_toplevel_window(surface) {
                if let Some(location) = self.space.element_location(&window) {
                    let current = viscosity.sample();
                    sample.offset += Point::from((
                        current.loc.x - location.x as f64,
                        current.loc.y - location.y as f64,
                    ));
                    sample.size = Some(current.size);
                }
            }
        } else if let Some(movement) = self.window_move_animations.get(surface) {
            let current = movement.sample();
            sample.offset += current.offset;
            sample.size = current.size;
            sample.opacity *= current.opacity;
        }
        if let Some(sway) = self.window_sway.get(surface) {
            sample.offset.x += sway.sample();
        }
        sample
    }

    fn viscosity_for_surface(&self, surface: &WlSurface) -> f64 {
        if !self.config.water_effects {
            return 0.0;
        }
        let (app_id, title) = self.toplevel_identity(surface);
        self.config
            .resolve_window_rules(app_id.as_deref(), title.as_deref())
            .viscosity
            .unwrap_or(self.config.viscosity)
            .clamp(0.0, 4.0)
    }

    pub(crate) fn connected_resize_handles(
        &self,
        hit: &crate::layout::SplitHit,
    ) -> Vec<crate::layout::SplitResizeHandle> {
        let config = self.config.connected_vessels;
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
        let viscosity = self.viscosity_for_surface(surface);
        // Interactive input owns the visual geometry from this point. The
        // generic movement tween must not remain underneath it even when
        // viscosity is disabled and the pointer path becomes immediate.
        self.window_move_animations.remove(surface);
        if viscosity <= f64::EPSILON {
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
            motion.retarget(target, viscosity);
        } else {
            self.window_viscosity.insert(
                surface.clone(),
                crate::viscosity::ViscousMotion::new(live, target, viscosity),
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
        let (app_id, title) = self.toplevel_identity(surface);
        self.config
            .resolve_window_rules(app_id.as_deref(), title.as_deref())
            .sway
            .unwrap_or(self.config.sway.enabled)
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

    pub(crate) fn start_window_open_animation(&mut self, surface: &WlSurface) {
        self.closing_window_animations
            .retain(|closing| closing.surface != *surface);
        let animations = self.config.animations.clone();
        if !animations.enabled || !animations.open.enabled {
            self.window_open_animations.remove(surface);
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

    fn window_lifecycle_offset(
        &self,
        surface: &WlSurface,
        config: &crate::config::WindowAnimationConfig,
    ) -> Option<Point<f64, Logical>> {
        let window = self.mapped_toplevel_window(surface)?;
        let output = self.output_for_window(&window)?;
        let output_rect = self.space.output_geometry(&output)?;
        let window_rect = self
            .tiled_rect_for_surface(surface)
            .or_else(|| self.floating_workspace.get(surface).map(|tag| tag.rect))
            .or_else(|| self.space.element_geometry(&window))?;
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
            (old_rect.loc.x - new_rect.loc.x) as f64 + current_offset.x,
            (old_rect.loc.y - new_rect.loc.y) as f64 + current_offset.y,
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
        self.request_redraw();
    }

    /// Transfers the last drawable surface snapshot into a short close
    /// animation. Smithay's null-buffer commit still proceeds normally.
    pub(crate) fn start_window_close_animation(&mut self, surface: &WlSurface) {
        let animations = self.config.animations.clone();
        if !animations.enabled || !animations.close.enabled {
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
        self.window_open_animations.remove(surface);
        self.window_move_animations.remove(surface);
        self.window_viscosity.remove(surface);
        self.closing_window_animations
            .retain(|closing| closing.surface != *surface);
        self.closing_window_animations
            .push(crate::window_animation::ClosingWindowAnimation::new(
                surface.clone(),
                snapshot,
                crate::window_animation::WindowVisualAnimation::close(
                    &animations.close,
                    animations.slowdown,
                    offset,
                ),
            ));
        self.request_redraw();
    }

    pub(crate) fn closing_window_frame_elements(
        &mut self,
        _renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<crate::backend::udev::OutputRenderElements> {
        let output_scale = output.current_scale().fractional_scale();
        let scale = Scale::from(output_scale);
        let mut result = Vec::new();
        for closing in self
            .closing_window_animations
            .iter_mut()
            .filter(|closing| closing.snapshot.output == output.name())
        {
            result.extend(
                closing
                    .frame_elements(scale)
                    .into_iter()
                    .map(crate::backend::udev::OutputRenderElements::WindowSnapshot),
            );
        }
        result
    }

    pub(crate) fn desktop_render_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        skip: &[WlSurface],
    ) -> Option<Vec<crate::backend::udev::OutputRenderElements>> {
        self.space.output_geometry(output)?;
        let mut result = Vec::new();
        let shadow_program = self
            .shadows_possible()
            .then(|| crate::shadow::shadow_program(&mut self.shadow_program, renderer))
            .flatten();
        let rounded_program = self
            .rounding_possible()
            .then(|| {
                crate::decoration::rounded_surface_program(
                    &mut self.rounded_surface_program,
                    renderer,
                )
            })
            .flatten();
        let border_program = self
            .borders_possible()
            .then(|| crate::decoration::border_program(&mut self.border_program, renderer))
            .flatten();
        #[allow(clippy::too_many_arguments)]
        fn append_windows(
            state: &mut Smallvil,
            renderer: &mut GlesRenderer,
            output: &Output,
            fullscreen: bool,
            skip: &[WlSurface],
            shadow_program: Option<&GlesPixelProgram>,
            rounded_program: Option<&GlesTexProgram>,
            border_program: Option<&GlesPixelProgram>,
            result: &mut Vec<crate::backend::udev::OutputRenderElements>,
        ) {
            let Some(output_geo) = state.space.output_geometry(output) else {
                return;
            };
            let output_scale = output.current_scale().fractional_scale();
            let scale = Scale::from(output_scale);
            let windows: Vec<_> = state
                .space
                .elements_for_output(output)
                .rev()
                .cloned()
                .collect();
            for window in &windows {
                let surface = window.toplevel().map(|toplevel| toplevel.wl_surface());
                if surface.is_some_and(|surface| state.fullscreen.contains_key(surface))
                    != fullscreen
                {
                    continue;
                }
                if surface.is_some_and(|surface| skip.contains(surface)) {
                    continue;
                }
                let Some(location) = state.space.element_location(window) else {
                    continue;
                };
                let visual = surface
                    .map(|surface| state.window_visual_sample(surface))
                    .unwrap_or_default();
                let alpha = surface
                    .map(|surface| state.window_render_alpha(surface))
                    .unwrap_or(1.0)
                    * surface
                        .map(|surface| state.depth_live_alpha(surface))
                        .unwrap_or(1.0)
                    * visual.opacity;
                let visual_offset: Point<i32, Logical> = Point::from((
                    visual.offset.x.round() as i32,
                    visual.offset.y.round() as i32,
                ));
                let visual_size = visual.rounded_size_or(window.geometry().size);
                let render_location =
                    location - output_geo.loc - window.geometry().loc + visual_offset;
                if let Some(surface) = surface {
                    let (popups, main) = state.window_surface_elements(
                        renderer,
                        output,
                        window,
                        surface,
                        render_location,
                        visual_size,
                        alpha,
                        rounded_program.cloned(),
                    );
                    result.extend(popups);
                    if let Some(program) = border_program {
                        if let Some(border) = state.window_border_element(
                            output,
                            window,
                            surface,
                            program.clone(),
                            visual,
                        ) {
                            result.push(crate::backend::udev::OutputRenderElements::Border(border));
                        }
                    }
                    result.extend(main);
                } else {
                    result.extend(
                        window
                            .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                                renderer,
                                render_location.to_physical_precise_round(output_scale),
                                scale,
                                alpha,
                            )
                            .into_iter()
                            .map(SpaceRenderElements::Surface)
                            .map(crate::backend::udev::OutputRenderElements::Space),
                    );
                }
                if let (Some(surface), Some(program)) = (surface, shadow_program) {
                    if let Some(shadow) = state.window_shadow_element(
                        output,
                        window,
                        surface,
                        program.clone(),
                        visual,
                    ) {
                        result.push(crate::backend::udev::OutputRenderElements::Shadow(shadow));
                    }
                }
            }
        }

        fn append_layers(
            state: &Smallvil,
            renderer: &mut GlesRenderer,
            output: &Output,
            kinds: &[WlrLayer],
            result: &mut Vec<crate::backend::udev::OutputRenderElements>,
        ) {
            let output_scale = output.current_scale().fractional_scale();
            let scale = Scale::from(output_scale);
            let layer_map = layer_map_for_output(output);
            for kind in kinds {
                for layer in layer_map.layers_on(*kind).rev() {
                    if state.unmapped_layer_surfaces.contains(layer.wl_surface()) {
                        continue;
                    }
                    let Some(geometry) = layer_map.layer_geometry(layer) else {
                        continue;
                    };
                    result.extend(
                        layer
                            .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                                renderer,
                                geometry.loc.to_physical_precise_round(output_scale),
                                scale,
                                1.0,
                            )
                            .into_iter()
                            .map(SpaceRenderElements::Surface)
                            .map(crate::backend::udev::OutputRenderElements::Space),
                    );
                }
            }
        }

        append_windows(
            self,
            renderer,
            output,
            true,
            skip,
            shadow_program.as_ref(),
            rounded_program.as_ref(),
            border_program.as_ref(),
            &mut result,
        );
        append_layers(
            self,
            renderer,
            output,
            &[WlrLayer::Overlay, WlrLayer::Top],
            &mut result,
        );
        append_windows(
            self,
            renderer,
            output,
            false,
            skip,
            shadow_program.as_ref(),
            rounded_program.as_ref(),
            border_program.as_ref(),
            &mut result,
        );
        append_layers(
            self,
            renderer,
            output,
            &[WlrLayer::Bottom, WlrLayer::Background],
            &mut result,
        );
        Some(result)
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
        let mut changed = false;
        // Collect per-surface tier transitions while we still hold the
        // mutable borrow on `window_depths`, then emit after the loop --
        // `emit_ipc_event` needs `&mut self` and can't run mid-iteration.
        let mut tier_changes: Vec<(WlSurface, u8)> = Vec::new();
        for (surface, depth) in self.window_depths.iter_mut() {
            let old_tier = depth.tier();
            let local_changed = if enabled {
                depth.update(&self.config.depth)
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
        if changed {
            self.request_redraw();
        }
        for (surface, tier) in tier_changes {
            self.emit_ipc_event(crate::ipc::IpcEvent::DepthChanged { surface, tier });
        }
    }

    /// Broadcasts one state-change event to every matching IPC subscriber.
    /// The fast path (no subscribers connected) is a single `is_empty`
    /// check and return -- this is called from focus/workspace/map/unmap
    /// hot paths and must stay cheap when nothing is listening.
    ///
    /// For real subscribers, the per-event JSON line is snapshotted once
    /// (the borrow-checker-friendly shared-borrow path through
    /// `IpcEvent::to_json_line`), then appended to each subscriber's
    /// `pending` deque with an inline `try_flush` so a healthy subscriber
    /// gets sub-millisecond latency without waiting on the periodic timer.
    /// A subscriber whose `pending` exceeds `SUBSCRIBER_PENDING_CAP` after
    /// the attempt is retired via `remove_ipc_subscriber` -- bounded
    /// memory wins over best-effort delivery to a wedged client.
    pub(crate) fn emit_ipc_event(&mut self, event: crate::ipc::IpcEvent) {
        if self.ipc_subscribers.is_empty() {
            return;
        }
        let payload = event.to_json_line(self);
        let mut to_drop: Vec<usize> = Vec::new();
        for (id, sub) in self.ipc_subscribers.iter_mut() {
            if !crate::ipc::event_matches(&sub.filter, &event) {
                continue;
            }
            sub.pending.extend(&payload);
            if !sub.try_flush() {
                // Peer closed between events. Don't keep queuing; drop on
                // the next sweep unless `remove_ipc_subscriber` already
                // grabbed it (the read-side EOF watcher may have beaten us
                // to the entry). Marking here makes the next flush's
                // `remove_ipc_subscriber` retire the entry.
                to_drop.push(*id);
                continue;
            }
            if sub.pending.len() > crate::ipc::SUBSCRIBER_PENDING_CAP {
                tracing::warn!(
                    id,
                    pending = sub.pending.len(),
                    cap = crate::ipc::SUBSCRIBER_PENDING_CAP,
                    "IPC subscriber exceeded pending cap; dropping"
                );
                to_drop.push(*id);
            }
        }
        for id in to_drop {
            self.remove_ipc_subscriber(id);
        }
    }

    /// Periodic retry path for subscribers whose kernel write buffer was
    /// full at `emit_ipc_event` time. Registered as a recurring 16ms
    /// `Timer` in `ipc::init`. Also retires any subscriber flagged for
    /// removal mid-emit (peer-closed or cap-exceeded).
    pub(crate) fn flush_ipc_subscribers(&mut self) {
        if self.ipc_subscribers.is_empty() {
            return;
        }
        let mut to_drop: Vec<usize> = Vec::new();
        for (id, sub) in self.ipc_subscribers.iter_mut() {
            if !sub.try_flush() {
                to_drop.push(*id);
                continue;
            }
            if sub.pending.len() > crate::ipc::SUBSCRIBER_PENDING_CAP {
                to_drop.push(*id);
            }
        }
        for id in to_drop {
            self.remove_ipc_subscriber(id);
        }
    }

    /// Tears down a subscriber: removes its entry from `ipc_subscribers`
    /// (dropping the write-side FD) and retires its read-side EOF source
    /// via `loop_handle.remove`. Safe to call from inside any calloop
    /// callback, not just the EOF source itself -- calloop's
    /// `loop_handle.remove` is documented as idempotent against
    /// already-removed tokens, and cross-source removal from within
    /// another source's callback is the supported pattern (interior
    /// `RefCell` borrow, brief).
    fn remove_ipc_subscriber(&mut self, id: usize) {
        if let Some(mut sub) = self.ipc_subscribers.remove(&id) {
            if let Some(token) = sub.read_token.take() {
                self.loop_handle.remove(token);
            }
            drop(sub);
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
    ) -> (
        Vec<crate::backend::udev::OutputRenderElements>,
        Vec<WlSurface>,
    ) {
        if !self.config.water_effects || !self.config.depth.enabled {
            return (Vec::new(), Vec::new());
        }
        let Some(output_geo) = self.space.output_geometry(output) else {
            return (Vec::new(), Vec::new());
        };
        let visible: Vec<(WlSurface, Rectangle<i32, Logical>)> = self
            .space
            .elements_for_output(output)
            .rev()
            .filter_map(|window| {
                let surface = window.toplevel()?.wl_surface().clone();
                let mut rect = self.space.element_geometry(window)?;
                let visual = self.window_visual_sample(&surface);
                rect.loc += Point::from((
                    visual.offset.x.round() as i32,
                    visual.offset.y.round() as i32,
                ));
                rect.size = visual.rounded_size_or(rect.size);
                Some((
                    surface,
                    Rectangle::new(rect.loc - output_geo.loc, rect.size),
                ))
            })
            .collect();
        let program =
            crate::depth::depth_overlay_program(&mut self.depth_overlay_program, renderer);
        let mut elements = Vec::new();
        let mut replaced = Vec::new();

        for (surface, area) in visible {
            let Some(depth) = self.window_depths.get(&surface) else {
                continue;
            };
            let tier = depth.tier();
            let urgent = self.urgent.contains(&surface);

            if let Some(program) = program.as_ref().filter(|_| tier == 1 || urgent) {
                elements.push(crate::backend::udev::OutputRenderElements::DepthOverlay(
                    crate::depth::DepthOverlayElement::new(
                        depth,
                        area,
                        program.clone(),
                        &self.config.depth,
                        urgent,
                    ),
                ));
            }

            if tier < 2 {
                self.depth_schematics.remove(&surface);
                continue;
            }

            let title = crate::tab_strip::window_title(&surface);
            let size = (area.size.w.max(1), area.size.h.max(1));
            let rebuild = self
                .depth_schematics
                .get(&surface)
                .is_none_or(|schematic| !schematic.matches(size, &title, tier, &self.config.depth));
            if rebuild {
                self.depth_schematics.insert(
                    surface.clone(),
                    crate::depth::DepthSchematic::build(size, title, tier, &self.config.depth),
                );
            }
            let Some(element) = self.depth_schematics.get(&surface).and_then(|schematic| {
                schematic.render_element(renderer, (area.loc.x as f64, area.loc.y as f64))
            }) else {
                continue;
            };
            replaced.push(surface);
            elements.push(crate::backend::udev::OutputRenderElements::Composited(
                element,
            ));
        }

        (elements, replaced)
    }

    pub fn new(event_loop: &mut EventLoop<'static, Smallvil>, display: Display<Self>) -> Self {
        let start_time = std::time::Instant::now();
        let (config, startup_config_error, startup_config_warnings) = Config::load_with_error();
        // Copied out before `config` moves into the `Self { config, .. }`
        // field below, so `layout: ...` further down can still read it.
        let default_layout = config.default_layout;
        let master_orientation = config.master_orientation;
        let bsp_split_bias = config.bsp_split_bias;

        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let kde_decoration_state = KdeDecorationState::new::<Self>(
            &dh,
            smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode::Server,
        );
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let data_control_state =
            DataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), trusted_client);
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&dh, trusted_client);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let mut xdg_toplevel_icon_manager = XdgToplevelIconManager::new::<Self>(&dh);
        // Advertise the sizes TideWM-owned consumers are most likely to use.
        // Clients remain free to submit other sizes as required by the
        // protocol; these are preferences, not limits.
        xdg_toplevel_icon_manager.replace_icon_sizes([32, 48, 64, 128]);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&dh);
        let clock = Clock::<Monotonic>::new();
        let presentation_state = PresentationState::new::<Self>(&dh, clock.id() as u32);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let foreign_toplevel_list_state =
            ForeignToplevelListState::new_with_filter::<Self>(&dh, trusted_client);
        // The older wlr-foreign-toplevel-management-v1 protocol (hand-rolled,
        // no Smithay module). See `handlers/wlr_foreign_toplevel.rs`.
        let wlr_foreign_toplevel_state =
            crate::handlers::wlr_foreign_toplevel::WlrForeignToplevelState::new(&dh);
        let wlr_output_management_state =
            crate::handlers::wlr_output_management::WlrOutputManagementState::new(&dh);
        let wlr_output_power_management_state =
            crate::handlers::wlr_output_power_management::WlrOutputPowerManagementState::new(&dh);
        let wlr_gamma_control_state =
            crate::handlers::wlr_gamma_control::WlrGammaControlState::new(&dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        let pointer_gestures_state = PointerGesturesState::new::<Self>(&dh);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&dh);
        let text_input_manager_state = TextInputManagerState::new::<Self>(&dh);
        let input_method_manager_state =
            InputMethodManagerState::new::<Self, _>(&dh, trusted_client);
        let virtual_keyboard_manager_state =
            VirtualKeyboardManagerState::new::<Self, _>(&dh, trusted_client);
        let _security_context_state = SecurityContextState::new::<Self, _>(&dh, trusted_client);
        let popups = PopupManager::default();
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let image_capture_source_state = ImageCaptureSourceState::new();
        let output_capture_source_state =
            OutputCaptureSourceState::new_with_filter::<Self, _>(&dh, trusted_client);
        let toplevel_capture_source_state =
            ToplevelCaptureSourceState::new_with_filter::<Self, _>(&dh, trusted_client);
        let image_copy_capture_state =
            ImageCopyCaptureState::new_with_filter::<Self, _>(&dh, trusted_client);
        let wlr_screencopy_global = dh.create_global::<Self, ZwlrScreencopyManagerV1, ()>(3, ());

        // A seat is a group of keyboards, pointer and touch devices.
        // A seat typically has a pointer and maintains a keyboard focus and a pointer focus.
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        // Notify clients that we have a keyboard, for the sake of the example we assume that keyboard is always present.
        // You may want to track keyboard hot-plug in real compositor.
        //
        // A bad `xkb_layout`/`xkb_variant`/`xkb_options` in the user's
        // config must not take the whole compositor down with it -- fall
        // back to the default keymap (which always compiles) and log it,
        // rather than unwrapping a config-controlled `Result`.
        if seat
            .add_keyboard(
                config.input.xkb_config(),
                config.input.repeat_delay,
                config.input.repeat_rate,
            )
            .is_err()
        {
            tracing::error!(
                layout = %config.input.xkb_layout,
                variant = %config.input.xkb_variant,
                options = ?config.input.xkb_options,
                "Configured XKB keymap failed to compile, falling back to default layout"
            );
            seat.add_keyboard(
                XkbConfig::default(),
                config.input.repeat_delay,
                config.input.repeat_rate,
            )
            .expect("default XKB keymap must always compile");
        }

        // Notify clients that we have a pointer (mouse)
        // Here we assume that there is always pointer plugged in
        seat.add_pointer();

        // Advertise touch unconditionally too, matching pointer/keyboard
        // above and the same "just advertise it" precedent `PointerGesturesState`
        // already uses for a capability that may or may not have real
        // hardware behind it -- a client simply never gets touch events if
        // there's no touch panel, harmless either way.
        seat.add_touch();

        // A space represents a two-dimensional plane. Windows and Outputs can be mapped onto it.
        //
        // Windows get a position and stacking order through mapping.
        // Outputs become views of a part of the Space and can be rendered via Space::render_output.
        let space = Space::default();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        // Get the loop signal, used to stop the event loop
        let loop_handle = event_loop.handle();
        let loop_signal = event_loop.get_signal();

        let idle_inhibit_manager_state = IdleInhibitManagerState::new::<Self>(&dh);
        let idle_notifier_state = IdleNotifierState::<Self>::new(&dh, loop_handle.clone());

        Self {
            start_time,
            display_handle: dh,

            config,
            known_touchpads: Vec::new(),
            compositor_gesture: None,
            toast: None,
            config_error_overlay: startup_config_error
                .map(|message| {
                    crate::error_overlay::ConfigErrorOverlay::new(
                        message,
                        crate::error_overlay::OverlaySeverity::Error,
                    )
                })
                .or_else(|| {
                    (!startup_config_warnings.is_empty()).then(|| {
                        crate::error_overlay::ConfigErrorOverlay::new(
                            startup_config_warnings.join("; "),
                            crate::error_overlay::OverlaySeverity::Warning,
                        )
                    })
                }),
            builtin_wallpaper: crate::wallpaper::BuiltinWallpaper::build(),
            overview: None,
            #[cfg(feature = "screencast")]
            screencast_picker: None,
            welcome_hint: None,
            last_config_event: Instant::now() - Duration::from_secs(1),
            config_reload_timer_armed: false,
            last_pointer_motion: Instant::now(),
            cursor_idle_timer_armed: false,
            needs_redraw: true,
            active_submap: None,

            ipc_subscribers: HashMap::new(),
            next_ipc_subscriber_id: 1,

            layout: {
                let mut layout = Layouts::default();
                layout.set_default_algorithm(default_layout);
                layout.set_master_orientation(master_orientation);
                layout.set_split_bias(bsp_split_bias);
                layout
            },
            space,
            window_opacity: HashMap::new(),
            window_glass_modes: HashMap::new(),
            backdrop_textures: HashMap::new(),
            water_glass_program: None,
            frost_glass_program: None,
            shadow_program: None,
            rounded_surface_program: None,
            border_program: None,
            ripples: Vec::new(),
            ripple_program: None,
            window_open_animations: HashMap::new(),
            window_move_animations: HashMap::new(),
            window_viscosity: HashMap::new(),
            window_sway: HashMap::new(),
            window_frame_snapshots: HashMap::new(),
            closing_window_animations: Vec::new(),
            window_depths: HashMap::new(),
            depth_schematics: HashMap::new(),
            depth_overlay_program: None,
            depth_last_tick: Instant::now(),
            pending_workspace_transitions: HashMap::new(),
            workspace_transitions: HashMap::new(),
            workspace_transition_program: None,
            loop_handle,
            loop_signal,
            socket_name,

            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            kde_decoration_state,
            layer_shell_state,
            shm_state,
            output_manager_state,
            pointer_constraints_state,
            relative_pointer_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            data_control_state,
            session_lock_state,
            session_lock: SessionLock::Unlocked,
            session_lock_client: None,
            lock_surfaces: HashMap::new(),
            locked_outputs: HashSet::new(),
            lock_blank: HashMap::new(),
            xdg_activation_state,
            xdg_toplevel_icon_manager,
            single_pixel_buffer_state,
            presentation_state,
            clock,
            fractional_scale_manager_state,
            foreign_toplevel_list_state,
            foreign_toplevels: HashMap::new(),
            foreign_toplevel_numeric_ids: HashMap::new(),
            next_foreign_toplevel_numeric_id: 1,
            wlr_foreign_toplevel_state: Some(wlr_foreign_toplevel_state),
            wlr_output_management_state,
            wlr_output_power_management_state,
            set_output_power: None,
            wlr_gamma_control_state,
            gamma_size: None,
            set_gamma: None,
            wlr_foreign_toplevels: HashMap::new(),
            keyboard_shortcuts_inhibit_state,
            pointer_gestures_state,
            cursor_shape_manager_state,
            text_input_manager_state,
            input_method_manager_state,
            virtual_keyboard_manager_state,
            popups,
            unmapped_popup_surfaces: HashSet::new(),
            popup_grab: None,
            unmapped_toplevels: HashMap::new(),
            unmapped_layer_surfaces: HashSet::new(),
            viewporter_state,
            image_capture_source_state,
            output_capture_source_state,
            toplevel_capture_source_state,
            image_copy_capture_state,
            wlr_screencopy_global,
            capture_sessions: Vec::new(),
            pending_captures: Vec::new(),
            idle_inhibit_manager_state,
            idle_notifier_state,
            idle_inhibitors: HashMap::new(),
            seat,
            window_focus: None,
            on_demand_layer_focus: None,
            keyboard_focus: KeyboardFocusTarget::None,

            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            udev_renderer: None,
            session: None,
            cursor_status: CursorImageStatus::default_named(),
            cursor_theme: None,
            fullscreen: HashMap::new(),
            maximized: HashMap::new(),
            floating_workspace: HashMap::new(),
            pinned: HashSet::new(),
            scratchpad_previous: HashMap::new(),
            scratchpad_named: HashMap::new(),
            swallowed: HashMap::new(),
            workspace_previous: HashMap::new(),
            pseudo_tiled: HashSet::new(),
            urgent: HashSet::new(),
            focus_history: Vec::new(),
            cycling_focus: false,
            is_lid_closed: false,
            is_tablet_mode: false,
            groups: Vec::new(),
            next_ui_node_id: 1_000,
            #[cfg(feature = "screencast")]
            screencast: None,
            #[cfg(feature = "accessibility")]
            accessibility: None,
            #[cfg(feature = "accessibility")]
            accessibility_sync_timer_armed: false,
        }
    }

    fn init_wayland_listener(
        display: Display<Smallvil>,
        event_loop: &mut EventLoop<Smallvil>,
    ) -> OsString {
        // Creates a new listening socket, automatically choosing the next available `wayland` socket name.
        let listening_socket = ListeningSocketSource::new_auto().unwrap();

        // Get the name of the listening socket.
        // Clients will connect to this socket.
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();

        let (disconnect_sender, disconnect_source) =
            smithay::reexports::calloop::channel::channel();
        loop_handle
            .insert_source(disconnect_source, |event, _, state: &mut Smallvil| {
                if let smithay::reexports::calloop::channel::Event::Msg(client_id) = event {
                    state.handle_client_disconnect(client_id);
                }
            })
            .expect("Failed to init the client-disconnect event source.");

        let sender_for_clients = disconnect_sender.clone();
        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                // Inside the callback, you should insert the client into the display.
                //
                // You may also associate some data with the client when inserting the client.
                if let Err(err) = state.display_handle.insert_client(
                    client_stream,
                    Arc::new(ClientState {
                        compositor_state: CompositorClientState::default(),
                        security_context: None,
                        disconnect_sender: Some(sender_for_clients.clone()),
                    }),
                ) {
                    tracing::warn!(%err, "Failed to register incoming Wayland client");
                }
            })
            .expect("Failed to init the wayland event source.");

        // You also need to add the display itself to the event loop, so that client events will be processed by wayland-server.
        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: we don't drop the display
                    if let Err(err) = unsafe { display.get_mut().dispatch_clients(state) } {
                        tracing::error!(%err, "Wayland display dispatch failed; stopping cleanly");
                        state.loop_signal.stop();
                        return Ok(PostAction::Remove);
                    }
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    /// Finds the topmost surface under `pos`, checking the same manually
    /// interleaved order used by `desktop_render_elements`: fullscreen,
    /// Overlay/Top, regular windows, then Bottom/Background.
    /// Walk up through `surface`'s parent chain and return the first
    /// ancestor (including `surface` itself) that has a pointer constraint
    /// registered for `pointer`. Constraints are typically created on the
    /// xdg-toplevel's main surface, but `surface_under` may return a
    /// subsurface of that toplevel (a popup, a sub-surface the client uses
    /// for its own cursor, etc.) -- this resolves the disconnect.
    ///
    /// Returns `None` if no constraint is found anywhere in the chain.
    pub(crate) fn root_with_constraint(
        &self,
        surface: &WlSurface,
        pointer: &PointerHandle<Smallvil>,
    ) -> Option<WlSurface> {
        let mut current = Some(surface.clone());
        while let Some(s) = current.clone() {
            let mut found: Option<WlSurface> = None;
            with_pointer_constraint(&s, pointer, |c| {
                if c.is_some() {
                    found = Some(s.clone());
                }
            });
            if let Some(root) = found {
                return Some(root);
            }
            current = get_parent(&s);
        }
        None
    }

    /// Walk up through `surface`'s parent chain and return its root
    /// (the topmost ancestor with no parent). Used to compare two surfaces
    /// for "same window" without caring whether either is a subsurface.
    pub(crate) fn surface_root(&self, surface: &WlSurface) -> Option<WlSurface> {
        let mut current = surface.clone();
        loop {
            match get_parent(&current) {
                Some(p) => current = p,
                None => return Some(current),
            }
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.space.output_under(pos).next()?;
        let output_geo = self.space.output_geometry(output)?;

        // While locked, only the lock surface is hit-testable -- never the
        // windows/layers underneath, even if there is no lock surface yet
        // for this output (in which case there's simply nothing here).
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            let lock_surface = self.lock_surfaces.get(output)?;
            let output_local = pos - output_geo.loc.to_f64();
            return under_from_surface_tree(
                lock_surface.wl_surface(),
                output_local,
                (0, 0),
                WindowSurfaceType::ALL,
            )
            .map(|(s, p)| (s, p.to_f64() + output_geo.loc.to_f64()));
        }

        self.fullscreen_surface_under(output, pos)
            .or_else(|| {
                self.layer_surface_under(
                    output,
                    output_geo,
                    pos,
                    &[WlrLayer::Overlay, WlrLayer::Top],
                )
            })
            .or_else(|| {
                self.space
                    .element_under(pos)
                    .and_then(|(window, location)| {
                        window
                            .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                            .map(|(s, p)| (s, (p + location).to_f64()))
                    })
            })
            .or_else(|| {
                self.layer_surface_under(
                    output,
                    output_geo,
                    pos,
                    &[WlrLayer::Bottom, WlrLayer::Background],
                )
            })
    }

    fn fullscreen_surface_under(
        &self,
        output: &Output,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space
            .elements_for_output(output)
            .rev()
            .filter(|window| {
                window
                    .toplevel()
                    .is_some_and(|toplevel| self.fullscreen.contains_key(toplevel.wl_surface()))
            })
            .find_map(|window| {
                let location = self.space.element_location(window)?;
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, point)| (surface, (point + location).to_f64()))
            })
    }

    fn layer_surface_under(
        &self,
        output: &Output,
        output_geo: Rectangle<i32, Logical>,
        pos: Point<f64, Logical>,
        layers: &[WlrLayer],
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output_local = pos - output_geo.loc.to_f64();
        let map = layer_map_for_output(output);
        for kind in layers {
            for layer in map.layers_on(*kind).rev() {
                if self.unmapped_layer_surfaces.contains(layer.wl_surface()) {
                    continue;
                }
                let Some(layer_geo) = map.layer_geometry(layer) else {
                    continue;
                };
                if let Some((s, p)) = layer.surface_under(
                    output_local - layer_geo.loc.to_f64(),
                    WindowSurfaceType::ALL,
                ) {
                    return Some((
                        s,
                        p.to_f64() + layer_geo.loc.to_f64() + output_geo.loc.to_f64(),
                    ));
                }
            }
        }
        None
    }

    /// The layer surface (if any) whose bounds -- including its popups --
    /// contain `pos` at the frontmost rendered position. Resolving through
    /// `surface_under` is important: Bottom/Background layers must not steal
    /// input from a window rendered above them.
    pub(crate) fn layer_under_pointer(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<desktop::LayerSurface> {
        let output = self.space.output_under(pos).next()?;
        let (surface, _) = self.surface_under(pos)?;
        let map = layer_map_for_output(output);
        map.layer_for_surface(&surface, WindowSurfaceType::ALL)
            .filter(|layer| !self.unmapped_layer_surfaces.contains(layer.wl_surface()))
            .cloned()
    }

    /// The output containing `pos`, if any. Used to decide which output's
    /// tiling tree a click/drag/new-window action should target.
    pub(crate) fn output_for_point(&self, pos: Point<f64, Logical>) -> Option<Output> {
        self.space.output_under(pos).next().cloned()
    }

    /// The output containing the largest share of a window's current geometry.
    /// Output overlap storage inside Smithay is a HashMap, so taking its first
    /// entry made straddling-window ownership nondeterministic across runs.
    /// Ties use the stable output name. `None` if the window is not visible.
    pub(crate) fn output_for_window(&self, window: &Window) -> Option<Output> {
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

    /// The output a new action without any other spatial hint should
    /// target. Used for "which monitor does this land on" decisions (new
    /// windows, layer surfaces the client didn't pin to a specific output,
    /// workspace keybinds).
    ///
    /// Resolution order depends on `focus_follows_mouse` because the two
    /// settings describe the same underlying intent -- "where my attention
    /// is" -- and ought to agree on which monitor that is:
    ///
    ///   * **`focus_follows_mouse = true` (default):** pointer output
    ///     first. This is what makes a freshly-plugged second monitor get
    ///     new windows on the first `Super+Enter` after moving the mouse
    ///     over to it, even when that monitor has no windows yet for
    ///     `focus_follows_mouse` itself to shift keyboard focus onto.
    ///     Hyprland/i3/sway's "active monitor follows mouse" default is the
    ///     same idea. Suspended while an Exclusive/OnDemand layer owns the
    ///     keyboard (matching `focus_follows_mouse`'s own early-return in
    ///     that case): a launcher or lock screen shouldn't redirect spawns
    ///     to whatever output the pointer happens to have drifted onto
    ///     during the layer interaction -- the remembered window's output
    ///     is the better signal of pre-layer intent.
    ///
    ///   * **`focus_follows_mouse = false`:** focused window's output
    ///     first. In a click-to-focus model an unrelated pointer position
    ///     is a weaker signal of intent than whatever the user last
    ///     clicked, so the focused window wins.
    ///
    /// Either way the focused window's output, then the first mapped
    /// output, fill in the remaining fallbacks.
    pub(crate) fn primary_output(&self) -> Option<Output> {
        let intended_output = self.window_focus.as_ref().and_then(|surface| {
            self.layout
                .output_of(surface)
                .and_then(|name| self.output_by_name(name))
                .or_else(|| {
                    self.mapped_toplevel_window(surface)
                        .and_then(|window| self.output_for_window(&window))
                })
                .or_else(|| {
                    self.floating_workspace
                        .get(surface)
                        .and_then(|tag| self.output_by_name(&tag.output))
                })
        });
        let focused_output = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .and_then(|surface| {
                self.space
                    .elements()
                    .find(|w| is_window(w, &surface))
                    .and_then(|w| self.output_for_window(w))
            });
        let pointer_output = self
            .seat
            .get_pointer()
            .and_then(|p| self.output_for_point(p.current_location()));
        let first_output = || self.space.outputs().next().cloned();

        // Mirror `focus_follows_mouse`'s own runtime gate: pointer-driven
        // spawn is suspended while an Exclusive layer owns the keyboard, so
        // a launcher or lock screen can't redirect new windows to whatever
        // output the pointer drifted onto during the interaction.
        let prefer_pointer =
            self.config.input.focus_follows_mouse && self.exclusive_layer().is_none();

        if prefer_pointer {
            pointer_output
                .or(intended_output)
                .or(focused_output)
                .or_else(first_output)
        } else {
            intended_output
                .or(focused_output)
                .or(pointer_output)
                .or_else(first_output)
        }
    }

    /// Requests ordinary window focus. The request becomes the retained
    /// window intent, while `reconcile_keyboard_focus` may temporarily give
    /// actual keyboard focus to a mapped Exclusive layer instead.
    pub(crate) fn focus_window(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
    ) {
        self.focus_window_with_ripple(surface, serial, true);
    }

    /// Gives a freshly mapped window focus without also treating that
    /// lifecycle step as a user-visible focus handoff. The map ripple is the
    /// single visual cue for this transaction; later pointer/keyboard focus
    /// changes continue through `focus_window` and animate normally.
    pub(crate) fn focus_window_on_map(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
        animate_handoff: bool,
    ) {
        self.focus_window_with_ripple(surface, serial, animate_handoff);
    }

    fn focus_window_with_ripple(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
        animate_handoff: bool,
    ) {
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

    /// Requests focus for an OnDemand/Exclusive layer selected by first map
    /// or click. Exclusive priority itself is derived globally; only an
    /// OnDemand target must be remembered as explicit intent.
    pub(crate) fn focus_layer(&mut self, surface: WlSurface, serial: smithay::utils::Serial) {
        if self.layer_keyboard_interactivity(&surface) == Some(KeyboardInteractivity::OnDemand) {
            self.on_demand_layer_focus = Some(surface);
        }
        self.reconcile_keyboard_focus(serial);
    }

    /// Recomputes actual seat focus from mapped state and applies XDG
    /// Activated in the same transaction. Priority is mapped Exclusive
    /// layer, valid OnDemand intent, valid visible window intent, then none.
    pub(crate) fn reconcile_keyboard_focus(&mut self, serial: smithay::utils::Serial) {
        if self
            .window_focus
            .as_ref()
            .is_some_and(|surface| !self.window_is_visible(surface))
        {
            self.window_focus = None;
        }
        if self.on_demand_layer_focus.as_ref().is_some_and(|surface| {
            self.layer_keyboard_interactivity(surface) != Some(KeyboardInteractivity::OnDemand)
        }) {
            self.on_demand_layer_focus = None;
        }

        // Locked takes absolute priority over everything else -- no window
        // or layer may hold keyboard focus while the session is locked,
        // even if none of them changed their own state. `None` here (no
        // lock surface registered yet) intentionally clears keyboard focus
        // entirely rather than falling through to the normal chain below.
        let resolved = if !matches!(self.session_lock, SessionLock::Unlocked) {
            self.lock_focus_target()
                .map(KeyboardFocusTarget::Lock)
                .unwrap_or(KeyboardFocusTarget::None)
        } else {
            self.exclusive_layer()
                .map(|layer| KeyboardFocusTarget::Layer(layer.wl_surface().clone()))
                .or_else(|| {
                    self.on_demand_layer_focus
                        .clone()
                        .map(KeyboardFocusTarget::Layer)
                })
                .or_else(|| self.window_focus.clone().map(KeyboardFocusTarget::Window))
                .unwrap_or(KeyboardFocusTarget::None)
        };

        self.release_popup_grab_if_focus_leaves(&resolved);

        let popup_keyboard_focus = self.popup_grab.as_ref().and_then(|popup| {
            let owns_keyboard = popup.has_keyboard_grab
                && !popup.grab.has_ended()
                && resolved.surface() == Some(&popup.root);
            owns_keyboard.then(|| popup.grab.current_grab()).flatten()
        });
        let seat_target = popup_keyboard_focus.or_else(|| resolved.surface().cloned());

        let seat_surface = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        let focus_changed =
            resolved != self.keyboard_focus || seat_surface.as_ref() != seat_target.as_ref();

        let old_window = match &self.keyboard_focus {
            KeyboardFocusTarget::Window(surface) => Some(surface.clone()),
            _ => None,
        };
        let new_window = match &resolved {
            KeyboardFocusTarget::Window(surface) => Some(surface.clone()),
            _ => None,
        };

        // Assigned here, before the activation-change block below, so that
        // `refresh_wlr_toplevel_state`'s `is_window_activated` check (which
        // reads `self.keyboard_focus`) already sees the new focus target by
        // the time it runs for either surface -- otherwise the surface
        // losing focus would still read back as activated for one more
        // statement, since `self.keyboard_focus` wouldn't have moved yet.
        self.keyboard_focus = resolved;

        let mut activation_changed = false;
        if old_window != new_window {
            // Capture the focus target for the IPC event before the
            // `if let Some(surface) = new_window` block below moves it.
            let new_window_for_event = new_window.clone();
            if let Some(surface) = old_window {
                activation_changed |= self.set_window_activated(&surface, false);
                self.refresh_wlr_toplevel_state(&surface);
            }
            if let Some(surface) = new_window {
                activation_changed |= self.set_window_activated(&surface, true);
                self.refresh_wlr_toplevel_state(&surface);
                self.note_depth_attention(&surface);
                if self.urgent.remove(&surface) {
                    if let Some(depth) = self.window_depths.get_mut(&surface) {
                        depth.visual_changed();
                    }
                    self.emit_ipc_event(crate::ipc::IpcEvent::UrgentChanged {
                        surface: surface.clone(),
                        urgent: false,
                    });
                }
                if !self.cycling_focus {
                    self.focus_history.retain(|s| s != &surface);
                    self.focus_history.insert(0, surface);
                }
            }
            // Focus handoff between two real windows (or window -> none).
            // `reconcile_keyboard_focus` is called from many paths -- focus
            // authority, popup grab release, layer-surface interactivity
            // changes, workspace switches, window map -- all of which are
            // exactly the transitions a bar widget wants to know about.
            self.emit_ipc_event(crate::ipc::IpcEvent::FocusChanged {
                surface: new_window_for_event,
            });
        }

        if let Some(keyboard) = self.seat.get_keyboard() {
            if keyboard.current_focus() != seat_target {
                keyboard.set_focus(self, seat_target, serial);
            }
        }
        if focus_changed || activation_changed {
            self.request_redraw();
        }
    }

    /// Ends a popup grab when logical focus moves away from its root. The
    /// pointer grab is released from a calloop idle so this remains safe if a
    /// future focus path originates inside a pointer-grab callback.
    fn release_popup_grab_if_focus_leaves(&mut self, target: &KeyboardFocusTarget) {
        let leaving = self
            .popup_grab
            .as_ref()
            .is_some_and(|popup| popup.has_keyboard_grab && target.surface() != Some(&popup.root));
        if !leaving {
            return;
        }

        self.release_popup_grab();
    }

    fn release_popup_grab(&mut self) {
        let Some(mut popup) = self.popup_grab.take() else {
            return;
        };
        let grab_serial = popup.grab.serial();
        let previous_serial = popup.grab.previous_serial();
        popup.grab.ungrab(PopupUngrabStrategy::All);

        if let Some(keyboard) = self.seat.get_keyboard() {
            let owns_grab = keyboard.has_grab(grab_serial)
                || previous_serial.is_some_and(|serial| keyboard.has_grab(serial));
            if owns_grab {
                keyboard.unset_grab(self);
            }
        }

        let serial = SERIAL_COUNTER.next_serial();
        let time = self.start_time.elapsed().as_millis() as u32;
        self.loop_handle.insert_idle(move |state| {
            let Some(pointer) = state.seat.get_pointer() else {
                return;
            };
            let owns_grab = pointer.has_grab(grab_serial)
                || previous_serial.is_some_and(|serial| pointer.has_grab(serial));
            if owns_grab {
                pointer.unset_grab(state, serial, time);
            }
        });
    }

    fn release_popup_grab_for_root(&mut self, root: &WlSurface) {
        if self
            .popup_grab
            .as_ref()
            .is_some_and(|popup| &popup.root == root)
        {
            self.release_popup_grab();
        }
    }

    /// Removes a bufferless popup from the active grab without discarding a
    /// still-mapped parent menu. A null-buffer commit keeps the xdg_popup
    /// role alive, so Smithay's resource-lifetime cleanup cannot perform this
    /// transition for us. Only the topmost popup can be removed from a valid
    /// nested grab; if a client unmaps a non-topmost ancestor, dismiss the
    /// whole chain rather than retain focus on an invisible hierarchy.
    pub(crate) fn unmap_popup_grab(&mut self, surface: &WlSurface, root: Option<&WlSurface>) {
        if root.is_none()
            || self
                .popup_grab
                .as_ref()
                .is_none_or(|popup| Some(&popup.root) != root)
        {
            return;
        }
        let Some(mut popup) = self.popup_grab.take() else {
            return;
        };
        let topmost = popup.grab.current_grab();
        let strategy = if topmost.as_ref() == Some(surface) {
            PopupUngrabStrategy::Topmost
        } else {
            PopupUngrabStrategy::All
        };
        popup.grab.ungrab(strategy);
        self.popup_grab = Some(popup);
        self.refresh_popup_grab();
        self.reconcile_keyboard_focus(SERIAL_COUNTER.next_serial());
    }

    /// Drops completed popup-grab bookkeeping after `PopupManager::cleanup`
    /// and restores both input devices to the centralized root focus.
    pub(crate) fn refresh_popup_grab(&mut self) {
        let ended = self
            .popup_grab
            .as_ref()
            .is_some_and(|popup| popup.grab.has_ended());
        if !ended {
            return;
        }

        let popup = self.popup_grab.take().unwrap();
        let grab_serial = popup.grab.serial();
        let previous_serial = popup.grab.previous_serial();
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.start_time.elapsed().as_millis() as u32;
        if let Some(pointer) = self.seat.get_pointer() {
            let owns_grab = pointer.has_grab(grab_serial)
                || previous_serial.is_some_and(|candidate| pointer.has_grab(candidate));
            if owns_grab {
                pointer.unset_grab(self, serial, time);
            }
        }
        self.reconcile_keyboard_focus(serial);
    }

    /// Returns whether a popup rooted at `root` may grab and, if so, whether
    /// it should install a keyboard grab in addition to the pointer grab.
    /// Ordinary windows and keyboard-interactive layers must already own
    /// logical focus; a mapped `KeyboardInteractivity::None` layer is allowed
    /// a pointer-only menu grab.
    pub(crate) fn popup_grab_policy(&self, root: &WlSurface) -> Option<bool> {
        if self.mapped_toplevel_window(root).is_some() {
            return matches!(&self.keyboard_focus, KeyboardFocusTarget::Window(surface) if surface == root)
                .then_some(true);
        }

        match self.layer_keyboard_interactivity(root)? {
            KeyboardInteractivity::None => Some(false),
            KeyboardInteractivity::OnDemand | KeyboardInteractivity::Exclusive => {
                matches!(&self.keyboard_focus, KeyboardFocusTarget::Layer(surface) if surface == root)
                    .then_some(true)
            }
        }
    }

    /// The ordinary window that owns logical WM focus. During an xdg-popup
    /// keyboard grab the real seat focus is a descendant popup surface, but
    /// compositor actions and IPC must continue to address its root window.
    pub(crate) fn focused_window_surface(&self) -> Option<WlSurface> {
        match &self.keyboard_focus {
            KeyboardFocusTarget::Window(surface) => Some(surface.clone()),
            KeyboardFocusTarget::None
            | KeyboardFocusTarget::Layer(_)
            | KeyboardFocusTarget::Lock(_) => None,
        }
    }

    /// The surface actually holding keyboard focus right now, whatever its
    /// role (window, layer, lock surface). `input.rs`'s shortcuts-inhibit
    /// gate needs the real focused surface -- an inhibitor registered by a
    /// VM client sits on its toplevel, and `focused_window_surface`'s
    /// window-only view would miss the layer case -- but can't match on
    /// the private `KeyboardFocusTarget` itself.
    pub(crate) fn keyboard_focused_surface(&self) -> Option<&WlSurface> {
        match &self.keyboard_focus {
            KeyboardFocusTarget::Window(surface)
            | KeyboardFocusTarget::Layer(surface)
            | KeyboardFocusTarget::Lock(surface) => Some(surface),
            KeyboardFocusTarget::None => None,
        }
    }

    /// Which lock surface should own keyboard focus while locked: the
    /// pointer's current output's, falling back to any registered one --
    /// same focus-follows-mouse convention `focus_follows_mouse` already
    /// uses elsewhere. `None` while no lock surface has been registered for
    /// any output yet; `reconcile_keyboard_focus` then clears keyboard
    /// focus entirely rather than leaking it to a window.
    fn lock_focus_target(&self) -> Option<WlSurface> {
        let pointer_output = self.seat.get_pointer().and_then(|pointer| {
            self.space
                .output_under(pointer.current_location())
                .next()
                .cloned()
        });
        pointer_output
            .and_then(|output| self.lock_surfaces.get(&output))
            .or_else(|| self.lock_surfaces.values().next())
            .map(|lock_surface| lock_surface.wl_surface().clone())
    }

    /// Forces Smithay to re-resolve pointer enter/leave focus at the
    /// pointer's current location without an actual device motion --
    /// needed whenever `surface_under`'s answer changes out from under the
    /// pointer (locking, unlocking, a lock surface registering), since
    /// nothing else would otherwise trigger the re-resolution before the
    /// next real mouse move. A click with no preceding motion (a trackpad
    /// tap, most commonly) would otherwise still hit whatever was focused
    /// before the change.
    fn refresh_pointer_focus(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let pos = pointer.current_location();
        let under = self.surface_under(pos);
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.start_time.elapsed().as_millis() as u32;
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    /// The retained visible window underneath any layer focus. Used only for
    /// placement policy, not user actions against the currently focused
    /// client.
    pub(crate) fn intended_window_surface(&self) -> Option<WlSurface> {
        self.window_focus.clone()
    }

    /// Applies XDG Activated to one tracked role. Looking through the
    /// layout/floating registries (via `mapped_toplevel_window`) keeps this
    /// correct for a protocol-mapped window on an inactive workspace without
    /// turning every animated layer commit into an O(window-count) sweep.
    fn set_window_activated(&mut self, surface: &WlSurface, activated: bool) -> bool {
        let mapped = !self.unmapped_toplevels.contains_key(surface);
        let window = self
            .unmapped_toplevels
            .get(surface)
            .cloned()
            .or_else(|| self.mapped_toplevel_window(surface));
        let Some(window) = window else { return false };
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        let changed = window.set_activated(activated && mapped);
        if changed && mapped && toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
        changed
    }

    /// Whether `surface` currently holds keyboard focus. `KeyboardFocusTarget`
    /// is private to this module, so this is the query surface for callers
    /// elsewhere (`handlers/xdg_shell.rs`'s wlr-foreign-toplevel state
    /// mirroring) that need it without exposing the enum itself.
    pub(crate) fn is_window_activated(&self, surface: &WlSurface) -> bool {
        matches!(&self.keyboard_focus, KeyboardFocusTarget::Window(s) if s == surface)
    }

    pub(crate) fn window_is_visible(&self, surface: &WlSurface) -> bool {
        self.space
            .elements()
            .any(|window| is_window(window, surface))
    }

    /// Commits a visible floating window's live Space placement back into
    /// its durable ownership tag after an interactive move. In particular,
    /// crossing an output transfers it to that output's active workspace;
    /// otherwise later workspace actions would still treat it as belonging
    /// to the output where the drag began.
    pub(crate) fn sync_visible_floating_window(&mut self, window: &Window) {
        let Some(surface) = window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
        else {
            return;
        };
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
        // Falls back to `primary_output()` for a window dragged fully
        // outside every output's geometry, same fallback
        // `toggle_floating`'s own floating-to-tiled path already uses for
        // the identical ambiguity. Without it, `owner` stays `None` here
        // and `tag.output`/`tag.workspace` below would keep whatever
        // output the window *left* -- its `rect` still updates to the new
        // (off-screen) position, but a later workspace switch on that
        // stale output would still try to hide a window that isn't
        // actually there anymore.
        let owner = self
            .output_for_window(window)
            .or_else(|| self.primary_output())
            .map(|output| {
                // The window may have just been dragged onto a different
                // output; its surfaces need to hear about that output's scale.
                self.set_window_fractional_scale(window, &output);
                let name = output.name();
                let workspace = self.layout.active_workspace(&name);
                (name, workspace)
            });

        let tag = self.floating_workspace.get_mut(&surface).unwrap();
        tag.rect = rect;
        if let Some((output, workspace)) = owner {
            tag.output = output;
            tag.workspace = workspace;
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
            if self.window_is_visible(&surface) {
                let window = self
                    .floating_workspace
                    .get(&surface)
                    .unwrap()
                    .window
                    .clone();
                let Some(mut rect) = self.space.element_geometry(&window) else {
                    continue;
                };
                rect.loc += delta;
                self.space.map_element(window, rect.loc, false);
                self.floating_workspace.get_mut(&surface).unwrap().rect = rect;
            } else if let Some(tag) = self.floating_workspace.get_mut(&surface) {
                tag.rect.loc += delta;
            }
        }
    }

    fn layer_keyboard_interactivity(&self, surface: &WlSurface) -> Option<KeyboardInteractivity> {
        if self.unmapped_layer_surfaces.contains(surface) {
            return None;
        }
        self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .map(|layer| layer.cached_state().keyboard_interactivity)
        })
    }

    /// Selects a deterministic ordinary-window fallback after the current
    /// focus owner disappears. Pointer intent wins when focus-follows-mouse
    /// is enabled; then the topmost visible window on the preferred output,
    /// then the topmost visible window anywhere.
    pub(crate) fn repair_keyboard_focus(
        &mut self,
        preferred_output: Option<&str>,
        serial: smithay::utils::Serial,
    ) {
        if self
            .window_focus
            .as_ref()
            .is_some_and(|surface| !self.window_is_visible(surface))
        {
            self.window_focus = None;
        }
        if self.on_demand_layer_focus.as_ref().is_some_and(|surface| {
            self.layer_keyboard_interactivity(surface) != Some(KeyboardInteractivity::OnDemand)
        }) {
            self.on_demand_layer_focus = None;
        }

        if self.window_focus.is_none() && self.config.input.focus_follows_mouse {
            if let Some(pos) = self
                .seat
                .get_pointer()
                .map(|pointer| pointer.current_location())
            {
                if self.layer_under_pointer(pos).is_none() {
                    if let Some(surface) = self.window_at_layout_position(pos) {
                        self.window_focus = Some(surface);
                    }
                }
            }
        }

        if self.window_focus.is_none() {
            self.space.refresh();
            self.window_focus = preferred_output
                .and_then(|output_name| {
                    self.space
                        .elements()
                        .rev()
                        .find(|window| {
                            self.output_for_window(window)
                                .is_some_and(|output| output.name() == output_name)
                        })
                        .and_then(|window| {
                            window
                                .toplevel()
                                .map(|toplevel| toplevel.wl_surface().clone())
                        })
                })
                .or_else(|| {
                    self.space.elements().rev().find_map(|window| {
                        window
                            .toplevel()
                            .map(|toplevel| toplevel.wl_surface().clone())
                    })
                });
        }
        self.reconcile_keyboard_focus(serial);
    }

    /// Drops a destroyed/unmapped window from retained focus intent before
    /// choosing a fallback. If an Exclusive layer currently owns the seat,
    /// this still repairs the window that should be restored underneath it.
    pub(crate) fn forget_window_focus(&mut self, surface: &WlSurface) {
        self.release_popup_grab_for_root(surface);
        if self.window_focus.as_ref() == Some(surface) {
            self.window_focus = None;
        }
    }

    /// Drops an unmapped/destroyed OnDemand layer from retained focus
    /// intent. Exclusive focus is derived from mapped layer state and needs
    /// no separate bookkeeping here.
    pub(crate) fn forget_layer_focus(&mut self, surface: &WlSurface) {
        self.release_popup_grab_for_root(surface);
        if self.on_demand_layer_focus.as_ref() == Some(surface) {
            self.on_demand_layer_focus = None;
        }
    }

    fn window_at_layout_position(&self, pos: Point<f64, Logical>) -> Option<WlSurface> {
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

    /// Pushes `output`'s fractional scale to every surface in `window`'s
    /// tree that bound wp_fractional_scale. Cheap to call from placement
    /// paths: Smithay only emits the protocol event when the value
    /// actually changes.
    pub(crate) fn set_window_fractional_scale(&self, window: &Window, output: &Output) {
        let scale = output.current_scale().fractional_scale();
        window.with_surfaces(|_, states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }

    /// Same as `set_window_fractional_scale` for a layer-shell surface.
    /// Called once at map time -- a layer surface's output never changes
    /// afterwards, so there is nothing to refresh later.
    pub(crate) fn set_layer_fractional_scale(
        &self,
        layer: &desktop::LayerSurface,
        output: &Output,
    ) {
        let scale = output.current_scale().fractional_scale();
        layer.with_surfaces(|_, states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }

    /// Sends the frame-done callback to every mapped layer surface on
    /// `output`, the layer-shell equivalent of the per-window
    /// `window.send_frame(...)` loop each backend already runs. Without
    /// this, clients that throttle redraws on frame callbacks (most
    /// wlr-layer-shell clients do) never get their next one.
    pub fn send_layer_frames(&self, output: &Output, time: Duration) {
        for layer in layer_map_for_output(output).layers() {
            // A role remains registered in LayerMap across null-buffer
            // unmap so it can be arranged for a later fresh configure, but
            // it is not eligible for frame callbacks while protocol-
            // unmapped. Filtering also prevents a bufferless client from
            // driving a commit/callback redraw loop.
            if self.unmapped_layer_surfaces.contains(layer.wl_surface()) {
                continue;
            }
            layer.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    /// The lock-surface equivalent of `send_layer_frames`: a lock surface
    /// is a bare `wl_surface`, not a `desktop::Window`/`LayerSurface`
    /// wrapper, so it has no `.send_frame()` convenience of its own --
    /// `send_frames_surface_tree` is the primitive both of those wrap.
    /// Without this, a lock daemon that redraws on frame callbacks (an
    /// animated clock, an unlock-failure shake) would freeze after its
    /// first commit.
    pub fn send_lock_frames(&self, output: &Output, time: Duration) {
        if let Some(lock_surface) = self.lock_surfaces.get(output) {
            send_frames_surface_tree(
                lock_surface.wl_surface(),
                output,
                time,
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }

    /// Entry point for `SessionLockHandler::lock` (`handlers/mod.rs`):
    /// cancels whatever interactive pointer/popup grab was live (grabs
    /// bypass `surface_under`-based routing entirely, so locking alone
    /// would not stop an in-progress window drag), resolves keyboard/
    /// pointer focus onto the lock surface (or nothing yet, if none is
    /// registered), and requests a redraw so every output blanks
    /// immediately. The actual `SessionLocker::lock()` confirmation only
    /// fires once every output has rendered a locked frame -- see
    /// `mark_output_locked_frame`.
    pub(crate) fn lock_session(&mut self, confirmation: SessionLocker) {
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            // Another client is already locking/locked. Dropping
            // `confirmation` here auto-sends `finished` to the new client,
            // matching the protocol's own documented policy.
            return;
        }

        self.session_lock_client = self
            .display_handle
            .get_client(confirmation.ext_session_lock().id())
            .ok()
            .map(|client| client.id());

        #[cfg(feature = "screencast")]
        self.screencast_picker.take();

        // A transition texture contains the unlocked desktop. Release it
        // before the fail-closed lock render path can begin.
        self.pending_workspace_transitions.clear();
        self.workspace_transitions.clear();
        self.locked_outputs.clear();

        let serial = SERIAL_COUNTER.next_serial();
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.unset_grab(self, serial, self.start_time.elapsed().as_millis() as u32);
        }
        self.release_popup_grab();

        if self.space.outputs().next().is_none() {
            // No displays attached -- nothing to render a locked frame on,
            // so there is nothing to wait for either.
            confirmation.lock();
            self.session_lock = SessionLock::Locked;
        } else {
            self.session_lock = SessionLock::Locking(confirmation);
        }

        self.reconcile_keyboard_focus(serial);
        self.refresh_pointer_focus();
        self.request_redraw();
    }

    /// Entry point for `SessionLockHandler::unlock`.
    pub(crate) fn unlock_session(&mut self) {
        self.session_lock = SessionLock::Unlocked;
        self.session_lock_client = None;
        self.lock_surfaces.clear();
        self.locked_outputs.clear();
        let serial = SERIAL_COUNTER.next_serial();
        self.reconcile_keyboard_focus(serial);
        self.refresh_pointer_focus();
        self.request_redraw();
    }

    fn handle_client_disconnect(&mut self, client_id: ClientId) {
        if self.session_lock_client.as_ref() == Some(&client_id)
            && !matches!(self.session_lock, SessionLock::Unlocked)
        {
            // Fail closed: ending the compositor returns control to the
            // display/login manager. Continuing with an unlocked desktop
            // would make killing the locker a lock bypass.
            tracing::error!(
                "Session-lock client crashed; terminating the compositor session securely"
            );
            self.session_lock_client = None;
            self.loop_signal.stop();
        }
    }

    /// Entry point for `SessionLockHandler::new_surface`: configures the
    /// surface to fill `output` and registers it, then re-resolves focus --
    /// without this a lock surface registered after `lock_session` already
    /// ran (the common case: the client creates its lock surfaces only
    /// after seeing the manager global) would never receive
    /// `wl_keyboard.enter`, and password entry would be impossible.
    pub(crate) fn register_lock_surface(&mut self, output: Output, surface: LockSurface) {
        let Some(size) = self.space.output_geometry(&output).map(|geo| geo.size) else {
            return;
        };
        surface.with_pending_state(|state| {
            state.size = Some((size.w.max(0) as u32, size.h.max(0) as u32).into());
        });
        surface.send_configure();
        self.lock_surfaces.insert(output, surface);

        let serial = SERIAL_COUNTER.next_serial();
        self.reconcile_keyboard_focus(serial);
        self.refresh_pointer_focus();
        self.request_redraw();
    }

    /// Called by both backends' render loops right after successfully
    /// presenting a locked (blanked, or real lock-surface) frame on
    /// `output`. No-op once already `Locked`, or while `Unlocked`.
    pub(crate) fn mark_output_locked_frame(&mut self, output: &Output) {
        if !matches!(self.session_lock, SessionLock::Locking(_)) {
            return;
        }
        self.locked_outputs.insert(output.clone());
        self.try_confirm_lock();
    }

    fn try_confirm_lock(&mut self) {
        let SessionLock::Locking(_) = &self.session_lock else {
            return;
        };
        if !self
            .space
            .outputs()
            .all(|o| self.locked_outputs.contains(o))
        {
            return;
        }
        let SessionLock::Locking(confirmation) =
            std::mem::replace(&mut self.session_lock, SessionLock::Locked)
        else {
            unreachable!()
        };
        confirmation.lock();
    }

    /// Render elements for whatever should be visible on `output` while
    /// `session_lock` isn't `Unlocked`: the registered lock surface's own
    /// content, if there is one, in front of a full-output blank fill that
    /// guarantees nothing underneath is ever visible -- even before a lock
    /// surface exists, or if it doesn't cover the whole output. Elements
    /// are front-to-back (index 0 topmost, same convention `custom_elements`
    /// uses elsewhere in both backends), so the surface must come *before*
    /// the blank in the returned `Vec`, not after -- reversed once during
    /// development, which put the opaque blank in front and silently
    /// occluded every lock surface. Shared by both backends, same pattern
    /// as `tab_strip_elements`.
    pub(crate) fn lock_render_elements(
        &mut self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Vec<LockRenderElement<GlesRenderer>> {
        let Some(size) = self.space.output_geometry(output).map(|geo| geo.size) else {
            return Vec::new();
        };
        let scale = output.current_scale().fractional_scale();

        let mut elements = Vec::new();
        if let Some(lock_surface) = self.lock_surfaces.get(output) {
            let surface_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer,
                    lock_surface.wl_surface(),
                    (0, 0),
                    scale,
                    1.0,
                    Kind::Unspecified,
                );
            elements.extend(surface_elements.into_iter().map(LockRenderElement::Surface));
        }

        let blank_buffer = self
            .lock_blank
            .entry(output.clone())
            .or_insert_with(|| SolidColorBuffer::new((0, 0), [0.0, 0.0, 0.0, 1.0]));
        blank_buffer.update(size, [0.0, 0.0, 0.0, 1.0]);
        let blank = SolidColorRenderElement::from_buffer(
            blank_buffer,
            (0, 0),
            scale,
            1.0,
            Kind::Unspecified,
        );
        elements.push(LockRenderElement::Blank(blank));

        elements
    }

    /// Collects the wp_presentation feedback callbacks every visible
    /// window and layer surface on `output` registered since its last
    /// frame. The backend calls `presented` on the returned value once the
    /// frame carrying that content is actually on screen; dropping it
    /// without presenting discards every callback inside
    /// (`SurfacePresentationFeedback`'s own Drop), which is the correct
    /// answer for a frame that never reached the display. Modeled on
    /// anvil's helper of the same name.
    pub(crate) fn take_presentation_feedback(
        &self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);

        self.space.elements().for_each(|window| {
            if self.space.outputs_for_element(window).contains(output) {
                window.take_presentation_feedback(
                    &mut feedback,
                    surface_primary_scanout_output,
                    |surface, _| {
                        surface_presentation_feedback_flags_from_states(
                            surface,
                            None,
                            render_element_states,
                        )
                    },
                );
            }
        });

        let map = layer_map_for_output(output);
        for layer in map.layers() {
            if self.unmapped_layer_surfaces.contains(layer.wl_surface()) {
                continue;
            }
            layer.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        feedback
    }

    /// Marks that something changed and a frame needs to be composited.
    /// Backends should render only when this is true (or a toast is active)
    /// rather than redrawing every frame regardless of damage.
    pub fn request_redraw(&mut self) {
        self.needs_redraw = true;
        #[cfg(feature = "accessibility")]
        self.schedule_accessibility_sync();
    }

    /// Whether something is still mid-animation and needs another frame
    /// even though nothing else marked itself dirty in the meantime --
    /// today that's just a fading toast, but this is the one place a future
    /// water/decoration effect (ripple decay, workspace-transition
    /// progress) plugs into instead of both backends growing their own
    /// copy of this check, the way they used to for the toast alone.
    pub fn has_active_animation(&mut self) -> bool {
        // Completion cleanup must not depend on an output actually being
        // rendered. A DPMS-off/minimized output can skip its frame path;
        // pruning here still releases the full-output transition texture
        // on the event-loop tick that notices the animation ended.
        self.workspace_transitions
            .retain(|_, transition| !transition.finished());
        self.window_open_animations
            .retain(|_, animation| !animation.finished());
        self.window_move_animations
            .retain(|_, animation| !animation.finished());
        self.window_viscosity.retain(|_, motion| !motion.finished());
        self.window_sway.retain(|_, sway| !sway.finished());
        self.closing_window_animations
            .retain(|closing| !closing.animation.finished());
        self.toast
            .as_ref()
            .is_some_and(|toast| toast.needs_continued_redraw())
            || self.ripples.iter().any(|r| !r.finished())
            || !self.workspace_transitions.is_empty()
            || !self.window_open_animations.is_empty()
            || !self.window_move_animations.is_empty()
            || !self.window_viscosity.is_empty()
            || !self.window_sway.is_empty()
            || !self.closing_window_animations.is_empty()
            || self.animated_borders_possible()
    }

    #[cfg(feature = "accessibility")]
    fn schedule_accessibility_sync(&mut self) {
        const COALESCE: Duration = Duration::from_millis(50);
        if self.accessibility.is_none() || self.accessibility_sync_timer_armed {
            return;
        }
        self.accessibility_sync_timer_armed = true;
        let result = self.loop_handle.insert_source(
            Timer::from_duration(COALESCE),
            move |_, _, state: &mut Smallvil| {
                state.accessibility_sync_timer_armed = false;
                state.sync_accessibility_tree();
                TimeoutAction::Drop
            },
        );
        if let Err(err) = result {
            self.accessibility_sync_timer_armed = false;
            tracing::warn!(%err, "Failed to schedule accessibility tree update");
            self.sync_accessibility_tree();
        }
    }

    /// Publishes compositor-owned UI to AccessKit. Building a small owned
    /// snapshot here keeps the adapter's worker threads completely detached
    /// from Smithay and Wayland object lifetimes.
    #[cfg(feature = "accessibility")]
    pub(crate) fn sync_accessibility_tree(&mut self) {
        if self.accessibility.is_none() {
            return;
        }

        let locked = !matches!(self.session_lock, SessionLock::Unlocked);
        let primary = self.primary_output().map(|output| {
            let output_name = output.name();
            let workspace = self.layout.active_workspace(&output_name);
            (output_name, workspace)
        });
        let workspace_label = primary
            .as_ref()
            .map(|(_, workspace)| {
                self.config
                    .workspace_names
                    .iter()
                    .find_map(|(name, number)| (*number == *workspace).then(|| name.clone()))
                    .unwrap_or_else(|| workspace.to_string())
            })
            .unwrap_or_default();

        let workspace_name = |workspace: u32| {
            self.config
                .workspace_names
                .iter()
                .find_map(|(name, number)| (*number == workspace).then(|| name.clone()))
                .unwrap_or_else(|| workspace.to_string())
        };
        let overview_workspaces = self
            .overview
            .as_ref()
            .map(|overview| {
                overview
                    .workspaces()
                    .iter()
                    .map(|&workspace| (workspace, workspace_name(workspace)))
                    .collect()
            })
            .unwrap_or_default();

        let groups = primary
            .as_ref()
            .map(|(output_name, workspace)| {
                self.groups
                    .iter()
                    .filter(|group| &group.output == output_name && group.workspace == *workspace)
                    .map(|group| {
                        let id = group.ui_node_id;
                        crate::accessibility::GroupSnapshot {
                            id,
                            tabs: group
                                .members
                                .iter()
                                .map(|member| {
                                    (
                                        member.ui_node_id,
                                        crate::tab_strip::window_title(&member.surface),
                                    )
                                })
                                .collect(),
                            active: group.active,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let snapshot = crate::accessibility::UiSnapshot {
            workspace: workspace_label,
            locked,
            toast: self.toast.as_ref().map(|toast| {
                (
                    toast.message().to_owned(),
                    matches!(toast.kind(), ToastKind::Error),
                )
            }),
            config_error: self
                .config_error_overlay
                .as_ref()
                .map(|error| error.message().to_owned()),
            overview_workspaces,
            groups,
        };
        if let Some(accessibility) = self.accessibility.as_mut() {
            accessibility.update_ui(snapshot);
        }
    }

    /// Whether a frame was explicitly marked dirty. Timed toasts request
    /// their next frame from the backend after each successful render;
    /// treating every live toast as implicitly dirty here would make a
    /// persistent error toast redraw forever at the timer's full rate.
    /// Clears the flag as a side effect, since the caller is about to render.
    pub fn take_needs_redraw(&mut self) -> bool {
        let needs_redraw = self.needs_redraw;
        self.needs_redraw = false;
        needs_redraw
    }

    /// Whether the welcome-hint card should actually be drawn this frame:
    /// still enabled in config (`welcome_hint` built at all) and nothing
    /// mapped anywhere yet -- it gives way to the first real window the
    /// same way Hyprland's own hint does, rather than sitting on top of it.
    pub fn should_show_welcome_hint(&self) -> bool {
        self.welcome_hint.is_some() && self.space.elements().next().is_none()
    }

    /// The area tiled windows lay out into on `output`: the output's own
    /// geometry with any layer-shell exclusive zone (a bar's height, say)
    /// subtracted, translated from `non_exclusive_zone()`'s output-local
    /// coordinates into `space`-global ones. Not yet gap-inset -- callers
    /// that want a single rect filling this whole area (maximize) apply
    /// `layout::inset` themselves; `Layouts::layout` does it per-leaf.
    pub(crate) fn output_tiling_area(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let output_geo = self.space.output_geometry(output)?;
        let mut area = layer_map_for_output(output).non_exclusive_zone();
        if let Some(error) = &self.config_error_overlay {
            let reserved = error.reserved_height().min(area.size.h.max(0));
            area.loc.y += reserved;
            area.size.h -= reserved;
        }
        area.loc += output_geo.loc;
        Some(area)
    }

    /// Builds the persistent config-error render element at the top of the
    /// layer-shell non-exclusive zone. The matching space reservation lives
    /// in `output_tiling_area` above.
    pub(crate) fn config_error_element(
        &mut self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let mode = output.current_mode()?;
        let scale = output.current_scale().fractional_scale();
        let local_y = layer_map_for_output(output).non_exclusive_zone().loc.y;
        let logical_width = (mode.size.w as f64 / scale).round() as i32;
        self.config_error_overlay
            .as_mut()?
            .render_element(renderer, logical_width, local_y, scale)
    }

    pub(crate) fn wallpaper_element(
        &self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let mode = output.current_mode()?;
        self.builtin_wallpaper.render_element(
            renderer,
            mode.size,
            output.current_scale().fractional_scale(),
        )
    }

    /// The physical-space rect a floating `surface` currently occupies on
    /// `output`, or `None` if it's not visible there right now (hidden on
    /// a different workspace -- a hidden window isn't in `space.elements()`
    /// at all). Shared by backdrop capture and water-glass element
    /// placement so both agree on exactly the same rect the window itself
    /// renders at.
    pub(crate) fn floating_window_physical_rect(
        &self,
        surface: &WlSurface,
        output: &Output,
    ) -> Option<Rectangle<i32, Physical>> {
        let output_geo = self.space.output_geometry(output)?;
        let output_scale = output.current_scale().fractional_scale();
        let window = self.floating_workspace.get(surface)?.window.clone();
        let location = self.space.element_location(&window)?;
        let size = window.geometry().size;
        let logical_rect = Rectangle::new(location - output_geo.loc, size);
        Some(logical_rect.to_physical_precise_round(output_scale))
    }

    fn glass_mode_for_surface(&self, surface: &WlSurface) -> Option<crate::config::GlassMode> {
        let frost = self.frost_config_for_surface(surface);
        let opacity = self.window_render_alpha(surface);
        selected_glass_mode(
            self.window_glass_modes.get(surface).copied(),
            (opacity < 1.0).then_some(opacity),
            frost.enabled,
        )
    }

    pub(crate) fn window_render_alpha(&self, surface: &WlSurface) -> f32 {
        let focused = matches!(&self.keyboard_focus, KeyboardFocusTarget::Window(focused) if focused == surface);
        let fullscreen = self.fullscreen.contains_key(surface);
        self.window_opacity
            .get(surface)
            .copied()
            .map(|opacity| opacity.alpha(focused, fullscreen))
            .unwrap_or(1.0)
    }

    fn frost_config_for_surface(&self, surface: &WlSurface) -> crate::config::FrostConfig {
        let (app_id, title) = self.toplevel_identity(surface);
        let rule = self
            .config
            .resolve_window_rules(app_id.as_deref(), title.as_deref());
        rule.frost
            .as_ref()
            .map(|overrides| overrides.apply_to(&self.config.frost))
            .unwrap_or_else(|| self.config.frost.clone())
    }

    fn shadow_config_for_surface(&self, surface: &WlSurface) -> crate::config::ShadowConfig {
        let (app_id, title) = self.toplevel_identity(surface);
        let rule = self
            .config
            .resolve_window_rules(app_id.as_deref(), title.as_deref());
        rule.shadow
            .as_ref()
            .map(|overrides| overrides.apply_to(&self.config.shadow))
            .unwrap_or_else(|| self.config.shadow.clone())
    }

    fn rounding_config_for_surface(&self, surface: &WlSurface) -> crate::config::RoundingConfig {
        let (app_id, title) = self.toplevel_identity(surface);
        let rule = self
            .config
            .resolve_window_rules(app_id.as_deref(), title.as_deref());
        rule.rounding
            .as_ref()
            .map(|overrides| overrides.apply_to(&self.config.rounding))
            .unwrap_or_else(|| self.config.rounding.clone())
    }

    fn border_config_for_surface(&self, surface: &WlSurface) -> crate::config::BorderConfig {
        let (app_id, title) = self.toplevel_identity(surface);
        let rule = self
            .config
            .resolve_window_rules(app_id.as_deref(), title.as_deref());
        rule.border
            .as_ref()
            .map(|overrides| overrides.apply_to(&self.config.border))
            .unwrap_or_else(|| self.config.border.clone())
    }

    fn rounding_applies(
        &self,
        surface: &WlSurface,
        config: &crate::config::RoundingConfig,
    ) -> bool {
        config.enabled
            && config.clip
            && (!config.floating_only || self.floating_workspace.contains_key(surface))
            && (!self.fullscreen.contains_key(surface) || config.fullscreen)
            && config.radii.iter().any(|radius| *radius > 0.0)
    }

    fn rounding_possible(&self) -> bool {
        (self.config.rounding.enabled && self.config.rounding.clip)
            || self.config.window_rules.iter().any(|rule| {
                rule.rounding.as_ref().is_some_and(|rounding| {
                    rounding.enabled == Some(true) || rounding.clip == Some(true)
                })
            })
    }

    fn borders_possible(&self) -> bool {
        self.config.border.enabled
            || self.config.window_rules.iter().any(|rule| {
                rule.border
                    .as_ref()
                    .and_then(|border| border.enabled)
                    .unwrap_or(false)
            })
    }

    fn animated_borders_possible(&self) -> bool {
        self.space.elements().any(|window| {
            let Some(surface) = window.toplevel().map(|toplevel| toplevel.wl_surface()) else {
                return false;
            };
            let border = self.border_config_for_surface(surface);
            let focused = matches!(
                &self.keyboard_focus,
                KeyboardFocusTarget::Window(focused) if focused == surface
            );
            let urgent = self.urgent.contains(surface);
            let state_animated = if urgent {
                border.animate_urgent
            } else if focused {
                border.animate_focused
            } else {
                border.animate_inactive
            };
            border.enabled
                && border.width > 0.0
                && border.animate
                && state_animated
                && (focused || urgent || border.inactive_enabled)
                && (!border.floating_only || self.floating_workspace.contains_key(surface))
                && (!self.fullscreen.contains_key(surface) || border.fullscreen)
        })
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
        let clip = self.rounding_applies(surface, &rounding);
        let physical_rect = Some(
            Rectangle::new(render_location + window.geometry().loc, natural_size)
                .to_physical_precise_round(output_scale),
        );
        let radii = rounding.radii.map(|radius| radius * output_scale as f32);
        let raw_main = render_elements_from_surface_tree(
            renderer,
            surface,
            physical_location,
            scale,
            alpha,
            Kind::Unspecified,
        );
        if self.config.animations.enabled && self.config.animations.close.enabled {
            if let Some(snapshot) = crate::window_animation::WindowFrameSnapshot::capture(
                output.name(),
                physical_anchor,
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
            if let (true, Some(program), Some(geometry)) =
                (clip, rounded_program.clone(), physical_rect)
            {
                let rounded = crate::decoration::RoundedSurfaceElement::new(
                    element,
                    program,
                    geometry,
                    radii,
                    rounding.power,
                    rounding.antialias * output_scale as f32,
                    scale,
                );
                if resize_active {
                    main.push(
                        crate::backend::udev::OutputRenderElements::AnimatedRoundedSurface(
                            RescaleRenderElement::from_element(
                                rounded,
                                physical_anchor,
                                resize_scale,
                            ),
                        ),
                    );
                } else {
                    main.push(crate::backend::udev::OutputRenderElements::RoundedSurface(
                        rounded,
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

    fn window_border_element(
        &self,
        output: &Output,
        window: &Window,
        surface: &WlSurface,
        program: GlesPixelProgram,
        visual: crate::window_animation::VisualSample,
    ) -> Option<crate::decoration::BorderElement> {
        let mut border = self.border_config_for_surface(surface);
        border.opacity *= visual.opacity;
        border.inactive_opacity *= visual.opacity;
        border.urgent_opacity *= visual.opacity;
        let fullscreen = self.fullscreen.contains_key(surface);
        let focused = matches!(
            &self.keyboard_focus,
            KeyboardFocusTarget::Window(focused) if focused == surface
        );
        let urgent = self.urgent.contains(surface);
        if !border.enabled
            || border.width <= 0.0
            || (!focused && !urgent && !border.inactive_enabled)
            || (border.floating_only && !self.floating_workspace.contains_key(surface))
            || (fullscreen && !border.fullscreen)
        {
            return None;
        }
        let output_geo = self.space.output_geometry(output)?;
        let output_scale = output.current_scale().fractional_scale();
        let location = self.space.element_location(window)?;
        let visual_offset = Point::from((
            visual.offset.x.round() as i32,
            visual.offset.y.round() as i32,
        ));
        let visual_size = visual.rounded_size_or(window.geometry().size);
        let logical_rect = Rectangle::new(location - output_geo.loc + visual_offset, visual_size);
        let physical_rect = logical_rect.to_physical_precise_round(output_scale);
        let mut rounding = self.rounding_config_for_surface(surface);
        if !rounding.enabled {
            rounding.radii = [0.0; 4];
        }
        let id = smithay::backend::renderer::element::Id::from(surface);
        Some(crate::decoration::BorderElement::new(
            &id,
            physical_rect,
            output_scale,
            program,
            &rounding,
            &border,
            focused,
            urgent,
            self.start_time.elapsed().as_secs_f32(),
        ))
    }

    fn shadows_possible(&self) -> bool {
        self.config.shadow.enabled
            || self.config.window_rules.iter().any(|rule| {
                rule.shadow
                    .as_ref()
                    .and_then(|shadow| shadow.enabled)
                    .unwrap_or(false)
            })
    }

    fn window_shadow_element(
        &self,
        output: &Output,
        window: &Window,
        surface: &WlSurface,
        program: GlesPixelProgram,
        visual: crate::window_animation::VisualSample,
    ) -> Option<crate::shadow::ShadowElement> {
        let mut config = self.shadow_config_for_surface(surface);
        config.opacity *= visual.opacity;
        config.inactive_opacity *= visual.opacity;
        config.urgent_opacity *= visual.opacity;
        if config.corner_radius == 0.0 {
            let rounding = self.rounding_config_for_surface(surface);
            if rounding.enabled {
                config.corner_radius = rounding.radii[0];
            }
        }
        let fullscreen = self.fullscreen.contains_key(surface);
        if !config.enabled
            || (config.floating_only && !self.floating_workspace.contains_key(surface))
            || (fullscreen && !config.fullscreen)
        {
            return None;
        }

        let output_geo = self.space.output_geometry(output)?;
        let output_scale = output.current_scale().fractional_scale();
        let location = self.space.element_location(window)?;
        let visual_offset = Point::from((
            visual.offset.x.round() as i32,
            visual.offset.y.round() as i32,
        ));
        let visual_size = visual.rounded_size_or(window.geometry().size);
        let logical_rect = Rectangle::new(location - output_geo.loc + visual_offset, visual_size);
        let physical_rect = logical_rect.to_physical_precise_round(output_scale);
        let focused = matches!(
            &self.keyboard_focus,
            KeyboardFocusTarget::Window(focused) if focused == surface
        );
        let urgent = self.urgent.contains(surface);
        let id = crate::shadow::shadow_id(surface, &config, focused, urgent);
        Some(crate::shadow::ShadowElement::new(
            id,
            physical_rect,
            output_scale,
            program,
            config,
            focused,
            urgent,
        ))
    }

    /// Captures the backdrop behind every currently-visible floating window
    /// on `output` into `backdrop_textures` immediately before the visible
    /// output bind. Gated on `water_effects`, the master toggle for this
    /// whole roadmap. Water and frost glass sample the stored texture while
    /// building that same visible frame.
    ///
    /// Tiled windows are deliberately out of scope here: they tile edge to
    /// edge, so "what's behind" one is either another tile (visually
    /// uninteresting to refract) or nothing this pass has a reason to
    /// capture yet. Floating windows overlapping other content are the
    /// case this effect is actually for.
    pub(crate) fn capture_floating_backdrops(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) {
        if !self.config.water_effects {
            return;
        }

        let surfaces: Vec<WlSurface> = self
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == output.name()
                    && self
                        .window_depths
                        .get(*surface)
                        .is_none_or(|depth| depth.tier() < 2)
                    && self.glass_mode_for_surface(surface).is_some()
            })
            .map(|(surface, _)| surface.clone())
            .collect();

        let mut captured_first_backdrop = false;
        for surface in surfaces {
            let Some(mut physical_rect) = self.floating_window_physical_rect(&surface, output)
            else {
                // Hidden (different workspace) right now -- nothing to
                // capture until it's visible again.
                continue;
            };
            let visual = self.window_visual_sample(&surface);
            let visual_offset: Point<i32, Logical> = Point::from((
                visual.offset.x.round() as i32,
                visual.offset.y.round() as i32,
            ));
            if let Some(window) = self.mapped_toplevel_window(&surface) {
                let visual_size = visual.rounded_size_or(window.geometry().size);
                physical_rect.size = visual_size
                    .to_physical_precise_round(output.current_scale().fractional_scale());
            }
            physical_rect.loc +=
                visual_offset.to_physical_precise_round(output.current_scale().fractional_scale());

            let Some(space_elements) =
                self.desktop_render_elements(renderer, output, std::slice::from_ref(&surface))
            else {
                continue;
            };
            let behind: Vec<crate::backend::udev::OutputRenderElements> = self
                .wallpaper_element(output, renderer)
                .map(crate::backend::udev::OutputRenderElements::Composited)
                .into_iter()
                .chain(space_elements)
                .collect();

            let reusable = self
                .backdrop_textures
                .get(&surface)
                .map(|capture| capture.texture.clone());
            if let Some(texture) =
                crate::backdrop::capture_backdrop(renderer, physical_rect, behind, reusable)
            {
                let first_capture = !self.backdrop_textures.contains_key(&surface);
                let (id, mut commit) = match self.backdrop_textures.get(&surface) {
                    Some(existing) => (existing.id.clone(), existing.commit),
                    None => (
                        smithay::backend::renderer::element::Id::new(),
                        CommitCounter::default(),
                    ),
                };
                commit.increment();
                self.backdrop_textures.insert(
                    surface,
                    crate::backdrop::BackdropCapture {
                        texture,
                        id,
                        commit,
                    },
                );
                captured_first_backdrop |= first_capture;
            }
        }
        // The frame that triggered the first capture could otherwise be the
        // last dirty frame on a static desktop. Schedule exactly one more so
        // the newly available texture is actually consumed; later capture
        // replacements do not self-sustain an idle redraw loop.
        if captured_first_backdrop {
            self.request_redraw();
        }
    }

    /// Floating windows on `output` eligible for a captured glass layer this
    /// frame: `water_effects` on, either an explicit `glass` mode or the
    /// backward-compatible implicit trigger (`opacity` below 1.0 means
    /// water), and a backdrop already captured for them. Callers pass this
    /// list to `desktop_render_elements`'s `skip`
    /// so these windows are pulled out of their normal z-slot, then use
    /// `glass_frame_elements` to build what replaces them.
    pub(crate) fn glass_eligible_surfaces(&self, output: &Output) -> Vec<WlSurface> {
        if !self.config.water_effects {
            return Vec::new();
        }
        self.floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == output.name()
                    && self
                        .window_depths
                        .get(*surface)
                        .is_none_or(|depth| depth.tier() < 2)
                    && self.glass_mode_for_surface(surface).is_some()
                    && self.backdrop_textures.contains_key(*surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect()
    }

    /// Builds, for each of `surfaces` (from `glass_eligible_surfaces`),
    /// the window's own surface element immediately followed by its
    /// selected glass layer -- in that order so the window's real (semi-
    /// transparent) content draws on top of the treated backdrop. Meant to
    /// be prepended ahead of the rest of
    /// `desktop_render_elements`'s output (called with the same `surfaces`
    /// as `skip`), which puts every eligible window topmost among windows;
    /// see `water_glass.rs`'s module doc comment for why plain layering
    /// (not real multi-window z-order) is the deliberate scope for this
    /// first cut. Lazily compiles the shader on first call.
    pub(crate) fn glass_frame_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        surfaces: &[WlSurface],
    ) -> Vec<crate::backend::udev::OutputRenderElements> {
        let mut result = Vec::new();
        if surfaces.is_empty() {
            return result;
        }
        let needs_water = surfaces.iter().any(|surface| {
            self.window_glass_modes
                .get(surface)
                .is_none_or(|mode| *mode == crate::config::GlassMode::Water)
        });
        let needs_frost = surfaces.iter().any(|surface| {
            self.window_glass_modes.get(surface) == Some(&crate::config::GlassMode::Frost)
        });
        let water_program = needs_water
            .then(|| {
                crate::water_glass::water_glass_program(&mut self.water_glass_program, renderer)
            })
            .flatten();
        let frost_program = needs_frost
            .then(|| {
                crate::frost_glass::frost_glass_program(&mut self.frost_glass_program, renderer)
            })
            .flatten();
        let shadow_program = self
            .shadows_possible()
            .then(|| crate::shadow::shadow_program(&mut self.shadow_program, renderer))
            .flatten();
        let rounded_program = self
            .rounding_possible()
            .then(|| {
                crate::decoration::rounded_surface_program(
                    &mut self.rounded_surface_program,
                    renderer,
                )
            })
            .flatten();
        let border_program = self
            .borders_possible()
            .then(|| crate::decoration::border_program(&mut self.border_program, renderer))
            .flatten();
        let Some(output_geo) = self.space.output_geometry(output) else {
            return result;
        };
        for surface in surfaces {
            let Some((capture_id, capture_commit, capture_texture)) = self
                .backdrop_textures
                .get(surface)
                .map(|capture| (capture.id.clone(), capture.commit, capture.texture.clone()))
            else {
                continue;
            };
            let Some(mut physical_rect) = self.floating_window_physical_rect(surface, output)
            else {
                continue;
            };
            let Some(window) = self.mapped_toplevel_window(surface) else {
                continue;
            };
            let Some(location) = self.space.element_location(&window) else {
                continue;
            };
            let visual = self.window_visual_sample(surface);
            let visual_offset: Point<i32, Logical> = Point::from((
                visual.offset.x.round() as i32,
                visual.offset.y.round() as i32,
            ));
            let visual_size = visual.rounded_size_or(window.geometry().size);
            physical_rect.size =
                visual_size.to_physical_precise_round(output.current_scale().fractional_scale());
            physical_rect.loc +=
                visual_offset.to_physical_precise_round(output.current_scale().fractional_scale());
            let alpha =
                self.window_render_alpha(surface) * self.depth_live_alpha(surface) * visual.opacity;
            let render_location = location - output_geo.loc - window.geometry().loc + visual_offset;

            let (popups, main) = self.window_surface_elements(
                renderer,
                output,
                &window,
                surface,
                render_location,
                visual_size,
                alpha,
                rounded_program.clone(),
            );
            result.extend(popups);
            if let Some(program) = &border_program {
                if let Some(border) =
                    self.window_border_element(output, &window, surface, program.clone(), visual)
                {
                    result.push(crate::backend::udev::OutputRenderElements::Border(border));
                }
            }
            result.extend(main);
            match self.window_glass_modes.get(surface).copied() {
                Some(crate::config::GlassMode::Frost) => {
                    if let Some(program) = &frost_program {
                        let mut frost = self.frost_config_for_surface(surface);
                        frost.opacity *= visual.opacity;
                        let rounding = self.rounding_config_for_surface(surface);
                        let output_scale = output.current_scale().fractional_scale() as f32;
                        let (corner_radii, rounding_power, corner_softness) = if rounding.enabled {
                            (
                                rounding.radii.map(|radius| radius * output_scale),
                                rounding.power,
                                rounding.antialias * output_scale,
                            )
                        } else {
                            (
                                [frost.corner_radius * output_scale; 4],
                                2.0,
                                frost.corner_softness * output_scale,
                            )
                        };
                        result.push(crate::backend::udev::OutputRenderElements::FrostGlass(
                            crate::frost_glass::FrostGlassElement::new(
                                capture_id.clone(),
                                capture_commit,
                                capture_texture.clone(),
                                physical_rect,
                                program.clone(),
                                frost,
                                corner_radii,
                                rounding_power,
                                corner_softness,
                            ),
                        ));
                    }
                }
                Some(crate::config::GlassMode::Plain) => {}
                Some(crate::config::GlassMode::Water) | None => {
                    if let Some(program) = &water_program {
                        let rounding = self.rounding_config_for_surface(surface);
                        let corner_radii = if rounding.enabled {
                            rounding.radii.map(|radius| {
                                radius * output.current_scale().fractional_scale() as f32
                            })
                        } else {
                            [0.0; 4]
                        };
                        result.push(crate::backend::udev::OutputRenderElements::WaterGlass(
                            crate::water_glass::WaterGlassElement::new(
                                capture_id.clone(),
                                capture_commit,
                                capture_texture.clone(),
                                physical_rect,
                                program.clone(),
                                corner_radii,
                                rounding.power,
                                rounding.antialias
                                    * output.current_scale().fractional_scale() as f32,
                                visual.opacity,
                            ),
                        ));
                    }
                }
            }
            // Front-to-back ordering: the real client surface is first,
            // then its sampled glass backdrop, then the shadow immediately
            // behind both. This keeps translucent text/content above the
            // glass and prevents the shadow from becoming a full-window tint.
            if let Some(program) = &shadow_program {
                if let Some(shadow) =
                    self.window_shadow_element(output, &window, surface, program.clone(), visual)
                {
                    result.push(crate::backend::udev::OutputRenderElements::Shadow(shadow));
                }
            }
        }
        result
    }

    fn capture_workspace_desktop(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        geometry: Rectangle<i32, Physical>,
    ) -> Option<GlesTexture> {
        let glass_surfaces = self.glass_eligible_surfaces(output);
        let glass_elements = self.glass_frame_elements(renderer, output, &glass_surfaces);
        let (depth_elements, depth_surfaces) = self.depth_frame_elements(renderer, output);
        let mut skip = depth_surfaces;
        if !glass_elements.is_empty() {
            skip.extend(glass_surfaces.iter().cloned());
        }
        let space_elements = self.desktop_render_elements(renderer, output, &skip)?;
        let closing_windows = self.closing_window_frame_elements(renderer, output);
        let elements: Vec<crate::backend::udev::OutputRenderElements> = closing_windows
            .into_iter()
            .chain(depth_elements)
            .chain(glass_elements)
            .chain(space_elements)
            .chain(
                self.wallpaper_element(output, renderer)
                    .map(crate::backend::udev::OutputRenderElements::Composited),
            )
            .collect();
        crate::backdrop::capture_backdrop(renderer, geometry, elements, None)
    }

    /// Captures the currently-visible desktop for a queued workspace
    /// switch, applies the switch, optionally captures the incoming desktop
    /// for synchronized motion, then starts the transition.
    ///
    /// Both backends call this only after submitting the visible outgoing
    /// frame. Captures contain the desktop and wallpaper, while compositor
    /// chrome (cursor, toast, overview, tab strip) remains live above the
    /// transition. If the optional incoming capture fails, the effect falls
    /// back to the one-texture wipe. If the outgoing capture fails, the
    /// workspace still switches immediately rather than leaving input
    /// apparently ignored.
    pub(crate) fn capture_pending_workspace_transition(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) {
        let output_name = output.name();
        let Some(target) = self.pending_workspace_transitions.remove(&output_name) else {
            return;
        };
        let current = self.layout.active_workspace(&output_name);
        if current == target {
            return;
        }
        if !self.config.water_effects || !self.config.workspace_transition.enabled {
            self.apply_workspace_switch(output, current, target);
            return;
        }

        let direction = match self.config.workspace_transition.direction {
            crate::config::WorkspaceTransitionDirectionMode::Auto if target > current => {
                crate::workspace_transition::WorkspaceTransitionDirection::RightToLeft
            }
            crate::config::WorkspaceTransitionDirectionMode::Auto => {
                crate::workspace_transition::WorkspaceTransitionDirection::LeftToRight
            }
            crate::config::WorkspaceTransitionDirectionMode::LeftToRight => {
                crate::workspace_transition::WorkspaceTransitionDirection::LeftToRight
            }
            crate::config::WorkspaceTransitionDirectionMode::RightToLeft => {
                crate::workspace_transition::WorkspaceTransitionDirection::RightToLeft
            }
        };
        let geometry = output
            .current_mode()
            .map(|mode| Rectangle::from_size(mode.size));

        let outgoing_texture = geometry
            .and_then(|geometry| self.capture_workspace_desktop(renderer, output, geometry));
        let workspace_motion = self.config.workspace_transition.workspace_motion;

        self.apply_workspace_switch(output, current, target);

        let incoming_texture = if workspace_motion && outgoing_texture.is_some() {
            geometry.and_then(|geometry| self.capture_workspace_desktop(renderer, output, geometry))
        } else {
            None
        };

        if let (Some(outgoing_texture), Some(geometry)) = (outgoing_texture, geometry) {
            self.workspace_transitions.insert(
                output_name,
                crate::workspace_transition::WorkspaceTransition::new(
                    outgoing_texture,
                    incoming_texture,
                    direction,
                    geometry,
                    &self.config.workspace_transition,
                ),
            );
        }
    }

    /// Builds this output's active wipe element and eagerly evicts its
    /// texture once the animation has finished.
    pub(crate) fn workspace_transition_frame_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Option<crate::backend::udev::OutputRenderElements> {
        if !self.config.water_effects
            || !self.config.workspace_transition.enabled
            || !matches!(self.session_lock, SessionLock::Unlocked)
        {
            return None;
        }
        let output_name = output.name();
        if self
            .workspace_transitions
            .get(&output_name)
            .is_some_and(|transition| transition.finished())
        {
            self.workspace_transitions.remove(&output_name);
            return None;
        }
        if !self.workspace_transitions.contains_key(&output_name) {
            return None;
        }
        let program = crate::workspace_transition::workspace_transition_program(
            &mut self.workspace_transition_program,
            renderer,
        )?;
        self.workspace_transitions
            .get_mut(&output_name)
            .map(|transition| {
                crate::backend::udev::OutputRenderElements::WorkspaceTransition(
                    transition.frame_element(program),
                )
            })
    }

    /// Drops transient render state for a disconnected output. In
    /// particular, this releases a full-output texture immediately instead
    /// of retaining VRAM under an orphaned connector name.
    pub(crate) fn remove_workspace_transition_output(&mut self, output_name: &str) {
        self.pending_workspace_transitions.remove(output_name);
        self.workspace_transitions.remove(output_name);
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
        let (app_id, title) = self.toplevel_identity(surface);
        let rule = self
            .config
            .resolve_window_rules(app_id.as_deref(), title.as_deref());
        let mut cfg = self
            .config
            .resolve_ripple_config(rule.ripple.as_ref(), trigger);
        if cfg.enabled == Some(false) || !cfg.fires_on(trigger) {
            return;
        }

        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        let Some(output) = self.output_for_window(&window) else {
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
        let win = self
            .layout
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
                (!self.fullscreen.contains_key(surface) && !self.maximized.contains_key(surface))
                    .then(|| self.floating_workspace.get(surface).map(|tag| tag.rect))
                    .flatten()
            })
            .or_else(|| {
                self.space
                    .element_location(&window)
                    .map(|loc| Rectangle::new(loc, window.geometry().size))
            });
        let Some(win) = win else {
            return;
        };
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

    /// Clamps `pos` into the bounding box of every mapped output. Needed for
    /// relative pointer motion (a real mouse's delta accumulates onto the
    /// last known position with nothing else bounding it) -- absolute motion
    /// doesn't need this, it's already transformed against one output's
    /// geometry directly. Bounding-box, not per-output containment: an
    /// L-shaped multi-monitor arrangement could still let the cursor drift
    /// into a gap between two non-adjacent outputs. Not solved here; the
    /// single/extended-desktop case this fixes is unaffected.
    pub(crate) fn clamp_to_outputs(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        let mut min = Point::<f64, Logical>::from((f64::MAX, f64::MAX));
        let mut max = Point::<f64, Logical>::from((f64::MIN, f64::MIN));
        for output in self.space.outputs() {
            let Some(geo) = self.space.output_geometry(output) else {
                continue;
            };
            min.x = min.x.min(geo.loc.x as f64);
            min.y = min.y.min(geo.loc.y as f64);
            max.x = max.x.max((geo.loc.x + geo.size.w) as f64);
            max.y = max.y.max((geo.loc.y + geo.size.h) as f64);
        }
        if min.x > max.x || min.y > max.y {
            return pos;
        }
        (pos.x.clamp(min.x, max.x), pos.y.clamp(min.y, max.y)).into()
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
        self.retile_with_viscosity(false);
    }

    /// Interactive tiled resize/drop path. It shares every layout and
    /// protocol-state operation with `retile`, changing only the short-lived
    /// render follower chosen for geometry that moved under the pointer.
    pub(crate) fn retile_viscous(&mut self) {
        self.retile_with_viscosity(true);
    }

    fn retile_with_viscosity(&mut self, interactive: bool) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in &outputs {
            let Some(area) = self.output_tiling_area(output) else {
                continue;
            };
            let full_output_geo = self.space.output_geometry(output);
            let workspace = self.layout.active_workspace(&output.name());

            let gaps = self.gaps_for(&output.name(), workspace);
            for (window, mut rect) in self.layout.layout(&output.name(), workspace, area, gaps) {
                if let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) {
                    if let (Some(entry), Some(full)) =
                        (self.fullscreen.get(&surface), full_output_geo)
                    {
                        if entry.output == output.name() {
                            rect = full;
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
                    toplevel.send_pending_configure();
                }
                if let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) {
                    if let Some(old_location) = self.space.element_location(&window) {
                        let old_rect = Rectangle::new(old_location, window.geometry().size);
                        if old_rect != rect {
                            let viscous = (interactive
                                || self.window_viscosity.contains_key(&surface))
                                && self.retarget_window_viscosity(&surface, rect);
                            if !viscous {
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
                self.space.map_element(window, maximized_rect.loc, false);
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

        if self.layout.contains(surface) {
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
            // Tile onto whichever output the window is actually sitting on
            // right now (it was floating, so it has a real on-screen
            // position already), not wherever the focused window happens
            // to be. Falls back to `primary_output()` for the edge case of
            // a floating window dragged fully outside every output's
            // geometry -- previously (single-output, no such thing as
            // "outside the output") floating-to-tiled always succeeded, and
            // this keeps that true rather than silently no-oping.
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
        }

        self.retile();
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
    }

    /// Resolves a `"workspace:N"`/`"move-to-workspace:N"` target to a real
    /// workspace number: a `Number` passes through, a `Name` looks itself
    /// up in `config.workspace_names`. Warns and returns `None` for an
    /// unknown name, the same "bad input, log and no-op" convention an
    /// invalid numeric workspace used before names existed.
    pub(crate) fn resolve_workspace_ref(&self, r: &WorkspaceRef) -> Option<u32> {
        match r {
            WorkspaceRef::Number(n) => Some(*n),
            WorkspaceRef::Name(name) => {
                let resolved = self.config.workspace_names.get(name).copied();
                if resolved.is_none() {
                    tracing::warn!(name, "Unknown workspace name, ignoring");
                }
                resolved
            }
        }
    }

    /// Switches `output`'s visible workspace to `workspace`: hides
    /// everything currently on the active one (unmapped, not destroyed or
    /// untiled) and shows everything belonging to the new one. No-op if
    /// `workspace` is already active, or if an exclusive-interactivity layer
    /// (e.g. a lock screen) is mapped -- same guard `cycle_focus` uses, so a
    /// lock screen can't be escaped by switching workspaces out from under
    /// it. `self.pinned` windows are exempt from this whole cycle -- never
    /// hidden, never re-shown, since they're already visible regardless.
    /// This is also how the scratchpad works: it's just workspace
    /// `SCRATCHPAD_WORKSPACE` under the hood, switched to like any other.
    pub fn switch_workspace(&mut self, output: &Output, workspace: u32) {
        if self.exclusive_layer().is_some() {
            return;
        }
        let output_name = output.name();
        let current = self.layout.active_workspace(&output_name);
        // A second request before the first one's outgoing capture ran can
        // cancel it by selecting the still-visible workspace. Treating
        // this as auto-back-and-forth would instead jump somewhere the
        // user did not ask to see.
        if self
            .pending_workspace_transitions
            .contains_key(&output_name)
            && current == workspace
        {
            self.pending_workspace_transitions.remove(&output_name);
            self.request_redraw();
            return;
        }
        let workspace = if current != workspace {
            workspace
        } else if self.config.workspace_auto_back_and_forth {
            // niri's `workspace-auto-back-and-forth`: re-selecting the
            // already-active workspace jumps back to whichever one was
            // active immediately before it, instead of no-opping.
            match self.workspace_previous.get(&output_name) {
                Some(&previous) if previous != current => previous,
                _ => return,
            }
        } else {
            return;
        };

        if self.config.water_effects && self.config.workspace_transition.enabled {
            // A new switch supersedes any wipe already running on this
            // output. That keeps both latency and texture memory bounded:
            // never a queue, never more than one full-output capture.
            self.workspace_transitions.remove(&output_name);
            self.pending_workspace_transitions
                .insert(output_name, workspace);
            self.request_redraw();
            return;
        }

        self.apply_workspace_switch(output, current, workspace);
    }

    /// Immediate variant for protocol-driven activation: focus must land
    /// on the newly visible surface in the same dispatch, so this path
    /// cannot wait for the render loop's outgoing-frame capture.
    fn switch_workspace_immediate(&mut self, output: &Output, workspace: u32) {
        if self.exclusive_layer().is_some() {
            return;
        }
        let output_name = output.name();
        let current = self.layout.active_workspace(&output_name);
        if current == workspace {
            return;
        }
        self.pending_workspace_transitions.remove(&output_name);
        self.workspace_transitions.remove(&output_name);
        self.apply_workspace_switch(output, current, workspace);
    }

    fn apply_workspace_switch(&mut self, output: &Output, current: u32, workspace: u32) {
        let output_name = output.name();
        self.workspace_previous.insert(output_name.clone(), current);

        // Hide everything on the outgoing workspace. Tiled windows come
        // from the tree; floating ones from the tag, snapshotting each
        // one's latest position first so it comes back exactly where it was.
        for window in self.layout.windows_in(&output_name, current) {
            self.space.unmap_elem(&window);
        }
        // Pinned windows are exempt from this whole hide/show cycle -- they
        // stay mapped and visible regardless of which workspace tag they
        // happen to carry, so both loops below skip them entirely.
        let outgoing_floating: Vec<WlSurface> = self
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == output_name
                    && tag.workspace == current
                    && !self.pinned.contains(*surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in &outgoing_floating {
            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
                .cloned();
            let Some(window) = window else { continue };
            if !self.fullscreen.contains_key(surface) && !self.maximized.contains_key(surface) {
                if let Some(rect) = self.space.element_geometry(&window) {
                    if let Some(tag) = self.floating_workspace.get_mut(surface) {
                        tag.rect = rect;
                    }
                }
            }
            self.space.unmap_elem(&window);
        }

        self.layout.set_active_workspace(&output_name, workspace);

        // Show everything belonging to the new workspace. Tiled windows
        // come back through retile() below (it only ever walks the
        // *active* workspace's tree per output, so switching that above is
        // enough); floating ones need an explicit map at their saved rect,
        // using the `Window` handle the tag itself holds -- a hidden
        // floating window was `unmap_elem`'d, so it isn't in
        // `space.elements()` to look up anymore, unlike a tiled one (whose
        // `Window` always lives on in its `Layouts` tree regardless).
        let incoming_floating: Vec<(Window, Point<i32, Logical>)> = self
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == output_name
                    && tag.workspace == workspace
                    && !self.pinned.contains(*surface)
            })
            .map(|(_, tag)| (tag.window.clone(), tag.rect.loc))
            .collect();
        for (window, loc) in incoming_floating {
            self.space.map_element(window, loc, false);
        }

        self.retile();

        self.refocus_after_hide(&output_name);

        self.emit_ipc_event(crate::ipc::IpcEvent::WorkspaceChanged {
            output: output_name,
            from: current,
            to: workspace,
        });

        self.request_redraw();
    }

    /// Reassigns keyboard focus to the first mapped window on `output_name`,
    /// or clears focus if there is none -- `unmap_elem` never touches
    /// keyboard focus itself, so without this a window that was just hidden
    /// (workspace switch, or moved to another workspace) would stay
    /// "focused" while mapped nowhere. Shared by `switch_workspace` and
    /// `move_to_workspace`, whose "leaving the visible workspace" cases are
    /// otherwise identical.
    fn refocus_after_hide(&mut self, output_name: &str) {
        // `Space::outputs_for_element` (which `output_for_window` below
        // uses) reads a per-element cache that's normally kept fresh by
        // each backend's render loop calling `space.refresh()` once a
        // frame -- but that hasn't run yet since whatever `map_element`/
        // `unmap_elem` calls preceded this, so without an explicit refresh
        // here it would see stale cache data. Cheap and idempotent to call
        // an extra time (it no-ops unless something actually changed).
        self.space.refresh();

        let next_focus = self
            .space
            .elements()
            .rev()
            .find(|w| {
                self.output_for_window(w)
                    .is_some_and(|o| o.name() == output_name)
            })
            .and_then(|w| w.toplevel())
            .map(|t| t.wl_surface().clone());
        self.focus_window(next_focus, SERIAL_COUNTER.next_serial());
    }

    /// Moves `surface` to `workspace` on whichever output it's currently on.
    /// Stays on the current workspace rather than following (matching i3's
    /// default `Mod+Shift+N` behavior) -- the simpler of the two common
    /// conventions, easy to flip later if it feels wrong in practice.
    pub fn move_to_workspace(&mut self, surface: &WlSurface, workspace: u32) {
        let Some(output) = self.layout.output_of(surface).map(str::to_string) else {
            // Not tiled -- must be floating, or not tracked at all.
            let Some(tag) = self.floating_workspace.get(surface) else {
                return;
            };
            let output = tag.output.clone();
            // The window's *own* current workspace, not the output's active
            // one -- it may already be sitting hidden on some other
            // workspace (having been moved there previously), in which case
            // the active workspace tells us nothing about its visibility.
            let current = tag.workspace;
            if workspace == current {
                return;
            }
            let active = self.layout.active_workspace(&output);
            if let Some(tag) = self.floating_workspace.get_mut(surface) {
                tag.workspace = workspace;
            }

            // Pinned windows stay mapped and visible regardless of which
            // workspace they're nominally tagged with -- same exemption
            // `switch_workspace` applies, nothing to show or hide here.
            if self.pinned.contains(surface) {
                return;
            }

            if current == active && workspace != active {
                // Leaving the visible workspace: hide it.
                let window = self
                    .space
                    .elements()
                    .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
                    .cloned();
                if let Some(window) = window {
                    if !self.fullscreen.contains_key(surface)
                        && !self.maximized.contains_key(surface)
                    {
                        if let Some(rect) = self.space.element_geometry(&window) {
                            if let Some(tag) = self.floating_workspace.get_mut(surface) {
                                tag.rect = rect;
                            }
                        }
                    }
                    self.space.unmap_elem(&window);
                }
                self.refocus_after_hide(&output);
            } else if current != active && workspace == active {
                // Entering the visible workspace: show it at its saved rect.
                // (`current != active` is what tells this case apart from
                // "already visible, just retagging its number" -- comparing
                // only `workspace == active` would wrongly skip this too.)
                if let Some(tag) = self.floating_workspace.get(surface) {
                    self.space
                        .map_element(tag.window.clone(), tag.rect.loc, false);
                }
                // Fullscreen/maximized tags intentionally retain their
                // normal restore rectangle while hidden. Reconcile the
                // output-owned placement immediately instead of exposing
                // that normal location with a viewport-sized/stateful
                // buffer until some unrelated future retile.
                if self.fullscreen.contains_key(surface) || self.maximized.contains_key(surface) {
                    self.retile();
                }
            }
            self.request_redraw();
            return;
        };

        // Same distinction as the floating branch above: the window's own
        // current workspace, not the output's active one, since it may
        // already be hidden on some other workspace than whichever is
        // active right now.
        let current = self
            .layout
            .workspace_of(surface)
            .unwrap_or_else(|| self.layout.active_workspace(&output));
        if workspace == current {
            return;
        }
        let active = self.layout.active_workspace(&output);
        // Look the `Window` up through the tree, not `space.elements()`:
        // a tiled window already hidden on a non-active workspace isn't
        // mapped, so it wouldn't be found there, but the tree always holds
        // it regardless of visibility.
        let Some(window) = self.layout.window_of(surface) else {
            return;
        };
        self.layout.remove(surface);
        self.layout.insert(&output, workspace, window.clone(), None);

        if current == active {
            // Leaving the visible workspace: hide it, then retile so its
            // former neighbors on the active tree expand to fill the space
            // it left behind (the same reconciliation any other tiled-
            // window removal already triggers).
            self.space.unmap_elem(&window);
            self.retile();
            self.refocus_after_hide(&output);
        } else {
            // Either staying hidden (moving between two non-active
            // workspaces) or becoming visible (`workspace == active`) --
            // retile() maps anything now in the active tree on its own,
            // nothing else to do either way.
            self.retile();
        }
    }

    /// Shows/hides the scratchpad on `output`: if it's already showing,
    /// returns to whatever workspace was active before (so toggling back
    /// and forth doesn't strand you on some fixed fallback); otherwise
    /// remembers the current workspace and shows the scratchpad. The
    /// scratchpad has no structure of its own -- it's just workspace
    /// `SCRATCHPAD_WORKSPACE`, reusing `switch_workspace`'s hide/show
    /// mechanics as-is.
    /// `name` selects a named scratchpad; `None` is the classic unnamed
    /// one. Toggling scratchpad B while scratchpad A is showing switches
    /// straight to B (matching Hyprland's special-workspace behavior)
    /// without recording A as the workspace to return to -- only a real,
    /// non-scratchpad workspace ever lands in `scratchpad_previous`, so
    /// toggling off always returns to actual work.
    pub fn toggle_scratchpad(&mut self, output: &Output, name: Option<&str>) {
        let Some(target) = self.scratchpad_workspace(name) else {
            return;
        };
        let output_name = output.name();
        let current = self.layout.active_workspace(&output_name);
        if current == target {
            let previous = self
                .scratchpad_previous
                .get(&output_name)
                .copied()
                .unwrap_or(1);
            self.switch_workspace(output, previous);
        } else {
            if !is_scratchpad_workspace(current) {
                self.scratchpad_previous.insert(output_name, current);
            }
            self.switch_workspace(output, target);
        }
    }

    /// Moves `surface` to the (possibly named) scratchpad on whichever
    /// output it's currently on -- just `move_to_workspace` with the
    /// reserved scratchpad number, so it works for both tiled and floating
    /// windows exactly like moving to any other workspace does.
    pub fn move_to_scratchpad(&mut self, surface: &WlSurface, name: Option<&str>) {
        if let Some(workspace) = self.scratchpad_workspace(name) {
            self.move_to_workspace(surface, workspace);
        }
    }

    /// The effective tiling gap for (output, workspace): a per-workspace
    /// `workspace_gaps` override wins, then the output's own `gaps`, then
    /// the global `gaps`. Every layout-geometry call site routes through
    /// this so the three levels can't drift apart.
    pub(crate) fn gaps_for(&self, output_name: &str, workspace: u32) -> i32 {
        if let Some(&gaps) = self.config.workspace_gaps.get(&workspace) {
            return gaps;
        }
        self.config
            .outputs
            .iter()
            .find(|o| o.name == output_name)
            .and_then(|o| o.gaps)
            .unwrap_or(self.config.gaps)
    }

    /// The reserved workspace number for scratchpad `name`, allocating one
    /// on first use. `None` (the bare `toggle-scratchpad` action) is the
    /// classic unnamed scratchpad, workspace `SCRATCHPAD_WORKSPACE`.
    /// Returns `None` only if all 4096 named slots are somehow exhausted.
    fn scratchpad_workspace(&mut self, name: Option<&str>) -> Option<u32> {
        let Some(name) = name else {
            return Some(SCRATCHPAD_WORKSPACE);
        };
        if let Some(&workspace) = self.scratchpad_named.get(name) {
            return Some(workspace);
        }
        let next = NAMED_SCRATCHPAD_BASE.checked_add(self.scratchpad_named.len() as u32)?;
        self.scratchpad_named.insert(name.to_string(), next);
        Some(next)
    }

    /// The name of the named scratchpad `workspace` belongs to, if any.
    /// Used by the IPC `workspaces` query so bars can label (or hide)
    /// scratchpad entries instead of showing a raw reserved number.
    pub(crate) fn scratchpad_name_of(&self, workspace: u32) -> Option<&str> {
        self.scratchpad_named
            .iter()
            .find(|(_, &ws)| ws == workspace)
            .map(|(name, _)| name.as_str())
    }

    /// Toggles `surface` pinned: exempt from every workspace's hide/show
    /// cycle on its output, staying mapped and visible no matter which
    /// workspace is active. Un-tiles first if `surface` is currently
    /// tiled (reusing `toggle_floating`'s own tiled-to-floating path) --
    /// pinning only ever makes sense for a floating window, since only one
    /// workspace's tiling tree is ever rendered per output at a time.
    /// Unpinning doesn't re-tile it back; it just stays floating in place,
    /// matching Hyprland's own pin behavior.
    pub fn toggle_pin(&mut self, surface: &WlSurface) {
        if let Some(was_pinned) = self.fullscreen.get(surface).map(|entry| entry.was_pinned) {
            // Fullscreen temporarily suspends pinning. A toggle changes the
            // mode to restore on exit without making two placement owners
            // authoritative at once.
            let wants_pinned = !was_pinned;
            if wants_pinned {
                if self.layout.contains(surface) {
                    self.toggle_floating(surface);
                    if let Some(entry) = self.fullscreen.get_mut(surface) {
                        entry.pin_floated_it = true;
                    }
                }
            } else if self
                .fullscreen
                .get(surface)
                .is_some_and(|e| e.pin_floated_it)
            {
                // Undo specifically the float *this* pin toggle caused,
                // not a general "unpinning re-tiles" rule -- that's
                // deliberately not how the non-fullscreen case above
                // works (see this function's own doc comment: unpinning
                // normally leaves a window floating in place, matching
                // Hyprland). This differs because the whole pin-then-
                // unpin round trip happened without the window ever being
                // visibly floating (fullscreen was still covering it the
                // entire time) -- there's no "user saw it floating and
                // might want to keep it that way" moment to preserve, so
                // reverting the mechanical side effect back to how it
                // started is the least surprising outcome once fullscreen
                // ends. Only fires when this toggle is the one that
                // floated it; a window already floating before it got
                // pinned is left alone, same as the normal case.
                self.toggle_floating(surface);
            }
            if let Some(entry) = self.fullscreen.get_mut(surface) {
                entry.was_pinned = wants_pinned;
                if !wants_pinned {
                    entry.pin_floated_it = false;
                }
            }
            self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
                surface: surface.clone(),
            });
            self.request_redraw();
            return;
        }
        if self.pinned.remove(surface) {
            // While pinned, this window stayed mapped regardless of which
            // workspace was actually active -- so its FloatingTag may still
            // say whatever workspace was active back when it got pinned,
            // not wherever it visually is now. Without reconciling that
            // here, switch_workspace's hide loop would look for it on the
            // stale workspace and leave it stuck visible on every other
            // one until the user happens to pass through that specific
            // workspace number again.
            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
                .cloned();
            if let Some(output) = window.as_ref().and_then(|w| self.output_for_window(w)) {
                let active = self.layout.active_workspace(&output.name());
                if let Some(tag) = self.floating_workspace.get_mut(surface) {
                    tag.output = output.name();
                    tag.workspace = active;
                }
            }
            self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
                surface: surface.clone(),
            });
            self.request_redraw();
            return;
        }
        if self.layout.contains(surface) {
            self.toggle_floating(surface);
        }
        self.pinned.insert(surface.clone());
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
        self.request_redraw();
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

                if let Some(entry) = self.fullscreen.get_mut(&surface) {
                    entry.move_to_output(to_output.to_string(), delta);
                    if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, to_bounds) {
                        *rect = clamp_rect_visible(*rect, bounds);
                    }
                }
                if let Some(entry) = self.maximized.get_mut(&surface) {
                    entry.move_to_output(to_output.to_string(), delta);
                    if let Some(bounds) = to_bounds {
                        entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
                    }
                }

                if workspace == active_workspace {
                    if let Some(output) = &to_output_handle {
                        self.set_window_fractional_scale(&window, output);
                    }
                } else {
                    self.space.unmap_elem(&window);
                }
            }
        }

        // A group's parked members are deliberately absent from both Space
        // and Layouts, so the group's durable ownership tag must move along
        // with whichever active member was migrated above.
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

            if let Some(entry) = self.fullscreen.get_mut(&surface) {
                entry.move_to_output(to_output.to_string(), delta);
                if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, to_bounds) {
                    *rect = clamp_rect_visible(*rect, bounds);
                }
            }
            if let Some(entry) = self.maximized.get_mut(&surface) {
                entry.move_to_output(to_output.to_string(), delta);
                if let Some(bounds) = to_bounds {
                    entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
                }
            }

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
    }

    /// Runs the udev backend's real DRM power hook (if any -- `None` under
    /// winit, logical-only there) for `output`, then updates
    /// `wlr_output_power_management_state`'s own tracking/broadcast on
    /// success. The one place that does both together, shared by the
    /// protocol's own `SetMode` handler and `toggle_dpms` below.
    fn set_output_power_state(&mut self, output: &Output, on: bool) -> bool {
        let applied = match &mut self.set_output_power {
            Some(hook) => hook(output, on),
            None => true,
        };
        if applied {
            if !on {
                self.remove_workspace_transition_output(&output.name());
            }
            self.wlr_output_power_management_state.set(output, on);
        }
        applied
    }

    /// `toggle-dpms` (Phase N tier 2): both niri (`power-off-monitors`) and
    /// Hyprland (`dpms`) treat this as an ordinary bindable WM action, not
    /// external-tool-only, even though `wlr-output-power-management-v1`
    /// already exposes the same control to a client like `wlopm`. Toggles
    /// every output together rather than just one -- if any output is
    /// currently on, this turns all of them off, otherwise turns all back
    /// on, so a multi-monitor setup doesn't end up in a mixed on/off state
    /// from one bind press.
    pub fn toggle_dpms(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let any_on = outputs
            .iter()
            .any(|output| self.wlr_output_power_management_state.is_on(output));
        let target_on = !any_on;
        for output in &outputs {
            self.set_output_power_state(output, target_on);
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
        if !self.floating_workspace.contains_key(surface) {
            return;
        }
        let Some(current) = self.space.element_geometry(&window) else {
            return;
        };
        let loc = position.map(Point::from).unwrap_or(current.loc);
        let size = size.map(Size::from).unwrap_or(current.size);
        let rect = Rectangle::new(loc, size);

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = Some(rect.size);
            });
            toplevel.send_pending_configure();
        }
        self.space.map_element(window, rect.loc, false);
        if let Some(tag) = self.floating_workspace.get_mut(surface) {
            tag.rect = rect;
        }
    }

    /// Records real pointer motion and, if `cursor_hide_after_ms` is
    /// configured, makes sure a wake-up timer is running -- damage-gated
    /// rendering means nothing else would naturally trigger a redraw at the
    /// moment the idle threshold passes on an otherwise-static desktop, so
    /// the auto-hide needs its own explicit prod. Only arms a new timer if
    /// one isn't already pending (`cursor_idle_timer_armed`); a real mouse
    /// fires this on every motion event, so spawning a fresh calloop timer
    /// source each time would mean hundreds per second. The one live timer
    /// re-reads `last_pointer_motion` when it fires and reschedules itself
    /// further out (`TimeoutAction::ToDuration`, the same self-extending
    /// idiom `winit.rs`'s own redraw timer uses) if motion happened more
    /// recently than expected, rather than a fresh source per motion event.
    /// The timer only requests a redraw; the actual hide/show decision is
    /// re-derived fresh from `last_pointer_motion` at render time
    /// (`backend/udev.rs`), so a slightly-early/late fire can't itself
    /// cause an incorrect result. No-op under winit (`udev_renderer` is
    /// `None` there) -- only the udev backend composites its own cursor at
    /// all.
    pub(crate) fn note_pointer_motion(&mut self) {
        self.last_pointer_motion = Instant::now();
        if self.config.cursor_hide_after_ms <= 0 || self.udev_renderer.is_none() {
            return;
        }
        if self.cursor_idle_timer_armed {
            return;
        }
        self.cursor_idle_timer_armed = true;
        let delay = Duration::from_millis(self.config.cursor_hide_after_ms as u64);
        let result = self.loop_handle.insert_source(
            Timer::from_duration(delay),
            move |_, _, state: &mut Smallvil| {
                let configured =
                    Duration::from_millis(state.config.cursor_hide_after_ms.max(0) as u64);
                let elapsed = state.last_pointer_motion.elapsed();
                if elapsed >= configured {
                    state.cursor_idle_timer_armed = false;
                    state.request_redraw();
                    TimeoutAction::Drop
                } else {
                    TimeoutAction::ToDuration(configured - elapsed)
                }
            },
        );
        if let Err(err) = result {
            self.cursor_idle_timer_armed = false;
            tracing::warn!(%err, "Failed to register cursor idle timer");
        }
    }

    /// Raises `surface` to the top of the floating stack. No-op on a tiled
    /// window -- tiled windows never overlap by construction, so raising
    /// one has no meaning (same restriction `toggle_pseudo_tile` applies in
    /// reverse). Floating windows are already always rendered above the
    /// tiled layer (see the Z-order invariant `retile()` enforces); this
    /// only changes relative order *within* the floating stack.
    pub fn raise_window(&mut self, surface: &WlSurface) {
        if !self.floating_workspace.contains_key(surface) {
            return;
        }
        if let Some(window) = self.mapped_toplevel_window(surface) {
            self.space.raise_element(&window, false);
            self.request_redraw();
        }
    }

    /// Sends `surface` to the bottom of the floating stack, still above
    /// every tiled window. `Space` has no direct "lower" primitive, so this
    /// raises every *other* floating window instead, preserving their
    /// relative order -- the same trick `retile()` already uses to restore
    /// the floating-above-tiled invariant after a tiling pass, just with
    /// `surface` deliberately left out of the re-raise.
    pub fn lower_window(&mut self, surface: &WlSurface) {
        if !self.floating_workspace.contains_key(surface) {
            return;
        }
        let others: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                w.toplevel().is_some_and(|t| {
                    let s = t.wl_surface();
                    s != surface && self.floating_workspace.contains_key(s)
                })
            })
            .cloned()
            .collect();
        if others.is_empty() {
            return;
        }
        for window in others {
            self.space.raise_element(&window, false);
        }
        self.request_redraw();
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
    /// `Layouts` leaf, cycled between via `cycle_tab`. Reuses the same
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
        if !self.layout.contains(&focused)
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
        if !self.layout.contains(&neighbor_surface)
            || self.fullscreen.contains_key(&neighbor_surface)
            || self.pseudo_tiled.contains(&neighbor_surface)
        {
            return;
        }
        self.group_with(&focused, &neighbor_surface);
    }

    /// Merges `b` into `a`'s tiled slot as a new parked tab. Both must
    /// already be tiled (checked by `group_direction`); `b`'s former leaf
    /// collapses exactly like a normal close, since `Layouts::remove`
    /// removes it from the tree the same way either way. Merging two
    /// windows that are *both* already in (different) groups is out of
    /// scope for now -- no-op rather than picking a side to discard.
    pub fn group_with(&mut self, a: &WlSurface, b: &WlSurface) {
        if self.group_of(b).is_some() {
            tracing::debug!("group-with: target is itself already grouped, skipping");
            return;
        }
        let Some(b_window) = self.layout.window_of(b) else {
            return;
        };
        self.layout.remove(b);
        self.space.unmap_elem(&b_window);

        if let Some(idx) = self.group_of(a) {
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
            let Some(output) = self.layout.output_of(a).map(str::to_string) else {
                return;
            };
            let workspace = self
                .layout
                .workspace_of(a)
                .unwrap_or_else(|| self.layout.active_workspace(&output));
            self.groups.push(WindowGroup {
                ui_node_id: group_ui_node_id,
                output,
                workspace,
                members: vec![
                    GroupMember {
                        ui_node_id: a_ui_node_id,
                        surface: a.clone(),
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
        let active_window = was_active.then(|| self.layout.window_of(surface)).flatten();

        let (new_active, dissolves) =
            group_removal_outcome(self.groups[idx].members.len(), self.groups[idx].active, pos);
        let removed = self.groups[idx].members.remove(pos);
        let removed_window = active_window.or(removed.parked_window);

        if dissolves {
            let last_parked = self.groups[idx].members[0].parked_window.take();
            self.groups.remove(idx);
            if let Some(window) = last_parked {
                self.layout.replace_leaf(surface, &window);
            }
            self.retile();
            return removed_window;
        }

        self.groups[idx].active = new_active;
        self.groups[idx].strip = None;
        if was_active {
            if let Some(window) = self.groups[idx].members[new_active].parked_window.take() {
                self.layout.replace_leaf(surface, &window);
            }
        }
        self.retile();
        removed_window
    }

    /// Removes `surface` from its group, if grouped. The window keeps
    /// existing (unlike `leave_group_on_close`), so it becomes its own
    /// ordinary tile again -- splitting off from the last leaf in tree
    /// order (`BspLayout::insert`'s own fallback), since "which leaf
    /// currently has focus" isn't meaningfully derivable here: the group
    /// this came from may have just dissolved or promoted a different
    /// member into its old leaf.
    pub fn ungroup(&mut self, surface: &WlSurface) {
        let Some(idx) = self.group_of(surface) else {
            return;
        };
        let output = self.groups[idx].output.clone();
        let workspace = self.groups[idx].workspace;
        let Some(window) = self.leave_group(idx, surface) else {
            return;
        };
        self.layout.insert(&output, workspace, window, None);
        self.retile();
        self.request_redraw();
    }

    /// If `surface` belongs to a window group, leaves it (promoting the
    /// next tab or dissolving the group, as `leave_group` describes)
    /// instead of letting `detach_mapped_toplevel`'s ordinary
    /// `self.layout.remove(surface)` collapse its leaf. No-op for an
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
        let Some(old_window) = self.layout.window_of(&old_surface) else {
            return;
        };
        let Some(new_window) = self.groups[idx].members[new_active].parked_window.take() else {
            return;
        };

        self.layout.replace_leaf(&old_surface, &new_window);
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

    /// Whether an xdg-activation token's originating serial (if any) is
    /// still fresh enough to steal focus outright. `None` (no serial at
    /// all) is always fresh -- see `handlers/mod.rs`'s `token_created` for
    /// why xwayland-satellite/notification-daemon tokens have to stay
    /// unconditionally trusted here. Shared by `token_created` (whether to
    /// mint the token) and `request_activation` (whether to grant focus or
    /// downgrade to `mark_urgent` once it's actually consumed).
    pub(crate) fn activation_serial_is_fresh(
        &self,
        serial: &Option<(
            smithay::utils::Serial,
            smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        )>,
    ) -> bool {
        let Some((serial, seat)) = serial else {
            return true;
        };
        if Seat::from_resource(seat).as_ref() != Some(&self.seat) {
            return false;
        }
        let keyboard_fresh = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.last_enter())
            .is_some_and(|last_enter| serial.is_no_older_than(&last_enter));
        let pointer_fresh = self
            .seat
            .get_pointer()
            .and_then(|pointer| pointer.last_enter())
            .is_some_and(|last_enter| serial.is_no_older_than(&last_enter));
        keyboard_fresh || pointer_fresh
    }

    /// Marks `surface` urgent instead of stealing focus for it -- the
    /// downgrade path for a stale-serial xdg-activation request (see
    /// `request_activation`). A no-op if the window isn't actually mapped;
    /// urgency only means something for a window the user could plausibly
    /// switch to.
    pub(crate) fn mark_urgent(&mut self, surface: &WlSurface) {
        if self.mapped_toplevel_window(surface).is_none() {
            return;
        }
        self.urgent.insert(surface.clone());
        if let Some(depth) = self.window_depths.get_mut(surface) {
            depth.visual_changed();
        }
        // Single-shot for now, same as the map/focus triggers -- a no-op
        // if the window's workspace is hidden, since spawn_ripple needs a
        // real on-screen location. A repeating "pulse until acknowledged"
        // is later scope (AGENT.md's Phase R1 entry), not built here.
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

    /// Grants an xdg-activation request for `surface`: focus its window,
    /// switching to its workspace first if that workspace is hidden,
    /// promoting it to its group's active tab if it's a parked member, and
    /// raising it if it's floating. No-op while an Exclusive layer surface
    /// (e.g. a lock screen) is mapped -- the same guard every other
    /// focus-moving action uses, so a background client can't activate its
    /// way past one.
    pub(crate) fn activate_toplevel(&mut self, surface: &WlSurface) {
        if self.exclusive_layer().is_some() {
            return;
        }
        if self.mapped_toplevel_window(surface).is_none() {
            return;
        }

        // Where does the window live? A parked group member isn't in any
        // tree under its own surface -- the group owns its slot.
        let ownership = self
            .group_of(surface)
            .map(|idx| (self.groups[idx].output.clone(), self.groups[idx].workspace))
            .or_else(|| {
                let name = self.layout.output_of(surface)?;
                let workspace = self.layout.workspace_of(surface)?;
                Some((name.to_string(), workspace))
            })
            .or_else(|| {
                self.floating_workspace
                    .get(surface)
                    .map(|tag| (tag.output.clone(), tag.workspace))
            });

        if let Some((output_name, workspace)) = ownership {
            let hidden = self.layout.active_workspace(&output_name) != workspace
                && !self.pinned.contains(surface);
            if hidden {
                if let Some(output) = self.output_by_name(&output_name) {
                    self.switch_workspace_immediate(&output, workspace);
                }
            }
        }

        if let Some(idx) = self.group_of(surface) {
            if let Some(tab) = self.groups[idx]
                .members
                .iter()
                .position(|m| &m.surface == surface)
            {
                self.group_activate_tab(idx, tab);
            }
        }

        // Raise only a floating window: raising a tiled one would lift the
        // whole tiled layer above any overlapping floating window, breaking
        // the z-order invariant -- tiled windows never overlap each other,
        // so they never need raising at all.
        if self.floating_workspace.contains_key(surface) {
            if let Some(window) = self.mapped_toplevel_window(surface) {
                self.space.raise_element(&window, false);
            }
        }

        self.focus_window(Some(surface.clone()), SERIAL_COUNTER.next_serial());
    }

    /// Render elements for every group's tab strip, anchored to the top
    /// edge of its active member's on-screen rect. Called by both
    /// backends' `render_surface`, same as `toast`'s render element -- a
    /// solo tile never has a group entry at all, so this is a genuine
    /// no-op (empty `Vec`) in the common case. Each group's cached
    /// `strip` buffer is only rebuilt here when missing (just
    /// created, or invalidated by a membership/active change -- see
    /// `WindowGroup::strip`'s own doc comment) or when the leaf's width no
    /// longer matches the cached one (an output resize, a sibling split
    /// drag) -- not every frame.
    pub fn tab_strip_elements(
        &mut self,
        renderer: &mut GlesRenderer,
    ) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let mut elements = Vec::new();
        for group in &mut self.groups {
            let active_surface = &group.members[group.active].surface;
            let Some(window) = self
                .space
                .elements()
                .find(|w| is_window(w, active_surface))
                .cloned()
            else {
                continue;
            };
            let Some(rect) = self.space.element_geometry(&window) else {
                continue;
            };

            if group.strip.is_none() || group.strip_width != rect.size.w {
                let titles: Vec<String> = group
                    .members
                    .iter()
                    .map(|m| crate::tab_strip::window_title(&m.surface))
                    .collect();
                let (buffer, width) =
                    crate::tab_strip::build_buffer(&titles, group.active, rect.size.w);
                group.strip = Some(buffer);
                group.strip_width = width;
            }

            if let Some(buffer) = &group.strip {
                let location = (rect.loc.x as f64, rect.loc.y as f64);
                if let Some(element) = crate::tab_strip::render_element(buffer, renderer, location)
                {
                    elements.push(element);
                }
            }
        }
        elements
    }

    /// The mapped output named `name`, if any. Used to resolve
    /// `Action::SwapWorkspacesWithOutput`'s config-supplied output name
    /// into a real `Output`.
    pub(crate) fn output_by_name(&self, name: &str) -> Option<Output> {
        self.space.outputs().find(|o| o.name() == name).cloned()
    }

    /// Swaps `output_a`'s and `output_b`'s currently-active workspace
    /// *content* -- the tiled tree and every tagged floating window trade
    /// places, relocating onto the other monitor's screen area, while
    /// each output keeps its own workspace-number bookkeeping untouched
    /// (`layout::Layouts` is per-output-namespaced, see its own doc
    /// comment -- there's no single global "workspace 3" to move between
    /// monitors the way i3 or Hyprland's fully-global workspace model
    /// has). Matches Hyprland's `swapactiveworkspaces` dispatcher. A
    /// plain one-directional "move workspace to output" isn't
    /// implemented separately -- switch the destination to an empty
    /// workspace first, then swap, for the same effect. No-op if either
    /// output is the same one, or an exclusive-interactivity layer is
    /// mapped (same guard every other workspace-navigation path uses).
    pub fn swap_workspaces(&mut self, output_a: &Output, output_b: &Output) {
        if self.exclusive_layer().is_some() {
            return;
        }
        let name_a = output_a.name();
        let name_b = output_b.name();
        if name_a == name_b {
            return;
        }
        let ws_a = self.layout.active_workspace(&name_a);
        let ws_b = self.layout.active_workspace(&name_b);

        // Capture tiled membership before swapping the trees. Fullscreen is
        // an override keyed outside `Layouts`, so moving a tree does not move
        // its FullscreenEntry automatically.
        let fullscreen_tiled_a: Vec<WlSurface> = self
            .layout
            .windows_in(&name_a, ws_a)
            .into_iter()
            .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone()))
            .filter(|surface| self.fullscreen.contains_key(surface))
            .collect();
        let fullscreen_tiled_b: Vec<WlSurface> = self
            .layout
            .windows_in(&name_b, ws_b)
            .into_iter()
            .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone()))
            .filter(|surface| self.fullscreen.contains_key(surface))
            .collect();

        let fullscreen_floating_a: Vec<WlSurface> = self
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == name_a
                    && tag.workspace == ws_a
                    && !self.pinned.contains(*surface)
                    && self.fullscreen.contains_key(*surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        let fullscreen_floating_b: Vec<WlSurface> = self
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == name_b
                    && tag.workspace == ws_b
                    && !self.pinned.contains(*surface)
                    && self.fullscreen.contains_key(*surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        let moving_fullscreen_a: Vec<WlSurface> = fullscreen_tiled_a
            .iter()
            .chain(&fullscreen_floating_a)
            .cloned()
            .collect();
        let moving_fullscreen_b: Vec<WlSurface> = fullscreen_tiled_b
            .iter()
            .chain(&fullscreen_floating_b)
            .cloned()
            .collect();

        // A destination may already own a fullscreen entry on a hidden
        // workspace. If fullscreen content is arriving there, pre-empt that
        // non-moving entry first so the swap cannot create two fullscreen
        // owners for one output. The active fullscreen on the other side is
        // excluded because it is vacating the destination in this same swap.
        let mut preempt = Vec::new();
        if !moving_fullscreen_a.is_empty() {
            if let Some(surface) = self
                .fullscreen
                .iter()
                .find(|(surface, entry)| {
                    entry.output == name_b && !moving_fullscreen_b.contains(*surface)
                })
                .map(|(surface, _)| surface.clone())
            {
                preempt.push(surface);
            }
        }
        if !moving_fullscreen_b.is_empty() {
            if let Some(surface) = self
                .fullscreen
                .iter()
                .find(|(surface, entry)| {
                    entry.output == name_a && !moving_fullscreen_a.contains(*surface)
                })
                .map(|(surface, _)| surface.clone())
            {
                preempt.push(surface);
            }
        }
        for surface in preempt {
            let toplevel = self
                .mapped_toplevel_window(&surface)
                .and_then(|window| window.toplevel().cloned());
            if let Some(toplevel) = toplevel {
                self.do_unfullscreen(&toplevel);
            } else {
                self.fullscreen.remove(&surface);
            }
        }

        self.layout.swap_active(&name_a, &name_b);

        let delta = match (
            self.space.output_geometry(output_a),
            self.space.output_geometry(output_b),
        ) {
            (Some(geo_a), Some(geo_b)) => Some(geo_b.loc - geo_a.loc),
            _ => None,
        };
        let bounds_a = self
            .output_tiling_area(output_a)
            .or_else(|| self.space.output_geometry(output_a));
        let bounds_b = self
            .output_tiling_area(output_b)
            .or_else(|| self.space.output_geometry(output_b));

        // FloatingTag.rect is a hide-time snapshot and deliberately stale
        // while visible. Capture current non-fullscreen geometry before the
        // ownership mutation so a workspace swap cannot snap a moved/resized
        // floater back to its old position. A fullscreen tag retains its
        // windowed restore rect instead of the viewport geometry.
        let live_floating_rects: Vec<(WlSurface, Rectangle<i32, Logical>)> = self
            .space
            .elements()
            .filter_map(|window| {
                let surface = window.toplevel()?.wl_surface().clone();
                if self.fullscreen.contains_key(&surface) || self.maximized.contains_key(&surface) {
                    None
                } else {
                    self.space
                        .element_geometry(window)
                        .map(|rect| (surface, rect))
                }
            })
            .collect();
        for surface in fullscreen_tiled_a {
            if let Some(entry) = self.fullscreen.get_mut(&surface) {
                entry.move_to_output(name_b.clone(), delta);
                if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, bounds_b) {
                    *rect = clamp_rect_visible(*rect, bounds);
                }
            }
        }
        for surface in fullscreen_tiled_b {
            if let Some(entry) = self.fullscreen.get_mut(&surface) {
                entry.move_to_output(
                    name_a.clone(),
                    delta.map(|delta| (-delta.x, -delta.y).into()),
                );
                if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, bounds_a) {
                    *rect = clamp_rect_visible(*rect, bounds);
                }
            }
        }

        // Floating windows aren't tracked by `Layouts` at all, so they need
        // their own retag (their content-group's identity moved to the
        // other output) and reposition (translate by the delta between the
        // two outputs' origins, preserving relative on-screen placement --
        // they were never unmapped, so nothing else will move them).
        let mut moved: Vec<(Window, Point<i32, Logical>)> = Vec::new();
        for (surface, tag) in &mut self.floating_workspace {
            // Pinning exempts a window from workspace membership; including
            // it only when its intentionally stale nominal workspace happened
            // to be active made cross-output behavior nondeterministic.
            if self.pinned.contains(surface) {
                continue;
            }
            if tag.output == name_a && tag.workspace == ws_a {
                if let Some((_, rect)) = live_floating_rects
                    .iter()
                    .find(|(candidate, _)| candidate == surface)
                {
                    tag.rect = *rect;
                }
                tag.output = name_b.clone();
                tag.workspace = ws_b;
                if let Some(delta) = delta {
                    tag.rect.loc += delta;
                    if let Some(bounds) = bounds_b {
                        tag.rect = clamp_rect_visible(tag.rect, bounds);
                    }
                    moved.push((tag.window.clone(), tag.rect.loc));
                }
                if let Some(entry) = self.fullscreen.get_mut(surface) {
                    entry.move_to_output(name_b.clone(), delta);
                    if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, bounds_b) {
                        *rect = clamp_rect_visible(*rect, bounds);
                    }
                }
                if let Some(entry) = self.maximized.get_mut(surface) {
                    entry.move_to_output(name_b.clone(), delta);
                    if let Some(bounds) = bounds_b {
                        entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
                    }
                }
            } else if tag.output == name_b && tag.workspace == ws_b {
                if let Some((_, rect)) = live_floating_rects
                    .iter()
                    .find(|(candidate, _)| candidate == surface)
                {
                    tag.rect = *rect;
                }
                tag.output = name_a.clone();
                tag.workspace = ws_a;
                if let Some(delta) = delta {
                    tag.rect.loc -= delta;
                    if let Some(bounds) = bounds_a {
                        tag.rect = clamp_rect_visible(tag.rect, bounds);
                    }
                    moved.push((tag.window.clone(), tag.rect.loc));
                }
                if let Some(entry) = self.fullscreen.get_mut(surface) {
                    entry.move_to_output(
                        name_a.clone(),
                        delta.map(|delta| (-delta.x, -delta.y).into()),
                    );
                    if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, bounds_a) {
                        *rect = clamp_rect_visible(*rect, bounds);
                    }
                }
                if let Some(entry) = self.maximized.get_mut(surface) {
                    entry.move_to_output(
                        name_a.clone(),
                        delta.map(|delta| (-delta.x, -delta.y).into()),
                    );
                    if let Some(bounds) = bounds_a {
                        entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
                    }
                }
            }
        }
        for (window, loc) in moved {
            self.space.map_element(window, loc, false);
        }

        self.retile();
    }

    /// If `config.input.focus_follows_mouse` is on, moves keyboard focus to
    /// whatever window (not layer surface -- bars/launchers are deliberately
    /// excluded, matching typical i3/sway/Hyprland scope) is under `pos`.
    /// Called on every pointer motion. Deliberately doesn't raise -- Hyprland's
    /// own default hover-focus doesn't either, only clicking raises. No-ops
    /// if the window under `pos` is already focused (hovering the same
    /// window repeatedly shouldn't spam redundant focus events), or if an
    /// exclusive-interactivity layer (e.g. a lock screen) is mapped, same
    /// guard every other focus-changing path already checks.
    pub fn focus_follows_mouse(&mut self, pos: Point<f64, Logical>) {
        if !self.config.input.focus_follows_mouse {
            return;
        }
        if self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
        {
            return;
        }
        if self.exclusive_layer().is_some() {
            return;
        }
        if self.layer_under_pointer(pos).is_some() {
            return;
        }
        let surface = self
            .space
            .element_under(pos)
            .and_then(|(window, _)| window.toplevel().map(|t| t.wl_surface().clone()));
        let Some(surface) = surface else {
            return;
        };
        let current = self.focused_window_surface();
        if current.as_ref() == Some(&surface) {
            return;
        }
        self.focus_window(Some(surface), SERIAL_COUNTER.next_serial());
    }

    /// Cycles keyboard focus to the next mapped window (tiled or floating)
    /// and raises it to the top of the stack. This is the only way to reach
    /// a window that's fully covered by another one, since you can't click
    /// something you can't see.
    pub fn cycle_focus(&mut self) {
        // Don't let a keybind tab focus away from an exclusive-interactivity
        // layer (e.g. a lock screen) while it's still mapped.
        if self.exclusive_layer().is_some() {
            return;
        }
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        if windows.is_empty() {
            return;
        }
        let surface_of = |w: &Window| w.toplevel().map(|t| t.wl_surface().clone());

        // MRU order: every visible window that's been focused before, most
        // recent first, then any visible window `focus_history` doesn't
        // know about yet (freshly mapped, never focused) in `Space`'s own
        // order -- matches niri/Hyprland's own Alt-Tab convention instead
        // of the previous plain z-order walk.
        let mut ordered: Vec<WlSurface> = self
            .focus_history
            .iter()
            .filter(|s| windows.iter().any(|w| surface_of(w).as_ref() == Some(*s)))
            .cloned()
            .collect();
        for window in &windows {
            if let Some(surface) = surface_of(window) {
                if !ordered.contains(&surface) {
                    ordered.push(surface);
                }
            }
        }
        if ordered.is_empty() {
            return;
        }

        let current = self.focused_window_surface();
        let current_index = current
            .as_ref()
            .and_then(|s| ordered.iter().position(|o| o == s));
        let next_index = match current_index {
            Some(i) => (i + 1) % ordered.len(),
            None => 0,
        };
        let next_surface = ordered[next_index].clone();
        let Some(next) = windows
            .iter()
            .find(|w| surface_of(w).as_ref() == Some(&next_surface))
        else {
            return;
        };

        self.space.raise_element(next, false);
        self.cycling_focus = true;
        self.focus_window(Some(next_surface), SERIAL_COUNTER.next_serial());
        self.cycling_focus = false;
    }

    /// Moves keyboard focus to the nearest mapped window in `direction`
    /// from the currently focused one (tiled or floating; works purely off
    /// on-screen geometry, so it doesn't care which). No-op if nothing is
    /// focused or nothing else lies in that direction.
    pub fn focus_direction(&mut self, direction: Direction) {
        let Some(focused) = self.focused_window_surface() else {
            return;
        };
        let Some(current) = self
            .space
            .elements()
            .find(|w| is_window(w, &focused))
            .cloned()
        else {
            return;
        };
        let Some(next) = self.neighbor_in_direction(&current, direction) else {
            return;
        };

        self.space.raise_element(&next, false);
        self.focus_window(
            Some(next.toplevel().unwrap().wl_surface().clone()),
            SERIAL_COUNTER.next_serial(),
        );
    }

    /// Swaps the currently focused *tiled* window with its neighbor in
    /// `direction`, keeping focus on the same window (which moves to the
    /// neighbor's former slot). Floating windows aren't part of the tiling
    /// tree, so this only applies to tiled ones -- swapping a floating
    /// window's screen position doesn't mean anything the way it does for
    /// two tiled slots.
    pub fn swap_direction(&mut self, direction: Direction) {
        let Some(focused) = self.focused_window_surface() else {
            return;
        };
        if !self.layout.contains(&focused) {
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
        if !self.layout.contains(&neighbor_surface) {
            // Only swap within the tiling tree; a floating window nearest
            // in that direction isn't a valid swap target.
            return;
        }

        self.layout.swap(&focused, &neighbor_surface);
        self.retile();
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

        if !self.layout.contains(&surface) {
            // `Window::geometry().loc` is surface-local (usually 0,0), not
            // the element's global position in `Space`. Reusing it here
            // would teleport every keyboard-resized floater to the output's
            // origin.
            let location = self.space.element_location(&window).unwrap_or_default();
            let mut geometry = window.geometry();
            match direction {
                Direction::Right => geometry.size.w = geometry.size.w.saturating_add(STEP),
                Direction::Down => geometry.size.h = geometry.size.h.saturating_add(STEP),
                Direction::Left => geometry.size.w = (geometry.size.w - STEP).max(64),
                Direction::Up => geometry.size.h = (geometry.size.h - STEP).max(48),
            }
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(geometry.size));
                toplevel.send_pending_configure();
            }
            self.space.map_element(window, location, true);
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

    /// Shows/hides the workspace overview on the current output (see
    /// `overview.rs`, `Action::ToggleOverview`). Built fresh each time it's
    /// toggled on -- it doesn't animate or otherwise need to stay in sync
    /// with changes made while it's open, so there's nothing to cache
    /// across frames the way `tab_strip_elements` caches per group.
    pub fn toggle_overview(&mut self) {
        if self.overview.take().is_some() {
            self.request_redraw();
            return;
        }

        let Some(output) = self.primary_output() else {
            return;
        };
        let Some(mode) = output.current_mode() else {
            return;
        };
        let output_name = output.name();
        let active_workspace = self.layout.active_workspace(&output_name);

        // Every workspace this output has ever populated, plus the active
        // one even if it's currently empty, so there's always at least one
        // cell to show.
        let mut workspaces: Vec<u32> = self
            .layout
            .populated_workspaces()
            .into_iter()
            .filter(|(name, _)| name == &output_name)
            .map(|(_, workspace)| workspace)
            .collect();
        if !workspaces.contains(&active_workspace) {
            workspaces.push(active_workspace);
        }
        workspaces.sort_unstable();

        const CELL_GAP: i32 = 12;
        let cell_count = workspaces.len() as i32;
        let cell_w = (mode.size.w - CELL_GAP * (cell_count + 1)) / cell_count.max(1);
        let cell_h = mode.size.h - CELL_GAP * 2;

        let cells: Vec<crate::overview::OverviewCell> = workspaces
            .iter()
            .enumerate()
            .map(|(i, &workspace)| {
                let x = CELL_GAP + i as i32 * (cell_w + CELL_GAP);
                let area =
                    Rectangle::new((x, CELL_GAP).into(), (cell_w.max(1), cell_h.max(1)).into());
                // Reuses whichever algorithm (BSP or master) is actually
                // active for this workspace, computed directly at the
                // cell's own smaller size -- no separate scaling math
                // needed, `Layouts::layout` already produces proportional
                // rects for whatever area it's given.
                let windows = self
                    .layout
                    .layout(&output_name, workspace, area, 4)
                    .into_iter()
                    .filter_map(|(window, rect)| {
                        let surface = window.toplevel()?.wl_surface().clone();
                        Some((rect, crate::tab_strip::window_title(&surface)))
                    })
                    .collect();
                crate::overview::OverviewCell {
                    workspace,
                    area,
                    active: workspace == active_workspace,
                    windows,
                }
            })
            .collect();

        self.overview = Some(crate::overview::Overview::build(
            output_name,
            &cells,
            (mode.size.w, mode.size.h),
        ));
        self.request_redraw();
    }

    /// Finds the mapped window whose center lies nearest `from`'s center in
    /// `direction`. "Nearest" ranks candidates by distance along that
    /// direction's axis, with a penalty for how far off-axis they sit, so a
    /// neighbor roughly level with `from` wins over one that's technically
    /// closer in raw distance but well off to the side.
    fn neighbor_in_direction(&self, from: &Window, direction: Direction) -> Option<Window> {
        let from_center = center(self.space.element_geometry(from)?);

        self.space
            .elements()
            .filter(|w| *w != from)
            .filter_map(|w| {
                let c = center(self.space.element_geometry(w)?);
                let (primary, off_axis) = match direction {
                    Direction::Left => (from_center.x - c.x, from_center.y - c.y),
                    Direction::Right => (c.x - from_center.x, from_center.y - c.y),
                    Direction::Up => (from_center.y - c.y, from_center.x - c.x),
                    Direction::Down => (c.y - from_center.y, from_center.x - c.x),
                };
                (primary > 0).then_some((w.clone(), primary as f64 + (off_axis as f64).abs() * 2.0))
            })
            .min_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(w, _)| w)
    }

    /// Records one relevant filesystem event and ensures a single trailing
    /// reload runs after the event stream has been quiet for 150ms. Editors
    /// that truncate then rewrite a file can otherwise make the first event
    /// load a partial file and have the completed write discarded by a
    /// leading-edge debounce.
    pub fn note_config_event(&mut self) {
        const QUIET: Duration = Duration::from_millis(150);
        self.last_config_event = Instant::now();
        if self.config_reload_timer_armed {
            return;
        }
        self.config_reload_timer_armed = true;
        let result = self.loop_handle.insert_source(
            Timer::from_duration(QUIET),
            move |_, _, state: &mut Smallvil| {
                let elapsed = state.last_config_event.elapsed();
                if elapsed >= QUIET {
                    state.config_reload_timer_armed = false;
                    state.reload_config();
                    TimeoutAction::Drop
                } else {
                    TimeoutAction::ToDuration(QUIET - elapsed)
                }
            },
        );
        if let Err(err) = result {
            self.config_reload_timer_armed = false;
            tracing::warn!(%err, "Failed to register config debounce timer; reloading immediately");
            self.reload_config();
        }
    }

    /// Re-reads the config file and applies what can be applied live
    /// (keybinds, input repeat rate). Shows a toast either way so a reload
    /// is never silent, success or failure.
    pub fn reload_config(&mut self) {
        match Config::reload() {
            Ok((new_config, warnings)) => {
                let had_error_overlay = self.config_error_overlay.take().is_some();
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.change_repeat_info(
                        new_config.input.repeat_rate,
                        new_config.input.repeat_delay,
                    );
                    // Same bad-config-must-not-break-things guarantee as
                    // the startup path in `Smallvil::new`: on failure this
                    // just logs and leaves the previously-loaded keymap in
                    // place, it doesn't touch `inner.keyboard` at all.
                    if let Err(e) = keyboard.set_xkb_config(self, new_config.input.xkb_config()) {
                        tracing::error!(
                            error = ?e,
                            layout = %new_config.input.xkb_layout,
                            "Reloaded XKB keymap failed to compile, keeping the previous one"
                        );
                    }
                }
                if new_config.show_welcome_hint {
                    if self.welcome_hint.is_none() {
                        self.welcome_hint =
                            Some(crate::welcome::WelcomeHint::build(&new_config.terminal));
                    }
                } else {
                    self.welcome_hint = None;
                }
                self.config = new_config;
                if !self.config.animations.enabled || !self.config.animations.open.enabled {
                    self.window_open_animations.clear();
                }
                if !self.config.animations.enabled || !self.config.animations.movement.enabled {
                    self.window_move_animations.clear();
                }
                let disabled_viscosity: Vec<WlSurface> = self
                    .window_viscosity
                    .keys()
                    .filter(|surface| self.viscosity_for_surface(surface) <= f64::EPSILON)
                    .cloned()
                    .collect();
                for surface in disabled_viscosity {
                    self.window_viscosity.remove(&surface);
                }
                let disabled_sway: Vec<WlSurface> = self
                    .window_sway
                    .keys()
                    .filter(|surface| !self.sway_enabled_for_surface(surface))
                    .cloned()
                    .collect();
                for surface in disabled_sway {
                    self.window_sway.remove(&surface);
                }
                if !self.config.animations.enabled || !self.config.animations.close.enabled {
                    self.window_frame_snapshots.clear();
                    self.closing_window_animations.clear();
                }
                self.depth_schematics.clear();
                self.depth_last_tick = Instant::now() - Duration::from_millis(100);
                self.update_window_depths();
                if !self.config.water_effects || !self.config.workspace_transition.enabled {
                    self.workspace_transitions.clear();
                    let pending = std::mem::take(&mut self.pending_workspace_transitions);
                    for (output_name, workspace) in pending {
                        if let Some(output) = self.output_by_name(&output_name) {
                            self.switch_workspace_immediate(&output, workspace);
                        }
                    }
                }
                self.window_opacity = self
                    .foreign_toplevels
                    .keys()
                    .filter_map(|surface| {
                        let (app_id, title) = self.toplevel_identity(surface);
                        let rule = self
                            .config
                            .resolve_window_rules(app_id.as_deref(), title.as_deref());
                        crate::config::WindowOpacity::from_rule(&rule)
                            .map(|opacity| (surface.clone(), opacity))
                    })
                    .collect();
                self.window_glass_modes = self
                    .foreign_toplevels
                    .keys()
                    .filter_map(|surface| {
                        let (app_id, title) = self.toplevel_identity(surface);
                        self.config
                            .resolve_window_rules(app_id.as_deref(), title.as_deref())
                            .glass
                            .map(|mode| (surface.clone(), mode))
                    })
                    .collect();
                // The mode or frost tuning may have changed. Force the
                // shared pre-frame pipeline to rebuild against the current
                // window geometry instead of briefly showing stale content.
                self.backdrop_textures.clear();
                // A reload that dropped or renamed the currently-active
                // submap would otherwise leave every key silently
                // unmatched (falling through as plain input) with no
                // indication why -- exit back to the base keybinds
                // instead, the same table a name that never existed at
                // all would resolve to.
                if let Some(name) = &self.active_submap {
                    if !self.config.submaps.contains_key(name) {
                        self.active_submap = None;
                    }
                }
                self.layout
                    .set_default_algorithm(self.config.default_layout);
                self.layout
                    .set_master_orientation(self.config.master_orientation);
                self.layout.set_split_bias(self.config.bsp_split_bias);
                // The DeviceAdded path is the only other place this runs;
                // an already-connected touchpad (a laptop's built-in one,
                // which won't see another DeviceAdded short of a restart)
                // otherwise never picks up an `[input.touchpad]` edit.
                for device in self.known_touchpads.iter_mut() {
                    crate::input::apply_touchpad_config(&self.config.input.touchpad, device);
                }
                tracing::info!("Config reloaded");
                self.toast = Some(Toast::new("Config reloaded", ToastKind::Info));
                // Unlike a hard parse failure, these diagnostics don't mean
                // the reload was rejected -- `new_config` above is already
                // in effect. Still worth a persistent nudge instead of a
                // toast that scrolls away, since a footgun lint is exactly
                // the kind of thing you want to notice before it bites,
                // not after.
                if !warnings.is_empty() {
                    self.config_error_overlay =
                        Some(crate::error_overlay::ConfigErrorOverlay::new(
                            warnings.join("; "),
                            crate::error_overlay::OverlaySeverity::Warning,
                        ));
                }
                self.retile();
                if had_error_overlay || !warnings.is_empty() {
                    self.request_redraw();
                }
            }
            Err(err) => {
                tracing::warn!(%err, "Failed to reload config, keeping the previous one");
                // "Config error {err}" (no colon) rather than "Config
                // error: {err}" -- err already starts with "in file ... at
                // line N: ...", matching Hyprland's own on-screen config
                // error phrasing ("config error in file <path> at line
                // <N>: <message>") rather than inventing our own shape.
                let message = format!("Config error {err}");
                self.config_error_overlay = Some(crate::error_overlay::ConfigErrorOverlay::new(
                    &message,
                    crate::error_overlay::OverlaySeverity::Error,
                ));
                // Keep the established notification path for immediate
                // visual feedback/debugging; the reserved panel is the
                // persistent, readable diagnostic.
                self.toast = Some(Toast::new("Config reload failed", ToastKind::Error));
                self.retile();
                self.request_redraw();
            }
        }
        // Emit on both success and failure: a bar may want to refresh
        // config-derived state either way, and a reload that failed
        // still left the previously-applied config in place.
        self.emit_ipc_event(crate::ipc::IpcEvent::ConfigReloaded);
    }
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

fn center(rect: Rectangle<i32, Logical>) -> Point<i32, Logical> {
    (rect.loc.x + rect.size.w / 2, rect.loc.y + rect.size.h / 2).into()
}

/// Keeps at least a small, grabbable part of a floating window inside the
/// target working area after an output move. Raw origin translation can put
/// almost the entire window off-screen when the destination is smaller.
fn clamp_rect_visible(
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
    let min_x = bounds.loc.x - rect.size.w + visible_w;
    let max_x = bounds.loc.x + bounds.size.w - visible_w;
    let min_y = bounds.loc.y - rect.size.h + visible_h;
    let max_y = bounds.loc.y + bounds.size.h - visible_h;
    rect.loc.x = rect.loc.x.clamp(min_x.min(max_x), min_x.max(max_x));
    rect.loc.y = rect.loc.y.clamp(min_y.min(max_y), min_y.max(max_y));
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

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
