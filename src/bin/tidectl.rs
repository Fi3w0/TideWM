//! tidectl: a small CLI for TideWM's IPC socket (see `src/tide_core/ipc.rs` for the
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
//!
//! Two host-side commands live outside the socket protocol entirely:
//! `tidectl doctor` (a battery of quick health checks) and `tidectl report`
//! (a plain-text diagnostic file for attaching to GitHub issues) -- see
//! `tidectl_diagnostics.rs`. `tidectl perf` is the third: it does use the
//! socket (two sampled `perf` snapshots across a window) but runs its own
//! request/response + output cycle rather than the generic single-request
//! path.
//!
//! `tidectl subscribe [event...]` is the one long-lived command: it opens
//! the subscribe mode (`ipc.rs`) and prints one JSON line per event,
//! `{"event": "<kind>", "data": ...}`, until the compositor disappears.
//! A bar widget runs it as a persistent process and parses stdout instead
//! of polling the one-shot queries -- instant updates, no busy loop.

mod tidectl_diagnostics;

use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Request/response IPC is local and normally completes in one event-loop
/// turn. Keep a stalled peer from holding a command or script forever while
/// still allowing ample time for a heavily loaded compositor to answer.
const IPC_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// One-shot queries can legitimately list many mapped windows, but no TideWM
/// response needs unbounded storage. This is a protocol/resource bound, not a
/// display-mode or hardware assumption.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// The compositor drops a subscriber after roughly this much queued event
/// data, so one individual newline-delimited record cannot legitimately be
/// larger. The client independently enforces the same wire-level envelope.
const MAX_SUBSCRIPTION_RECORD_BYTES: usize = 256 * 1024;

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

    // Host-side commands: run before any socket work, so a compositor that
    // won't even start can still be diagnosed.
    match args[0].as_str() {
        "doctor" => return cmd_doctor(json_output),
        "report" => return cmd_report(),
        _ => {}
    }

    let socket = socket_override
        .clone()
        .map(Ok)
        .unwrap_or_else(find_socket)
        .unwrap_or_else(|msg| fail(&msg));

    // `perf` needs the socket (two sampled snapshots across a window), so it
    // runs after socket discovery but still handles its own request/response
    // cycle and output formatting rather than the generic single-request path.
    if args[0] == "perf" {
        cmd_perf(&socket, &args[1..], json_output);
    }

    // Long-lived subscribe mode: takes over the process, never returns.
    if args[0] == "subscribe" {
        cmd_subscribe(&socket, socket_override.is_none(), &args[1..]);
    }

    let request = build_request(&args).unwrap_or_else(|msg| fail(&msg));
    let response = match send_request(&socket, &request) {
        Ok(r) => r,
        Err(e)
            if socket_override.is_none() && e.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            // A stale socket file: TideWM's cleanup only runs on a clean
            // `Drop` (see ipc.rs's `SocketGuard`), which a SIGKILL or a
            // crash skips entirely, leaving the file behind after the
            // compositor is long gone. Remove it and retry once against
            // whatever socket (if any) is left.
            let _ = std::fs::remove_file(&socket);
            let retry_socket = find_socket().unwrap_or_else(|msg| fail(&msg));
            send_request(&retry_socket, &request).unwrap_or_else(|e| {
                fail(&format!(
                    "failed to connect to {}: {e}",
                    retry_socket.display()
                ))
            })
        }
        Err(e) => fail(&format!("failed to connect to {}: {e}", socket.display())),
    };

    let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if json_output {
        println!("{response}");
    } else if !ok {
        let err = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        eprintln!("tidectl: {err}");
    } else {
        print_response(&args[0], response.get("data").unwrap_or(&Value::Null));
    }
    std::process::exit(if ok { 0 } else { 1 });
}

