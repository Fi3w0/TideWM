//! TideWM's JSON-over-Unix-socket control interface. Two modes share one socket:
//!
//! * **Request-response** (Phase A item 3 of the WM feature-parity roadmap):
//!   one connection, one request line in, one response line out, then the
//!   server closes it.
//! * **Subscribe** (Phase R3 of the render/visual-identity roadmap, see
//!   AGENT.md): the connection stays open and the server pushes JSON lines
//!   as the desktop changes -- so a reactive widget (a waybar module, an
//!   eww `deflisten`, a QuickShell socket reader) doesn't have to poll the
//!   one-shot queries. Clean documented JSON, deliberately not a mimicry
//!   of Hyprland's `socket2` event-string wire shape: there's no
//!   compatibility reason to match that specific format.
//!
//! Read queries: `{"request": "outputs"}`, `{"request": "workspaces"}`,
//! `{"request": "windows"}`, `{"request": "focused-window"}`,
//! `{"request": "active-submap"}` (the currently active `submap <name> { }`
//! block, if any -- `null` when the base binds are in effect).
//! Actions: `{"request": "action", "action": "<string>"}`, where the string
//! is the *exact* same syntax a `bind` statement's action half uses in
//! config.wave (e.g. `"workspace:3"`, `"close-window"`, `"spawn:kitty"`) --
//! routed through
//! `config::parse_action`/`Smallvil::run_action` directly, so every action
//! a keybind can trigger is IPC-addressable for free, including ones added
//! by later phases, with zero new dispatch code here.
//!
//! Subscribe: `{"request": "subscribe", "events": ["window", "workspace",
//! "focus", "urgent", "depth", "config"]}`. Omitting `events` (or sending
//! an empty array) subscribes to all of them. The server replies with one
//! ack line, then keeps the connection open and writes one JSON line per
//! matching event: `{"event": "<kind>", "data": ...}`. The connection's
//! lifetime is the subscription's lifetime -- closing it unsubscribes.
//!
//! Every request-response reply is `{"ok": true, "data": ...}` or
//! `{"ok": false, "error": "..."}` -- malformed JSON or an unrecognized
//! action string gets an error response, not a dropped connection, so a
//! scripting mistake is visible to whatever's on the other end of the
//! socket.

use std::{
    cell::Cell,
    collections::{HashSet, VecDeque},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use serde::Deserialize;
use serde_json::json;
use smithay::reexports::{
    calloop::{
        generic::Generic,
        timer::{TimeoutAction, Timer},
        EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
    },
    wayland_server::protocol::wl_surface::WlSurface,
};

use crate::{config, state::Smallvil};

/// A client that never completes a request line within this many bytes
/// gets disconnected rather than growing the per-connection buffer
/// unbounded -- there's no legitimate request anywhere near this size.
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

struct ConnectionLease(Arc<AtomicUsize>);

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Deserialize, Debug)]
#[serde(tag = "request", rename_all = "kebab-case")]
enum Request {
    Outputs,
    Workspaces,
    Windows,
    FocusedWindow,
    ActiveSubmap,
    /// Compositor-side facts for `tidectl doctor`/`report`: version, build
    /// profile, commit, backend, uptime, engine, config path and warnings,
    /// and counts that a bug report wants without guessing from the host.
    Diagnostics,
    Action {
        action: String,
    },
    Batch {
        actions: Vec<String>,
    },
    /// Evaluate a Wave expression on the compositor's session Lua and
    /// return the value as JSON: `{"request": "eval", "expression":
    /// "theme.primary"}`. Config globals and section tables are visible,
    /// and the `tide` table is refreshed before evaluation.
    Eval {
        expression: String,
    },
    /// Long-lived subscribe mode -- see the module docs. `events: None`
    /// means "all event kinds"; an explicit list filters server-side so a
    /// bar that only cares about workspace changes doesn't pay for
    /// per-window snapshot serialization at all.
    Subscribe {
        events: Option<Vec<EventKind>>,
    },
}

const MAX_BATCH_ACTIONS: usize = 128;

/// Subscriber-selected event channel. Matches the `events` array of a
/// `subscribe` request: a subscriber with `filter.is_empty()` receives
/// every event kind, otherwise only those listed.
#[derive(Deserialize, Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EventKind {
    /// `window-opened`, `window-closed`, `window-changed`.
    Window,
    /// `workspace-changed`.
    Workspace,
    /// `focus-changed`.
    Focus,
    /// `urgent-changed`.
    Urgent,
    /// `depth-changed`.
    Depth,
    /// `config-reloaded`.
    Config,
}

impl EventKind {
    const ALL: [EventKind; 6] = [
        EventKind::Window,
        EventKind::Workspace,
        EventKind::Focus,
        EventKind::Urgent,
        EventKind::Depth,
        EventKind::Config,
    ];

    /// Lowercase kebab string used in the ack and in the `events` array of
    /// a subscribe request. Kept in one place so the ack in
    /// `register_subscriber` and the test's idea of the wire format can't
    /// drift apart.
    fn wire_name(self) -> &'static str {
        match self {
            EventKind::Window => "window",
            EventKind::Workspace => "workspace",
            EventKind::Focus => "focus",
            EventKind::Urgent => "urgent",
            EventKind::Depth => "depth",
            EventKind::Config => "config",
        }
    }
}

/// Subscriber-side delivery decision: an empty filter means "subscribe to
/// everything", otherwise the event's top-level kind must be in the set.
/// Extracted to a free function so the filter rule has a unit test without
/// having to spin up a Smallvil.
pub(crate) fn event_matches(filter: &HashSet<EventKind>, event: &IpcEvent) -> bool {
    filter.is_empty() || filter.contains(&event.kind())
}

