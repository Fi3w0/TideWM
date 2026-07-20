//! Screen-capture plumbing shared by the capture protocol handlers
//! (`handlers/capture.rs` for ext-image-copy-capture-v1,
//! `handlers/screencopy.rs` for wlr-screencopy-unstable-v1) and both
//! backends' render loops.
//!
//! A capture request arrives on the Wayland dispatch side, which has no
//! access to a GL renderer -- those live in the backends. The request is
//! validated, queued as a `PendingCapture`, and drained by the backend's
//! render loop, the only place an EGL context is known to be current. The
//! actual capture renders the output exactly like the visible frame does
//! (same `render_output` convenience path the winit backend uses, so windows
//! and layer-shell surfaces stack identically), reads the pixels back into a
//! CPU mapping, and copies them into the client-provided SHM buffer. Both
//! protocols share that whole path; only the completion event differs.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen, TextureMapping, damage::OutputDamageTracker,
            element::memory::MemoryRenderBufferRenderElement,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    input::pointer::CursorImageStatus,
    output::Output,
    reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    utils::{Buffer as BufferCoords, IsAlive, Rectangle, Size, Transform},
    wayland::{
        image_copy_capture::{CaptureFailureReason, Frame},
        shm::with_buffer_contents_mut,
    },
};
use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1;

use crate::{state::SessionLock, Smallvil};

/// How a drained capture reports completion to its client. Everything up to
/// this point (render, readback, SHM write) is protocol-independent.
pub enum CaptureCompletion {
    /// ext-image-copy-capture-v1: buffer is attached to the frame itself.
    Ext(Frame),
    /// wlr-screencopy-unstable-v1: buffer arrives with the `copy` request;
    /// `report_damage` asks for the v2 full-damage event before `ready`.
    Wlr {
        frame: ZwlrScreencopyFrameV1,
        buffer: WlBuffer,
        report_damage: bool,
    },
}

/// A validated capture request waiting for a backend render loop (which owns
/// the GL renderer) to produce the pixels.
pub struct PendingCapture {
    pub output: Output,
    /// The client asked for the cursor to be composited into the capture.
    pub draw_cursor: bool,
    /// Buffer-space region of the output to copy out (full output when
    /// `None`). Only ever a crop of the readback; the render itself is
    /// always full-output.
    pub region: Option<Rectangle<i32, BufferCoords>>,
    pub completion: CaptureCompletion,
}

impl Smallvil {
    /// Drains every `PendingCapture` targeting `output`, rendering each into
    /// its client-provided SHM buffer. Called from a backend's render loop
    /// with the backend's renderer. `composite_cursor` is set by the udev
    /// backend, the only place TideWM itself draws a cursor (under winit the
    /// host compositor's cursor is never part of the frame anyway).
    pub fn render_pending_captures(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        composite_cursor: bool,
    ) {
        if self.pending_captures.is_empty() {
            return;
        }

        let mut mine = Vec::new();
        let mut remaining = Vec::with_capacity(self.pending_captures.len());
        for capture in self.pending_captures.drain(..) {
            if &capture.output == output {
                mine.push(capture);
            } else {
                remaining.push(capture);
            }
        }
        self.pending_captures = remaining;

        for capture in mine {
            self.render_one_capture(renderer, capture, composite_cursor);
        }
    }