/// `tidectl doctor [--json]`: runs the health checks and prints one line
/// per check. Exit code: 0 = nothing wrong, 1 = warnings, 2 = failures
/// (skipped checks don't count either way). `--json` prints the checks as
/// machine-readable JSON for bars/scripts.
fn cmd_doctor(json_output: bool) {
    let (checks, _diagnostics) = tidectl_diagnostics::run_checks();
    if json_output {
        let payload: Vec<Value> = checks
            .iter()
            .map(|check| {
                json!({
                    "name": check.name,
                    "verdict": check.verdict.label().to_lowercase(),
                    "detail": check.detail,
                })
            })
            .collect();
        let verdict = if checks
            .iter()
            .any(|c| c.verdict == tidectl_diagnostics::Verdict::Fail)
        {
            "fail"
        } else if checks
            .iter()
            .any(|c| c.verdict == tidectl_diagnostics::Verdict::Warn)
        {
            "warn"
        } else {
            "ok"
        };
        println!(
            "{}",
            json!({ "ok": true, "verdict": verdict, "checks": payload })
        );
    } else {
        for check in &checks {
            println!(
                "[{:>4}] {}: {}",
                check.verdict.label(),
                check.name,
                check.detail
            );
        }
        let warns = checks
            .iter()
            .filter(|c| c.verdict == tidectl_diagnostics::Verdict::Warn)
            .count();
        let fails = checks
            .iter()
            .filter(|c| c.verdict == tidectl_diagnostics::Verdict::Fail)
            .count();
        println!();
        println!(
            "{} -- {} passed, {} warnings, {} failed, {} skipped",
            if fails > 0 {
                "PROBLEMS DETECTED"
            } else if warns > 0 {
                "WARNINGS"
            } else {
                "Everything looks OK"
            },
            checks
                .iter()
                .filter(|c| c.verdict == tidectl_diagnostics::Verdict::Pass)
                .count(),
            warns,
            fails,
            checks
                .iter()
                .filter(|c| c.verdict == tidectl_diagnostics::Verdict::Skip)
                .count(),
        );
        println!("For a full report to attach to an issue: tidectl report");
    }
    let fails = checks
        .iter()
        .filter(|c| c.verdict == tidectl_diagnostics::Verdict::Fail)
        .count();
    let warns = checks
        .iter()
        .filter(|c| c.verdict == tidectl_diagnostics::Verdict::Warn)
        .count();
    std::process::exit(if fails > 0 {
        2
    } else if warns > 0 {
        1
    } else {
        0
    });
}

/// `tidectl report [--output <path>]`: writes the full diagnostic report
/// to a file (default `tidewm-report.txt` in the current directory) and
/// prints where it went. The quick check runs first and is embedded; the
/// report stays compact unless problems were detected, in which case the
/// log-heavy sections expand.
fn cmd_report() {
    let mut output: PathBuf = PathBuf::from("tidewm-report.txt");
    let mut args = env::args().skip(2);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--output" => match args.next() {
                Some(path) => output = PathBuf::from(path),
                None => fail("--output requires a path argument"),
            },
            "-h" | "--help" => {
                println!("USAGE: tidectl report [--output <path>]");
                std::process::exit(0);
            }
            other => fail(&format!("unrecognized argument '{other}' for report")),
        }
    }

    let (checks, diagnostics) = tidectl_diagnostics::run_checks();
    let verbose = tidectl_diagnostics::needs_verbose(&checks);
    let report = tidectl_diagnostics::render_report(&checks, &diagnostics, verbose);
    if let Err(err) = std::fs::write(&output, &report) {
        fail(&format!("failed to write {}: {err}", output.display()));
    }
    println!("Report written to {}", output.display());
    let fails = checks
        .iter()
        .filter(|c| c.verdict == tidectl_diagnostics::Verdict::Fail)
        .count();
    let warns = checks
        .iter()
        .filter(|c| c.verdict == tidectl_diagnostics::Verdict::Warn)
        .count();
    if fails > 0 {
        println!("Problems detected -- the report includes expanded detail.");
    } else if warns > 0 {
        println!("A few warnings to review -- the report stays compact.");
    } else {
        println!("Everything looks healthy; the report is compact.");
    }
    println!("Attach the file to a GitHub issue at https://github.com/Fi3w0/TideWM/issues");
    std::process::exit(0);
}