/// One state change broadcast to matching subscribers. Constructed at the
/// call site where the change actually happens (`state.rs`/`xdg_shell.rs`)
/// and handed to `Smallvil::emit_ipc_event`, which snapshots the relevant
/// window/workspace data and writes a JSON line per matching subscriber.
///
/// The `WlSurface`-carrying variants are intentionally surface-driven
/// rather than pre-snapshotting the JSON at the call site: the snapshot
/// helpers in this file need `&Smallvil` (to look up the `Window` from the
/// surface, plus focused/pinned/etc. flags), and the call site already
/// holds `&mut Smallvil`. Carrying the surface into `emit_ipc_event` and
/// letting it do the single shared-borrow snapshot avoids a clumsy
/// split borrow at every call site.
pub(crate) enum IpcEvent {
    WindowOpened {
        surface: WlSurface,
    },
    /// Identity captured at the call site *before* the per-window tracking
    /// maps are drained -- by the time the close event fires, the window
    /// has already been detached from `space` and its foreign-toplevel /
    /// identity entries have been cleaned up, so the snapshot helpers
    /// can't read them after the fact.
    WindowClosed {
        window_id: Option<u64>,
        app_id: Option<String>,
        title: Option<String>,
    },
    WindowChanged {
        surface: WlSurface,
    },
    WorkspaceChanged {
        output: String,
        from: u32,
        to: u32,
    },
    FocusChanged {
        surface: Option<WlSurface>,
    },
    UrgentChanged {
        surface: WlSurface,
        urgent: bool,
    },
    DepthChanged {
        surface: WlSurface,
        tier: u8,
    },
    ConfigReloaded,
}

impl IpcEvent {
    /// Top-level channel name (`window`, `workspace`, ...) used to filter
    /// against a subscriber's `EventKind` set. The fine-grained event
    /// string (`window-opened` etc.) is chosen later in `event_name`.
    pub(crate) fn kind(&self) -> EventKind {
        match self {
            IpcEvent::WindowOpened { .. }
            | IpcEvent::WindowClosed { .. }
            | IpcEvent::WindowChanged { .. } => EventKind::Window,
            IpcEvent::WorkspaceChanged { .. } => EventKind::Workspace,
            IpcEvent::FocusChanged { .. } => EventKind::Focus,
            IpcEvent::UrgentChanged { .. } => EventKind::Urgent,
            IpcEvent::DepthChanged { .. } => EventKind::Depth,
            IpcEvent::ConfigReloaded => EventKind::Config,
        }
    }

    /// Fine-grained event string for the JSON `event` field. One per
    /// variant so a widget can route on the specific change without
    /// re-deriving it from the payload shape.
    fn event_name(&self) -> &'static str {
        match self {
            IpcEvent::WindowOpened { .. } => "window-opened",
            IpcEvent::WindowClosed { .. } => "window-closed",
            IpcEvent::WindowChanged { .. } => "window-changed",
            IpcEvent::WorkspaceChanged { .. } => "workspace-changed",
            IpcEvent::FocusChanged { .. } => "focus-changed",
            IpcEvent::UrgentChanged { .. } => "urgent-changed",
            IpcEvent::DepthChanged { .. } => "depth-changed",
            IpcEvent::ConfigReloaded => "config-reloaded",
        }
    }

    /// Build the complete JSON line for this event (including the trailing
    /// newline) using `state` for any window/workspace snapshotting.
    pub(crate) fn to_json_line(&self, state: &Smallvil) -> Vec<u8> {
        let data: serde_json::Value = match self {
            IpcEvent::WindowOpened { surface } | IpcEvent::WindowChanged { surface } => {
                snapshot_window(state, surface).unwrap_or(serde_json::Value::Null)
            }
            IpcEvent::WindowClosed {
                window_id,
                app_id,
                title,
            } => json!({
                "window_id": window_id,
                "app_id": app_id,
                "title": title,
            }),
            IpcEvent::WorkspaceChanged { output, from, to } => json!({
                "output": output,
                "from": from,
                "to": to,
            }),
            IpcEvent::FocusChanged { surface } => match surface {
                Some(surface) => snapshot_window(state, surface).unwrap_or(serde_json::Value::Null),
                None => serde_json::Value::Null,
            },
            IpcEvent::UrgentChanged { surface, urgent } => {
                let mut snap = snapshot_window(state, surface).unwrap_or_else(|| {
                    // Window already unmapped by the time the urgent-clear
                    // fires (closed without losing focus first) -- emit a
                    // minimal identity so the widget can still match on
                    // window_id if it has one in flight.
                    json!({ "window_id": state.foreign_toplevel_numeric_ids.get(surface).copied() })
                });
                if let serde_json::Value::Object(ref mut map) = snap {
                    map.insert("urgent".into(), json!(urgent));
                }
                snap
            }
            IpcEvent::DepthChanged { surface, tier } => {
                let mut snap = snapshot_window(state, surface).unwrap_or_else(|| {
                    json!({ "window_id": state.foreign_toplevel_numeric_ids.get(surface).copied() })
                });
                if let serde_json::Value::Object(ref mut map) = snap {
                    map.insert("tier".into(), json!(tier));
                }
                snap
            }
            IpcEvent::ConfigReloaded => serde_json::Value::Null,
        };
        let mut line = serde_json::to_vec(&json!({ "event": self.event_name(), "data": data }))
            .unwrap_or_else(|_| b"{\"event\":\"error\",\"data\":null}\n".to_vec());
        line.push(b'\n');
        line
    }
}

/// Window snapshot shared by `WindowOpened`/`WindowChanged`/`FocusChanged`/
/// `UrgentChanged`/`DepthChanged`. Returns `None` if the surface is no
/// longer mapped (race: closed between the emit call and the snapshot) --
/// callers each have a sensible fallback for that case.
fn snapshot_window(state: &Smallvil, surface: &WlSurface) -> Option<serde_json::Value> {
    let focused = state.focused_window_surface();
    let window = state
        .space
        .elements()
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))?;
    window_json(state, window, focused.as_ref())
}

/// A long-lived subscribe connection. One per `subscribe` request, stored
/// in `Smallvil::ipc_subscribers` keyed by a process-unique `usize` id.
pub(crate) struct IpcSubscriber {
    /// Write handle. Distinct FD from `read_source`'s owned stream (both
    /// are `dup()`s of the original socket), so a write here doesn't
    /// disturb calloop's read-side readiness tracking.
    pub(crate) stream: UnixStream,
    /// Empty means "subscribe to all". Otherwise only `EventKind`s in
    /// this set are delivered, filtered server-side.
    pub(crate) filter: HashSet<EventKind>,
    /// Bytes queued but not yet accepted by the kernel (write returned
    /// short or `WouldBlock`). Drained opportunistically by both
    /// `emit_ipc_event` (inline attempt immediately after queueing) and
    /// the periodic `flush_ipc_subscribers` timer.
    pub(crate) pending: VecDeque<u8>,
    /// The calloop source token for the read-side EOF watcher. Stored so
    /// `remove_ipc_subscriber` (which can fire from `emit_ipc_event`'s
    /// slow-subscriber path, not from the read callback itself) can
    /// retire the source via `loop_handle.remove`. `None` only between
    /// insert and token-patch in `register_subscriber`.
    pub(crate) read_token: Option<RegistrationToken>,
}

