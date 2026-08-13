//! `tidectl doctor` and `tidectl report` -- host-side diagnostics for a
//! TideWM installation.
//!
//! `doctor` runs a battery of quick checks (compositor reachability, build
//! provenance, config state, PipeWire, xdg-desktop-portal, GPU, XWayland,
//! recent journal errors, core dumps, memory) and prints one line per check
//! with a PASS/WARN/FAIL/SKIP verdict. `report` embeds the same check
//! results ("quick check") inside a full plain-text report file meant to be
//! attached to a GitHub issue, expanding detail only when a problem was
//! actually detected -- a healthy system gets a compact ~2-3 page report,
//! a broken one gets the long version.
//!
//! Host checks run as this CLI's own process; compositor-side facts come
//! over the IPC socket via the `diagnostics` request (and the existing
//! `outputs`/`workspaces`/`windows` queries). When TideWM isn't running --
//! the exact case where someone couldn't even start it -- the
//! compositor-dependent checks report SKIP and the host half still works,
//! including a journal excerpt that may explain the failed launch.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

/// Verdict for one diagnostic check. `Skip` means the check couldn't run
/// (missing tool, compositor not running) and is neither good nor bad.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Warn => "WARN",
            Verdict::Fail => "FAIL",
            Verdict::Skip => "SKIP",
        }
    }
}

#[derive(Debug)]
pub struct Check {
    pub name: String,
    pub verdict: Verdict,
    pub detail: String,
}

impl Check {
    fn new(name: impl Into<String>, verdict: Verdict, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            verdict,
            detail: detail.into(),
        }
    }
}

/// The compositor-side facts fetched over IPC when TideWM is reachable.
/// `None` when the socket couldn't be reached -- the host checks still ran,
/// the compositor-dependent ones reported SKIP.
pub struct Diagnostics {
    pub json: Value,
    pub socket: PathBuf,
}