/// `tidectl perf [--window <secs>] [--json]`: samples the compositor's
/// resource use and render state over a window (default 3s) and prints a
/// compact performance summary. Takes two IPC `perf` snapshots spaced by
/// the window and derives CPU% from the delta of the compositor's own
/// `getrusage` microsecond counters (so it's always about the right
/// process and needs no CLK_TCK). PSS/RSS/threads and the render
/// self-stats come from the second snapshot. No GPU-busy%: that needs a
/// vendor-specific source the compositor can't read portably.
fn cmd_perf(socket: &Path, args: &[String], json_output: bool) -> ! {
    let mut window_secs: f64 = 3.0;
    let mut iter = args.iter().map(String::as_str);
    while let Some(arg) = iter.next() {
        match arg {
            "--window" | "-w" => match iter.next() {
                Some(v) => match v.parse::<f64>() {
                    Ok(n) if n.is_finite() && n > 0.0 => window_secs = n,
                    _ => fail("--window needs a positive number of seconds"),
                },
                None => fail("--window requires a value"),
            },
            "-h" | "--help" => {
                println!("USAGE: tidectl perf [--window <secs>] [--json]");
                println!("       samples CPU/RAM and render state over <secs> (default 3)");
                std::process::exit(0);
            }
            "--json" | "-j" => {
                // Already consumed by the global flag parser; accept quietly
                // in case it appears after the subcommand.
            }
            other => fail(&format!("unrecognized argument '{other}' for perf")),
        }
    }

    let request = json!({ "request": "perf" });
    let sample = || -> Value {
        let response = match send_request(socket, &request) {
            Ok(r) => r,
            Err(e) => fail(&format!("failed to query compositor: {e}")),
        };
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            fail(&format!(
                "compositor rejected perf request: {}",
                response.get("error").and_then(Value::as_str).unwrap_or("?")
            ));
        }
        response.get("data").cloned().unwrap_or(Value::Null)
    };

    let t0 = Instant::now();
    let first = sample();
    std::thread::sleep(Duration::from_secs_f64(window_secs));
    let second = sample();
    let wall = t0.elapsed();

    let cpu_pct = cpu_percent(&first, &second, wall);

    if json_output {
        let mut out = second;
        if let Some(obj) = out.as_object() {
            let mut obj = obj.clone();
            obj.insert("cpu_pct".into(), json!(cpu_pct));
            obj.insert("sample_window_secs".into(), json!(wall.as_secs_f64()));
            out = Value::Object(obj);
        }
        println!("{out}");
    } else {
        print_perf_summary(&second, cpu_pct, wall);
    }
    std::process::exit(0);
}

/// CPU% over the window: (user_us_delta + system_us_delta) / wall_us * 100.
/// Reported as a fraction of one core, so a single-threaded compositor's
/// main loop stays under ~100; a value near 0 means it idled.
fn cpu_percent(first: &Value, second: &Value, wall: Duration) -> Option<f64> {
    let u0 = first.get("cpu_user_us").and_then(Value::as_u64)?;
    let s0 = first.get("cpu_system_us").and_then(Value::as_u64)?;
    let u1 = second.get("cpu_user_us").and_then(Value::as_u64)?;
    let s1 = second.get("cpu_system_us").and_then(Value::as_u64)?;
    let delta_us = u1.saturating_sub(u0) + s1.saturating_sub(s0);
    let wall_us = wall.as_micros() as f64;
    if wall_us <= 0.0 {
        return Some(0.0);
    }
    Some(delta_us as f64 / wall_us * 100.0)
}

