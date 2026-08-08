//! `org.freedesktop.impl.portal.ScreenCast`, the actual `xdg-desktop-portal`
//! backend interface, registered as `org.freedesktop.impl.portal.desktop.tidewm`
//! on the same DBus connection `dbus.rs` already holds open for
//! `org.gnome.Mutter.ScreenCast`.
//!
//! This is the door apps like Discord and OBS actually reach through: they
//! call `org.freedesktop.portal.ScreenCast` on the `xdg-desktop-portal`
//! frontend daemon, which forwards to whichever backend `portals.conf`
//! names for this interface. `org.gnome.Mutter.ScreenCast` alone only helps
//! users willing to install `xdg-desktop-portal-gnome`, which pulls in a
//! `gtk4`/`libadwaita`/`nautilus` dependency chain this project doesn't want
//! to ask for. Implementing the real backend interface directly keeps
//! screencasting fully self-contained, and ships alongside `share/xdg-desktop-portal/tidewm.portal`
//! and `tidewm-portals.conf`, both of which are load-bearing -- nothing
//! routes to this code without them. See AGENT.md's "Screencasting" section.
//!
//! One stream is supported per session. `Start` asks the compositor thread
//! to show its source picker; monitor, window, and virtual choices never
//! require the D-Bus worker to inspect live compositor state. This proves the whole
//! `xdg-desktop-portal` -> PipeWire pipe end-to-end as the smallest
//! reviewable slice, the same "land the safe testable core first" sequencing
//! the original Mutter/PipeWire split used.
//!
//! **Verification honesty note:** the interface/method/property shapes are
//! copied from the installed `xdg-desktop-portal` package's own
//! `org.freedesktop.impl.portal.ScreenCast.xml`, not guessed. The service
//! has been driven with direct `busctl` calls, but a real `xdg-desktop-portal`
//! process cannot be rerouted to it from a nested session: the already-running
//! system portal daemon picked its backend from `XDG_CURRENT_DESKTOP` at its
//! own startup, before this process (or its `XDG_CURRENT_DESKTOP=tidewm`)
//! existed. Real Discord/OBS validation needs a fresh login on real hardware.
//! See CHANGELOG for the exact verification breakdown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::interface;
use zbus::message::Header;
use zbus::names::{OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{NoneValue, OwnedObjectPath, OwnedValue, Value};

use super::{pipewire_thread, OutputSnapshot, ScreencastEvent};

const MAX_PORTAL_SESSIONS: usize = 8;

const SOURCE_TYPE_MONITOR: u32 = 1;
const SOURCE_TYPE_WINDOW: u32 = 2;
const SOURCE_TYPE_VIRTUAL: u32 = 4;
const SOURCE_TYPES: u32 = SOURCE_TYPE_MONITOR | SOURCE_TYPE_WINDOW | SOURCE_TYPE_VIRTUAL;
/// `AvailableCursorModes` bits this backend can actually satisfy: `Hidden`
/// (1) and `Embedded` (2). `Metadata` (4) is not implemented.
const CURSOR_MODES: u32 = 1 | 2;

pub(super) struct SessionEntry {
    owner: OwnedUniqueName,
    /// False while the path is only reserved in `SessionMap`; callers cannot
    /// use the session until object-server registration has succeeded.
    registered: AtomicBool,
    /// `None` until `SelectSources` runs; `Start` requires it.
    draw_cursor: Mutex<Option<bool>>,
    source_types: Mutex<Option<u32>>,
    stream: Mutex<PortalStreamState>,
}

struct PortalStream {
    _handle: pipewire_thread::StreamHandle,
}

enum PortalStreamState {
    Idle,
    Starting,
    Active { _stream: PortalStream },
    Closed,
}

impl SessionEntry {
    fn close(&self) {
        // Drop a live PipeWire handle after releasing the state mutex: its
        // destructor stops and joins the worker thread.
        let previous = {
            let mut state = self.stream.lock().unwrap();
            std::mem::replace(&mut *state, PortalStreamState::Closed)
        };
        drop(previous);
    }
}

/// Owns the atomic `Starting` reservation across the picker/PipeWire awaits.
/// Any early return resets it to `Idle`; a concurrent close changes it to
/// `Closed`, causing `complete` to reject and drop the newly started stream.
struct StartReservation {
    entry: Arc<SessionEntry>,
    armed: bool,
}

impl StartReservation {
    fn begin(entry: Arc<SessionEntry>) -> Option<Self> {
        {
            let mut state = entry.stream.lock().unwrap();
            if !matches!(*state, PortalStreamState::Idle) {
                return None;
            }
            *state = PortalStreamState::Starting;
        }
        Some(Self { entry, armed: true })
    }

    fn complete(mut self, stream: PortalStream) -> Result<(), PortalStream> {
        let mut state = self.entry.stream.lock().unwrap();
        let result = if matches!(*state, PortalStreamState::Starting) {
            *state = PortalStreamState::Active { _stream: stream };
            Ok(())
        } else {
            Err(stream)
        };
        self.armed = false;
        result
    }
}

impl Drop for StartReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.entry.stream.lock().unwrap();
        if matches!(*state, PortalStreamState::Starting) {
            *state = PortalStreamState::Idle;
        }
    }
}

