//! Screencasting: `org.gnome.Mutter.ScreenCast` over the session DBus,
//! streamed through PipeWire. See AGENT.md's "Screencasting" section for
//! the full design writeup (why this path over `xdg-desktop-portal-wlr`,
//! why the render side reuses `capture.rs` rather than a parallel path,
//! and the verification bar before any of this can be trusted).
//!
//! `dbus` owns the Mutter-compatible object model while `pipewire_thread`
//! owns one PipeWire main loop per active monitor stream. PipeWire process
//! callbacks ask the compositor thread for a frame through the calloop
//! channel below; only the compositor thread ever touches `Smallvil` or GL.
//!
//! The DBus service thread never touches `Smallvil` directly -- every
//! existing `Smallvil` field assumes single-threaded access. It only
//! reads a shared, read-only output snapshot (`outputs` below, refreshed
//! by the main thread on hotplug) and sends `ScreencastEvent`s back over
//! the calloop channel set up
//! here. That channel is the only sanctioned way either thread affects
//! compositor state.

mod dbus;
mod pipewire_thread;
mod portal;

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use smithay::output::Output;
use smithay::reexports::calloop::{channel, LoopHandle};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::state::Smallvil;

/// A cheap, DBus-thread-readable copy of the bits of `Output` that
/// `RecordMonitor`'s connector validation and `Stream::Parameters` need.
/// Never carries the real `Output` handle across the thread boundary --
/// `Output` is `Smallvil`-adjacent state, not `Send`-safe to treat as
/// shared-ownership from another thread the way this plain data is.
#[derive(Clone)]
pub(crate) struct OutputSnapshot {
    pub(crate) name: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone)]
pub(crate) struct WindowSnapshot {
    pub(crate) id: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Debug)]
pub(crate) enum ScreencastSource {
    Output(String),
    Window(u64),
}

#[derive(Clone, Debug)]
pub(crate) struct SourceChoice {
    pub(crate) source: ScreencastSource,
    pub(crate) source_type: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) label: String,
}

