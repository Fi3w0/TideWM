//! `org.freedesktop.a11y.KeyboardMonitor` over the session DBus: lets a
//! screen reader (Orca) grab or watch specific keys system-wide, regardless
//! of window focus. Wayland's own per-client input isolation otherwise
//! makes this impossible -- a client can't snoop another client's
//! keyboard input, and an accessibility tool is not a special case to
//! Wayland itself, so the compositor has to be the one exposing this.
//! Interface shape and grab semantics ported from niri's own
//! `src/dbus/freedesktop_a11y.rs` (the reference this project studies for
//! foundational mechanisms), read directly rather than guessed at.
//!
//! **Deliberately narrower than niri's own `Manager`:** niri bundles
//! `PointerLocator` (mouse-review support, a synchronous
//! DBus-thread-asks-compositor-for-pointer-contents round trip) into the
//! same object. Not ported here -- no request for mouse review, and it
//! would need its own bidirectional channel; `KeyboardMonitor` alone is
//! what makes Orca's *own* keybinds (e.g. its modifier key) work at all,
//! which is the load-bearing piece.
//!
//! **Threading, deliberately stricter than niri's own reference:** niri's
//! `process_key` (called synchronously from the compositor's own input
//! handling) locks its shared grab state and then calls
//! `async_io::block_on` for `KeyEvent` signal emission *while still
//! holding that lock*, blocking the compositor's own thread on DBus I/O.
//! This project has already been burned twice by a blocking call hiding
//! in a hot synchronous path (`TileMoveGrab`'s deadlock, the fence-wait
//! retry loop) and by the same "never touch `Smallvil` from a DBus
//! thread" rule `screencast/mod.rs` already established, so this doesn't
//! copy that shape: `process_key` only ever touches the shared
//! `Arc<Mutex<KeyboardGrabs>>` (a fast, in-memory, never-held-across-I/O
//! critical section, the same safe shape `ScreencastState.outputs`
//! already uses) and queues outbound `KeyEvent`s on a bounded
//! `std::sync::mpsc` channel; the actual (blocking) signal emission
//! happens on the DBus thread itself, which has nothing else to do while
//! idle anyway. See `dbus.rs` for that half.

mod dbus;
mod tree;

pub(crate) use tree::{GroupSnapshot, UiSnapshot};

use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::TrySendError,
    Arc, Mutex,
};
use std::time::Duration;

use smithay::input::keyboard::Keysym;
use zbus::names::OwnedUniqueName;

const KEY_EVENT_QUEUE_CAPACITY: usize = 4096;

/// Result of `AccessibilityState::process_key`, read by
/// `Smallvil`'s own keyboard-input handling (`input.rs`) to decide whether
/// this keystroke should reach the compositor's normal dispatch at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KbMonBlock {
    /// Not grabbed/watched by anyone in a way that blocks it -- handle normally.
    Pass,
    /// Blocked, and this is the first press of a grabbed modifier held
    /// alone -- the caller must not let this touch XKB state at all (skip
    /// calling `keyboard.input()` for it entirely), matching niri's own
    /// handling of this case exactly.
    ModifierFirstPress,
    /// Blocked; suppress normal dispatch for this key (press or release).
    Block,
}

#[derive(Debug, Default)]
struct ClientGrab {
    watched: bool,
    grabbed: bool,
    modifiers: HashSet<Keysym>,
    keystrokes: Vec<(Keysym, u32)>,
}

impl ClientGrab {
    fn should_grab_keypress(
        &self,
        suppressed_keys: &HashSet<Keysym>,
        mods: u32,
        keysym: Keysym,
    ) -> bool {
        if self.grabbed {
            return true;
        }
        for modifier in &self.modifiers {
            // Either this key IS a grabbed modifier, or a grabbed modifier
            // is currently held down (so e.g. Grabbed-Mod+X grabs X too).
            if *modifier == keysym || suppressed_keys.contains(modifier) {
                return true;
            }
        }
        self.keystrokes
            .iter()
            .any(|(sym, grabbed_mods)| *sym == keysym && *grabbed_mods == mods)
    }