pub(super) type SessionMap = HashMap<OwnedObjectPath, Arc<SessionEntry>>;

fn reserve_session(
    sessions: &mut SessionMap,
    path: OwnedObjectPath,
    entry: Arc<SessionEntry>,
) -> bool {
    if sessions.len() >= MAX_PORTAL_SESSIONS || sessions.contains_key(&path) {
        return false;
    }
    sessions.insert(path, entry);
    true
}

fn remove_session_if_same(
    sessions: &mut SessionMap,
    path: &OwnedObjectPath,
    expected: &Arc<SessionEntry>,
) -> bool {
    if !sessions
        .get(path)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
    {
        return false;
    }
    sessions.remove(path);
    true
}

pub(super) struct Portal {
    compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
    next_id: Arc<AtomicU64>,
    sessions: Arc<Mutex<SessionMap>>,
}

impl Portal {
    pub(super) fn new(
        _outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
        compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
        next_id: Arc<AtomicU64>,
    ) -> (Self, Arc<Mutex<SessionMap>>) {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        (
            Self {
                compositor,
                next_id,
                sessions: sessions.clone(),
            },
            sessions,
        )
    }
}

fn sender_of(header: &Header<'_>) -> zbus::fdo::Result<OwnedUniqueName> {
    header
        .sender()
        .map(|name| OwnedUniqueName::from(name.to_owned()))
        .ok_or_else(|| zbus::fdo::Error::Failed("no sender on portal screencast DBus call".into()))
}

