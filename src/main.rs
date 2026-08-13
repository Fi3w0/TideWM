#![allow(irrefutable_let_patterns)]

mod handlers;

#[cfg(feature = "accessibility")]
mod accessibility;
mod backend;
mod capture;
mod cursor;
mod grabs;
#[cfg(feature = "screencast")]
mod screencast;
mod tide_core;
mod visual;
mod xwayland;

pub(crate) use tide_core::{
    classic_depth, config, input, ipc, layout, ocean, placement, state, wave, waves,
};
#[cfg(feature = "screencast")]
pub(crate) use visual::source_picker;
pub(crate) use visual::{
    animation, backdrop, buoyancy, cascade_transition, caustics, compass, currents, decoration,
    depth, depth_deck, depth_transition, error_overlay, float_physics, frost_glass, minimap,
    ocean_canvas, overview, ripple, shadow, sway, swim, tab_strip, text, toast, ui_theme,
    viscosity, wallpaper, water_glass, welcome, window_animation, workspace_transition,
};

use std::{
    collections::HashMap,
    path::Path,
    process::{Child, Command, ExitStatus, Output},
    sync::Mutex,
    thread::{self, JoinHandle},
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

/// Spawns `cmd`, splitting on whitespace so a simple invocation with
/// arguments (`"kitty -e fish"`) works. Deliberately not shell-parsed --
/// no quoting/globs/pipes, and no injection surprise from untrusted
/// config content -- spawn `sh -c "..."` directly if you need those.
/// Shared by every spawn call site in the compositor (`-s`/`--spawn`
/// below, `config.spawn_at_startup`, and `Action::Spawn` in `input.rs`)
/// so they all get the same argument support for free.
pub(crate) fn spawn(cmd: &str) -> std::io::Result<()> {
    let mut parts = cmd.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;
    let child = std::process::Command::new(program).args(parts).spawn()?;
    track_child(child);
    Ok(())
}

/// Applies `[env]` (`config.env`) to this process via `set_var`, before the
/// backend starts -- so e.g. an `XCURSOR_THEME` override actually reaches
/// `cursor::Theme::load()` (called from `init_udev`, right after this),
/// not just processes TideWM spawns later. Called separately from, and
/// before, `export_session_environment` below, which needs the real
/// `WAYLAND_DISPLAY` the backend sets up and so can only run afterward.
fn apply_user_env(env: &std::collections::HashMap<String, String>) {
    for (key, value) in env {
        if let Err(reason) = crate::config::validate_env_entry(key, value) {
            // Config lowering already filters these and emits a user-facing
            // warning. Keep this guard at the mutation boundary as defense
            // in depth for programmatically constructed Config values.
            tracing::error!(key = ?key, reason, "Skipping invalid environment entry");
            continue;
        }
        std::env::set_var(key, value);
    }
}

const FOREIGN_COMPOSITOR_ENV: &[&str] = &[
    "HYPRLAND_INSTANCE_SIGNATURE",
    "SWAYSOCK",
    "I3SOCK",
    "NIRI_SOCKET",
    "WAYFIRE_SOCKET",
];

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
        Command::new(program).args(args).status()
    }

    fn output(&self, program: &str, args: &[String]) -> std::io::Result<Output> {
        Command::new(program).args(args).output()
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
) -> std::io::Result<JoinHandle<()>> {
    start_session_environment_worker_with_runner(
        validated_session_environment_keys(env),
        ProcessSessionEnvironmentRunner,
    )
}

fn start_session_environment_worker_with_runner<R>(
    env_keys: Vec<String>,
    runner: R,
) -> std::io::Result<JoinHandle<()>>
where
    R: SessionEnvironmentRunner,
{
    thread::Builder::new()
        .name("tidewm-session-environment".to_string())
        .spawn(move || run_session_environment_tasks(&env_keys, &runner))
}

