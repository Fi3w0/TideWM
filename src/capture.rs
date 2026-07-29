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
            damage::OutputDamageTracker,
            element::{
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                AsRenderElements, Kind,
            },
            gles::{GlesRenderer, GlesTarget, GlesTexture},
            Bind, ExportMem, Offscreen,
        },
    },
    desktop::{layer_map_for_output, space::SpaceElement},
    input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
    utils::{Buffer as BufferCoords, IsAlive, Logical, Rectangle, Scale, Size, Transform},
    wayland::{
        compositor::with_states,
        image_copy_capture::{CaptureFailureReason, Frame},
        shm::with_buffer_contents_mut,
    },
};
use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1;

use crate::{backend::udev::OutputRenderElements, state::SessionLock, Smallvil};

/// A malicious or broken client must not be able to queue an unlimited
/// number of full-output GL readbacks before the backend renders a frame.
const MAX_PENDING_CAPTURES: usize = 64;
/// Even a bounded queue can freeze the compositor if all of its full-size
/// GL readbacks run in one event-loop turn. Spread bursts over frames while
/// still allowing both screencast cursor variants and ordinary screenshots
/// to make progress together.
const MAX_CAPTURE_RENDERS_PER_OUTPUT_FRAME: usize = 4;

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
    /// wlr-screencopy targeting a client-owned DMA-BUF. The compositor
    /// renders straight into this buffer and never maps/copies it through
    /// CPU memory.
    WlrDmabuf {
        frame: ZwlrScreencopyFrameV1,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        report_damage: bool,
    },
    /// PipeWire-owned DMA-BUF rendered in place. Completion is signalled
    /// only after the GL fence, before PipeWire requeues the buffer.
    #[cfg(feature = "screencast")]
    PipewireDmabuf {
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        done: std::sync::mpsc::SyncSender<bool>,
    },
    /// PipeWire monitor stream: the target is compositor-owned memory, not
    /// an untrusted Wayland buffer, but it intentionally shares the entire
    /// render/readback/privacy-exclusion path above the final copy.
    #[cfg(feature = "screencast")]
    Screencast(Vec<crate::screencast::FrameTarget>),
}

impl CaptureCompletion {
    fn fail(self, reason: CaptureFailureReason) {
        match self {
            Self::Ext(frame) => frame.fail(reason),
            Self::Wlr { frame, .. } => frame.failed(),
            Self::WlrDmabuf { frame, .. } => frame.failed(),
            #[cfg(feature = "screencast")]
            Self::PipewireDmabuf { done, .. } => {
                let _ = done.send(false);
            }
            #[cfg(feature = "screencast")]
            Self::Screencast(targets) => {
                for target in targets {
                    target.complete(None);
                }
            }
        }
    }

    fn output_unavailable(self) {
        match self {
            Self::Ext(frame) => frame.fail(CaptureFailureReason::Unknown),
            Self::Wlr { frame, .. } => frame.failed(),
            Self::WlrDmabuf { frame, .. } => frame.failed(),
            #[cfg(feature = "screencast")]
            Self::PipewireDmabuf { done, .. } => {
                let _ = done.send(false);
            }
            #[cfg(feature = "screencast")]
            Self::Screencast(targets) => {
                for target in targets {
                    target.close();
                }
            }
        }
    }
}

/// A validated capture request waiting for a backend render loop (which owns
/// the GL renderer) to produce the pixels.
pub struct PendingCapture {
    pub output: Output,
    /// A mapped toplevel surface for per-window capture. `None` means the
    /// full output. The output remains explicit because it selects the GL
    /// renderer/scale even when the window is hidden on another workspace.
    pub window: Option<WlSurface>,
    /// The client asked for the cursor to be composited into the capture.
    pub draw_cursor: bool,
    /// Buffer-space region of the output to copy out (full output when
    /// `None`). Only ever a crop of the readback; the render itself is
    /// always full-output.
    pub region: Option<Rectangle<i32, BufferCoords>>,
    pub completion: CaptureCompletion,
}

impl Smallvil {
    pub(crate) fn queue_capture(&mut self, capture: PendingCapture) {
        if self.pending_captures.len() >= MAX_PENDING_CAPTURES {
            tracing::warn!(
                limit = MAX_PENDING_CAPTURES,
                "Capture queue full; rejecting request"
            );
            capture.completion.fail(CaptureFailureReason::Unknown);
            return;
        }
        self.pending_captures.push(capture);
        self.request_redraw();
    }

