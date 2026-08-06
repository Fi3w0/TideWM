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

use std::process::{Command, Stdio};

const MAX_DISPLAY: u32 = 50;

pub struct Satellite {
    /// PID of the spawned child, kept so the compositor can tell an X11
    /// window apart from a native Wayland one (`rule { xwayland = ... }`)
    /// by comparing a surface's client PID against this one -- every X11
    /// app arrives as one of satellite's own Wayland surfaces.
    pub pid: u32,
}

/// Spawn `xwayland-satellite :N` eagerly and export `DISPLAY=:N` for every
/// process spawned afterward. Fails soft: any problem (binary missing, too
/// old, spawn error) logs a warning and returns `None` so X11 apps just
/// don't work rather than the compositor failing to start.
pub fn setup(path: &str) -> Option<Satellite> {
    if !probe(path) {
        return None;
    }

    let display = match find_free_display() {
        Some(n) => n,
        None => {
            tracing::warn!("no free X11 display number found, disabling xwayland-satellite");
            return None;
        }
    };
    let display_name = format!(":{display}");

    let child = match Command::new(path)
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

    let pid = child.id();
    tracing::info!(pid, display = %display_name, "Spawned xwayland-satellite");
    crate::track_child(child);
    std::env::set_var("DISPLAY", &display_name);

    Some(Satellite { pid })
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

    match child.wait() {
        Ok(status) if status.success() => true,
        Ok(_) => {
            tracing::warn!(
                path,
                "xwayland-satellite is too old (need >= 0.7) -- X11 apps disabled"
            );
            false
        }
        Err(err) => {
            tracing::warn!(%err, "error waiting for xwayland-satellite probe");
            false
        }
    }
}

/// Display number with both the lock file and the unix socket absent --
/// either artifact present means another X server holds it.
fn find_free_display() -> Option<u32> {
    (0..MAX_DISPLAY).find(|&n| !display_in_use(n))
}

fn display_in_use(n: u32) -> bool {
    std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists()
        || std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
}
