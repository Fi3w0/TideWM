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

use std::{cell::RefCell, rc::Rc, time::Instant};

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
    desktop::layer_map_for_output,
    input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    reexports::{
        calloop::{generic::Generic, Interest, Mode, PostAction},
        wayland_server::{
            backend::ClientId,
            protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
        },
    },
    utils::{Buffer as BufferCoords, IsAlive, Logical, Point, Rectangle, Scale, Size, Transform},
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
/// Reserve queue capacity for other capture clients during a burst. Normal
/// screenshot and stream clients keep at most one frame in flight.
const MAX_PENDING_CAPTURES_PER_CLIENT: usize = 8;
/// Even a bounded queue can freeze the compositor if all of its full-size
/// GL readbacks run in one event-loop turn. Spread bursts over frames while
/// still allowing both screencast cursor variants and ordinary screenshots
/// to make progress together.
const MAX_CAPTURE_RENDERS_PER_OUTPUT_FRAME: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureQueueLimit {
    Client,
    Output,
    Global,
}

fn capture_queue_limit(
    total: usize,
    per_client: Option<usize>,
    per_output: usize,
    output_count: usize,
) -> Option<CaptureQueueLimit> {
    let per_output_limit = MAX_PENDING_CAPTURES
        .div_ceil(output_count.max(1))
        .max(MAX_CAPTURE_RENDERS_PER_OUTPUT_FRAME);
    if per_client.is_some_and(|count| count >= MAX_PENDING_CAPTURES_PER_CLIENT) {
        Some(CaptureQueueLimit::Client)
    } else if output_count > 1 && per_output >= per_output_limit {
        Some(CaptureQueueLimit::Output)
    } else if total >= MAX_PENDING_CAPTURES {
        Some(CaptureQueueLimit::Global)
    } else {
        None
    }
}

/// Pixel size of TideWM's upright, output-local offscreen capture target.
/// A rotated scanout swaps the logical axes; it must not make the capture
/// renderer and the protocol's region coordinates disagree about width and
/// height.
pub(crate) fn output_capture_size(output: &Output) -> Option<Size<i32, BufferCoords>> {
    let mode = output.current_mode()?;
    let transformed = output.current_transform().transform_size(mode.size);
    Some(Size::from((transformed.w.max(1), transformed.h.max(1))))
}

