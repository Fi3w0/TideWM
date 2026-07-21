//! tidectl: a small CLI for TideWM's IPC socket (see `src/ipc.rs` for the
//! wire protocol this speaks -- one JSON request line in, one JSON
//! response line out, then the connection closes).
//!
//! Two kinds of commands: read queries (`outputs`/`workspaces`/`windows`/
//! `focused-window`) and actions, which are exactly the same strings a
//! `bind` statement's action half uses in config.wave (`close-window`,
//! `workspace:3`, `spawn:kitty`, ...) -- this CLI does no validation of its
//! own beyond a
//! few space-separated shorthands (`workspace 3` instead of `workspace:3`);
//! an unrecognized action string is rejected by the compositor itself,
//! same as a bad keybind would be.

use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

fn main() {
    let raw_args: Vec<String> = env::args().skip(1).collect();

    let mut json_output = false;
    let mut socket_override: Option<PathBuf> = None;
    let mut args: Vec<String> = Vec::with_capacity(raw_args.len());
    let mut iter = raw_args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" | "-j" => json_output = true,
            "--socket" => match iter.next() {
                Some(path) => socket_override = Some(PathBuf::from(path)),
                None => fail("--socket requires a path argument"),
            },
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            _ => args.push(arg),
        }
    }

    if args.is_empty() {
        print_help();
        std::process::exit(1);
    }

    let request = build_request(&args).unwrap_or_else(|msg| fail(&msg));
    let socket = socket_override
        .clone()
        .map(Ok)
        .unwrap_or_else(find_socket)
        .unwrap_or_else(|msg| fail(&msg));
    let response = match send_request(&socket, &request) {
        Ok(r) => r,
        Err(e) if socket_override.is_none() && e.kind() == std::io::ErrorKind::ConnectionRefused => {
            // A stale socket file: TideWM's cleanup only runs on a clean
            // `Drop` (see ipc.rs's `SocketGuard`), which a SIGKILL or a
            // crash skips entirely, leaving the file behind after the
            // compositor is long gone. Remove it and retry once against
            // whatever socket (if any) is left.
            let _ = std::fs::remove_file(&socket);
            let retry_socket = find_socket().unwrap_or_else(|msg| fail(&msg));
            send_request(&retry_socket, &request)
                .unwrap_or_else(|e| fail(&format!("failed to connect to {}: {e}", retry_socket.display())))
        }
        Err(e) => fail(&format!("failed to connect to {}: {e}", socket.display())),
    };

    let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if json_output {
        println!("{response}");
    } else if !ok {
        let err = response.get("error").and_then(Value::as_str).unwrap_or("unknown error");
        eprintln!("tidectl: {err}");
    } else {
        print_response(&args[0], response.get("data").unwrap_or(&Value::Null));
    }
    std::process::exit(if ok { 0 } else { 1 });
}

fn fail(msg: &str) -> ! {
    eprintln!("tidectl: {msg}");
    std::process::exit(1);
}

/// Builds the `{"request": ...}` JSON body. Read queries pass straight
/// through; everything else is an action, either one of the space-separated
/// shorthands below or forwarded verbatim (covers exact keybind syntax
/// typed directly, e.g. `tidectl workspace:3` or `tidectl toggle-floating`).
fn build_request(args: &[String]) -> Result<Value, String> {
    let rest = &args[1..];
    match args[0].as_str() {
        "outputs" => Ok(json!({ "request": "outputs" })),
        "workspaces" => Ok(json!({ "request": "workspaces" })),
        "windows" => Ok(json!({ "request": "windows" })),
        "focused-window" | "focused" => Ok(json!({ "request": "focused-window" })),
        "active-submap" => Ok(json!({ "request": "active-submap" })),
        "action" if !rest.is_empty() => Ok(action_request(&rest.join(" "))),
        "workspace" if !rest.is_empty() => Ok(action_request(&format!("workspace:{}", rest.join(" ")))),
        "move-to-workspace" if !rest.is_empty() => {
            Ok(action_request(&format!("move-to-workspace:{}", rest.join(" "))))
        }
        "swap-workspaces" if !rest.is_empty() => {
            Ok(action_request(&format!("swap-workspaces:{}", rest.join(" "))))
        }
        "spawn" if !rest.is_empty() => Ok(action_request(&format!("spawn:{}", rest.join(" ")))),
        "submap" if !rest.is_empty() => Ok(action_request(&format!("submap:{}", rest.join(" ")))),
        _ => Ok(action_request(&args.join(" "))),
    }
}

fn action_request(action: &str) -> Value {
    json!({ "request": "action", "action": action })
}

/// `$TIDEWM_SOCKET` (already set for anything TideWM itself spawns) if
/// present, else the newest `tidewm-*.sock` under `$XDG_RUNTIME_DIR` (or
/// `/tmp`, matching `ipc.rs`'s own fallback) -- covers running `tidectl`
/// by hand from an ordinary terminal, which never inherits that env var.
fn find_socket() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("TIDEWM_SOCKET") {
        return Ok(PathBuf::from(path));
    }

    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let entries = std::fs::read_dir(&runtime_dir)
        .map_err(|e| format!("failed to read {}: {e}", runtime_dir.display()))?;

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("tidewm-") && name.ends_with(".sock") {
            if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                    newest = Some((modified, entry.path()));
                }
            }
        }
    }
    newest.map(|(_, path)| path).ok_or_else(|| {
        format!(
            "no TideWM socket found in {} (is TideWM running?)",
            runtime_dir.display()
        )
    })
}