/// Where the IPC socket lives, or the error saying why we couldn't find it.
pub fn find_socket() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("TIDEWM_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
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

/// Runs every check. `doctor` prints the results; `report` embeds them.
pub fn run_checks() -> (Vec<Check>, Option<Diagnostics>) {
    let mut checks: Vec<Check> = Vec::new();

    // --- Compositor reachability -------------------------------------------------
    let socket = find_socket();
    let (diagnostics, mut checks) = match socket {
        Ok(path) => match request(&path, &json!({ "request": "diagnostics" })) {
            Ok(json) => {
                let detail = match json.get("data") {
                    Some(data) => format!(
                        "TideWM {} reachable on {}",
                        data.get("version").and_then(Value::as_str).unwrap_or("?"),
                        path.display()
                    ),
                    None => format!("reachable on {}", path.display()),
                };
                checks.push(Check::new("compositor running", Verdict::Pass, detail));
                (Some(Diagnostics { json, socket: path }), checks)
            }
            Err(err) if err.contains("Connection refused") => {
                // A stale socket file: the compositor died without cleaning
                // up (SIGKILL or crash). Not a live problem, but the check
                // can't run either -- mirror the main CLI's stale-socket
                // behavior and suggest the fix.
                checks.push(Check::new(
                    "compositor running",
                    Verdict::Skip,
                    format!(
                        "not running (stale socket {} -- a crash or SIGKILL left it behind; remove it or start TideWM)",
                        path.display()
                    ),
                ));
                (None, checks)
            }
            Err(err) => {
                // Reached the socket but got garbage: worth a FAIL.
                checks.push(Check::new(
                    "compositor running",
                    Verdict::Fail,
                    format!("socket responded incorrectly: {err}"),
                ));
                (None, checks)
            }
        },
        Err(err) => {
            checks.push(Check::new(
                "compositor running",
                Verdict::Skip,
                format!("{err} -- compositor-specific checks skipped"),
            ));
            (None, checks)
        }
    };

    // --- Build provenance --------------------------------------------------------
    if let Some(diag) = &diagnostics {
        let data = &diag.json["data"];
        let version = data.get("version").and_then(Value::as_str).unwrap_or("?");
        let profile = data.get("profile").and_then(Value::as_str).unwrap_or("?");
        let commit = data
            .get("commit")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(|c| format!("commit {c}"))
            .unwrap_or_else(|| "no commit info".to_string());
        let detail = format!("{version} ({profile}, {commit})");
        let verdict = if profile == "debug" {
            Verdict::Warn
        } else {
            Verdict::Pass
        };
        checks.push(Check::new("build", verdict, detail));
    } else {
        checks.push(Check::new(
            "build",
            Verdict::Skip,
            "compositor not running; using tidectl's own version".to_string(),
        ));
    }

    // --- Config ------------------------------------------------------------------
    if let Some(diag) = &diagnostics {
        let data = &diag.json["data"];
        let path = data
            .get("config_path")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let warnings: Vec<String> = data
            .get("config_warnings")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if warnings.is_empty() {
            checks.push(Check::new(
                "config",
                Verdict::Pass,
                format!("{path} parses cleanly"),
            ));
        } else {
            checks.push(Check::new(
                "config",
                Verdict::Warn,
                format!(
                    "{} warning(s) in {path}: {}",
                    warnings.len(),
                    warnings.join("; ")
                ),
            ));
        }
    } else {
        checks.push(Check::new(
            "config",
            Verdict::Skip,
            "compositor not running; config state unknown".to_string(),
        ));
    }

    // --- PipeWire ------------------------------------------------------------------
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let pipewire_socket = runtime_dir
        .as_ref()
        .map(|dir| dir.join("pipewire-0"))
        .unwrap_or_else(|| PathBuf::from("/run/user/pipewire-0"));
    let pipewire_installed = command_output(&["sh", "-c", "command -v pipewire"]).is_some();
    if pipewire_socket.exists() {
        checks.push(Check::new(
            "pipewire",
            Verdict::Pass,
            format!("socket {} present", pipewire_socket.display()),
        ));
    } else if runtime_dir.is_none() || !pipewire_installed {
        // No user runtime dir at all (headless CI, minimal container), or
        // PipeWire isn't even installed on this machine: not a graphical
        // desktop session to hold to this standard. Expected, not a
        // problem -- unlike a real desktop where PipeWire is installed and
        // XDG_RUNTIME_DIR exists but the socket is still missing, which
        // means the user service isn't running.
        checks.push(Check::new(
            "pipewire",
            Verdict::Skip,
            "no XDG_RUNTIME_DIR or PipeWire not installed -- not a graphical desktop session"
                .to_string(),
        ));
    } else {
        checks.push(Check::new(
            "pipewire",
            Verdict::Fail,
            format!(
                "no PipeWire socket at {} -- audio and PipeWire screencasting will not work; start the pipewire user service",
                pipewire_socket.display()
            ),
        ));
    }

    // --- xdg-desktop-portal --------------------------------------------------------
    // The *frontend's* MainPID comes from systemd (the same source
    // main.rs's stale-portal check uses) because a bare pgrep matches both
    // the frontend and the per-desktop backends (`xdg-desktop-portal-
    // hyprland` and friends), and the backend's env is meaningless here --
    // a leftover backend from a previous login can carry the old desktop's
    // XDG_CURRENT_DESKTOP forever without being the thing that routes
    // requests. The frontend is the only process whose env decides routing.
    let portal_pid = command_output(&[
        "systemctl",
        "--user",
        "show",
        "-p",
        "MainPID",
        "--value",
        "xdg-desktop-portal.service",
    ])
    .as_deref()
    .and_then(|s| s.trim().parse::<i64>().ok())
    .filter(|&pid| pid != 0)
    .or_else(|| {
        // Fallback when systemctl isn't available: match only the exact
        // frontend binary path so a backend process can never match.
        command_output(&[
            "pgrep",
            "-f",
            "^/usr/lib/xdg-desktop-portal$|xdg-desktop-portal$",
        ])
        .as_deref()
        .and_then(|s| s.lines().next())
        .and_then(|line| line.trim().parse::<i64>().ok())
    });
    match portal_pid {
        Some(pid) => {
            let env = read_proc_environ(pid);
            let current_desktop = env
                .get("XDG_CURRENT_DESKTOP")
                .map(String::as_str)
                .unwrap_or("");
            // The portal frontend fixes its backend at startup from the
            // session env it inherited. The staleness signal is a *mismatch*
            // with the desktop this CLI is running inside -- a portal that
            // predates a session switch (relogin on a surviving systemd user
            // manager) keeps the old desktop's env and routes every request
            // to the wrong backend. Matching the active session is healthy
            // even when that desktop isn't TideWM (Hyprland, KDE, ...).
            let session_desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
            if current_desktop.is_empty() {
                checks.push(Check::new(
                    "xdg-desktop-portal",
                    Verdict::Warn,
                    format!("running (pid {pid}) but XDG_CURRENT_DESKTOP unset -- screensharing may route to the wrong backend"),
                ));
            } else if current_desktop == session_desktop {
                checks.push(Check::new(
                    "xdg-desktop-portal",
                    Verdict::Pass,
                    format!("running (pid {pid}), backend matches the active session ({current_desktop})"),
                ));
            } else if session_desktop.is_empty() {
                checks.push(Check::new(
                    "xdg-desktop-portal",
                    Verdict::Warn,
                    format!(
                        "running (pid {pid}) with XDG_CURRENT_DESKTOP={current_desktop}, and this shell's session desktop is unset -- can't verify routing"
                    ),
                ));
            } else {
                checks.push(Check::new(
                    "xdg-desktop-portal",
                    Verdict::Fail,
                    format!(
                        "running (pid {pid}) with XDG_CURRENT_DESKTOP={current_desktop} but this session is {session_desktop} -- the portal predates a session switch and routes requests to the wrong backend; restart it after logging in through {session_desktop}"
                    ),
                ));
            }
        }
        None => {
            checks.push(Check::new(
                "xdg-desktop-portal",
                Verdict::Warn,
                "not running -- portal-based screensharing (OBS, browser) will fail until it is started".to_string(),
            ));
        }
    }

    // --- GPU -------------------------------------------------------------------------
    let dri_paths = [
        PathBuf::from("/dev/dri/renderD128"),
        PathBuf::from("/dev/dri/renderD129"),
        PathBuf::from("/dev/dri/card0"),
    ];
    let dri_present = dri_paths.iter().any(|p| p.exists());
    let dri_detail = if dri_present {
        dri_paths
            .iter()
            .filter(|p| p.exists())
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        "/dev/dri has no render nodes".to_string()
    };

    // Which GPU driver module is loaded? Reads /proc/modules (cheap, no
    // external tools) and falls back to lspci for the device line.
    let modules = std::fs::read_to_string("/proc/modules").unwrap_or_default();
    let mut drivers: Vec<String> = Vec::new();
    for name in ["amdgpu", "radeon", "nvidia", "nouveau", "i915", "xe"] {
        if modules.lines().any(|line| line.starts_with(name)) {
            drivers.push(name.to_string());
        }
    }
    let driver_detail = if drivers.is_empty() {
        "no known GPU driver module in /proc/modules".to_string()
    } else {
        format!("drivers loaded: {}", drivers.join(", "))
    };

    let lspci = command_output(&["lspci", "-nnk", "-d", "::0300"]);
    let gpu_detail = match lspci {
        Some(line) => format!(
            "{driver_detail}; {}",
            line.lines().next().unwrap_or("").trim()
        ),
        None => driver_detail,
    };

    // No render node *and* no known driver module loaded means there is no
    // real GPU here at all -- expected on a headless CI/build machine, not
    // a problem to report. A driver loaded with no render node is the real
    // failure case (permissions, missing KMS support, misconfiguration).
    let verdict = if dri_present {
        Verdict::Pass
    } else if drivers.is_empty() {
        Verdict::Skip
    } else {
        Verdict::Fail
    };
    checks.push(Check::new(
        "gpu",
        verdict,
        format!("render nodes: {dri_detail}; {gpu_detail}"),
    ));

    // --- XWayland ----------------------------------------------------------------------
    let xwayland_enabled = diagnostics
        .as_ref()
        .and_then(|d| d.json["data"].get("xwayland_enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let satellite = command_output(&["pgrep", "-f", "xwayland-satellite"]).is_some();
    if xwayland_enabled {
        if satellite {
            checks.push(Check::new(
                "xwayland",
                Verdict::Pass,
                "xwayland-satellite running (X11 apps supported)".to_string(),
            ));
        } else {
            checks.push(Check::new(
                "xwayland",
                Verdict::Fail,
                "xwayland is enabled in config but xwayland-satellite is not running -- X11 apps will fail to start".to_string(),
            ));
        }
    } else if satellite {
        checks.push(Check::new(
            "xwayland",
            Verdict::Warn,
            "xwayland-satellite running but xwayland is disabled in config -- stray process"
                .to_string(),
        ));
    } else {
        checks.push(Check::new(
            "xwayland",
            Verdict::Pass,
            "disabled in config (nothing to check)".to_string(),
        ));
    }

    // --- Journal (compositor errors) -----------------------------------------------------
    let journal = journal_errors(24);
    match &journal {
        Some(lines) if lines.is_empty() => {
            checks.push(Check::new(
                "journal",
                Verdict::Pass,
                "no TideWM error/panic lines in the last 24 hours".to_string(),
            ));
        }
        Some(lines) => {
            let last = lines[0].chars().take(140).collect::<String>();
            checks.push(Check::new(
                "journal",
                Verdict::Warn,
                format!(
                    "{} error line(s) in the last 24h; newest: {last}",
                    lines.len()
                ),
            ));
        }
        None => {
            checks.push(Check::new(
                "journal",
                Verdict::Skip,
                "journalctl unavailable or no TideWM entries".to_string(),
            ));
        }
    }

    // --- Core dumps -----------------------------------------------------------------------
    let coredumps = core_dumps();
    match &coredumps {
        Some(lines) if lines.is_empty() => {
            checks.push(Check::new(
                "core dumps",
                Verdict::Pass,
                "no TideWM core dumps in the last 7 days".to_string(),
            ));
        }
        Some(lines) => {
            checks.push(Check::new(
                "core dumps",
                Verdict::Fail,
                format!(
                    "{} core dump(s) in the last 7 days -- a crash; first: {}",
                    lines.len(),
                    lines[0]
                ),
            ));
        }
        None => {
            checks.push(Check::new(
                "core dumps",
                Verdict::Skip,
                "coredumpctl unavailable".to_string(),
            ));
        }
    }

    // --- Memory ---------------------------------------------------------------------------
    if let Some(_diag) = &diagnostics {
        let pid = compositor_pid();
        match pid.and_then(compositor_pss) {
            Some(pss) => {
                // 2GB is AGENT.md's current absolute ceiling (Hard
                // Constraints, revised 2026-07-30 from the old flat
                // 1.5GB/3GB estimate): a tripwire for runaway feature
                // growth, not a resting point. Real measured PSS scales
                // with enabled effects -- basic tiling is ~50MB, the full
                // water/decoration stack is ~60-70MB idle -- so this can't
                // judge whether a given reading is normal for the
                // maintainer's actual feature set, only whether it's blown
                // well past what any configuration should ever reach.
                let warn = pss > 2_000_000_000;
                checks.push(Check::new(
                    "memory",
                    if warn { Verdict::Warn } else { Verdict::Pass },
                    format!("compositor PSS {:.1} MiB", pss as f64 / (1024.0 * 1024.0)),
                ));
            }
            None => checks.push(Check::new(
                "memory",
                Verdict::Skip,
                "could not find compositor process for PSS".to_string(),
            )),
        }
    }

    (checks, diagnostics)
}

/// Runs a command, returning stdout as a String on success (empty string
/// if it ran but printed nothing), None if it couldn't run at all.
fn command_output(args: &[&str]) -> Option<String> {
    Command::new(args[0])
        .args(&args[1..])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Reads a process's environment from /proc/<pid>/environ.
fn read_proc_environ(pid: i64) -> std::collections::HashMap<String, String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return std::collections::HashMap::new();
    };
    let mut env = std::collections::HashMap::new();
    for entry in raw.split(|&b| b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Some(eq) = entry.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = String::from_utf8_lossy(&entry[..eq]).into_owned();
        let value = String::from_utf8_lossy(&entry[eq + 1..]).into_owned();
        env.insert(key, value);
    }
    env
}

/// The running TideWM compositor's PID, found by socket path (the PID is
/// embedded in the socket name `tidewm-<pid>.sock`).
fn compositor_pid() -> Option<i64> {
    let socket = find_socket().ok()?;
    let name = socket.file_name()?.to_string_lossy();
    let pid = name.trim_start_matches("tidewm-").trim_end_matches(".sock");
    pid.parse::<i64>().ok()
}

/// Compositor PSS from /proc/<pid>/smaps_rollup, if readable.
fn compositor_pss(pid: i64) -> Option<u64> {
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    rollup
        .lines()
        .find_map(|line| {
            line.strip_prefix("Pss:")?
                .trim()
                .strip_suffix(" kB")
                .and_then(|kb| kb.trim().parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
}

/// TideWM error/panic lines from the last `hours`, newest first. Returns
/// None when journalctl can't run or has no TideWM entries at all.
fn journal_errors(hours: u64) -> Option<Vec<String>> {
    let since = format!("{hours} hours ago");
    let all = Command::new("journalctl")
        .args([
            "-b",
            "--reverse",
            "--since",
            &since,
            "--no-pager",
            "-o",
            "short-iso",
            "_COMM=TideWM",
        ])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())?;
    let interesting: Vec<String> = all
        .lines()
        .filter(|line| line.contains("error") || line.contains("panic") || line.contains("ERROR"))
        .map(str::to_string)
        .take(20)
        .collect();
    Some(interesting)
}

/// TideWM core dumps in the last 7 days, newest first. None when
/// coredumpctl can't run.
fn core_dumps() -> Option<Vec<String>> {
    let out = Command::new("coredumpctl")
        .args(["--no-pager", "--reverse", "--since", "-7 days", "list"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())?;
    let lines: Vec<String> = out
        .lines()
        .filter(|line| line.contains("TideWM"))
        .map(str::to_string)
        .collect();
    Some(lines)
}

/// Sends one JSON request over the IPC socket and parses the response.
fn request(socket: &Path, request: &Value) -> Result<Value, String> {
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|e| format!("failed to connect: {e}"))?;
    let mut payload = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    payload.push(b'\n');
    stream.write_all(&payload).map_err(|e| e.to_string())?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let line = buf.split(|&b| b == b'\n').next().unwrap_or(&[]);
    serde_json::from_slice(line).map_err(|e| e.to_string())
}

/// Fetches an IPC query's `data` field, or None if the compositor is
/// unreachable.
fn query_data(diagnostics: &Diagnostics, req: &Value) -> Option<Value> {
    let response = request(&diagnostics.socket, req).ok()?;
    response.get("data").cloned()
}

/// Whether the report should expand into verbose mode: any FAIL, or more
/// than one WARN. A healthy system stays compact.
pub fn needs_verbose(checks: &[Check]) -> bool {
    let warns = checks.iter().filter(|c| c.verdict == Verdict::Warn).count();
    checks.iter().any(|c| c.verdict == Verdict::Fail) || warns > 1
}

// ---------------------------------------------------------------------------
// Report rendering
// ---------------------------------------------------------------------------

/// Renders the full report into a String. `checks` come from `run_checks`;
/// when the compositor is reachable the report also includes its live
/// outputs/workspaces/windows. Compact by default; expands the log-heavy
/// sections when `verbose` (problems detected).
pub fn render_report(checks: &[Check], diagnostics: &Option<Diagnostics>, verbose: bool) -> String {
    let mut out = String::new();
    let verdict = overall_verdict(checks);

    out.push_str("TideWM diagnostic report\n");
    out.push_str("========================\n");
    out.push_str(&format!("Generated: {}\n", local_now()));
    out.push_str("Attach this file to a GitHub issue: https://github.com/Fi3w0/TideWM/issues\n");
    out.push_str("Privacy: the report includes window titles and config warnings. Review before attaching.\n\n");

    // --- Summary ------------------------------------------------------------
    out.push_str("== 1. Summary ==\n");
    if let Some(diag) = diagnostics {
        let data = &diag.json["data"];
        let version = data.get("version").and_then(Value::as_str).unwrap_or("?");
        let commit = data
            .get("commit")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let dirty = data.get("dirty").and_then(Value::as_bool).unwrap_or(false);
        let profile = data.get("profile").and_then(Value::as_str).unwrap_or("?");
        let build_date = data
            .get("build_date")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let backend = data.get("backend").and_then(Value::as_str).unwrap_or("?");
        let engine = data
            .get("spatial_engine")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let uptime = data.get("uptime_secs").and_then(Value::as_u64).unwrap_or(0);
        let water = data
            .get("water_effects")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        out.push_str(&format!(
            "Version     : {version} (commit {commit}{}, {profile}, built {build_date})\n",
            if dirty { "-dirty" } else { "" }
        ));
        out.push_str(&format!("Backend     : {backend}\n"));
        out.push_str(&format!(
            "Engine      : {engine}  (water effects {})\n",
            if water { "on" } else { "off" }
        ));
        out.push_str(&format!("Uptime      : {}\n", human_duration(uptime)));
        out.push_str(&format!(
            "Config      : {} ({})\n",
            data.get("config_path")
                .and_then(Value::as_str)
                .unwrap_or("?"),
            match data
                .get("config_warnings")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
            {
                0 => "no warnings".to_string(),
                n => format!("{n} warning(s)"),
            }
        ));
        out.push_str(&format!(
            "Layers      : {} layer surfaces, {} keybinds\n",
            data.get("layer_surfaces")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            data.get("keybind_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ));
    } else {
        out.push_str("Version     : tidectl's own (see below -- compositor not running)\n");
        out.push_str(&format!(
            "  tidectl {} ({})\n",
            env!("CARGO_PKG_VERSION"),
            build_tag()
        ));
    }
    out.push_str(&format!(
        "Quick check : {} -- {} passed, {} warnings, {} failed, {} skipped\n\n",
        verdict,
        checks.iter().filter(|c| c.verdict == Verdict::Pass).count(),
        checks.iter().filter(|c| c.verdict == Verdict::Warn).count(),
        checks.iter().filter(|c| c.verdict == Verdict::Fail).count(),
        checks.iter().filter(|c| c.verdict == Verdict::Skip).count(),
    ));

    // --- Quick check ----------------------------------------------------------
    out.push_str("== 2. Quick check (tidectl doctor) ==\n");
    for check in checks {
        out.push_str(&format!(
            "[{:>4}] {}: {}\n",
            check.verdict.label(),
            check.name,
            check.detail
        ));
    }
    out.push('\n');

    // --- System ---------------------------------------------------------------
    out.push_str("== 3. System ==\n");
    if let Some(pretty) = os_release_pretty_name() {
        out.push_str(&format!("OS          : {pretty}\n"));
    }
    if let Some(kernel) = uname_release() {
        out.push_str(&format!("Kernel      : {kernel}\n"));
    }
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "?".into());
    out.push_str(&format!("Session     : {session_type}\n"));
    let host = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "?".into());
    if host == "tidewm" {
        out.push_str("Host        : TideWM (standalone session)\n");
    } else {
        out.push_str(&format!("Host        : {host} (nested host compositor)\n"));
    }
    if let Some(wayland) = std::env::var_os("WAYLAND_DISPLAY") {
        out.push_str(&format!("Wayland     : {}\n", wayland.to_string_lossy()));
    }
    if let Some(gpu) = gpu_summary() {
        out.push_str(&format!("GPU         : {gpu}\n"));
    }
    if let Some(mesa) = mesa_version() {
        out.push_str(&format!("Mesa        : {mesa}\n"));
    }
    out.push('\n');

    // --- Compositor state --------------------------------------------------------
    if let Some(diag) = diagnostics {
        out.push_str("== 4. Compositor state ==\n");
        let outputs = query_data(diag, &json!({ "request": "outputs" }));
        if let Some(Value::Array(outputs)) = outputs {
            out.push_str("Outputs:\n");
            for o in outputs {
                let name = o.get("name").and_then(Value::as_str).unwrap_or("?");
                let size = o.get("size").and_then(Value::as_array);
                let (w, h) = size
                    .and_then(|s| Some((s.first()?.as_i64()?, s.get(1)?.as_i64()?)))
                    .unwrap_or((0, 0));
                let scale = o.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
                let ws = o
                    .get("active_workspace")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                out.push_str(&format!("  {name}  {w}x{h} scale={scale} workspace={ws}\n"));
            }
        }

        let workspaces = query_data(diag, &json!({ "request": "workspaces" }));
        if let Some(Value::Array(workspaces)) = workspaces {
            out.push_str("Workspaces:\n");
            for w in workspaces {
                let output = w.get("output").and_then(Value::as_str).unwrap_or("?");
                let ws = w.get("workspace").and_then(Value::as_u64).unwrap_or(0);
                let active = w.get("active").and_then(Value::as_bool).unwrap_or(false);
                let count = w.get("window_count").and_then(Value::as_u64).unwrap_or(0);
                out.push_str(&format!(
                    "  {} {} workspace={ws} windows={count}\n",
                    if active { "*" } else { " " },
                    output
                ));
            }
        }

        let windows = query_data(diag, &json!({ "request": "windows" }));
        if let Some(Value::Array(windows)) = windows {
            let cap = if verbose { windows.len() } else { 12 };
            out.push_str(&format!(
                "Windows ({}{}):\n",
                windows.len(),
                if windows.len() > cap {
                    format!(", first {cap} shown")
                } else {
                    String::new()
                }
            ));
            for w in windows.iter().take(cap) {
                let title = w.get("title").and_then(Value::as_str).unwrap_or("");
                let app_id = w.get("app_id").and_then(Value::as_str).unwrap_or("");
                let ws = w.get("workspace").and_then(Value::as_u64).unwrap_or(0);
                let focused = w.get("focused").and_then(Value::as_bool).unwrap_or(false);
                out.push_str(&format!(
                    "  {app_id} \"{title}\" workspace={ws}{}\n",
                    if focused { " [focused]" } else { "" }
                ));
            }
            if windows.len() > cap {
                out.push_str(&format!("  ... {} more\n", windows.len() - cap));
            }
        }

        let warnings = diag.json["data"]
            .get("config_warnings")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !warnings.is_empty() {
            out.push_str("Config warnings:\n");
            for warning in warnings {
                out.push_str(&format!("  - {warning}\n"));
            }
        }
        out.push('\n');
    }

    // --- Services -----------------------------------------------------------------
    out.push_str("== 5. Services ==\n");
    for name in [
        "pipewire",
        "xdg-desktop-portal",
        "xwayland",
        "journal",
        "core dumps",
    ] {
        if let Some(check) = checks.iter().find(|c| c.name == name) {
            out.push_str(&format!("{:<20}: {}\n", name, check.detail));
        }
    }
    out.push('\n');

    // --- Journal detail ---------------------------------------------------------------
    if let Some(lines) = journal_errors(24) {
        let cap = if verbose { 40 } else { 10 };
        if !lines.is_empty() {
            out.push_str(&format!(
                "== 6. Recent compositor errors (journalctl, {}) ==\n",
                if verbose {
                    "most recent 40 lines"
                } else {
                    "most recent 10 lines"
                }
            ));
            for line in lines.iter().take(cap) {
                out.push_str(&format!("  {line}\n"));
            }
            out.push('\n');
        }
    } else {
        out.push_str(
            "== 6. Recent compositor errors ==\n  journalctl unavailable or no TideWM entries\n\n",
        );
    }

    // --- Core dumps detail ---------------------------------------------------------------
    if let Some(lines) = core_dumps() {
        if !lines.is_empty() {
            out.push_str("== 7. Core dumps (coredumpctl, last 7 days) ==\n");
            for line in lines.iter().take(if verbose { 10 } else { 3 }) {
                out.push_str(&format!("  {line}\n"));
            }
            out.push('\n');
        }
    }

    // --- Footer ------------------------------------------------------------------------
    out.push_str("---\n");
    let footer = match overall_verdict(checks) {
        "PROBLEMS DETECTED" => {
            "Problems detected -- the sections above include the expanded detail. Attach this file to the issue."
        }
        "WARNINGS" => {
            "A few warnings to review -- see the WARN lines above. Attach this file to the issue and describe what you did."
        }
        _ => {
            "Everything looks healthy. Attach this file to the issue and describe what you did."
        }
    };
    out.push_str(&format!("Verdict: {}. {footer}\n", overall_verdict(checks)));
    out
}

fn overall_verdict(checks: &[Check]) -> &'static str {
    if checks.iter().any(|c| c.verdict == Verdict::Fail) {
        "PROBLEMS DETECTED"
    } else if checks.iter().any(|c| c.verdict == Verdict::Warn) {
        "WARNINGS"
    } else {
        "OK"
    }
}

fn os_release_pretty_name() -> Option<String> {
    let content = std::fs::read_to_string("/etc/os-release").ok()?;
    content
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim_matches('"').to_string())
}

fn uname_release() -> Option<String> {
    command_output(&["uname", "-r", "-m"]).map(|s| s.trim().to_string())
}

fn gpu_summary() -> Option<String> {
    let modules = std::fs::read_to_string("/proc/modules").ok()?;
    let drivers: Vec<&str> = ["amdgpu", "radeon", "nvidia", "nouveau", "i915", "xe"]
        .into_iter()
        .filter(|name| modules.lines().any(|line| line.starts_with(name)))
        .collect();
    let lspci = command_output(&["lspci", "-nnk", "-d", "::0300"]);
    match lspci {
        Some(line) => Some(format!(
            "{} ({})",
            line.lines().next().unwrap_or("").trim(),
            drivers.join(", ")
        )),
        None => Some(drivers.join(", ")),
    }
}

fn mesa_version() -> Option<String> {
    let glxinfo = command_output(&["glxinfo", "-B"])?;
    glxinfo.lines().find_map(|line| {
        if line.contains("OpenGL renderer") || line.contains("OpenGL version") {
            Some(line.trim().to_string())
        } else {
            None
        }
    })
}

/// `tidectl --version`-style build tag; mirrors `main.rs::build_tag`. The
/// version itself is printed separately by callers -- this only appends the
/// provenance parts.
fn build_tag() -> String {
    let mut parts: Vec<String> = Vec::new();
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

fn human_duration(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn local_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Date-only in UTC to avoid pulling a timezone crate into the CLI.
    let days = secs / 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    let hour = (secs % 86_400) / 3_600;
    let minute = (secs % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}