fn print_perf_summary(snap: &Value, cpu_pct: Option<f64>, wall: Duration) {
    let mib = |key: &str| {
        snap.get(key)
            .and_then(Value::as_u64)
            .map(|b| b as f64 / (1024.0 * 1024.0))
    };
    let bool_of = |key: &str| snap.get(key).and_then(Value::as_bool).unwrap_or(false);
    let pss = mib("pss_bytes");
    let rss = mib("rss_bytes");
    let threads = snap.get("threads").and_then(Value::as_u64);
    let visible = snap.get("visible_windows").and_then(Value::as_u64);
    let textures = snap.get("tide_texture_estimate");
    let outputs = snap.get("outputs").and_then(Value::as_array);

    println!("TideWM perf over {:.2}s", wall.as_secs_f64());
    if let Some(pss) = pss {
        println!("  PSS:        {:>7.1} MiB", pss);
    }
    if let Some(rss) = rss {
        println!("  RSS:        {:>7.1} MiB", rss);
    }
    if let Some(cpu) = cpu_pct {
        println!("  CPU:        {:>7.1}%  (of one core)", cpu);
    }
    if let Some(t) = threads {
        println!("  threads:    {:>7}", t);
    }
    println!(
        "  backend:    {:>7}   engine: {}   profile: {}",
        snap.get("backend").and_then(Value::as_str).unwrap_or("?"),
        snap.get("spatial_engine")
            .and_then(Value::as_str)
            .unwrap_or("?"),
        snap.get("profile").and_then(Value::as_str).unwrap_or("?"),
    );
    println!(
        "  identity:   water_effects={} builtin_wallpaper={} animating={}",
        bool_of("water_effects"),
        bool_of("builtin_wallpaper"),
        bool_of("animation_active"),
    );
    if let Some(v) = visible {
        println!("  visible windows: {}", v);
    }
    if let Some(textures) = textures {
        let texture_mib = |key: &str| {
            textures
                .get(key)
                .and_then(Value::as_u64)
                .map(|bytes| bytes as f64 / (1024.0 * 1024.0))
                .unwrap_or(0.0)
        };
        println!(
            "  Tide textures: {:.1} MiB  (backdrops {:.1}, wallpaper {:.1}, caustics {:.1}, transition {:.1})",
            texture_mib("bytes"),
            texture_mib("backdrop_bytes"),
            texture_mib("wallpaper_bytes"),
            texture_mib("caustics_bytes"),
            texture_mib("workspace_transition_bytes"),
        );
    }
    if let Some(outputs) = outputs {
        for o in outputs {
            let hz = o.get("refresh_hz").and_then(Value::as_f64).unwrap_or(0.0);
            let scale = o.get("scale").and_then(Value::as_f64).unwrap_or(0.0);
            let w = o.get("logical_width").and_then(Value::as_i64).unwrap_or(0);
            let h = o.get("logical_height").and_then(Value::as_i64).unwrap_or(0);
            println!(
                "  output {}: {}x{} @ {:.1} Hz  scale {:.2}  {}",
                o.get("name").and_then(Value::as_str).unwrap_or("?"),
                w,
                h,
                hz,
                scale,
                o.get("transform").and_then(Value::as_str).unwrap_or("?"),
            );
        }
    }
    println!();
    println!(
        "(Texture figures exclude client buffers/driver overhead; GPU-busy% needs a vendor tool.)"
    );
}

fn fail(msg: &str) -> ! {
    eprintln!("tidectl: {msg}");
    std::process::exit(1);
}