fn send_request(socket_path: &Path, request: &Value) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(socket_path)?;

    let mut payload = serde_json::to_vec(request).map_err(std::io::Error::other)?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let line = buf.split(|&b| b == b'\n').next().unwrap_or(&[]);
    serde_json::from_slice(line).map_err(std::io::Error::other)
}

fn print_response(command: &str, data: &Value) {
    match command {
        "outputs" => print_outputs(data),
        "workspaces" => print_workspaces(data),
        "windows" => print_windows(data),
        "focused-window" | "focused" => print_window(data),
        "active-submap" => match data.as_str() {
            Some(name) => println!("{name}"),
            None => println!("(none, base keybinds active)"),
        },
        _ => println!("ok"),
    }
}

fn print_outputs(data: &Value) {
    let Some(outputs) = data.as_array() else { return };
    for o in outputs {
        let name = o.get("name").and_then(Value::as_str).unwrap_or("?");
        let size = o.get("size").and_then(Value::as_array);
        let (w, h) = size
            .and_then(|s| Some((s.first()?.as_i64()?, s.get(1)?.as_i64()?)))
            .unwrap_or((0, 0));
        let refresh_mhz = o.get("refresh_mhz").and_then(Value::as_i64).unwrap_or(0);
        let scale = o.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
        let transform = o.get("transform").and_then(Value::as_str).unwrap_or("?");
        let workspace = o.get("active_workspace").and_then(Value::as_u64).unwrap_or(0);
        let pos = o.get("position").and_then(Value::as_array);
        let (x, y) = pos
            .and_then(|p| Some((p.first()?.as_i64()?, p.get(1)?.as_i64()?)))
            .unwrap_or((0, 0));
        println!(
            "{name}  {w}x{h}@{:.1}Hz  scale={scale}  pos=({x},{y})  transform={transform}  workspace={workspace}",
            refresh_mhz as f64 / 1000.0
        );
    }
}

fn print_workspaces(data: &Value) {
    let Some(workspaces) = data.as_array() else { return };
    for w in workspaces {
        let output = w.get("output").and_then(Value::as_str).unwrap_or("?");
        let workspace = w.get("workspace").and_then(Value::as_u64).unwrap_or(0);
        let active = w.get("active").and_then(Value::as_bool).unwrap_or(false);
        let count = w.get("window_count").and_then(Value::as_u64).unwrap_or(0);
        let marker = if active { "*" } else { " " };
        println!("{marker} {output}  workspace={workspace}  windows={count}");
    }
}

fn print_windows(data: &Value) {
    let Some(windows) = data.as_array() else { return };
    if windows.is_empty() {
        println!("(no mapped windows)");
        return;
    }
    for w in windows {
        print_window(w);
    }
}

fn print_window(w: &Value) {
    if w.is_null() {
        println!("(none)");
        return;
    }
    let title = w.get("title").and_then(Value::as_str).unwrap_or("");
    let app_id = w.get("app_id").and_then(Value::as_str).unwrap_or("");
    let output = w.get("output").and_then(Value::as_str).unwrap_or("?");
    let workspace = w
        .get("workspace")
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());
    let mut flags = Vec::new();
    for (key, label) in [
        ("floating", "floating"),
        ("pinned", "pinned"),
        ("pseudo_tiled", "pseudo-tiled"),
        ("fullscreen", "fullscreen"),
        ("maximized", "maximized"),
        ("focused", "focused"),
    ] {
        if w.get(key).and_then(Value::as_bool).unwrap_or(false) {
            flags.push(label);
        }
    }
    let flags = if flags.is_empty() { String::new() } else { format!("  [{}]", flags.join(", ")) };
    println!("{app_id}  \"{title}\"  output={output}  workspace={workspace}{flags}");
}

fn print_help() {
    println!(
        r#"tidectl - control interface for a running TideWM

USAGE:
    tidectl <query>
    tidectl <action>

QUERIES:
    outputs             list outputs (name, mode, scale, position, transform, active workspace)
    workspaces          list known workspaces (output, number, active, window count)
    windows             list currently mapped windows
    focused-window       (alias: focused) the currently focused window, if any
    active-submap        the currently active `submap <name> {{ }}` block, if any

ACTIONS:
    Any action a `bind` accepts in config.wave works here too, e.g.:
        close-window, toggle-floating, toggle-fullscreen, toggle-pin,
        toggle-scratchpad, move-to-scratchpad, toggle-pseudo-tile,
        cycle-focus, focus-left/right/up/down, swap-left/right/up/down,
        group-left/right/up/down, ungroup, cycle-tab-next/prev, quit,
        submap:<name>, exit-submap, layout:bsp, layout:master,
        master-grow, master-shrink, toggle-overview

    A few space-separated shorthands, equivalent to the colon syntax above:
        tidectl workspace <N>              same as "workspace:N"
        tidectl move-to-workspace <N>      same as "move-to-workspace:N"
        tidectl swap-workspaces <output>   same as "swap-workspaces:<output>"
        tidectl spawn <cmd...>             same as "spawn:<cmd...>"
        tidectl submap <name>              same as "submap:<name>"
        tidectl action <string>            explicit passthrough

FLAGS:
    --json, -j        print the raw JSON response instead of a formatted view
    --socket <path>   connect to this socket instead of auto-discovering one
    -h, --help        show this help

By default tidectl uses $TIDEWM_SOCKET if set, otherwise the newest
tidewm-*.sock under $XDG_RUNTIME_DIR."#
    );
}
