//! `org.freedesktop.a11y.KeyboardMonitor` on the session DBus, at the
//! well-known object path/name real screen readers expect
//! (`/org/freedesktop/a11y/Manager`, `org.freedesktop.a11y.Manager`) --
//! confirmed against niri's own `src/dbus/freedesktop_a11y.rs`, not
//! guessed at. Method/signal shapes match that reference exactly; see this
//! module's parent doc for what's deliberately not ported (`PointerLocator`)
//! and why signal emission is split off the compositor's own thread.
//!
//! Runs on its own OS thread via `zbus::blocking`, matching
//! `screencast/dbus.rs`'s exact precedent in this codebase: the thread's
//! job is to build the connection, register the interface, then loop
//! forever -- here, draining `KeyEventMsg`s and emitting them, rather than
//! `screencast/dbus.rs`'s idle `thread::park()`, since this thread
//! actually has ongoing work once the compositor starts sending it
//! anything. Never touches `Smallvil`.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::message::Header;
use zbus::names::{BusName, OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::NoneValue;
use zbus::{fdo, interface};

use super::{KeyEventMsg, KeyboardGrabs};

const PATH: &str = "/org/freedesktop/a11y/Manager";
const NAME: &str = "org.freedesktop.a11y.Manager";

/// Spawns the DBus service thread. Fire-and-forget: a failure here (bus
/// unreachable, name already taken) is logged and leaves accessibility
/// unavailable, not a reason to fail compositor startup over an optional,
/// default-off feature -- same policy `screencast::dbus::spawn` uses.
pub(super) fn spawn(grabs: Arc<Mutex<KeyboardGrabs>>, from_compositor: mpsc::Receiver<KeyEventMsg>) {
    std::thread::Builder::new()
        .name("a11y-dbus".into())
        .spawn(move || run(grabs, from_compositor))
        .expect("failed to spawn accessibility DBus thread");
}

fn run(grabs: Arc<Mutex<KeyboardGrabs>>, from_compositor: mpsc::Receiver<KeyEventMsg>) {
    let iface = KeyboardMonitor { grabs: grabs.clone() };

    let connection = match zbus::blocking::connection::Builder::session()
        .and_then(|builder| builder.serve_at(PATH, iface))
        .and_then(|builder| builder.name(NAME))
        .and_then(|builder| builder.build())
    {
        Ok(connection) => connection,
        Err(err) => {
            tracing::warn!(%err, "Accessibility DBus service failed to start");
            return;
        }
    };
    tracing::info!(bus_name = NAME, path = PATH, "Accessibility DBus service registered");

    // A client that closes its DBus connection without an explicit
    // Ungrab/Unwatch (crashed, killed) must not leave a phantom grab
    // suppressing keys forever -- watch the bus's own NameOwnerChanged for
    // exactly this client disappearing and clean it up. Spawned onto the
    // connection's own async executor (zbus dispatches incoming calls on
    // it regardless of what this thread does below), same as niri's own
    // `monitor_disappeared_clients`.
    let watch_grabs = grabs.clone();
    let async_conn = connection.inner().clone();
    let task = connection.inner().executor().spawn(
        async move {
            if let Err(err) = watch_disconnects(&async_conn, watch_grabs).await {
                tracing::warn!(%err, "Accessibility DBus disconnect watcher failed");
            }
        },
        "a11y disconnect watcher",
    );
    task.detach();

    let Ok(iface_ref) = connection.object_server().interface::<_, KeyboardMonitor>(PATH) else {
        tracing::warn!("Accessibility DBus interface vanished immediately after registration");
        return;
    };
    let emitter = iface_ref.signal_emitter().clone();

    // This thread's only remaining job: drain outbound KeyEvents and emit
    // them. Blocks on `recv()` while idle, which is correct here -- unlike
    // the compositor's own single event-loop thread, this one has nothing
    // else to do, so blocking it on DBus I/O (via `async_io::block_on`
    // below) can't stall anything else in the process.
    while let Ok(msg) = from_compositor.recv() {
        let ctxt = emitter.clone().set_destination(BusName::Unique(msg.destination.into()));
        async_io::block_on(async {
            if let Err(err) =
                KeyboardMonitor::key_event(&ctxt, msg.released, msg.mods, msg.keysym, msg.unichar, msg.keycode as u16)
                    .await
            {
                tracing::warn!(%err, "Failed to emit accessibility KeyEvent signal");
            }
        });
    }
}

struct KeyboardMonitor {
    grabs: Arc<Mutex<KeyboardGrabs>>,
}

/// Interface for monitoring keyboard input by assistive technologies (a
/// screen reader grabbing/watching keys system-wide). Method/doc shape
/// matches niri's own `src/dbus/freedesktop_a11y.rs` -- this is a
/// well-known, cross-desktop interface (KDE, GNOME/Mutter, niri all
/// implement it), not a TideWM-specific invention.
#[interface(name = "org.freedesktop.a11y.KeyboardMonitor")]
impl KeyboardMonitor {
    /// Grabs every key event: the caller receives everything via
    /// `KeyEvent`, and normal dispatch (compositor keybinds, the focused
    /// client, XKB toggle-state changes) is fully suppressed until
    /// `UngrabKeyboard` or the connection closes.
    async fn grab_keyboard(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        let sender = sender_of(&hdr)?;
        tracing::debug!(%sender, "a11y: GrabKeyboard");
        let mut data = self.grabs.lock().unwrap();
        data.clients.entry(sender).or_default().grabbed = true;
        Ok(())
    }

    /// Reverses `GrabKeyboard`. Any `SetKeyGrabs` keystrokes and
    /// `WatchKeyboard` state stay in effect regardless.
    async fn ungrab_keyboard(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        let sender = sender_of(&hdr)?;
        tracing::debug!(%sender, "a11y: UngrabKeyboard");
        let mut data = self.grabs.lock().unwrap();
        if let Some(client) = data.clients.get_mut(&sender) {
            client.grabbed = false;
        }
        Ok(())
    }

    /// Starts receiving every key event via `KeyEvent`, without suppressing
    /// normal dispatch -- unlike `GrabKeyboard`, the compositor and focused
    /// client still see everything too.
    async fn watch_keyboard(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        let sender = sender_of(&hdr)?;
        tracing::debug!(%sender, "a11y: WatchKeyboard");
        let mut data = self.grabs.lock().unwrap();
        data.clients.entry(sender).or_default().watched = true;
        Ok(())
    }

    /// Reverses `WatchKeyboard`.
    async fn unwatch_keyboard(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        let sender = sender_of(&hdr)?;
        tracing::debug!(%sender, "a11y: UnwatchKeyboard");
        let mut data = self.grabs.lock().unwrap();
        if let Some(client) = data.clients.get_mut(&sender) {
            client.watched = false;
        }
        Ok(())
    }

    /// Replaces this client's specific grab list, overriding any previous
    /// call. `modifiers` is a list of XKB keysyms to grab as bare
    /// modifiers (see `AccessibilityState::process_key`'s double-tap
    /// passthrough); `keystrokes` is `(keysym, xkb_modifier_mask)` pairs
    /// for exact key+modifier combinations.
    async fn set_key_grabs(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        modifiers: Vec<u32>,
        keystrokes: Vec<(u32, u32)>,
    ) -> fdo::Result<()> {
        let sender = sender_of(&hdr)?;
        tracing::debug!(%sender, ?modifiers, ?keystrokes, "a11y: SetKeyGrabs");
        let mut data = self.grabs.lock().unwrap();
        let client = data.clients.entry(sender).or_default();
        client.modifiers = modifiers.into_iter().map(smithay::input::keyboard::Keysym::new).collect();
        client.keystrokes = keystrokes
            .into_iter()
            .map(|(k, mods)| (smithay::input::keyboard::Keysym::new(k), mods))
            .collect();
        data.rebuild_grabbed_mods();
        Ok(())
    }

    /// Emitted for every key press/release a client is grabbing or
    /// watching. `state` is the XKB effective-modifier mask at the time of
    /// this key; `unichar` is the Unicode codepoint, `0` if none.
    #[zbus(signal)]
    async fn key_event(
        ctxt: &SignalEmitter<'_>,
        released: bool,
        state: u32,
        keysym: u32,
        unichar: u32,
        keycode: u16,
    ) -> zbus::Result<()>;
}

fn sender_of(hdr: &Header<'_>) -> fdo::Result<OwnedUniqueName> {
    hdr.sender()
        .map(|name| OwnedUniqueName::from(name.to_owned()))
        .ok_or_else(|| fdo::Error::Failed("no sender on accessibility DBus call".to_owned()))
}

async fn watch_disconnects(conn: &zbus::Connection, grabs: Arc<Mutex<KeyboardGrabs>>) -> zbus::Result<()> {
    let proxy = fdo::DBusProxy::new(conn).await?;
    let mut stream = proxy.receive_name_owner_changed_with_args(&[(2, UniqueName::null_value())]).await?;

    while let Some(signal) = stream.next().await {
        let args = signal.args()?;
        let Some(name) = &**args.old_owner() else { continue };
        // `new_owner` being empty is exactly what the args filter above
        // already selects for -- a client that dropped off the bus, not
        // one that merely changed which name it owns.
        let name = OwnedUniqueName::from(name.to_owned());
        let mut data = grabs.lock().unwrap();
        if data.clients.remove(&name).is_some() {
            tracing::trace!(%name, "Accessibility DBus client disconnected, dropping its grabs");
            data.rebuild_grabbed_mods();
        }
    }

    Ok(())
}