/// Per-subscriber cap on accumulated unwritten bytes. A subscriber that
/// can't keep up with this stream of small JSON lines (no `read()` in
/// roughly the time it takes to fill a quarter-meg of events) is
/// unconditionally dropped -- bounded memory matters more than best-effort
/// delivery to a wedged client, see AGENT.md's RAM constraint.
pub(crate) const SUBSCRIBER_PENDING_CAP: usize = 256 * 1024;

/// Periodic flush interval. Matches the frame budget (60Hz) so a bar
/// widget reacting to a workspace switch lands its update in the same
/// frame the user sees the new workspace. Fast subscribers (the common
/// case) are already drained inline by `emit_ipc_event` -- this timer is
/// the retry path for subscribers whose kernel write buffer was full at
/// emit time.
const FLUSH_INTERVAL: Duration = Duration::from_millis(16);

impl IpcSubscriber {
    /// Non-blocking write of as much of `pending` as the kernel will take.
    /// Returns `true` if the connection looks healthy afterwards (either
    /// fully drained, or partial progress without error), `false` if the
    /// peer has closed -- in which case the caller should retire the
    /// subscriber entirely.
    pub(crate) fn try_flush(&mut self) -> bool {
        while !self.pending.is_empty() {
            match self.stream.write(self.pending.as_slices().0) {
                Ok(0) => return false,
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return true,
                // EPIPE / ECONNRESET / etc. -- peer is gone.
                Err(_) => return false,
            }
        }
        true
    }
}

/// Unlinks its socket file on drop. Keep the returned guard alive for the
/// process lifetime; dropping it early tears down the control interface
/// while TideWM keeps running.
pub struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Binds `$XDG_RUNTIME_DIR/tidewm-<pid>.sock` -- PID-scoped so a nested dev
/// session and a real hardware session (both genuinely happen at once, see
/// the git history on this file) never collide on the same socket path --
/// and registers it with the event loop. Exports `TIDEWM_SOCKET` so
/// spawned children (bar scripts, a future CLI) can find it without
/// hardcoding the path.
pub fn init(event_loop: &mut EventLoop<Smallvil>) -> std::io::Result<SocketGuard> {
    let loop_handle = event_loop.handle();
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let path = runtime_dir.join(format!("tidewm-{}.sock", std::process::id()));
    // Defensive only: the PID-scoped path means a stale file here would
    // require a PID to wrap around and land on a process that also crashed
    // without cleanup -- essentially never, but removing first is free.
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path)?;
    if let Err(err) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&path);
        return Err(err);
    }
    listener.set_nonblocking(true)?;

    let accept_handle = loop_handle.clone();
    let active_connections = Arc::new(AtomicUsize::new(0));
    let accept_connections = active_connections.clone();
    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_readiness, listener, _state: &mut Smallvil| {
                // Level-triggered: a second pending connection left
                // unaccepted here just fires again next iteration, so a
                // single accept() per wakeup is enough, no drain loop.
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        if accept_connections.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                            tracing::warn!(limit = MAX_CONNECTIONS, "IPC connection limit reached; dropping client");
                            return Ok(PostAction::Continue);
                        }
                        if let Err(err) = stream.set_nonblocking(true) {
                            tracing::warn!(%err, "Failed to set IPC connection non-blocking; dropping it");
                        } else {
                            register_connection(&accept_handle, stream, accept_connections.clone());
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(err) => tracing::warn!(%err, "Failed to accept IPC connection"),
                }
                // Never propagate an Err here: that would tear down the
                // whole calloop dispatch (and with it, the compositor)
                // over a single bad IPC accept.
                Ok(PostAction::Continue)
            },
        )
        .map_err(|err| std::io::Error::other(err.to_string()))?;

    // Periodic subscriber flush. The fast path (write buffer not full at
    // emit time) is already handled inline inside `emit_ipc_event`; this
    // timer is the retry path for subscribers whose kernel write buffer
    // was momentarily full, and also the probe that retires a wedged
    // subscriber once its pending cap is exceeded. Re-arms itself on the
    // same interval rather than using `Timer::immediate()` so an idle
    // compositor with no subscribers costs one short wakeup per frame --
    // the same cost both backends' own redraw-timer already pays.
    loop_handle
        .insert_source(
            Timer::from_duration(FLUSH_INTERVAL),
            |_, _, state: &mut Smallvil| {
                state.flush_ipc_subscribers();
                TimeoutAction::ToDuration(FLUSH_INTERVAL)
            },
        )
        .map_err(|err| std::io::Error::other(err.to_string()))?;

    std::env::set_var("TIDEWM_SOCKET", &path);
    tracing::info!(path = %path.display(), "IPC socket listening");
    Ok(SocketGuard(path))
}

