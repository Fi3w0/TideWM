//! xwayland-satellite integration.
//!
//! X11 apps run through `xwayland-satellite` (a separate process) rather
//! than TideWM implementing an in-process X11 window manager via Smithay's
//! `XwmHandler`. Satellite does the X11 window-manager work itself and
//! presents X11 clients to TideWM as ordinary Wayland `xdg_shell` surfaces,
//! so nothing elsewhere in the compositor needs to know an X11 client is
//! involved at all. Matches how niri and driftwm both integrate XWayland
//! (see AGENT.md); TideWM's per-output tiling trees have no shared global
//! coordinate system for an in-process X11 WM to plug into anyway.
//!
//! Spawned eagerly at startup rather than on first X11 connection: the
//! on-demand `-listenfd` handoff (pre-bind the X11 socket, hand the FD to
//! satellite when a client connects) has a documented interop bug with
//! Xwayland 24.x and multi-layout XKB configs (the queued connection races
//! Xwayland's keyboard init against the `wl_keyboard.keymap` event). Eager
//! "vanilla" mode -- satellite binds its own socket on startup -- sidesteps
//! that race by construction, at the cost of a satellite process (~30MB)
//! resident even if no X11 client ever runs.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const MAX_DISPLAY: u32 = 50;

/// How long the `--test-listenfd-support` probe may take before we give up
/// on the binary entirely.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a freshly spawned satellite may take to create its X11 socket
/// before we declare that display attempt failed and move on.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

const POLL_INTERVAL: Duration = Duration::from_millis(20);

pub struct Satellite {
    /// PID of the spawned child, kept so the compositor can tell an X11
    /// window apart from a native Wayland one (`rule { xwayland = ... }`)
    /// by comparing a surface's client PID against this one -- every X11
    /// app arrives as one of satellite's own Wayland surfaces.
    pub pid: u32,
}

/// Spawn `xwayland-satellite :N` eagerly and export `DISPLAY=:N` for every
/// process spawned afterward. Fails soft: any problem (binary missing, too
/// old, spawn error, display never coming up) logs a warning and returns
/// `None` so X11 apps just don't work rather than the compositor failing
/// to start.
///
/// `DISPLAY` is exported only after the X11 socket for the chosen display
/// actually exists and the child is still alive, so clients spawned right
/// after startup cannot race satellite's own socket setup, and a satellite
/// that dies immediately (for example because another X server grabbed the
/// display between our free-display check and its bind) leaves no poisoned
/// environment behind -- the next display number is tried instead.
pub fn setup(path: &str) -> Option<Satellite> {
    if !probe(path) {
        return None;
    }

    for display in 0..MAX_DISPLAY {
        if display_in_use(display) {
            continue;
        }
        let display_name = format!(":{display}");

        let mut child = match Command::new(path)
            .arg(&display_name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                tracing::warn!(%err, path, "Failed to spawn xwayland-satellite; X11 apps won't work");
                return None;
            }
        };

        match wait_until_ready(&mut child, display) {
            Ok(()) => {
                let pid = child.id();
                tracing::info!(pid, display = %display_name, "Spawned xwayland-satellite");
                crate::track_child(child);
                std::env::set_var("DISPLAY", &display_name);
                return Some(Satellite { pid });
            }
            Err(reason) => {
                tracing::warn!(
                    display = %display_name,
                    reason,
                    "xwayland-satellite did not bring up its display; trying the next one"
                );
            }
        }
    }

    tracing::warn!("no usable X11 display number found, disabling xwayland-satellite");
    None
}

/// Wait until the satellite's X11 socket exists while the child is still
/// alive. A child that exits first (bind failure on a raced display, crash)
/// and a socket that never appears are both failures; the child is reaped
/// or killed-and-reaped so nothing is left behind either way.
fn wait_until_ready(child: &mut Child, display: u32) -> Result<(), &'static str> {
    let socket = format!("/tmp/.X11-unix/X{display}");
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::debug!(?status, "xwayland-satellite exited during startup");
                return Err("child exited before creating its socket");
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(%err, "error polling xwayland-satellite startup");
                return Err("error polling child");
            }
        }
        if std::path::Path::new(&socket).exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            kill_and_reap(child);
            return Err("timed out waiting for the X11 socket");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn kill_and_reap(child: &mut Child) {
    if let Err(err) = child.kill() {
        tracing::debug!(%err, "error killing xwayland-satellite child");
    }
    if let Err(err) = child.wait() {
        tracing::debug!(%err, "error reaping xwayland-satellite child");
    }
}

/// Poll a child with a deadline instead of blocking on `wait()` forever: a
/// wedged binary must not stall compositor startup. Kills and reaps the
/// child on timeout.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(%err, "error waiting for xwayland-satellite child");
                return None;
            }
        }
        if Instant::now() >= deadline {
            kill_and_reap(child);
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Cheap existence/version check: every `xwayland-satellite` since 0.7
/// recognizes `--test-listenfd-support` and exits zero on it. We don't
/// actually use listenfd downstream (see module docs), just its presence
/// as a version marker.
fn probe(path: &str) -> bool {
    let mut child = match Command::new(path)
        .args([":0", "--test-listenfd-support"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(
                %err, path,
                "xwayland-satellite not found -- X11 apps disabled \
                 (install xwayland-satellite, or set xwayland.enabled = false to silence)"
            );
            return false;
        }
    };

    match wait_with_timeout(&mut child, PROBE_TIMEOUT) {
        Some(status) if status.success() => true,
        Some(_) => {
            tracing::warn!(
                path,
                "xwayland-satellite is too old (need >= 0.7) -- X11 apps disabled"
            );
            false
        }
        None => {
            tracing::warn!(
                path,
                "xwayland-satellite probe did not exit in time -- X11 apps disabled"
            );
            false
        }
    }
}

/// Display-number pre-filter: both the lock file and the unix socket absent
/// means *probably* free. This is only a hint, not a guarantee -- another X
/// server can claim the display between this check and satellite's bind.
/// `wait_until_ready` detects that loss after the fact and `setup` retries
/// the next number, so the race costs one failed spawn instead of leaving
/// a dead `DISPLAY` exported.
fn display_in_use(n: u32) -> bool {
    std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists()
        || std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
}
