mod capture;
mod compositor;
mod layer_shell;
mod screencopy;
pub(crate) mod wlr_foreign_toplevel;
pub(crate) mod wlr_gamma_control;
pub(crate) mod wlr_output_management;
pub(crate) mod wlr_output_power_management;
mod xdg_shell;

use crate::Smallvil;

//
// Wl Seat
//

use smithay::backend::renderer::ImportDma;
use smithay::desktop::utils::surface_primary_scanout_output;
use smithay::desktop::{PopupKind, PopupManager};
use smithay::input::{pointer::PointerHandle, Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::compositor::{get_parent, with_states};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::foreign_toplevel_list::{
    ForeignToplevelListHandler, ForeignToplevelListState,
};
use smithay::wayland::fractional_scale::{with_fractional_scale, FractionalScaleHandler};
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};
use smithay::wayland::input_method::{InputMethodHandler, PopupSurface};
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraintsHandler};
use smithay::wayland::security_context::{
    SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
};
use smithay::wayland::selection::data_device::{
    set_data_device_focus, DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::{
    delegate_cursor_shape, delegate_data_control, delegate_data_device, delegate_dmabuf,
    delegate_foreign_toplevel_list, delegate_fractional_scale, delegate_idle_inhibit,
    delegate_idle_notify, delegate_input_method_manager, delegate_kde_decoration,
    delegate_keyboard_shortcuts_inhibit, delegate_output, delegate_pointer_constraints,
    delegate_pointer_gestures, delegate_presentation, delegate_primary_selection,
    delegate_relative_pointer, delegate_seat, delegate_security_context, delegate_session_lock,
    delegate_single_pixel_buffer, delegate_text_input_manager, delegate_viewporter,
    delegate_virtual_keyboard_manager, delegate_xdg_activation, delegate_xdg_decoration,
};

use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::{
    Mode, OrgKdeKwinServerDecoration,
};
use smithay::reexports::wayland_server::WEnum;

impl SeatHandler for Smallvil {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Smallvil> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        self.cursor_status = image;
        self.request_redraw();
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}

delegate_seat!(Smallvil);

// `wp_cursor_shape_manager_v1`'s `Dispatch` impl requires `TabletSeatHandler`
// alongside `SeatHandler` (a `wp_cursor_shape_device_v1` can also be created
// for a `zwp_tablet_tool_v2`) even though TideWM has no tablet-manager
// protocol of its own -- the trait's one method already defaults to a no-op,
// so there's nothing to override here.
impl smithay::wayland::tablet_manager::TabletSeatHandler for Smallvil {}
delegate_cursor_shape!(Smallvil);

//
// Wl Data Device
//

impl SelectionHandler for Smallvil {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Smallvil {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl WaylandDndGrabHandler for Smallvil {}

delegate_data_device!(Smallvil);

//
// wp-primary-selection-unstable-v1 (select-to-copy / middle-click paste)
//

impl PrimarySelectionHandler for Smallvil {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

delegate_primary_selection!(Smallvil);

//
// wlr-data-control-unstable-v1 (clipboard-manager hooks)
//
// Lets a client that's never focused (cliphist, clipman, wl-clip-persist)
// read and write the clipboard via wl_data_device's protocol shape, which
// TideWM already implements above -- this just exposes it to a client that
// wouldn't otherwise get a wl_data_device at all. No handler logic of its
// own beyond the state getter; Smithay's convenience module covers the
// rest.
//

impl DataControlHandler for Smallvil {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

delegate_data_control!(Smallvil);

//
// Wl Output & Xdg Output
//

impl OutputHandler for Smallvil {}
delegate_output!(Smallvil);

//
// Linux DMA-BUF
//
// Only relevant on the udev backend (see backend/udev.rs), the only place
// that creates the global and populates `udev_renderer`. No global means
// no client ever binds it, so `dmabuf_imported` never fires under winit.
//

impl DmabufHandler for Smallvil {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        let Some(renderer) = &self.udev_renderer else {
            notifier.failed();
            return;
        };
        if renderer.borrow_mut().import_dmabuf(&dmabuf, None).is_ok() {
            let _ = notifier.successful::<Smallvil>();
        } else {
            notifier.failed();
        }
    }
}

delegate_dmabuf!(Smallvil);

//
// wp_viewporter
//
// No handler logic needed: `on_commit_buffer_handler` (handlers/compositor.rs)
// already validates viewport state on every commit. Exists so the global is
// present at all -- `xwayland-satellite` (src/xwayland.rs) requires it and
// panics on startup otherwise.
//

delegate_viewporter!(Smallvil);

//
// Idle inhibit + idle notify
//
// zwp_idle_inhibit_manager_v1 lets a client (video player, presentation app)
// block the session from being considered idle while its surface is
// inhibiting. ext-idle-notify lets an external tool (a swayidle-style
// daemon) watch idle/resume transitions and act on them (screen off, lock,
// suspend). TideWM has no idle/DPMS/lock behavior of its own, so
// idle-inhibit only has an effect by feeding idle-notify's
// `set_is_inhibited`. `notify_activity` is called from every real input
// event in `input.rs`.
//

impl IdleInhibitHandler for Smallvil {
    fn inhibit(&mut self, surface: WlSurface) {
        let was_empty = self.idle_inhibitors.is_empty();
        *self.idle_inhibitors.entry(surface).or_insert(0) += 1;
        if was_empty {
            self.idle_notifier_state.set_is_inhibited(true);
        }
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        if let Some(count) = self.idle_inhibitors.get_mut(&surface) {
            *count -= 1;
            if *count == 0 {
                self.idle_inhibitors.remove(&surface);
            }
        }
        if self.idle_inhibitors.is_empty() {
            self.idle_notifier_state.set_is_inhibited(false);
        }
    }
}

delegate_idle_inhibit!(Smallvil);

impl IdleNotifierHandler for Smallvil {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

delegate_idle_notify!(Smallvil);

//
// ext-session-lock-v1
//
// Lets a privileged client (swaylock, a hyprlock-style daemon) take over
// every output and gate all window/layer input until it decides to unlock.
// The actual state machine (grab/focus teardown, per-output locked-frame
// confirmation, render substitution) lives on `Smallvil` in state.rs
// (`lock_session`/`unlock_session`/`register_lock_surface`/
// `mark_output_locked_frame`) -- this impl just adapts Smithay's handler
// shape to it.
//
// If a lock client dies without unlocking, the session stays locked and a
// second `lock()` request is refused (see `lock_session`) -- the protocol
// explicitly allows this as compositor policy ("It is acceptable for the
// session to be permanently locked if this happens"). A dead-client
// takeover was considered and dropped: Smithay 0.7's own
// `SessionLockManagerState::locked_outputs` (its per-output
// `get_lock_surface` duplicate check, unrelated to `Smallvil`'s own
// `locked_outputs`) is only ever cleared on a graceful
// `unlock_and_destroy`, never on ungraceful client death -- so a new
// client's `get_lock_surface` would still hit a `duplicate_output`
// protocol error (which disconnects it) even if this handler accepted its
// `lock()` call. Not fixable from the compositor side at this pinned
// Smithay version.
//

impl SessionLockHandler for Smallvil {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        self.lock_session(confirmation);
    }

    fn unlock(&mut self) {
        self.unlock_session();
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else {
            return;
        };
        self.register_lock_surface(output, surface);
    }
}

delegate_session_lock!(Smallvil);

//
// xdg-decoration + KDE server-decoration
//
// Two competing protocols for the same thing: letting a client (GTK, Qt)
// ask the compositor whether the client should draw its own title bar.
// TideWM enforces server-side on both, which (since TideWM itself draws
// no decorations) means no decorations at all -- the sway/Hyprland tiling
// convention. A client-drawn header bar wastes vertical space in a tiling
// layout, and floating windows focus/move/resize through Super-modifier
// actions rather than a title-bar hit-test.
//
// `request_mode`/`unset_mode` deliberately do not honor a client's ask for
// client-side: the protocol explicitly allows the compositor to disagree
// (the client is "requesting", not "telling"). The configure that follows
// re-advertises server-side, and well-behaved clients stop drawing CSD.
// This is the same enforcement shape sway uses; anvil honors client
// requests because anvil actually implements SSD drawing, which TideWM
// does not.
//

impl XdgDecorationHandler for Smallvil {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: zxdg_toplevel_decoration_v1::Mode) {
        // Ignore what the client asked for; reaffirm server-side.
        let _ = mode;
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

delegate_xdg_decoration!(Smallvil);

impl KdeDecorationHandler for Smallvil {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration_state
    }

    fn request_mode(
        &mut self,
        _surface: &WlSurface,
        decoration: &OrgKdeKwinServerDecoration,
        mode: WEnum<Mode>,
    ) {
        // Same enforcement: re-advertise server regardless of what was
        // requested. (Still need to read `mode` so the WEnum is consumed
        // even on the no-op path.)
        let _ = mode;
        decoration.mode(Mode::Server);
    }
}

delegate_kde_decoration!(Smallvil);

//
// xdg-activation-v1
//
// Focus handoff on request: a client presents a token (its own, or one a
// launcher/notification daemon passed to it) and asks for one of its
// surfaces to be activated. xwayland-satellite also binds this global --
// it warned "could not bind xdg activation (NotPresent)" at startup
// before this existed.
//
// Token policy follows niri's shape: a token carrying a seat serial is
// valid only if the serial is no older than the last keyboard or pointer
// enter on our seat (it derives from a real recent user interaction, and
// pointer is checked too because a KeyboardInteractivity::None layer
// never holds keyboard focus). A serial-less token is accepted outright:
// niri downgrades those to "urgency only", but TideWM has no urgency
// indicator anywhere, so there is nothing else useful to do with them --
// and rejecting them would break exactly the cases this protocol is for
// here (xwayland-satellite can't produce a Wayland serial on an X11
// client's behalf; notification daemons hand out tokens without one).
//

impl XdgActivationHandler for Smallvil {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        // Only refuses to mint a token at all for a serial that names a
        // foreign seat -- a present-but-*stale* serial still mints the
        // token (unlike this used to work): `request_activation` re-checks
        // freshness once the token is actually consumed and downgrades to
        // `mark_urgent` instead of refusing outright, the one case where
        // "urgent" genuinely applies. A missing serial altogether is always
        // accepted outright, never downgraded -- see
        // `Smallvil::activation_serial_is_fresh`'s own doc comment for why
        // (xwayland-satellite/notification-daemon tokens depend on it).
        match &data.serial {
            None => true,
            Some((_, seat)) => Seat::from_resource(seat).as_ref() == Some(&self.seat),
        }
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Tokens are single-use here. Dropping it whether or not the
        // request is granted also keeps `known_tokens` from growing
        // without bound.
        self.xdg_activation_state.remove_token(&token);

        // Same 10s freshness window anvil and niri both use.
        if token_data.timestamp.elapsed().as_secs() >= 10 {
            return;
        }
        if self.activation_serial_is_fresh(&token_data.serial) {
            self.activate_toplevel(&surface);
        } else {
            self.mark_urgent(&surface);
        }
    }
}

delegate_xdg_activation!(Smallvil);

//
// wp-single-pixel-buffer-v1
//
// Pure global advertisement: there is no handler trait to implement, and
// Smithay's `WaylandSurfaceRenderElement` resolves these buffers itself
// during rendering.
//

delegate_single_pixel_buffer!(Smallvil);

//
// wp-presentation-time
//
// No handler trait here either: the global dispatch is fully covered by
// Smithay's PresentationState. The compositor-side work is collecting and
// presenting feedback per frame, which lives in
// `Smallvil::take_presentation_feedback` and each backend's render loop.
//

delegate_presentation!(Smallvil);

//
// wp-fractional-scale-v1
//
// The `[[output]] scale` config knob has always accepted fractions, but
// without this global clients only ever saw the integer wl_output scale
// and rendered blurry at 1.25x/1.5x. `new_fractional_scale` resolves the
// surface's output the same way anvil does (primary scan-out output,
// then the root surface's, then the window's own output, finally the
// first output in the space) and seeds the preferred scale; afterwards
// `set_window_fractional_scale`/`set_layer_fractional_scale` (state.rs)
// refresh it from the actual placement paths (retile, floating drags,
// layer map).
//

impl FractionalScaleHandler for Smallvil {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        let scanout = |surface: &WlSurface| {
            with_states(surface, |states| {
                surface_primary_scanout_output(surface, states)
            })
        };
        let output = scanout(&surface)
            .or_else(|| (root != surface).then(|| scanout(&root)).flatten())
            .or_else(|| {
                self.mapped_toplevel_window(&root)
                    .and_then(|window| self.output_for_window(&window))
            })
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            return;
        };

        let scale = output.current_scale().fractional_scale();
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

delegate_fractional_scale!(Smallvil);

//
// ext-foreign-toplevel-list-v1
//
// Exposes every mapped toplevel to external tooling (waybar-style taskbars,
// window switchers, per-window capture) as a handle with title/app_id. The
// handler itself is a bare getter: handle lifetime is driven from the
// toplevel lifecycle in `handlers/xdg_shell.rs` (created in `map_toplevel`,
// closed in `detach_mapped_toplevel`, title/app_id refreshed in
// `handle_commit`), tracked in `Smallvil::foreign_toplevels`. Read-only by
// design -- this protocol has no activate/close requests, so a bar that
// wants click-to-focus builds that on xdg-activation (already implemented),
// not here.
//

impl ForeignToplevelListHandler for Smallvil {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

delegate_foreign_toplevel_list!(Smallvil);

//
// wp-keyboard-shortcuts-inhibit + wp-pointer-gestures
//
// Shortcuts-inhibit lets a VM or remote-desktop client capture combos the
// compositor would otherwise intercept (Alt+Tab etc.) for its guest.
// Activation policy is the simple one niri and wlroots both use: activate
// on creation, gate at event time -- `input.rs`'s keyboard filter checks
// whether the *keyboard-focused* surface holds an active inhibitor before
// matching `[keybinds]`, so the inhibit only applies while that client is
// actually focused. VT-switch is checked before the gate, same as the
// session-lock early-return: a stuck guest can't take away the escape
// hatch.
//
// Pointer-gestures is pure global advertisement: no handler trait exists.
// `input.rs` forwards the backend's libinput swipe/pinch/hold events to
// the pointer handle, and Smithay's own machinery delivers them to
// whichever client created gesture objects for the focused surface.
//

impl KeyboardShortcutsInhibitHandler for Smallvil {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }
}

delegate_keyboard_shortcuts_inhibit!(Smallvil);
delegate_pointer_gestures!(Smallvil);

//
// zwp_text_input_v3 + zwp_input_method_v2 + zwp_virtual_keyboard_v1
//
// Text-input (client-side: "I'm a text field, here's my content type and
// surrounding text") and input-method (IME-side: receives that state,
// replies with commit_string/preedit) are two ends of the same relay,
// almost entirely Smithay's own machinery. Text-input has no handler
// trait at all: focus tracking is automatic off `WlSurface`'s own
// `KeyboardTarget` impl, which already fires from
// `reconcile_keyboard_focus`'s existing `keyboard.set_focus` call -- an
// IME activates the moment a focused text field enables itself, with
// zero code here. An IME's `grab_keyboard` request composes with
// `input.rs`'s own filter the same way any other keyboard grab does: the
// filter always runs first (VT-switch, session-lock, shortcuts-inhibit,
// `[keybinds]` all still win), and only un-intercepted keys get routed to
// the grab instead of the normally-focused client.
//
// `InputMethodHandler` below is the one real integration point: placing
// the IME's candidate-window popup. Reuses the same `PopupManager`/
// `PopupKind` machinery xdg-popups and layer-shell popups already go
// through (`PopupKind::InputMethod` was already a live match arm in
// `xdg_shell.rs`'s commit handler, just unreachable until this). No
// configure/ack handshake exists for this popup type at the protocol
// level, so that existing no-op arm is correct as-is. `parent_geometry`
// uses `mapped_toplevel_window` rather than anvil's `space.elements()`
// scan, since a text field can legitimately be focused on a
// currently-hidden workspace; falls back to a zero rect for a
// non-window parent (a layer-shell surface, say) -- matching anvil's own
// simpler behavior, not solved here.
//
// Virtual-keyboard is the odd one out: its `key`/`modifiers` requests
// bypass `input.rs`'s filter and any active grab entirely by design,
// delivering straight to whichever client actually holds keyboard focus
// (Smithay's `VirtualKeyboardManagerState`, no handler trait). That's
// correct per spec -- an on-screen keyboard or `wtype`-style tool wants
// its keys delivered as literal input, not reinterpreted as a WM
// keybind.
//
// None of the three globals restrict which client can bind them
// (`|_client| true`, matching anvil): any client on the socket can
// register as the system IME or inject synthetic keystrokes into
// whatever's focused. There's no cheaper boundary than
// `security-context-v1` (not implemented) to narrow this with, so it's a
// deliberate, documented gap rather than an oversight -- worth revisiting
// if `security-context-v1` ever lands.
//

impl InputMethodHandler for Smallvil {
    fn new_popup(&mut self, surface: PopupSurface) {
        if let Err(err) = self.popups.track_popup(PopupKind::InputMethod(surface)) {
            tracing::warn!(%err, "Failed to track input-method popup");
        }
    }

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::InputMethod(surface));
        }
    }

    fn popup_repositioned(&mut self, _surface: PopupSurface) {}

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        self.mapped_toplevel_window(parent)
            .map(|window| window.geometry())
            .unwrap_or_default()
    }
}