const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_OTHER_ERROR: u32 = 2;

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl Portal {
    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        SOURCE_TYPES
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        CURSOR_MODES
    }

    /// Claims support for interface version 3 (adds `source_type` to each
    /// stream, which `start` below sets) -- not the current upstream version
    /// 6, since `restore_data`/`persist_mode`/`mapping_id`/`pipewire-serial`
    /// (versions 4-6) aren't implemented. Claiming a version this backend
    /// doesn't back would invite a frontend to rely on fields that never
    /// arrive.
    ///
    /// `name = "version"` matters: `org.freedesktop.impl.portal.ScreenCast.xml`
    /// declares this one property lowercase (unlike `AvailableCursorModes`/
    /// `AvailableSourceTypes`, which are PascalCase), and zbus's default
    /// PascalCase-ing would otherwise export it as `Version`. xdg-desktop-portal's
    /// frontend does a case-sensitive lookup for `version` to decide whether the
    /// backend is new enough to bother mirroring `AvailableCursorModes` at all --
    /// get the case wrong and that lookup silently reads 0, so it never binds
    /// `AvailableCursorModes` and every client's requested cursor mode reads back
    /// as unavailable, forever, with no error anywhere on TideWM's side to see.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        3
    }

    /// `handle` (the `org.freedesktop.impl.portal.Request` path the frontend
    /// reserved for this call) is intentionally unused.
    ///
    /// ponytail: v1 never shows a dialog, so every call here returns before
    /// a real client would ever have a reason to call `Close()` on a Request
    /// object at that path. Add one if/when `start` grows a real picker
    /// dialog with actual wall-clock time for a user to cancel during.
    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let owner = sender_of(&header)?;
        let entry = Arc::new(SessionEntry {
            owner,
            registered: AtomicBool::new(false),
            draw_cursor: Mutex::new(None),
            source_types: Mutex::new(None),
            stream: Mutex::new(PortalStreamState::Idle),
        });

        {
            let mut sessions = self.sessions.lock().unwrap();
            if !reserve_session(&mut sessions, session_handle.clone(), entry.clone()) {
                return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
            }
            // Reserve atomically before awaiting object registration. A
            // duplicate caller can no longer replace an existing session.
        }

        let session_object = SessionObject {
            path: session_handle.clone(),
            sessions: self.sessions.clone(),
            entry: entry.clone(),
        };
        if let Err(err) = emitter
            .connection()
            .object_server()
            .at(&session_handle, session_object)
            .await
        {
            let mut sessions = self.sessions.lock().unwrap();
            remove_session_if_same(&mut sessions, &session_handle, &entry);
            tracing::warn!(%err, "Failed to register portal screencast session object");
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        }
        entry.registered.store(true, Ordering::Release);

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let token = format!("tidewm{id}");
        let mut results = HashMap::new();
        // `session_handle_token` is load-bearing: modern xdg-desktop-portal
        // (1.17+) builds its own session from the backend's token and
        // *asserts* it is present (`xdp_session_initable_init`), crashing
        // the whole portal when a backend replies without it. The old
        // `session_id` key is kept for older portal versions.
        results.insert(
            "session_handle_token".to_string(),
            OwnedValue::try_from(Value::from(token.clone()))
                .expect("a String always converts to OwnedValue"),
        );
        results.insert(
            "session_id".to_string(),
            OwnedValue::try_from(Value::from(token))
                .expect("a String always converts to OwnedValue"),
        );
        tracing::debug!(%session_handle, "Portal screencast session created");
        Ok((RESPONSE_SUCCESS, results))
    }

    /// Honors the portal source-type bitmask and `cursor_mode`.
    /// `multiple`, `restore_data`, and `persist_mode` are read by no one --
    /// this backend never offers more than one stream and never persists,
    /// so there is nothing to do with them.
    async fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let Some(entry) = self.authorized_session(&session_handle, &header)? else {
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        };

        let types = options
            .get("types")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(SOURCE_TYPE_MONITOR)
            & SOURCE_TYPES;
        if types == 0 {
            tracing::warn!(
                types,
                "Portal screencast requested no supported source type"
            );
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        }

        let cursor_mode = options
            .get("cursor_mode")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(1);
        *entry.draw_cursor.lock().unwrap() = Some(cursor_mode & 2 != 0);
        *entry.source_types.lock().unwrap() = Some(types);

        Ok((RESPONSE_SUCCESS, HashMap::new()))
    }

    /// `parent_window` is unused because the picker is compositor-owned rather
    /// than a separate desktop window that needs transient parenting.
    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let Some(entry) = self.authorized_session(&session_handle, &header)? else {
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        };

        let Some(draw_cursor) = *entry.draw_cursor.lock().unwrap() else {
            tracing::warn!(%session_handle, "Portal screencast Start called before SelectSources");
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        };
        let Some(source_types) = *entry.source_types.lock().unwrap() else {
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        };
        let Some(start_reservation) = StartReservation::begin(entry.clone()) else {
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        };

        let (choice_tx, choice_rx) = mpsc::sync_channel(1);
        if self
            .compositor
            .send(ScreencastEvent::PickSource {
                source_types,
                response: choice_tx,
            })
            .is_err()
        {
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        }
        let choice = blocking::unblock(move || choice_rx.recv())
            .await
            .ok()
            .flatten();
        let Some(choice) = choice else {
            // Portal response 1 is user cancellation.
            return Ok((1, HashMap::new()));
        };

        let source = choice.source;
        let compositor = self.compositor.clone();
        let (width, height) = (choice.width, choice.height);
        let started = blocking::unblock(move || {
            pipewire_thread::start(source, width, height, draw_cursor, compositor)
        })
        .await;

        let (handle, node_id) = match started {
            Ok(started) => started,
            Err(err) => {
                tracing::warn!(%err, "Portal screencast PipeWire startup failed");
                return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
            }
        };

        if let Err(stream) = start_reservation.complete(PortalStream { _handle: handle }) {
            // The session was closed while the picker or PipeWire startup was
            // in flight. Dropping this handle stops the late worker.
            drop(stream);
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        }

        let mut stream_props: HashMap<String, OwnedValue> = HashMap::new();
        stream_props.insert(
            "position".to_string(),
            OwnedValue::try_from(Value::from((0i32, 0i32)))
                .expect("tuple of two i32s always converts to OwnedValue"),
        );
        stream_props.insert(
            "size".to_string(),
            OwnedValue::try_from(Value::from((width as i32, height as i32)))
                .expect("tuple of two i32s always converts to OwnedValue"),
        );
        stream_props.insert(
            "source_type".to_string(),
            OwnedValue::from(choice.source_type),
        );
        let streams: Vec<(u32, HashMap<String, OwnedValue>)> = vec![(node_id, stream_props)];

        let mut results = HashMap::new();
        results.insert(
            "streams".to_string(),
            OwnedValue::try_from(Value::from(streams))
                .expect("Vec<(u32, HashMap<String, OwnedValue>)> always converts to OwnedValue"),
        );
        tracing::debug!(%session_handle, node_id, "Portal screencast stream started");
        Ok((RESPONSE_SUCCESS, results))
    }
}

