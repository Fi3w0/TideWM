//! `org.gnome.Mutter.ScreenCast` + `.Session` + `.Stream` on the session
//! DBus, via `zbus::interface`. Matches niri's choice of the Mutter
//! interface over `xdg-desktop-portal-wlr`'s own (see AGENT.md's
//! "Screencasting" section): `xdg-desktop-portal-gnome` already knows how
//! to bridge this interface to the standard portal, so any user with it
//! installed gets a working screencast without TideWM implementing the
//! portal's own permission-dialog session dance.
//!
//! **What's real:** the object lifecycle (`CreateSession` ->
//! `RecordMonitor` -> `Stop`), connector validation against a live output
//! snapshot, and `Stream::Parameters`. **What's not:** `Session::Start`
//! always returns an error instead of ever emitting
//! `PipeWireStreamAdded`, because `pipewire_thread` doesn't exist yet --
//! a real client would otherwise hang forever waiting for a signal that
//! never comes. `RecordWindow`/`RecordArea`/`RecordVirtual` aren't
//! implemented at all (not declared, so a real client sees the standard
//! DBus `UnknownMethod` error) -- YAGNI until something other than
//! whole-monitor capture is the target.
//!
//! **Verification honesty note:** this interface's shape (method/signal
//! names, argument order, object path scheme) is written from
//! documented/well-known knowledge of Mutter's screencast DBus API, not
//! cross-checked against Mutter's own source or a running GNOME Shell --
//! neither is available on this machine (Hyprland, no `mutter` package
//! installed). It's been tested with a direct `zbus` client written for
//! this purpose (round-trips `CreateSession`/`RecordMonitor`/`Stop`
//! correctly, rejects unknown connectors, `Start` fails as designed).
//! It has *not* been tested against a real portal-mediated client (OBS,
//! `xdg-desktop-portal-gnome`): neither active portal backend on this
//! machine (`xdg-desktop-portal-hyprland`, `-gtk`) calls into
//! `org.gnome.Mutter.ScreenCast` at all, so that path has nothing to
//! route through here regardless of what TideWM implements. See
//! CHANGELOG for the exact verification breakdown.
//!
//! Runs on its own OS thread via `zbus::blocking`, not calloop: zbus's
//! blocking connection dispatches incoming method calls on its own
//! internal executor for the life of the `Connection`, so this thread's
//! only job is to build the connection, register the interfaces, and
//! then block forever keeping it alive. It never touches `Smallvil`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::OutputSnapshot;

/// Spawns the DBus service thread. Fire-and-forget: a failure here (bus
/// unreachable, name already taken) is logged and leaves screencasting
/// unavailable, not a reason to fail compositor startup over an
/// optional, default-off feature.
pub(super) fn spawn(outputs: Arc<Mutex<Vec<OutputSnapshot>>>) {
    std::thread::Builder::new()
        .name("screencast-dbus".into())
        .spawn(move || run(outputs))
        .expect("failed to spawn screencast DBus thread");
}

fn run(outputs: Arc<Mutex<Vec<OutputSnapshot>>>) {
    let root = Mutter {
        outputs,
        next_id: Arc::new(AtomicU64::new(1)),
    };

    let _connection = match zbus::blocking::connection::Builder::session()
        .and_then(|builder| builder.serve_at("/org/gnome/Mutter/ScreenCast", root))
        .and_then(|builder| builder.name("org.gnome.Mutter.ScreenCast"))
        .and_then(|builder| builder.build())
    {
        Ok(connection) => connection,
        Err(err) => {
            tracing::warn!(%err, "Screencast DBus service failed to start");
            return;
        }
    };
    tracing::info!("Screencast DBus service registered as org.gnome.Mutter.ScreenCast");

    // Keeps `_connection` (and with it, the registered interfaces) alive
    // for the process lifetime. zbus dispatches incoming calls on its
    // own internal executor threads regardless of whether this thread is
    // parked; there is nothing left for this thread itself to do.
    loop {
        std::thread::park();
    }
}