fn register_connection(
    loop_handle: &LoopHandle<Smallvil>,
    stream: UnixStream,
    active_connections: Arc<AtomicUsize>,
) {
    let mut buf = Vec::new();
    active_connections.fetch_add(1, Ordering::AcqRel);
    let lease = ConnectionLease(active_connections.clone());
    let timeout_token = Rc::new(Cell::new(None));
    let source_token = Rc::new(Cell::new(None));
    let timeout_for_connection = timeout_token.clone();
    let source_for_connection = source_token.clone();
    let connection_handle = loop_handle.clone();
    let response_connections = active_connections.clone();
    let result = loop_handle.insert_source(
        Generic::new(stream, Interest::READ, Mode::Level),
        move |_readiness, stream, state: &mut Smallvil| {
            let _keep_lease_alive = &lease;
            macro_rules! finish {
                () => {{
                    if let Some(token) = timeout_for_connection.take() {
                        connection_handle.remove(token);
                    }
                    source_for_connection.set(None);
                    return Ok(PostAction::Remove);
                }};
            }
            let mut chunk = [0u8; 4096];
            loop {
                let mut reader: &UnixStream = stream;
                match reader.read(&mut chunk) {
                    Ok(0) => finish!(),
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > MAX_REQUEST_BYTES {
                            tracing::warn!("IPC request exceeded size cap; dropping connection");
                            finish!();
                        }
                        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let line = buf[..pos].to_vec();
                            // Subscribe takes a different lifecycle than the
                            // one-line-in/one-line-out requests: the connection
                            // stays open and gets converted into a long-lived
                            // subscriber. Detected here, before
                            // `response_payload` runs, so a Subscribe doesn't
                            // get formatted into the standard ok/error
                            // envelope (its ack is written by
                            // `register_subscriber` directly through the new
                            // subscriber's own pending buffer).
                            match serde_json::from_slice::<Request>(&line) {
                                Ok(Request::Subscribe { events }) => {
                                    let filter = events
                                        .unwrap_or_default()
                                        .into_iter()
                                        .collect();
                                    register_subscriber(
                                        &connection_handle,
                                        stream,
                                        filter,
                                        state,
                                    );
                                    finish!();
                                }
                                _ => {
                                    let payload = response_payload(&line, state);
                                    match stream.try_clone() {
                                        Ok(stream) => register_response(
                                            &connection_handle,
                                            stream,
                                            payload,
                                            response_connections.clone(),
                                        ),
                                        Err(err) => {
                                            tracing::warn!(%err, "Failed to clone IPC response stream")
                                        }
                                    }
                                    finish!();
                                }
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        return Ok(PostAction::Continue);
                    }
                    Err(err) => {
                        tracing::warn!(%err, "IPC connection read error");
                        finish!();
                    }
                }
            }
        },
    );
    let token = match result {
        Ok(token) => token,
        Err(err) => {
            tracing::warn!(%err, "Failed to register IPC connection");
            return;
        }
    };
    source_token.set(Some(token));

    let timeout_handle = loop_handle.clone();
    let timeout_for_timer = timeout_token.clone();
    let source_for_timer = source_token.clone();
    match loop_handle.insert_source(
        Timer::from_duration(REQUEST_TIMEOUT),
        move |_, _, _state| {
            timeout_for_timer.set(None);
            if let Some(token) = source_for_timer.take() {
                timeout_handle.remove(token);
            }
            TimeoutAction::Drop
        },
    ) {
        Ok(token) => timeout_token.set(Some(token)),
        Err(err) => {
            tracing::warn!(%err, "Failed to register IPC request timeout; dropping connection");
            if let Some(token) = source_token.take() {
                loop_handle.remove(token);
            }
        }
    }
}

/// Converts an incoming `subscribe` request's connection into a long-lived
/// subscriber. Called from inside `register_connection`'s read callback,
/// right before that source retires via `PostAction::Remove` -- the
/// connection's request-response phase is over, and the subscribe phase
/// takes over with its own read-side EOF watcher on a cloned FD.
///
/// The original stream is owned by calloop's retiring source and will be
/// closed when that source drops. We `try_clone()` twice: once for our
/// write side and once for a new calloop READ source whose only job is to
/// detect peer-side close (so a widget that disconnects is reaped even if
/// no events would otherwise probe the connection).
fn register_subscriber(
    loop_handle: &LoopHandle<Smallvil>,
    stream: &UnixStream,
    filter: HashSet<EventKind>,
    state: &mut Smallvil,
) {
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "Failed to clone IPC subscriber write stream");
            return;
        }
    };
    let eof_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "Failed to clone IPC subscriber EOF stream");
            return;
        }
    };

    let id = state.next_ipc_subscriber_id;
    state.next_ipc_subscriber_id += 1;

    state.ipc_subscribers.insert(
        id,
        IpcSubscriber {
            stream: write_stream,
            filter: filter.clone(),
            pending: VecDeque::new(),
            read_token: None,
        },
    );

    // The ack echoes back the resolved event set: the full menu when the
    // subscriber asked for "all", the actual filter otherwise -- so a
    // widget can tell at handshake time which events it will in fact
    // receive, including a typo'd filter rejecting nothing silently.
    let resolved_events: Vec<&'static str> = if filter.is_empty() {
        EventKind::ALL.iter().map(|k| k.wire_name()).collect()
    } else {
        filter.iter().map(|k| k.wire_name()).collect()
    };
    let ack_line = serde_json::to_vec(&json!({
        "ok": true,
        "data": {
            "subscription_id": id,
            "events": resolved_events,
        }
    }))
    .unwrap_or_else(|_| br#"{"ok":true,"data":{"subscription_id":0,"events":[]}}"#.to_vec());
    if let Some(sub) = state.ipc_subscribers.get_mut(&id) {
        sub.pending.extend(ack_line);
        sub.pending.push_back(b'\n');
        // Best-effort inline flush. The periodic `flush_ipc_subscribers`
        // timer is the retry path if the kernel buffer was full.
        sub.try_flush();
    }

    // EOF watcher on the cloned read side. Any data the client sends after
    // the initial subscribe request is silently drained -- the protocol is
    // strictly subscribe-then-receive, no second request. EOF or read
    // error tears down the subscriber.
    let result = loop_handle.insert_source(
        Generic::new(eof_stream, Interest::READ, Mode::Level),
        move |_readiness, stream, state: &mut Smallvil| {
            let mut buf = [0u8; 64];
            let mut reader: &UnixStream = stream;
            match reader.read(&mut buf) {
                Ok(0) => {
                    // Clean client-side close.
                    state.ipc_subscribers.remove(&id);
                    Ok(PostAction::Remove)
                }
                Ok(_) => {
                    // Stray bytes post-subscribe -- drain and ignore, the
                    // subscription is the only valid request on this
                    // connection.
                    Ok(PostAction::Continue)
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    Ok(PostAction::Continue)
                }
                Err(err) => {
                    tracing::warn!(%err, "IPC subscriber read error; dropping");
                    state.ipc_subscribers.remove(&id);
                    Ok(PostAction::Remove)
                }
            }
        },
    );
    let token = match result {
        Ok(token) => token,
        Err(err) => {
            tracing::warn!(%err, "Failed to register IPC subscriber EOF source; dropping subscriber");
            state.ipc_subscribers.remove(&id);
            return;
        }
    };
    // Patch the registration token so a later slow-subscriber drop from
    // `emit_ipc_event`/`flush_ipc_subscribers` can retire this source via
    // `loop_handle.remove`. Without it, a wedged subscriber's read source
    // would leak until the next EOF, which may never come.
    if let Some(sub) = state.ipc_subscribers.get_mut(&id) {
        sub.read_token = Some(token);
    }
}