#[derive(Clone)]
pub struct FrameTarget {
    pub(crate) frame: Arc<Mutex<Option<Arc<ScreencastFrame>>>>,
    pub(crate) draw_cursor: bool,
    pending: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl FrameTarget {
    fn new(draw_cursor: bool) -> Self {
        Self {
            frame: Arc::new(Mutex::new(None)),
            draw_cursor,
            pending: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn request(
        &self,
        sender: &channel::Sender<ScreencastEvent>,
        source: &ScreencastSource,
    ) {
        if !self.closed.load(Ordering::Acquire)
            && !self.pending.swap(true, Ordering::AcqRel)
            && sender
                .send(ScreencastEvent::FrameRequested {
                    source: source.clone(),
                    target: self.clone(),
                })
                .is_err()
        {
            self.pending.store(false, Ordering::Release);
        }
    }

    pub(crate) fn complete(&self, mut frame: Option<Arc<ScreencastFrame>>) {
        if self.closed.load(Ordering::Acquire) {
            frame = None;
        }
        *self.frame.lock().unwrap() = frame;
        self.pending.store(false, Ordering::Release);
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.complete(None);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

pub(crate) struct ScreencastFrame {
    pub(crate) pixels: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
}

pub enum ScreencastEvent {
    FrameRequested {
        source: ScreencastSource,
        target: FrameTarget,
    },
    PickSource {
        source_types: u32,
        response: mpsc::SyncSender<Option<SourceChoice>>,
    },
    DmabufFrameRequested {
        source: ScreencastSource,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        draw_cursor: bool,
        done: mpsc::SyncSender<bool>,
    },
}

/// Screencast subsystem handle.
pub struct ScreencastState {
    sender: channel::Sender<ScreencastEvent>,
    outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
    windows: Arc<Mutex<HashMap<u64, WindowSnapshot>>>,
}

impl ScreencastState {
    /// Rebuilds the output snapshot the DBus thread validates
    /// `RecordMonitor` connector names against. Call this whenever the
    /// output list changes (hotplug add/remove) -- see the two call
    /// sites in `backend/udev.rs`. The winit backend's output count
    /// never changes after `init`, so it has no call site.
    pub fn refresh_outputs<'a>(&self, outputs: impl IntoIterator<Item = &'a Output>) {
        let snapshot = outputs
            .into_iter()
            .filter_map(|output| {
                let mode = output.current_mode()?;
                let width = u32::try_from(mode.size.w).ok().filter(|&value| value > 0)?;
                let height = u32::try_from(mode.size.h).ok().filter(|&value| value > 0)?;
                Some(OutputSnapshot {
                    name: output.name(),
                    width,
                    height,
                })
            })
            .collect();
        *self.outputs.lock().unwrap() = snapshot;
    }

    pub(crate) fn refresh_windows(&self, windows: HashMap<u64, WindowSnapshot>) {
        *self.windows.lock().unwrap() = windows;
    }

    pub(crate) fn refresh_window(&self, window: WindowSnapshot) {
        self.windows.lock().unwrap().insert(window.id, window);
    }
}

impl Smallvil {
    /// Rebuilds the cross-thread window list used by Mutter `RecordWindow`.
    /// Called from the xdg commit lifecycle so map, unmap, and resize all
    /// become visible before a portal starts a stream.
    pub(crate) fn refresh_screencast_windows(&self) {
        let Some(screencast) = &self.screencast else {
            return;
        };
        let windows = self
            .foreign_toplevels
            .keys()
            .filter_map(|surface| {
                let window = self.mapped_toplevel_window(surface)?;
                let output = self.capture_output_for_screencast(surface)?;
                let scale = output.current_scale().fractional_scale();
                let size = window.geometry().size;
                let id = *self.foreign_toplevel_numeric_ids.get(surface)?;
                Some((
                    id,
                    WindowSnapshot {
                        id,
                        width: (size.w as f64 * scale).round().max(1.0) as u32,
                        height: (size.h as f64 * scale).round().max(1.0) as u32,
                    },
                ))
            })
            .collect();
        screencast.refresh_windows(windows);
    }

    /// Updates one mapped window after an ordinary surface commit. Map and
    /// unmap still rebuild the keyed snapshot because membership changed;
    /// resize/content commits only replace this entry.
    pub(crate) fn refresh_screencast_window(&self, surface: &WlSurface) {
        let Some(screencast) = &self.screencast else {
            return;
        };
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        let Some(output) = self.capture_output_for_screencast(surface) else {
            return;
        };
        let Some(&id) = self.foreign_toplevel_numeric_ids.get(surface) else {
            return;
        };
        let scale = output.current_scale().fractional_scale();
        let size = window.geometry().size;
        screencast.refresh_window(WindowSnapshot {
            id,
            width: (size.w as f64 * scale).round().max(1.0) as u32,
            height: (size.h as f64 * scale).round().max(1.0) as u32,
        });
    }
}

/// Registers the event channel with the event loop and spawns the DBus
/// service thread (a single DBus connection thread, matching niri's own
/// `try_start` shape per AGENT.md). The PipeWire thread starts lazily on
/// the first actual screencast session request instead, not here, since
/// there is no reason to hold a PipeWire connection open with zero
/// active casts.
pub fn init<'a>(
    loop_handle: &LoopHandle<'static, Smallvil>,
    outputs: impl IntoIterator<Item = &'a Output>,
) -> Option<ScreencastState> {
    let (sender, source) = channel::channel();
    if let Err(err) = loop_handle.insert_source(source, |event, _, state: &mut Smallvil| {
        let channel::Event::Msg(event) = event else {
            return;
        };
        match event {
            ScreencastEvent::FrameRequested { source, target } => match source {
                ScreencastSource::Output(name) => {
                    let output = state
                        .space
                        .outputs()
                        .find(|candidate| candidate.name() == name)
                        .cloned();
                    if let Some(output) = output {
                        state.queue_screencast_frame(output, target);
                    } else {
                        target.close();
                    }
                }
                ScreencastSource::Window(id) => {
                    let surface = state.foreign_toplevel_numeric_ids.iter().find_map(
                        |(surface, candidate)| (*candidate == id).then(|| surface.clone()),
                    );
                    if let Some(surface) = surface {
                        if let Some(output) = state.capture_output_for_screencast(&surface) {
                            state.queue_window_screencast_frame(output, surface, target);
                        } else {
                            target.close();
                        }
                    } else {
                        target.close();
                    }
                }
            },
            ScreencastEvent::PickSource {
                source_types,
                response,
            } => state.open_screencast_picker(source_types, response),
            ScreencastEvent::DmabufFrameRequested {
                source,
                dmabuf,
                draw_cursor,
                done,
            } => {
                use crate::capture::{CaptureCompletion, PendingCapture};
                let target = match source {
                    ScreencastSource::Output(name) => state
                        .space
                        .outputs()
                        .find(|output| output.name() == name)
                        .cloned()
                        .map(|output| (output, None)),
                    ScreencastSource::Window(id) => state
                        .foreign_toplevel_numeric_ids
                        .iter()
                        .find_map(|(surface, candidate)| {
                            (*candidate == id).then(|| surface.clone())
                        })
                        .and_then(|surface| {
                            state
                                .capture_output_for_screencast(&surface)
                                .map(|output| (output, Some(surface)))
                        }),
                };
                let Some((output, window)) = target else {
                    let _ = done.send(false);
                    return;
                };
                if window.is_none() && state.config.has_layer_capture_exclusions() {
                    let _ = done.send(false);
                    return;
                }
                state.queue_capture(PendingCapture {
                    output,
                    window,
                    draw_cursor,
                    region: None,
                    completion: CaptureCompletion::PipewireDmabuf { dmabuf, done },
                });
            }
        }
    }) {
        tracing::warn!(%err, "Failed to register screencast compositor channel");
        return None;
    }

    let state = ScreencastState {
        sender,
        outputs: Arc::new(Mutex::new(Vec::new())),
        windows: Arc::new(Mutex::new(HashMap::new())),
    };
    state.refresh_outputs(outputs);
    dbus::spawn(
        state.outputs.clone(),
        state.windows.clone(),
        state.sender.clone(),
    );
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_frame_clears_stale_pixels_and_close_is_terminal() {
        let target = FrameTarget::new(false);
        target.complete(Some(Arc::new(ScreencastFrame {
            pixels: vec![1, 2, 3, 4],
            width: 1,
            height: 1,
            stride: 4,
        })));
        assert!(target.frame.lock().unwrap().is_some());

        target.complete(None);
        assert!(target.frame.lock().unwrap().is_none());

        target.close();
        assert!(target.is_closed());
        assert!(target.frame.lock().unwrap().is_none());
    }
}