/// Hands a directly-rendered DMA-BUF capture back to its consumer once the
/// render fence is known signaled. Split out so it can run either inline
/// (fence already reached) or later, from a calloop callback watching the
/// exported fence FD -- see the `direct_completion` branch of
/// `render_one_capture`, M-03 in `report.md`.
fn complete_dmabuf_capture(
    start_time: Instant,
    rect: Rectangle<i32, BufferCoords>,
    completion: CaptureCompletion,
) {
    match completion {
        CaptureCompletion::WlrDmabuf {
            frame,
            report_damage,
            ..
        } => {
            if report_damage {
                frame.damage(0, 0, rect.size.w as u32, rect.size.h as u32);
            }
            let elapsed = start_time.elapsed();
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
}

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
    /// Wayland client responsible for this request. Internal PipeWire work has
    /// no Wayland owner and remains bounded by the output and global limits.
    pub client_id: Option<ClientId>,
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
        let per_client = capture.client_id.as_ref().map(|client_id| {
            self.pending_captures
                .iter()
                .filter(|pending| pending.client_id.as_ref() == Some(client_id))
                .count()
        });
        let per_output = self
            .pending_captures
            .iter()
            .filter(|pending| pending.output == capture.output)
            .count();
        let output_count = self.space.outputs().count();
        if let Some(reason) = capture_queue_limit(
            self.pending_captures.len(),
            per_client,
            per_output,
            output_count,
        ) {
            tracing::warn!(
                ?reason,
                total = self.pending_captures.len(),
                per_client,
                per_output,
                output_count,
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
            client_id: None,
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
            client_id: None,
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
                    client_id,
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
                        client_id,
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
                    client_id: None,
                    output: output.clone(),
                    window: None,
                    draw_cursor: false,
                    region: None,
                    completion: CaptureCompletion::Screencast(without_cursor),
                });
            }
            if !with_cursor.is_empty() {
                regular.push(PendingCapture {
                    client_id: None,
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
            client_id: _,
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
        let locked = !matches!(self.session_lock, SessionLock::Unlocked);
        // Toplevel capture has no lock composition, so fail before resolving
        // client content whenever the session is locked.
        if locked && window.is_some() {
            fail!(completion, CaptureFailureReason::Unknown);
            return;
        }
        let window_target = window
            .as_ref()
            .and_then(|surface| self.mapped_toplevel_window(surface));
        if window.is_some() && window_target.is_none() {
            fail!(completion, CaptureFailureReason::Unknown);
            return;
        }
        let scale = output.current_scale().fractional_scale();
        let (size, window_origin): (Size<i32, BufferCoords>, _) =
            if let Some(target) = &window_target {
                let logical = target.bbox_with_popups();
                (
                    Size::from((
                        (logical.size.w as f64 * scale).round().max(1.0) as i32,
                        (logical.size.h as f64 * scale).round().max(1.0) as i32,
                    )),
                    Point::from((-logical.loc.x, -logical.loc.y))
                        .to_physical_precise_round(Scale::from(scale)),
                )
            } else {
                (
                    output_capture_size(&output)
                        .unwrap_or_else(|| Size::from((mode.size.w, mode.size.h))),
                    Point::default(),
                )
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
                self.resolve_window_rules_for(toplevel.wl_surface())
                    .block_capture
            });
            let opacity = window_target
                .toplevel()
                .map(|toplevel| self.window_render_alpha(toplevel.wl_surface()))
                .unwrap_or(1.0);
            let window_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = if blocked {
                Vec::new()
            } else {
                AsRenderElements::render_elements(
                    &window_target,
                    renderer,
                    window_origin,
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

        // Mirror visible composition. While locked, omit the entire desktop
        // path because `render_output` would otherwise include layer-shell
        // content independently of the supplied spaces.
        let mut elements: Vec<OutputRenderElements> = Vec::new();
        if !locked {
            // Element index zero is frontmost, so visible overlays are pushed first.
            if let Some(depth_deck_element) = self
                .depth_deck_overlay
                .as_ref()
                .filter(|deck| deck.output_name() == output.name())
                .and_then(|deck| deck.render_element(renderer))
            {
                elements.push(OutputRenderElements::Composited(depth_deck_element));
            }
            if let Some(overview_element) = self
                .overview
                .as_ref()
                .filter(|overview| overview.output_name() == output.name())
                .and_then(|overview| overview.render_element(renderer))
            {
                elements.push(OutputRenderElements::Composited(overview_element));
            }
            if let Some(toast_element) = self.toast_element(&output, renderer) {
                elements.push(OutputRenderElements::Composited(toast_element));
            }
            if let Some(error_element) = self.config_error_element(&output, renderer) {
                elements.push(OutputRenderElements::Composited(error_element));
            }
            elements.extend(
                self.tab_strip_elements(renderer, &output)
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
            // Pointer-lock coordinates are stale; never capture a frozen glyph.
            let pointer_locked = self.pointer_is_locked();
            let forced_visible = CursorImageStatus::Named(CursorIcon::Default);
            let hidden = CursorImageStatus::Hidden;
            let cursor_status = if idle_hidden || pointer_locked {
                &hidden
            } else if matches!(self.cursor_status, CursorImageStatus::Hidden)
                && self.config.cursor_always_visible
            {
                &forced_visible
            } else {
                &self.cursor_status
            };
            let output_scale = output.current_scale();
            let scale = output_scale.fractional_scale();
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
                            theme.render_element(renderer, local, output_scale, elapsed, *icon)
                        })
                        .or_else(|| crate::cursor::fallback_glyph_element(renderer, local.into()));
                    if let Some(glyph) = glyph {
                        elements.push(OutputRenderElements::Composited(glyph));
                    }
                }
                CursorImageStatus::Hidden => {}
            }
        }

        // Capture is top-down logical content, so do not inherit winit's
        // presentation-only `Flipped180` transform. Rotated udev outputs also
        // remain upright here rather than exposing scanout orientation.
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
            // Use the visible frame's effect substitutions and z-order:
            // AboveAll, chrome-less AboveWindows, windows, BelowWindows and
            // wallpaper, then BelowAll.
            let workspace_transition = self.workspace_transition_frame_element(renderer, &output);
            let workspace_glide = self.workspace_glide_frame_element(&output);
            let depth_transition = self.depth_transition_frame_element(renderer, &output);
            let closing_windows = self.closing_window_frame_elements(renderer, &output);
            let Some(placements) = self.render_placements(&output) else {
                fail!(completion, CaptureFailureReason::Unknown);
                return;
            };
            let glass_surfaces = self.glass_eligible_surfaces(&placements);
            #[allow(clippy::mutable_key_type)]
            let mut glass_layers =
                self.glass_layer_elements(renderer, &output, &placements, &glass_surfaces);
            let (depth_elements, depth_surfaces) =
                self.depth_frame_elements(renderer, &output, &placements);
            // Glass windows render in their normal z-slot; only
            // depth-replaced windows are skipped.
            let skip = depth_surfaces;
            // The canvas grid, caustics, and BelowWindows ripples all sit
            // between windows and the wallpaper, passed INTO the walk so a
            // layer-shell wallpaper engine can't cover them.
            let ripple_layers = self.ripple_frame_elements(renderer, &output);
            let ocean_canvas = self.ocean_canvas_frame_element(renderer, &output);
            let caustics = self.caustics_frame_element(renderer, &output);
            let backdrop: Vec<OutputRenderElements> = ocean_canvas
                .into_iter()
                .chain(caustics)
                .chain(ripple_layers.below_windows)
                .collect();
            match self.desktop_render_elements(
                renderer,
                &output,
                &placements,
                &skip,
                &mut glass_layers,
                backdrop,
            ) {
                Some(space_elements) => {
                    elements.extend(ripple_layers.above_all);
                    elements.extend(depth_transition);
                    elements.extend(ripple_layers.above_windows);
                    elements.extend(workspace_transition);
                    elements.extend(workspace_glide);
                    elements.extend(closing_windows);
                    elements.extend(depth_elements);
                    elements.extend(space_elements);
                    if let Some(wallpaper) = self.wallpaper_element(&output, renderer) {
                        elements.push(OutputRenderElements::Wallpaper(wallpaper));
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
            // waits for GL, so the render fence must be observed before a
            // consumer may read/reuse the DMA-BUF. Blocking this thread on
            // it stalls input and every other client's protocol dispatch
            // for as long as a slow or wedged GPU takes to signal, so the
            // fence is exported and watched from the event loop instead of
            // waited on synchronously -- the same non-blocking completion
            // niri's screencopy/PipeWire cast paths use for this exact
            // problem (`Screencopy::submit_after_sync`,
            // `Cast::queue_after_sync`).
            let sync = render_result.sync;
            if sync.is_reached() {
                complete_dmabuf_capture(self.start_time, rect, completion);
                return;
            }
            match sync.export() {
                Some(sync_fd) => {
                    let start_time = self.start_time;
                    // `insert_source` only returns the source, not the
                    // callback, on failure, so `completion` is parked in a
                    // shared cell rather than moved by value -- the failure
                    // branch below can still recover and complete it
                    // instead of silently dropping a client's request.
                    let completion_cell = Rc::new(RefCell::new(Some(completion)));
                    let callback_cell = Rc::clone(&completion_cell);
                    let result = self.loop_handle.insert_source(
                        Generic::new(sync_fd, Interest::READ, Mode::OneShot),
                        move |_, _, _state: &mut Smallvil| {
                            if let Some(completion) = callback_cell.borrow_mut().take() {
                                complete_dmabuf_capture(start_time, rect, completion);
                            }
                            Ok(PostAction::Remove)
                        },
                    );
                    if let Err(err) = result {
                        tracing::warn!(
                            %err,
                            "Failed to watch DMA-BUF capture fence; completing without waiting for it"
                        );
                        if let Some(completion) = completion_cell.borrow_mut().take() {
                            complete_dmabuf_capture(start_time, rect, completion);
                        }
                    }
                }
                None => {
                    // The fence exists but this driver can't export it as a
                    // native fence FD (pre-EGL_ANDROID_native_fence_sync),
                    // outside this project's declared platform scope
                    // (recent kernel/Mesa). Fall back to a blocking wait
                    // rather than hand back a buffer the GPU may not have
                    // finished writing.
                    if let Err(err) = sync.wait() {
                        tracing::warn!(%err, "DMA-BUF capture fence wait failed");
                        completion.fail(CaptureFailureReason::Unknown);
                        return;
                    }
                    complete_dmabuf_capture(self.start_time, rect, completion);
                }
            }
            return;
        }

        let excluded_rects: Vec<Rectangle<i32, BufferCoords>> = if locked {
            // Only `above_lock_screen` layers are even rendered while
            // locked (`lock_render_elements`); `block_capture` still has to
            // black those specific ones out here or a screenshot could read
            // a namespace the maintainer explicitly excluded, same hazard
            // `block_capture` exists for in the unlocked branch below.
            let scale = output.current_scale().fractional_scale();
            let layer_map = layer_map_for_output(&output);
            layer_map
                .layers()
                .filter(|layer| {
                    self.config.layer_above_lock_screen(layer.namespace())
                        && self.config.layer_blocks_capture(layer.namespace())
                })
                .filter_map(|layer| layer_map.layer_geometry(layer))
                .map(|geo| logical_rect_to_buffer(geo, size, scale, Transform::Normal))
                .collect()
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

    /// Drops work owned by a disconnected Wayland client immediately instead
    /// of letting dead protocol resources occupy its queue share until render.
    pub(crate) fn discard_captures_for_client(&mut self, client_id: &ClientId) {
        self.pending_captures
            .retain(|capture| capture.client_id.as_ref() != Some(client_id));
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
    use smithay::output::{Mode, PhysicalProperties, Subpixel};

    fn test_output(size: (i32, i32), transform: Transform) -> Output {
        let output = Output::new(
            "capture-test".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".to_string(),
                model: "test".to_string(),
                serial_number: "test".to_string(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: size.into(),
                refresh: 73_000,
            }),
            Some(transform),
            None,
            None,
        );
        output
    }

    #[test]
    fn output_capture_size_follows_live_mode_and_rotation() {
        let normal = test_output((113, 71), Transform::Normal);
        assert_eq!(output_capture_size(&normal), Some((113, 71).into()));

        let rotated = test_output((113, 71), Transform::_90);
        assert_eq!(output_capture_size(&rotated), Some((71, 113).into()));

        let flipped_rotated = test_output((113, 71), Transform::Flipped270);
        assert_eq!(
            output_capture_size(&flipped_rotated),
            Some((71, 113).into())
        );
    }

    #[test]
    fn capture_queue_reserves_capacity_per_client_and_output() {
        assert_eq!(capture_queue_limit(0, Some(0), 0, 1), None);
        assert_eq!(
            capture_queue_limit(
                MAX_PENDING_CAPTURES_PER_CLIENT - 1,
                Some(MAX_PENDING_CAPTURES_PER_CLIENT - 1),
                MAX_PENDING_CAPTURES_PER_CLIENT - 1,
                1,
            ),
            None
        );
        assert_eq!(
            capture_queue_limit(
                MAX_PENDING_CAPTURES_PER_CLIENT,
                Some(MAX_PENDING_CAPTURES_PER_CLIENT),
                MAX_PENDING_CAPTURES_PER_CLIENT,
                1,
            ),
            Some(CaptureQueueLimit::Client)
        );
        assert_eq!(
            capture_queue_limit(MAX_PENDING_CAPTURES / 2, None, MAX_PENDING_CAPTURES / 2, 2,),
            Some(CaptureQueueLimit::Output)
        );
        assert_eq!(
            capture_queue_limit(MAX_PENDING_CAPTURES, None, MAX_PENDING_CAPTURES, 1),
            Some(CaptureQueueLimit::Global)
        );
    }

    #[test]
    fn logical_rect_to_buffer_scales_location_and_size_together() {
        let geo = Rectangle::new((10, 20).into(), (100, 50).into());
        let size = Size::from((137, 89));

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
