//! One artwork source/timeline for every deep window. File IO and decoding
//! stay on a worker; only one requested frame can be in flight. Smithay's
//! memory buffer caches imports by renderer context, including multiple GPUs.

use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::{Cursor, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

use image::{AnimationDecoder, ImageDecoder};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                Element, Id, Kind, RenderElement,
            },
            gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture},
            utils::{CommitCounter, DamageSet, OpaqueRegions},
            ContextId, Renderer,
        },
    },
    reexports::calloop::ping::Ping,
    utils::{
        user_data::UserDataMap, Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
    },
};

use crate::config::DepthBackgroundFit;

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;
const MIN_DELAY: Duration = Duration::from_millis(20);

struct DecodedFrame {
    size: (i32, i32),
    pixels: Vec<u8>,
    delay: Option<Duration>,
    opaque: bool,
}

struct Worker {
    requests: mpsc::SyncSender<()>,
    results: mpsc::Receiver<Result<DecodedFrame, String>>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

pub struct DepthBackground {
    wake: Ping,
    path: Option<PathBuf>,
    worker: Option<Worker>,
    buffer: Option<MemoryRenderBuffer>,
    size: (i32, i32),
    delay: Option<Duration>,
    deadline: Option<Instant>,
    visible: bool,
    waiting: bool,
    failed_contexts: HashSet<ContextId<GlesTexture>>,
    imported_contexts: HashSet<ContextId<GlesTexture>>,
    presented_frames: u64,
    error: Option<String>,
    /// Survives asset resets: a slow cancelled decode must finish before a
    /// replacement can allocate its decoder state. Queued stale jobs exit.
    decode_gate: Arc<Mutex<()>>,
}

impl DepthBackground {
    pub fn new(wake: Ping) -> Self {
        Self {
            wake,
            path: None,
            worker: None,
            buffer: None,
            size: (0, 0),
            delay: None,
            deadline: None,
            visible: false,
            waiting: false,
            failed_contexts: HashSet::new(),
            imported_contexts: HashSet::new(),
            presented_frames: 0,
            error: None,
            decode_gate: Arc::new(Mutex::new(())),
        }
    }

    /// Reload invalidates even a same-path asset (file-watch edits/recovery).
    /// Dropping both channels cancels an old decoder without joining on the
    /// compositor thread. At most its already-started decode can finish.
    pub fn reset(&mut self) {
        let gate = self.decode_gate.clone();
        *self = Self::new(self.wake.clone());
        self.decode_gate = gate;
    }

    pub fn configure(&mut self, path: Option<&Path>, visible: bool, now: Instant) {
        if self.path.as_deref() != path {
            self.reset();
            self.path = path.map(Path::to_path_buf);
        }
        if visible != self.visible {
            self.visible = visible;
            // Resume on the retained frame; do not catch up hidden time.
            self.deadline = if visible {
                self.delay.map(|delay| now + delay)
            } else {
                None
            };
        }
        if visible && self.worker.is_none() && self.buffer.is_none() {
            if let Some(path) = &self.path {
                let (requests, commands) = mpsc::sync_channel(1);
                let (results, frames) = mpsc::sync_channel(1);
                let path = path.clone();
                let wake = self.wake.clone();
                let gate = self.decode_gate.clone();
                let cancelled = Arc::new(AtomicBool::new(false));
                let worker_cancelled = cancelled.clone();
                match std::thread::Builder::new()
                    .name("depth-artwork".into())
                    .spawn(move || {
                        if worker_cancelled.load(Ordering::Acquire) {
                            return;
                        }
                        // This mutex is only acquired by workers, never by
                        // the compositor. It bounds heavy decode memory even
                        // across rapid IPC reselections/config watcher reloads.
                        let _decode = gate.lock().unwrap_or_else(|error| error.into_inner());
                        if worker_cancelled.load(Ordering::Acquire) {
                            return;
                        }
                        let mut source = match Source::open(&path) {
                            Ok(source) => source,
                            Err(error) => {
                                let _ = results.send(Err(error));
                                wake.ping();
                                return;
                            }
                        };
                        loop {
                            if worker_cancelled.load(Ordering::Acquire) {
                                break;
                            }
                            let frame = source.next();
                            let done = frame.as_ref().map_or(true, |frame| frame.delay.is_none());
                            if results.send(frame).is_err() {
                                break;
                            }
                            wake.ping();
                            if done || commands.recv().is_err() {
                                break;
                            }
                        }
                    }) {
                    Ok(_) => {
                        self.worker = Some(Worker {
                            requests,
                            results: frames,
                            cancelled,
                        });
                        self.waiting = true;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Cannot start depth artwork decoder");
                        self.error = Some(error.to_string());
                        // Keep a disconnected worker to avoid frame-rate retries.
                        self.worker = Some(Worker {
                            requests,
                            results: frames,
                            cancelled,
                        });
                    }
                }
            }
        }
    }