    #[cfg(feature = "screencast")]
    pub(crate) fn queue_screencast_frame(
        &mut self,
        output: Output,
        target: crate::screencast::FrameTarget,
    ) {
        self.queue_capture(PendingCapture {
            output,
            window: None,
            draw_cursor: target.draw_cursor,
            region: None,
            completion: CaptureCompletion::Screencast(vec![target]),
        });
    }

    #[cfg(feature = "screencast")]
    pub(crate) fn queue_window_screencast_frame(
        &mut self,
        output: Output,
        surface: WlSurface,
        target: crate::screencast::FrameTarget,
    ) {
        self.queue_capture(PendingCapture {
            output,
            window: Some(surface),
            draw_cursor: false,
            region: None,
            completion: CaptureCompletion::Screencast(vec![target]),
        });
    }

    /// Drains a bounded batch of `PendingCapture`s targeting `output`,
    /// rendering each into its client-provided SHM buffer. Called from a backend's render loop
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

        // PipeWire consumers asking for the same output/cursor variant can
        // share one render and one owned frame. Without this fan-out, N
        // streams caused N full-size textures, readbacks, and CPU copies per
        // compositor frame.
        #[cfg(feature = "screencast")]
        {
            let mut regular = Vec::new();
            let mut with_cursor = Vec::new();
            let mut without_cursor = Vec::new();
            for capture in mine.drain(..) {
                let PendingCapture {
                    output,
                    window,
                    draw_cursor,
                    region,
                    completion,
                } = capture;
                match (window, region, completion) {
                    (None, None, CaptureCompletion::Screencast(mut targets)) => {
                        if draw_cursor {
                            with_cursor.append(&mut targets);
                        } else {
                            without_cursor.append(&mut targets);
                        }
                    }
                    (window, region, completion) => regular.push(PendingCapture {
                        output,
                        window,
                        draw_cursor,
                        region,
                        completion,
                    }),
                }
            }
            if !without_cursor.is_empty() {
                regular.push(PendingCapture {
                    output: output.clone(),
                    window: None,
                    draw_cursor: false,
                    region: None,
                    completion: CaptureCompletion::Screencast(without_cursor),
                });
            }
            if !with_cursor.is_empty() {
                regular.push(PendingCapture {
                    output: output.clone(),
                    window: None,
                    draw_cursor: true,
                    region: None,
                    completion: CaptureCompletion::Screencast(with_cursor),
                });
            }
            mine = regular;
        }