impl Portal {
    fn authorized_session(
        &self,
        session_handle: &OwnedObjectPath,
        header: &Header<'_>,
    ) -> zbus::fdo::Result<Option<Arc<SessionEntry>>> {
        let owner = sender_of(header)?;
        let sessions = self.sessions.lock().unwrap();
        Ok(sessions.get(session_handle).and_then(|entry| {
            (entry.registered.load(Ordering::Acquire) && entry.owner == owner)
                .then(|| entry.clone())
        }))
    }
}

/// Registered at the frontend-supplied `session_handle` path for every
/// created session. `org.freedesktop.impl.portal.Session` (unlike
/// `org.freedesktop.impl.portal.ScreenCast` above) is shared by every portal
/// interface that has sessions, so this type intentionally doesn't know
/// anything ScreenCast-specific beyond how to tear one down.
struct SessionObject {
    path: OwnedObjectPath,
    sessions: Arc<Mutex<SessionMap>>,
    entry: Arc<SessionEntry>,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    /// Lowercase per `org.freedesktop.impl.portal.Session.xml` -- see the
    /// `Portal::version` doc comment above for why the case matters here.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let caller = sender_of(&header)?;
        if caller != self.entry.owner {
            return Err(zbus::fdo::Error::AccessDenied(
                "portal session belongs to another D-Bus client".into(),
            ));
        }

        // Mark closed before unlinking so an in-flight Start cannot publish a
        // worker after this call. Remove only this exact reservation; a stale
        // object can never tear down a newer entry at the same path.
        self.entry.close();
        let mut sessions = self.sessions.lock().unwrap();
        remove_session_if_same(&mut sessions, &self.path, &self.entry);
        drop(sessions);

