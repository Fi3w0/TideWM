//! Native placement for X11 override-redirect overlays.
//!
//! Xwayland-satellite maps an `override_redirect` window without an X11
//! parent as an ordinary `xdg_toplevel`. XDG toplevels carry no position, and
//! satellite also moves the X11 window to whatever rect TideWM chooses, so
//! by the time the Wayland surface maps the client's requested position is
//! gone. Steam's friend-playing toasts are exactly this shape
//! (`notificationtoasts_N_desktop`), which is why they used to land centered.
//!
//! The requested rect is read when the X11 window maps, not when it is
//! created: clients may move an override-redirect window between those two
//! events (Steam's notification toast starts centred on its main window and
//! moves to the corner just before mapping). A small observer child selects
//! `SubstructureNotify` on the root window and records that rect plus the
//! window's title and `WM_CLASS`; this is read-only and does not manage or
//! move any X11 window. `Smallvil` matches a record to the X11-origin Wayland
//! toplevel by title, WM_CLASS and initial size, then treats it as an
//! overlay: float it at the recorded position, do not focus it, and skip
//! compositor decorations/depth. When RECORD is available, the observer also
//! follows ConfigureWindow requests from each window's owning X11 connection;
//! satellite's requests are excluded to prevent placement feedback loops.
//! The observer runs as a separate process so
//! the compositor's event loop never blocks on an X11 round trip.

mod owner_moves;

