#![allow(irrefutable_let_patterns)]

mod handlers;

#[cfg(feature = "accessibility")]
mod accessibility;
mod backend;
mod capture;
mod cursor;
mod frame_clock;
mod grabs;
#[cfg(feature = "screencast")]
mod screencast;
mod session_env;
mod tide_core;
mod visual;
mod xwayland;

pub(crate) use tide_core::{
    classic_depth, config, ipc, layout, ocean, output_layout, placement, redraw, snap, state, wave,
    waves, window_size,
};
#[cfg(feature = "screencast")]
pub(crate) use visual::source_picker;
pub(crate) use visual::{
    animation, backdrop, buoyancy, cascade_transition, caustics, compass, currents, decoration,
    depth, depth_deck, depth_transition, error_overlay, float_physics, frost_glass, minimap,
    ocean_canvas, overview, ripple, shader_effect, shadow, sway, swim, tab_strip, text, toast,
    ui_theme, viscosity, wallpaper, water_glass, welcome, window_animation, workspace_transition,
};

use std::{
    collections::HashMap,
    ffi::OsStr,
    mem::MaybeUninit,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, ExitStatus, Output},
    sync::{mpsc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

use smithay::reexports::{
    calloop::{
        self,
        signals::{Signal, Signals},
        EventLoop,
    },
    wayland_server::Display,
};
pub use tide_core::state::Smallvil;

/// Children started by TideWM and not synchronously waited by their call
/// site. Keeping the `Child` handles lets the SIGCHLD event source below reap
/// them with `try_wait`; dropping a `Child` without waiting would leave a
/// zombie until the compositor exits.
static SPAWNED_CHILDREN: Mutex<Vec<Child>> = Mutex::new(Vec::new());

pub(crate) fn track_child(child: Child) {
    SPAWNED_CHILDREN.lock().unwrap().push(child);
}

/// Builds a child command whose pre-exec hook clears TideWM's inherited
/// signal mask. `calloop::Signals` blocks handled signals in this process and
/// every thread/child created afterwards; without this hook, ordinary apps
/// would inherit blocked SIGINT/SIGTERM/SIGHUP and ignore normal shutdown.
pub(crate) fn child_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    // `env { }` changes made by a config reload after startup.
    session_env::apply_to(&mut command);
    // Safety: after fork and before exec this hook calls only libc signal-mask
    // functions over stack-owned plain data. It captures nothing and performs
    // no work beyond replacing the signal mask or returning that libc error.
    unsafe {
        command.pre_exec(clear_child_signal_mask);
    }
    command
}

fn clear_child_signal_mask() -> std::io::Result<()> {
    let mut empty = MaybeUninit::<libc::sigset_t>::uninit();
    // Safety: `sigemptyset` initializes the pointed-to sigset on success.
    if unsafe { libc::sigemptyset(empty.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Safety: the successful call above initialized `empty` completely.
    let empty = unsafe { empty.assume_init() };
    // Safety: both pointers are valid for the call; the previous mask is not
    // needed in the soon-to-exec child. pthread_sigmask returns the errno
    // value directly rather than storing it in `errno`.
    let result = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(result))
    }
}

fn reap_spawned_children(state: &mut Smallvil) {
    SPAWNED_CHILDREN
        .lock()
        .unwrap()
        .retain_mut(|child| match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                let pid = child.id();
                let satellite_exited = i32::try_from(pid)
                    .ok()
                    .is_some_and(|pid| state.xwayland_satellite_pid == Some(pid));
                if satellite_exited {
                    state.xwayland_satellite_pid = None;
                    if state.xwayland_display.as_deref().is_some_and(|display| {
                        std::env::var("DISPLAY").ok().as_deref() == Some(display)
                    }) {
                        std::env::remove_var("DISPLAY");
                    }
                    state.xwayland_display = None;
                    tracing::warn!(
                        pid,
                        ?status,
                        "xwayland-satellite exited; X11 support is unavailable"
                    );
                } else {
                    tracing::debug!(pid, ?status, "Spawned child exited");
                }
                false
            }
            Err(err) => {
                tracing::warn!(pid = child.id(), %err, "Failed to reap spawned child");
                false
            }
        });
}

/// Spawns `cmd`. Arguments split on whitespace, with POSIX-style quoting so
/// an argument can contain spaces: `'...'` is literal, `"..."` honors `\"`
/// and `\\`, and a backslash outside quotes escapes the next character
/// (`kitty sh -c 'fastfetch; sleep 5'`). Deliberately not a shell -- no
/// `$` expansion, globs, pipes or redirection, and no injection surprise
/// from untrusted config content. An unterminated quote falls back to the
/// plain whitespace split, so a config that relied on a literal quote keeps
/// working. Shared by every spawn call site in the compositor
/// (`-s`/`--spawn` below, `config.spawn_at_startup`, and `Action::Spawn`
/// in `input.rs`) so they all get the same argument support for free.
pub(crate) fn spawn(cmd: &str) -> std::io::Result<()> {
    let parts =
        split_command(cmd).unwrap_or_else(|| cmd.split_whitespace().map(str::to_string).collect());
    let mut parts = parts.into_iter();
    let program = parts
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;
    let child = child_command(&program).args(parts).spawn()?;
    track_child(child);
    Ok(())
}

/// Splits a spawn command into argv (see [`spawn`]). `None` for an
/// unterminated quote or a trailing lone backslash.
fn split_command(cmd: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_arg = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => current.push(c),
                    }
                }
            }
            '"' => {
                in_arg = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\') => current.push(c),
                            c => {
                                current.push('\\');
                                current.push(c);
                            }
                        },
                        c => current.push(c),
                    }
                }
            }
            '\\' => {
                in_arg = true;
                current.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                in_arg = true;
                current.push(c);
            }
        }
    }
    if in_arg {
        args.push(current);
    }
    Some(args)
}

/// Applies `[env]` (`config.env`) to this process via `set_var`, before the
/// backend starts -- so e.g. an `XCURSOR_THEME` override actually reaches
/// `cursor::Theme::load()` (called from `init_udev`, right after this),
/// not just processes TideWM spawns later. Called separately from, and
/// before, `export_session_environment` below, which needs the real
/// `WAYLAND_DISPLAY` the backend sets up and so can only run afterward.
/// Reload-time changes go through `session_env::update` instead.
fn apply_user_env(env: &std::collections::HashMap<String, String>) {
    session_env::apply_startup(env);
}

