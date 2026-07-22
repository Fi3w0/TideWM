use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, GestureBeginEvent,
            GestureEndEvent, GesturePinchUpdateEvent as BackendGesturePinchUpdateEvent,
            GestureSwipeUpdateEvent as BackendGestureSwipeUpdateEvent, InputBackend, InputEvent,
            KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
            Switch, SwitchState, SwitchToggleEvent, TouchEvent,
        },
        session::Session,
    },
    desktop::layer_map_for_output,
    input::{
        keyboard::{keysyms, FilterResult, Keysym},
        pointer::{
            AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
            GrabStartData as PointerGrabStartData, MotionEvent, RelativeMotionEvent,
        },
        touch::{
            DownEvent as TouchDownData, MotionEvent as TouchMotionData, UpEvent as TouchUpData,
        },
    },
    reexports::input::{
        AccelProfile, ClickMethod, Device as InputDevice, DeviceConfigResult, DragLockState,
        ScrollMethod,
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::RegionAttributes,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat,
        pointer_constraints::{with_pointer_constraint, PointerConstraint},
        shell::wlr_layer::KeyboardInteractivity,
    },
};

#[cfg(feature = "accessibility")]
use std::time::Duration;

use crate::{
    config::{Action, Keybind, TouchpadConfig},
    grabs::{
        resize_grab::ResizeEdge, MoveSurfaceGrab, ResizeSurfaceGrab, TileMoveGrab, TileResizeGrab,
        TileWindowResizeGrab,
    },
    state::{SessionLock, Smallvil},
    toast::{Toast, ToastKind},
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// Applies `[input.touchpad]` to a libinput device libinput just reported
/// (device-add at startup enumeration, or hotplug), if it's touchpad-class
/// (`config_tap_finger_count() > 0` -- the same check sway/Hyprland use; a
/// device with none, like a plain mouse, doesn't have most of these knobs
/// to begin with). Every field is opt-in: unset means "don't touch this
/// setting." Only called from the udev backend (see `backend/udev.rs`) --
/// winit's simulated devices never carry real libinput config.
pub fn apply_touchpad_config(cfg: &TouchpadConfig, device: &mut InputDevice) {
    if device.config_tap_finger_count() == 0 {
        return;
    }

    if let Some(enabled) = cfg.tap_to_click {
        warn_on_config_err(device.config_tap_set_enabled(enabled), "tap_to_click");
    }
    if let Some(enabled) = cfg.tap_and_drag {
        warn_on_config_err(device.config_tap_set_drag_enabled(enabled), "tap_and_drag");
    }
    if let Some(enabled) = cfg.drag_lock {
        let state = if enabled {
            DragLockState::EnabledTimeout
        } else {
            DragLockState::Disabled
        };
        warn_on_config_err(device.config_tap_set_drag_lock_enabled(state), "drag_lock");
    }
    if let Some(enabled) = cfg.disable_while_typing {
        warn_on_config_err(
            device.config_dwt_set_enabled(enabled),
            "disable_while_typing",
        );
    }
    if let Some(enabled) = cfg.natural_scroll {
        warn_on_config_err(
            device.config_scroll_set_natural_scroll_enabled(enabled),
            "natural_scroll",
        );
    }
    if let Some(enabled) = cfg.left_handed {
        warn_on_config_err(device.config_left_handed_set(enabled), "left_handed");
    }
    if let Some(enabled) = cfg.middle_emulation {
        warn_on_config_err(
            device.config_middle_emulation_set_enabled(enabled),
            "middle_emulation",
        );
    }
    if let Some(speed) = cfg.accel_speed {
        warn_on_config_err(device.config_accel_set_speed(speed), "accel_speed");
    }
    if let Some(profile) = &cfg.accel_profile {
        match profile.as_str() {
            "flat" => warn_on_config_err(
                device.config_accel_set_profile(AccelProfile::Flat),
                "accel_profile",
            ),
            "adaptive" => warn_on_config_err(
                device.config_accel_set_profile(AccelProfile::Adaptive),
                "accel_profile",
            ),
            other => tracing::warn!(value = other, "Unknown accel_profile, ignoring"),
        }
    }
    if let Some(method) = &cfg.click_method {
        match method.as_str() {
            "button-areas" => warn_on_config_err(
                device.config_click_set_method(ClickMethod::ButtonAreas),
                "click_method",
            ),
            "clickfinger" => warn_on_config_err(
                device.config_click_set_method(ClickMethod::Clickfinger),
                "click_method",
            ),
            other => tracing::warn!(value = other, "Unknown click_method, ignoring"),
        }
    }
    if let Some(method) = &cfg.scroll_method {
        match method.as_str() {
            "none" => warn_on_config_err(
                device.config_scroll_set_method(ScrollMethod::NoScroll),
                "scroll_method",
            ),
            "two-finger" => warn_on_config_err(
                device.config_scroll_set_method(ScrollMethod::TwoFinger),
                "scroll_method",
            ),
            "edge" => warn_on_config_err(
                device.config_scroll_set_method(ScrollMethod::Edge),
                "scroll_method",
            ),
            "on-button-down" => warn_on_config_err(
                device.config_scroll_set_method(ScrollMethod::OnButtonDown),
                "scroll_method",
            ),
            other => tracing::warn!(value = other, "Unknown scroll_method, ignoring"),
        }
    }
}

fn warn_on_config_err(result: DeviceConfigResult, setting: &str) {
    if let Err(e) = result {
        tracing::warn!(setting, error = ?e, "Failed to apply touchpad setting");
    }
}

/// Hit-tests `point` (already known to be inside `rect`, since callers only
/// reach this after `Space::element_under` matched the window) against
/// `rect`'s own edges for a no-modifier floating resize -- both niri and
/// Hyprland let a plain border-drag resize a floating window without a
/// modifier held, the tiled equivalent of which is `layout::hit_test_split`
/// (whose "how close counts as a hit" threshold convention this mirrors).
/// `None` means the point is in the interior, an ordinary focus click.
fn floating_resize_edge(
    rect: Rectangle<i32, Logical>,
    point: Point<f64, Logical>,
    threshold: f64,
) -> Option<ResizeEdge> {
    let left = point.x - rect.loc.x as f64;
    let right = (rect.loc.x + rect.size.w) as f64 - point.x;
    let top = point.y - rect.loc.y as f64;
    let bottom = (rect.loc.y + rect.size.h) as f64 - point.y;

    let mut edge = ResizeEdge::empty();
    if left <= threshold {
        edge |= ResizeEdge::LEFT;
    }
    if right <= threshold {
        edge |= ResizeEdge::RIGHT;
    }
    if top <= threshold {
        edge |= ResizeEdge::TOP;
    }
    if bottom <= threshold {
        edge |= ResizeEdge::BOTTOM;
    }
    (!edge.is_empty()).then_some(edge)
}

impl Smallvil {
    /// Maps a touch (or any other absolute-position) event's normalized
    /// coordinates onto logical space, the same way `PointerMotionAbsolute`
    /// already does just below -- first output, no per-device output
    /// binding. A real touch panel is virtually always the built-in one, so
    /// this deliberately matches the existing absolute-pointer convention
    /// rather than anvil's own "prefer eDP, else first" heuristic; revisit
    /// if a real multi-touch-panel setup ever needs per-device binding.
    fn touch_location<I: InputBackend, E: AbsolutePositionEvent<I>>(
        &self,
        event: &E,
    ) -> Option<Point<f64, Logical>> {
        let output = self.space.outputs().next()?;
        let output_geo = self.space.output_geometry(output)?;
        Some(event.position_transformed(output_geo.size) + output_geo.loc.to_f64())
    }

    /// Feeds this keystroke to the accessibility keyboard monitor (a
    /// screen reader's grab/watch registrations), if anything is
    /// listening. Must run *before* `keyboard.input()` is called for this
    /// event, not from inside its filter closure -- see the call site's
    /// own comment in `process_input_event` for why. Reads the seat's
    /// XKB state as it stands *before* this keystroke updates it (matches
    /// niri's own `a11y_process_key`), since a grab decision has to be
    /// made before Smithay's own `keyboard.input()` gets a chance to
    /// mutate that state at all.
    #[cfg(feature = "accessibility")]
    fn a11y_process_key(
        &mut self,
        time: Duration,
        keycode: smithay::backend::input::Keycode,
        key_state: KeyState,
    ) -> crate::accessibility::KbMonBlock {
        if self.accessibility.is_none() {
            return crate::accessibility::KbMonBlock::Pass;
        }

        let keyboard = self.seat.get_keyboard().unwrap();
        let (mods, keysym, unichar) = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            // SAFETY: not changing the ref count, only reading the
            // current state through it -- same justification niri's own
            // reference implementation gives for this identical call.
            let state = unsafe { xkb.state() };
            (
                state.serialize_mods(smithay::input::keyboard::xkb::STATE_MODS_EFFECTIVE),
                state.key_get_one_sym(keycode),
                state.key_get_utf32(keycode),
            )
        });

        let repeat_delay = Duration::from_millis(self.config.input.repeat_delay.max(0) as u64);
        self.accessibility.as_ref().unwrap().process_key(
            repeat_delay,
            time,
            key_state == KeyState::Released,
            mods,
            keysym,
            unichar,
            keycode.raw(),
        )
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Device topology changes aren't user activity; every other variant
        // reaching this function is a real keyboard/pointer/touch event.
        if !matches!(
            event,
            InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
        ) {
            self.idle_notifier_state.notify_activity(&self.seat);
        }

        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let key_state = event.state();
                tracing::trace!(keycode = ?event.key_code(), ?key_state, "Raw key event");

                // Accessibility keyboard grabs (a screen reader watching or
                // grabbing keys system-wide, `org.freedesktop.a11y.KeyboardMonitor`)
                // have to be decided *before* `keyboard.input()` runs at
                // all, not from inside its filter closure below: a grabbed
                // modifier's first press must not touch XKB state (e.g. a
                // Caps-Lock-shaped toggle), and `keyboard.input()` itself
                // is what updates that state, before the closure ever
                // runs. See `Smallvil::a11y_process_key`'s own doc for why
                // this mirrors niri's own `KeyboardMonitor` positioning
                // exactly. `a11y_block` (a plain `bool`, not the 3-way
                // enum) is what the closure below actually branches on, so
                // its body compiles identically regardless of whether the
                // `accessibility` feature is on.
                #[cfg(feature = "accessibility")]
                let a11y_block = {
                    let block = self.a11y_process_key(
                        Duration::from_millis(u64::from(time)),
                        event.key_code(),
                        key_state,
                    );
                    if block == crate::accessibility::KbMonBlock::ModifierFirstPress {
                        return;
                    }
                    block != crate::accessibility::KbMonBlock::Pass
                };
                #[cfg(not(feature = "accessibility"))]
                let a11y_block = false;

                let keyboard = self.seat.get_keyboard().unwrap();
                keyboard.input::<(), _>(
                    self,
                    event.key_code(),
                    key_state,
                    serial,
                    time,
                    |data, modifiers, handle| {
                        // VT-switch is the hardware/session-level escape
                        // hatch and must stay reachable even if
                        // accessibility (buggy or otherwise) is currently
                        // grabbing everything -- checked first, before
                        // `a11y_block` gets a say, same reasoning the
                        // lock/inhibit gates below already use for their
                        // own escape-hatch ordering. Wrapped in its own
                        // press check (rather than relying on the shared
                        // guard further down, which now comes after this)
                        // to keep this block's own behavior byte-for-byte
                        // unchanged from before.
                        if key_state == KeyState::Pressed {
                            // Ctrl+Alt+F<N>: usually resolves to a distinct
                            // XF86Switch_VT_N keysym once libinput/DRM (not a host
                            // compositor) owns the keyboard -- xkbcommon's default
                            // keymap maps that combo to this range at the
                            // *modified* level, not literally "F<N> plus two held
                            // modifiers", which is why this checks
                            // `modified_sym()` rather than the `raw_syms()`
                            // keybind matching below uses. Not every keymap
                            // resolves it that way, though, so this also falls
                            // back to modifiers+base-keysym: raw_syms() for an
                            // F-key is always the plain F<N> symbol regardless of
                            // modifiers (F-keys have no shift level), so that
                            // check doesn't depend on the keymap doing anything
                            // special at all. No-op under winit, where
                            // `data.session` stays `None` -- there's a host
                            // compositor already handling VT switches there.
                            let modified = handle.modified_sym().raw();
                            let raw = handle.raw_syms().first().map(|s| s.raw());
                            tracing::debug!(
                                ?modified,
                                ?raw,
                                ?modifiers,
                                "Key pressed (checked against VT-switch)"
                            );
                            let vt_switch = (keysyms::KEY_XF86Switch_VT_1
                                ..=keysyms::KEY_XF86Switch_VT_12)
                                .contains(&modified)
                                .then(|| (modified - keysyms::KEY_XF86Switch_VT_1 + 1) as i32)
                                .or_else(|| {
                                    if !(modifiers.ctrl && modifiers.alt) {
                                        return None;
                                    }
                                    let raw = raw?;
                                    (keysyms::KEY_F1..=keysyms::KEY_F12)
                                        .contains(&raw)
                                        .then(|| (raw - keysyms::KEY_F1 + 1) as i32)
                                });
                            if let Some(vt) = vt_switch {
                                match data.session.as_mut() {
                                    Some(session) => {
                                        tracing::info!(vt, "Switching VT");
                                        if let Err(err) = session.change_vt(vt) {
                                            tracing::warn!(vt, %err, "Failed to switch VT");
                                        }
                                    }
                                    None => tracing::debug!(
                                        vt,
                                        "VT-switch key seen but no session (winit backend)"
                                    ),
                                }
                                return FilterResult::Intercept(());
                            }
                        }

                        if a11y_block {
                            // Mirrors niri's own workaround: forward a
                            // grabbed *modifier's* release to the client
                            // anyway, since Wayland's own
                            // wl_keyboard.enter/modifiers events can leak
                            // it to a freshly-focused client regardless
                            // (e.g. opening an a11y tool's own menu with a
                            // modifier still logically held) -- better to
                            // let that one case through than have the
                            // client's own key presses silently do
                            // nothing until the modifier is re-tapped.
                            let is_modifier = matches!(
                                handle.modified_sym(),
                                Keysym::Shift_L
                                    | Keysym::Shift_R
                                    | Keysym::Control_L
                                    | Keysym::Control_R
                                    | Keysym::Super_L
                                    | Keysym::Super_R
                                    | Keysym::Alt_L
                                    | Keysym::Alt_R
                            );
                            if key_state != KeyState::Pressed && is_modifier {
                                return FilterResult::Forward;
                            }
                            return FilterResult::Intercept(());
                        }

                        if key_state != KeyState::Pressed {
                            return FilterResult::Forward;
                        }

                        // Locked: no WM keybind fires (Super+anything must
                        // not reach `run_action` while the screen is
                        // supposed to be secured), but VT-switch above
                        // still works -- that's a hardware/session-level
                        // escape hatch, not something a lock client can or
                        // should be able to withhold. The key still
                        // forwards, landing on whatever `wl_keyboard`
                        // focus `reconcile_keyboard_focus` set (the lock
                        // surface, or nothing yet).
                        if !matches!(data.session_lock, SessionLock::Unlocked) {
                            return FilterResult::Forward;
                        }

                        // wp-keyboard-shortcuts-inhibit: the focused client
                        // holds an active inhibitor (a VM or remote-desktop
                        // session capturing input for its guest), so every
                        // combo below -- including the WM's own Super
                        // keybinds -- forwards to it instead of
                        // intercepting. VT-switch and the lock gate above
                        // both still win, same reasoning as the lock case:
                        // a stuck or malicious guest can't take away
                        // either escape hatch.
                        if data
                            .keyboard_focused_surface()
                            .and_then(|surface| {
                                data.seat.keyboard_shortcuts_inhibitor_for_surface(surface)
                            })
                            .is_some_and(|inhibitor| inhibitor.is_active())
                        {
                            return FilterResult::Forward;
                        }

                        let keysym = match handle.raw_syms().first().cloned() {
                            Some(keysym) => keysym,
                            None => return FilterResult::Forward,
                        };
                        tracing::trace!(?keysym, ?modifiers, "Key pressed");

                        // A submap fully replaces the base table rather
                        // than layering on top of it (matches sway/
                        // Hyprland: while "in a mode," only that mode's
                        // own binds fire as compositor actions -- an
                        // empty slice for an active_submap name that
                        // somehow isn't in `submaps` shouldn't happen
                        // (reload_config clears it if the name vanishes),
                        // but degrades safely to "nothing matches,
                        // everything forwards" rather than panicking.
                        let table: &[Keybind] = match &data.active_submap {
                            Some(name) => data
                                .config
                                .submaps
                                .get(name)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]),
                            None => &data.config.keybinds,
                        };
                        let action = table
                            .iter()
                            .find(|bind| bind.keysym == keysym && bind.mods.matches(modifiers))
                            .map(|bind| bind.action.clone());

                        match action {
                            Some(action) => {
                                tracing::trace!("Matched keybind action");
                                data.run_action(action);
                                FilterResult::Intercept(())
                            }
                            None => FilterResult::Forward,
                        }
                    },
                );
            }
            InputEvent::PointerMotion { event, .. } => {
                self.note_pointer_motion();
                // Relative motion: what every real mouse/trackpad sends
                // (absolute is tablets/touchscreens, or a nested backend's
                // host compositor giving already-absolute coordinates).
                // Accumulates the event's delta onto the pointer's own
                // last-known position, clamped so it can't wander off the
                // mapped output(s) entirely.
                let pointer = self.seat.get_pointer().unwrap();
                let current_loc = pointer.current_location();
                let under = self.surface_under(current_loc);

                // Always emit relative motion first, regardless of any
                // active pointer constraint: wp_relative_pointer is an
                // independent protocol that any client can opt into (FPS
                // clients bind both this and pointer-constraints, but
                // non-FPS clients like design tools use it for
                // pressure-sensitive input too). Constraint enforcement
                // only gates the regular `motion` call below.
                pointer.relative_motion(
                    self,
                    under.as_ref().map(|(s, l)| (s.clone(), *l)),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );

                // Walk up from the focused surface (which may be a
                // subsurface) to find any constraint registered on its
                // xdg-toplevel root. Constraints are typically set on the
                // toplevel; the focused surface may be a subsurface.
                let constraint_root = under
                    .as_ref()
                    .and_then(|(s, _)| self.root_with_constraint(s, &pointer));

                // Inspect the active constraint, if any, and decide what
                // kind of clamp/skip applies this event.
                let mut locked = false;
                let mut confined = false;
                let mut confine_region: Option<RegionAttributes> = None;
                if let Some(root) = constraint_root.as_ref() {
                    with_pointer_constraint(root, &pointer, |c| match c {
                        Some(c) if c.is_active() => match &*c {
                            PointerConstraint::Locked(_) => locked = true,
                            PointerConstraint::Confined(confined_ptr) => {
                                confined = true;
                                // None means "whole surface"; we model that
                                // by leaving confine_region empty and
                                // checking surface membership instead.
                                if let Some(region) = confined_ptr.region() {
                                    confine_region = Some(region.clone());
                                }
                            }
                        },
                        _ => {}
                    });
                }

                // Locked pointer: client gets only relative motion (already
                // emitted above). Skip the regular motion call entirely so
                // the cursor stays put; just close out the frame.
                if locked {
                    pointer.frame(self);
                    if self.udev_renderer.is_some() {
                        self.request_redraw();
                    }
                    return;
                }

                let new_loc = self.clamp_to_outputs(current_loc + event.delta());

                // Confined pointer: drop events that would leave either the
                // constraint's region (if specified) or the surface itself
                // (if no region). The surface check uses `under` (the
                // pre-motion focus) so the pointer can still move within
                // the same surface without falling off the edge.
                if confined {
                    let surface_loc = under.as_ref().map(|(_, l)| *l).unwrap_or_default();
                    let mut drop_motion = false;
                    if let Some(region) = &confine_region {
                        let local = (new_loc - surface_loc).to_i32_round();
                        if !region.contains(local) {
                            drop_motion = true;
                        }
                    } else if let Some(root_surf) = constraint_root.as_ref() {
                        // No region: confine to the surface itself. If the
                        // surface-under at the new location has a different
                        // root, drop the motion.
                        let new_under = self.surface_under(new_loc);
                        let new_root = new_under.as_ref().and_then(|(s, _)| self.surface_root(s));
                        if new_root.as_ref() != Some(root_surf) {
                            drop_motion = true;
                        }
                    }
                    if drop_motion {
                        pointer.frame(self);
                        if self.udev_renderer.is_some() {
                            self.request_redraw();
                        }
                        return;
                    }
                }

                let serial = SERIAL_COUNTER.next_serial();
                let new_under = self.surface_under(new_loc);

                pointer.motion(
                    self,
                    new_under.clone(),
                    &MotionEvent {
                        location: new_loc,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                self.focus_follows_mouse(new_loc);

                // After moving, activate any constraint the pointer is now
                // sitting inside. The protocol requires the compositor to
                // call `activate()` to send `locked`/`confined` to the
                // client, and clients wait on that signal before assuming
                // the constraint is in effect.
                if let Some((root_surf, surface_loc)) = new_under
                    .as_ref()
                    .and_then(|(s, l)| Some((self.root_with_constraint(s, &pointer)?, *l)))
                {
                    with_pointer_constraint(&root_surf, &pointer, |c| {
                        if let Some(c) = c {
                            if !c.is_active() {
                                let local = (new_loc - surface_loc).to_i32_round();
                                let in_region =
                                    c.region().map(|r| r.contains(local)).unwrap_or(true);
                                if in_region {
                                    c.activate();
                                }
                            }
                        }
                    });
                }

                if self.udev_renderer.is_some() {
                    self.request_redraw();
                }
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                self.note_pointer_motion();
                // No output to map an absolute position onto -- reachable
                // during shutdown if a host compositor (winit backend)
                // delivers a final motion event after outputs are already
                // torn down. Nothing sensible to do with it; drop it rather
                // than panic on the `Space` lookups below.
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                let serial = SERIAL_COUNTER.next_serial();

                let pointer = self.seat.get_pointer().unwrap();

                let under = self.surface_under(pos);

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                self.focus_follows_mouse(pos);

                // Only the udev backend composites its own cursor (see
                // cursor.rs); under winit the host draws the real one, so
                // marking every motion dirty there would just burn cycles
                // recompositing a frame that looks identical either way.
                if self.udev_renderer.is_some() {
                    self.request_redraw();
                }
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let keyboard = self.seat.get_keyboard().unwrap();

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();

                // Locked: forward the click to whatever `surface_under`
                // already resolved (the lock surface, via the motion path
                // that ran before this button event) and skip every WM
                // action below -- no raise, no focus-window, no grab-start.
                // Same shape as the Exclusive-layer early-return further
                // down, just unconditional.
                if !matches!(self.session_lock, SessionLock::Unlocked) {
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state: button_state,
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(self);
                    return;
                }

                // A popup pointer grab still owns dispatch, but a press is
                // also an ordinary WM activation gesture. Resolve only the
                // plain focus/raise part here; move/resize/split grabs remain
                // forbidden while any pointer grab is live. Clicking a
                // different root makes the centralized focus authority
                // dismiss the popup before Smithay forwards the press.
                if button_state == ButtonState::Pressed
                    && pointer.is_grabbed()
                    && self.popup_grab.is_some()
                {
                    let pos = pointer.current_location();
                    if let Some(layer) = self.layer_under_pointer(pos) {
                        if layer.cached_state().keyboard_interactivity
                            != KeyboardInteractivity::None
                        {
                            self.focus_layer(layer.wl_surface().clone(), serial);
                        }
                    } else if self.exclusive_layer().is_none() {
                        if let Some((window, _)) = self.space.element_under(pos) {
                            let window = window.clone();
                            let surface = window.toplevel().unwrap().wl_surface().clone();
                            if !self.layout.contains(&surface) {
                                self.space.raise_element(&window, false);
                            }
                            self.focus_window(Some(surface), serial);
                        } else {
                            self.focus_window(None, serial);
                        }
                    }
                }

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    // Layer surfaces (bars, launchers, lock screens) sit
                    // outside window management entirely: no raise, no
                    // tiling, no grabs, just their own declared keyboard
                    // interactivity. Checked first so a launcher/bar
                    // visually overlapping the tiled area (most don't
                    // reserve an exclusive zone) doesn't fall through into
                    // window-click or split-drag handling underneath it.
                    if let Some(layer) = self.layer_under_pointer(pointer.current_location()) {
                        if layer.cached_state().keyboard_interactivity
                            != KeyboardInteractivity::None
                        {
                            self.focus_layer(layer.wl_surface().clone(), serial);
                        }
                        pointer.button(
                            self,
                            &ButtonEvent {
                                button,
                                state: button_state,
                                serial,
                                time: event.time_msec(),
                            },
                        );
                        pointer.frame(self);
                        return;
                    }

                    // An Exclusive layer owns keyboard interaction for its
                    // mapped lifetime. A click outside its geometry must not
                    // raise, activate, resize, or start a grab on an ordinary
                    // window behind it (important for lock-screen surfaces).
                    if self.exclusive_layer().is_some() {
                        pointer.button(
                            self,
                            &ButtonEvent {
                                button,
                                state: button_state,
                                serial,
                                time: event.time_msec(),
                            },
                        );
                        pointer.frame(self);
                        return;
                    }

                    let under = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, l)| (w.clone(), l));

                    // Super+drag moves/resizes a floating window, the same
                    // convention Hyprland and most tiling WMs use so you
                    // don't need decorations to reposition anything.
                    //
                    // Super+Left-drag on a *tiled* window instead picks it up
                    // for drag-to-swap (TileMoveGrab, grabs/tile_move_grab.rs)
                    // -- dropping it on another tile swaps the two, dropping
                    // anywhere else snaps it back. This shipped once, froze
                    // the entire machine on its first real-hardware test
                    // (unresponsive even to VT-switch, hard reboot needed),
                    // and was disabled immediately. Root cause since found
                    // and fixed: `TileMoveGrab::drop()` called
                    // `seat.get_pointer().current_location()` from inside the
                    // grab's own `button()` callback, which already holds
                    // that same pointer mutex for the whole dispatch -- a
                    // guaranteed self-deadlock, confirmed against Smithay's
                    // source. Fixed by tracking pointer location from motion
                    // events instead, same as MoveSurfaceGrab already does.
                    // Re-enabled here for the real-hardware retest this
                    // project's own standing rule required before trusting
                    // it again -- see AGENT.md/CHANGELOG.md.
                    let super_drag = keyboard.modifier_state().logo
                        && (button == BTN_LEFT || button == BTN_RIGHT);
                    if super_drag {
                        if let Some((window, loc)) = under.clone() {
                            let wl_surface = window.toplevel().unwrap().wl_surface().clone();
                            if self.fullscreen.contains_key(&wl_surface)
                                || self.maximized.contains_key(&wl_surface)
                            {
                                // Fullscreen/maximized are output-owned placements;
                                // a compositor drag must not move/resize it
                                // behind the protocol state's back.
                            } else if !self.layout.contains(&wl_surface) {
                                self.space.raise_element(&window, false);
                                self.focus_window(Some(wl_surface.clone()), serial);

                                let start_data = PointerGrabStartData {
                                    focus: Some((wl_surface, loc.to_f64())),
                                    button,
                                    location: pointer.current_location(),
                                };

                                if button == BTN_LEFT {
                                    let grab = MoveSurfaceGrab {
                                        start_data,
                                        window,
                                        initial_window_location: loc,
                                    };
                                    pointer.set_grab(self, grab, serial, Focus::Clear);
                                } else {
                                    let initial_rect = Rectangle::new(loc, window.geometry().size);
                                    let grab = ResizeSurfaceGrab::start(
                                        start_data,
                                        window,
                                        ResizeEdge::BOTTOM_RIGHT,
                                        initial_rect,
                                    );
                                    pointer.set_grab(self, grab, serial, Focus::Clear);
                                }
                                return;
                            } else if button == BTN_LEFT {
                                let output = self.layout.output_of(&wl_surface).map(str::to_string);
                                let workspace = self.layout.workspace_of(&wl_surface);
                                if let (Some(output), Some(workspace)) = (output, workspace) {
                                    self.focus_window(Some(wl_surface.clone()), serial);

                                    let start_data = PointerGrabStartData {
                                        focus: Some((wl_surface.clone(), loc.to_f64())),
                                        button,
                                        location: pointer.current_location(),
                                    };
                                    let grab = TileMoveGrab::start(
                                        start_data, window, wl_surface, output, workspace, loc,
                                    );
                                    pointer.set_grab(self, grab, serial, Focus::Clear);
                                    return;
                                }
                            } else if button == BTN_RIGHT {
                                // Resize a tiled window by dragging its own
                                // body, matching Hyprland's own
                                // `bindm ... resizewindow` -- the tiled
                                // counterpart to the floating Super+Right-drag
                                // resize above, and distinct from
                                // TileResizeGrab (no modifier, requires
                                // hitting the shared border pixel-exactly).
                                let output = self.output_for_window(&window);
                                let area = output.as_ref().and_then(|o| self.output_tiling_area(o));
                                if let (Some(output), Some(area)) = (output, area) {
                                    let workspace = self.layout.active_workspace(&output.name());
                                    let handles: Vec<_> = self
                                        .layout
                                        .resize_splits(&output.name(), workspace, area, &wl_surface)
                                        .into_iter()
                                        .filter_map(|hit| {
                                            let ratio = self.layout.ratio_at(
                                                &hit.output,
                                                hit.workspace,
                                                &hit.path,
                                            )?;
                                            Some((hit, ratio))
                                        })
                                        .collect();
                                    if !handles.is_empty() {
                                        self.focus_window(Some(wl_surface.clone()), serial);

                                        let start_data = PointerGrabStartData {
                                            focus: Some((wl_surface, loc.to_f64())),
                                            button,
                                            location: pointer.current_location(),
                                        };
                                        let grab = TileWindowResizeGrab::start(start_data, handles);
                                        pointer.set_grab(self, grab, serial, Focus::Clear);
                                        return;
                                    }
                                }
                            }
                        }
                    }

                    // A plain (no-modifier) left click landing on a
                    // floating window's own edge resizes it directly, the
                    // same convention niri and Hyprland both use -- the
                    // floating counterpart to the tiled hit_test_split drag
                    // just below. Skipped entirely once `super_drag` above
                    // already claimed the click.
                    if !super_drag && button == BTN_LEFT {
                        if let Some((window, loc)) = under.clone() {
                            let wl_surface = window.toplevel().unwrap().wl_surface().clone();
                            if !self.layout.contains(&wl_surface)
                                && !self.fullscreen.contains_key(&wl_surface)
                                && !self.maximized.contains_key(&wl_surface)
                            {
                                let rect = Rectangle::new(loc, window.geometry().size);
                                let threshold = (self.config.gaps as f64).max(4.0);
                                if let Some(edge) = floating_resize_edge(
                                    rect,
                                    pointer.current_location(),
                                    threshold,
                                ) {
                                    self.space.raise_element(&window, false);
                                    self.focus_window(Some(wl_surface.clone()), serial);

                                    let start_data = PointerGrabStartData {
                                        focus: Some((wl_surface, loc.to_f64())),
                                        button,
                                        location: pointer.current_location(),
                                    };
                                    let grab =
                                        ResizeSurfaceGrab::start(start_data, window, edge, rect);
                                    pointer.set_grab(self, grab, serial, Focus::Clear);
                                    return;
                                }
                            }
                        }
                    }

                    // A plain click that hit nothing might still have
                    // landed in the gap between two tiled windows -- drag
                    // that to adjust the split ratio, the tiled equivalent
                    // of the Super+drag floating resize above. No modifier
                    // needed, matching how i3/sway let you drag borders
                    // directly; a click that's genuinely on empty desktop
                    // won't be within hit_test_split's threshold of any
                    // split boundary.
                    if under.is_none() && button == BTN_LEFT {
                        let hit =
                            self.output_for_point(pointer.current_location())
                                .and_then(|output| {
                                    let output_geo = self.space.output_geometry(&output)?;
                                    let mut area =
                                        layer_map_for_output(&output).non_exclusive_zone();
                                    area.loc += output_geo.loc;
                                    let workspace = self.layout.active_workspace(&output.name());
                                    self.layout.hit_test_split(
                                        &output.name(),
                                        workspace,
                                        area,
                                        self.config.gaps,
                                        pointer.current_location(),
                                    )
                                });
                        if let Some(hit) = hit {
                            if let Some(start_ratio) =
                                self.layout.ratio_at(&hit.output, hit.workspace, &hit.path)
                            {
                                let start_data = PointerGrabStartData {
                                    focus: None,
                                    button,
                                    location: pointer.current_location(),
                                };
                                let grab = TileResizeGrab::start(start_data, hit, start_ratio);
                                pointer.set_grab(self, grab, serial, Focus::Clear);
                                return;
                            }
                        }
                    }

                    if let Some((window, _loc)) = under {
                        let wl_surface = window.toplevel().unwrap().wl_surface().clone();

                        if !self.layout.contains(&wl_surface) {
                            self.space.raise_element(&window, false);
                        }
                        self.focus_window(Some(wl_surface), serial);
                    } else {
                        self.focus_window(None, serial);
                    }
                };

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::TouchDown { event, .. } => {
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                let Some(location) = self.touch_location(&event) else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(location);
                touch.down(
                    self,
                    under.clone(),
                    &TouchDownData {
                        slot: event.slot(),
                        location,
                        serial,
                        time: event.time_msec(),
                    },
                );
            }
            InputEvent::TouchMotion { event, .. } => {
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                let Some(location) = self.touch_location(&event) else {
                    return;
                };
                let under = self.surface_under(location);
                touch.motion(
                    self,
                    under,
                    &TouchMotionData {
                        slot: event.slot(),
                        location,
                        time: event.time_msec(),
                    },
                );
            }
            InputEvent::TouchUp { event, .. } => {
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                touch.up(
                    self,
                    &TouchUpData {
                        slot: event.slot(),
                        serial,
                        time: event.time_msec(),
                    },
                );
            }
            InputEvent::TouchFrame { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.frame(self);
                }
            }
            InputEvent::TouchCancel { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.cancel(self);
                }
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();

                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.
                });
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }

                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            InputEvent::SwitchToggle { event, .. } => {
                // libinput's switch capability (laptop lid, tablet-mode
                // hinge) -- udev backend only, winit can't surface it. The
                // `notify_activity` call above already ran for this event,
                // which is correct for lid-open (user came back) and a
                // harmless no-op for lid-close: on a system running
                // systemd-logind, its own `HandleLidSwitch=` policy decides
                // suspend/lock/ignore independently, so we're not racing it
                // either way -- and on a system without logind, nothing else
                // is racing this either. A
                // `switch()` of `None` would mean libinput introduced a
                // switch type Smithay doesn't name -- log and ignore
                // rather than panic on a future enum extension.
                let Some(switch) = event.switch() else {
                    tracing::debug!("Switch toggle for an unknown switch type, ignoring");
                    return;
                };
                let state = event.state();
                match switch {
                    Switch::Lid => {
                        let closed = state == SwitchState::On;
                        if std::mem::replace(&mut self.is_lid_closed, closed) == closed {
                            tracing::trace!(
                                closed,
                                "Lid switch event with no state change, ignoring"
                            );
                            return;
                        }
                        tracing::info!(closed, "Lid {}", if closed { "closed" } else { "opened" });
                        let action = if closed {
                            self.config.switch_events.lid_close.clone()
                        } else {
                            self.config.switch_events.lid_open.clone()
                        };
                        if let Some(action) = action {
                            self.run_action(action);
                        }
                    }
                    Switch::TabletMode => {
                        let tablet = state == SwitchState::On;
                        if std::mem::replace(&mut self.is_tablet_mode, tablet) == tablet {
                            tracing::trace!(
                                tablet,
                                "Tablet-mode switch event with no state change, ignoring"
                            );
                            return;
                        }
                        tracing::info!(
                            tablet,
                            "Tablet mode {}",
                            if tablet { "entered" } else { "left" }
                        );
                        let action = if tablet {
                            self.config.switch_events.tablet_mode_on.clone()
                        } else {
                            self.config.switch_events.tablet_mode_off.clone()
                        };
                        if let Some(action) = action {
                            self.run_action(action);
                        }
                    }
                    // `Switch` is `#[non_exhaustive]` so Smithay can add
                    // new switch kinds without a downstream type error.
                    // We have no semantics or config binding for any
                    // unknown kind, just log it.
                    _ => tracing::debug!(?switch, "Unhandled switch type, ignoring"),
                }
            }
            // wp-pointer-gestures: real libinput touchpads (udev backend
            // only -- winit never emits these, same as relative motion)
            // report swipe/pinch/hold as their own event stream, not as
            // pointer motion. Forward to the pointer handle unchanged:
            // Smithay routes through any active grab first (the grabs all
            // pass these through to the default grab), and the default
            // grab delivers to whichever client created gesture objects
            // for the focused surface. No client bound the protocol, the
            // events simply go nowhere.
            InputEvent::GestureSwipeBegin { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_swipe_begin(
                    self,
                    &GestureSwipeBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: Event::time_msec(&event),
                        fingers: GestureBeginEvent::fingers(&event),
                    },
                );
            }
            InputEvent::GestureSwipeUpdate { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_swipe_update(
                    self,
                    &GestureSwipeUpdateEvent {
                        time: Event::time_msec(&event),
                        delta: BackendGestureSwipeUpdateEvent::delta(&event),
                    },
                );
            }
            InputEvent::GestureSwipeEnd { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_swipe_end(
                    self,
                    &GestureSwipeEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: Event::time_msec(&event),
                        cancelled: GestureEndEvent::cancelled(&event),
                    },
                );
            }
            InputEvent::GesturePinchBegin { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_pinch_begin(
                    self,
                    &GesturePinchBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: Event::time_msec(&event),
                        fingers: GestureBeginEvent::fingers(&event),
                    },
                );
            }
            InputEvent::GesturePinchUpdate { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_pinch_update(
                    self,
                    &GesturePinchUpdateEvent {
                        time: Event::time_msec(&event),
                        delta: BackendGesturePinchUpdateEvent::delta(&event),
                        scale: BackendGesturePinchUpdateEvent::scale(&event),
                        rotation: BackendGesturePinchUpdateEvent::rotation(&event),
                    },
                );
            }
            InputEvent::GesturePinchEnd { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_pinch_end(
                    self,
                    &GesturePinchEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: Event::time_msec(&event),
                        cancelled: GestureEndEvent::cancelled(&event),
                    },
                );
            }
            InputEvent::GestureHoldBegin { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_hold_begin(
                    self,
                    &GestureHoldBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: Event::time_msec(&event),
                        fingers: GestureBeginEvent::fingers(&event),
                    },
                );
            }
            InputEvent::GestureHoldEnd { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_hold_end(
                    self,
                    &GestureHoldEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: Event::time_msec(&event),
                        cancelled: GestureEndEvent::cancelled(&event),
                    },
                );
            }
            _ => {}
        }
    }

    pub(crate) fn run_action(&mut self, action: Action) {
        match action {
            Action::Spawn(cmd) => {
                if let Err(err) = crate::spawn(&cmd) {
                    tracing::warn!(%err, cmd, "Failed to spawn command");
                    self.toast = Some(Toast::new(
                        &format!("Failed to spawn: {cmd}"),
                        ToastKind::Error,
                    ));
                    self.request_redraw();
                }
            }
            Action::CloseWindow => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    let window = self
                        .space
                        .elements()
                        .find(|w| {
                            w.toplevel()
                                .map(|t| t.wl_surface() == &surface)
                                .unwrap_or(false)
                        })
                        .cloned();
                    if let Some(window) = window {
                        window.toplevel().unwrap().send_close();
                    }
                }
            }
            Action::ToggleFloating => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.toggle_floating(&surface);
                }
            }
            Action::ToggleFullscreen => {
                self.toggle_fullscreen();
            }
            Action::TogglePin => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.toggle_pin(&surface);
                }
            }
            Action::TogglePseudoTile => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.toggle_pseudo_tile(&surface);
                }
            }
            Action::RaiseWindow => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.raise_window(&surface);
                }
            }
            Action::LowerWindow => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.lower_window(&surface);
                }
            }
            Action::FocusUrgent => {
                self.focus_urgent();
            }
            Action::ToggleDpms => {
                self.toggle_dpms();
            }
            Action::ToggleScratchpad => {
                if let Some(output) = self.primary_output() {
                    self.toggle_scratchpad(&output);
                }
            }
            Action::MoveToScratchpad => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.move_to_scratchpad(&surface);
                }
            }
            Action::CycleFocus => {
                self.cycle_focus();
            }
            Action::FocusDirection(direction) => {
                self.focus_direction(direction);
            }
            Action::SwapDirection(direction) => {
                self.swap_direction(direction);
            }
            Action::GroupDirection(direction) => {
                self.group_direction(direction);
            }
            Action::Ungroup => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.ungroup(&surface);
                }
            }
            Action::CycleTabForward => {
                self.cycle_tab(true);
            }
            Action::CycleTabBackward => {
                self.cycle_tab(false);
            }
            Action::SwitchWorkspace(workspace) => {
                let Some(workspace) = self.resolve_workspace_ref(&workspace) else {
                    return;
                };
                // Always the currently-focused/pointer output -- same
                // resolution order `primary_output` uses everywhere else --
                // so the keybind acts on whichever monitor you're actually
                // looking at, not some fixed one.
                if let Some(output) = self.primary_output() {
                    self.switch_workspace(&output, workspace);
                }
            }
            Action::MoveToWorkspace(workspace) => {
                let Some(workspace) = self.resolve_workspace_ref(&workspace) else {
                    return;
                };
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.move_to_workspace(&surface, workspace);
                }
            }
            Action::SwapWorkspacesWithOutput(name) => {
                let Some(this_output) = self.primary_output() else {
                    return;
                };
                let Some(other_output) = self.output_by_name(&name) else {
                    tracing::warn!(output = %name, "swap-workspaces: no such output");
                    return;
                };
                self.swap_workspaces(&this_output, &other_output);
            }
            Action::EnterSubmap(name) => {
                if self.config.submaps.contains_key(&name) {
                    self.active_submap = Some(name);
                } else {
                    tracing::warn!(name, "submap not found in config, ignoring");
                }
            }
            Action::ExitSubmap => {
                self.active_submap = None;
            }
            Action::SetLayout(algorithm) => {
                self.set_layout_algorithm(algorithm);
            }
            Action::GrowMaster => {
                self.adjust_master_ratio(0.05);
            }
            Action::ShrinkMaster => {
                self.adjust_master_ratio(-0.05);
            }
            Action::ToggleOverview => {
                self.toggle_overview();
            }
            Action::Quit => {
                self.loop_signal.stop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::{Point, Size};

    fn rect() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((100, 100)), Size::from((200, 200)))
    }

    #[test]
    fn interior_click_is_not_a_resize() {
        assert_eq!(
            floating_resize_edge(rect(), Point::from((200.0, 200.0)), 4.0),
            None
        );
    }

    #[test]
    fn near_each_single_edge_hits_only_that_edge() {
        let r = rect();
        assert_eq!(
            floating_resize_edge(r, Point::from((101.0, 200.0)), 4.0),
            Some(ResizeEdge::LEFT)
        );
        assert_eq!(
            floating_resize_edge(r, Point::from((299.0, 200.0)), 4.0),
            Some(ResizeEdge::RIGHT)
        );
        assert_eq!(
            floating_resize_edge(r, Point::from((200.0, 101.0)), 4.0),
            Some(ResizeEdge::TOP)
        );
        assert_eq!(
            floating_resize_edge(r, Point::from((200.0, 299.0)), 4.0),
            Some(ResizeEdge::BOTTOM)
        );
    }

    #[test]
    fn corner_hits_both_adjacent_edges() {
        assert_eq!(
            floating_resize_edge(rect(), Point::from((101.0, 101.0)), 4.0),
            Some(ResizeEdge::TOP_LEFT)
        );
        assert_eq!(
            floating_resize_edge(rect(), Point::from((299.0, 299.0)), 4.0),
            Some(ResizeEdge::BOTTOM_RIGHT)
        );
    }
}
