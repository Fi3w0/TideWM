//! PipeWire producer for monitor and per-window screencasts.
//!
//! Negotiates DMA-BUF first with mapped MemFd as the portable fallback.
//! PipeWire-owned DMA-BUF fds are duplicated and sent to the compositor for
//! direct GL rendering with an explicit completion fence; SHM continues to
//! copy the newest owned BGRA readback. No GL, Wayland, or `Smallvil` value
//! crosses into this thread.

use std::{
    os::fd::BorrowedFd,
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

#[derive(Clone, Copy)]
struct BuffersProperty(u32);

impl BuffersProperty {
    fn as_raw(self) -> u32 {
        self.0
    }
}

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
        .param_changed(move |stream, _, id, param| {
            if id != spa::param::ParamType::Format.as_raw() || param.is_none() {
                return;
            }
            // SPA_PARAM_BUFFERS_* values are stable protocol enum values
            // from spa/param/buffers.h.
            //
            // MemFd only, DMA-BUF disabled for now: offering DmaBuf here
            // (even with a concrete Linear modifier negotiated cleanly on
            // the format side) made PipeWire's own core buffer allocator
            // fail identically both with `Modifier::Invalid` and
            // `Modifier::Linear` -- confirmed live, `pw.link: ... allocating
            // -> error error alloc buffers: Operation not supported (-95)`
            // in `pipewire.service`'s own log, i.e. a PipeWire-core
            // allocation failure, not a format-pod mistake. The generic
            // `pw_stream` path apparently can't fabricate a DMA-BUF on this
            // system without the producer allocating it itself (real
            // compositors do this via their own GBM device, handing
            // pre-allocated buffers to PipeWire through `add_buffer` +
            // `ALLOC_BUFFERS` -- a real subproject, not a pod tweak, and
            // hardware-facing code of the same class that froze the machine
            // once already, see `TileMoveGrab`/CHANGELOG 0.15.1). This is
            // the discriminating build: MemFd-only tells us whether OBS
            // even accepts it before that work is worth starting.
            let memfd_mask = 1i32 << spa::buffer::DataType::MemFd.as_raw();
            let buffers = pw::spa::pod::object!(
                pw::spa::utils::SpaTypes::ObjectParamBuffers,
                pw::spa::param::ParamType::Buffers,
                pw::spa::pod::property!(BuffersProperty(1), Int, 4i32),
                pw::spa::pod::property!(BuffersProperty(2), Int, 1i32),
                pw::spa::pod::property!(BuffersProperty(3), Int, (width * height * 4) as i32),
                pw::spa::pod::property!(BuffersProperty(4), Int, (width * 4) as i32),
                pw::spa::pod::property!(
                    BuffersProperty(6),
                    pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(
                        pw::spa::utils::Choice(
                            pw::spa::utils::ChoiceFlags::empty(),
                            pw::spa::utils::ChoiceEnum::Flags {
                                default: memfd_mask,
                                flags: vec![memfd_mask],
                            },
                        ),
                    ))
                ),
            );
            let Ok(serialized) = pw::spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &pw::spa::pod::Value::Object(buffers),
            ) else {
                return;
            };
            let bytes = serialized.0.into_inner();
            if let Some(pod) = Pod::from_bytes(&bytes) {
                let _ = stream.update_params(&mut [pod]);
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let expected_stride = width as usize * 4;
            let expected_len = expected_stride.saturating_mul(height as usize);

            if data.type_() == spa::buffer::DataType::DmaBuf && data.fd() >= 0 {
                // PipeWire's own allocator (see the Linear-modifier comment
                // on the format enum below) may pad the stride for GPU
                // alignment -- this buffer's chunk metadata, already
                // populated by whoever allocated it before this callback
                // ever ran, reports the real one. Assuming width*4 here
                // would silently produce a sheared/skewed image the moment
                // the negotiated size needs padding; fall back to width*4
                // only if the reported stride is unset.
                let stride = match data.chunk().stride() {
                    stride if stride > 0 => stride as u32,
                    _ => expected_stride as u32,
                };
                // SAFETY: PipeWire owns this fd for at least the duration of
                // the process callback. Duplicate it before sending the
                // buffer to the compositor thread.
                let borrowed = unsafe { BorrowedFd::borrow_raw(data.fd()) };
                if let Ok(fd) = borrowed.try_clone_to_owned() {
                    let mut builder = smithay::backend::allocator::dmabuf::Dmabuf::builder(
                        (width as i32, height as i32),
                        smithay::backend::allocator::Fourcc::Argb8888,
                        smithay::backend::allocator::Modifier::Linear,
                        smithay::backend::allocator::dmabuf::DmabufFlags::empty(),
                    );
                    if builder.add_plane(fd, 0, data.as_raw().mapoffset, stride) {
                        if let Some(dmabuf) = builder.build() {
                            let (done_tx, done_rx) = mpsc::sync_channel(1);
                            let sent = compositor_for_process
                                .send(ScreencastEvent::DmabufFrameRequested {
                                    source: source_for_process.clone(),
                                    dmabuf,
                                    draw_cursor,
                                    done: done_tx,
                                })
                                .is_ok();
                            if sent && done_rx.recv_timeout(Duration::from_millis(100)) == Ok(true)
                            {
                                let size = (stride as usize).saturating_mul(height as usize);
                                let chunk = data.chunk_mut();
                                *chunk.offset_mut() = 0;
                                *chunk.stride_mut() = stride as i32;
                                *chunk.size_mut() = size as u32;
                                return;
                            }
                        }
                    }
                }
                *data.chunk_mut().size_mut() = 0;
                return;
            }

            target_for_process.request(&compositor_for_process, &source_for_process);
            let Some(destination) = data.data() else {
                return;
            };

            // Clone only the Arc while holding the cross-thread lock; copying
            // a full frame must never stall the compositor's replacement of
            // the latest slot.
            let frame = target_for_process.frame.lock().unwrap().clone();
            // TEMP DIAGNOSTIC: see whether this thread is ever observing a
            // completed frame at all, and whether its dimensions actually
            // match what this stream negotiated.
            tracing::warn!(
                stream_width = width,
                stream_height = height,
                frame_present = frame.is_some(),
                frame_width = frame.as_ref().map(|f| f.width),
                frame_height = frame.as_ref().map(|f| f.height),
                "DIAG process() frame snapshot"
            );
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

    fn format_properties(width: u32, height: u32) -> Vec<pw::spa::pod::Property> {
        vec![
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
        ]
    }

    // MemFd only for now -- see the matching comment on the Buffers param
    // above for why DMA-BUF is disabled rather than tuned further. A format
    // enum without a modifier property is what tells PipeWire's negotiation
    // this format is SHM/MemFd-only; adding one back is the first step if
    // DMA-BUF export gets a real (app-allocates-the-buffer) implementation.
    let format_plain = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: format_properties(width, height),
    };
    let plain_values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format_plain),
    )
    .map_err(|err| err.to_string())?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&plain_values).ok_or("invalid serialized video format")?];
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
