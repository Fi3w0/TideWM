//! PipeWire producer for monitor and per-window screencasts.
//!
//! This deliberately starts with mapped SHM buffers. TideWM's compositor
//! thread already has a verified GL readback path, so a process callback only
//! copies the newest owned BGRA frame into a PipeWire buffer. No GL, Wayland,
//! or `Smallvil` value crosses into this thread.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;

use super::{FrameTarget, ScreencastEvent, ScreencastSource};

pub(super) struct StreamHandle {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl StreamHandle {
    pub(super) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

pub(super) fn start(
    source: ScreencastSource,
    width: u32,
    height: u32,
    draw_cursor: bool,
    compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
) -> Result<(StreamHandle, u32), String> {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (stop, stop_rx) = mpsc::channel();
    let alive = Arc::new(AtomicBool::new(true));
    let alive_for_worker = alive.clone();
    let worker = std::thread::Builder::new()
        .name(match &source {
            ScreencastSource::Output(output) => format!("screencast-pw-{output}"),
            ScreencastSource::Window(id) => format!("screencast-pw-window-{id}"),
        })
        .spawn(move || {
            struct MarkStopped(Arc<AtomicBool>);
            impl Drop for MarkStopped {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _mark_stopped = MarkStopped(alive_for_worker);
            if let Err(err) = run(
                source,
                width,
                height,
                draw_cursor,
                compositor,
                stop_rx,
                started_tx.clone(),
            ) {
                let _ = started_tx.send(Err(err));
            }
        })
        .map_err(|err| err.to_string())?;

    match started_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(node_id)) => Ok((
            StreamHandle {
                stop,
                worker: Some(worker),
                alive,
            },
            node_id,
        )),
        Ok(Err(err)) => {
            let _ = stop.send(());
            let _ = worker.join();
            Err(err)
        }
        Err(_) => {
            // Do not join here: initialization may still be blocked inside a
            // PipeWire call and the public five-second timeout must remain a
            // real bound. Once initialization returns, the queued stop (or a
            // disconnected receiver) makes the worker exit immediately.
            let _ = stop.send(());
            Err("timed out creating PipeWire stream".into())
        }
    }
}

fn run(
    source: ScreencastSource,
    width: u32,
    height: u32,
    draw_cursor: bool,
    compositor: smithay::reexports::calloop::channel::Sender<ScreencastEvent>,
    stop: mpsc::Receiver<()>,
    started: mpsc::SyncSender<Result<u32, String>>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|err| err.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|err| err.to_string())?;
    let core = context.connect_rc(None).map_err(|err| err.to_string())?;
    let target = FrameTarget::new(draw_cursor);
    let node_name = match &source {
        ScreencastSource::Output(output) => format!("tidewm-screencast-{output}"),
        ScreencastSource::Window(id) => format!("tidewm-screencast-window-{id}"),
    };
    let stream = pw::stream::StreamBox::new(
        &core,
        "TideWM monitor capture",
        properties! {
            *pw::keys::NODE_NAME => node_name,
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::MEDIA_CLASS => "Video/Source",
        },
    )
    .map_err(|err| err.to_string())?;

    let started_for_listener = started.clone();
    let source_for_process = source.clone();
    let target_for_process = target.clone();
    let compositor_for_process = compositor.clone();
    let _listener = stream
        .add_local_listener::<()>()
        .state_changed(move |stream, _, _, new| match new {
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                let node_id = stream.node_id();
                if node_id != pw::constants::ID_ANY {
                    let _ = started_for_listener.try_send(Ok(node_id));
                }
            }
            pw::stream::StreamState::Error(err) => {
                let _ = started_for_listener.try_send(Err(err));
            }
            _ => {}
        })
        .process(move |stream, _| {
            target_for_process.request(&compositor_for_process, &source_for_process);
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let Some(destination) = data.data() else {
                return;
            };

            let expected_stride = width as usize * 4;
            let expected_len = expected_stride.saturating_mul(height as usize);
            // Clone only the Arc while holding the cross-thread lock; copying
            // a full frame must never stall the compositor's replacement of
            // the latest slot.
            let frame = target_for_process.frame.lock().unwrap().clone();
            if frame
                .as_ref()
                .is_some_and(|frame| frame.width != width || frame.height != height)
            {
                // PipeWire negotiated a fixed size for this node. Continuing
                // after an output modeset would produce an endless black
                // stream and request needless captures forever; disconnect
                // this node so the session can be started again at the fresh
                // dimensions.
                target_for_process.close();
            }
            let written = if let Some(frame) = frame.as_ref().filter(|frame| {
                frame.width == width
                    && frame.height == height
                    && frame.stride as usize >= expected_stride
            }) {
                let dst_len = destination.len().min(expected_len);
                let rows = height as usize;
                let mut written = 0;
                for row in 0..rows {
                    let src_start = row * frame.stride as usize;
                    let dst_start = row * expected_stride;
                    if src_start + expected_stride > frame.pixels.len()
                        || dst_start + expected_stride > dst_len
                    {
                        break;
                    }
                    destination[dst_start..dst_start + expected_stride]
                        .copy_from_slice(&frame.pixels[src_start..src_start + expected_stride]);
                    written += expected_stride;
                }
                written
            } else {
                let len = destination.len().min(expected_len);
                destination[..len].fill(0);
                len
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = expected_stride as i32;
            *chunk.size_mut() = written as u32;
        })
        .register()
        .map_err(|err| err.to_string())?;

    let format = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Id,
            pw::spa::param::video::VideoFormat::BGRA
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Rectangle,
            pw::spa::utils::Rectangle { width, height }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Fraction,
            pw::spa::utils::Fraction { num: 30, denom: 1 }
        ),
    );
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format),
    )
    .map_err(|err| err.to_string())?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("invalid serialized video format")?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|err| err.to_string())?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if target.is_closed() {
            break;
        }
        match stop.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(250)));
        if Instant::now() >= deadline && stream.node_id() == pw::constants::ID_ANY {
            let _ = started.try_send(Err("PipeWire did not assign a node id".into()));
            break;
        }
    }
    let _ = stream.disconnect();
    Ok(())
}
