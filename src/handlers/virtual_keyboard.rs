//! `zwp_virtual_keyboard_v1` handling.
//!
//! A virtual keyboard (wtype, an on-screen keyboard, an input method) is
//! handled like a real keyboard: its keys go through binds, grabs, idle and
//! focus in `process_input_event`. It differs from one in a few ways, all
//! following niri's handling of the same Smithay API:
//!
//! - Its keymap may differ from the configured one. It is swapped onto the
//!   shared seat keyboard before the device's events are processed, and the
//!   configured keymap comes back on the next physical key, on removal, or on
//!   a config reload, with the layout and Caps/Num Lock from before the swap.
//! - A device without a keymap, or with the configured one, uses the seat's
//!   xkb state directly.
//! - Keys an input method forwards back through its own virtual keyboard
//!   while it grabs the keyboard skip the grab, or they would loop.
//! - Modifiers set with the `modifiers` request apply to the shared xkb state
//!   and are not reset on removal: the masks are absolute, so there is no way
//!   to tell which bits this device set.

use smithay::{
    backend::input::{Event, InputEvent, KeyboardKeyEvent},
    input::keyboard::{KeymapFile, Layout},
    utils::SERIAL_COUNTER,
    wayland::{
        input_method::InputMethodSeat,
        virtual_keyboard::{
            VirtualKeyboardBackend, VirtualKeyboardDevice, VirtualKeyboardHandler,
            VirtualKeyboardKeyEvent, VirtualKeyboardSpecialEvent,
        },
    },
};

use crate::Smallvil;

/// Keyboard state that a keymap swap resets, saved to restore afterwards.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SavedKeyboardState {
    layout: Layout,
    caps_lock: bool,
    num_lock: bool,
}

/// A virtual keyboard's keymap, temporarily replacing the configured one.
#[derive(Debug)]
pub(crate) struct VirtualKeyboardKeymap {
    device: VirtualKeyboardDevice,
    keymap: KeymapFile,
    saved: SavedKeyboardState,
}

impl VirtualKeyboardHandler for Smallvil {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        match event {
            InputEvent::Keyboard { event } => {
                let device = event.device();
                self.activate_virtual_keyboard_keymap(&device);
                if self.is_input_method_virtual_keyboard(&device) {
                    self.forward_input_method_key(&event);
                } else {
                    self.process_input_event(InputEvent::<VirtualKeyboardBackend>::Keyboard {
                        event,
                    });
                }
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::Modifiers {
                device,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            }) => {
                self.activate_virtual_keyboard_keymap(&device);
                let keyboard = self.seat.get_keyboard().unwrap();
                keyboard.with_xkb_state(self, |mut context| {
                    context.set_modifier_mask(mods_depressed, mods_latched, mods_locked, group);
                });
            }
            // The new keymap is activated by the device's next key or
            // modifiers event.
            InputEvent::Special(VirtualKeyboardSpecialEvent::KeymapChanged { .. }) => {}
            InputEvent::DeviceRemoved { device } => {
                // Checked by device, not keymap: the client may have replaced
                // its keymap after it was activated. Smithay has already
                // released the device's held keys.
                let active = self
                    .virtual_keyboard_keymap
                    .as_ref()
                    .is_some_and(|active| active.device == device);
                if active {
                    self.restore_configured_keymap();
                }
                self.process_input_event(InputEvent::<VirtualKeyboardBackend>::DeviceRemoved {
                    device,
                });
            }
            event => self.process_input_event(event),
        }
    }
}

impl Smallvil {
    /// Whether the device belongs to the input method holding the keyboard grab.
    fn is_input_method_virtual_keyboard(&self, device: &VirtualKeyboardDevice) -> bool {
        let grab_client = self.seat.input_method().keyboard_grab_client();
        grab_client.is_some() && grab_client == device.client()
    }

    /// Sends a key the input method passes through straight to the focused
    /// surface. Its physical key already went through binds and then to the
    /// input method; handling it again would send it back to the grab.
    fn forward_input_method_key(&mut self, event: &VirtualKeyboardKeyEvent) {
        let keyboard = self.seat.get_keyboard().unwrap();
        // The physical key's modifiers went to the input method, not the
        // surface, so always send the current ones.
        keyboard.input_forward_bypassing_grab(
            self,
            event.key_code(),
            event.state(),
            SERIAL_COUNTER.next_serial(),
            event.time(),
            true,
        );
    }

