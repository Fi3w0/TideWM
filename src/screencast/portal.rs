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
//! **v1 scope, deliberately thin:** MONITOR source type only, one stream per
//! session, no source picker -- `Start` auto-selects the first available
//! output instead of asking the user which one (see the `ponytail:` marker
//! on `Portal::start` for the upgrade path). This proves the whole
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::interface;
use zbus::message::Header;
use zbus::names::{OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{NoneValue, OwnedObjectPath, OwnedValue, Value};

use super::{pipewire_thread, OutputSnapshot, ScreencastEvent, ScreencastSource};

const MAX_PORTAL_SESSIONS: usize = 8;

/// `AvailableSourceTypes` bit for monitors. `WINDOW` (2) and `VIRTUAL` (4)
/// are not implemented in v1.
const SOURCE_TYPE_MONITOR: u32 = 1;
/// `AvailableCursorModes` bits this backend can actually satisfy: `Hidden`
/// (1) and `Embedded` (2). `Metadata` (4) is not implemented.
const CURSOR_MODES: u32 = 1 | 2;

pub(super) struct SessionEntry {
    owner: OwnedUniqueName,
    /// `None` until `SelectSources` runs; `Start` requires it.
    draw_cursor: Mutex<Option<bool>>,
    stream: Mutex<Option<PortalStream>>,
}

struct PortalStream {
    _handle: pipewire_thread::StreamHandle,
}

pub(super) type SessionMap = HashMap<OwnedObjectPath, Arc<SessionEntry>>;

pub(super) struct Portal {
    outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
    compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
    next_id: Arc<AtomicU64>,
    sessions: Arc<Mutex<SessionMap>>,
}

impl Portal {
    pub(super) fn new(
        outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
        compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
        next_id: Arc<AtomicU64>,
    ) -> (Self, Arc<Mutex<SessionMap>>) {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        (
            Self {
                outputs,
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
        SOURCE_TYPE_MONITOR
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
    #[zbus(property)]
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

        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.len() >= MAX_PORTAL_SESSIONS {
                return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
            }
            sessions.insert(
                session_handle.clone(),
                Arc::new(SessionEntry {
                    owner,
                    draw_cursor: Mutex::new(None),
                    stream: Mutex::new(None),
                }),
            );
        }

        let session_object = SessionObject {
            path: session_handle.clone(),
            sessions: self.sessions.clone(),
        };
        if let Err(err) = emitter
            .connection()
            .object_server()
            .at(&session_handle, session_object)
            .await
        {
            self.sessions.lock().unwrap().remove(&session_handle);
            tracing::warn!(%err, "Failed to register portal screencast session object");
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut results = HashMap::new();
        results.insert(
            "session_id".to_string(),
            OwnedValue::try_from(Value::from(format!("tidewm{id}")))
                .expect("a String always converts to OwnedValue"),
        );
        tracing::debug!(%session_handle, "Portal screencast session created");
        Ok((RESPONSE_SUCCESS, results))
    }

    /// v1 only honors `types` (must include `MONITOR`) and `cursor_mode`.
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
            .unwrap_or(SOURCE_TYPE_MONITOR);
        if types & SOURCE_TYPE_MONITOR == 0 {
            tracing::warn!(
                types,
                "Portal screencast requested a source type this v1 backend doesn't support (only MONITOR)"
            );
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        }

        let cursor_mode = options
            .get("cursor_mode")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(1);
        *entry.draw_cursor.lock().unwrap() = Some(cursor_mode & 2 != 0);

        Ok((RESPONSE_SUCCESS, HashMap::new()))
    }

    /// `parent_window` is intentionally unused (only meaningful for parenting
    /// a real picker dialog, which v1 doesn't show).
    ///
    /// ponytail: no source picker. Auto-selects the first output in the
    /// live snapshot instead of asking the user which one to share -- the
    /// same shortcut a single-monitor setup would never notice. Upgrade path:
    /// a first-party picker overlay (matching the `overview.rs` pattern) that
    /// lets `Start` pick a specific connector, the way `hyprland-share-picker`
    /// / a `wlr` chooser-script do for their respective backends.
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
        if entry.stream.lock().unwrap().is_some() {
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        }

        let Some(output) = self.outputs.lock().unwrap().first().cloned() else {
            tracing::warn!("Portal screencast Start with no outputs available");
            return Ok((RESPONSE_OTHER_ERROR, HashMap::new()));
        };

        let source = ScreencastSource::Output(output.name.clone());
        let compositor = self.compositor.clone();
        let (width, height) = (output.width, output.height);
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

        *entry.stream.lock().unwrap() = Some(PortalStream { _handle: handle });

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
            OwnedValue::from(SOURCE_TYPE_MONITOR),
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
            (entry.owner == owner).then(|| entry.clone())
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
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }

    async fn close(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        // Dropping the removed entry drops its `PortalStream`, whose
        // `StreamHandle` stops and joins the PipeWire worker thread.
        self.sessions.lock().unwrap().remove(&self.path);

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
        let stale: Vec<OwnedObjectPath> = {
            let mut sessions = sessions.lock().unwrap();
            let stale: Vec<_> = sessions
                .iter()
                .filter(|(_, entry)| entry.owner == owner)
                .map(|(path, _)| path.clone())
                .collect();
            for path in &stale {
                sessions.remove(path);
            }
            stale
        };
        for path in stale {
            let _ = connection
                .object_server()
                .remove::<SessionObject, _>(&path)
                .await;
            tracing::debug!(%path, "Removed abandoned portal screencast session");
        }
    }
    Ok(())
}