/// `tidectl subscribe [event...]`: opens the long-lived subscribe mode and
/// prints one JSON line per matching event until the compositor closes the
/// connection. Event names are the IPC `events` array entries (`window`,
/// `workspace`, `focus`, `urgent`, `depth`, `config`); with no arguments
/// every kind is subscribed. A stale socket file (TideWM gone without a
/// clean drop) is removed and auto-discovery runs again, same as the
/// one-shot path. An explicit `--socket` is never removed or replaced.
fn cmd_subscribe(socket_path: &Path, auto_discovered: bool, events: &[String]) -> ! {
    let request = if events.is_empty() {
        json!({ "request": "subscribe" })
    } else {
        json!({ "request": "subscribe", "events": events })
    };
    let (stream, connected_path) = match connect_with_timeout(socket_path) {
        Ok(stream) => (stream, socket_path.to_path_buf()),
        Err(e) if auto_discovered && e.kind() == std::io::ErrorKind::ConnectionRefused => {
            // Mirrors the one-shot path's stale-socket dance: remove only
            // an auto-discovered stale entry, then discover again because
            // another live TideWM instance may use a different path.
            let _ = std::fs::remove_file(socket_path);
            let retry_path = find_socket().unwrap_or_else(|msg| fail(&msg));
            let stream = connect_with_timeout(&retry_path).unwrap_or_else(|e| {
                fail(&format!(
                    "failed to connect to {}: {e}",
                    retry_path.display()
                ))
            });
            (stream, retry_path)
        }
        Err(e) => fail(&format!(
            "failed to connect to {}: {e}",
            socket_path.display()
        )),
    };
    stream
        .set_read_timeout(Some(IPC_IO_TIMEOUT))
        .unwrap_or_else(|e| fail(&format!("failed to set socket read timeout: {e}")));
    stream
        .set_write_timeout(Some(IPC_IO_TIMEOUT))
        .unwrap_or_else(|e| fail(&format!("failed to set socket write timeout: {e}")));
    let mut write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => fail(&format!("failed to clone socket: {e}")),
    };
    let mut payload = serde_json::to_vec(&request).unwrap_or_else(|e| fail(&format!("{e}")));
    payload.push(b'\n');
    write_stream
        .write_all(&payload)
        .unwrap_or_else(|e| fail(&format!("failed to write request: {e}")));

    let mut reader = BufReader::new(stream);
    let mut ack = Vec::new();
    match read_bounded_line(&mut reader, &mut ack, MAX_SUBSCRIPTION_RECORD_BYTES) {
        Ok(0) => fail("no response from TideWM (is it running?)"),
        Err(e) => fail(&format!("failed to read subscription response: {e}")),
        Ok(_) => {}
    }
    let ack: Value = serde_json::from_slice(&ack).unwrap_or_else(|_| {
        fail("unrecognized response from TideWM");
    });
    if !ack.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let err = ack
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("subscribe refused");
        fail(err);
    }

    // The handshake is bounded, but a healthy subscription is expected to
    // sit idle indefinitely between events. Keep only the per-record size
    // bound for the streaming phase.
    reader
        .get_ref()
        .set_read_timeout(None)
        .unwrap_or_else(|e| fail(&format!("failed to clear socket read timeout: {e}")));

    // Streaming: one JSON line per event, echoed verbatim. Exit when the
    // compositor goes away (socket EOF), so a supervisor can restart us.
    let mut line = Vec::new();
    loop {
        let read = read_bounded_line(&mut reader, &mut line, MAX_SUBSCRIPTION_RECORD_BYTES)
            .unwrap_or_else(|e| fail(&format!("read error on {}: {e}", connected_path.display())));
        if read == 0 {
            break;
        }
        std::str::from_utf8(&line).unwrap_or_else(|_| fail("non-UTF-8 event from TideWM"));
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&line)
            .and_then(|()| stdout.flush())
            .unwrap_or_else(|e| fail(&format!("failed to write event: {e}")));
    }
    std::process::exit(0);
}

/// Reads one newline-delimited record without letting `BufRead::read_line`
/// grow its destination indefinitely. The cap includes the trailing newline.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    destination: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<usize> {
    destination.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(destination.len());
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if destination.len().saturating_add(take) > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("IPC record exceeds {cap}-byte limit"),
            ));
        }
        destination.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(destination.len());
        }
    }
}

fn read_bounded_to_end<R: Read>(reader: R, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .take((cap as u64).saturating_add(1))
        .read_to_end(&mut buf)?;
    if buf.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("IPC response exceeds {cap}-byte limit"),
        ));
    }
    Ok(buf)
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
        "eval" if !rest.is_empty() => Ok(json!({
            "request": "eval",
            "expression": rest.join(" ")
        })),
        "batch" if !rest.is_empty() => Ok(json!({ "request": "batch", "actions": rest })),
        "action" if !rest.is_empty() => Ok(action_request(&rest.join(" "))),
        "workspace" if !rest.is_empty() => {
            Ok(action_request(&format!("workspace:{}", rest.join(" "))))
        }
        "move-to-workspace" if !rest.is_empty() => Ok(action_request(&format!(
            "move-to-workspace:{}",
            rest.join(" ")
        ))),
        "swap-workspaces" if !rest.is_empty() => Ok(action_request(&format!(
            "swap-workspaces:{}",
            rest.join(" ")
        ))),
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

/// `UnixStream` has read/write timeouts but no connect timeout. A blocking
/// AF_UNIX connect can still wait behind a full listener backlog, so perform
/// just that syscall on a worker and bound how long the CLI waits for it.
/// Every caller exits the process on a timeout; a kernel-stuck worker cannot
/// outlive the timed-out `tidectl` command.
fn connect_with_timeout(socket_path: &Path) -> std::io::Result<UnixStream> {
    let path = socket_path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("tidectl-connect".into())
        .spawn(move || {
            let _ = tx.send(UnixStream::connect(path));
        })?;

    match rx.recv_timeout(IPC_IO_TIMEOUT) {
        Ok(result) => {
            let _ = worker.join();
            result
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "IPC connection did not complete within {} seconds",
                IPC_IO_TIMEOUT.as_secs()
            ),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = worker.join();
            Err(std::io::Error::other("IPC connection worker stopped"))
        }
    }
}