/// Publishes a reload's `env { }` changes to the systemd user manager and
/// the D-Bus activation environment, with explicit values (the process
/// environment is deliberately not rewritten after startup). Same
/// best-effort, never-blocking contract as the startup export.
pub(crate) fn export_env_change(change: session_env::EnvChange) {
    if change.is_empty() {
        return;
    }
    let spawned = thread::Builder::new()
        .name("tidewm-session-environment".to_string())
        .spawn(move || {
            let runner = ProcessSessionEnvironmentRunner;
            let mut tasks: Vec<(&str, Vec<String>)> = Vec::new();
            if !change.set.is_empty() {
                let mut args = vec!["--systemd".to_string()];
                args.extend(change.set.iter().map(|(k, v)| format!("{k}={v}")));
                tasks.push(("dbus-update-activation-environment", args));
            }
            if !change.unset.is_empty() {
                let mut args = vec!["--user".to_string(), "unset-environment".to_string()];
                args.extend(change.unset.iter().cloned());
                tasks.push(("systemctl", args));
            }
            for (program, args) in tasks {
                match runner.status(program, &args) {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        tracing::debug!(program, ?status, "Environment update exited non-zero")
                    }
                    Err(err) => {
                        tracing::debug!(%err, program, "Environment update command not available")
                    }
                }
            }
        });
    if let Err(err) = spawned {
        tracing::debug!(%err, "Failed to start environment update worker");
    }
}

const FOREIGN_COMPOSITOR_ENV: &[&str] = &[
    "HYPRLAND_INSTANCE_SIGNATURE",
    "SWAYSOCK",
    "I3SOCK",
    "NIRI_SOCKET",
    "WAYFIRE_SOCKET",
];

/// How long startup autostarts wait for the session environment to reach the
/// systemd user manager and the D-Bus activation environment. Typical is a
/// few milliseconds; the bound exists so that a wedged user bus still cannot
/// delay the session.
const SESSION_ENV_READY_TIMEOUT: Duration = Duration::from_secs(2);

/// Makes TideWM's identity authoritative for this process and everything it
/// spawns.  This matters especially for the nested backend: the process that
/// launches us belongs to the host desktop, so blindly retaining its session
/// variables makes tools such as fastfetch identify nested TideWM clients as
/// Hyprland and lets children accidentally address the host compositor's IPC.
/// `<version> (commit <hash>, <profile>, built <date>)` with each part
/// best-effort -- outside a git checkout the commit and date are omitted.
/// Shared in spirit with tidectl's own copy (separate binaries can't share
/// code without a lib target); keep the two in sync.
pub fn build_tag() -> String {
    let mut parts = vec![env!("CARGO_PKG_VERSION").to_string()];
    if let Some(commit) = option_env!("TIDEWM_GIT_COMMIT") {
        let dirty = option_env!("TIDEWM_GIT_DIRTY").is_some();
        parts.push(format!(
            "commit {commit}{}",
            if dirty { "-dirty" } else { "" }
        ));
    }
    parts.push(if cfg!(debug_assertions) {
        "debug build".to_string()
    } else {
        "release build".to_string()
    });
    if let Some(date) = option_env!("TIDEWM_BUILD_DATE") {
        parts.push(format!("built {date}"));
    }
    parts.join(", ")
}

fn configure_session_environment() {
    std::env::set_var("XDG_CURRENT_DESKTOP", "tidewm");
    std::env::set_var("XDG_SESSION_DESKTOP", "tidewm");
    std::env::set_var("DESKTOP_SESSION", "tidewm");
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    std::env::set_var("TIDEWM_VERSION", env!("CARGO_PKG_VERSION"));

    // Control sockets/signatures identify the *host* compositor. They are
    // never valid for TideWM's children, and keeping them is worse than a
    // cosmetic misidentification: a child could issue commands to the outer
    // desktop rather than this session.
    for key in FOREIGN_COMPOSITOR_ENV {
        std::env::remove_var(key);
    }
}

// Exports TideWM's graphical-session variables plus any `[env]` entries
// into the systemd user session and the D-Bus session-activation environment,
// so anything activated by either (a portal backend, a polkit agent) sees a
// real graphical session instead of whatever it inherited from before TideWM
// started (nothing, on a bare TTY login). `set_var` alone only affects this
// process's own children, not something already running or activated on demand
// later. These external commands are how sway, Hyprland, and niri get this to
// the rest of the session.
//
// `XDG_CURRENT_DESKTOP=tidewm` is a real decision, not a formality: no portal
// backend ships a matching profile for a brand-new compositor name out of the
// box, so screen-sharing/file-picker portals still need the user's own
// `xdg-desktop-portal` backend configured (same situation niri and sway users
// are in). Missing helpers are logged at `debug` and otherwise ignored, per
// this project's any-distro requirement.
trait SessionEnvironmentRunner: Send + 'static {
    fn status(&self, program: &str, args: &[String]) -> std::io::Result<ExitStatus>;
    fn output(&self, program: &str, args: &[String]) -> std::io::Result<Output>;
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
}

struct ProcessSessionEnvironmentRunner;

impl SessionEnvironmentRunner for ProcessSessionEnvironmentRunner {
    fn status(&self, program: &str, args: &[String]) -> std::io::Result<ExitStatus> {
        child_command(program).args(args).status()
    }

