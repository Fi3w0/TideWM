//! `org.gnome.Mutter.ScreenCast` + `.Session` + `.Stream` on the session
//! DBus, via `zbus::interface`. Matches niri's choice of the Mutter
//! interface over `xdg-desktop-portal-wlr`'s own (see AGENT.md's
//! "Screencasting" section): `xdg-desktop-portal-gnome` already knows how
//! to bridge this interface to the standard portal, so any user with it
//! installed gets a working screencast without TideWM implementing the
//! portal's own permission-dialog session dance.
//!
//! Monitor and window lifecycles (`CreateSession` -> `RecordMonitor` or
//! `RecordWindow` -> `Start` -> PipeWire node -> `Stop`) are implemented.
//! `RecordArea` and `RecordVirtual` are not declared, so callers receive
//! DBus `UnknownMethod` for those unsupported source types.
//!
//! **Verification honesty note:** the method and property shapes have been
//! cross-checked against Mutter's upstream implementation. The service has
//! been tested with direct zbus calls, but not yet through a real
//! portal-mediated client (OBS/Firefox) on a DRM session.
//! See CHANGELOG for the exact verification breakdown.
//!
//! Runs on its own OS thread via `zbus::blocking`, not calloop: zbus's
//! blocking connection dispatches incoming method calls on its own
//! internal executor for the life of the `Connection`, so this thread's
//! only job is to build the connection, register the interfaces, and
//! then block forever keeping it alive. It never touches `Smallvil`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::interface;
use zbus::message::Header;
use zbus::names::{BusName, OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{NoneValue, OwnedObjectPath, OwnedValue, Value};

use super::{pipewire_thread, OutputSnapshot, ScreencastEvent, ScreencastSource, WindowSnapshot};

const MAX_SESSIONS: usize = 32;
const MAX_SESSIONS_PER_CLIENT: usize = 8;
const MAX_STREAMS_PER_SESSION: usize = 8;

#[derive(Default)]
struct SessionRegistry {
    by_owner: HashMap<OwnedUniqueName, HashSet<OwnedObjectPath>>,
}

impl SessionRegistry {
    fn reserve(&mut self, owner: OwnedUniqueName, path: OwnedObjectPath) -> bool {
        let total = self.by_owner.values().map(HashSet::len).sum::<usize>();
        let owner_sessions = self.by_owner.get(&owner).map_or(0, HashSet::len);
        if total >= MAX_SESSIONS || owner_sessions >= MAX_SESSIONS_PER_CLIENT {
            return false;
        }
        self.by_owner.entry(owner).or_default().insert(path);
        true
    }

    fn remove(&mut self, owner: &OwnedUniqueName, path: &OwnedObjectPath) {
        let remove_owner = self.by_owner.get_mut(owner).is_some_and(|paths| {
            paths.remove(path);
            paths.is_empty()
        });
        if remove_owner {
            self.by_owner.remove(owner);
        }
    }

    fn take_owner(&mut self, owner: &OwnedUniqueName) -> Vec<OwnedObjectPath> {
        self.by_owner
            .remove(owner)
            .map(|paths| paths.into_iter().collect())
            .unwrap_or_default()
    }
}

/// Spawns the DBus service thread. Fire-and-forget: a failure here (bus
/// unreachable, name already taken) is logged and leaves screencasting
/// unavailable, not a reason to fail compositor startup over an
/// optional, default-off feature.
pub(super) fn spawn(
    outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
    windows: Arc<Mutex<Vec<WindowSnapshot>>>,
    compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
) {
    if let Err(err) = std::thread::Builder::new()
        .name("screencast-dbus".into())
        .spawn(move || run(outputs, windows, compositor))
    {
        tracing::warn!(%err, "Failed to spawn screencast DBus service thread");
    }
}

fn run(
    outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
    windows: Arc<Mutex<Vec<WindowSnapshot>>>,
    compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
) {
    let sessions = Arc::new(Mutex::new(SessionRegistry::default()));
    let next_id = Arc::new(AtomicU64::new(1));
    let root = Mutter {
        outputs: outputs.clone(),
        windows,
        compositor: compositor.clone(),
        next_id: next_id.clone(),
        sessions: sessions.clone(),
    };
    let (portal_root, portal_sessions) = super::portal::Portal::new(outputs, compositor, next_id);

    let connection = match zbus::blocking::connection::Builder::session()
        .and_then(|builder| builder.serve_at("/org/gnome/Mutter/ScreenCast", root))
        .and_then(|builder| builder.name("org.gnome.Mutter.ScreenCast"))
        .and_then(|builder| builder.serve_at("/org/freedesktop/portal/desktop", portal_root))
        .and_then(|builder| builder.build())
    {
        Ok(connection) => connection,
        Err(err) => {
            tracing::warn!(%err, "Screencast DBus service failed to start");
            return;
        }
    };
    tracing::info!("Screencast DBus service registered as org.gnome.Mutter.ScreenCast");

    let async_connection = connection.inner().clone();
    let watcher_connection = async_connection.clone();
    let task = async_connection.executor().spawn(
        async move {
            if let Err(err) = watch_disconnects(&watcher_connection, sessions).await {
                tracing::warn!(%err, "Screencast DBus disconnect watcher failed");
            }
        },
        "screencast disconnect watcher",
    );
    task.detach();

    // The portal backend name is requested separately (rather than via
    // `.name()` above) so a name collision here -- unlikely, but possible if
    // another compositor's backend is somehow already claiming it -- doesn't
    // take down the already-working Mutter interface too.
    //
    // Both case variants are claimed: xdg-desktop-portal derives the backend
    // name from `XDG_CURRENT_DESKTOP`, and display managers hand that out in
    // the session's exact spelling (`TideWM` here) while the conventional
    // portal naming is lowercase -- some xdp versions lowercase the desktop
    // name before looking up the backend, some use it verbatim. Claiming
    // both means the backend is reachable either way, at the cost of one
    // inert extra name.
    let portal_connection = async_connection.clone();
    let portal_task = async_connection.executor().spawn(
        async move {
            for name in [
                "org.freedesktop.impl.portal.desktop.tidewm",
                "org.freedesktop.impl.portal.desktop.TideWM",
            ] {
                match portal_connection.request_name(name).await {
                    Ok(()) => tracing::info!("Screencast portal backend registered as {name}"),
                    Err(err) => {
                        tracing::warn!(
                            %err,
                            name,
                            "Failed to claim the screencast portal backend name; Discord/OBS-style \
                             portal screen sharing will not reach TideWM (org.gnome.Mutter.ScreenCast \
                             is still available for xdg-desktop-portal-gnome setups)"
                        );
                    }
                }
            }
            if let Err(err) = super::portal::watch_disconnects(&portal_connection, portal_sessions).await {
                tracing::warn!(%err, "Screencast portal disconnect watcher failed");
            }
        },
        "screencast portal backend",
    );
    portal_task.detach();

    // Keeps `_connection` (and with it, the registered interfaces) alive
    // for the process lifetime. zbus dispatches incoming calls on its
    // own internal executor threads regardless of whether this thread is
    // parked; there is nothing left for this thread itself to do.
    loop {
        std::thread::park();
    }
}

/// One counter shared by every dynamically-created `Session`/`Stream`
/// object. A single ever-increasing counter also keeps paths unique while
/// asynchronous object-server cleanup from an earlier session completes.
struct Mutter {
    outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
    windows: Arc<Mutex<Vec<WindowSnapshot>>>,
    compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
    next_id: Arc<AtomicU64>,
    sessions: Arc<Mutex<SessionRegistry>>,
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
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let owner = sender_of(&header)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let path = OwnedObjectPath::try_from(format!("/org/gnome/Mutter/ScreenCast/Session/u{id}"))
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        if !self
            .sessions
            .lock()
            .unwrap()
            .reserve(owner.clone(), path.clone())
        {
            return Err(zbus::fdo::Error::LimitsExceeded(
                "too many active screencast sessions".into(),
            ));
        }
        let session = Session {
            outputs: self.outputs.clone(),
            windows: self.windows.clone(),
            compositor: self.compositor.clone(),
            next_id: self.next_id.clone(),
            owner: owner.clone(),
            path: path.clone(),
            registry: self.sessions.clone(),
            closed: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            stream_slots: AtomicUsize::new(0),
            streams: Mutex::new(Vec::new()),
        };
        if let Err(err) = emitter
            .connection()
            .object_server()
            .at(&path, session)
            .await
        {
            self.sessions.lock().unwrap().remove(&owner, &path);
            return Err(zbus::fdo::Error::Failed(err.to_string()));
        }
        let proxy = match zbus::fdo::DBusProxy::new(emitter.connection()).await {
            Ok(proxy) => proxy,
            Err(err) => {
                self.sessions.lock().unwrap().remove(&owner, &path);
                remove_abandoned_session(emitter.connection(), &path).await;
                return Err(zbus::fdo::Error::Failed(err.to_string()));
            }
        };
        let owner_alive = match proxy
            .name_has_owner(BusName::Unique(owner.clone().into()))
            .await
        {
            Ok(alive) => alive,
            Err(err) => {
                self.sessions.lock().unwrap().remove(&owner, &path);
                remove_abandoned_session(emitter.connection(), &path).await;
                return Err(zbus::fdo::Error::Failed(err.to_string()));
            }
        };
        if !owner_alive {
            self.sessions.lock().unwrap().remove(&owner, &path);
            remove_abandoned_session(emitter.connection(), &path).await;
            return Err(zbus::fdo::Error::Disconnected(
                "screencast client disconnected while creating session".into(),
            ));
        }
        tracing::debug!(%path, "Screencast session created");
        Ok(path)
    }
}