fn register_response(
    loop_handle: &LoopHandle<Smallvil>,
    stream: UnixStream,
    payload: Vec<u8>,
    active_connections: Arc<AtomicUsize>,
) {
    let mut written = 0;
    active_connections.fetch_add(1, Ordering::AcqRel);
    let lease = ConnectionLease(active_connections);
    let timeout_token = Rc::new(Cell::new(None));
    let source_token = Rc::new(Cell::new(None));
    let timeout_for_response = timeout_token.clone();
    let source_for_response = source_token.clone();
    let response_handle = loop_handle.clone();
    let result = loop_handle.insert_source(
        Generic::new(stream, Interest::WRITE, Mode::Level),
        move |_readiness, stream, _state: &mut Smallvil| {
            let _keep_lease_alive = &lease;
            loop {
                let mut writer: &UnixStream = stream;
                match writer.write(&payload[written..]) {
                    Ok(0) => {
                        tracing::warn!("IPC client closed before response completed");
                        break;
                    }
                    Ok(count) => {
                        written += count;
                        if written == payload.len() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        return Ok(PostAction::Continue);
                    }
                    Err(err) => {
                        tracing::warn!(%err, "Failed to write IPC response");
                        break;
                    }
                }
            }
            if let Some(token) = timeout_for_response.take() {
                response_handle.remove(token);
            }
            source_for_response.set(None);
            Ok(PostAction::Remove)
        },
    );
    let token = match result {
        Ok(token) => token,
        Err(err) => {
            tracing::warn!(%err, "Failed to register IPC response writer");
            return;
        }
    };
    source_token.set(Some(token));

    let timeout_handle = loop_handle.clone();
    let timeout_for_timer = timeout_token.clone();
    let source_for_timer = source_token.clone();
    match loop_handle.insert_source(
        Timer::from_duration(REQUEST_TIMEOUT),
        move |_, _, _state| {
            timeout_for_timer.set(None);
            if let Some(token) = source_for_timer.take() {
                timeout_handle.remove(token);
            }
            TimeoutAction::Drop
        },
    ) {
        Ok(token) => timeout_token.set(Some(token)),
        Err(err) => {
            tracing::warn!(%err, "Failed to register IPC response timeout; dropping response");
            if let Some(token) = source_token.take() {
                loop_handle.remove(token);
            }
        }
    }
}

fn response_payload(line: &[u8], state: &mut Smallvil) -> Vec<u8> {
    let response = match serde_json::from_slice::<Request>(line) {
        Ok(request) => handle_request(state, request),
        Err(err) => json!({ "ok": false, "error": format!("invalid request: {err}") }),
    };
    let mut payload = serde_json::to_vec(&response)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"internal error"}"#.to_vec());
    payload.push(b'\n');
    payload
}

fn handle_request(state: &mut Smallvil, request: Request) -> serde_json::Value {
    match request {
        Request::Outputs => json!({ "ok": true, "data": outputs_json(state) }),
        Request::Workspaces => json!({ "ok": true, "data": workspaces_json(state) }),
        Request::Windows => json!({ "ok": true, "data": windows_json(state) }),
        Request::FocusedWindow => json!({ "ok": true, "data": focused_window_json(state) }),
        Request::ActiveSubmap => json!({ "ok": true, "data": state.active_submap }),
        Request::Diagnostics => json!({ "ok": true, "data": diagnostics_json(state) }),
        // Subscribe is intercepted upstream in `register_connection` before
        // `response_payload` ever runs, since handling it requires the
        // connection's stream (not available here). Reaching this arm means
        // Subscribe was sent after another request on the same connection --
        // not a valid sequence on this one-shot protocol.
        Request::Subscribe { .. } => json!({
            "ok": false,
            "error": "subscribe must be the first and only request on a fresh connection"
        }),
        Request::Action { action } => match config::parse_action(&action) {
            Some(parsed) => match run_ipc_action(state, parsed) {
                Ok(()) => json!({ "ok": true }),
                Err(err) => json!({ "ok": false, "error": err }),
            },
            None => json!({ "ok": false, "error": format!("unknown action: {action}") }),
        },
        Request::Eval { expression } => match state.eval_expression(&expression) {
            Ok(value) => json!({ "ok": true, "data": value }),
            Err(err) => json!({ "ok": false, "error": err }),
        },
        Request::Batch { actions } => {
            if actions.is_empty() || actions.len() > MAX_BATCH_ACTIONS {
                return json!({
                    "ok": false,
                    "error": format!("batch must contain 1..={MAX_BATCH_ACTIONS} actions")
                });
            }
            // Validate the complete batch first. A typo in action N must not
            // leave the desktop half-mutated after actions 0..N-1 ran.
            let parsed: Option<Vec<_>> = actions
                .iter()
                .map(|action| config::parse_action(action))
                .collect();
            let Some(parsed) = parsed else {
                let invalid = actions
                    .iter()
                    .find(|action| config::parse_action(action).is_none())
                    .map(String::as_str)
                    .unwrap_or("unknown");
                return json!({ "ok": false, "error": format!("unknown action: {invalid}") });
            };
            let count = parsed.len();
            for action in parsed {
                if let Err(err) = run_ipc_action(state, action) {
                    return json!({ "ok": false, "error": err });
                }
            }
            json!({ "ok": true, "data": { "executed": count } })
        }
    }
}