    /// Puts the device's keymap on the seat keyboard, unless it already is.
    fn activate_virtual_keyboard_keymap(&mut self, device: &VirtualKeyboardDevice) {
        let Some(keymap) = device.keymap() else {
            // Keycodes are then relative to the configured keymap.
            self.restore_configured_keymap();
            return;
        };

        if let Some(active) = &mut self.virtual_keyboard_keymap {
            if active.keymap.id() == keymap.id() {
                // Another device may have installed an identical keymap. Take
                // it over, so removing that device leaves this one working.
                active.device = device.clone();
                return;
            }
        }

        let keyboard = self.seat.get_keyboard().unwrap();
        // An identical keymap (an input method forwarding keys with the one
        // it got from us) needs no swap, which would reset keyboard state.
        if self.virtual_keyboard_keymap.is_none() && keyboard.keymap_file().id() == keymap.id() {
            return;
        }

        // Saved from before the first swap, so chained virtual keymaps still
        // restore the user's own state.
        let saved = match &self.virtual_keyboard_keymap {
            Some(active) => active.saved,
            None => self.current_keyboard_state(),
        };
        let configured = match self.configured_keymap.take() {
            Some(configured) => configured,
            None => keyboard.keymap_file(),
        };
        self.configured_keymap = Some(configured);
        if let Err(err) = keyboard.set_keymap_from_keymap_file(self, keymap.clone()) {
            tracing::warn!(?err, "Failed to apply virtual keyboard keymap");
            return;
        }
        self.virtual_keyboard_keymap = Some(VirtualKeyboardKeymap {
            device: device.clone(),
            keymap,
            saved,
        });
    }

    /// Switches the seat back to the configured keymap, if a virtual
    /// keyboard's replaced it. A no-op otherwise.
    pub(crate) fn restore_configured_keymap(&mut self) {
        let Some(active) = self.virtual_keyboard_keymap.take() else {
            return;
        };
        let keyboard = self.seat.get_keyboard().unwrap();
        let restored = self.configured_keymap.take().is_some_and(|keymap| {
            keyboard
                .set_keymap_from_keymap_file(self, keymap)
                .inspect_err(|err| tracing::warn!(?err, "Failed to restore configured keymap"))
                .is_ok()
        });
        if !restored {
            // Fall back to compiling the configured keymap from the config.
            let input = self.config.input.clone();
            if let Err(err) = keyboard.set_xkb_config(self, input.xkb_config()) {
                tracing::error!(?err, "Failed to reload configured keymap");
            }
        }
        self.restore_keyboard_state(active.saved);
    }

    /// Forgets any active virtual keymap after the configured keymap was
    /// replaced on the seat (config reload), keeping the user's lock state.
    pub(crate) fn configured_keymap_replaced(&mut self) {
        self.configured_keymap = None;
        if let Some(active) = self.virtual_keyboard_keymap.take() {
            self.restore_keyboard_state(active.saved);
        }
    }

    fn current_keyboard_state(&mut self) -> SavedKeyboardState {
        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let layout = keyboard.with_xkb_state(self, |context| {
            context.xkb().lock().unwrap().active_layout()
        });
        SavedKeyboardState {
            layout,
            caps_lock: mods.caps_lock,
            num_lock: mods.num_lock,
        }
    }

    fn restore_keyboard_state(&mut self, state: SavedKeyboardState) {
        let keyboard = self.seat.get_keyboard().unwrap();
        let mut mods = keyboard.modifier_state();
        if mods.caps_lock != state.caps_lock || mods.num_lock != state.num_lock {
            mods.caps_lock = state.caps_lock;
            mods.num_lock = state.num_lock;
            keyboard.set_modifier_state(mods);
            // set_modifier_state() doesn't tell the focused client itself.
            keyboard.advertise_modifier_state(self);
        }
        // set_modifier_state() can reset the layout, so restore it after.
        keyboard.with_xkb_state(self, |mut context| {
            let (exists, current) = {
                let xkb = context.xkb().lock().unwrap();
                (
                    xkb.layouts().any(|layout| layout == state.layout),
                    xkb.active_layout(),
                )
            };
            if exists && current != state.layout {
                context.set_layout(state.layout);
            }
        });
    }
}