    fn output(&self, program: &str, args: &[String]) -> std::io::Result<Output> {
        child_command(program).args(args).output()
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

fn validated_session_environment_keys(env: &HashMap<String, String>) -> Vec<String> {
    let mut keys: Vec<String> = env
        .iter()
        .filter(|(key, value)| crate::config::validate_env_entry(key, value).is_ok())
        .map(|(key, _)| key.clone())
        .collect();
    keys.sort_unstable();
    keys
}

fn start_session_environment_worker(
    env: &HashMap<String, String>,
) -> std::io::Result<(JoinHandle<()>, mpsc::Receiver<()>)> {
    start_session_environment_worker_with_runner(
        validated_session_environment_keys(env),
        ProcessSessionEnvironmentRunner,
    )
}

fn start_session_environment_worker_with_runner<R>(
    env_keys: Vec<String>,
    runner: R,
) -> std::io::Result<(JoinHandle<()>, mpsc::Receiver<()>)>
where
    R: SessionEnvironmentRunner,
{
    let (ready, exported) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("tidewm-session-environment".to_string())
        .spawn(move || run_session_environment_tasks(&env_keys, &runner, ready))?;
    Ok((worker, exported))
}

fn run_session_environment_tasks<R>(env_keys: &[String], runner: &R, exported: mpsc::Sender<()>)
where
    R: SessionEnvironmentRunner,
{
    export_session_environment(env_keys, runner);
    let recovery = restart_stale_portal_frontend(runner);

    // Readiness is signalled only after portal recovery: a client that reads
    // portal settings as soon as it is spawned must never be handed a
    // frontend that is being restarted underneath it. `--no-block` means this
    // does not wait for the unit to become healthy, only that the recovery is
    // queued.
    //
    // A failed send means the compositor stopped waiting (see
    // SESSION_ENV_READY_TIMEOUT and its `warn`), so spawns may already have
    // activated a backend against a half-published environment. Only a
    // manager that exposes unit state can be repaired; when the stale-frontend
    // pass above already repaired it, there is nothing left to do.
    if exported.send(()).is_err() && recovery == PortalRecovery::Idle {
        tracing::info!(
            "Session environment export finished after startup stopped waiting; clearing failed portal backends once"
        );
        reset_failed_portal_backends(runner);
    }
}

fn export_session_environment<R>(env_keys: &[String], runner: &R)
where
    R: SessionEnvironmentRunner,
{
    let mut vars: Vec<&str> = vec![
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
        "XDG_SESSION_TYPE",
        "TIDEWM_VERSION",
    ];
    vars.extend(env_keys.iter().map(String::as_str));

    for (program, mut args) in [
        ("dbus-update-activation-environment", vec!["--systemd"]),
        ("systemctl", vec!["--user", "import-environment"]),
    ] {
        args.extend_from_slice(&vars);
        let args: Vec<String> = args.into_iter().map(str::to_string).collect();
        match runner.status(program, &args) {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::debug!(
                program,
                ?status,
                "Session environment export exited non-zero"
            ),
            Err(err) => {
                tracing::debug!(%err, program, "Session environment export command not available")
            }
        }
    }

    // A persistent user manager may still carry control sockets from the
    // previous graphical login. D-Bus has no remove operation for activation
    // variables, so replace those with empty values there; then remove them
    // completely from systemd's own manager environment. This only runs for
    // standalone TideWM -- a nested session must never edit its host's state.
    let empty_foreign_vars: Vec<String> = FOREIGN_COMPOSITOR_ENV
        .iter()
        .map(|key| format!("{key}="))
        .collect();
    match runner.status("dbus-update-activation-environment", &empty_foreign_vars) {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::debug!(
            ?status,
            "Foreign compositor DBus environment cleanup exited non-zero"
        ),
        Err(err) => {
            tracing::debug!(%err, "Foreign compositor DBus environment cleanup unavailable")
        }
    }
    let systemd_cleanup_args: Vec<String> = ["--user", "unset-environment"]
        .into_iter()
        .chain(FOREIGN_COMPOSITOR_ENV.iter().copied())
        .map(str::to_string)
        .collect();
    match runner.status("systemctl", &systemd_cleanup_args) {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::debug!(
            ?status,
            "Foreign compositor systemd environment cleanup exited non-zero"
        ),
        Err(err) => {
            tracing::debug!(%err, "Foreign compositor systemd environment cleanup unavailable")
        }
    }
}

/// A systemd `--user` manager commonly outlives a session switch (SDDM
/// "switch session", or any relogin that doesn't tear the user manager
/// down) -- so `xdg-desktop-portal.service` may already be running from a
/// *previous* login, with the previous desktop's `XDG_CURRENT_DESKTOP`
/// baked into its own process environment. The exports above only change
/// the activation environment for future activations; they cannot reach
/// into an already-running process. Confirmed live: a stale `Hyprland`
/// value on an already-running frontend silently routed every screencast
/// request (OBS included) to `xdg-desktop-portal-hyprland` instead of this
/// compositor's own backend, with no error anywhere on TideWM's side to
/// see -- it just looked like screencasting was broken. Restarting only
/// the frontend is enough: backends are re-resolved fresh, from the
/// now-correct activation environment, the next time it starts.
/// Outcome of the portal recovery pass, in terms the caller can act on.
///
/// Only systemd --user exposes unit state to clear (`failed`) and a frontend
/// to restart. Every other init, and a session with no per-user service
/// manager at all, activates portals purely through D-Bus, where the
/// activation environment is the only thing that matters and there is nothing
/// to inspect or repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortalRecovery {
    /// No per-user service manager to talk to.
    NoSessionManager,
    /// Manager reachable, nothing needed repairing.
    Idle,
    /// Manager reachable, failed state cleared and/or a restart queued.
    Repaired,
}

fn restart_stale_portal_frontend<R>(runner: &R) -> PortalRecovery
where
    R: SessionEnvironmentRunner,
{
    let show_args: Vec<String> = [
        "--user",
        "show",
        "-p",
        "MainPID",
        "--value",
        "xdg-desktop-portal.service",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let frontend_pid = runner
        .output("systemctl", &show_args)
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .filter(|&pid| pid != 0);

    // No unit to ask about: either systemd --user is not this session's
    // manager, or no frontend is running yet. Portals are D-Bus-activated
    // either way, so the exported activation environment is all they need and
    // there is no unit state to reset or restart.
    let Some(frontend_pid) = frontend_pid else {
        return PortalRecovery::NoSessionManager;
    };

    let fresh_identity = runner
        .read(Path::new(&format!("/proc/{frontend_pid}/environ")))
        .ok()
        .map(|environ| {
            environ
                .split(|&byte| byte == 0)
                .any(|entry| entry == b"XDG_CURRENT_DESKTOP=tidewm")
        });
    let frontend_start = runner
        .read(Path::new(&format!("/proc/{frontend_pid}/stat")))
        .ok()
        .and_then(|stat| proc_starttime(&stat));
    let own_start = runner
        .read(Path::new("/proc/self/stat"))
        .ok()
        .and_then(|stat| proc_starttime(&stat));

    // Identity alone cannot see a relog from TideWM into TideWM: that
    // frontend is tagged correctly, but the backend units it manages died
    // with the previous compositor and stay `failed`, so a client reading
    // portal settings then blocks until its own call times out (a real login
    // lost ~40s that way). A frontend that started before this compositor is
    // from a previous login either way.
    if !portal_frontend_stale(fresh_identity, frontend_start, own_start) {
        return PortalRecovery::Idle;
    }

    // Housekeeping: backends that died with the previous compositor are left
    // `failed`; clearing that keeps the unit list honest. It cannot cancel a
    // D-Bus activation still pending from the old frontend -- that is what
    // actually stalled a quick TideWM->TideWM relog, and the clean-shutdown
    // stop (`stop_session_portals`) is what prevents it.
    reset_failed_portal_backends(runner);

    // `--no-block` is load-bearing, not an optimization: this runs before the
    // event loop starts, and the restarted frontend's backend (e.g.
    // xdg-desktop-portal-gtk) is itself a Wayland client of *this*
    // compositor. A synchronous restart waits for the portal, the portal
    // waits for a compositor whose only thread is sitting in that wait --
    // a circular hang that froze the whole session (mouse, keyboard, VT
    // switch) on a real SDDM login. The restart's outcome only matters for
    // requests minutes later, so enqueueing the job is enough.
    let restart_args: Vec<String> = [
        "--user",
        "--no-block",
        "restart",
        "xdg-desktop-portal.service",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    match runner.status("systemctl", &restart_args) {
        Ok(status) if status.success() => tracing::info!(
            "Restarting xdg-desktop-portal.service: it or its backends belong to an earlier login"
        ),
        Ok(status) => tracing::debug!(?status, "Stale portal frontend restart exited non-zero"),
        Err(err) => tracing::debug!(%err, "Stale portal frontend restart unavailable"),
    }
    PortalRecovery::Repaired
}

/// Clears the failed state of portal backends so the next activation starts
/// them instead of replaying the failure they were left with.
///
/// This is the one repair that is inherently systemd-specific: `failed` is a
/// unit state, and only a manager that has units can be asked to forget it.
/// Inits without a per-user service manager simply have nothing to reset --
/// their portals are D-Bus-activated -- which is why this is only reached when
/// a manager answered the frontend query. Targeted on purpose: it touches
/// units already in `failed`, and `reset-failed` starts nothing.
fn reset_failed_portal_backends<R>(runner: &R)
where
    R: SessionEnvironmentRunner,
{
    let reset_args: Vec<String> = ["--user", "reset-failed", "*xdg-desktop-portal*.service"]
        .into_iter()
        .map(str::to_string)
        .collect();
    match runner.status("systemctl", &reset_args) {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::debug!(?status, "Portal backend reset exited non-zero"),
        Err(err) => tracing::debug!(%err, "Portal backend reset unavailable"),
    }
}

/// How long a clean shutdown waits for the portal units to stop. Stopping
/// takes milliseconds; the bound only keeps a wedged user bus from holding up
/// logout.
const PORTAL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Stops this session's portal frontend, then its backends, when TideWM
/// exits cleanly.
///
/// Without this the frontend outlives the compositor. Its backends (e.g.
/// xdg-desktop-portal-gtk) are Wayland clients of TideWM and die with it; the
/// frontend at once asks D-Bus to activate them again, the new backend exits
/// with `cannot open display:`, and dbus-daemon keeps that activation pending
/// for its whole service_start_timeout (120 s) because systemd never reports
/// the failure back. Every activation of the same backend joins that pending
/// one, so a relog inside those two minutes gets a frontend whose Settings,
/// AppChooser and Print proxies each time out after 25 s and are never
/// retried (measured: a shell 40 s late, `Settings.ReadAll` failing). Stopping
/// the frontend first leaves nothing to re-activate a backend into a dead
/// display. `stop` is a no-op for units that are not running, and without a
/// systemd --user manager there is nothing to stop.
fn stop_session_portals<R>(runner: &R)
where
    R: SessionEnvironmentRunner,
{
    for unit in [
        "xdg-desktop-portal.service",
        "*xdg-desktop-portal-*.service",
    ] {
        let args: Vec<String> = ["--user", "stop", unit]
            .into_iter()
            .map(str::to_string)
            .collect();
        match runner.status("systemctl", &args) {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::debug!(?status, unit, "Portal stop on shutdown exited non-zero"),
            Err(err) => tracing::debug!(%err, unit, "Portal stop on shutdown unavailable"),
        }
    }
}

/// Runs `stop_session_portals` on a worker and waits at most `timeout`.
/// Returns whether it finished; on timeout TideWM exits anyway and the worker
/// ends with the process.
fn stop_session_portals_bounded<R>(runner: R, timeout: Duration) -> bool
where
    R: SessionEnvironmentRunner,
{
    let (done, finished) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("tidewm-portal-shutdown".to_string())
        .spawn(move || {
            stop_session_portals(&runner);
            let _ = done.send(());
        });
    if let Err(err) = spawned {
        tracing::debug!(%err, "Failed to start portal shutdown worker");
        return false;
    }
    match finished.recv_timeout(timeout) {
        Ok(()) => true,
        Err(_) => {
            tracing::warn!(
                ?timeout,
                "Portal units did not stop in time; exiting anyway"
            );
            false
        }
    }
}

/// Start time of a process in clock ticks since boot (`/proc/<pid>/stat`
/// field 22). `comm` may contain spaces and parentheses, so parse from the
/// last `)`.
fn proc_starttime(stat: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(stat).ok()?;
    text.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Whether an already-running portal frontend has to be restarted before this
/// session uses it.
///
/// A mismatched identity is the loud case (another desktop's frontend routes
/// every request to that desktop's backend). Start time catches the quiet
/// one, where the identity is already `tidewm` because the previous login was
/// TideWM too, but the frontend and its dead backends predate this
/// compositor. Unknown values never trigger a restart: a frontend that is not
/// running yet is activated with the freshly exported environment anyway.
fn portal_frontend_stale(
    fresh_identity: Option<bool>,
    frontend_start: Option<u64>,
    own_start: Option<u64>,
) -> bool {
    match fresh_identity {
        Some(false) => true,
        Some(true) => match (frontend_start, own_start) {
            (Some(frontend), Some(own)) => frontend < own,
            _ => false,
        },
        None => false,
    }
}

/// Parsed CLI arguments (`-c`/`--config`, `-s`/`--spawn`; `-v`/`--version`
/// and `-h`/`--help` exit immediately from `parse_args` itself, since
/// neither needs the rest of `main` to run at all).
struct Args {
    spawn: Option<String>,
}

/// Must run before `Smallvil::new` (which calls `Config::load`) -- setting
/// `--config`'s override any later wouldn't affect the load that already
/// happened. Exits the process directly for `--version`/`--help` or a bad
/// invocation, rather than threading an exit code back through `main`.
fn parse_args() -> Args {
    let mut spawn = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-v" | "--version" => {
                println!("TideWM {} ({})", env!("CARGO_PKG_VERSION"), build_tag());
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-c" | "--config" => match args.next() {
                Some(path) => config::set_config_path_override(std::path::PathBuf::from(path)),
                None => fail("--config requires a path argument"),
            },
            "-s" | "--spawn" => match args.next() {
                Some(command) => spawn = Some(command),
                None => fail("--spawn requires a command argument"),
            },
            other => fail(&format!("unrecognized argument '{other}' (see --help)")),
        }
    }
    Args { spawn }
}

fn fail(msg: &str) -> ! {
    eprintln!("tidewm: {msg}");
    std::process::exit(1);
}

fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "{}",
        [
            format!("TideWM {version} -- a water-styled Wayland compositor"),
            String::new(),
            "USAGE:".to_string(),
            "    tidewm [OPTIONS]".to_string(),
            String::new(),
            "OPTIONS:".to_string(),
            "    -c, --config <path>    Use this config file instead of the default".to_string(),
            "    -s, --spawn <command>  Spawn one specific command at launch (no shell parsing)"
                .to_string(),
            "    -v, --version          Print version and exit".to_string(),
            "    -h, --help             Print this help and exit".to_string(),
        ]
        .join("\n")
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();