        if mine.len() > MAX_CAPTURE_RENDERS_PER_OUTPUT_FRAME {
            let deferred = mine.split_off(MAX_CAPTURE_RENDERS_PER_OUTPUT_FRAME);
            self.pending_captures.extend(deferred);
            self.request_redraw();
        }

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
            window,
            draw_cursor,
            region,
            mut completion,
        } = capture;

        macro_rules! fail {
            ($completion:expr, $reason:expr) => {
                $completion.fail($reason)
            };
        }

        let Some(mode) = output.current_mode() else {
            fail!(completion, CaptureFailureReason::Unknown);
            return;
        };
        let window_target = window
            .as_ref()
            .and_then(|surface| self.mapped_toplevel_window(surface));
        if window.is_some() && window_target.is_none() {
            fail!(completion, CaptureFailureReason::Unknown);
            return;
        }
        let scale = output.current_scale().fractional_scale();
        let size: Size<i32, BufferCoords> = if let Some(target) = &window_target {
            let logical = SpaceElement::geometry(target).size;
            Size::from((
                (logical.w as f64 * scale).round().max(1.0) as i32,
                (logical.h as f64 * scale).round().max(1.0) as i32,
            ))
        } else {
            Size::from((mode.size.w, mode.size.h))
        };
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

        let mut direct_dmabuf = match &mut completion {
            CaptureCompletion::WlrDmabuf { dmabuf, .. } => Some(dmabuf.clone()),
            #[cfg(feature = "screencast")]
            CaptureCompletion::PipewireDmabuf { dmabuf, .. } => Some(dmabuf.clone()),
            _ => None,
        };
        let mut texture: Option<GlesTexture> = if direct_dmabuf.is_none() {
            match renderer.create_buffer(Fourcc::Argb8888, size) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    tracing::warn!(%err, "Failed to allocate capture texture");
                    fail!(completion, CaptureFailureReason::Unknown);
                    return;
                }
            }
        } else {
            None
        };
        let bind_result = match direct_dmabuf.as_mut() {
            Some(dmabuf) => renderer.bind(dmabuf),
            None => renderer.bind(texture.as_mut().expect("capture texture exists")),
        };
        let mut target = match bind_result {
            Ok(target) => target,
            Err(err) => {
                tracing::warn!(%err, "Failed to bind capture target");
                fail!(completion, CaptureFailureReason::Unknown);
                return;
            }
        };

        // A toplevel source is rendered on its own transparent/black canvas
        // at the scale of its owning output. It includes subsurfaces and
        // popups belonging to that toplevel, but no neighboring windows,
        // compositor chrome, wallpaper, or pointer.
        if let Some(window_target) = window_target {
            let blocked = window_target.toplevel().is_some_and(|toplevel| {
                let (app_id, title) = self.toplevel_identity(toplevel.wl_surface());
                self.config
                    .resolve_window_rules(app_id.as_deref(), title.as_deref())
                    .block_capture
            });
            let opacity = window_target
                .toplevel()
                .and_then(|toplevel| self.window_opacity.get(toplevel.wl_surface()))
                .copied()
                .unwrap_or(1.0);
            let window_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = if blocked {
                Vec::new()
            } else {
                AsRenderElements::render_elements(
                    &window_target,
                    renderer,
                    (0, 0).into(),
                    Scale::from(scale),
                    opacity,
                )
            };
            let mut damage_tracker =
                OutputDamageTracker::new((size.w, size.h), 1.0, Transform::Normal);
            if let Err(err) = damage_tracker.render_output(
                renderer,
                &mut target,
                0,
                &window_elements,
                [0.0, 0.0, 0.0, 0.0],
            ) {
                tracing::warn!(%err, "Failed to render toplevel capture frame");
                fail!(completion, CaptureFailureReason::Unknown);
                return;
            }
            self.finish_capture_readback(renderer, target, size, rect, Vec::new(), completion);
            return;
        }

        let locked = !matches!(self.session_lock, SessionLock::Unlocked);

        // What the user sees: space + layer-shell surfaces (both come from
        // `render_output`) plus the toast OSD and any window-group tab
        // strips, plus the composited cursor when the client asked for it.
        // None of that applies while locked: a capture must show exactly what the
        // visible frame shows -- the lock content, never the desktop
        // underneath -- or a screenshot tool could read straight through
        // the lock. Skipped, not just covered, for the same reason the
        // visible-frame render loops skip it: `render_output` below pulls
        // layer-shell content from `layer_map_for_output` unconditionally,
        // independent of whatever `spaces` it's given.
        let mut elements: Vec<OutputRenderElements> = Vec::new();
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
                elements.push(OutputRenderElements::Composited(overview_element));
            }
            if let Some(toast_element) = self
                .toast
                .as_ref()
                .and_then(|toast| toast.render_element(renderer, mode.size))
            {
                elements.push(OutputRenderElements::Composited(toast_element));
            }
            if let Some(error_element) = self.config_error_element(&output, renderer) {
                elements.push(OutputRenderElements::Composited(error_element));
            }
            elements.extend(
                self.tab_strip_elements(renderer)
                    .into_iter()
                    .map(OutputRenderElements::Composited),
            );
            if self.should_show_welcome_hint() {
                if let Some(welcome_element) = self
                    .welcome_hint
                    .as_ref()
                    .and_then(|hint| hint.render_element(renderer, mode.size))
                {
                    elements.push(OutputRenderElements::Composited(welcome_element));
                }
            }
        }
        if !locked && composite_cursor && draw_cursor {
            let idle_hidden = self.config.cursor_hide_after_ms > 0
                && self.last_pointer_motion.elapsed()
                    >= std::time::Duration::from_millis(self.config.cursor_hide_after_ms as u64);
            let forced_visible = CursorImageStatus::Named(CursorIcon::Default);
            let hidden = CursorImageStatus::Hidden;
            let cursor_status = if idle_hidden {
                &hidden
            } else if matches!(self.cursor_status, CursorImageStatus::Hidden)
                && self.config.cursor_always_visible
            {
                &forced_visible
            } else {
                &self.cursor_status
            };
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
            match cursor_status {
                CursorImageStatus::Surface(cursor_surface) => {
                    let hotspot = with_states(cursor_surface, |states| {
                        states
                            .data_map
                            .get::<CursorImageSurfaceData>()
                            .map(|data| data.lock().unwrap().hotspot)
                            .unwrap_or_default()
                    });
                    let local = (pointer_loc - output_loc.to_f64()).to_physical(scale)
                        - hotspot.to_f64().to_physical(scale);
                    elements.extend(
                        render_elements_from_surface_tree(
                            renderer,
                            cursor_surface,
                            local.to_i32_round(),
                            scale,
                            1.0,
                            Kind::Unspecified,
                        )
                        .into_iter()
                        .map(OutputRenderElements::Cursor),
                    );
                }
                CursorImageStatus::Named(icon) => {
                    let local = (pointer_loc - output_loc.to_f64()).to_physical(scale);
                    let elapsed = self.start_time.elapsed();
                    let glyph = self
                        .cursor_theme
                        .as_mut()
                        .and_then(|theme| {
                            theme.render_element(renderer, local, scale as u32, elapsed, *icon)
                        })
                        .or_else(|| crate::cursor::fallback_glyph_element(renderer, local.into()));
                    if let Some(glyph) = glyph {
                        elements.push(OutputRenderElements::Composited(glyph));
                    }
                }
                CursorImageStatus::Hidden => {}
            }
        }

        // Not `from_output`: that inherits the output's advertised
        // transform, and the winit backend advertises `Flipped180` purely
        // to cancel its EGL surface's y-orientation at present time (see
        // `backend/winit.rs`) -- `render_output` multiplies the transform
        // into the GL projection (smithay `damage/mod.rs`), so inheriting
        // it bakes a real vertical flip into this offscreen texture and
        // breaks `finish_capture_readback`'s top-down orientation contract
        // (grim captures came out upside-down on winit, while real
        // hardware with a Normal transform was fine). `Transform::Normal`
        // matches what the window-capture path above always used. Known
        // simplification: a udev output configured rotated/flipped gets
        // its capture in logical (upright) orientation, not scanout
        // orientation -- untested territory before this change too.
        let mut damage_tracker = OutputDamageTracker::new(
            (size.w, size.h),
            output.current_scale().fractional_scale(),
            Transform::Normal,
        );
        let render_result = if locked {
            let lock_elements = self.lock_render_elements(&output, renderer);
            damage_tracker.render_output(
                renderer,
                &mut target,
                0,
                &lock_elements,
                [0.0, 0.0, 0.0, 1.0],
            )
        } else {
            // Same water-glass substitution the visible-frame loops apply
            // (`backend/winit.rs`) -- without it, a screenshot of a
            // water-glass window would show its plain, unrefracted content
            // instead of what's actually on screen, the exact "separate
            // render path forgot the new effect" bug class this codebase
            // already hit once with session-lock (see AGENT.md). Same
            // reasoning applies to ripples -- a screenshot mid-ripple
            // would otherwise drop the ring from the captured frame.
            // Ripple layers are respected just like the visible-frame
            // loops: AboveAll frontmost, then chrome-less AboveWindows,
            // then windows, then BelowWindows/wallpaper, then BelowAll.
            let ripple_layers = self.ripple_frame_elements(renderer, &output);
            let water_glass_surfaces = self.water_glass_eligible_surfaces(&output);
            let water_glass_elements =
                self.water_glass_frame_elements(renderer, &output, &water_glass_surfaces);
            let skip: &[WlSurface] = if water_glass_elements.is_empty() {
                &[]
            } else {
                &water_glass_surfaces
            };
            match self.desktop_render_elements(renderer, &output, skip) {
                Some(space_elements) => {
                    elements.extend(ripple_layers.above_all);
                    elements.extend(ripple_layers.above_windows);
                    elements.extend(water_glass_elements);
                    elements.extend(space_elements.into_iter().map(OutputRenderElements::Space));
                    elements.extend(ripple_layers.below_windows);
                    if let Some(wallpaper) = self.wallpaper_element(&output, renderer) {
                        elements.push(OutputRenderElements::Composited(wallpaper));
                    }
                    elements.extend(ripple_layers.below_all);
                    damage_tracker.render_output(
                        renderer,
                        &mut target,
                        0,
                        &elements,
                        [0.05, 0.05, 0.05, 1.0],
                    )
                }
                None => {
                    tracing::warn!("Failed to gather capture render elements");
                    fail!(completion, CaptureFailureReason::Unknown);
                    return;
                }
            }
        };
        let render_result = match render_result {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(%err, "Failed to render capture frame");
                fail!(completion, CaptureFailureReason::Unknown);
                return;
            }
        };

        let direct_completion = matches!(completion, CaptureCompletion::WlrDmabuf { .. });
        #[cfg(feature = "screencast")]
        let direct_completion =
            direct_completion || matches!(completion, CaptureCompletion::PipewireDmabuf { .. });
        if direct_completion {
            drop(target);
            // `ready` transfers the buffer back to the client. Unlike the
            // SHM readback path below, no map/copy operation implicitly
            // waits for GL, so explicitly wait for the render fence before
            // telling a consumer it may read/reuse the DMA-BUF.
            if let Err(err) = render_result.sync.wait() {
                tracing::warn!(%err, "DMA-BUF capture fence wait failed");
                completion.fail(CaptureFailureReason::Unknown);
                return;
            }
            match completion {
                CaptureCompletion::WlrDmabuf {
                    frame,
                    report_damage,
                    ..
                } => {
                    if report_damage {
                        frame.damage(0, 0, rect.size.w as u32, rect.size.h as u32);
                    }
                    let elapsed = self.start_time.elapsed();
                    let secs = elapsed.as_secs();
                    frame.ready(
                        (secs >> 32) as u32,
                        (secs & 0xFFFF_FFFF) as u32,
                        elapsed.subsec_nanos(),
                    );
                }
                #[cfg(feature = "screencast")]
                CaptureCompletion::PipewireDmabuf { done, .. } => {
                    let _ = done.send(true);
                }
                _ => unreachable!(),
            }
            return;
        }

        let excluded_rects: Vec<Rectangle<i32, BufferCoords>> = if locked {
            Vec::new()
        } else {
            let scale = output.current_scale().fractional_scale();
            let layer_map = layer_map_for_output(&output);
            layer_map
                .layers()
                .filter(|layer| self.config.layer_blocks_capture(layer.namespace()))
                .filter_map(|layer| layer_map.layer_geometry(layer))
                // Must use the same transform the capture render above
                // used (now always Normal), or the black-out rects land
                // mirrored/rotated relative to the actual image content.
                .map(|geo| logical_rect_to_buffer(geo, size, scale, Transform::Normal))
                .collect()
        };
        self.finish_capture_readback(renderer, target, size, rect, excluded_rects, completion);
    }

    fn finish_capture_readback(
        &mut self,
        renderer: &mut GlesRenderer,
        target: GlesTarget<'_>,
        size: Size<i32, BufferCoords>,
        rect: Rectangle<i32, BufferCoords>,
        excluded_rects: Vec<Rectangle<i32, BufferCoords>>,
        completion: CaptureCompletion,
    ) {
        let full_rect = Rectangle::from_size(size);
        let mapping = match renderer.copy_framebuffer(&target, full_rect, Fourcc::Argb8888) {
            Ok(mapping) => mapping,
            Err(err) => {
                tracing::warn!(%err, "Failed to read back capture frame");
                completion.fail(CaptureFailureReason::Unknown);
                return;
            }
        };
        drop(target);
        // Not `mapping.flipped()`: that describes raw glReadPixels output in
        // isolation (always bottom-up for GLES), but everything read back
        // here went through `render_output`/`GlesRenderer::render`, whose
        // projection multiplies in a constant `flip180` regardless of
        // target -- that already cancels the GL-readback orientation, so
        // the mapped pixels are already top-down. Reversing rows again here
        // double-flips the image (confirmed upside-down on real hardware
        // via grim before this fix).
        let pixels = match renderer.map_texture(&mapping) {
            Ok(pixels) => pixels,
            Err(err) => {
                tracing::warn!(%err, "Failed to map capture readback");
                completion.fail(CaptureFailureReason::Unknown);
                return;
            }
        };

        #[cfg(feature = "screencast")]
        if let CaptureCompletion::Screencast(targets) = &completion {
            let stride = size.w as usize * 4;
            let mut owned = vec![0u8; stride * size.h as usize];
            for row in 0..size.h as usize {
                let source_start = row * stride;
                let destination_start = row * stride;
                owned[destination_start..destination_start + stride]
                    .copy_from_slice(&pixels[source_start..source_start + stride]);
            }
            black_out_rects(&mut owned, size, full_rect, &excluded_rects);
            let frame = std::sync::Arc::new(crate::screencast::ScreencastFrame {
                pixels: owned,
                width: size.w as u32,
                height: size.h as u32,
                stride: stride as u32,
            });
            for target in targets {
                target.complete(Some(frame.clone()));
            }
            return;
        }

        let buffer = match &completion {
            CaptureCompletion::Ext(frame) => frame.buffer(),
            CaptureCompletion::Wlr { buffer, .. } => buffer.clone(),
            CaptureCompletion::WlrDmabuf { .. } => unreachable!(),
            #[cfg(feature = "screencast")]
            CaptureCompletion::PipewireDmabuf { .. } => unreachable!(),
            #[cfg(feature = "screencast")]
            CaptureCompletion::Screencast(_) => unreachable!(),
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
            let row_bytes = rect.size.w as usize * 4;
            let dst_end = shm_copy_end(offset, dst_stride, rows, row_bytes);
            if dst_end.is_none_or(|end| end > len)
                || row_bytes > dst_stride
                || pixels.len() < src_stride * size.h as usize
                || (rect.loc.x as usize) * 4 + (rect.size.w as usize) * 4 > src_stride
            {
                return false;
            }
            for row in 0..rows {
                let image_row = rect.loc.y as usize + row;
                let src_start = image_row * src_stride + rect.loc.x as usize * 4;
                let src = &pixels[src_start..src_start + row_bytes];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr(),
                        ptr.add(offset + row * dst_stride),
                        row_bytes,
                    );
                }
            }

            // Overwrite each excluded layer's rect with opaque black, same
            // B,G,R,A byte order as the real copy above. Intersected
            // against `rect` (the crop already applied to the real copy)
            // and re-bounded against `rows`/`dst_stride`/`len` again here,
            // since this is still writing into untrusted client memory.
            for excluded in &excluded_rects {
                let Some(overlap) = excluded.intersection(rect).filter(|r| !r.is_empty()) else {
                    continue;
                };
                let col_start = (overlap.loc.x - rect.loc.x) as usize;
                for local_row in 0..overlap.size.h as usize {
                    let dst_row = (overlap.loc.y - rect.loc.y) as usize + local_row;
                    if dst_row >= rows {
                        continue;
                    }
                    let row_start = offset + dst_row * dst_stride + col_start * 4;
                    let row_end = row_start + overlap.size.w as usize * 4;
                    if row_end > len {
                        continue;
                    }
                    for pixel_start in (row_start..row_end).step_by(4) {
                        unsafe {
                            std::ptr::write_bytes(ptr.add(pixel_start), 0, 3);
                            *ptr.add(pixel_start + 3) = 255;
                        }
                    }
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
            #[cfg(feature = "screencast")]
            (_, CaptureCompletion::Screencast(_)) => unreachable!(),
            (_, CaptureCompletion::WlrDmabuf { .. }) => unreachable!(),
            #[cfg(feature = "screencast")]
            (_, CaptureCompletion::PipewireDmabuf { .. }) => unreachable!(),
            (Ok(false), completion) => completion.fail(CaptureFailureReason::BufferConstraints),
            (Err(err), completion) => {
                tracing::warn!(%err, "Failed to access capture target buffer");
                completion.fail(CaptureFailureReason::BufferConstraints);
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
                capture.completion.output_unavailable();
            }
        }
        self.pending_captures = remaining;
    }

    /// A mapped output can temporarily remain in `Space` while DPMS has
    /// disabled its CRTC. There is no renderer target to service captures
    /// in that state, so fail them instead of leaving clients and stream
    /// workers waiting forever behind an output that cleanup still sees as
    /// logically present.
    pub(crate) fn fail_captures_for_output(&mut self, output: &Output) {
        if self.pending_captures.is_empty() {
            return;
        }
        let mut remaining = Vec::with_capacity(self.pending_captures.len());
        for capture in self.pending_captures.drain(..) {
            if &capture.output == output {
                capture.completion.output_unavailable();
            } else {
                remaining.push(capture);
            }
        }
        self.pending_captures = remaining;
    }
}