struct Session {
    outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
    windows: Arc<Mutex<Vec<WindowSnapshot>>>,
    compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
    next_id: Arc<AtomicU64>,
    owner: OwnedUniqueName,
    path: OwnedObjectPath,
    registry: Arc<Mutex<SessionRegistry>>,
    closed: AtomicBool,
    starting: AtomicBool,
    stream_slots: AtomicUsize,
    streams: Mutex<Vec<RecordedStream>>,
}

struct RecordedStream {
    path: OwnedObjectPath,
    source: ScreencastSource,
    width: u32,
    height: u32,
    cursor_mode: u32,
    pipewire: Option<pipewire_thread::StreamHandle>,
}

impl Session {
    fn ensure_owner(&self, header: &Header<'_>) -> zbus::fdo::Result<()> {
        if sender_of(header)? == self.owner {
            Ok(())
        } else {
            Err(zbus::fdo::Error::AccessDenied(
                "screencast session belongs to another DBus client".into(),
            ))
        }
    }

    fn close_and_take_streams(&self) -> Option<Vec<RecordedStream>> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(self.streams.lock().unwrap().drain(..).collect())
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    async fn record_monitor(
        &self,
        connector: String,
        properties: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.ensure_owner(&header)?;
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
        // Mutter defines 0 = hidden, 1 = embedded, 2 = metadata. TideWM's
        // SHM transport supports hidden and embedded; metadata requests fall
        // back to embedded so the viewer does not lose the pointer entirely.
        let requested_cursor_mode = properties
            .get("cursor-mode")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(1);
        let cursor_mode = if requested_cursor_mode == 0 { 0 } else { 1 };
        let path = OwnedObjectPath::try_from(format!("/org/gnome/Mutter/ScreenCast/Stream/u{id}"))
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        if self
            .stream_slots
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_STREAMS_PER_SESSION).then_some(count + 1)
            })
            .is_err()
        {
            return Err(zbus::fdo::Error::LimitsExceeded(
                "too many streams in screencast session".into(),
            ));
        }
        if let Err(err) = emitter
            .connection()
            .object_server()
            .at(
                &path,
                Stream {
                    width: width as i32,
                    height: height as i32,
                    cursor_mode,
                },
            )
            .await
        {
            self.stream_slots.fetch_sub(1, Ordering::AcqRel);
            return Err(zbus::fdo::Error::Failed(err.to_string()));
        }
        let mut streams = self.streams.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            drop(streams);
            self.stream_slots.fetch_sub(1, Ordering::AcqRel);
            let _ = emitter
                .connection()
                .object_server()
                .remove::<Stream, _>(&path)
                .await;
            return Err(zbus::fdo::Error::Failed("session is closed".into()));
        }
        streams.push(RecordedStream {
            path: path.clone(),
            source: ScreencastSource::Output(connector.clone()),
            width,
            height,
            cursor_mode,
            pipewire: None,
        });
        drop(streams);
        tracing::debug!(%path, connector, "Screencast monitor stream created");
        Ok(path)
    }

    /// Creates a stream for TideWM's numeric foreign-toplevel id. Mutter's
    /// API uses the same `window-id` u64 property; TideWM exposes this id in
    /// its `windows` IPC response so a portal/chooser can pass it back.
    async fn record_window(
        &self,
        properties: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.ensure_owner(&header)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(zbus::fdo::Error::Failed("session is closed".into()));
        }
        let window_id = properties
            .get("window-id")
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("missing window-id".into()))?;
        let (width, height) = self
            .windows
            .lock()
            .unwrap()
            .iter()
            .find(|window| window.id == window_id)
            .map(|window| (window.width, window.height))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown window-id".into()))?;
        let cursor_mode = properties
            .get("cursor-mode")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let path = OwnedObjectPath::try_from(format!("/org/gnome/Mutter/ScreenCast/Stream/u{id}"))
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        if self
            .stream_slots
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_STREAMS_PER_SESSION).then_some(count + 1)
            })
            .is_err()
        {
            return Err(zbus::fdo::Error::LimitsExceeded(
                "too many streams in screencast session".into(),
            ));
        }
        if let Err(err) = emitter
            .connection()
            .object_server()
            .at(
                &path,
                Stream {
                    width: width as i32,
                    height: height as i32,
                    cursor_mode,
                },
            )
            .await
        {
            self.stream_slots.fetch_sub(1, Ordering::AcqRel);
            return Err(zbus::fdo::Error::Failed(err.to_string()));
        }
        let mut streams = self.streams.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            drop(streams);
            self.stream_slots.fetch_sub(1, Ordering::AcqRel);
            let _ = emitter
                .connection()
                .object_server()
                .remove::<Stream, _>(&path)
                .await;
            return Err(zbus::fdo::Error::Failed("session is closed".into()));
        }
        streams.push(RecordedStream {
            path: path.clone(),
            source: ScreencastSource::Window(window_id),
            width,
            height,
            cursor_mode,
            pipewire: None,
        });
        tracing::debug!(%path, window_id, "Screencast window stream created");
        Ok(path)
    }

    async fn start(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.ensure_owner(&header)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(zbus::fdo::Error::Failed("session is closed".into()));
        }
        if self.starting.swap(true, Ordering::AcqRel) {
            return Err(zbus::fdo::Error::Failed(
                "screencast session is already starting".into(),
            ));
        }
        struct StartingGuard<'a>(&'a AtomicBool);
        impl Drop for StartingGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _starting = StartingGuard(&self.starting);

        let output_sizes: HashMap<String, (u32, u32)> = self
            .outputs
            .lock()
            .unwrap()
            .iter()
            .map(|output| (output.name.clone(), (output.width, output.height)))
            .collect();
        let window_sizes: HashMap<u64, (u32, u32)> = self
            .windows
            .lock()
            .unwrap()
            .iter()
            .map(|window| (window.id, (window.width, window.height)))
            .collect();
        let specifications = {
            let mut streams = self.streams.lock().unwrap();
            if streams.is_empty() {
                return Err(zbus::fdo::Error::Failed("session has no streams".into()));
            }
            for stream in streams.iter_mut() {
                if stream
                    .pipewire
                    .as_ref()
                    .is_some_and(|handle| !handle.is_alive())
                {
                    stream.pipewire.take();
                }
            }
            let mut specifications = Vec::new();
            for stream in streams
                .iter_mut()
                .filter(|stream| stream.pipewire.is_none())
            {
                let dimensions = match &stream.source {
                    ScreencastSource::Output(connector) => output_sizes.get(connector).copied(),
                    ScreencastSource::Window(id) => window_sizes.get(id).copied(),
                };
                let Some((width, height)) = dimensions else {
                    return Err(zbus::fdo::Error::Failed(
                        "capture source is no longer available".into(),
                    ));
                };
                stream.width = width;
                stream.height = height;
                specifications.push((
                    stream.path.clone(),
                    stream.source.clone(),
                    width,
                    height,
                    stream.cursor_mode != 0,
                ));
            }
            specifications
        };

        let compositor = self.compositor.clone();
        let started = blocking::unblock(move || {
            std::thread::scope(|scope| {
                let jobs: Vec<_> = specifications
                    .into_iter()
                    .map(|(path, source, width, height, draw_cursor)| {
                        let compositor = compositor.clone();
                        scope.spawn(move || {
                            pipewire_thread::start(source, width, height, draw_cursor, compositor)
                                .map(|(handle, node_id)| (path, handle, node_id))
                        })
                    })
                    .collect();
                let mut additions = Vec::with_capacity(jobs.len());
                for job in jobs {
                    match job.join() {
                        Ok(Ok(addition)) => additions.push(addition),
                        Ok(Err(err)) => return Err(err),
                        Err(_) => return Err("PipeWire startup worker panicked".into()),
                    }
                }
                Ok::<_, String>(additions)
            })
        })
        .await
        .map_err(zbus::fdo::Error::Failed)?;

        if self.closed.load(Ordering::Acquire) {
            drop(started);
            return Err(zbus::fdo::Error::Failed(
                "session was stopped while starting".into(),
            ));
        }

        let additions = {
            let mut streams = self.streams.lock().unwrap();
            let mut signals = Vec::new();
            for (path, handle, node_id) in started {
                let Some(stream) = streams.iter_mut().find(|stream| stream.path == path) else {
                    continue;
                };
                stream.pipewire = Some(handle);
                signals.push((path, node_id));
            }
            signals
        };

        for (path, node_id) in additions {
            let stream_emitter = SignalEmitter::new(emitter.connection(), path)
                .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
            Stream::pipe_wire_stream_added(&stream_emitter, node_id)
                .await
                .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        }
        Ok(())
    }

    async fn stop(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.ensure_owner(&header)?;
        let Some(streams) = self.close_and_take_streams() else {
            return Ok(());
        };
        let paths: Vec<_> = streams.into_iter().map(|stream| stream.path).collect();
        let mut cleanup_errors = Vec::new();
        for path in paths {
            match SignalEmitter::new(emitter.connection(), path.clone()) {
                Ok(stream_emitter) => {
                    if let Err(err) = Stream::closed(&stream_emitter).await {
                        cleanup_errors.push(format!("could not signal {path} closed: {err}"));
                    }
                }
                Err(err) => cleanup_errors.push(format!("invalid stream path {path}: {err}")),
            }
            if let Err(err) = emitter
                .connection()
                .object_server()
                .remove::<Stream, _>(&path)
                .await
            {
                cleanup_errors.push(format!("could not remove {path}: {err}"));
            }
        }
        if let Err(err) = Self::closed(&emitter).await {
            cleanup_errors.push(format!("could not signal session closed: {err}"));
        }

        self.registry
            .lock()
            .unwrap()
            .remove(&self.owner, &self.path);
        let connection = emitter.connection().clone();
        let removal_connection = connection.clone();
        let path = self.path.clone();
        let task = connection.executor().spawn(
            async move {
                // The current method owns the Session interface's read lock.
                // Removal waits for this method to return, then drops the
                // otherwise-permanent object-server entry.
                if let Err(err) = removal_connection
                    .object_server()
                    .remove::<Session, _>(&path)
                    .await
                {
                    tracing::debug!(%path, %err, "Screencast session was already removed");
                }
            },
            "remove stopped screencast session",
        );
        task.detach();
        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(cleanup_errors.join("; ")))
        }
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