/// `quit` needs one event-loop turn of grace so the tiny success response
/// registered by the connection callback can reach the client before the
/// loop stops. Other actions remain synchronous, preserving batch order.
fn run_ipc_action(state: &mut Smallvil, action: config::Action) -> Result<(), String> {
    if matches!(action, config::Action::Quit) {
        state
            .loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(25)),
                |_, _, state: &mut Smallvil| {
                    state.run_action(config::Action::Quit);
                    TimeoutAction::Drop
                },
            )
            .map(|_| ())
            .map_err(|err| format!("failed to schedule compositor shutdown: {err}"))
    } else {
        state.run_action(action);
        Ok(())
    }
}

fn outputs_json(state: &Smallvil) -> serde_json::Value {
    let ocean_engine = state.config.spatial_engine == crate::config::SpatialEngine::Ocean;
    let outputs: Vec<_> = state
        .space
        .outputs()
        .map(|output| {
            let loc = output.current_location();
            let mode = output.current_mode();
            let mut entry = json!({
                "name": output.name(),
                "position": [loc.x, loc.y],
                "size": mode.map(|m| [m.size.w, m.size.h]),
                "refresh_mhz": mode.map(|m| m.refresh),
                "scale": output.current_scale().fractional_scale(),
                "transform": format!("{:?}", output.current_transform()),
                "active_workspace": if ocean_engine {
                    serde_json::Value::Null
                } else {
                    json!(state.layout.active_workspace(&output.name()))
                },
            });
            if ocean_engine {
                let camera = state.ocean.camera(&output.name()).origin;
                entry["camera_origin"] = json!([camera.x, camera.y]);
                entry["camera_zoom"] = json!(state.ocean.camera(&output.name()).zoom);
            }
            entry
        })
        .collect();
    json!(outputs)
}

fn workspaces_json(state: &Smallvil) -> serde_json::Value {
    if state.config.spatial_engine == crate::config::SpatialEngine::Ocean {
        return json!([]);
    }
    let mut keys: HashSet<(String, u32)> =
        state.layout.populated_workspaces().into_iter().collect();
    for output in state.space.outputs() {
        let name = output.name();
        keys.insert((name.clone(), state.layout.active_workspace(&name)));
    }
    for tag in state.floating_workspace.values() {
        keys.insert((tag.output.clone(), tag.workspace));
    }

    let workspaces: Vec<_> = keys
        .into_iter()
        .map(|(output, workspace)| {
            let tiled = state.layout.windows_in(&output, workspace).len();
            let floating = state
                .floating_workspace
                .values()
                .filter(|t| t.output == output && t.workspace == workspace)
                .count();
            let mut entry = json!({
                "output": output,
                "workspace": workspace,
                "active": state.layout.active_workspace(&output) == workspace,
                "window_count": tiled + floating,
            });
            // Scratchpad workspaces carry their name ("" for the unnamed
            // one) so bars can label or hide them instead of showing the
            // raw reserved number.
            if crate::state::is_scratchpad_workspace(workspace) {
                entry["scratchpad"] = json!(state.scratchpad_name_of(workspace).unwrap_or(""));
            }
            entry
        })
        .collect();
    json!(workspaces)
}

/// Only currently-mapped (visible) windows -- a window tiled or tagged on a
/// hidden workspace isn't in `space.elements()` at all, same structural
/// limitation `FloatingTag` was introduced to work around internally.
/// Listing hidden-workspace windows too is a reasonable follow-up once
/// something needs it.
fn windows_json(state: &Smallvil) -> serde_json::Value {
    let focused = state.focused_window_surface();
    let windows: Vec<_> = state
        .space
        .elements()
        .filter_map(|window| window_json(state, window, focused.as_ref()))
        .collect();
    json!(windows)
}

fn focused_window_json(state: &Smallvil) -> serde_json::Value {
    let Some(focused) = state.focused_window_surface() else {
        return serde_json::Value::Null;
    };
    let window = state
        .space
        .elements()
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &focused));
    match window {
        Some(window) => {
            window_json(state, window, Some(&focused)).unwrap_or(serde_json::Value::Null)
        }
        None => serde_json::Value::Null,
    }
}

/// Compositor-side facts for `tidectl doctor`/`report`. The host-side
/// checks (kernel, GPU, services) run in the CLI itself; this is the half
/// only a running TideWM knows: what it is, how it was built, how long it's
/// been up, what the config parser said, and what state it's in right now.
fn diagnostics_json(state: &Smallvil) -> serde_json::Value {
    let layer_surfaces: usize = state
        .space
        .outputs()
        .map(|output| {
            smithay::desktop::layer_map_for_output(output)
                .layers()
                .count()
        })
        .sum();
    let keybinds = state.config.keybinds.len();
    let submaps = state.config.submaps.len();
    #[cfg(feature = "screencast")]
    let screencast_feature = state.screencast.is_some();
    #[cfg(not(feature = "screencast"))]
    let screencast_feature = false;
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "commit": option_env!("TIDEWM_GIT_COMMIT"),
        "dirty": option_env!("TIDEWM_GIT_DIRTY").is_some(),
        "build_date": option_env!("TIDEWM_BUILD_DATE"),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "backend": state.backend_name,
        "uptime_secs": state.start_time.elapsed().as_secs(),
        "spatial_engine": match state.config.spatial_engine {
            crate::config::SpatialEngine::Classic => "classic",
            crate::config::SpatialEngine::Ocean => "ocean",
        },
        "water_effects": state.config.water_effects,
        "config_path": crate::config::Config::path(),
        "config_warnings": state.config_warnings,
        "xwayland_enabled": state.config.xwayland.enabled,
        "session_lock": !matches!(state.session_lock, crate::tide_core::state::SessionLock::Unlocked),
        "layer_surfaces": layer_surfaces,
        "keybind_count": keybinds,
        "submap_count": submaps,
        "screencast_feature": screencast_feature,
    })
}