fn send_request(socket_path: &Path, request: &Value) -> std::io::Result<Value> {
    let mut stream = connect_with_timeout(socket_path)?;
    stream.set_read_timeout(Some(IPC_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_IO_TIMEOUT))?;

    let mut payload = serde_json::to_vec(request).map_err(std::io::Error::other)?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let buf = read_bounded_to_end(stream, MAX_RESPONSE_BYTES)?;
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
        "eval" => println!("{}", serde_json::to_string_pretty(data).unwrap_or_default()),
        _ => println!("ok"),
    }
}

fn print_outputs(data: &Value) {
    let Some(outputs) = data.as_array() else {
        return;
    };
    for o in outputs {
        let name = o.get("name").and_then(Value::as_str).unwrap_or("?");
        let size = o.get("size").and_then(Value::as_array);
        let (w, h) = size
            .and_then(|s| Some((s.first()?.as_i64()?, s.get(1)?.as_i64()?)))
            .unwrap_or((0, 0));
        let refresh_mhz = o.get("refresh_mhz").and_then(Value::as_i64).unwrap_or(0);
        let scale = o.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
        let transform = o.get("transform").and_then(Value::as_str).unwrap_or("?");
        let workspace = o
            .get("active_workspace")
            .and_then(Value::as_u64)
            .unwrap_or(0);
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
    let Some(workspaces) = data.as_array() else {
        return;
    };
    for w in workspaces {
        let output = w.get("output").and_then(Value::as_str).unwrap_or("?");
        let workspace = w.get("workspace").and_then(Value::as_u64).unwrap_or(0);
        let active = w.get("active").and_then(Value::as_bool).unwrap_or(false);
        let count = w.get("window_count").and_then(Value::as_u64).unwrap_or(0);
        let marker = if active { "*" } else { " " };
        let place = scratchpad_label(w).unwrap_or_else(|| format!("workspace={workspace}"));
        println!("{marker} {output}  {place}  windows={count}");
    }
}

fn print_windows(data: &Value) {
    let Some(windows) = data.as_array() else {
        return;
    };
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
    let place = scratchpad_label(w).unwrap_or_else(|| {
        let workspace = w
            .get("workspace")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".to_string());
        format!("workspace={workspace}")
    });
    let mut flags = Vec::new();
    for (key, label) in [
        ("floating", "floating"),
        ("pinned", "pinned"),
        ("pseudo_tiled", "pseudo-tiled"),
        ("fullscreen", "fullscreen"),
        ("maximized", "maximized"),
        ("urgent", "urgent"),
        ("focused", "focused"),
    ] {
        if w.get(key).and_then(Value::as_bool).unwrap_or(false) {
            flags.push(label);
        }
    }
    let flags = if flags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", flags.join(", "))
    };
    println!("{app_id}  \"{title}\"  output={output}  {place}{flags}");
}

/// A scratchpad's synthetic workspace number (0, or counting down from
/// `u32::MAX`) is meaningless to a reader; the server sends the name
/// alongside it, so show that instead when present.
fn scratchpad_label(w: &Value) -> Option<String> {
    let name = w.get("scratchpad")?.as_str()?;
    Some(if name.is_empty() {
        "scratchpad".to_string()
    } else {
        format!("scratchpad:{name}")
    })
}

