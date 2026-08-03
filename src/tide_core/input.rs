//! Input dispatch and compositor action handling.

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
        keyboard::{keysyms, FilterResult, Keysym, ModifiersState},
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
    utils::{Logical, Point, Rectangle, Size, SERIAL_COUNTER},
    wayland::{
        compositor::RegionAttributes,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat,
        pointer_constraints::{with_pointer_constraint, PointerConstraint},
        shell::wlr_layer::KeyboardInteractivity,
    },
};

use std::{collections::HashSet, time::Duration};

use crate::{
    config::{Action, Direction, Keybind, Mods, TouchpadConfig},
    grabs::{
        resize_grab::ResizeEdge, CascadeResizeGrab, MoveSurfaceGrab, OceanPanGrab,
        OceanTileMoveGrab, ResizeSurfaceGrab, TileMoveGrab, TileResizeGrab, TileWindowResizeGrab,
    },
    state::{CompositorGesture, SessionLock, Smallvil},
    toast::{Toast, ToastKind},
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// Resolve one key press from an authoritative Waves table. When a user has
/// both `H` and `P+H`, holding P must pick the more specific chord regardless
/// of the HashMap iteration order the config was lowered from. Additional held
/// helpers therefore select the matching bind with the largest helper set.
fn matching_keybind<'a>(
    table: &'a [Keybind],
    keysym: Keysym,
    modifiers: &ModifiersState,
    helpers: &HashSet<Keysym>,
    ocean_selected: bool,
) -> Option<&'a Keybind> {
    table
        .iter()
        .filter(|bind| {
            bind.keysym == keysym
                && bind.mods.matches(modifiers)
                && bind.held_keys_match(helpers)
                && (ocean_selected || !crate::config::is_ocean_action(&bind.action))
        })
        .max_by_key(|bind| bind.held_keysyms.len())
}

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

/// Resolves a completed horizontal workspace swipe. Vertical/diagonal
/// gestures are ignored even if their X component happens to cross the
/// threshold, and workspace numbering never wraps through the reserved
/// scratchpad workspace 0.
fn workspace_swipe_target(current: u32, delta_x: f64, delta_y: f64, threshold: f64) -> Option<u32> {
    if delta_x.abs() < threshold || delta_x.abs() <= delta_y.abs() {
        return None;
    }
    if delta_x < 0.0 {
        current.checked_add(1)
    } else if current > 1 {
        Some(current - 1)
    } else {
        None
    }
}

/// Resolves how many of a swim drag's requested whole-spot `advances` can
/// actually land, applying the same boundary guard `workspace_swipe_target`
/// above already established for the discrete switch: never step down into
/// the reserved scratchpad workspace 0, never overflow stepping up. Stops
/// at the first refused step rather than silently dropping the remainder,
/// so a fast fling against either edge lands exactly on it. Returns the
/// landing workspace and how many of the requested steps actually applied
/// (same sign as `advances`, shorter in magnitude when a boundary was hit)
/// so the caller can fold any refused remainder back into the camera.
fn swim_advance_target(current: u32, advances: i32) -> (u32, i32) {
    let mut workspace = current;
    let mut applied = 0;
    for _ in 0..advances.unsigned_abs() {
        let next = if advances > 0 {
            workspace.checked_add(1)
        } else {
            (workspace > 1).then(|| workspace - 1)
        };
        match next {
            Some(next) => {
                workspace = next;
                applied += advances.signum();
            }
            None => break,
        }
    }
    (workspace, applied)
}

fn completed_swipe_direction(delta_x: f64, delta_y: f64, threshold: f64) -> Option<Direction> {
    if delta_x.abs().max(delta_y.abs()) < threshold {
        return None;
    }
    if delta_x.abs() > delta_y.abs() {
        Some(if delta_x < 0.0 {
            Direction::Left
        } else {
            Direction::Right
        })
    } else {
        Some(if delta_y < 0.0 {
            Direction::Up
        } else {
            Direction::Down
        })
    }
}

impl Smallvil {
    fn run_workspace_swipe(&mut self, delta_x: f64, delta_y: f64, threshold: f64) {
        if let Some(output) = self.primary_output() {
            let current = self.layout.active_workspace(&output.name());
            if let Some(target) = workspace_swipe_target(current, delta_x, delta_y, threshold) {
                self.switch_workspace(&output, target);
            }
        }
    }

    /// Drives the swim camera for one gesture-update tick: converts the raw
    /// pixel delta into spot-widths (`workspace_swipe_distance` is one full
    /// spot, `swim.response` the gain on top), drags the camera, and applies
    /// any whole-spot advances that fall out to the real discrete workspace
    /// axis via `switch_workspace_immediate` -- the wave-transition capture
    /// `switch_workspace` would otherwise queue is redundant here, the
    /// camera already owns the outgoing/incoming visual. A step the axis
    /// refuses (the scratchpad boundary) is folded back into the camera
    /// instead of vanishing, so pressing against the edge reads as
    /// resistance rather than the pan silently doing nothing.
    fn run_swim_update(&mut self, delta_x: f64) {
        let Some(output) = self.primary_output() else {
            return;
        };
        let output_name = output.name();
        let distance = self
            .config
            .input
            .touchpad
            .workspace_swipe_distance
            .unwrap_or(200.0)
            .max(1.0);
        let delta_spots = (-delta_x / distance) as f32 * self.config.swim.response;
        let camera = self.swim_cameras.entry(output_name.clone()).or_default();
        let advances = camera.drag(delta_spots);
        if advances != 0 {
            let current = self.layout.active_workspace(&output_name);
            let (target, applied) = swim_advance_target(current, advances);
            if applied != advances {
                if let Some(camera) = self.swim_cameras.get_mut(&output_name) {
                    camera.cancel_advance(advances - applied);
                }
            }
            if target != current {
                self.switch_workspace_immediate(&output, target);
            }
        }
        self.request_redraw();
    }

    /// Ends a swim-driven gesture. `cancelled` (backend-cancelled, or a
    /// session lock racing in mid-drag) snaps the camera to rest
    /// immediately rather than running a multi-hundred-ms spring while
    /// locked; otherwise the residual offset springs back to rest over
    /// `swim.snap_duration_ms` -- a no-op if the drag already ended on
    /// (near) a spot boundary. The workspace crossing itself already
    /// happened live in `run_swim_update`; this only settles the visual.
    fn run_swim_release(&mut self, cancelled: bool) {
        let Some(output) = self.primary_output() else {
            return;
        };
        if let Some(camera) = self.swim_cameras.get_mut(&output.name()) {
            if cancelled {
                camera.snap_to_rest();
            } else {
                let duration = Duration::from_millis(u64::from(self.config.swim.snap_duration_ms));
                camera.release(duration);
            }
        }
        self.request_redraw();
    }