    fn should_watch_keypress(
        &self,
        suppressed_keys: &HashSet<Keysym>,
        mods: u32,
        keysym: Keysym,
    ) -> bool {
        self.watched || self.should_grab_keypress(suppressed_keys, mods, keysym)
    }
}

#[derive(Debug, Default)]
struct KeyboardGrabs {
    /// Keyed by the client's DBus unique name (`":1.42"`-shaped), not a
    /// well-known name -- that's what `zbus`'s message header hands us,
    /// and it's stable for the life of one connection regardless of
    /// whether the client owns any well-known name at all.
    clients: HashMap<OwnedUniqueName, ClientGrab>,
    /// Union of every client's grabbed modifiers, rebuilt on every
    /// `SetKeyGrabs`/disconnect rather than recomputed per keystroke.
    grabbed_mods: HashSet<Keysym>,
    grabbed_mod_last_press: HashMap<Keysym, Duration>,
    /// Keys currently down that were grabbed on press, so the matching
    /// release is also suppressed instead of leaking through.
    suppressed_keys: HashSet<Keysym>,
    /// DBus clients that received each key's press. Modifier state may
    /// change before release, so recomputing recipients at release time can
    /// otherwise deliver an unmatched press with no corresponding release.
    pressed_recipients: HashMap<Keysym, HashSet<OwnedUniqueName>>,
}

impl KeyboardGrabs {
    fn rebuild_grabbed_mods(&mut self) {
        self.grabbed_mods.clear();
        for client in self.clients.values() {
            self.grabbed_mods.extend(&client.modifiers);
        }
        self.grabbed_mod_last_press
            .retain(|keysym, _| self.grabbed_mods.contains(keysym));
    }
}

/// One outbound `KeyEvent` signal, queued by `process_key` (compositor
/// thread) and drained by the DBus thread, which does the actual send.
struct KeyEventMsg {
    destination: OwnedUniqueName,
    released: bool,
    mods: u32,
    keysym: u32,
    unichar: u32,
    keycode: u32,
}

pub struct AccessibilityState {
    grabs: Arc<Mutex<KeyboardGrabs>>,
    to_dbus: std::sync::mpsc::SyncSender<KeyEventMsg>,
    dbus_backpressured: AtomicBool,
    tree: Option<tree::AccessibilityTree>,
}

impl AccessibilityState {
    pub(crate) fn update_ui(&mut self, snapshot: UiSnapshot) {
        if let Some(tree) = self.tree.as_mut() {
            tree.update(snapshot);
        }
    }

