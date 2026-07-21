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
    collections::HashSet,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

use serde::Deserialize;
use serde_json::json;
use smithay::reexports::calloop::{generic::Generic, EventLoop, Interest, LoopHandle, Mode, PostAction};

use crate::{config, state::Smallvil};

/// A client that never completes a request line within this many bytes
/// gets disconnected rather than growing the per-connection buffer
/// unbounded -- there's no legitimate request anywhere near this size.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(tag = "request", rename_all = "kebab-case")]
enum Request {
    Outputs,
    Workspaces,
    Windows,
    FocusedWindow,
    ActiveSubmap,
    Action { action: String },
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
    listener.set_nonblocking(true)?;

    let accept_handle = loop_handle.clone();
    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_readiness, listener, _state: &mut Smallvil| {
                // Level-triggered: a second pending connection left
                // unaccepted here just fires again next iteration, so a
                // single accept() per wakeup is enough, no drain loop.
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        if let Err(err) = stream.set_nonblocking(true) {
                            tracing::warn!(%err, "Failed to set IPC connection non-blocking; dropping it");
                        } else {
                            register_connection(&accept_handle, stream);
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

fn register_connection(loop_handle: &LoopHandle<Smallvil>, stream: UnixStream) {
    let mut buf = Vec::new();
    let result = loop_handle.insert_source(
        Generic::new(stream, Interest::READ, Mode::Level),
        move |_readiness, stream, state: &mut Smallvil| {
            let mut chunk = [0u8; 4096];
            loop {
                let mut reader: &UnixStream = stream;
                match reader.read(&mut chunk) {
                    Ok(0) => return Ok(PostAction::Remove),
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > MAX_REQUEST_BYTES {
                            tracing::warn!("IPC request exceeded size cap; dropping connection");
                            return Ok(PostAction::Remove);
                        }
                        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let line = buf[..pos].to_vec();
                            respond(&line, stream, state);
                            return Ok(PostAction::Remove);
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        return Ok(PostAction::Continue);
                    }
                    Err(err) => {
                        tracing::warn!(%err, "IPC connection read error");
                        return Ok(PostAction::Remove);
                    }
                }
            }
        },
    );
    if let Err(err) = result {
        tracing::warn!(%err, "Failed to register IPC connection");
    }
}

fn respond(line: &[u8], stream: &UnixStream, state: &mut Smallvil) {
    let response = match serde_json::from_slice::<Request>(line) {
        Ok(request) => handle_request(state, request),
        Err(err) => json!({ "ok": false, "error": format!("invalid request: {err}") }),
    };
    let mut payload = serde_json::to_vec(&response)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"internal error"}"#.to_vec());
    payload.push(b'\n');
    let mut writer: &UnixStream = stream;
    if let Err(err) = writer.write_all(&payload) {
        tracing::warn!(%err, "Failed to write IPC response");
    }
}

fn handle_request(state: &mut Smallvil, request: Request) -> serde_json::Value {
    match request {
        Request::Outputs => json!({ "ok": true, "data": outputs_json(state) }),
        Request::Workspaces => json!({ "ok": true, "data": workspaces_json(state) }),
        Request::Windows => json!({ "ok": true, "data": windows_json(state) }),
        Request::FocusedWindow => json!({ "ok": true, "data": focused_window_json(state) }),
        Request::ActiveSubmap => json!({ "ok": true, "data": state.active_submap }),
        Request::Action { action } => match config::parse_action(&action) {
            Some(parsed) => {
                state.run_action(parsed);
                json!({ "ok": true })
            }
            None => json!({ "ok": false, "error": format!("unknown action: {action}") }),
        },
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
    let mut keys: HashSet<(String, u32)> = state.layout.populated_workspaces().into_iter().collect();
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
            json!({
                "output": output,
                "workspace": workspace,
                "active": state.layout.active_workspace(&output) == workspace,
                "window_count": tiled + floating,
            })
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
        Some(window) => window_json(state, window, Some(&focused)).unwrap_or(serde_json::Value::Null),
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
        .or_else(|| state.floating_workspace.get(surface).map(|tag| tag.output.clone()))
        .or_else(|| state.output_for_window(window).map(|o| o.name()));
    let workspace = if is_floating {
        state.floating_workspace.get(surface).map(|t| t.workspace)
    } else {
        state.layout.workspace_of(surface)
    };

    Some(json!({
        "title": title,
        "app_id": app_id,
        "output": output,
        "workspace": workspace,
        "floating": is_floating,
        "pinned": state.pinned.contains(surface),
        "pseudo_tiled": state.pseudo_tiled.contains(surface),
        "fullscreen": state.fullscreen.contains_key(surface),
        // While fullscreen, `maximized` is retained internally only as the
        // mode to restore on exit; it is not the current protocol/placement
        // state and should not be exposed as simultaneously active.
        "maximized": state.maximized.contains_key(surface)
            && !state.fullscreen.contains_key(surface),
        "focused": focused == Some(surface),
    }))
}
