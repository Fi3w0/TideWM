//! Minimal JSON-over-Unix-socket control interface (Phase A item 3 of the
//! WM feature-parity roadmap, see AGENT.md). One connection, one request
//! line in, one response line out, then the server closes it -- not a
//! persistent multi-request session and no event-stream mode, both left for
//! a later phase once something actually needs them.
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
//! Every response is `{"ok": true, "data": ...}` or `{"ok": false, "error":
//! "..."}` -- malformed JSON or an unrecognized action string gets an error
//! response, not a dropped connection, so a scripting mistake is visible to
//! whatever's on the other end of the socket.

use std::{
    cell::Cell,
    collections::HashSet,
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
use smithay::reexports::calloop::{
    generic::Generic,
    timer::{TimeoutAction, Timer},
    EventLoop, Interest, LoopHandle, Mode, PostAction,
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

#[derive(Deserialize)]
#[serde(tag = "request", rename_all = "kebab-case")]
enum Request {
    Outputs,
    Workspaces,
    Windows,
    FocusedWindow,
    ActiveSubmap,
    Action { action: String },
    Batch { actions: Vec<String> },
}

const MAX_BATCH_ACTIONS: usize = 128;

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
        Request::Action { action } => match config::parse_action(&action) {
            Some(parsed) => match run_ipc_action(state, parsed) {
                Ok(()) => json!({ "ok": true }),
                Err(err) => json!({ "ok": false, "error": err }),
            },
            None => json!({ "ok": false, "error": format!("unknown action: {action}") }),
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
    let outputs: Vec<_> = state
        .space
        .outputs()
        .map(|output| {
            let loc = output.current_location();
            let mode = output.current_mode();
            json!({
                "name": output.name(),
                "position": [loc.x, loc.y],
                "size": mode.map(|m| [m.size.w, m.size.h]),
                "refresh_mhz": mode.map(|m| m.refresh),
                "scale": output.current_scale().fractional_scale(),
                "transform": format!("{:?}", output.current_transform()),
                "active_workspace": state.layout.active_workspace(&output.name()),
            })
        })
        .collect();
    json!(outputs)
}

fn workspaces_json(state: &Smallvil) -> serde_json::Value {
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

fn window_json(
    state: &Smallvil,
    window: &smithay::desktop::Window,
    focused: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
) -> Option<serde_json::Value> {
    let toplevel = window.toplevel()?;
    let surface = toplevel.wl_surface();
    let (app_id, title) = state.toplevel_identity(surface);

    let is_floating = state.floating_workspace.contains_key(surface);
    let output = state
        .layout
        .output_of(surface)
        .map(str::to_string)
        .or_else(|| {
            state
                .floating_workspace
                .get(surface)
                .map(|tag| tag.output.clone())
        })
        .or_else(|| state.output_for_window(window).map(|o| o.name()));
    let workspace = if is_floating {
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
    if let Some(workspace) = workspace {
        if crate::state::is_scratchpad_workspace(workspace) {
            entry["scratchpad"] = json!(state.scratchpad_name_of(workspace).unwrap_or(""));
        }
    }
    Some(entry)
}