fn window_json(
    state: &Smallvil,
    window: &smithay::desktop::Window,
    focused: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
) -> Option<serde_json::Value> {
    let toplevel = window.toplevel()?;
    let surface = toplevel.wl_surface();
    let (app_id, title) = state.toplevel_identity(surface);

    let ocean_engine = state.config.spatial_engine == crate::config::SpatialEngine::Ocean;
    let is_floating = if ocean_engine {
        state.ocean.floating_rect(surface).is_some()
    } else {
        state.floating_workspace.contains_key(surface)
    };
    let output = if ocean_engine {
        None
    } else {
        state
            .layout
            .output_of(surface)
            .map(str::to_string)
            .or_else(|| {
                state
                    .floating_workspace
                    .get(surface)
                    .map(|tag| tag.output.clone())
            })
            .or_else(|| state.output_for_window(window).map(|o| o.name()))
    };
    let workspace = if ocean_engine {
        None
    } else if is_floating {
        state.floating_workspace.get(surface).map(|t| t.workspace)
    } else {
        state.layout.workspace_of(surface)
    };

    let mut entry = json!({
        "window_id": state.foreign_toplevel_numeric_ids.get(surface).copied(),
        "title": title,
        "app_id": app_id,
        "output": output,
        "workspace": workspace,
        "floating": is_floating,
        "pinned": state.pinned.contains(surface),
        "pseudo_tiled": state.pseudo_tiled.contains(surface),
        "fullscreen": state.fullscreen.contains_key(surface),
        "urgent": state.urgent.contains(surface),
        // While fullscreen, `maximized` is retained internally only as the
        // mode to restore on exit; it is not the current protocol/placement
        // state and should not be exposed as simultaneously active.
        "maximized": state.maximized.contains_key(surface)
            && !state.fullscreen.contains_key(surface),
        "focused": focused == Some(surface),
    });
    if ocean_engine {
        entry["entry_output"] = json!(state.ocean.entry_output(surface));
    }
    if let Some(workspace) = workspace {
        if crate::state::is_scratchpad_workspace(workspace) {
            entry["scratchpad"] = json!(state.scratchpad_name_of(workspace).unwrap_or(""));
        }
    }
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

    /// Marker type satisfying both `ClientData` (for the throwaway test
    /// client) and `Dispatch<WlSurface, ()>` (for resource creation) --
    /// `request` is never called since these tests never dispatch a real
    /// client's bytes.
    struct DummyDispatch;

    impl smithay::reexports::wayland_server::backend::ClientData for DummyDispatch {}

    impl smithay::reexports::wayland_server::Dispatch<WlSurface, ()> for DummyDispatch {
        fn request(
            _state: &mut Self,
            _client: &smithay::reexports::wayland_server::Client,
            _resource: &WlSurface,
            _request: <WlSurface as smithay::reexports::wayland_server::Resource>::Request,
            _data: &(),
            _dhandle: &smithay::reexports::wayland_server::DisplayHandle,
            _data_init: &mut smithay::reexports::wayland_server::DataInit<'_, Self>,
        ) {
            unreachable!("test surfaces never receive real protocol requests")
        }
    }

    /// Surface stand-in for tests that don't touch Smallvil. `kind`/
    /// `event_name`/`event_matches` only read the variant discriminant, not
    /// the surface's content, but wayland-server has no "unbound" resource
    /// constructor -- a `WlSurface` is only ever produced by creating a real
    /// (if never-dispatched) protocol object on an in-process client. The
    /// backing `Display` is leaked so the returned resource stays valid for
    /// the life of the test process; nothing ever reads or writes its
    /// socket half.
    fn dummy_surface() -> WlSurface {
        use smithay::reexports::wayland_server::Display;

        let display: &'static mut Display<DummyDispatch> =
            Box::leak(Box::new(Display::new().unwrap()));
        let mut handle = display.handle();
        let (stream, _peer) = UnixStream::pair().unwrap();
        let client = handle
            .insert_client(stream, std::sync::Arc::new(DummyDispatch))
            .unwrap();
        client
            .create_resource::<WlSurface, (), DummyDispatch>(&handle, 1, ())
            .unwrap()
    }

    #[test]
    fn event_kind_deserializes_from_kebab_case() {
        // Matches the rename_all on the enum and the wire format documented
        // in the module docs. A typo here would silently parse-fail and the
        // subscriber would get "all events" instead of the filtered set.
        let parsed: EventKind = serde_json::from_str("\"window\"").unwrap();
        assert_eq!(parsed, EventKind::Window);
        assert_eq!(
            serde_json::from_str::<EventKind>("\"workspace\"").unwrap(),
            EventKind::Workspace
        );
        assert_eq!(
            serde_json::from_str::<EventKind>("\"focus\"").unwrap(),
            EventKind::Focus
        );
        assert_eq!(
            serde_json::from_str::<EventKind>("\"urgent\"").unwrap(),
            EventKind::Urgent
        );
        assert_eq!(
            serde_json::from_str::<EventKind>("\"depth\"").unwrap(),
            EventKind::Depth
        );
        assert_eq!(
            serde_json::from_str::<EventKind>("\"config\"").unwrap(),
            EventKind::Config
        );
    }

    #[test]
    fn unknown_event_kind_is_rejected_not_silently_defaulted() {
        // A typo in a subscribe filter must fail loudly (the deserializer
        // returns Err), not fall back to a default variant -- otherwise a
        // widget subscribing to "windows" (plural) would silently get
        // nothing it asked for.
        assert!(serde_json::from_str::<EventKind>("\"windows\"").is_err());
        assert!(serde_json::from_str::<EventKind>("\"WINDOW\"").is_err());
        assert!(serde_json::from_str::<EventKind>("\"scratchpad\"").is_err());
    }

    #[test]
    fn subscribe_with_no_events_field_means_all() {
        // The wire format documented in the module docs: omitting `events`
        // is the same as listing all six channels.
        let parsed: Request = serde_json::from_str(r#"{"request":"subscribe"}"#).unwrap();
        match parsed {
            Request::Subscribe { events: None } => {}
            other => panic!("expected Subscribe with no events, got {other:?}"),
        }
    }

    #[test]
    fn subscribe_with_explicit_filter_round_trips() {
        let parsed: Request =
            serde_json::from_str(r#"{"request":"subscribe","events":["workspace","focus"]}"#)
                .unwrap();
        match parsed {
            Request::Subscribe { events: Some(list) } => {
                assert_eq!(list, vec![EventKind::Workspace, EventKind::Focus]);
            }
            other => panic!("expected Subscribe with events, got {other:?}"),
        }
    }

    #[test]
    fn event_name_and_kind_are_consistent_per_variant() {
        let surface = dummy_surface();
        // Every variant's `kind()` must match its top-level channel, and
        // every `event_name()` must be the documented wire string -- a
        // widget routing on these strings would silently miss events
        // otherwise.
        let cases: Vec<(IpcEvent, EventKind, &'static str)> = vec![
            (
                IpcEvent::WindowOpened {
                    surface: surface.clone(),
                },
                EventKind::Window,
                "window-opened",
            ),
            (
                IpcEvent::WindowClosed {
                    window_id: None,
                    app_id: None,
                    title: None,
                },
                EventKind::Window,
                "window-closed",
            ),
            (
                IpcEvent::WindowChanged {
                    surface: surface.clone(),
                },
                EventKind::Window,
                "window-changed",
            ),
            (
                IpcEvent::WorkspaceChanged {
                    output: "eDP-1".into(),
                    from: 1,
                    to: 2,
                },
                EventKind::Workspace,
                "workspace-changed",
            ),
            (
                IpcEvent::FocusChanged {
                    surface: Some(surface.clone()),
                },
                EventKind::Focus,
                "focus-changed",
            ),
            (
                IpcEvent::UrgentChanged {
                    surface: surface.clone(),
                    urgent: true,
                },
                EventKind::Urgent,
                "urgent-changed",
            ),
            (
                IpcEvent::DepthChanged {
                    surface: surface.clone(),
                    tier: 1,
                },
                EventKind::Depth,
                "depth-changed",
            ),
            (
                IpcEvent::ConfigReloaded,
                EventKind::Config,
                "config-reloaded",
            ),
        ];
        for (event, kind, name) in cases {
            assert_eq!(event.kind(), kind, "mismatch for {name}");
            assert_eq!(event.event_name(), name, "name mismatch");
        }
    }

    #[test]
    fn empty_filter_matches_everything() {
        // The "subscribe to all" semantic: an empty filter set must
        // accept every event variant. A regression here would silently
        // drop events from subscribers that sent `{}` or omitted `events`.
        let filter = HashSet::new();
        assert!(event_matches(
            &filter,
            &IpcEvent::WorkspaceChanged {
                output: "x".into(),
                from: 0,
                to: 1,
            }
        ));
        assert!(event_matches(&filter, &IpcEvent::ConfigReloaded));
        assert!(event_matches(
            &filter,
            &IpcEvent::FocusChanged { surface: None }
        ));
    }

    #[test]
    fn explicit_filter_passes_only_listed_kinds() {
        let mut filter = HashSet::new();
        filter.insert(EventKind::Workspace);
        filter.insert(EventKind::Focus);

        assert!(event_matches(
            &filter,
            &IpcEvent::WorkspaceChanged {
                output: "x".into(),
                from: 0,
                to: 1,
            }
        ));
        assert!(event_matches(
            &filter,
            &IpcEvent::FocusChanged {
                surface: Some(dummy_surface())
            }
        ));

        // A bar that asked only for workspace/focus must not be pinged on
        // every window-opened or urgent change -- those are different
        // channels, even though they're per-window.
        assert!(!event_matches(
            &filter,
            &IpcEvent::WindowOpened {
                surface: dummy_surface()
            }
        ));
        assert!(!event_matches(
            &filter,
            &IpcEvent::UrgentChanged {
                surface: dummy_surface(),
                urgent: true,
            }
        ));
        assert!(!event_matches(&filter, &IpcEvent::ConfigReloaded));
    }

    #[test]
    fn try_flush_drains_pending_into_the_kernel_buffer() {
        // Two-end Unix stream: we push bytes into the subscriber's
        // `pending`, call `try_flush`, then read them out the peer end.
        // This is the fast-path (peer is reading) that emit_ipc_event
        // exercises inline.
        let (subscriber_end, mut peer_end) = std::os::unix::net::UnixStream::pair().unwrap();
        subscriber_end.set_nonblocking(true).unwrap();
        peer_end.set_nonblocking(true).unwrap();

        let mut sub = IpcSubscriber {
            stream: subscriber_end,
            filter: HashSet::new(),
            pending: VecDeque::from(b"hello\n".to_vec()),
            read_token: None,
        };

        assert!(sub.try_flush());
        assert!(
            sub.pending.is_empty(),
            "fast-path drain should empty pending"
        );

        let mut got = [0u8; 8];
        let n = peer_end.read(&mut got).unwrap();
        assert_eq!(&got[..n], b"hello\n");
    }

    #[test]
    fn try_flush_returns_false_when_peer_has_closed() {
        // Drop the peer before flushing -- a write must fail, and
        // try_flush must report that so emit_ipc_event retires the
        // subscriber instead of accumulating bytes forever.
        let (subscriber_end, peer_end) = std::os::unix::net::UnixStream::pair().unwrap();
        subscriber_end.set_nonblocking(true).unwrap();
        drop(peer_end);

        let mut sub = IpcSubscriber {
            stream: subscriber_end,
            filter: HashSet::new(),
            pending: VecDeque::from(b"never-delivered\n".to_vec()),
            read_token: None,
        };
        // On most kernels the first write to a closed AF_UNIX socketpair
        // end returns EPIPE/ECONNRESET immediately. If a particular kernel
        // ever did accept the write into a buffer before observing the
        // close, try_flush would still report true for that one call and
        // false on the next -- so loop a bounded number of times.
        for _ in 0..16 {
            if !sub.try_flush() {
                return;
            }
            let delivered_clean = sub.pending.is_empty();
            if delivered_clean {
                // Refill so the next try actually writes again.
                sub.pending.extend(b"more\n");
            }
        }
        panic!(
            "try_flush never reported peer-closed after 16 attempts; kernel is buffering forever?"
        );
    }

    #[test]
    fn wire_name_is_lowercase_kebab_for_every_kind() {
        // The ack's `events` array uses wire_name directly -- a stray
        // capital or hyphen here would mismatch the documented wire
        // format silently.
        assert_eq!(EventKind::Window.wire_name(), "window");
        assert_eq!(EventKind::Workspace.wire_name(), "workspace");
        assert_eq!(EventKind::Focus.wire_name(), "focus");
        assert_eq!(EventKind::Urgent.wire_name(), "urgent");
        assert_eq!(EventKind::Depth.wire_name(), "depth");
        assert_eq!(EventKind::Config.wire_name(), "config");
    }
}