    // Log to stderr, not fmt()'s default stdout: a display-manager session
    // (SDDM) wires stdout to /dev/null and only stderr to
    // wayland-session.log, so the default silently discarded every line a
    // live session ever logged.
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();
    }

    let mut event_loop: EventLoop<'static, Smallvil> = EventLoop::try_new()?;

    // Create this before backend/optional-service initialization spawns any
    // threads. Threads inherit the signal mask, ensuring process-lifecycle
    // signals are delivered through this one event-loop source instead of to
    // an arbitrary worker. `child_command` clears the inherited mask before
    // every external exec.
    event_loop.handle().insert_source(
        Signals::new(&[
            Signal::SIGCHLD,
            Signal::SIGINT,
            Signal::SIGTERM,
            Signal::SIGHUP,
        ])?,
        |event, _, state| match event.signal() {
            Signal::SIGCHLD => reap_spawned_children(state),
            signal => {
                tracing::info!(?signal, "Stopping after process signal");
                state.loop_signal.stop();
            }
        },
    )?;

    let display: Display<Smallvil> = Display::new()?;
    let mut state = Smallvil::new(&mut event_loop, display);

    // Nested (WAYLAND_DISPLAY or DISPLAY set: there's a host compositor/X
    // server to open a window in) vs standalone TTY session, same
    // convention anvil and niri use for their own backend auto-selection.
    // Checked before `apply_user_env` so a stray WAYLAND_DISPLAY/DISPLAY in
    // `[env]` can't retroactively change which backend gets picked.
    let nested =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();

    apply_user_env(&state.config.env);

    if nested {
        state.backend_name = "winit";
        crate::backend::winit::init_winit(&mut event_loop, &mut state)?;
    } else {
        state.backend_name = "udev";
        crate::backend::udev::init_udev(&mut event_loop, &mut state)?;
    }

    // After backend init: both branches above have set TideWM's real
    // WAYLAND_DISPLAY (see `state.socket_name`), so it is now safe to make
    // our process-local identity authoritative before anything is spawned.
    configure_session_environment();

    // A nested compositor shares the host's systemd user manager and D-Bus
    // session. Importing wayland-N there would redirect subsequently
    // activated host services into this disposable test window. Standalone
    // TideWM owns the graphical session and should publish it normally.
    // Session-manager helpers can block on a wedged user bus. Keep their
    // exact ordering in one worker, but never make compositor readiness,
    // autostarts, or shutdown wait for them. Dropping this handle detaches
    // the worker; it is intentionally never joined.
    let (_session_environment_worker, session_env_exported) = if !nested {
        match start_session_environment_worker(&state.config.env) {
            Ok((worker, exported)) => (Some(worker), Some(exported)),
            Err(err) => {
                tracing::debug!(%err, "Failed to start session environment export worker");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    // Bound so it stays alive for the process lifetime; the config watcher
    // below is instead owned by `Smallvil` because reloads update its graph.
    let _satellite = if state.config.xwayland.enabled {
        crate::xwayland::setup(&state.config.xwayland.path)
    } else {
        None
    };
    state.xwayland_satellite_pid = _satellite.as_ref().map(|satellite| satellite.pid as i32);
    state.xwayland_display = _satellite
        .as_ref()
        .map(|satellite| satellite.display_name.clone());

    // After backend init, not inside `Smallvil::new`: the DBus service
    // thread needs a real initial output list (for `RecordMonitor`'s
    // connector validation) and there are no outputs yet before
    // `init_winit`/`init_udev` runs.
    #[cfg(feature = "screencast")]
    {
        state.screencast = crate::screencast::init(&event_loop.handle(), state.space.outputs());
    }

    // No output/backend dependency (unlike screencast above), but grouped
    // here with the other optional-subsystem init rather than inside
    // `Smallvil::new` for the same reason: keep `new` itself free of
    // feature-gated DBus-thread spawning.
    #[cfg(feature = "accessibility")]
    {
        state.accessibility = Some(crate::accessibility::init());
        state.sync_accessibility_tree();
    }

    // Owned by `Smallvil` so a successful reload can replace the include
    // graph and its directory registrations transactionally.
    state.config_watcher = match config::spawn_watcher(&state.config.source_paths) {
        Ok((watcher, changes)) => {
            match event_loop
                .handle()
                .insert_source(changes, |event, _, state| {
                    if let calloop::channel::Event::Msg(pending) = event {
                        pending.store(false, std::sync::atomic::Ordering::Release);
                        state.refresh_config_watch_anchors();
                        state.note_config_event();
                    }
                }) {
                Ok(_) => Some(watcher),
                Err(err) => {
                    tracing::warn!(%err, "Failed to register config watcher; hot-reload disabled");
                    None
                }
            }
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to watch config file for changes; hot-reload disabled");
            None
        }
    };

    // Kept alive for the process lifetime: its Drop impl unlinks the socket
    // file, dropping it early would tear down the control interface (or,
    // if dropped only at the very end, leave a stale path other clients
    // could try to connect to) while TideWM keeps running.
    let _ipc_socket = match ipc::init(&mut event_loop) {
        Ok(guard) => Some(guard),
        Err(err) => {
            tracing::warn!(%err, "Failed to init IPC socket; control interface disabled");
            None
        }
    };

    // Anything spawned below can reach the portal stack: a toolkit client may
    // read portal settings as soon as it starts, which D-Bus-activates a
    // portal backend. Those backends are Wayland clients, so activating one
    // before WAYLAND_DISPLAY reached the systemd user manager and the D-Bus
    // activation environment makes it exit with "cannot open display:", and
    // the failed unit is then left behind for the rest of the login -- a real
    // login lost ~40s that way, to two blocked settings reads. Wait for the
    // export, bounded: a wedged user bus must still not delay the session.
    if let Some(exported) = session_env_exported {
        if exported.recv_timeout(SESSION_ENV_READY_TIMEOUT).is_err() {
            tracing::warn!(
                "Session environment export still running after {SESSION_ENV_READY_TIMEOUT:?}; spawning startup commands anyway -- portal backends may have failed to start"
            );
        }
    }

    for cmd in &state.config.spawn_at_startup {
        if let Err(err) = spawn(cmd) {
            tracing::warn!(%err, cmd, "Failed to spawn startup command");
        }
    }

    // `-s`/`--spawn` spawns something specific on launch (a smallvil
    // scaffold leftover, kept as an explicit opt-in). Auto-spawning
    // `config.terminal` on every ordinary launch was the same scaffold's
    // default -- dropped now that the welcome hint below teaches
    // Super+Enter instead; a real daily-driver WM shouldn't force a
    // terminal window open on every login.
    if let Some(command) = &args.spawn {
        spawn(command).ok();
    }

    if state.config.show_welcome_hint {
        state.welcome_hint = Some(crate::welcome::WelcomeHint::build(&state.config));
        state.request_redraw();
    }

    // Flush after every dispatch, not just whichever event source happened to
    // fire -- matches anvil's own post-dispatch flush. Without this, outgoing
    // protocol messages (key events, frame-done callbacks) only reached
    // clients whenever the udev backend's maintenance timer or the winit
    // backend's render tick happened to run its own flush_clients() call, and
    // the maintenance timer's cadence backs off to a flat 1s once nothing
    // tracked by has_active_animation() is live -- the case for a plain tiled
    // window with no floating-window physics running. Clients sat starved of
    // both their key events and their draw permission for up to that long,
    // which is what produced the input lag, the client-side key-repeat storm
    // (the release was queued but unflushed, so kitty kept repeating), and
    // the near-frozen redraw rate on an all-tiled udev-backend desktop.
    let result = event_loop.run(None, &mut state, move |state| {
        let _ = state.display_handle.flush_clients();
    });

    // Still holding the display: stop the portal frontend before its backends
    // lose their compositor (see `stop_session_portals`). A nested session
    // shares the host's portals and must leave them alone.
    if !nested {
        stop_session_portals_bounded(ProcessSessionEnvironmentRunner, PORTAL_SHUTDOWN_TIMEOUT);
    }

    result?;
    Ok(())
}

#[cfg(test)]
mod session_environment_tests {
    use super::*;
    use std::{
        os::unix::process::ExitStatusExt,
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Barrier,
        },
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Status(String, Vec<String>),
        Output(String, Vec<String>),
        Read(PathBuf),
    }

    #[derive(Clone)]
    struct RecordingRunner {
        calls: Arc<Mutex<Vec<Call>>>,
        portal_environment: Vec<u8>,
        status_error: bool,
        /// `starttime` reported for `/proc/<frontend>/stat` and
        /// `/proc/self/stat`. Default: the frontend started after us.
        frontend_start: u64,
        own_start: u64,
        /// Fail only the portal restart, modelling `--no-block` returning
        /// while the unit is still not healthy.
        restart_error: bool,
        /// `MainPID` reported for the frontend. Zero models a session with no
        /// systemd --user (OpenRC, runit, s6, ...).
        frontend_pid: u32,
    }

    impl RecordingRunner {
        fn new(portal_environment: &[u8]) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                portal_environment: portal_environment.to_vec(),
                status_error: false,
                frontend_start: 300,
                own_start: 200,
                restart_error: false,
                frontend_pid: 4242,
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SessionEnvironmentRunner for RecordingRunner {
        fn status(&self, program: &str, args: &[String]) -> std::io::Result<ExitStatus> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Status(program.to_string(), args.to_vec()));
            if self.restart_error && args.iter().any(|arg| arg == "restart") {
                return Ok(ExitStatus::from_raw(1));
            }
            if self.status_error {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "test helper unavailable",
                ))
            } else {
                Ok(ExitStatus::from_raw(0))
            }
        }

        fn output(&self, program: &str, args: &[String]) -> std::io::Result<Output> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Output(program.to_string(), args.to_vec()));
            Ok(Output {
                status: ExitStatus::from_raw(0),
                stdout: format!("{}\n", self.frontend_pid).into_bytes(),
                stderr: Vec::new(),
            })
        }

        fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Read(path.to_path_buf()));
            if path.ends_with("stat") {
                // `/proc/<pid>/stat` field 22 (starttime), so a frontend from
                // a previous login is expressible.
                let start = if path == Path::new("/proc/self/stat") {
                    self.own_start
                } else {
                    self.frontend_start
                };
                return Ok(format!(
                    "4242 (proc) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 {start} 20"
                )
                .into_bytes());
            }
            Ok(self.portal_environment.clone())
        }
    }

    #[test]
    fn child_command_clears_the_parents_inherited_signal_mask_before_exec() {
        let mut blocked = MaybeUninit::<libc::sigset_t>::uninit();
        let mut previous = MaybeUninit::<libc::sigset_t>::uninit();
        // Safety: these calls initialize stack-owned sigsets and change only
        // this test thread's mask. The previous mask is restored immediately
        // after spawn, before inspecting the child result.
        unsafe {
            assert_eq!(libc::sigemptyset(blocked.as_mut_ptr()), 0);
            let mut blocked = blocked.assume_init();
            assert_eq!(libc::sigaddset(&mut blocked, libc::SIGTERM), 0);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, previous.as_mut_ptr()),
                0
            );
            let previous = previous.assume_init();
            let child = child_command("sh")
                .args(["-c", "kill -TERM $$; exit 42"])
                .spawn();
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()),
                0
            );

            let status = child.unwrap().wait().unwrap();
            assert_eq!(status.signal(), Some(libc::SIGTERM));
        }
    }

    #[test]
    fn session_environment_tasks_keep_order_and_restart_only_stale_portal() {
        let runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=Hyprland\0");
        let (exported, _waiting) = mpsc::channel();
        run_session_environment_tasks(&["CUSTOM_SESSION_KEY".to_string()], &runner, exported);

        let calls = runner.calls();
        assert_eq!(calls.len(), 10);

        let expected_export_vars = [
            "WAYLAND_DISPLAY",
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_DESKTOP",
            "DESKTOP_SESSION",
            "XDG_SESSION_TYPE",
            "TIDEWM_VERSION",
            "CUSTOM_SESSION_KEY",
        ];
        assert_eq!(
            calls[0],
            Call::Status(
                "dbus-update-activation-environment".to_string(),
                std::iter::once("--systemd")
                    .chain(expected_export_vars)
                    .map(str::to_string)
                    .collect(),
            )
        );
        assert_eq!(
            calls[1],
            Call::Status(
                "systemctl".to_string(),
                ["--user", "import-environment"]
                    .into_iter()
                    .chain(expected_export_vars)
                    .map(str::to_string)
                    .collect(),
            )
        );
        assert_eq!(
            calls[2],
            Call::Status(
                "dbus-update-activation-environment".to_string(),
                FOREIGN_COMPOSITOR_ENV
                    .iter()
                    .map(|key| format!("{key}="))
                    .collect(),
            )
        );
        assert_eq!(
            calls[3],
            Call::Status(
                "systemctl".to_string(),
                ["--user", "unset-environment"]
                    .into_iter()
                    .chain(FOREIGN_COMPOSITOR_ENV.iter().copied())
                    .map(str::to_string)
                    .collect(),
            )
        );
        assert!(matches!(&calls[4], Call::Output(program, _) if program == "systemctl"));
        assert_eq!(calls[5], Call::Read(PathBuf::from("/proc/4242/environ")));
        assert_eq!(calls[6], Call::Read(PathBuf::from("/proc/4242/stat")));
        assert_eq!(calls[7], Call::Read(PathBuf::from("/proc/self/stat")));
        assert_eq!(
            calls[8],
            Call::Status(
                "systemctl".to_string(),
                ["--user", "reset-failed", "*xdg-desktop-portal*.service"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            )
        );
        assert_eq!(
            calls[9],
            Call::Status(
                "systemctl".to_string(),
                [
                    "--user",
                    "--no-block",
                    "restart",
                    "xdg-desktop-portal.service",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            )
        );

        let fresh_runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=tidewm\0");
        let (exported, _waiting) = mpsc::channel();
        run_session_environment_tasks(&[], &fresh_runner, exported);
        assert_eq!(fresh_runner.calls().len(), 8);
        assert!(!fresh_runner.calls().iter().any(|call| {
            matches!(call, Call::Status(program, args) if program == "systemctl" && args.iter().any(|arg| arg == "restart"))
        }));
    }

    #[test]
    fn unavailable_helpers_do_not_stop_later_environment_steps() {
        let mut runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=tidewm\0");
        runner.status_error = true;

        let (exported, _waiting) = mpsc::channel();
        run_session_environment_tasks(&[], &runner, exported);

        let calls = runner.calls();
        assert_eq!(calls.len(), 8);
        assert!(
            matches!(&calls[0], Call::Status(program, _) if program == "dbus-update-activation-environment")
        );
        assert!(matches!(&calls[1], Call::Status(program, _) if program == "systemctl"));
        assert!(
            matches!(&calls[2], Call::Status(program, _) if program == "dbus-update-activation-environment")
        );
        assert!(matches!(&calls[3], Call::Status(program, _) if program == "systemctl"));
        assert!(matches!(&calls[4], Call::Output(program, _) if program == "systemctl"));
        assert!(matches!(&calls[5], Call::Read(_)));
        assert!(matches!(&calls[6], Call::Read(_)));
        assert!(matches!(&calls[7], Call::Read(_)));
    }

    #[test]
    fn worker_returns_while_a_helper_is_blocked() {
        struct BlockingRunner {
            blocked_once: AtomicBool,
            entered: Arc<Barrier>,
            release: Arc<Barrier>,
            worker_name: Arc<Mutex<Option<String>>>,
        }

        impl SessionEnvironmentRunner for BlockingRunner {
            fn status(&self, _program: &str, _args: &[String]) -> std::io::Result<ExitStatus> {
                if !self.blocked_once.swap(true, Ordering::SeqCst) {
                    *self.worker_name.lock().unwrap() =
                        thread::current().name().map(str::to_string);
                    self.entered.wait();
                    self.release.wait();
                }
                Ok(ExitStatus::from_raw(0))
            }

            fn output(&self, _program: &str, _args: &[String]) -> std::io::Result<Output> {
                Ok(Output {
                    status: ExitStatus::from_raw(0),
                    stdout: b"0\n".to_vec(),
                    stderr: Vec::new(),
                })
            }

            fn read(&self, _path: &Path) -> std::io::Result<Vec<u8>> {
                unreachable!("PID zero must skip the portal environment read")
            }
        }

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_name = Arc::new(Mutex::new(None));
        let runner = BlockingRunner {
            blocked_once: AtomicBool::new(false),
            entered: entered.clone(),
            release: release.clone(),
            worker_name: worker_name.clone(),
        };

        let (worker, _exported) =
            start_session_environment_worker_with_runner(Vec::new(), runner).unwrap();
        entered.wait();
        assert_eq!(
            worker_name.lock().unwrap().as_deref(),
            Some("tidewm-session-environment")
        );
        release.wait();
        worker.join().unwrap();
    }

    #[test]
    fn worker_clones_only_validated_environment_keys() {
        let env = HashMap::from([
            ("VALID_KEY".to_string(), "value".to_string()),
            ("INVALID=KEY".to_string(), "value".to_string()),
            ("INVALID_VALUE".to_string(), "nul\0value".to_string()),
        ]);

        assert_eq!(
            validated_session_environment_keys(&env),
            vec!["VALID_KEY".to_string()]
        );
    }

    #[test]
    fn portal_frontend_restart_needs_a_real_signal() {
        // Started after this compositor: this login's own frontend.
        assert!(!portal_frontend_stale(Some(true), Some(200), Some(100)));
        // Another desktop's frontend routes every request to its backend.
        assert!(portal_frontend_stale(Some(false), Some(200), Some(100)));
        // Same desktop, but from a previous login: its backends died with
        // the previous compositor and are stuck failed.
        assert!(portal_frontend_stale(Some(true), Some(50), Some(100)));
        // Not running, or timing unreadable: activation already has the
        // freshly exported environment, so no restart.
        assert!(!portal_frontend_stale(None, None, Some(100)));
        assert!(!portal_frontend_stale(Some(true), None, Some(100)));
        assert!(!portal_frontend_stale(Some(true), Some(50), None));
    }

    #[test]
    fn start_time_parses_around_a_comm_with_spaces() {
        let stat = b"1234 (fire fox (x)) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 20";
        assert_eq!(proc_starttime(stat), Some(4242));
    }

    #[test]
    fn no_session_manager_means_no_unit_recovery() {
        // systemd --user absent (OpenRC, runit, s6, ...): systemctl reports no
        // MainPID, so there is no unit state to inspect or reset -- not even
        // for a late export. Portals there are D-Bus-activated, and the
        // exported activation environment is the whole fix.
        let mut runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=tidewm\0");
        runner.frontend_pid = 0;
        let (exported, waiting) = mpsc::channel();
        drop(waiting);
        run_session_environment_tasks(&[], &runner, exported);

        let calls = runner.calls();
        assert_eq!(calls.len(), 5); // four env exports + the MainPID query
        assert!(!calls.iter().any(|call| matches!(call,
            Call::Status(_, args) if args.iter().any(|arg| arg == "reset-failed" || arg == "restart"))));
    }

    #[test]
    fn stale_same_desktop_frontend_is_repaired_before_readiness() {
        let mut runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=tidewm\0");
        runner.frontend_start = 100; // started before this compositor (200)
        let (exported, waiting) = mpsc::channel();
        run_session_environment_tasks(&[], &runner, exported);

        assert!(
            waiting.try_recv().is_ok(),
            "readiness is signalled even when recovery was needed"
        );
        let calls = runner.calls();
        assert_eq!(calls.len(), 10);
        assert!(matches!(&calls[8], Call::Status(program, args)
            if program == "systemctl" && args.iter().any(|arg| arg == "reset-failed")));
        assert!(matches!(&calls[9], Call::Status(program, args)
            if program == "systemctl" && args.iter().any(|arg| arg == "restart")));
    }

    #[test]
    fn late_export_clears_failed_portal_backends_once() {
        let runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=tidewm\0");
        let (exported, waiting) = mpsc::channel();
        drop(waiting); // startup stopped waiting for the export
        run_session_environment_tasks(&[], &runner, exported);

        let calls = runner.calls();
        assert_eq!(calls.len(), 9);
        assert!(matches!(&calls[8], Call::Status(program, args)
            if program == "systemctl" && args.iter().any(|arg| arg == "reset-failed")));
        // A healthy frontend is not restarted just because the export was late.
        assert!(!calls.iter().any(|call| matches!(call,
            Call::Status(_, args) if args.iter().any(|arg| arg == "restart"))));
    }

    #[test]
    fn delayed_portal_restart_does_not_gate_readiness() {
        let mut runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=tidewm\0");
        runner.frontend_start = 100;
        runner.restart_error = true; // queued with --no-block, not healthy yet
        let (exported, waiting) = mpsc::channel();
        run_session_environment_tasks(&[], &runner, exported);

        assert!(waiting.try_recv().is_ok());
        assert!(runner.calls().iter().any(|call| matches!(call,
            Call::Status(_, args) if args.iter().any(|arg| arg == "--no-block"))));
    }

    #[test]
    fn shutdown_stops_the_frontend_before_its_backends() {
        let runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=tidewm\0");
        stop_session_portals(&runner);

        let stops: Vec<Vec<String>> = runner
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Status(program, args) if program == "systemctl" => Some(args),
                _ => None,
            })
            .collect();
        assert_eq!(
            stops,
            [
                ["--user", "stop", "xdg-desktop-portal.service"],
                ["--user", "stop", "*xdg-desktop-portal-*.service"],
            ]
        );
    }

    #[test]
    fn shutdown_without_systemctl_still_finishes() {
        let mut runner = RecordingRunner::new(b"");
        runner.status_error = true; // no systemd --user: systemctl not found
        assert!(stop_session_portals_bounded(runner, Duration::from_secs(5)));
    }

    #[test]
    fn a_wedged_bus_cannot_hold_up_shutdown() {
        struct Wedged;
        impl SessionEnvironmentRunner for Wedged {
            fn status(&self, _: &str, _: &[String]) -> std::io::Result<ExitStatus> {
                thread::sleep(Duration::from_secs(60));
                Ok(ExitStatus::from_raw(0))
            }
            fn output(&self, _: &str, _: &[String]) -> std::io::Result<Output> {
                unreachable!()
            }
            fn read(&self, _: &Path) -> std::io::Result<Vec<u8>> {
                unreachable!()
            }
        }
        let started = std::time::Instant::now();
        assert!(!stop_session_portals_bounded(
            Wedged,
            Duration::from_millis(50)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

#[cfg(test)]
mod spawn_split_tests {
    use super::split_command as argv;

    #[test]
    fn keeps_plain_whitespace_behavior() {
        assert_eq!(argv("kitty -e fish").unwrap(), ["kitty", "-e", "fish"]);
        assert_eq!(argv("  foot   --server  ").unwrap(), ["foot", "--server"]);
        assert_eq!(argv("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn honors_quotes_and_escapes() {
        assert_eq!(
            argv("kitty sh -c 'fastfetch; sleep 5'").unwrap(),
            ["kitty", "sh", "-c", "fastfetch; sleep 5"]
        );
        assert_eq!(
            argv(r#"notify-send "hi \"there\"" done"#).unwrap(),
            ["notify-send", r#"hi "there""#, "done"]
        );
        assert_eq!(argv(r"foo a\ b").unwrap(), ["foo", "a b"]);
        assert_eq!(argv("foo ''").unwrap(), ["foo", ""]);
        assert_eq!(argv("foo x'y z'w").unwrap(), ["foo", "xy zw"]);
        // tidectl's quoting of an apostrophe round-trips.
        assert_eq!(
            argv(r"notify-send 'it'\''s here'").unwrap(),
            ["notify-send", "it's here"]
        );
        // Not a shell: variables and globs stay literal.
        assert_eq!(
            argv("echo $HOME *.png").unwrap(),
            ["echo", "$HOME", "*.png"]
        );
    }

    #[test]
    fn unterminated_quotes_return_none_for_the_fallback() {
        assert!(argv("sh -c 'oops").is_none());
        assert!(argv(r#"say "unfinished"#).is_none());
        assert!(argv(r"trailing \").is_none());
    }
}