    fn render_one_capture(
        &mut self,
        renderer: &mut GlesRenderer,
        capture: PendingCapture,
        composite_cursor: bool,
    ) {
        let PendingCapture {
            output,
            draw_cursor,
            region,
            completion,
        } = capture;

        macro_rules! fail {
            ($completion:expr, $reason:expr) => {
                match $completion {
                    CaptureCompletion::Ext(frame) => frame.fail($reason),
                    CaptureCompletion::Wlr { frame, .. } => frame.failed(),
                }
            };
        }

        let Some(mode) = output.current_mode() else {
            fail!(completion, CaptureFailureReason::Unknown);
            return;
        };
        let size: Size<i32, BufferCoords> = Size::from((mode.size.w, mode.size.h));
        let full_rect = Rectangle::from_size(size);
        // Region was already clamped to the output at queue time; re-check
        // against the *current* mode in case it changed since.
        let Some(rect) = region
            .and_then(|region| full_rect.intersection(region))
            .filter(|rect| !rect.is_empty())
            .or(region.is_none().then_some(full_rect))
        else {
            fail!(completion, CaptureFailureReason::BufferConstraints);
            return;
        };

        let mut texture: GlesTexture = match renderer.create_buffer(Fourcc::Argb8888, size) {
            Ok(texture) => texture,
            Err(err) => {
                tracing::warn!(%err, "Failed to allocate capture texture");
                fail!(completion, CaptureFailureReason::Unknown);
                return;
            }
        };
        let mut target = match renderer.bind(&mut texture) {
            Ok(target) => target,
            Err(err) => {
                tracing::warn!(%err, "Failed to bind capture texture");
                fail!(completion, CaptureFailureReason::Unknown);
                return;
            }
        };

        let locked = !matches!(self.session_lock, SessionLock::Unlocked);

        // What the user sees: space + layer-shell surfaces (both come from
        // `render_output`) plus the toast OSD and any window-group tab
        // strips, plus the composited cursor when the client asked for it.
        // A client-set cursor surface (`CursorImageStatus::Surface`) is not
        // composited yet -- only the xcursor/fallback glyph path is. None of
        // that applies while locked: a capture must show exactly what the
        // visible frame shows -- the lock content, never the desktop
        // underneath -- or a screenshot tool could read straight through
        // the lock. Skipped, not just covered, for the same reason the
        // visible-frame render loops skip it: `render_output` below pulls
        // layer-shell content from `layer_map_for_output` unconditionally,
        // independent of whatever `spaces` it's given.
        let mut custom: Vec<MemoryRenderBufferRenderElement<GlesRenderer>> = Vec::new();
        if !locked {
            // Pushed first so it ends up topmost (index 0 is the front,
            // this codebase's established render-element-list convention)
            // -- a screenshot taken while the overview is open shows it,
            // same as it shows a toast or a tab strip; this matches how
            // both of those are already captured rather than excluded.
            if let Some(overview_element) = self
                .overview
                .as_ref()
                .filter(|overview| overview.output_name() == output.name())
                .and_then(|overview| overview.render_element(renderer))
            {
                custom.push(overview_element);
            }
            if let Some(toast_element) =
                self.toast.as_ref().and_then(|toast| toast.render_element(renderer, mode.size))
            {
                custom.push(toast_element);
            }
            custom.extend(self.tab_strip_elements(renderer));
            if self.should_show_welcome_hint() {
                if let Some(welcome_element) =
                    self.welcome_hint.as_ref().and_then(|hint| hint.render_element(renderer, mode.size))
                {
                    custom.push(welcome_element);
                }
            }
        }
        if !locked && composite_cursor && draw_cursor {
            if let CursorImageStatus::Named(icon) = &self.cursor_status {
                let icon = *icon;
                let scale = output.current_scale().fractional_scale();
                let output_loc = self
                    .space
                    .output_geometry(&output)
                    .map(|geo| geo.loc)
                    .unwrap_or_default();
                let pointer_loc = self
                    .seat
                    .get_pointer()
                    .map(|pointer| pointer.current_location())
                    .unwrap_or_default();
                let local = (pointer_loc - output_loc.to_f64()).to_physical(scale);
                let elapsed = self.start_time.elapsed();
                let glyph = self
                    .cursor_theme
                    .as_mut()
                    .and_then(|theme| theme.render_element(renderer, local, scale as u32, elapsed, icon))
                    .or_else(|| crate::cursor::fallback_glyph_element(renderer, local.into()));
                if let Some(glyph) = glyph {
                    custom.push(glyph);
                }
            }
        }

        let mut damage_tracker = OutputDamageTracker::from_output(&output);
        let render_result = if locked {
            let lock_elements = self.lock_render_elements(&output, renderer);
            damage_tracker.render_output(renderer, &mut target, 0, &lock_elements, [0.0, 0.0, 0.0, 1.0])
        } else {
            smithay::desktop::space::render_output::<
                _,
                MemoryRenderBufferRenderElement<GlesRenderer>,
                _,
                _,
            >(
                &output,
                renderer,
                &mut target,
                1.0,
                0,
                [&self.space],
                &custom,
                &mut damage_tracker,
                [0.05, 0.05, 0.05, 1.0],
            )
        };
        if let Err(err) = render_result {
            tracing::warn!(%err, "Failed to render capture frame");
            fail!(completion, CaptureFailureReason::Unknown);
            return;
        }

        let mapping = match renderer.copy_framebuffer(&target, full_rect, Fourcc::Argb8888) {
            Ok(mapping) => mapping,
            Err(err) => {
                tracing::warn!(%err, "Failed to read back capture frame");
                fail!(completion, CaptureFailureReason::Unknown);
                return;
            }
        };
        drop(target);
        let flipped = mapping.flipped();
        let pixels = match renderer.map_texture(&mapping) {
            Ok(pixels) => pixels,
            Err(err) => {
                tracing::warn!(%err, "Failed to map capture readback");
                fail!(completion, CaptureFailureReason::Unknown);
                return;
            }
        };

        let buffer = match &completion {
            CaptureCompletion::Ext(frame) => frame.buffer(),
            CaptureCompletion::Wlr { buffer, .. } => buffer.clone(),
        };

        // The client buffer was already validated against the advertised
        // constraints before the request was queued, but it is still
        // untrusted client input: clamp the copy to whatever the buffer
        // actually reports. Readback is ARGB8888 (B,G,R,A bytes in memory),
        // identical layout to wl_shm's Argb8888/Xrgb8888, so rows copy
        // across verbatim.
        //
        // `unsafe` justification: Smithay hands out the shm pool as a raw
        // pointer deliberately -- creating a Rust slice into it is UB
        // because the client may mutate the pool concurrently (see
        // `with_buffer_contents_mut`'s own docs). Bounded raw-pointer row
        // writes, each length-checked against the pool length first, are
        // the intended usage.
        let copy = with_buffer_contents_mut(&buffer, |ptr, len, meta| {
            let src_stride = size.w as usize * 4;
            let dst_stride = meta.stride as usize;
            let rows = (rect.size.h as usize).min(meta.height as usize);
            let offset = meta.offset as usize;
            let dst_end = match offset.checked_add(dst_stride.saturating_mul(rows)) {
                Some(end) => end,
                None => return false,
            };
            if dst_end > len
                || pixels.len() < src_stride * size.h as usize
                || (rect.loc.x as usize) * 4 + (rect.size.w as usize) * 4 > src_stride
            {
                return false;
            }
            let row_bytes = rect.size.w as usize * 4;
            for row in 0..rows {
                let image_row = rect.loc.y as usize + row;
                let src_row = if flipped {
                    size.h as usize - 1 - image_row
                } else {
                    image_row
                };
                let src_start = src_row * src_stride + rect.loc.x as usize * 4;
                let src = &pixels[src_start..src_start + row_bytes];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr(),
                        ptr.add(offset + row * dst_stride),
                        row_bytes,
                    );
                }
            }
            true
        });

        let elapsed = self.start_time.elapsed();
        match (copy, completion) {
            (Ok(true), CaptureCompletion::Ext(frame)) => {
                frame.success(Transform::Normal, None, elapsed)
            }
            (
                Ok(true),
                CaptureCompletion::Wlr {
                    frame,
                    report_damage,
                    ..
                },
            ) => {
                if report_damage {
                    frame.damage(0, 0, rect.size.w as u32, rect.size.h as u32);
                }
                let secs = elapsed.as_secs();
                frame.ready(
                    (secs >> 32) as u32,
                    (secs & 0xFFFFFFFF) as u32,
                    elapsed.subsec_nanos(),
                );
            }
            (Ok(false), completion) => fail!(completion, CaptureFailureReason::BufferConstraints),
            (Err(err), completion) => {
                tracing::warn!(%err, "Failed to access capture target buffer");
                fail!(completion, CaptureFailureReason::BufferConstraints);
            }
        }
    }

    /// Capture-protocol housekeeping, run from both backends' cleanup ticks
    /// next to `PopupManager::cleanup`. Drops dead sessions (the owned
    /// `Session` stops itself on drop, so it must be kept until then) and
    /// fails queued frames whose output went away (hot-unplug), which would
    /// otherwise sit in `pending_captures` forever.
    pub fn cleanup_capture(&mut self) {
        self.capture_sessions.retain(|session| session.alive());
        self.image_copy_capture_state.cleanup();

        let mut remaining = Vec::with_capacity(self.pending_captures.len());
        for capture in self.pending_captures.drain(..) {
            if self.space.outputs().any(|output| output == &capture.output) {
                remaining.push(capture);
            } else {
                match capture.completion {
                    CaptureCompletion::Ext(frame) => frame.fail(CaptureFailureReason::Unknown),
                    CaptureCompletion::Wlr { frame, .. } => frame.failed(),
                }
            }
        }
        self.pending_captures = remaining;
    }
}