struct Stream {
    width: i32,
    height: i32,
    cursor_mode: u32,
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
        params.insert(
            "cursor-mode".to_string(),
            OwnedValue::from(self.cursor_mode),
        );
        params
    }

    #[zbus(signal)]
    async fn pipe_wire_stream_added(emitter: &SignalEmitter<'_>, node_id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

fn sender_of(header: &Header<'_>) -> zbus::fdo::Result<OwnedUniqueName> {
    header
        .sender()
        .map(|name| OwnedUniqueName::from(name.to_owned()))
        .ok_or_else(|| zbus::fdo::Error::Failed("no sender on screencast DBus call".into()))
}

async fn watch_disconnects(
    connection: &zbus::Connection,
    sessions: Arc<Mutex<SessionRegistry>>,
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
        let paths = sessions.lock().unwrap().take_owner(&owner);
        for path in paths {
            remove_abandoned_session(connection, &path).await;
        }
    }
    Ok(())
}

async fn remove_abandoned_session(connection: &zbus::Connection, path: &OwnedObjectPath) {
    let object_server = connection.object_server();
    let Ok(interface) = object_server.interface::<_, Session>(path).await else {
        return;
    };
    let streams = {
        let session = interface.get().await;
        session.close_and_take_streams()
    };
    if let Some(streams) = streams {
        for stream in streams {
            let _ = object_server.remove::<Stream, _>(&stream.path).await;
        }
    }
    let _ = object_server.remove::<Session, _>(path).await;
    tracing::debug!(%path, "Removed abandoned screencast session");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_registry_enforces_global_and_per_client_limits_and_prunes() {
        let mut registry = SessionRegistry::default();
        let owner = OwnedUniqueName::try_from(":1.10").unwrap();
        for id in 0..MAX_SESSIONS_PER_CLIENT {
            let path = OwnedObjectPath::try_from(format!("/test/{id}")).unwrap();
            assert!(registry.reserve(owner.clone(), path));
        }
        assert!(!registry.reserve(
            owner.clone(),
            OwnedObjectPath::try_from("/test/overflow").unwrap(),
        ));

        let removed = registry.take_owner(&owner);
        assert_eq!(removed.len(), MAX_SESSIONS_PER_CLIENT);
        assert!(registry.reserve(owner, OwnedObjectPath::try_from("/test/reused").unwrap(),));

        let mut registry = SessionRegistry::default();
        for id in 0..MAX_SESSIONS {
            let owner = OwnedUniqueName::try_from(format!(":1.{}", id + 20)).unwrap();
            let path = OwnedObjectPath::try_from(format!("/global/{id}")).unwrap();
            assert!(registry.reserve(owner, path));
        }
        assert!(!registry.reserve(
            OwnedUniqueName::try_from(":1.999").unwrap(),
            OwnedObjectPath::try_from("/global/overflow").unwrap(),
        ));
    }
}
