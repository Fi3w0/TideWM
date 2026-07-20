//! Screencasting: `org.gnome.Mutter.ScreenCast` over the session DBus,
//! streamed through PipeWire. See AGENT.md's "Screencasting" section for
//! the full design writeup (why this path over `xdg-desktop-portal-wlr`,
//! why the render side reuses `capture.rs` rather than a parallel path,
//! and the verification bar before any of this can be trusted).
//!
//! **DBus interface implemented, PipeWire not yet.** `dbus` registers the
//! real `org.gnome.Mutter.ScreenCast`/`.Session`/`.Stream` objects on the
//! session bus and validates `RecordMonitor` against a live output
//! snapshot; `Session::Start` deliberately errors ("PipeWire streaming
//! not implemented yet") instead of hanging a real client forever
//! waiting for a `PipeWireStreamAdded` signal that will never come.
//! `pipewire_thread` is still an empty stub. See `dbus.rs` for exactly
//! what is and isn't implemented, and the CHANGELOG for how it was
//! verified.
//!
//! The DBus service thread never touches `Smallvil` directly -- every
//! existing `Smallvil` field assumes single-threaded access. It only
//! reads a shared, read-only output snapshot (`outputs` below, refreshed
//! by the main thread on hotplug) and, once `pipewire_thread` is real,
//! will send `ScreencastEvent`s back over the calloop channel set up
//! here. That channel is the only sanctioned way either thread affects
//! compositor state.

mod dbus;
mod pipewire_thread;

use std::sync::{Arc, Mutex};

use smithay::output::Output;
use smithay::reexports::calloop::{channel, LoopHandle};

use crate::state::Smallvil;

/// A cheap, DBus-thread-readable copy of the bits of `Output` that
/// `RecordMonitor`'s connector validation and `Stream::Parameters` need.
/// Never carries the real `Output` handle across the thread boundary --
/// `Output` is `Smallvil`-adjacent state, not `Send`-safe to treat as
/// shared-ownership from another thread the way this plain data is.
pub(crate) struct OutputSnapshot {
    pub(crate) name: String,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

/// Sent from the (not yet implemented) PipeWire thread to the main
/// compositor thread. No variants yet -- e.g. `FrameRequested { .. }`
/// once `pipewire_thread` is real. An empty enum here still round-trips
/// through the channel and event loop cleanly, so this compiles and can
/// be exercised (an always-empty `Msg` case) before any real variant
/// exists to send.
pub enum ScreencastEvent {}

/// Screencast subsystem handle.
pub struct ScreencastState {
    #[allow(dead_code)]
    sender: channel::Sender<ScreencastEvent>,
    outputs: Arc<Mutex<Vec<OutputSnapshot>>>,
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
                Some(OutputSnapshot {
                    name: output.name(),
                    width: mode.size.w,
                    height: mode.size.h,
                })
            })
            .collect();
        *self.outputs.lock().unwrap() = snapshot;
    }
}

/// Registers the event channel with the event loop and spawns the DBus
/// service thread (a single DBus connection thread, matching niri's own
/// `try_start` shape per AGENT.md). The PipeWire thread starts lazily on
/// the first actual screencast session request instead, not here, since
/// there is no reason to hold a PipeWire connection open with zero
/// active casts -- and it isn't implemented yet regardless.
pub fn init<'a>(
    loop_handle: &LoopHandle<'static, Smallvil>,
    outputs: impl IntoIterator<Item = &'a Output>,
) -> Option<ScreencastState> {
    let (sender, source) = channel::channel();
    loop_handle
        .insert_source(source, |event, _, _state: &mut Smallvil| {
            let channel::Event::Msg(event) = event else { return };
            match event {}
        })
        .ok()?;

    let state = ScreencastState {
        sender,
        outputs: Arc::new(Mutex::new(Vec::new())),
    };
    state.refresh_outputs(outputs);
    dbus::spawn(state.outputs.clone());
    Some(state)
}