use std::{
    collections::{HashMap, HashSet},
    io::{self, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::process::CommandExt,
    },
    process::{Child, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use smithay::reexports::{
    calloop::{generic::Generic, Interest, LoopHandle, Mode, PostAction, RegistrationToken},
    wayland_server::protocol::wl_surface::WlSurface,
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt, CreateNotifyEvent,
        DestroyNotifyEvent, EventMask, Window,
    },
};

use crate::Smallvil;

/// Internal argv marker used to re-exec TideWM as the X11 observer child.
pub(crate) const OBSERVER_FLAG: &str = "--x11-overlay-observer";

const MAX_RECORD_AGE: Duration = Duration::from_secs(30);
const RECENT_FOR_MATCHING: Duration = Duration::from_secs(3);
const SCORE_NAME: u32 = 8;
const SCORE_CLASS: u32 = 4;
const SCORE_SIZE: u32 = 4;
const SCORE_RECENT: u32 = 1;

/// One output's X11-space rectangle. Satellite subtracts the minimum output
/// position when it lays out the Xwayland root, and uses each output's
/// physical mode size there, so the translation back to TideWM logical
/// coordinates is per-output:
/// `logical = output_position + (x11 - x11_output_position) / scale`.
#[derive(Debug, Clone)]
pub(crate) struct OutputInfo {
    pub name: String,
    pub position: (i32, i32),
    pub x11_position: (i32, i32),
    pub size: (i32, i32),
    pub scale: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct OverlayPlacement {
    id: u32,
    raw_position: (i32, i32),
    pub output: String,
    pub position: (i32, i32),
}

#[derive(Debug, Clone)]
struct OverlayRecord {
    id: u32,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
    name: String,
    res_name: String,
    res_class: String,
    received: Instant,
}

#[derive(Clone)]
struct OverlayBinding {
    surface: WlSurface,
    output: String,
    raw_position: (i32, i32),
}

#[derive(Default, Clone, Copy)]
struct OwnerPosition {
    x: Option<i32>,
    y: Option<i32>,
}

impl OwnerPosition {
    fn apply(self, position: (i32, i32)) -> (i32, i32) {
        (self.x.unwrap_or(position.0), self.y.unwrap_or(position.1))
    }
}

#[derive(Default)]
pub(crate) struct X11OverlayState {
    records: Vec<OverlayRecord>,
    buffer: Vec<u8>,
    child: Option<Child>,
    /// Surfaces already classified as X11 overlays. Kept here rather than in
    /// a render-only map so every decoration/depth check can ask one place.
    surfaces: HashSet<WlSurface>,
    token: Option<RegistrationToken>,
    bindings: HashMap<u32, OverlayBinding>,
    owner_positions: HashMap<u32, OwnerPosition>,
}

impl X11OverlayState {
    pub(crate) fn is_surface_overlay(&self, surface: &WlSurface) -> bool {
        self.surfaces.contains(surface)
    }

    pub(crate) fn mark_surface(&mut self, surface: WlSurface) {
        self.surfaces.insert(surface);
    }

    pub(crate) fn forget_surface(&mut self, surface: &WlSurface) {
        self.surfaces.remove(surface);
        let ids: Vec<_> = self
            .bindings
            .iter()
            .filter_map(|(id, binding)| (binding.surface == *surface).then_some(*id))
            .collect();
        for id in ids {
            self.bindings.remove(&id);
            self.owner_positions.remove(&id);
        }
    }

    /// Matches one Wayland surface's identity/size against the observer's
    /// pending override-redirect records and consumes the winner. The score
    /// floor keeps a size-only coincidence from claiming an ordinary X11
    /// window; a record needs at least a title match or a class+size match.
    /// Conflicting identities and equally ranked candidates are refused.
    /// Size alone is never identity: satellite may start a new surface at
    /// the session's smallest scale before receiving its destination enter.
    pub(crate) fn match_for(
        &mut self,
        app_id: Option<&str>,
        title: Option<&str>,
        size: (i32, i32),
        outputs: &[OutputInfo],
    ) -> Option<OverlayPlacement> {
        self.prune();

        let app_id = app_id.unwrap_or_default();
        let title = title.unwrap_or_default();
        let mut best: Option<(u32, u32)> = None;
        let mut ambiguous = false;

        for record in &self.records {
            let Some(output) = output_for_record(record, outputs) else {
                continue;
            };
            // Known identity fields must agree, even if another field and
            // the size happen to match a different window of the same app.
            if (!record.name.is_empty() && !title.is_empty() && record.name != title)
                || (!app_id.is_empty()
                    && (!record.res_name.is_empty() || !record.res_class.is_empty())
                    && !class_matches(record, app_id))
            {
                continue;
            }
            let mut score = 0;
            if !record.name.is_empty() && record.name == title {
                score += SCORE_NAME;
            }
            if class_matches(record, app_id) {
                score += SCORE_CLASS;
            }
            if size_matches(record, size, output.scale) {
                score += SCORE_SIZE;
            }
            if record.received.elapsed() < RECENT_FOR_MATCHING {
                score += SCORE_RECENT;
            }
            if score >= SCORE_NAME && best.is_none_or(|(_, best_score)| score > best_score) {
                best = Some((record.id, score));
                ambiguous = false;
            } else if score >= SCORE_NAME && best.is_some_and(|(_, best_score)| score == best_score)
            {
                ambiguous = true;
            }
        }

        if ambiguous {
            return None;
        }
        let (id, _) = best?;
        let index = self.records.iter().position(|record| record.id == id)?;
        let mut record = self.records.remove(index);
        if let Some(position) = self.owner_positions.remove(&id) {
            (record.x, record.y) = position.apply((record.x, record.y));
        }
        let output = output_for_record(&record, outputs)?;
        let scale = output.scale.max(f64::EPSILON);
        let local_x = (f64::from(record.x) - f64::from(output.x11_position.0)) / scale;
        let local_y = (f64::from(record.y) - f64::from(output.x11_position.1)) / scale;
        Some(OverlayPlacement {
            id,
            raw_position: (record.x, record.y),
            output: output.name.clone(),
            position: (
                output.position.0 + local_x.round() as i32,
                output.position.1 + local_y.round() as i32,
            ),
        })
    }

    /// Called from the observer's stdout source. Returns `true` on EOF so the
    /// caller can remove the now-permanently-readable source.
    fn read_fd(&mut self, fd: RawFd) -> bool {
        let mut chunk = [0u8; 4096];
        // Safety: `fd` is the live read end of the observer child's stdout
        // pipe, owned by the calloop source for the duration of this call.
        // The buffer is stack-owned and its length is passed exactly.
        let read = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if read > 0 {
            self.buffer.extend_from_slice(&chunk[..read as usize]);
            self.consume_lines();
            false
        } else if read == 0 {
            self.stop_child();
            true
        } else {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::Interrupted {
                false
            } else {
                tracing::warn!(%err, "X11 overlay observer read failed");
                self.stop_child();
                true
            }
        }
    }

    fn consume_lines(&mut self) {
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line = self.buffer[..newline].to_vec();
            self.buffer.drain(..=newline);
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<ObserverLine>(&line) {
                Ok(ObserverLine::Overlay {
                    id,
                    x,
                    y,
                    width,
                    height,
                    name,
                    res_name,
                    res_class,
                }) => {
                    tracing::debug!(
                        id,
                        x,
                        y,
                        width,
                        height,
                        name,
                        res_name,
                        res_class,
                        "X11 overlay record received"
                    );
                    self.records.retain(|record| record.id != id);
                    if self.records.len() >= 64 {
                        self.records.remove(0);
                    }
                    self.records.push(OverlayRecord {
                        id,
                        x,
                        y,
                        width,
                        height,
                        name,
                        res_name,
                        res_class,
                        received: Instant::now(),
                    });
                }
                Ok(ObserverLine::Move { id, x, y }) => {
                    if !self.owner_positions.contains_key(&id) && self.owner_positions.len() >= 64 {
                        // Keep the observer-to-main-loop queue bounded, including
                        // moves delivered before their first map record.
                        continue;
                    }
                    let pending = self.owner_positions.entry(id).or_default();
                    if x.is_some() {
                        pending.x = x;
                    }
                    if y.is_some() {
                        pending.y = y;
                    }
                }
                Ok(ObserverLine::Gone { id }) => {
                    self.records.retain(|record| record.id != id);
                    self.bindings.remove(&id);
                    self.owner_positions.remove(&id);
                }
                Err(err) => {
                    tracing::debug!(%err, "Ignoring malformed X11 overlay record");
                }
            }
        }
    }

    fn prune(&mut self) {
        self.records
            .retain(|record| record.received.elapsed() < MAX_RECORD_AGE);
    }

    fn stop_child(&mut self) {
        // An interrupted observer cannot validate pending matches anymore.
        self.records.clear();
        self.buffer.clear();
        self.owner_positions.clear();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for X11OverlayState {
    fn drop(&mut self) {
        self.stop_child();
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ObserverLine {
    Overlay {
        id: u32,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        #[serde(default)]
        name: String,
        #[serde(default)]
        res_name: String,
        #[serde(default)]
        res_class: String,
    },
    Move {
        id: u32,
        x: Option<i32>,
        y: Option<i32>,
    },
    Gone {
        id: u32,
    },
}

fn class_matches(record: &OverlayRecord, app_id: &str) -> bool {
    !app_id.is_empty()
        && (record.res_name.eq_ignore_ascii_case(app_id)
            || record.res_class.eq_ignore_ascii_case(app_id))
}

fn size_matches(record: &OverlayRecord, size: (i32, i32), scale: f64) -> bool {
    if size.0 <= 0 || size.1 <= 0 {
        return false;
    }
    let width = (f64::from(record.width) / scale).round() as i32;
    let height = (f64::from(record.height) / scale).round() as i32;
    (width - size.0).abs() <= 2 && (height - size.1).abs() <= 2
}

fn output_for_record<'a>(
    record: &OverlayRecord,
    outputs: &'a [OutputInfo],
) -> Option<&'a OutputInfo> {
    if outputs.is_empty() {
        return None;
    }
    let left = record.x;
    let top = record.y;
    let right = record.x.saturating_add(i32::from(record.width));
    let bottom = record.y.saturating_add(i32::from(record.height));
    let intersection = |output: &&OutputInfo| -> i64 {
        let x1 = left.max(output.x11_position.0);
        let y1 = top.max(output.x11_position.1);
        let x2 = right.min(output.x11_position.0.saturating_add(output.size.0));
        let y2 = bottom.min(output.x11_position.1.saturating_add(output.size.1));
        if x2 > x1 && y2 > y1 {
            i64::from(x2 - x1) * i64::from(y2 - y1)
        } else {
            0
        }
    };
    if let Some((output, area)) = outputs
        .iter()
        .map(|output| (output, intersection(&output)))
        .max_by_key(|(_, area)| *area)
    {
        if area > 0 {
            return Some(output);
        }
    }
    // A rect entirely outside every output (a stale/clamped request) still
    // belongs to the nearest one so its position can be clamped on screen.
    let center_x = f64::from(record.x) + f64::from(record.width) / 2.0;
    let center_y = f64::from(record.y) + f64::from(record.height) / 2.0;
    outputs.iter().min_by_key(|output| {
        let dx = center_x - (f64::from(output.x11_position.0) + f64::from(output.size.0) / 2.0);
        let dy = center_y - (f64::from(output.x11_position.1) + f64::from(output.size.1) / 2.0);
        (dx * dx + dy * dy) as i64
    })
}

/// Starts the observer child and registers its stdout with the compositor
/// loop. Failures are soft: the returned state simply has no records, so OR
/// windows keep TideWM's ordinary placement.
pub(crate) fn start(handle: &LoopHandle<'static, Smallvil>, display: &str) -> X11OverlayState {
    let mut state = X11OverlayState::default();
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            tracing::warn!(%err, "Could not resolve TideWM executable for the X11 overlay observer");
            return state;
        }
    };

    let mut command = crate::child_command(exe);
    // Die with the compositor even on SIGKILL: without this, an observer
    // waiting for an OR window could outlive an abruptly killed parent.
    // Safety: after fork and before exec this calls only `prctl` with plain
    // integer arguments; it captures nothing.
    unsafe {
        command.pre_exec(|| {
            let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    command
        .arg(OBSERVER_FLAG)
        .env("DISPLAY", display)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(%err, "Could not start the X11 overlay observer");
            return state;
        }
    };
    let Some(stdout) = child.stdout.take() else {
        tracing::warn!("X11 overlay observer produced no stdout");
        let _ = child.kill();
        let _ = child.wait();
        return state;
    };
    tracing::debug!(pid = child.id(), "X11 overlay observer started");
    state.child = Some(child);

    let eof_handle = handle.clone();
    match handle.insert_source(
        Generic::new(stdout, Interest::READ, Mode::Level),
        move |_, file, data| {
            let fd = file.as_raw_fd();
            if data.x11_overlay.read_fd(fd) {
                if let Some(token) = data.x11_overlay.token.take() {
                    eof_handle.remove(token);
                }
            }
            data.apply_x11_owner_moves();
            Ok(PostAction::Continue)
        },
    ) {
        Ok(token) => state.token = Some(token),
        Err(err) => {
            tracing::warn!(%err, "Could not register the X11 overlay observer");
            state.stop_child();
        }
    }
    state
}

impl Smallvil {
    /// True for a surface the observer classified as an X11
    /// override-redirect overlay. Decoration/depth code asks this rather than
    /// re-deriving OR status from config.
    pub(crate) fn is_x11_overlay(&self, surface: &WlSurface) -> bool {
        self.x11_overlay.is_surface_overlay(surface)
    }

    /// Looks up an observer record for a not-yet-mapped X11 surface. Only
    /// satellite's own Wayland clients are eligible; native Wayland windows
    /// never reach the observer at all.
    pub(crate) fn take_x11_overlay(&mut self, surface: &WlSurface) -> Option<OverlayPlacement> {
        if !self.is_xwayland_surface(surface) {
            return None;
        }
        let window = self.unmapped_toplevels.get(surface)?;
        let size = window.geometry().size;
        let (app_id, title) = self.toplevel_identity(surface);
        let mut outputs: Vec<OutputInfo> = self
            .space
            .outputs()
            .filter_map(|output| {
                let mode = output.current_mode()?;
                let position = output.current_location();
                Some(OutputInfo {
                    name: output.name(),
                    position: (position.x, position.y),
                    x11_position: (0, 0),
                    size: (mode.size.w, mode.size.h),
                    scale: output.current_scale().fractional_scale(),
                })
            })
            .collect();
        // Satellite subtracts the smallest output origin before laying out
        // the Xwayland root, so the X11 position is relative to that origin,
        // not to TideWM's logical output position.
        let min_x = outputs
            .iter()
            .map(|output| output.position.0)
            .min()
            .unwrap_or(0);
        let min_y = outputs
            .iter()
            .map(|output| output.position.1)
            .min()
            .unwrap_or(0);
        for output in &mut outputs {
            output.x11_position = (output.position.0 - min_x, output.position.1 - min_y);
        }
        let placement = self.x11_overlay.match_for(
            app_id.as_deref(),
            title.as_deref(),
            (size.w, size.h),
            &outputs,
        );
        tracing::debug!(
            ?size,
            app_id = app_id.as_deref(),
            title = title.as_deref(),
            pending = self.x11_overlay.records.len(),
            ?placement,
            "X11 overlay lookup"
        );
        if let Some(placement) = &placement {
            self.x11_overlay.bindings.insert(
                placement.id,
                OverlayBinding {
                    surface: surface.clone(),
                    output: placement.output.clone(),
                    raw_position: placement.raw_position,
                },
            );
        }
        placement
    }

    fn apply_x11_owner_moves(&mut self) {
        let ids: Vec<_> = self
            .x11_overlay
            .owner_positions
            .keys()
            .filter(|id| self.x11_overlay.bindings.contains_key(id))
            .copied()
            .collect();
        for id in ids {
            let movement = self.x11_overlay.owner_positions.remove(&id).unwrap();
            let binding = self.x11_overlay.bindings.get_mut(&id).unwrap();
            binding.raw_position = movement.apply(binding.raw_position);
            let binding = binding.clone();
            if !self.is_x11_overlay(&binding.surface)
                || self.fullscreen.contains_key(&binding.surface)
                || self.maximized.contains_key(&binding.surface)
            {
                continue;
            }
            let Some(output) = self.output_by_name(&binding.output) else {
                continue;
            };
            let min_x = self
                .space
                .outputs()
                .map(|o| o.current_location().x)
                .min()
                .unwrap_or(0);
            let min_y = self
                .space
                .outputs()
                .map(|o| o.current_location().y)
                .min()
                .unwrap_or(0);
            let origin = output.current_location();
            let scale = output.current_scale().fractional_scale();
            let position = (
                origin.x
                    + ((f64::from(binding.raw_position.0) - f64::from(origin.x - min_x)) / scale)
                        .round() as i32,
                origin.y
                    + ((f64::from(binding.raw_position.1) - f64::from(origin.y - min_y)) / scale)
                        .round() as i32,
            );
            let workspace = self
                .spatial
                .workspaces
                .floating_workspace
                .get(&binding.surface)
                .map(|tag| tag.workspace)
                .unwrap_or_else(|| {
                    self.spatial
                        .workspaces
                        .layout
                        .active_workspace(&binding.output)
                });
            let rule = crate::config::WindowRule {
                position: Some(position),
                ..Default::default()
            };
            self.place_initial_floating(&binding.surface, &output, workspace, &rule, None);
        }
    }
}

// ── Observer child ────────────────────────────────────────────────────────

struct Atoms {
    net_wm_name: Atom,
    utf8_string: Atom,
}

impl Atoms {
    fn intern(conn: &impl ConnectionExt) -> io::Result<Self> {
        let net_wm_name = conn
            .intern_atom(false, b"_NET_WM_NAME")
            .map_err(protocol_error)?
            .reply()
            .map_err(protocol_error)?
            .atom;
        let utf8_string = conn
            .intern_atom(false, b"UTF8_STRING")
            .map_err(protocol_error)?
            .reply()
            .map_err(protocol_error)?
            .atom;
        Ok(Self {
            net_wm_name,
            utf8_string,
        })
    }
}

/// Runs the observer's own event loop. Invoked only through
/// [`OBSERVER_FLAG`]; it never initializes the compositor.
pub(crate) fn run_observer() -> io::Result<()> {
    let (conn, screen_num) = connect_with_retry()?;
    let root = conn.setup().roots[screen_num].root;
    // Xwayland resets its root during startup, which clears any
    // SubstructureNotify selection made before the server settles. Select
    // immediately (so an early OR window is still seen when no reset
    // happens), then again after the reset window. Only this observer child
    // waits; the compositor loop continues dispatching independently.
    let _ = select_root(&conn, root);
    std::thread::sleep(Duration::from_millis(500));
    let atoms = Atoms::intern(&conn)?;
    select_root(&conn, root)?;
    conn.flush().map_err(protocol_error)?;

    let mut created: HashMap<Window, Geometry> = HashMap::new();
    let mut emitted: HashSet<Window> = HashSet::new();
    let tracked = Arc::new(Mutex::new(HashSet::new()));
    if let Err(err) = owner_moves::start(&conn, tracked.clone()) {
        eprintln!("X11 overlay observer: client movement observation unavailable: {err}");
    }
    let mut stdout = io::stdout();

    loop {
        let event = match conn.wait_for_event() {
            Ok(event) => event,
            Err(err) => return Err(protocol_error(err)),
        };
        match event {
            x11rb::protocol::Event::CreateNotify(event) if event.parent == root => {
                // Create is not proof of mapping, nor does size identify a
                // Wayland surface. Keep geometry private until MapNotify;
                // clients may enable override_redirect after creation.
                created.insert(event.window, geometry_from_create(&event));
                if event.override_redirect {
                    tracked.lock().unwrap().insert(event.window);
                }
            }
            x11rb::protocol::Event::ConfigureNotify(event) if event.event == root => {
                if let Some(geometry) = created.get_mut(&event.window) {
                    *geometry = Geometry {
                        x: i32::from(event.x),
                        y: i32::from(event.y),
                        width: event.width,
                        height: event.height,
                    };
                }
            }
            x11rb::protocol::Event::MapNotify(event)
                if event.event == root && event.override_redirect =>
            {
                // Snapshot the ordered Create/Configure event history here.
                // GetGeometry can already see a later compositor move by
                // the time its reply arrives, even while processing MapNotify.
                let Some(geometry) = created.get(&event.window).copied() else {
                    continue;
                };
                if emitted.contains(&event.window) {
                    continue;
                }
                let identity = (|| {
                    let name = window_name(&conn, event.window, &atoms)?.unwrap_or_default();
                    let (res_name, res_class) =
                        window_class(&conn, event.window)?.unwrap_or_default();
                    Ok::<_, io::Error>((name, res_name, res_class))
                })();
                let (name, res_name, res_class) = match identity {
                    Ok(identity) => identity,
                    // A popup may close before any property reply. Losing
                    // that one popup is normal; losing the observer is not.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                };
                tracked.lock().unwrap().insert(event.window);
                emitted.insert(event.window);
                write_record(
                    &mut stdout,
                    &serde_json::json!({
                        "kind": "overlay",
                        "id": event.window,
                        "x": geometry.x,
                        "y": geometry.y,
                        "width": geometry.width,
                        "height": geometry.height,
                        "name": name,
                        "res_name": res_name,
                        "res_class": res_class,
                    }),
                )?;
            }
            x11rb::protocol::Event::UnmapNotify(event) if event.event == root => {
                // X11 clients often reuse the same XID for their next popup.
                // End this map episode so the next MapNotify is eligible.
                if emitted.remove(&event.window) {
                    write_record(
                        &mut stdout,
                        &serde_json::json!({"kind":"gone", "id":event.window}),
                    )?;
                }
            }
            x11rb::protocol::Event::DestroyNotify(event) => {
                tracked.lock().unwrap().remove(&event.window);
                remove_window(&event, &mut created, &mut emitted, &mut stdout)?;
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Geometry {
    x: i32,
    y: i32,
    width: u16,
    height: u16,
}

fn select_root(conn: &impl ConnectionExt, root: Window) -> io::Result<()> {
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
    )
    .map_err(protocol_error)?
    .check()
    .map_err(protocol_error)?;
    Ok(())
}

fn geometry_from_create(event: &CreateNotifyEvent) -> Geometry {
    Geometry {
        x: i32::from(event.x),
        y: i32::from(event.y),
        width: event.width,
        height: event.height,
    }
}

fn remove_window(
    event: &DestroyNotifyEvent,
    created: &mut HashMap<Window, Geometry>,
    emitted: &mut HashSet<Window>,
    stdout: &mut impl Write,
) -> io::Result<()> {
    created.remove(&event.window);
    if emitted.remove(&event.window) {
        write_record(
            stdout,
            &serde_json::json!({ "kind": "gone", "id": event.window }),
        )?;
    }
    Ok(())
}

fn write_record(stdout: &mut impl Write, value: &serde_json::Value) -> io::Result<()> {
    // One write_all on Stdout holds its process-wide lock for the complete
    // JSON line, so the child RECORD reader cannot interleave its output.
    stdout.write_all(format!("{value}\n").as_bytes())?;
    stdout.flush()
}

fn window_name(
    conn: &impl ConnectionExt,
    window: Window,
    atoms: &Atoms,
) -> io::Result<Option<String>> {
    if let Some(value) = get_property(conn, window, atoms.net_wm_name, atoms.utf8_string)? {
        if !value.is_empty() {
            return Ok(Some(String::from_utf8_lossy(&value).into_owned()));
        }
    }
    Ok(get_property(
        conn,
        window,
        Atom::from(AtomEnum::WM_NAME),
        AtomEnum::STRING,
    )?
    .filter(|value| !value.is_empty())
    .map(|value| String::from_utf8_lossy(&value).into_owned()))
}

fn window_class(conn: &impl ConnectionExt, window: Window) -> io::Result<Option<(String, String)>> {
    let Some(value) = get_property(
        conn,
        window,
        Atom::from(AtomEnum::WM_CLASS),
        AtomEnum::STRING,
    )?
    else {
        return Ok(None);
    };
    let mut parts = value.split(|byte| *byte == 0);
    let res_name = parts.next().unwrap_or_default();
    let res_class = parts.next().unwrap_or_default();
    Ok(Some((
        String::from_utf8_lossy(res_name).into_owned(),
        String::from_utf8_lossy(res_class).into_owned(),
    )))
}

fn get_property(
    conn: &impl ConnectionExt,
    window: Window,
    property: impl Into<Atom> + Copy,
    type_: impl Into<Atom> + Copy,
) -> io::Result<Option<Vec<u8>>> {
    let reply = conn
        .get_property(false, window, property, type_, 0, 1024)
        .map_err(protocol_error)?
        .reply()
        .map_err(property_error)?;
    if reply.format == 8 {
        Ok(Some(reply.value))
    } else {
        Ok(None)
    }
}

fn property_error(err: x11rb::errors::ReplyError) -> io::Error {
    if matches!(&err, x11rb::errors::ReplyError::X11Error(error)
        if error.error_kind == x11rb::protocol::ErrorKind::Window)
    {
        io::Error::new(io::ErrorKind::NotFound, err)
    } else {
        protocol_error(err)
    }
}

fn protocol_error(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}

fn connect_with_retry() -> io::Result<(x11rb::rust_connection::RustConnection, usize)> {
    let mut last = None;
    for _ in 0..20 {
        match x11rb::connect(None) {
            Ok(pair) => return Ok(pair),
            Err(err) => {
                last = Some(err);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(protocol_error(
        last.expect("the retry loop always runs at least once"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> Vec<OutputInfo> {
        vec![OutputInfo {
            name: "HDMI-A-2".into(),
            position: (1920, 0),
            x11_position: (1920, 0),
            size: (2560, 1440),
            scale: 1.25,
        }]
    }

    fn record(name: &str, res_name: &str) -> OverlayRecord {
        OverlayRecord {
            id: 7,
            x: 4000,
            y: 1300,
            width: 227,
            height: 56,
            name: name.into(),
            res_name: res_name.into(),
            res_class: "Steam".into(),
            received: Instant::now(),
        }
    }

    #[test]
    fn matches_title_class_and_size_and_translates_the_x11_rect() {
        let mut state = X11OverlayState::default();
        state
            .records
            .push(record("notificationtoasts_3_desktop", "steam"));
        let placement = state
            .match_for(
                Some("steam"),
                Some("notificationtoasts_3_desktop"),
                (182, 45),
                &output(),
            )
            .expect("record should match");
        assert_eq!(placement.output, "HDMI-A-2");
        // (4000 - 1920) / 1.25 = 1664; (1300 - 0) / 1.25 = 1040.
        assert_eq!(placement.position, (3584, 1040));
        assert!(state.records.is_empty(), "a matched record is consumed");
    }

    #[test]
    fn requires_a_title_or_class_plus_size_match() {
        let mut state = X11OverlayState::default();
        let mut unmatched = record("", "unrelated");
        unmatched.res_class = "Unrelated".into();
        state.records.push(unmatched);
        assert!(state
            .match_for(
                Some("steam"),
                Some("notificationtoasts_3_desktop"),
                (182, 45),
                &output(),
            )
            .is_none());
    }

    #[test]
    fn class_match_is_case_insensitive_and_size_tolerates_rounding() {
        let mut state = X11OverlayState::default();
        state.records.push(record("", "STEAM"));
        assert!(state
            .match_for(Some("steam"), Some(""), (183, 44), &output())
            .is_some());
    }

    #[test]
    fn stale_records_are_pruned() {
        let mut state = X11OverlayState::default();
        let mut stale = record("notificationtoasts_3_desktop", "steam");
        stale.received = Instant::now() - MAX_RECORD_AGE - Duration::from_secs(1);
        state.records.push(stale);
        assert!(state
            .match_for(
                Some("steam"),
                Some("notificationtoasts_3_desktop"),
                (182, 45),
                &output(),
            )
            .is_none());
    }

    #[test]
    fn translation_uses_the_x11_root_origin_not_negative_logical_positions() {
        let mut state = X11OverlayState::default();
        let mut record = record("toast", "steam");
        record.x = 100;
        record.y = 200;
        record.width = 200;
        record.height = 100;
        state.records.push(record);
        let outputs = vec![
            OutputInfo {
                name: "DP-4".into(),
                position: (-1920, 0),
                x11_position: (0, 0),
                size: (1920, 1080),
                scale: 1.0,
            },
            OutputInfo {
                name: "HDMI-A-2".into(),
                position: (0, 0),
                x11_position: (1920, 0),
                size: (2560, 1440),
                scale: 1.0,
            },
        ];
        let placement = state
            .match_for(Some("steam"), Some("toast"), (200, 100), &outputs)
            .expect("record should match");
        assert_eq!(placement.output, "DP-4");
        assert_eq!(placement.position, (-1820, 200));
    }
    #[test]
    fn an_unmapped_x11_window_cannot_claim_an_ordinary_toplevel_by_size() {
        let mut state = X11OverlayState::default();
        state.buffer.extend_from_slice(
            b"{\"kind\":\"create\",\"id\":7,\"x\":4000,\"y\":1300,\"width\":227,\"height\":56}\n",
        );
        state.consume_lines();
        assert!(state
            .match_for(Some("ordinary"), Some("ordinary"), (182, 45), &output())
            .is_none());
    }

    #[test]
    fn a_conflicting_title_cannot_match_by_class_and_size() {
        let mut state = X11OverlayState::default();
        state.records.push(record("popup", "review"));
        assert!(state
            .match_for(Some("review"), Some("ordinary"), (182, 45), &output())
            .is_none());
    }

    #[test]
    fn pipe_reads_preserve_a_title_split_inside_utf8() {
        use std::os::unix::net::UnixStream;
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let line = serde_json::json!({"kind":"overlay", "id":7, "x":4000, "y":1300,
            "width":227, "height":56, "name":"café", "res_name":"review", "res_class":"Review"})
        .to_string()
            + "\n";
        let split = line.find('é').unwrap() + 1;
        let mut state = X11OverlayState::default();
        writer.write_all(&line.as_bytes()[..split]).unwrap();
        assert!(!state.read_fd(reader.as_raw_fd()));
        writer.write_all(&line.as_bytes()[split..]).unwrap();
        assert!(!state.read_fd(reader.as_raw_fd()));
        assert_eq!(state.records[0].name, "café");
    }
    #[test]
    fn equally_ranked_candidates_are_not_arbitrarily_consumed() {
        let mut state = X11OverlayState::default();
        let first = record("popup", "review");
        let mut second = first.clone();
        second.id = 8;
        second.x += 10;
        state.records.extend([first, second]);
        assert!(state
            .match_for(Some("review"), Some("popup"), (182, 45), &output())
            .is_none());
        assert_eq!(state.records.len(), 2);
    }

    #[test]
    fn known_identity_can_match_before_satellite_switches_to_the_output_scale() {
        let mut state = X11OverlayState::default();
        state.records.push(record("popup", "review"));
        // Satellite initializes at the smallest scale in a mixed-scale
        // session, then adjusts after the destination wl_surface.enter.
        assert!(state
            .match_for(Some("review"), Some("popup"), (227, 56), &output())
            .is_some());
    }

    #[test]
    fn unmap_discards_a_record_and_allows_the_same_xid_to_map_again() {
        let mut state = X11OverlayState::default();
        let line = serde_json::json!({"kind":"overlay", "id":7, "x":4000, "y":1300,
            "width":227, "height":56, "name":"popup", "res_name":"review", "res_class":"Review"})
        .to_string()
            + "\n";
        state.buffer.extend_from_slice(line.as_bytes());
        state.consume_lines();
        state
            .buffer
            .extend_from_slice(b"{\"kind\":\"gone\",\"id\":7}\n");
        state.consume_lines();
        assert!(state.records.is_empty());
        state.buffer.extend_from_slice(line.as_bytes());
        state.consume_lines();
        assert!(state
            .match_for(Some("review"), Some("popup"), (182, 45), &output())
            .is_some());
        state
            .buffer
            .extend_from_slice(b"{\"kind\":\"gone\",\"id\":7}\n");
        state.buffer.extend_from_slice(line.as_bytes());
        state.consume_lines();
        assert!(state
            .match_for(Some("review"), Some("popup"), (182, 45), &output())
            .is_some());
    }

    #[test]
    fn observer_eof_discards_unmatched_records() {
        use std::os::unix::net::UnixStream;
        let (writer, reader) = UnixStream::pair().unwrap();
        let mut state = X11OverlayState::default();
        state.records.push(record("popup", "review"));
        drop(writer);
        assert!(state.read_fd(reader.as_raw_fd()));
        assert!(state.records.is_empty());
    }
    #[test]
    fn owner_position_delivered_before_mapping_replaces_the_snapshot() {
        let mut state = X11OverlayState::default();
        state.records.push(record("popup", "review"));
        state
            .buffer
            .extend_from_slice(b"{\"kind\":\"move\",\"id\":7,\"x\":4100,\"y\":null}\n");
        state
            .buffer
            .extend_from_slice(b"{\"kind\":\"move\",\"id\":7,\"x\":null,\"y\":1200}\n");
        state.consume_lines();
        let placement = state
            .match_for(Some("review"), Some("popup"), (182, 45), &output())
            .unwrap();
        assert_eq!(placement.raw_position, (4100, 1200));
        assert_eq!(placement.position, (3664, 960));
        assert!(state.owner_positions.is_empty());
    }

    #[test]
    fn unmap_forgets_queued_owner_moves_and_the_queue_is_bounded() {
        let mut state = X11OverlayState::default();
        for id in 0..80 {
            state.buffer.extend_from_slice(
                format!("{{\"kind\":\"move\",\"id\":{id},\"x\":10,\"y\":20}}\n").as_bytes(),
            );
        }
        state.consume_lines();
        assert_eq!(state.owner_positions.len(), 64);
        state
            .buffer
            .extend_from_slice(b"{\"kind\":\"gone\",\"id\":7}\n");
        state.consume_lines();
        assert!(!state.owner_positions.contains_key(&7));
    }
}