    /// Physically-pressed Ctrl/Alt/Shift/Super, ignoring XKB's *effective*
    /// latched/locked state (which a nested host can also leave stale
    /// across keyboard-focus changes) -- see the modifier-drag button
    /// handling below for why a compositor drag/move must never trigger
    /// off anything but keys actually held down right now.
    fn held_modifiers(&self) -> crate::config::Mods {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return crate::config::Mods::default();
        };
        keyboard.with_pressed_keysyms(|keys| {
            let mut held = crate::config::Mods::default();
            for sym in keys.into_iter().flat_map(|key| key.raw_syms().into_iter()) {
                match sym {
                    Keysym::Control_L | Keysym::Control_R => held.ctrl = true,
                    Keysym::Alt_L | Keysym::Alt_R => held.alt = true,
                    Keysym::Shift_L | Keysym::Shift_R => held.shift = true,
                    Keysym::Super_L | Keysym::Super_R => held.logo = true,
                    _ => {}
                }
            }
            held
        })
    }

    /// `pointer_modifier` + a configured-finger-count touchpad swipe: the
    /// gesture counterpart of `pointer_modifier`+left-drag, needing no
    /// button press at all -- the two-finger touch itself is the "grab".
    /// Starts the exact same grab types the mouse path uses (so drag feel,
    /// tile-swap, and floating reattachment logic live in one place only),
    /// then `InputEvent::GestureSwipeUpdate`/`GestureSwipeEnd` drive it with
    /// synthetic `MotionEvent`s and a direct `unset_grab` instead of a real
    /// button release. Returns `false` when there's nothing to grab (a
    /// fullscreen/maximized window, or empty canvas outside Ocean), leaving
    /// the gesture free to forward to the focused client as usual.
    fn start_gesture_modifier_move(&mut self) -> bool {
        let pointer = self.seat.get_pointer().unwrap();
        let location = pointer.current_location();
        let serial = SERIAL_COUNTER.next_serial();

        let Some((window, loc)) = self.window_under(location) else {
            if self.config.spatial_engine != crate::config::SpatialEngine::Ocean {
                return false;
            }
            let Some(output) = self.output_for_point(location) else {
                return false;
            };
            let start_data = PointerGrabStartData {
                focus: None,
                button: BTN_LEFT,
                location,
            };
            pointer.set_grab(
                self,
                OceanPanGrab::start(start_data, output.name()),
                serial,
                Focus::Clear,
            );
            return true;
        };

        let wl_surface = window.toplevel().unwrap().wl_surface().clone();
        if self.fullscreen.contains_key(&wl_surface) || self.maximized.contains_key(&wl_surface) {
            // Output-owned placements; same exclusion the mouse path uses.
            return false;
        }

        self.focus_window(Some(wl_surface.clone()), serial);
        let start_data = PointerGrabStartData {
            focus: Some((wl_surface.clone(), loc.to_f64())),
            button: BTN_LEFT,
            location,
        };

        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean
            && self.config.ocean.smart_tiling
            && self.ocean.is_tiled(&wl_surface)
        {
            let Some(output) = self
                .ocean
                .entry_output(&wl_surface)
                .map(str::to_string)
                .or_else(|| self.output_for_window(&window).map(|output| output.name()))
            else {
                return false;
            };
            let Some(initial_rect) =
                self.ocean
                    .world_rect(&wl_surface, self.config.gaps, self.config.bsp_split_bias)
            else {
                return false;
            };
            let view_scale = self.ocean.camera(&output).zoom;
            let grab = OceanTileMoveGrab::start(
                start_data,
                window,
                wl_surface,
                output,
                initial_rect.loc,
                view_scale,
            );
            pointer.set_grab(self, grab, serial, Focus::Clear);
            return true;
        }

        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean
            && self.config.ocean.freeform_windows
            && self.ocean.is_tiled(&wl_surface)
        {
            self.toggle_floating(&wl_surface);
            let Some(model_rect) = self.ocean.floating_rect(&wl_surface) else {
                return false;
            };
            self.ocean.raise_floating(&wl_surface);
            self.space.raise_element(&window, false);
            let view_scale = self
                .output_for_point(location)
                .map(|output| self.ocean.camera(&output.name()).zoom)
                .unwrap_or(1.0);
            let last_location = start_data.location;
            pointer.set_grab(
                self,
                MoveSurfaceGrab {
                    start_data,
                    window,
                    initial_window_location: model_rect.loc,
                    view_scale,
                    smart_attach_ocean: self.config.ocean.smart_tiling,
                    last_location,
                },
                serial,
                Focus::Clear,
            );
            return true;
        }

        if !self.layout.contains(&wl_surface) && !self.ocean.is_tiled(&wl_surface) {
            self.ocean.raise_floating(&wl_surface);
            self.space.raise_element(&window, false);
            let model_loc = self
                .ocean
                .floating_rect(&wl_surface)
                .map(|rect| rect.loc)
                .unwrap_or(loc);
            let view_scale = self
                .output_for_point(location)
                .map(|output| self.ocean.camera(&output.name()).zoom)
                .unwrap_or(1.0);
            let last_location = start_data.location;
            pointer.set_grab(
                self,
                MoveSurfaceGrab {
                    start_data,
                    window,
                    initial_window_location: model_loc,
                    view_scale,
                    smart_attach_ocean: self.config.spatial_engine
                        == crate::config::SpatialEngine::Ocean
                        && self.config.ocean.smart_tiling,
                    last_location,
                },
                serial,
                Focus::Clear,
            );
            return true;
        }

        let output = self.layout.output_of(&wl_surface).map(str::to_string);
        let workspace = self.layout.workspace_of(&wl_surface);
        let (Some(output), Some(workspace)) = (output, workspace) else {
            return false;
        };
        let grab = TileMoveGrab::start(start_data, window, wl_surface, output, workspace, loc);
        pointer.set_grab(self, grab, serial, Focus::Clear);
        true
    }

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

    /// Synthesizes a key-release for every key the seat currently
    /// believes is pressed. Called when the *host* compositor takes
    /// keyboard focus away from a nested session's window: any release
    /// that happens while the host holds the keyboard is never delivered
    /// here (KDE's own `Super+L` lock mid-chord was the reproduced case),
    /// leaving xkb's modifier state -- and therefore the
    /// `wl_keyboard.modifiers` every client is told -- stuck with a
    /// phantom held modifier (kitty then decodes plain typing as CSI-u
    /// modifier sequences). Completes what `ad45af8` started: that fix
    /// stopped compositor-side drag decisions from trusting the stale
    /// state; this resets the state itself, clients included. Mirrors
    /// `wl_keyboard.leave` semantics, where a leave implies all keys
    /// released. Backend-agnostic on purpose -- a udev VT switch that
    /// eats a release mid-chord is the same shape of problem.
    pub(crate) fn release_stuck_keys(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let pressed: Vec<_> = keyboard.pressed_keys().into_iter().collect();
        if pressed.is_empty() {
            return;
        }
        tracing::debug!(
            count = pressed.len(),
            "Releasing keys stuck across host focus loss"
        );
        let time = self.start_time.elapsed().as_millis() as u32;
        for keycode in pressed {
            keyboard.input::<(), _>(
                self,
                keycode,
                KeyState::Released,
                SERIAL_COUNTER.next_serial(),
                time,
                |_, _, _| FilterResult::Forward,
            );
        }

        // This synthesizes releases through a no-op filter closure, not
        // the real one above -- so the minimap peek's own release-tracking
        // (which lives entirely inside that closure) never runs for these.
        // This is exactly the "stuck open" failure its state-checked
        // design exists to prevent: a lost real release is the whole
        // reason this function exists, so clear it explicitly here too.
        if self.minimap_trigger_down {
            self.minimap_trigger_down = false;
            self.close_minimap_peek();
        }
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
                if key_state == KeyState::Pressed {
                    if let Some(surface) = self.focused_window_surface() {
                        self.note_depth_attention(&surface);
                    }
                }

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
                            let released = handle.raw_syms().first().cloned();
                            if released.is_some_and(|keysym| data.helper_keys_down.remove(&keysym))
                            {
                                return FilterResult::Intercept(());
                            }

                            // Minimap peek dismissal is state-checked, not
                            // tied to catching one specific release event:
                            // a release can go missing (host focus loss,
                            // VT switch -- the exact reasons
                            // `release_stuck_keys` exists), so this
                            // recomputes "is the chord still fully held"
                            // from `modifiers`, which XKB has already
                            // updated for this event by the time this
                            // closure runs. Covers both ways the chord can
                            // break: the trigger key itself releasing, or a
                            // required modifier releasing while the
                            // trigger key stays down.
                            let trigger_released = data.minimap_trigger_down
                                && released == Some(data.config.minimap.keysym);
                            if data.minimap_trigger_down {
                                let held = Mods {
                                    ctrl: modifiers.ctrl,
                                    alt: modifiers.alt,
                                    shift: modifiers.shift,
                                    logo: modifiers.logo,
                                };
                                if trigger_released || !data.config.minimap.mods.is_held_by(held) {
                                    data.minimap_trigger_down = false;
                                    data.close_minimap_peek();
                                }
                            }
                            if trigger_released {
                                // Symmetric with the press below: a client
                                // never sees this key release without
                                // having seen the matching press either.
                                return FilterResult::Intercept(());
                            }
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

                        let keysym = match handle.raw_syms().first().cloned() {
                            Some(keysym) => keysym,
                            None => return FilterResult::Forward,
                        };

                        // Deliberately outside Waves: this is the one escape
                        // hatch for a valid but unusable config (for example,
                        // every normal bind was removed). It activates a
                        // temporary known-safe table without rewriting the
                        // file; the next successful reload/restart restores
                        // the user's table. It is checked before shortcut
                        // inhibition because a captured client must not be
                        // able to prevent recovery from compositor config.
                        if modifiers.ctrl
                            && modifiers.alt
                            && !modifiers.shift
                            && !modifiers.logo
                            && keysym == Keysym::Escape
                        {
                            data.rescue_keybinds_active = true;
                            data.active_submap = None;
                            data.helper_keys_down.clear();
                            data.toast = Some(Toast::new(
                                "Rescue keybinds active until config reload",
                                ToastKind::Info,
                                crate::ui_theme::UiTheme::from_config(&data.config),
                            ));
                            data.request_redraw();
                            return FilterResult::Intercept(());
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

                        tracing::trace!(?keysym, ?modifiers, "Key pressed");

                        // The portal picker is compositor-owned modal UI.
                        // While visible, no key press leaks into the client
                        // underneath it; arrows/Tab move and Enter/Escape
                        // accept/cancel.
                        #[cfg(feature = "screencast")]
                        if data.screencast_picker.is_some()
                            && data.handle_screencast_picker_key(keysym)
                        {
                            return FilterResult::Intercept(());
                        }

                        // Hold-to-peek the whole-world minimap (spatial
                        // roadmap S5's other half, alongside the compass).
                        // Unlike every entry in `table` below, a chord
                        // match here doesn't fire a one-shot action: it
                        // opens the peek and starts tracking the trigger
                        // keysym's own held state, since staying held is
                        // what keeps it open. Independent of `table`
                        // entirely (checked regardless of an active
                        // submap/rescue-mode) so it stays reachable the
                        // same way the screencast picker's own keys do.
                        //
                        // Only intercepted if a peek actually opened.
                        // `open_minimap_peek` itself no-ops under Classic,
                        // with `minimap.enabled = false`, or with no
                        // resolvable output -- and this chord must fall
                        // through to `table` below in all of those cases,
                        // or it would silently shadow an ordinary user
                        // keybind on the same combo for zero benefit
                        // (exactly what a Classic-engine config, the
                        // common case, would otherwise get).
                        if data.config.minimap.keysym == keysym {
                            let held = Mods {
                                ctrl: modifiers.ctrl,
                                alt: modifiers.alt,
                                shift: modifiers.shift,
                                logo: modifiers.logo,
                            };
                            if data.config.minimap.mods.is_held_by(held) {
                                if let Some(output) = data.primary_output() {
                                    data.open_minimap_peek(&output);
                                }
                                if data.minimap_peek.is_some() {
                                    data.minimap_trigger_down = true;
                                    return FilterResult::Intercept(());
                                }
                            }
                        }

                        // A submap fully replaces the base table rather
                        // than layering on top of it (matches sway/
                        // Hyprland: while "in a mode," only that mode's
                        // own binds fire as compositor actions -- an
                        // empty slice for an active_submap name that
                        // somehow isn't in `submaps` shouldn't happen
                        // (reload_config clears it if the name vanishes),
                        // but degrades safely to "nothing matches,
                        // everything forwards" rather than panicking.
                        let table: &[Keybind] = if data.rescue_keybinds_active {
                            &data.config.rescue_keybinds
                        } else {
                            match &data.active_submap {
                                Some(name) => data
                                    .config
                                    .submaps
                                    .get(name)
                                    .map(Vec::as_slice)
                                    .unwrap_or(&[]),
                                None => &data.config.keybinds,
                            }
                        };
                        let action = matching_keybind(
                            table,
                            keysym,
                            modifiers,
                            &data.helper_keys_down,
                            data.config.spatial_engine == crate::config::SpatialEngine::Ocean,
                        )
                        .map(|bind| bind.action.clone());

                        match action {
                            Some(action) => {
                                tracing::trace!("Matched keybind action");
                                data.run_action(action);
                                FilterResult::Intercept(())
                            }
                            None if table.iter().any(|bind| bind.uses_helper_key(keysym)) => {
                                data.helper_keys_down.insert(keysym);
                                FilterResult::Intercept(())
                            }
                            None => FilterResult::Forward,
                        }
                    },
                );
            }
            InputEvent::PointerMotion { event, .. } => {
                self.note_pointer_motion();

                // The minimap peek is compositor-owned modal chrome, the
                // same "no input leaks to a client underneath it" rule the
                // screencast picker already applies to keys -- so this
                // skips relative-motion delivery, surface-under hit
                // testing, and pointer-constraint handling entirely rather
                // than just not acting on their results. Still moves the
                // real cursor sprite (`pointer.motion` with no focus
                // target) so there's visual feedback while aiming a click.
                if self.minimap_peek.is_some() {
                    let pointer = self.seat.get_pointer().unwrap();
                    let new_loc = self.clamp_to_outputs(pointer.current_location() + event.delta());
                    self.update_minimap_pointer(new_loc);
                    pointer.motion(
                        self,
                        None,
                        &MotionEvent {
                            location: new_loc,
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(self);
                    if self.udev_renderer.is_some() {
                        self.request_redraw();
                    }
                    return;
                }

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

                if self.minimap_peek.is_some() {
                    let pointer = self.seat.get_pointer().unwrap();
                    self.update_minimap_pointer(pos);
                    pointer.motion(
                        self,
                        None,
                        &MotionEvent {
                            location: pos,
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(self);
                    if self.udev_renderer.is_some() {
                        self.request_redraw();
                    }
                    return;
                }

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

                // The minimap peek is compositor-owned modal chrome with no
                // client "under" it in any meaningful sense -- consume the
                // button fully rather than routing it through
                // `pointer.button()`, which is the call that runs grab
                // dispatch. This also means `minimap_click_travel`'s own
                // use of the peek's tracked `last_location` (never
                // `current_location()` from inside a grab callback) is the
                // only pointer-position read this path needs.
                if self.minimap_peek.is_some() {
                    if button_state == ButtonState::Pressed {
                        self.minimap_click_travel();
                    }
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
                        if let Some((window, _)) = self.window_under(pos) {
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

                    let under = self.window_under(pointer.current_location());

                    // Configured-modifier+drag moves/resizes a floating
                    // window, the same convention Hyprland and most tiling
                    // WMs use so you don't need decorations to reposition
                    // anything. The shipped config points this at `$mod`.
                    //
                    // Modifier+Left-drag on a *tiled* window instead picks it
                    // up for drag-to-swap (TileMoveGrab,
                    // grabs/tile_move_grab.rs)
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
                    // `modifier_state().logo` is the XKB *effective* state:
                    // it also includes latched/locked modifiers. In a nested
                    // compositor the host can additionally leave that state
                    // stale across keyboard-focus changes. Neither case means
                    // the user is physically holding the main modifier now.
                    // Require actually pressed modifier keys so an ordinary
                    // drag can never turn into a compositor move/resize.
                    let modifier_drag = self
                        .config
                        .pointer_modifier
                        .is_held_by(self.held_modifiers())
                        && (button == BTN_LEFT || button == BTN_RIGHT);
                    if modifier_drag {
                        if let Some((window, loc)) = under.clone() {
                            let wl_surface = window.toplevel().unwrap().wl_surface().clone();
                            if self.fullscreen.contains_key(&wl_surface)
                                || self.maximized.contains_key(&wl_surface)
                            {
                                // Fullscreen/maximized are output-owned placements;
                                // a compositor drag must not move/resize it
                                // behind the protocol state's back.
                            } else if self.config.spatial_engine
                                == crate::config::SpatialEngine::Ocean
                                && self.config.ocean.smart_tiling
                                && button == BTN_LEFT
                                && self.ocean.is_tiled(&wl_surface)
                            {
                                let Some(output) = self
                                    .ocean
                                    .entry_output(&wl_surface)
                                    .map(str::to_string)
                                    .or_else(|| {
                                        self.output_for_window(&window).map(|output| output.name())
                                    })
                                else {
                                    return;
                                };
                                let Some(initial_rect) = self.ocean.world_rect(
                                    &wl_surface,
                                    self.config.gaps,
                                    self.config.bsp_split_bias,
                                ) else {
                                    return;
                                };
                                let view_scale = self.ocean.camera(&output).zoom;
                                self.focus_window(Some(wl_surface.clone()), serial);
                                let start_data = PointerGrabStartData {
                                    focus: Some((wl_surface.clone(), loc.to_f64())),
                                    button,
                                    location: pointer.current_location(),
                                };
                                let grab = OceanTileMoveGrab::start(
                                    start_data,
                                    window,
                                    wl_surface,
                                    output,
                                    initial_rect.loc,
                                    view_scale,
                                );
                                pointer.set_grab(self, grab, serial, Focus::Clear);
                                return;
                            } else if self.config.spatial_engine
                                == crate::config::SpatialEngine::Ocean
                                && self.config.ocean.freeform_windows
                                && self.ocean.is_tiled(&wl_surface)
                            {
                                // In Ocean a reef is a useful local tiling
                                // tool, not a cage. Beginning either ordinary
                                // compositor drag detaches this tile at its
                                // exact world rectangle and continues as a
                                // zoom-aware free move/resize.
                                self.toggle_floating(&wl_surface);
                                let Some(model_rect) = self.ocean.floating_rect(&wl_surface) else {
                                    return;
                                };
                                self.ocean.raise_floating(&wl_surface);
                                self.space.raise_element(&window, false);
                                self.focus_window(Some(wl_surface.clone()), serial);
                                let start_data = PointerGrabStartData {
                                    focus: Some((wl_surface.clone(), loc.to_f64())),
                                    button,
                                    location: pointer.current_location(),
                                };
                                let view_scale = self
                                    .output_for_point(pointer.current_location())
                                    .map(|output| self.ocean.camera(&output.name()).zoom)
                                    .unwrap_or(1.0);
                                let last_location = start_data.location;
                                if button == BTN_LEFT {
                                    pointer.set_grab(
                                        self,
                                        MoveSurfaceGrab {
                                            start_data,
                                            window,
                                            initial_window_location: model_rect.loc,
                                            view_scale,
                                            smart_attach_ocean: self.config.ocean.smart_tiling,
                                            last_location,
                                        },
                                        serial,
                                        Focus::Clear,
                                    );
                                } else {
                                    pointer.set_grab(
                                        self,
                                        ResizeSurfaceGrab::start(
                                            start_data,
                                            window,
                                            ResizeEdge::BOTTOM_RIGHT,
                                            model_rect,
                                            view_scale,
                                        ),
                                        serial,
                                        Focus::Clear,
                                    );
                                }
                                return;
                            } else if !self.layout.contains(&wl_surface)
                                && !self.ocean.is_tiled(&wl_surface)
                            {
                                self.ocean.raise_floating(&wl_surface);
                                self.space.raise_element(&window, false);
                                self.focus_window(Some(wl_surface.clone()), serial);

                                let start_data = PointerGrabStartData {
                                    focus: Some((wl_surface.clone(), loc.to_f64())),
                                    button,
                                    location: pointer.current_location(),
                                };
                                let model_loc = self
                                    .ocean
                                    .floating_rect(&wl_surface)
                                    .map(|rect| rect.loc)
                                    .unwrap_or(loc);
                                let view_scale = self
                                    .output_for_point(pointer.current_location())
                                    .map(|output| self.ocean.camera(&output.name()).zoom)
                                    .unwrap_or(1.0);

                                if button == BTN_LEFT {
                                    let last_location = start_data.location;
                                    let grab = MoveSurfaceGrab {
                                        start_data,
                                        window,
                                        initial_window_location: model_loc,
                                        view_scale,
                                        smart_attach_ocean: self.config.spatial_engine
                                            == crate::config::SpatialEngine::Ocean
                                            && self.config.ocean.smart_tiling,
                                        last_location,
                                    };
                                    pointer.set_grab(self, grab, serial, Focus::Clear);
                                } else {
                                    let initial_rect =
                                        Rectangle::new(model_loc, window.geometry().size);
                                    let grab = ResizeSurfaceGrab::start(
                                        start_data,
                                        window,
                                        ResizeEdge::BOTTOM_RIGHT,
                                        initial_rect,
                                        view_scale,
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
                                // counterpart to the floating modifier+Right-drag
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
                                        .flat_map(|hit| self.connected_resize_handles(&hit))
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

                                    // BSP found no adjacent split (not a
                                    // BSP-tiled window) -- try cascade's own
                                    // grid boundaries next.
                                    let cascade_hits = self.layout.cascade_resize_splits(
                                        &output.name(),
                                        workspace,
                                        area,
                                        &wl_surface,
                                    );
                                    if !cascade_hits.is_empty() {
                                        self.focus_window(Some(wl_surface.clone()), serial);

                                        let start_data = PointerGrabStartData {
                                            focus: Some((wl_surface, loc.to_f64())),
                                            button,
                                            location: pointer.current_location(),
                                        };
                                        let grab =
                                            CascadeResizeGrab::start(start_data, cascade_hits);
                                        pointer.set_grab(self, grab, serial, Focus::Clear);
                                        return;
                                    }
                                }
                            }
                        }
                    }

                    let canvas_pan = self.config.spatial_engine
                        == crate::config::SpatialEngine::Ocean
                        && under.is_none()
                        && self.config.ocean.canvas_pan_button.matches(button)
                        && (!self.config.ocean.canvas_pan_requires_modifier
                            || self
                                .config
                                .pointer_modifier
                                .is_held_by(self.held_modifiers()));
                    if canvas_pan {
                        if let Some(output) = self.output_for_point(pointer.current_location()) {
                            let start_data = PointerGrabStartData {
                                focus: None,
                                button,
                                location: pointer.current_location(),
                            };
                            pointer.set_grab(
                                self,
                                OceanPanGrab::start(start_data, output.name()),
                                serial,
                                Focus::Clear,
                            );
                            return;
                        }
                    }

                    // A plain (no-modifier) left click landing on a
                    // floating window's own edge resizes it directly, the
                    // same convention niri and Hyprland both use -- the
                    // floating counterpart to the tiled hit_test_split drag
                    // just below. Skipped entirely once `modifier_drag` above
                    // already claimed the click.
                    if !modifier_drag && button == BTN_LEFT {
                        if let Some((window, loc)) = under.clone() {
                            let wl_surface = window.toplevel().unwrap().wl_surface().clone();
                            if !self.layout.contains(&wl_surface)
                                && !self.ocean.is_tiled(&wl_surface)
                                && !self.fullscreen.contains_key(&wl_surface)
                                && !self.maximized.contains_key(&wl_surface)
                            {
                                let view_scale = self
                                    .output_for_point(pointer.current_location())
                                    .map(|output| self.ocean.camera(&output.name()).zoom)
                                    .unwrap_or(1.0);
                                let rect = Rectangle::new(
                                    loc,
                                    Size::from((
                                        (window.geometry().size.w as f64 * view_scale)
                                            .round()
                                            .max(1.0)
                                            as i32,
                                        (window.geometry().size.h as f64 * view_scale)
                                            .round()
                                            .max(1.0)
                                            as i32,
                                    )),
                                );
                                let threshold = (self.config.gaps as f64).max(4.0);
                                if let Some(edge) = floating_resize_edge(
                                    rect,
                                    pointer.current_location(),
                                    threshold,
                                ) {
                                    self.ocean.raise_floating(&wl_surface);
                                    self.space.raise_element(&window, false);
                                    self.focus_window(Some(wl_surface.clone()), serial);

                                    let start_data = PointerGrabStartData {
                                        focus: Some((wl_surface.clone(), loc.to_f64())),
                                        button,
                                        location: pointer.current_location(),
                                    };
                                    let model_rect =
                                        self.ocean.floating_rect(&wl_surface).unwrap_or(rect);
                                    let grab = ResizeSurfaceGrab::start(
                                        start_data, window, edge, model_rect, view_scale,
                                    );
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
                        let output_area = self
                            .output_for_point(pointer.current_location())
                            .and_then(|output| {
                                let output_geo = self.space.output_geometry(&output)?;
                                let mut area = layer_map_for_output(&output).non_exclusive_zone();
                                area.loc += output_geo.loc;
                                Some((output, area))
                            });
                        if let Some((output, area)) = output_area {
                            let workspace = self.layout.active_workspace(&output.name());
                            let gap = self.gaps_for(&output.name(), workspace);
                            let hit = self.layout.hit_test_split(
                                &output.name(),
                                workspace,
                                area,
                                gap,
                                pointer.current_location(),
                            );
                            if let Some(hit) = hit {
                                let handles = self.connected_resize_handles(&hit);
                                if !handles.is_empty() {
                                    let start_data = PointerGrabStartData {
                                        focus: None,
                                        button,
                                        location: pointer.current_location(),
                                    };
                                    let grab = TileResizeGrab::start(start_data, handles);
                                    pointer.set_grab(self, grab, serial, Focus::Clear);
                                    return;
                                }
                            }

                            // BSP found no split boundary here -- try
                            // cascade's own grid boundaries next.
                            let cascade_hit = self.layout.cascade_hit_test(
                                &output.name(),
                                workspace,
                                area,
                                gap,
                                pointer.current_location(),
                            );
                            if let Some(hit) = cascade_hit {
                                let start_data = PointerGrabStartData {
                                    focus: None,
                                    button,
                                    location: pointer.current_location(),
                                };
                                let grab = CascadeResizeGrab::start(start_data, vec![hit]);
                                pointer.set_grab(self, grab, serial, Focus::Clear);
                                return;
                            }
                        }
                    }

                    if let Some((window, _loc)) = under {
                        let wl_surface = window.toplevel().unwrap().wl_surface().clone();

                        if !self.layout.contains(&wl_surface) {
                            self.ocean.raise_floating(&wl_surface);
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

                // Touch is an activation gesture just like a primary-button
                // press. Keep the actual wl_touch target above unchanged,
                // but route WM focus through the same centralized window/
                // layer authority pointer clicks use. Locked sessions only
                // forward to the lock surface and never alter WM focus.
                if matches!(self.session_lock, SessionLock::Unlocked) {
                    if let Some(layer) = self.layer_under_pointer(location) {
                        if layer.cached_state().keyboard_interactivity
                            != KeyboardInteractivity::None
                        {
                            self.focus_layer(layer.wl_surface().clone(), serial);
                        }
                    } else if self.exclusive_layer().is_none() {
                        if let Some((window, _)) = self.window_under(location) {
                            let surface = window.toplevel().unwrap().wl_surface().clone();
                            if !self.layout.contains(&surface) {
                                self.space.raise_element(&window, false);
                            }
                            self.focus_window(Some(surface), serial);
                        } else if under.is_none() {
                            self.focus_window(None, serial);
                        }
                    }
                }
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

                if self.config.spatial_engine == crate::config::SpatialEngine::Ocean
                    && self.config.ocean.zoom_enabled
                    && self.config.ocean.modifier_zoom
                    && vertical_amount != 0.0
                    && self
                        .config
                        .pointer_modifier
                        .is_held_by(self.held_modifiers())
                {
                    let pointer = self.seat.get_pointer().unwrap();
                    let location = pointer.current_location();
                    if let Some(output) = self.output_for_point(location) {
                        if let Some(output_geo) = self.space.output_geometry(&output) {
                            let steps = vertical_amount_discrete
                                .map(|value| value / 120.0)
                                .unwrap_or(vertical_amount / 15.0);
                            let current = self.ocean.camera(&output.name()).zoom;
                            let target = (current / self.config.ocean.zoom_step.powf(steps))
                                .clamp(self.config.ocean.min_zoom, self.config.ocean.max_zoom);
                            self.ocean.zoom_at(
                                &output.name(),
                                location - output_geo.loc.to_f64(),
                                target,
                                Duration::from_millis(
                                    self.config.ocean.camera_animation_ms.min(120),
                                ),
                            );
                            self.request_redraw();
                            return;
                        }
                    }
                }

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
                let fingers = GestureBeginEvent::fingers(&event);
                let touchpad = &self.config.input.touchpad;
                let has_action = touchpad.swipe_left.is_some()
                    || touchpad.swipe_right.is_some()
                    || touchpad.swipe_up.is_some()
                    || touchpad.swipe_down.is_some();
                let action_match = has_action
                    && touchpad
                        .gesture_swipe_fingers
                        .is_some_and(|configured| configured == fingers);
                let workspace_fallback = touchpad
                    .workspace_swipe_fingers
                    .is_some_and(|configured| configured == fingers);
                let compositor_swipe = (action_match || workspace_fallback)
                    && matches!(self.session_lock, SessionLock::Unlocked)
                    && self.exclusive_layer().is_none();
                if compositor_swipe {
                    // Decided once, at Begin: a config change mid-drag must
                    // not switch which model an already-running gesture
                    // follows out from under it. `!action_match` mirrors
                    // End's own bound-action-wins-over-workspace-fallback
                    // precedence -- direction isn't known yet at Begin, so
                    // this can't do better than gating on finger count, but
                    // it means swim never silently swallows a configured
                    // swipe_up/down/left/right bound at the same finger
                    // count `workspace_swipe_fingers` uses.
                    let swim = workspace_fallback && !action_match && self.swim_enabled();
                    self.compositor_gesture = Some(CompositorGesture::Swipe {
                        workspace_fallback,
                        swim,
                        delta_x: 0.0,
                        delta_y: 0.0,
                    });
                    return;
                }
                // `pointer_modifier` + a configured finger count: the
                // touchpad counterpart of `pointer_modifier`+left-drag,
                // with the two-finger touch itself standing in for the
                // button press. Checked after the plain-swipe path above so
                // an overlapping finger-count config still favors bound
                // swipe actions/workspace navigation, matching how those
                // already take priority over everything below them.
                // `!is_grabbed()` mirrors the same guard every mouse-driven
                // `set_grab` call in the button handler above uses: without
                // it, an unrelated concurrent mouse drag could be
                // superseded by this gesture starting mid-flight, and
                // `set_grab` unconditionally calls the superseded grab's
                // own `unset()` -- which now commits a tile swap/reattach
                // (see `OceanTileMoveGrab`/`TileMoveGrab`'s `commit`) based
                // on whatever position it last saw, not a real release.
                let modifier_pan = matches!(self.session_lock, SessionLock::Unlocked)
                    && self.exclusive_layer().is_none()
                    && !self.seat.get_pointer().unwrap().is_grabbed()
                    && self
                        .config
                        .input
                        .touchpad
                        .modifier_pan_fingers
                        .is_some_and(|configured| configured == fingers)
                    && self
                        .config
                        .pointer_modifier
                        .is_held_by(self.held_modifiers());
                if modifier_pan && self.start_gesture_modifier_move() {
                    self.compositor_gesture = Some(CompositorGesture::ModifierMove {
                        last_location: self.seat.get_pointer().unwrap().current_location(),
                    });
                    return;
                }
                let pointer = self.seat.get_pointer().unwrap();
                pointer.gesture_swipe_begin(
                    self,
                    &GestureSwipeBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: Event::time_msec(&event),
                        fingers,
                    },
                );
            }
            InputEvent::GestureSwipeUpdate { event, .. } => {
                let delta = BackendGestureSwipeUpdateEvent::delta(&event);
                let modifier_move_from = match &self.compositor_gesture {
                    Some(CompositorGesture::ModifierMove { last_location }) => Some(*last_location),
                    _ => None,
                };
                if let Some(last_location) = modifier_move_from {
                    let new_location = self.clamp_to_outputs(last_location + delta);
                    self.compositor_gesture = Some(CompositorGesture::ModifierMove {
                        last_location: new_location,
                    });
                    let pointer = self.seat.get_pointer().unwrap();
                    pointer.motion(
                        self,
                        None,
                        &MotionEvent {
                            location: new_location,
                            serial: SERIAL_COUNTER.next_serial(),
                            time: Event::time_msec(&event),
                        },
                    );
                    pointer.frame(self);
                    return;
                }
                let swim = if let Some(CompositorGesture::Swipe {
                    swim,
                    delta_x,
                    delta_y,
                    ..
                }) = &mut self.compositor_gesture
                {
                    *delta_x += delta.x;
                    *delta_y += delta.y;
                    Some(*swim)
                } else {
                    None
                };
                match swim {
                    Some(true) => {
                        self.run_swim_update(delta.x);
                        return;
                    }
                    Some(false) => return,
                    None => {}
                }
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
                // Checked (and cleared) before the `.take()` below: that
                // call unconditionally clears `compositor_gesture` as a
                // side effect of evaluating its scrutinee, so a `ModifierMove`
                // left unhandled here would still get silently cleared,
                // leaking the grab it started forever (nothing else would
                // ever call `unset_grab` on it).
                if matches!(
                    &self.compositor_gesture,
                    Some(CompositorGesture::ModifierMove { .. })
                ) {
                    self.compositor_gesture = None;
                    let pointer = self.seat.get_pointer().unwrap();
                    pointer.unset_grab(
                        self,
                        SERIAL_COUNTER.next_serial(),
                        Event::time_msec(&event),
                    );
                    return;
                }
                if let Some(CompositorGesture::Swipe {
                    workspace_fallback,
                    swim,
                    delta_x,
                    delta_y,
                }) = self.compositor_gesture.take()
                {
                    if swim {
                        let locked = !matches!(self.session_lock, SessionLock::Unlocked);
                        let cancelled = GestureEndEvent::cancelled(&event) || locked;
                        self.run_swim_release(cancelled);
                        return;
                    }
                    if !GestureEndEvent::cancelled(&event)
                        && matches!(self.session_lock, SessionLock::Unlocked)
                    {
                        let threshold = self
                            .config
                            .input
                            .touchpad
                            .workspace_swipe_distance
                            .unwrap_or(200.0)
                            .max(1.0);
                        let direction = completed_swipe_direction(delta_x, delta_y, threshold);
                        let action = direction.and_then(|direction| {
                            let touchpad = &self.config.input.touchpad;
                            match direction {
                                Direction::Left => touchpad.swipe_left.clone(),
                                Direction::Right => touchpad.swipe_right.clone(),
                                Direction::Up => touchpad.swipe_up.clone(),
                                Direction::Down => touchpad.swipe_down.clone(),
                            }
                        });
                        if let Some(action) = action {
                            self.run_action(action);
                        } else if workspace_fallback {
                            self.run_workspace_swipe(delta_x, delta_y, threshold);
                        }
                    }
                    return;
                }
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
                let fingers = GestureBeginEvent::fingers(&event);
                let touchpad = &self.config.input.touchpad;
                let compositor_pinch = (touchpad.pinch_in.is_some()
                    || touchpad.pinch_out.is_some())
                    && touchpad
                        .gesture_pinch_fingers
                        .is_some_and(|configured| configured == fingers)
                    && matches!(self.session_lock, SessionLock::Unlocked)
                    && self.exclusive_layer().is_none();
                if compositor_pinch {
                    self.compositor_gesture = Some(CompositorGesture::Pinch { scale: 1.0 });
                    return;
                }
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
                if let Some(CompositorGesture::Pinch { scale }) = &mut self.compositor_gesture {
                    *scale = BackendGesturePinchUpdateEvent::scale(&event);
                    return;
                }
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
                if let Some(CompositorGesture::Pinch { scale }) = self.compositor_gesture.take() {
                    if !GestureEndEvent::cancelled(&event)
                        && matches!(self.session_lock, SessionLock::Unlocked)
                    {
                        let action = if scale <= 0.8 {
                            self.config.input.touchpad.pinch_in.clone()
                        } else if scale >= 1.2 {
                            self.config.input.touchpad.pinch_out.clone()
                        } else {
                            None
                        };
                        if let Some(action) = action {
                            self.run_action(action);
                        }
                    }
                    return;
                }
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
                        crate::ui_theme::UiTheme::from_config(&self.config),
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
            Action::ToggleBorderFullscreen => {
                self.toggle_maximize();
            }
            Action::ResizeToMonitor => {
                self.resize_to_monitor();
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
            Action::ToggleFloatAmbient => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.toggle_float_ambient(&surface);
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
            Action::ToggleScratchpad(name) => {
                if let Some(output) = self.primary_output() {
                    self.toggle_scratchpad(&output, name.as_deref());
                }
            }
            Action::MoveToScratchpad(name) => {
                let focused = self.focused_window_surface();
                if let Some(surface) = focused {
                    self.move_to_scratchpad(&surface, name.as_deref());
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
            Action::Resize(direction) => {
                self.keyboard_resize(direction);
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
                if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
                    let Some(output) = self.primary_output() else {
                        return;
                    };
                    match workspace {
                        // Numbered keys are app-slots ("like focus but
                        // faster"), not bookmark jumps -- see
                        // `jump_to_app_slot`. A named ref (nothing binds
                        // one by default, but nothing stops a custom
                        // config from doing so) keeps the old bookmark
                        // behavior, since app-slots are inherently numbered.
                        crate::config::WorkspaceRef::Number(number) => {
                            self.jump_to_app_slot(&output, number as usize);
                        }
                        crate::config::WorkspaceRef::Name(name) => {
                            if self.ocean.animate_to_bookmark(
                                &output.name(),
                                &name,
                                Duration::from_millis(self.config.ocean.camera_animation_ms),
                                self.config.ocean.camera_sway,
                            ) {
                                self.request_redraw();
                            } else {
                                tracing::warn!(bookmark = %name, "Ocean bookmark not found");
                            }
                        }
                    }
                    return;
                }
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
                if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
                    tracing::warn!(
                        ?workspace,
                        "move-to-workspace has no Ocean meaning; window dredge belongs to S4"
                    );
                    return;
                }
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
            Action::SinkWindow => {
                if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
                    if !self.config.ocean.depth_enabled {
                        return;
                    }
                    let Some(surface) = self.focused_window_surface() else {
                        return;
                    };
                    let Some(output) = self.primary_output() else {
                        return;
                    };
                    let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size)
                    else {
                        return;
                    };
                    if self.ocean.sink_window(
                        &surface,
                        &output.name(),
                        viewport,
                        self.config.gaps,
                        self.config.bsp_split_bias,
                    ) {
                        self.retile();
                        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged { surface });
                    }
                } else {
                    self.sink_window();
                }
            }
            Action::Dive => {
                self.dive();
            }
            Action::DepthNext => {
                self.cycle_depth_deck(1);
            }
            Action::DepthPrevious => {
                self.cycle_depth_deck(-1);
            }
            Action::DepthSelect => {
                self.select_depth_deck();
            }
            Action::DepthCancel => {
                self.close_depth_deck();
            }
            Action::DepthDown => {
                if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
                    if !self.config.ocean.depth_enabled {
                        return;
                    }
                    let Some(output) = self.primary_output() else {
                        return;
                    };
                    let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size)
                    else {
                        return;
                    };
                    if self.ocean.navigate_depth(
                        &output.name(),
                        viewport,
                        true,
                        (
                            Duration::from_millis(self.config.ocean.camera_animation_ms),
                            self.config.ocean.camera_sway,
                        ),
                    ) {
                        self.request_redraw();
                    }
                } else {
                    self.switch_depth(true);
                }
            }
            Action::DepthUp => {
                if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
                    if !self.config.ocean.depth_enabled {
                        return;
                    }
                    let Some(output) = self.primary_output() else {
                        return;
                    };
                    let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size)
                    else {
                        return;
                    };
                    if self.ocean.navigate_depth(
                        &output.name(),
                        viewport,
                        false,
                        (
                            Duration::from_millis(self.config.ocean.camera_animation_ms),
                            -self.config.ocean.camera_sway,
                        ),
                    ) {
                        self.request_redraw();
                    }
                } else {
                    self.switch_depth(false);
                }
            }
            Action::OceanPan(direction) => {
                if self.config.spatial_engine != crate::config::SpatialEngine::Ocean {
                    return;
                }
                let Some(output) = self.primary_output() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(&output) else {
                    return;
                };
                let reef_resized = self.ocean.ensure_default_reef(output_geo.size);
                let camera = self.ocean.camera(&output.name());
                let step = self.config.ocean.camera_step as f64 / camera.zoom.max(0.05);
                let (dx, dy) = match direction {
                    Direction::Left => (-step, 0.0),
                    Direction::Right => (step, 0.0),
                    Direction::Up => (0.0, -step),
                    Direction::Down => (0.0, step),
                };
                self.ocean.animate_pan(
                    &output.name(),
                    dx,
                    dy,
                    Duration::from_millis(self.config.ocean.camera_animation_ms),
                    self.config.ocean.camera_sway,
                );
                if reef_resized {
                    self.retile();
                } else {
                    self.request_redraw();
                }
            }
            Action::OceanZoomIn | Action::OceanZoomOut | Action::OceanZoomReset => {
                if self.config.spatial_engine != crate::config::SpatialEngine::Ocean
                    || !self.config.ocean.zoom_enabled
                {
                    return;
                }
                let Some(output) = self.primary_output() else {
                    return;
                };
                let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size) else {
                    return;
                };
                let current = self.ocean.camera(&output.name()).zoom;
                let target = match action {
                    Action::OceanZoomIn => current * self.config.ocean.zoom_step,
                    Action::OceanZoomOut => current / self.config.ocean.zoom_step,
                    Action::OceanZoomReset => 1.0,
                    _ => unreachable!(),
                }
                .clamp(self.config.ocean.min_zoom, self.config.ocean.max_zoom);
                self.ocean.zoom_at(
                    &output.name(),
                    Point::from((viewport.w as f64 / 2.0, viewport.h as f64 / 2.0)),
                    target,
                    Duration::from_millis(self.config.ocean.camera_animation_ms),
                );
                self.request_redraw();
            }
            Action::OceanCenterFocused => {
                if self.config.spatial_engine != crate::config::SpatialEngine::Ocean {
                    return;
                }
                let Some(surface) = self.focused_window_surface() else {
                    return;
                };
                let Some(output) = self.primary_output() else {
                    return;
                };
                let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size) else {
                    return;
                };
                let Some(rect) =
                    self.ocean
                        .world_rect(&surface, self.config.gaps, self.config.bsp_split_bias)
                else {
                    return;
                };
                self.ocean.center_on_rect(
                    &output.name(),
                    viewport,
                    rect,
                    Duration::from_millis(self.config.ocean.camera_animation_ms),
                    self.config.ocean.camera_sway,
                );
                self.request_redraw();
            }
            Action::OceanDredgeWindow => {
                if self.config.spatial_engine != crate::config::SpatialEngine::Ocean
                    || !self.config.ocean.depth_enabled
                {
                    return;
                }
                let Some(output) = self.primary_output() else {
                    return;
                };
                let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size) else {
                    return;
                };
                if let Some(surface) = self.ocean.dredge_nearest(
                    &output.name(),
                    viewport,
                    self.config.gaps,
                    self.config.bsp_split_bias,
                ) {
                    self.retile();
                    self.focus_window(Some(surface.clone()), SERIAL_COUNTER.next_serial());
                    self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged { surface });
                }
            }
            Action::OceanSurfaceWindow => {
                if self.config.spatial_engine != crate::config::SpatialEngine::Ocean
                    || !self.config.ocean.depth_enabled
                {
                    return;
                }
                let Some(surface) = self.focused_window_surface() else {
                    return;
                };
                let Some(output) = self.primary_output() else {
                    return;
                };
                let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size) else {
                    return;
                };
                if let Some(rect) = self.ocean.surface_window(
                    &surface,
                    self.config.gaps,
                    self.config.bsp_split_bias,
                ) {
                    self.retile();
                    self.ocean.center_on_rect(
                        &output.name(),
                        viewport,
                        rect,
                        Duration::from_millis(self.config.ocean.camera_animation_ms),
                        -self.config.ocean.camera_sway,
                    );
                    self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged { surface });
                    self.request_redraw();
                }
            }
            Action::OceanBookmark(name) => {
                if self.config.spatial_engine != crate::config::SpatialEngine::Ocean {
                    return;
                }
                let Some(output) = self.primary_output() else {
                    return;
                };
                if self.ocean.animate_to_bookmark(
                    &output.name(),
                    &name,
                    Duration::from_millis(self.config.ocean.camera_animation_ms),
                    self.config.ocean.camera_sway,
                ) {
                    self.request_redraw();
                } else {
                    tracing::warn!(bookmark = %name, "Ocean bookmark not found");
                }
            }
            Action::OceanSaveBookmark(name) => {
                if self.config.spatial_engine != crate::config::SpatialEngine::Ocean {
                    return;
                }
                let Some(output) = self.primary_output() else {
                    return;
                };
                if !self.ocean.save_bookmark(&output.name(), name.clone()) {
                    tracing::warn!(bookmark = %name, "Ocean runtime bookmark cap reached");
                }
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
    use smithay::input::keyboard::xkb;
    use smithay::utils::{Point, Size};

    fn rect() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((100, 100)), Size::from((200, 200)))
    }

    #[test]
    fn held_helpers_choose_the_most_specific_matching_chord() {
        let h = xkb::keysym_from_name("h", xkb::KEYSYM_CASE_INSENSITIVE);
        let p = xkb::keysym_from_name("p", xkb::KEYSYM_CASE_INSENSITIVE);
        let r = xkb::keysym_from_name("r", xkb::KEYSYM_CASE_INSENSITIVE);
        let table = [
            Keybind {
                mods: Default::default(),
                held_keysyms: Vec::new(),
                keysym: h,
                action: Action::ToggleFullscreen,
            },
            Keybind {
                mods: Default::default(),
                held_keysyms: vec![p],
                keysym: h,
                action: Action::CloseWindow,
            },
            Keybind {
                mods: Default::default(),
                held_keysyms: vec![p, r],
                keysym: h,
                action: Action::ToggleFloating,
            },
        ];

        let none = HashSet::new();
        assert!(matches!(
            matching_keybind(&table, h, &ModifiersState::default(), &none, true,)
                .map(|bind| &bind.action),
            Some(Action::ToggleFullscreen)
        ));

        let one = HashSet::from([p]);
        assert!(matches!(
            matching_keybind(&table, h, &ModifiersState::default(), &one, true)
                .map(|bind| &bind.action),
            Some(Action::CloseWindow)
        ));

        let two = HashSet::from([p, r]);
        assert!(matches!(
            matching_keybind(&table, h, &ModifiersState::default(), &two, true)
                .map(|bind| &bind.action),
            Some(Action::ToggleFloating)
        ));
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

    #[test]
    fn horizontal_workspace_swipe_moves_one_workspace_without_wrapping_zero() {
        assert_eq!(workspace_swipe_target(3, -220.0, 12.0, 200.0), Some(4));
        assert_eq!(workspace_swipe_target(3, 220.0, 12.0, 200.0), Some(2));
        assert_eq!(workspace_swipe_target(1, 220.0, 12.0, 200.0), None);
    }

    #[test]
    fn short_or_vertical_workspace_swipe_is_ignored() {
        assert_eq!(workspace_swipe_target(3, 199.0, 0.0, 200.0), None);
        assert_eq!(workspace_swipe_target(3, 220.0, 300.0, 200.0), None);
    }

    #[test]
    fn swim_advance_target_steps_by_one_each_direction() {
        assert_eq!(swim_advance_target(3, 1), (4, 1));
        assert_eq!(swim_advance_target(3, -1), (2, -1));
        assert_eq!(swim_advance_target(3, 0), (3, 0));
    }

    #[test]
    fn swim_advance_target_refuses_to_step_down_into_the_scratchpad() {
        assert_eq!(swim_advance_target(1, -1), (1, 0));
        // A multi-step fling against the boundary lands exactly on the
        // edge, applying only the steps that fit.
        assert_eq!(swim_advance_target(2, -5), (1, -1));
    }

    #[test]
    fn swim_advance_target_refuses_to_overflow_stepping_up() {
        assert_eq!(swim_advance_target(u32::MAX, 1), (u32::MAX, 0));
        assert_eq!(swim_advance_target(u32::MAX - 1, 3), (u32::MAX, 1));
    }

    #[test]
    fn swim_advance_target_applies_multiple_steps_within_bounds() {
        assert_eq!(swim_advance_target(3, 3), (6, 3));
        assert_eq!(swim_advance_target(6, -3), (3, -3));
    }

    #[test]
    fn compositor_swipe_uses_the_dominant_axis() {
        assert_eq!(
            completed_swipe_direction(-240.0, 20.0, 200.0),
            Some(Direction::Left)
        );
        assert_eq!(
            completed_swipe_direction(20.0, 240.0, 200.0),
            Some(Direction::Down)
        );
        assert_eq!(completed_swipe_direction(199.0, 0.0, 200.0), None);
    }
}