fn print_help() {
    println!(
        r#"tidectl - control interface for a running TideWM

USAGE:
    tidectl <query>
    tidectl <action>
    tidectl subscribe [event...]

QUERIES:
    outputs             list outputs (name, mode, scale, position, transform, active workspace)
    workspaces          list known workspaces (output, number, active, window count)
    windows             list currently mapped windows
    focused-window       (alias: focused) the currently focused window, if any
    active-submap        the currently active `submap <name> {{ }}` block, if any

STREAMING:
    subscribe [event...]   long-lived: print one JSON line per event
                        ({{"event": "...", "data": ...}}) until TideWM exits.
                        Event names: window, workspace, focus, urgent,
                        depth, config. No names subscribes to all. Use
                        --socket to point at a specific compositor.

DIAGNOSTICS:
    doctor              run quick health checks (PASS/WARN/FAIL/SKIP)
    doctor --json       same checks as machine-readable JSON
    perf [--window <s>] sample the compositor's RAM/CPU and render state
                        over a window (default 3s); --json for raw output.
                        No GPU-busy% (not portable); use a vendor tool.
    report [--output <path>]   write a full diagnostic report file (default
                        tidewm-report.txt) for attaching to a GitHub issue;
                        embeds the doctor quick check, stays compact unless
                        problems were detected

ACTIONS:
    Any action a `bind` accepts in config.wave works here too, e.g.:
        close-window, toggle-floating, toggle-fullscreen, toggle-pin,
        toggle-scratchpad, move-to-scratchpad, toggle-pseudo-tile,
        toggle-float-ambient,
        raise-window, lower-window, focus-urgent, toggle-dpms,
        cycle-focus, focus-left/right/up/down, swap-left/right/up/down,
        group-left/right/up/down, ungroup, cycle-tab-next/prev, quit,
        submap:<name>, exit-submap, layout:bsp, layout:master,
        master-grow, master-shrink, resize-left/right/up/down, toggle-overview

    A few space-separated shorthands, equivalent to the colon syntax above:
        tidectl workspace <N>              same as "workspace:N"
        tidectl move-to-workspace <N>      same as "move-to-workspace:N"
        tidectl swap-workspaces <output>   same as "swap-workspaces:<output>"
        tidectl spawn <cmd...>             same as "spawn:<cmd...>"
        tidectl submap <name>              same as "submap:<name>"
        tidectl action <string>            explicit passthrough
        tidectl batch <action>...          validate then execute up to 128 actions

FLAGS:
    --json, -j        print the raw JSON response instead of a formatted view
    --socket <path>   connect to this socket instead of auto-discovering one
    -h, --help        show this help

By default tidectl uses $TIDEWM_SOCKET if set, otherwise the newest
tidewm-*.sock under $XDG_RUNTIME_DIR."#
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_is_delta_over_wall_as_one_core_fraction() {
        // 200_000 us of CPU over 1_000_000 us of wall == 20% of one core.
        let first = json!({ "cpu_user_us": 1_000_000u64, "cpu_system_us": 500_000u64 });
        let second = json!({ "cpu_user_us": 1_150_000u64, "cpu_system_us": 550_000u64 });
        let wall = Duration::from_secs(1);
        assert_eq!(cpu_percent(&first, &second, wall), Some(20.0));
    }

    #[test]
    fn cpu_percent_zero_when_idle() {
        let first = json!({ "cpu_user_us": 42u64, "cpu_system_us": 7u64 });
        let second = json!({ "cpu_user_us": 42u64, "cpu_system_us": 7u64 });
        assert_eq!(
            cpu_percent(&first, &second, Duration::from_secs(2)),
            Some(0.0)
        );
    }

    #[test]
    fn cpu_percent_none_when_fields_missing() {
        let first = json!({});
        let second = json!({ "cpu_user_us": 10u64, "cpu_system_us": 0u64 });
        assert_eq!(cpu_percent(&first, &second, Duration::from_secs(1)), None);
    }

    #[test]
    fn bounded_line_stops_at_newline_and_preserves_the_next_record() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"first\nsecond\n"));
        let mut line = Vec::new();

        assert_eq!(read_bounded_line(&mut reader, &mut line, 6).unwrap(), 6);
        assert_eq!(line, b"first\n");
        assert_eq!(read_bounded_line(&mut reader, &mut line, 7).unwrap(), 7);
        assert_eq!(line, b"second\n");
    }

    #[test]
    fn bounded_line_rejects_a_record_larger_than_its_cap() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"oversized\n"));
        let mut line = Vec::new();
        let err = read_bounded_line(&mut reader, &mut line, 9).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn bounded_response_accepts_exact_cap_and_rejects_one_more_byte() {
        assert_eq!(
            read_bounded_to_end(std::io::Cursor::new(b"1234"), 4).unwrap(),
            b"1234"
        );
        let err = read_bounded_to_end(std::io::Cursor::new(b"12345"), 4).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