    /// Called by the decoder wakeup or one shared GIF deadline, never per window.
    pub fn poll(&mut self, now: Instant) -> bool {
        let result = self
            .worker
            .as_ref()
            .and_then(|worker| worker.results.try_recv().ok());
        let mut changed = false;
        if let Some(result) = result {
            self.waiting = false;
            match result {
                Ok(frame) => {
                    let size = frame.size;
                    let buffer = self.buffer.get_or_insert_with(|| {
                        MemoryRenderBuffer::new(Fourcc::Argb8888, size, 1, Transform::Normal, None)
                    });
                    let mut context = buffer.render();
                    if self.size != size {
                        context.resize(size);
                    }
                    context
                        .draw(|pixels| {
                            pixels.copy_from_slice(&frame.pixels);
                            Ok::<_, std::convert::Infallible>(vec![Rectangle::from_size(
                                size.into(),
                            )])
                        })
                        .unwrap();
                    context.update_opaque_regions(
                        frame
                            .opaque
                            .then(|| vec![Rectangle::from_size(size.into())]),
                    );
                    self.size = size;
                    self.presented_frames = self.presented_frames.saturating_add(1);
                    self.delay = frame.delay;
                    self.deadline = if self.visible {
                        frame.delay.map(|delay| now + delay)
                    } else {
                        None
                    };
                    changed = true;
                }
                Err(error) => {
                    tracing::warn!(%error, path = ?self.path, "Depth artwork unavailable; using schematic");
                    self.error = Some(error);
                    self.buffer = None;
                    self.imported_contexts.clear();
                    self.size = (0, 0);
                    self.deadline = None;
                    self.delay = None;
                    changed = true;
                }
            }
        }
        if self.visible && !self.waiting && self.deadline.is_some_and(|due| now >= due) {
            self.deadline = None;
            if let Some(worker) = &self.worker {
                self.waiting = worker.requests.try_send(()).is_ok();
            }
        }
        changed
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn status(&self) -> (bool, bool, bool, bool) {
        (
            self.buffer.is_some(),
            self.visible,
            self.delay.is_some(),
            self.error.is_some(),
        )
    }

    pub fn animating(&self) -> bool {
        self.visible && self.delay.is_some()
    }

    pub fn can_present(&self) -> bool {
        self.failed_contexts.is_empty() || !self.imported_contexts.is_empty()
    }

    pub fn estimated_texture_bytes(&self) -> u64 {
        self.size.0 as u64 * self.size.1 as u64 * 4 * self.imported_contexts.len() as u64
    }

    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({
            "loaded": self.buffer.is_some(), "visible": self.visible,
            "animated": self.delay.is_some(), "decoding": self.waiting,
            "width": self.size.0, "height": self.size.1,
            "current_frame_bytes": if self.buffer.is_some() { self.size.0 as u64 * self.size.1 as u64 * 4 } else { 0 },
            "renderer_contexts": self.imported_contexts.len(),
            "presented_frames": self.presented_frames, "error": self.error,
            "scope": "shared artwork only; excludes decoder scratch, encoded file, title strips and driver overhead"
        })
    }

