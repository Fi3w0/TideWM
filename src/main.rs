#![allow(irrefutable_let_patterns)]

mod handlers;

#[cfg(feature = "accessibility")]
mod accessibility;
mod backend;
mod capture;
mod config;
mod cursor;
mod grabs;
mod input;
mod ipc;
mod layout;
mod overview;
#[cfg(feature = "screencast")]
mod screencast;
mod state;
mod tab_strip;
mod toast;
mod wallpaper;
mod waves;
mod welcome;
mod xwayland;

use std::{process::Child, sync::Mutex};

use smithay::reexports::{
    calloop::{
        self,
        signals::{Signal, Signals},
        EventLoop,
    },
    wayland_server::Display,
};
pub use state::Smallvil;

/// Children started by TideWM and not synchronously waited by their call
/// site. Keeping the `Child` handles lets the SIGCHLD event source below reap
/// them with `try_wait`; dropping a `Child` without waiting would leave a
/// zombie until the compositor exits.
static SPAWNED_CHILDREN: Mutex<Vec<Child>> = Mutex::new(Vec::new());

pub(crate) fn track_child(child: Child) {
    SPAWNED_CHILDREN.lock().unwrap().push(child);
}

fn reap_spawned_children() {
    SPAWNED_CHILDREN
        .lock()
        .unwrap()
        .retain_mut(|child| match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                tracing::debug!(pid = child.id(), ?status, "Spawned child exited");
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
        std::env::set_var(key, value);
    }
}

/// Exports `WAYLAND_DISPLAY`/`XDG_CURRENT_DESKTOP`/`XDG_SESSION_TYPE` plus
/// any `[env]` entries into the systemd user session and the D-Bus
/// session-activation environment, so anything activated by either (a
/// portal backend, a polkit agent) sees a real graphical session instead of
/// whatever it inherited from before TideWM started (nothing, on a bare
/// TTY login). `set_var` alone only affects this process's own children,
/// not something already running or activated on demand later -- these two
/// external commands are how sway, Hyprland, and niri all actually get
/// this to the rest of the session (matches Hyprland's own autostart.conf,
/// which does the same `dbus-update-activation-environment --systemd` call
/// listing its `env =` keys alongside `WAYLAND_DISPLAY`).
///
/// `XDG_CURRENT_DESKTOP=tidewm` is a real decision, not a formality: no
/// portal backend ships a matching profile for a brand-new compositor
/// name out of the box, so screen-sharing/file-picker portals still need
/// the user's own `xdg-desktop-portal` backend configured (same situation
/// niri and sway users are in) -- see README.
///
/// Both commands are missing on plenty of distros (no systemd, or a
/// minimal systemd without the user session bus) -- logged at `debug`
/// and otherwise ignored, never fatal, per this project's any-distro
/// requirement.
fn export_session_environment(env: &std::collections::HashMap<String, String>) {
    let mut vars: Vec<&str> = vec![
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
        "XDG_SESSION_TYPE",
        "TIDEWM_VERSION",
    ];
    vars.extend(env.keys().map(String::as_str));

    for (program, mut args) in [
        ("dbus-update-activation-environment", vec!["--systemd"]),
        ("systemctl", vec!["--user", "import-environment"]),
    ] {
        args.extend_from_slice(&vars);
        match std::process::Command::new(program).args(&args).status() {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::debug!(program, ?status, "Session environment export exited non-zero"),
            Err(err) => tracing::debug!(%err, program, "Session environment export command not available"),
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
                println!("TideWM {}", env!("CARGO_PKG_VERSION"));
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

    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    let mut event_loop: EventLoop<'static, Smallvil> = EventLoop::try_new()?;

    // Create this before backend/optional-service initialization spawns any
    // threads. Threads inherit the signal mask, ensuring SIGCHLD is delivered
    // through this one event-loop source instead of to an arbitrary worker.
    event_loop
        .handle()
        .insert_source(Signals::new(&[Signal::SIGCHLD])?, |_event, _, _state| {
            reap_spawned_children()
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
        crate::backend::winit::init_winit(&mut event_loop, &mut state)?;
    } else {
        crate::backend::udev::init_udev(&mut event_loop, &mut state)?;
    }

    // After backend init: both branches above set the real WAYLAND_DISPLAY
    // (see `state.socket_name`), which is exactly what needs to reach the
    // systemd user session and D-Bus activation environment for anything
    // session-activated (xdg-desktop-portal, a polkit agent) to find it,
    // same startup step sway/Hyprland/niri all take.
    export_session_environment(&state.config.env);

    // Bound so it stays alive for the process lifetime, same idiom as
    // `_config_watcher` below.
    let _satellite = if state.config.xwayland.enabled {
        crate::xwayland::setup(&state.config.xwayland.path)
    } else {
        None
    };

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