    /// Called once per real key press/release, before the compositor's own
    /// keybind/client dispatch -- see `Smallvil::a11y_process_key` in
    /// `input.rs` for the exact call site and why it has to run before
    /// `keyboard.input()`, not inside its filter closure. Ported from
    /// niri's `KeyboardMonitor::process_key`, with signal emission moved
    /// off this call path (see this module's own doc).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn process_key(
        &self,
        repeat_delay: Duration,
        time: Duration,
        released: bool,
        mods: u32,
        keysym: Keysym,
        unichar: u32,
        keycode: u32,
    ) -> KbMonBlock {
        let mut data = self.grabs.lock().unwrap();
        let mut delivery_failed = false;

        let current_recipients: HashSet<OwnedUniqueName> = data
            .clients
            .iter()
            .filter(|(_, client)| client.should_watch_keypress(&data.suppressed_keys, mods, keysym))
            .map(|(name, _)| name.clone())
            .collect();
        let recipients = if released {
            data.pressed_recipients
                .remove(&keysym)
                .unwrap_or(current_recipients)
        } else {
            data.pressed_recipients
                .entry(keysym)
                .or_default()
                .extend(current_recipients.iter().cloned());
            current_recipients
        };

        for name in recipients {
            let message = KeyEventMsg {
                destination: name,
                released,
                mods,
                keysym: keysym.raw(),
                unichar,
                keycode,
            };
            match self.to_dbus.try_send(message) {
                Ok(()) => {
                    if self.dbus_backpressured.swap(false, Ordering::AcqRel) {
                        tracing::info!("Accessibility DBus key-event queue recovered");
                    }
                }
                Err(TrySendError::Full(_)) => {
                    delivery_failed = true;
                    if !self.dbus_backpressured.swap(true, Ordering::AcqRel) {
                        tracing::warn!(
                            capacity = KEY_EVENT_QUEUE_CAPACITY,
                            "Accessibility DBus key-event queue full; dropping events"
                        );
                    }
                }
                Err(TrySendError::Disconnected(_)) => delivery_failed = true,
            }
        }

        if delivery_failed {
            // Grabbing a key is only safe while its event can actually be
            // delivered. Fail open if the DBus consumer is gone or cannot
            // keep up, otherwise a stalled accessibility client could make
            // normal keyboard input disappear indefinitely.
            data.clients.clear();
            data.rebuild_grabbed_mods();
            data.suppressed_keys.clear();
            data.pressed_recipients.clear();
            return KbMonBlock::Pass;
        }

        // A grabbed modifier pressed twice within the repeat-delay window
        // passes through as an ordinary keypress instead of being grabbed
        // -- lets an a11y tool's own "tap the modifier alone" gesture
        // (e.g. Orca's own modifier key) still work.
        if data.grabbed_mods.contains(&keysym) {
            if released {
                if !data.suppressed_keys.contains(&keysym) {
                    return KbMonBlock::Pass;
                }
            } else {
                let last_press = data
                    .grabbed_mod_last_press
                    .get(&keysym)
                    .copied()
                    .unwrap_or(Duration::ZERO);
                data.grabbed_mod_last_press.insert(keysym, time);
                if time <= last_press.saturating_add(repeat_delay) {
                    return KbMonBlock::Pass;
                }
            }
        }

        let mut block = false;
        if released {
            if data.suppressed_keys.remove(&keysym) {
                block = true;
            }
        } else if data.suppressed_keys.contains(&keysym) {
            // Second press for an already-down key (e.g. two keyboards).
            block = true;
        } else if data
            .clients
            .values()
            .any(|c| c.should_grab_keypress(&data.suppressed_keys, mods, keysym))
        {
            data.suppressed_keys.insert(keysym);
            block = true;
        }

        if !block {
            KbMonBlock::Pass
        } else if data.grabbed_mods.contains(&keysym) {
            KbMonBlock::ModifierFirstPress
        } else {
            KbMonBlock::Block
        }
    }
}