    pub fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        area: Rectangle<i32, Logical>,
        fit: DepthBackgroundFit,
        scale: f64,
        id: Id,
    ) -> Option<DepthImageElement> {
        let context = renderer.context_id();
        if self.failed_contexts.contains(&context) {
            return None;
        }
        let buffer = self.buffer.as_ref()?;
        let (source, destination) = fit_rect(self.size, area, fit);
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            destination.loc.to_f64().to_physical(scale),
            buffer,
            None,
            Some(source),
            Some(destination.size),
            Kind::Unspecified,
        ) {
            Ok(inner) => {
                self.imported_contexts.insert(context);
                Some(DepthImageElement { inner, id })
            }
            Err(error) => {
                tracing::warn!(%error, "Depth artwork import failed for renderer context");
                self.failed_contexts.insert(context);
                None
            }
        }
    }
}

fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_FRAME_BYTES * 2);
    limits
}

fn check_size((width, height): (u32, u32)) -> Result<(), String> {
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || u64::from(width) * u64::from(height) * 4 > MAX_FRAME_BYTES
    {
        Err("depth artwork exceeds the 8192px / 64 MiB frame limit".into())
    } else {
        Ok(())
    }
}

enum Source {
    Static(Option<image::RgbaImage>),
    Gif {
        bytes: Arc<[u8]>,
        frames: image::Frames<'static>,
        count: usize,
    },
}

impl Source {
    fn open(path: &Path) -> Result<Self, String> {
        // A FIFO/device selected accidentally must not strand a decoder.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err("depth artwork must be a regular file of at most 64 MiB".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("depth artwork file grew beyond 64 MiB".into());
        }
        let bytes: Arc<[u8]> = bytes.into();
        let format = image::guess_format(&bytes).map_err(|e| e.to_string())?;
        if format == image::ImageFormat::Gif {
            let frames = gif_frames(bytes.clone())?;
            Ok(Self::Gif {
                bytes,
                frames,
                count: 0,
            })
        } else {
            let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
            reader.limits(limits());
            let decoder = reader.into_decoder().map_err(|e| e.to_string())?;
            check_size(decoder.dimensions())?;
            let image = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
            Ok(Self::Static(Some(image.into_rgba8())))
        }
    }

    fn next(&mut self) -> Result<DecodedFrame, String> {
        let (pixels, delay) = match self {
            Self::Static(image) => (image.take().ok_or("static artwork already decoded")?, None),
            Self::Gif {
                bytes,
                frames,
                count,
            } => {
                let frame = match frames.next() {
                    Some(frame) => {
                        *count += 1;
                        frame
                    }
                    None => {
                        if *count == 0 {
                            return Err("GIF contains no frames".into());
                        }
                        let single = *count == 1;
                        *frames = gif_frames(bytes.clone())?;
                        *count = 1;
                        let frame = frames
                            .next()
                            .ok_or("GIF contains no frames")?
                            .map_err(|e| e.to_string())?;
                        if single {
                            return Ok(convert(frame.into_buffer(), None));
                        }
                        Ok(frame)
                    }
                }
                .map_err(|e| e.to_string())?;
                let (numerator, denominator) = frame.delay().numer_denom_ms();
                let delay = Duration::from_secs_f64(
                    f64::from(numerator) / f64::from(denominator.max(1)) / 1000.0,
                )
                .max(MIN_DELAY);
                (frame.into_buffer(), Some(delay))
            }
        };
        Ok(convert(pixels, delay))
    }
}

fn gif_frames(bytes: Arc<[u8]>) -> Result<image::Frames<'static>, String> {
    let mut decoder =
        image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    check_size(decoder.dimensions())?;
    decoder.set_limits(limits()).map_err(|e| e.to_string())?;
    Ok(decoder.into_frames())
}