        // Matches `dbus.rs`'s `Session::stop`: this method call holds the
        // object server's read lock on `self`, so removing the interface
        // from within the same call would deadlock. Deferring to a detached
        // task lets this call return first.
        let removal_connection = emitter.connection().clone();
        let path = self.path.clone();
        let task = emitter.connection().executor().spawn(
            async move {
                if let Err(err) = removal_connection
                    .object_server()
                    .remove::<SessionObject, _>(&path)
                    .await
                {
                    tracing::debug!(%path, %err, "Portal screencast session was already removed");
                }
            },
            "remove closed portal screencast session",
        );
        task.detach();
        Ok(())
    }
}

/// Spawned once alongside `dbus.rs`'s own Mutter-interface disconnect
/// watcher, on the same connection. A second, independent
/// `NameOwnerChanged` subscription is simpler and safer to reason about
/// than threading this module's session map through the existing watcher's
/// private state, at the cost of one extra lightweight signal subscription.
pub(super) async fn watch_disconnects(
    connection: &zbus::Connection,
    sessions: Arc<Mutex<SessionMap>>,
) -> zbus::Result<()> {
    let proxy = zbus::fdo::DBusProxy::new(connection).await?;
    let mut stream = proxy
        .receive_name_owner_changed_with_args(&[(2, UniqueName::null_value())])
        .await?;

    while let Some(signal) = stream.next().await {
        let args = signal.args()?;
        let Some(name) = &**args.old_owner() else {
            continue;
        };
        let owner = OwnedUniqueName::from(name.to_owned());
        let stale: Vec<(OwnedObjectPath, Arc<SessionEntry>)> = {
            let mut sessions = sessions.lock().unwrap();
            let stale: Vec<_> = sessions
                .iter()
                .filter(|(_, entry)| entry.owner == owner)
                .map(|(path, entry)| (path.clone(), entry.clone()))
                .collect();
            for (path, _) in &stale {
                sessions.remove(path);
            }
            stale
        };
        for (path, entry) in stale {
            entry.close();
            let _ = connection
                .object_server()
                .remove::<SessionObject, _>(&path)
                .await;
            tracing::debug!(%path, "Removed abandoned portal screencast session");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_entry() -> Arc<SessionEntry> {
        Arc::new(SessionEntry {
            owner: OwnedUniqueName::try_from(":1.42").unwrap(),
            registered: AtomicBool::new(true),
            draw_cursor: Mutex::new(Some(false)),
            source_types: Mutex::new(Some(SOURCE_TYPE_MONITOR)),
            stream: Mutex::new(PortalStreamState::Idle),
        })
    }

    #[test]
    fn start_reservation_is_exclusive_and_rolls_back() {
        let entry = session_entry();
        let first = StartReservation::begin(entry.clone()).expect("first start reserves");
        assert!(StartReservation::begin(entry.clone()).is_none());

        drop(first);
        assert!(StartReservation::begin(entry).is_some());
    }

    #[test]
    fn close_prevents_late_or_new_start() {
        let entry = session_entry();
        let in_flight = StartReservation::begin(entry.clone()).expect("start reserves");
        entry.close();
        drop(in_flight);

        assert!(StartReservation::begin(entry).is_none());
    }

    #[test]
    fn duplicate_path_reservation_does_not_replace_original() {
        let path = OwnedObjectPath::try_from("/org/freedesktop/portal/session/test").unwrap();
        let original = session_entry();
        let duplicate = session_entry();
        let mut sessions = SessionMap::new();
        assert!(reserve_session(
            &mut sessions,
            path.clone(),
            original.clone()
        ));
        let accepted = reserve_session(&mut sessions, path.clone(), duplicate.clone());

        assert!(!accepted);
        assert!(!remove_session_if_same(&mut sessions, &path, &duplicate));
        assert!(Arc::ptr_eq(sessions.get(&path).unwrap(), &original));
    }
}