/// Spawns the DBus service thread and returns the compositor-side handle.
/// Fire-and-forget, same as `screencast::init`: a failure to bind the bus
/// name is logged (see `dbus.rs`) and leaves accessibility unavailable,
/// not a reason to fail compositor startup over an optional feature. No
/// `LoopHandle` needed here -- unlike `screencast`, nothing flows from the
/// DBus thread back into the compositor's own event loop; every method
/// this interface exposes only ever mutates the shared grab state.
pub fn init() -> AccessibilityState {
    let grabs = Arc::new(Mutex::new(KeyboardGrabs::default()));
    let (to_dbus, from_compositor) = std::sync::mpsc::sync_channel(KEY_EVENT_QUEUE_CAPACITY);
    dbus::spawn(grabs.clone(), from_compositor);
    AccessibilityState {
        grabs,
        to_dbus,
        dbus_backpressured: AtomicBool::new(false),
        tree: Some(tree::AccessibilityTree::new(UiSnapshot::default())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No real DBus connection -- `process_key`'s own logic is a pure
    /// state machine over `KeyboardGrabs`, exercisable directly the same
    /// way this project already tests other logic real hardware/injection
    /// tools in this sandbox can't reach (see AGENT.md's own "test the
    /// underlying state-mutating function directly" precedent). The
    /// The receiver stays alive for the test: a disconnected DBus consumer
    /// deliberately makes production fail open and discard active grabs.
    fn state() -> (AccessibilityState, std::sync::mpsc::Receiver<KeyEventMsg>) {
        let (to_dbus, from_compositor) = std::sync::mpsc::sync_channel(16);
        (
            AccessibilityState {
                grabs: Arc::new(Mutex::new(KeyboardGrabs::default())),
                to_dbus,
                dbus_backpressured: AtomicBool::new(false),
                tree: None,
            },
            from_compositor,
        )
    }

    const DELAY: Duration = Duration::from_millis(200);

    #[test]
    fn ungrabbed_key_always_passes() {
        let (state, _receiver) = state();
        let sym = Keysym::a;
        assert_eq!(
            state.process_key(DELAY, Duration::ZERO, false, 0, sym, 0, 30),
            KbMonBlock::Pass
        );
        assert_eq!(
            state.process_key(DELAY, Duration::from_millis(1), true, 0, sym, 0, 30),
            KbMonBlock::Pass
        );
    }

    #[test]
    fn full_grab_blocks_every_key_press_and_its_matching_release() {
        let (state, _receiver) = state();
        state.grabs.lock().unwrap().clients.insert(
            OwnedUniqueName::try_from(":1.1").unwrap(),
            ClientGrab {
                grabbed: true,
                ..Default::default()
            },
        );
        let sym = Keysym::a;

        assert_eq!(
            state.process_key(DELAY, Duration::ZERO, false, 0, sym, 0, 30),
            KbMonBlock::Block
        );
        // The matching release must also be blocked (suppressed_keys
        // tracking), not just the press.
        assert_eq!(
            state.process_key(DELAY, Duration::from_millis(50), true, 0, sym, 0, 30),
            KbMonBlock::Block
        );
        // Released and no longer suppressed: a *third* "release" (e.g. a
        // stray autorepeat artifact) passes through again rather than
        // blocking forever.
        assert_eq!(
            state.process_key(DELAY, Duration::from_millis(60), true, 0, sym, 0, 30),
            KbMonBlock::Pass
        );
    }

    #[test]
    fn grabbed_key_release_keeps_press_recipients_after_modifier_release() {
        let (state, receiver) = state();
        let client = OwnedUniqueName::try_from(":1.1").unwrap();
        let control = Keysym::Control_L;
        let key = Keysym::a;
        {
            let mut grabs = state.grabs.lock().unwrap();
            grabs.clients.insert(
                client.clone(),
                ClientGrab {
                    modifiers: HashSet::from([control]),
                    ..Default::default()
                },
            );
            grabs.rebuild_grabbed_mods();
        }

        assert_eq!(
            state.process_key(DELAY, Duration::from_secs(1), false, 0, control, 0, 29),
            KbMonBlock::ModifierFirstPress
        );
        assert_eq!(
            state.process_key(DELAY, Duration::from_secs(1), false, 4, key, 97, 30),
            KbMonBlock::Block
        );
        assert_eq!(
            state.process_key(DELAY, Duration::from_secs(2), true, 0, control, 0, 29),
            KbMonBlock::ModifierFirstPress
        );
        assert_eq!(
            state.process_key(DELAY, Duration::from_secs(2), true, 0, key, 97, 30),
            KbMonBlock::Block
        );

        let messages: Vec<_> = receiver.try_iter().collect();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3].destination, client);
        assert!(messages[3].released);
        assert_eq!(messages[3].keysym, key.raw());
    }

    #[test]
    fn grabbed_modifier_double_tap_within_repeat_delay_passes_through() {
        let (state, _receiver) = state();
        let sym = Keysym::Super_L;
        {
            let mut data = state.grabs.lock().unwrap();
            data.clients.insert(
                OwnedUniqueName::try_from(":1.1").unwrap(),
                ClientGrab {
                    modifiers: HashSet::from([sym]),
                    ..Default::default()
                },
            );
            // The real `SetKeyGrabs` DBus handler always calls this after
            // touching `modifiers` -- `grabbed_mods` is a derived index,
            // not recomputed lazily, so skipping it here would test a
            // state `process_key` never actually sees in production.
            data.rebuild_grabbed_mods();
        }

        // Realistic-scale timestamps, not test-local small numbers: `time`
        // is a monotonic-clock reading in real use (e.g. milliseconds
        // since boot), so it's always far larger than the repeat delay --
        // a `last_press` timestamp that defaults to `Duration::ZERO` (no
        // prior press recorded yet) must never accidentally look "recent"
        // relative to it, or the very first press of a session would
        // wrongly pass through as a double-tap. And a real keyboard always
        // alternates press/release for the same key -- three presses in a
        // row with no release between them (what an earlier version of
        // this test did) isn't a sequence a real keyboard can produce, and
        // trips the *separate* already-pressed-key suppression instead of
        // the double-tap path this test means to exercise.
        let base = Duration::from_secs(1000);
        let at = |ms: u64| base + Duration::from_millis(ms);

        // First press: grabbed, and specifically the "don't touch XKB
        // state" variant since this IS a grabbed modifier.
        assert_eq!(
            state.process_key(DELAY, at(0), false, 0, sym, 0, 125),
            KbMonBlock::ModifierFirstPress
        );
        assert_eq!(
            state.process_key(DELAY, at(10), true, 0, sym, 0, 125),
            KbMonBlock::ModifierFirstPress
        );

        // Second press within the repeat-delay window of the *first*
        // press: treated as an ordinary keypress (e.g. the a11y tool's
        // own "tap modifier alone" gesture), not grabbed -- and so is its
        // release.
        assert_eq!(
            state.process_key(DELAY, at(50), false, 0, sym, 0, 125),
            KbMonBlock::Pass
        );
        assert_eq!(
            state.process_key(DELAY, at(60), true, 0, sym, 0, 125),
            KbMonBlock::Pass
        );

        // A press well after the delay window (measured from the *second*
        // press) is grabbed again.
        assert_eq!(
            state.process_key(DELAY, at(500), false, 0, sym, 0, 125),
            KbMonBlock::ModifierFirstPress
        );
    }

    #[test]
    fn rebuilding_grabbed_modifiers_prunes_old_press_timestamps() {
        let mut grabs = KeyboardGrabs::default();
        grabs
            .grabbed_mod_last_press
            .insert(Keysym::Super_L, Duration::from_secs(1));
        grabs.rebuild_grabbed_mods();
        assert!(grabs.grabbed_mod_last_press.is_empty());
    }

    #[test]
    fn watch_only_client_never_blocks_but_still_gets_queued_a_key_event() {
        let (to_dbus, from_compositor) = std::sync::mpsc::sync_channel(1);
        let state = AccessibilityState {
            grabs: Arc::new(Mutex::new(KeyboardGrabs::default())),
            to_dbus,
            dbus_backpressured: AtomicBool::new(false),
            tree: None,
        };
        state.grabs.lock().unwrap().clients.insert(
            OwnedUniqueName::try_from(":1.1").unwrap(),
            ClientGrab {
                watched: true,
                ..Default::default()
            },
        );
        let sym = Keysym::a;

        assert_eq!(
            state.process_key(DELAY, Duration::ZERO, false, 0, sym, 97, 30),
            KbMonBlock::Pass
        );
        let msg = from_compositor
            .try_recv()
            .expect("watched client should have been queued a KeyEvent");
        assert_eq!(msg.keysym, sym.raw());
        assert_eq!(msg.unichar, 97);
        assert!(!msg.released);
    }

    #[test]
    fn dbus_backpressure_discards_grabs_and_fails_open() {
        let (to_dbus, _from_compositor) = std::sync::mpsc::sync_channel(1);
        let state = AccessibilityState {
            grabs: Arc::new(Mutex::new(KeyboardGrabs::default())),
            to_dbus,
            dbus_backpressured: AtomicBool::new(false),
            tree: None,
        };
        state.grabs.lock().unwrap().clients.insert(
            OwnedUniqueName::try_from(":1.1").unwrap(),
            ClientGrab {
                grabbed: true,
                ..Default::default()
            },
        );

        assert_eq!(
            state.process_key(DELAY, Duration::ZERO, false, 0, Keysym::a, 0, 30),
            KbMonBlock::Block
        );
        assert_eq!(
            state.process_key(DELAY, Duration::from_millis(1), false, 0, Keysym::b, 0, 48,),
            KbMonBlock::Pass
        );
        let grabs = state.grabs.lock().unwrap();
        assert!(grabs.clients.is_empty());
        assert!(grabs.suppressed_keys.is_empty());
    }
}