fn shm_copy_end(offset: usize, stride: usize, rows: usize, row_bytes: usize) -> Option<usize> {
    if rows == 0 {
        Some(offset)
    } else {
        stride
            .checked_mul(rows - 1)
            .and_then(|last_row| offset.checked_add(last_row))
            .and_then(|last_row| last_row.checked_add(row_bytes))
    }
}

/// Applies capture-exclusion rectangles to an owned BGRA frame. This is the
/// safe-memory equivalent of the raw-pointer overwrite used for Wayland SHM
/// below and is used by PipeWire's compositor-owned frame slot.
#[cfg(feature = "screencast")]
fn black_out_rects(
    pixels: &mut [u8],
    size: Size<i32, BufferCoords>,
    rect: Rectangle<i32, BufferCoords>,
    excluded_rects: &[Rectangle<i32, BufferCoords>],
) {
    let stride = size.w as usize * 4;
    for excluded in excluded_rects {
        let Some(overlap) = excluded.intersection(rect).filter(|area| !area.is_empty()) else {
            continue;
        };
        for row in overlap.loc.y..overlap.loc.y + overlap.size.h {
            let start = row as usize * stride + overlap.loc.x as usize * 4;
            let end = start + overlap.size.w as usize * 4;
            if end > pixels.len() {
                continue;
            }
            for pixel in pixels[start..end].chunks_exact_mut(4) {
                pixel.copy_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
}

/// Converts a layer surface's logical-space geometry (`LayerMap::layer_geometry`)
/// into the buffer-pixel space `PendingCapture`'s `size`/`rect` already use
/// (`output.current_mode().size`, i.e. already output-scaled and transformed).
/// This must apply rotation/flip as well as scale: the layer map reports
/// output-local logical coordinates while the captured pixels are in buffer
/// coordinates.
fn logical_rect_to_buffer(
    geo: Rectangle<i32, Logical>,
    buffer_size: Size<i32, BufferCoords>,
    scale: f64,
    transform: Transform,
) -> Rectangle<i32, BufferCoords> {
    let logical_size = buffer_size.to_f64().to_logical(scale, transform);
    geo.to_f64()
        .to_buffer(scale, transform, &logical_size)
        .to_i32_round()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_rect_to_buffer_scales_location_and_size_together() {
        let geo = Rectangle::new((10, 20).into(), (100, 50).into());
        let size = Size::from((1920, 1080));

        let unscaled = logical_rect_to_buffer(geo, size, 1.0, Transform::Normal);
        assert_eq!(unscaled, Rectangle::new((10, 20).into(), (100, 50).into()));

        let doubled = logical_rect_to_buffer(geo, size, 2.0, Transform::Normal);
        assert_eq!(doubled, Rectangle::new((20, 40).into(), (200, 100).into()));

        // A fractional scale (1.5x) must round, not truncate -- an
        // excluded rect one pixel too small at an edge would leak a sliver
        // of real content into the capture.
        let fractional = logical_rect_to_buffer(geo, size, 1.5, Transform::Normal);
        assert_eq!(
            fractional,
            Rectangle::new((15, 30).into(), (150, 75).into())
        );
    }

    #[test]
    fn logical_rect_to_buffer_applies_output_rotation() {
        let geo = Rectangle::new((10, 20).into(), (30, 40).into());
        // A 90x70 buffer rotated 90 degrees exposes a 70x90 logical area.
        let rotated = logical_rect_to_buffer(geo, Size::from((90, 70)), 1.0, Transform::_90);
        assert_eq!(rotated, Rectangle::new((30, 10).into(), (40, 30).into()));
    }

    #[test]
    fn shm_copy_bound_ends_at_last_rows_pixels_not_next_stride() {
        // Two four-byte rows with four bytes of padding between them need
        // 12 bytes, not two complete eight-byte strides.
        assert_eq!(shm_copy_end(0, 8, 2, 4), Some(12));
        assert_eq!(shm_copy_end(5, 8, 2, 4), Some(17));
        assert_eq!(shm_copy_end(5, 8, 0, 4), Some(5));
    }

    #[cfg(feature = "screencast")]
    #[test]
    fn screencast_privacy_rect_is_opaque_black_and_bounded() {
        let size: Size<i32, BufferCoords> = (4, 3).into();
        let mut pixels = vec![7; 4 * 3 * 4];
        black_out_rects(
            &mut pixels,
            size,
            Rectangle::from_size(size),
            &[Rectangle::new((1, 1).into(), (2, 1).into())],
        );

        assert_eq!(&pixels[20..24], &[0, 0, 0, 255]);
        assert_eq!(&pixels[24..28], &[0, 0, 0, 255]);
        assert_eq!(&pixels[16..20], &[7, 7, 7, 7]);
        assert_eq!(&pixels[28..32], &[7, 7, 7, 7]);
    }
}