fn convert(image: image::RgbaImage, delay: Option<Duration>) -> DecodedFrame {
    let size = (image.width() as i32, image.height() as i32);
    let mut pixels = image.into_raw();
    for pixel in pixels.chunks_exact_mut(4) {
        let [r, g, b, a] = [pixel[0], pixel[1], pixel[2], pixel[3]];
        let premultiply = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
        pixel.copy_from_slice(&[premultiply(b), premultiply(g), premultiply(r), a]);
    }
    let opaque = pixels.chunks_exact(4).all(|pixel| pixel[3] == 255);
    DecodedFrame {
        size,
        pixels,
        delay,
        opaque,
    }
}

fn fit_rect(
    size: (i32, i32),
    area: Rectangle<i32, Logical>,
    fit: DepthBackgroundFit,
) -> (Rectangle<f64, Logical>, Rectangle<i32, Logical>) {
    let (width, height) = (size.0 as f64, size.1 as f64);
    let (dw, dh) = (area.size.w.max(1) as f64, area.size.h.max(1) as f64);
    let mut source = Rectangle::from_size(Size::from((width, height)));
    let mut dest = area;
    match fit {
        DepthBackgroundFit::Cover => {
            let scale = (dw / width).max(dh / height);
            source.size = (dw / scale, dh / scale).into();
            source.loc = (
                (width - source.size.w) / 2.0,
                (height - source.size.h) / 2.0,
            )
                .into();
        }
        DepthBackgroundFit::Contain => {
            let scale = (dw / width).min(dh / height);
            dest.size = (
                (width * scale).round().max(1.0) as i32,
                (height * scale).round().max(1.0) as i32,
            )
                .into();
            dest.loc += Point::from((
                (area.size.w - dest.size.w) / 2,
                (area.size.h - dest.size.h) / 2,
            ));
        }
    }
    (source, dest)
}

/// Sharing an image must not share its *placement* identity. Independent IDs
/// keep damage/occlusion correct when one source is drawn in multiple windows.
pub struct DepthImageElement {
    inner: MemoryRenderBufferRenderElement<GlesRenderer>,
    id: Id,
}