delegate_text_input_manager!(Smallvil);
delegate_input_method_manager!(Smallvil);
delegate_virtual_keyboard_manager!(Smallvil);

//
// wp-pointer-constraints + wp-relative-pointer
//
// Pointer constraints come in two flavors: locked (pointer frozen in
// place, used for FPS look -- the client gets no motion events but
// receives relative motion through the separate relative-pointer
// protocol below) and confined (pointer visible but can't leave a
// region, used for strategy games and modal menus). The compositor
// decides whether to honor a constraint at `new_constraint` time; the
// actual enforcement -- skipping the regular `motion` call for locked,
// clamping to region for confined -- happens in `input.rs`'s
// `PointerMotion` arm, which is the only path that mutates pointer
// position.
//
// Activation policy: activate the constraint immediately if the pointer
// is currently focused on the constraint's surface (or any subsurface
// of it -- the constraint is set on the toplevel, but pointer focus may
// legitimately land on a subsurface). Otherwise leave it inactive;
// `input.rs` will activate it on the next motion that lands inside the
// region. This matches anvil's own policy. Smithay auto-deactivates
// when the surface loses pointer focus, so we never have to.
//
// Relative-pointer is purely additive: no handler trait, just the
// global. `input.rs` calls `pointer.relative_motion(...)` once per
// motion event and any client that bound `zwp_relative_pointer_v1`
// gets the delta. Existing clients see no change.
//

impl PointerConstraintsHandler for Smallvil {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        let Some(focus) = pointer.current_focus() else {
            return;
        };
        // Walk from the focused subsurface up to its root, activating if
        // the constraint's surface is anywhere in that chain -- the
        // constraint is typically set on the xdg toplevel while focus may
        // be on a subsurface of it.
        let mut current: Option<WlSurface> = Some(focus);
        while let Some(s) = current {
            if &s == surface {
                with_pointer_constraint(surface, pointer, |c| {
                    if let Some(c) = c {
                        if !c.is_active() {
                            c.activate();
                        }
                    }
                });
                return;
            }
            current = smithay::wayland::compositor::get_parent(&s);
        }
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        // The client told us where inside the surface it's rendering the
        // cursor (locked-pointer clients draw their own software cursor).
        // TideWM doesn't currently composite a cursor inside locked
        // pointer state -- the lock hides the system cursor entirely --
        // so there's nothing to do with the hint today. Hook exists for
        // the future water-effects cursor (ripple at the lock point) and
        // for any overlay the user might want.
    }
}

delegate_pointer_constraints!(Smallvil);
delegate_relative_pointer!(Smallvil);