fn run_session_environment_tasks<R>(env_keys: &[String], runner: &R)
where
    R: SessionEnvironmentRunner,
{
    export_session_environment(env_keys, runner);
    restart_stale_portal_frontend(runner);
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
fn restart_stale_portal_frontend<R>(runner: &R)
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
    let has_fresh_env = runner
        .output("systemctl", &show_args)
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .filter(|&pid| pid != 0)
        .and_then(|pid| runner.read(Path::new(&format!("/proc/{pid}/environ"))).ok())
        .map(|environ| {
            environ
                .split(|&byte| byte == 0)
                .any(|entry| entry == b"XDG_CURRENT_DESKTOP=tidewm")
        });

    // `None` covers both "not running yet" and "couldn't check" -- its next
    // activation already gets the freshly exported environment either way,
    // so only an already-running, confirmed-stale instance needs a kick.
    // `--no-block` is load-bearing, not an optimization: this runs before the
    // event loop starts, and the restarted frontend's backend (e.g.
    // xdg-desktop-portal-gtk) is itself a Wayland client of *this*
    // compositor. A synchronous restart waits for the portal, the portal
    // waits for a compositor whose only thread is sitting in that wait --
    // a circular hang that froze the whole session (mouse, keyboard, VT
    // switch) on a real SDDM login. The restart's outcome only matters for
    // screencast requests minutes later, so enqueueing the job is enough.
    if has_fresh_env == Some(false) {
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
                "Restarting xdg-desktop-portal.service: it was still carrying another desktop's identity"
            ),
            Ok(status) => tracing::debug!(?status, "Stale portal frontend restart exited non-zero"),
            Err(err) => tracing::debug!(%err, "Stale portal frontend restart unavailable"),
        }
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
    // threads. Threads inherit the signal mask, ensuring SIGCHLD is delivered
    // through this one event-loop source instead of to an arbitrary worker.
    event_loop
        .handle()
        .insert_source(Signals::new(&[Signal::SIGCHLD])?, |_event, _, state| {
            reap_spawned_children(state)
        })?;

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
    let _session_environment_worker = if !nested {
        match start_session_environment_worker(&state.config.env) {
            Ok(worker) => Some(worker),
            Err(err) => {
                tracing::debug!(%err, "Failed to start session environment export worker");
                None
            }
        }
    } else {
        None
    };

    // Bound so it stays alive for the process lifetime, same idiom as
    // `_config_watcher` below.
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

    // Kept alive for the process lifetime: dropping it stops the watch.
    let _config_watcher = match config::spawn_watcher() {
        Ok((watcher, changes)) => {
            match event_loop
                .handle()
                .insert_source(changes, |event, _, state| {
                    if let calloop::channel::Event::Msg(pending) = event {
                        pending.store(false, std::sync::atomic::Ordering::Release);
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
        state.welcome_hint = Some(crate::welcome::WelcomeHint::build(&state.config.terminal));
        state.request_redraw();
    }

    event_loop.run(None, &mut state, move |_| {
        // Smallvil is running
    })?;

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
    }

    impl RecordingRunner {
        fn new(portal_environment: &[u8]) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                portal_environment: portal_environment.to_vec(),
                status_error: false,
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
                stdout: b"4242\n".to_vec(),
                stderr: Vec::new(),
            })
        }

        fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Read(path.to_path_buf()));
            Ok(self.portal_environment.clone())
        }
    }

    #[test]
    fn session_environment_tasks_keep_order_and_restart_only_stale_portal() {
        let runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=Hyprland\0");
        run_session_environment_tasks(&["CUSTOM_SESSION_KEY".to_string()], &runner);

        let calls = runner.calls();
        assert_eq!(calls.len(), 7);

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
        assert_eq!(
            calls[6],
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
        run_session_environment_tasks(&[], &fresh_runner);
        assert_eq!(fresh_runner.calls().len(), 6);
        assert!(!fresh_runner.calls().iter().any(|call| {
            matches!(call, Call::Status(program, args) if program == "systemctl" && args.iter().any(|arg| arg == "restart"))
        }));
    }

    #[test]
    fn unavailable_helpers_do_not_stop_later_environment_steps() {
        let mut runner = RecordingRunner::new(b"XDG_CURRENT_DESKTOP=tidewm\0");
        runner.status_error = true;

        run_session_environment_tasks(&[], &runner);

        let calls = runner.calls();
        assert_eq!(calls.len(), 6);
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

        let worker = start_session_environment_worker_with_runner(Vec::new(), runner).unwrap();
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
}