impl Element for DepthImageElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for DepthImageElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        self.inner.draw(frame, src, dst, damage, opaque, cache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "tide-depth-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn png(&self) -> PathBuf {
            let path = self.0.join("art.png");
            image::RgbaImage::from_pixel(4, 2, image::Rgba([200, 100, 50, 255]))
                .save(&path)
                .unwrap();
            path
        }
        fn gif(&self, single: bool) -> PathBuf {
            let path = self.0.join("art.gif");
            let mut encoder =
                image::codecs::gif::GifEncoder::new(std::fs::File::create(&path).unwrap());
            for (i, color) in [[255, 0, 0, 255], [0, 0, 255, 255]].into_iter().enumerate() {
                if single && i == 1 {
                    break;
                }
                encoder
                    .encode_frame(image::Frame::from_parts(
                        image::RgbaImage::from_pixel(4, 2, image::Rgba(color)),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(if i == 0 { 50 } else { 70 }, 1),
                    ))
                    .unwrap();
            }
            drop(encoder);
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn receive(background: &mut DepthBackground) {
        let end = Instant::now() + Duration::from_secs(3);
        while background.waiting {
            background.poll(Instant::now());
            assert!(Instant::now() < end, "decoder did not respond");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn cover_crops_and_contain_centres_without_stretching() {
        let area = Rectangle::new((20, 30).into(), (300, 400).into());
        let (source, dest) = fit_rect((800, 400), area, DepthBackgroundFit::Cover);
        assert_eq!(
            source,
            Rectangle::new((250.0, 0.0).into(), (300.0, 400.0).into())
        );
        assert_eq!(dest, area);
        let (source, dest) = fit_rect((800, 400), area, DepthBackgroundFit::Contain);
        assert_eq!(source.size, (800.0, 400.0).into());
        assert_eq!(dest, Rectangle::new((20, 155).into(), (300, 150).into()));
    }

    #[test]
    fn rgba_is_premultiplied_bgra_and_static_sources_stop() {
        let frame = convert(
            image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 128, 64, 128])),
            None,
        );
        assert_eq!(frame.pixels, [32, 64, 128, 128]);
        assert!(!frame.opaque);
        let fixture = Fixture::new();
        let frame = Source::open(&fixture.png()).unwrap().next().unwrap();
        assert_eq!(frame.size, (4, 2));
        assert_eq!(frame.pixels[..4], [50, 100, 200, 255]);
        assert!(frame.opaque);
        assert!(frame.delay.is_none());
    }

    #[test]
    fn gif_stream_preserves_delays_and_loops_one_timeline() {
        let fixture = Fixture::new();
        let mut source = Source::open(&fixture.gif(false)).unwrap();
        let first = source.next().unwrap();
        let second = source.next().unwrap();
        let third = source.next().unwrap();
        assert_eq!(first.pixels[..4], [0, 0, 255, 255]);
        assert_eq!(second.pixels[..4], [255, 0, 0, 255]);
        assert_eq!(third.pixels, first.pixels);
        assert_eq!(first.delay, Some(Duration::from_millis(50)));
        assert_eq!(second.delay, Some(Duration::from_millis(70)));
        let mut single = Source::open(&fixture.gif(true)).unwrap();
        single.next().unwrap();
        assert!(single.next().unwrap().delay.is_none());
    }

    #[test]
    fn hidden_gif_stops_requests_and_resumes_without_catching_up() {
        let fixture = Fixture::new();
        let path = fixture.gif(false);
        let (wake, _source) = smithay::reexports::calloop::ping::make_ping().unwrap();
        let mut background = DepthBackground::new(wake);
        let now = Instant::now();
        background.configure(Some(&path), false, now);
        assert!(background.worker.is_none(), "hidden asset must not load");
        background.configure(Some(&path), true, now);
        receive(&mut background);
        assert_eq!(background.presented_frames, 1);
        background.configure(Some(&path), false, now);
        background.poll(now + Duration::from_secs(300));
        assert!(background.deadline().is_none());
        assert!(!background.waiting);
        assert_eq!(background.presented_frames, 1);
        let resume = Instant::now();
        background.configure(Some(&path), true, resume);
        assert_eq!(
            background.deadline(),
            Some(resume + Duration::from_millis(50))
        );
        background.poll(background.deadline().unwrap());
        receive(&mut background);
        assert_eq!(background.presented_frames, 2);
        background.reset();
        assert!(background.buffer.is_none());
        assert!(background.worker.is_none());
    }

    #[test]
    fn static_image_is_shared_and_failure_waits_for_explicit_reload() {
        let fixture = Fixture::new();
        let path = fixture.png();
        let (wake, _source) = smithay::reexports::calloop::ping::make_ping().unwrap();
        let mut background = DepthBackground::new(wake);
        background.configure(Some(&path), true, Instant::now());
        receive(&mut background);
        for _ in 0..100 {
            background.configure(Some(&path), true, Instant::now());
            assert!(!background.poll(Instant::now()));
        }
        assert_eq!(background.presented_frames, 1);
        assert!(background.deadline().is_none());
        let missing = fixture.0.join("missing.png");
        background.configure(Some(&missing), true, Instant::now());
        receive(&mut background);
        assert!(background.error.is_some());
        std::fs::copy(&path, &missing).unwrap();
        background.configure(Some(&missing), true, Instant::now());
        assert!(!background.poll(Instant::now()));
        assert!(background.buffer.is_none());
        background.reset();
        background.configure(Some(&missing), true, Instant::now());
        receive(&mut background);
        assert!(background.buffer.is_some());
        assert!(background.error.is_none());
    }

    #[test]
    fn rapid_reselection_cancels_queued_decoders_without_blocking_the_compositor() {
        let fixture = Fixture::new();
        let path = fixture.png();
        let (wake, _source) = smithay::reexports::calloop::ping::make_ping().unwrap();
        let mut background = DepthBackground::new(wake);
        let gate = background.decode_gate.clone();
        // Simulate one previous slow decode while selections keep arriving.
        let slow_decode = gate.lock().unwrap();
        let mut cancelled = Vec::new();
        for _ in 0..12 {
            background.reset();
            assert!(Arc::ptr_eq(&gate, &background.decode_gate));
            background.configure(Some(&path), true, Instant::now());
            cancelled.push(background.worker.as_ref().unwrap().cancelled.clone());
        }
        assert!(cancelled[..11]
            .iter()
            .all(|flag| flag.load(Ordering::Acquire)));
        assert!(!cancelled[11].load(Ordering::Acquire));
        assert!(background.buffer.is_none());
        drop(slow_decode);
        receive(&mut background);
        assert_eq!(background.presented_frames, 1);
        assert!(background.buffer.is_some());
    }

    #[test]
    fn oversized_and_invalid_sources_are_rejected_before_large_allocation() {
        assert!(check_size((8192, 8192)).is_err());
        assert!(check_size((0, 1)).is_err());
        assert!(check_size((4096, 4096)).is_ok());
        let fixture = Fixture::new();
        let path = fixture.0.join("bad.gif");
        std::fs::write(&path, b"invalid image").unwrap();
        assert!(Source::open(&path).is_err());
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(Source::open(&path).is_err());
        // A large logical GIF canvas is rejected using only its header.
        std::fs::write(&path, b"GIF89a\xff\xff\xff\xff\x00\x00\x00;").unwrap();
        assert!(Source::open(&path).is_err());
    }

    #[test]
    fn real_renderer_contexts_share_cpu_pixels_with_independent_damage_ids() {
        use smithay::backend::renderer::element::UnderlyingStorage;
        let Some(mut first) = crate::shader_effect::software_renderer() else {
            eprintln!("SKIP: no software EGL device");
            return;
        };
        let Some(mut second) = crate::shader_effect::software_renderer() else {
            eprintln!("SKIP: no second software EGL context");
            return;
        };
        assert_ne!(first.context_id(), second.context_id());
        let fixture = Fixture::new();
        let path = fixture.png();
        let (wake, _source) = smithay::reexports::calloop::ping::make_ping().unwrap();
        let mut background = DepthBackground::new(wake);
        background.configure(Some(&path), true, Instant::now());
        receive(&mut background);
        let area = Rectangle::new((20, 30).into(), (300, 400).into());
        let a = background
            .render_element(&mut first, area, DepthBackgroundFit::Cover, 1.25, Id::new())
            .unwrap();
        let b = background
            .render_element(&mut first, area, DepthBackgroundFit::Cover, 1.25, Id::new())
            .unwrap();
        let c = background
            .render_element(
                &mut second,
                area,
                DepthBackgroundFit::Cover,
                1.25,
                Id::new(),
            )
            .unwrap();
        assert_ne!(a.id(), b.id());
        assert_eq!(a.current_commit(), b.current_commit());
        assert_eq!(
            a.geometry(1.25.into()),
            Rectangle::new((25, 38).into(), (375, 500).into())
        );
        let pixels = |element: &DepthImageElement, renderer: &mut GlesRenderer| match element
            .inner
            .underlying_storage(renderer)
            .unwrap()
        {
            UnderlyingStorage::Memory(memory) => memory.as_ptr() as usize,
            _ => panic!("expected shared memory storage"),
        };
        assert_eq!(pixels(&a, &mut first), pixels(&b, &mut first));
        assert_eq!(pixels(&a, &mut first), pixels(&c, &mut second));
        assert_eq!(background.imported_contexts.len(), 2);
        assert_eq!(background.presented_frames, 1);
        // Returning to the first renderer must reuse it, not evict/redecode.
        background
            .render_element(&mut first, area, DepthBackgroundFit::Cover, 1.0, Id::new())
            .unwrap();
        assert_eq!(background.imported_contexts.len(), 2);
        assert_eq!(background.presented_frames, 1);
        eprintln!("PROVED: two EGL contexts, one CPU frame, independent placement IDs");
    }
}