/// One counter shared by every dynamically-created `Session`/`Stream`
/// object, rather than per-type or per-session counters: `Session::Stop`
/// doesn't unregister its `Stream` objects from the bus yet (a known M1
/// gap -- see `Session::stop`), so a per-session counter restarting from
/// 1 could collide with a still-registered object from an earlier,
/// stopped session. A single ever-increasing counter can't collide with
/// itself.
struct Mutter {
    outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
    next_id: Arc<AtomicU64>,
}

#[interface(name = "org.gnome.Mutter.ScreenCast")]
impl Mutter {
    /// Present mainly so a real portal client never sees `UnknownProperty`
    /// on a `Get` for it -- some clients read this before deciding whether
    /// to proceed at all, so an absent property risks rejecting the whole
    /// backend before `CreateSession` is ever tried, which is worse than
    /// reporting a best-effort number. The value itself (matching recent
    /// Mutter releases at the time of writing) is not cross-checked
    /// against Mutter's own source -- see the module doc's honesty note.
    #[zbus(property)]
    fn version(&self) -> i32 {
        4
    }

    async fn create_session(
        &self,
        _properties: HashMap<String, OwnedValue>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let path = OwnedObjectPath::try_from(format!("/org/gnome/Mutter/ScreenCast/Session/u{id}"))
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        let session = Session {
            outputs: self.outputs.clone(),
            next_id: self.next_id.clone(),
            closed: AtomicBool::new(false),
        };
        emitter
            .connection()
            .object_server()
            .at(&path, session)
            .await
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        tracing::debug!(%path, "Screencast session created");
        Ok(path)
    }
}

struct Session {
    outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
    next_id: Arc<AtomicU64>,
    closed: AtomicBool,
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    async fn record_monitor(
        &self,
        connector: String,
        _properties: HashMap<String, OwnedValue>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        if self.closed.load(Ordering::Acquire) {
            return Err(zbus::fdo::Error::Failed("session is closed".into()));
        }

        let (width, height) = self
            .outputs
            .lock()
            .unwrap()
            .iter()
            .find(|output| output.name == connector)
            .map(|output| (output.width, output.height))
            .ok_or_else(|| {
                zbus::fdo::Error::InvalidArgs(format!("unknown output connector: {connector}"))
            })?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let path = OwnedObjectPath::try_from(format!("/org/gnome/Mutter/ScreenCast/Stream/u{id}"))
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        emitter
            .connection()
            .object_server()
            .at(&path, Stream { width, height })
            .await
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        tracing::debug!(%path, connector, "Screencast monitor stream created");
        Ok(path)
    }

    fn start(&self) -> zbus::fdo::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(zbus::fdo::Error::Failed("session is closed".into()));
        }
        // No PipeWire thread exists yet to ever emit `PipeWireStreamAdded`
        // on the streams this session created -- fail loudly now rather
        // than let a real client hang forever waiting for a signal that
        // will never come. See src/screencast/pipewire_thread.rs.
        Err(zbus::fdo::Error::Failed("PipeWire streaming not implemented yet".into()))
    }

    async fn stop(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        self.closed.store(true, Ordering::Release);
        Self::closed(&emitter)
            .await
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        Ok(())
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

struct Stream {
    width: i32,
    height: i32,
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
impl Stream {
    /// Only the `size` key: real Mutter's `Parameters` dict is documented
    /// to carry more (`position`, `cursor-mode`, `mapping-id`...) but
    /// `size` is the one every consumer actually needs before a stream
    /// exists to negotiate PipeWire formats against, and the only one
    /// this module has verified data for. Add the rest if a real client
    /// turns out to require them.
    #[zbus(property)]
    fn parameters(&self) -> HashMap<String, OwnedValue> {
        let mut params = HashMap::new();
        params.insert(
            "size".to_string(),
            OwnedValue::try_from(Value::from((self.width, self.height)))
                .expect("tuple of two i32s always converts to OwnedValue"),
        );
        params
    }

    #[zbus(signal)]
    async fn pipe_wire_stream_added(emitter: &SignalEmitter<'_>, node_id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
