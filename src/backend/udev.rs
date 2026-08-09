//! Standalone TTY/DRM backend: no host compositor, drives real display
//! hardware directly via KMS/DRM, GBM and a libseat session.
//!
//! Structured after `malbiruk/driftwm`'s `backend/udev.rs` (single GPU,
//! direct `DrmCompositor`, no multi-GPU render-node juggling), not anvil's
//! `DrmOutputManager`/`MultiRenderer` machinery, which exists to composite
//! across several GPUs at once -- more than TideWM needs right now. All
//! Smithay API calls here were cross-checked against the actual pinned
//! `smithay v0.7.0` source, not guessed from either reference.
//!
//! Runtime output hotplug (a monitor plugged/unplugged into a port on the
//! GPU already in use) is handled -- see `handle_connector_change` -- but
//! windows on a disconnected output aren't migrated to another one, just
//! left in that output's now-orphaned tiling tree. A hot-added or removed
//! *GPU* (as opposed to a monitor) is out of scope entirely, matching the
//! single-GPU design above.
//!
//! Scope deliberately left out of this first pass (tracked as a follow-up,
//! not silently missing): retrying `DrmCompositor::new` with
//! `Modifier::Invalid` (implicit modifiers) if the first negotiation fails,
//! which driftwm does and some hardware needs -- `create_surface` below
//! just drops the surface on that failure instead. See AGENT.md.

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, time::Duration};

use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Format,
        },
        drm::{
            compositor::{DrmCompositor, FrameError, FrameFlags, PrimaryPlaneElement},
            exporter::gbm::GbmFramebufferExporter,
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType,
        },
        egl::{context::ContextPriority, EGLContext, EGLDevice, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            element::{
                memory::MemoryRenderBufferRenderElement,
                solid::SolidColorRenderElement,
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                Kind,
            },
            gles::GlesRenderer,
            ImportDma,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{self, UdevBackend, UdevEvent},
    },
    desktop::{space::SpaceRenderElements, utils::OutputPresentationFeedback},
    input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData},
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            ping::make_ping,
            timer::{TimeoutAction, Timer},
            EventLoop, LoopHandle,
        },
        drm::{
            control::{connector, crtc, Device as ControlDevice, Mode as DrmMode, ModeTypeFlags},
            Device as DrmRawDevice,
        },
        input::Libinput,
        rustix::fs::OFlags,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Transform},
    wayland::{compositor::with_states, dmabuf::DmabufFeedbackBuilder, presentation::Refresh},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::{
    config::OutputTransformConfig,
    cursor,
    state::{LockRenderElement, SessionLock, Smallvil},
};

const SUPPORTED_COLOR_FORMATS: &[smithay::backend::allocator::Fourcc] = &[
    smithay::backend::allocator::Fourcc::Argb8888,
    smithay::backend::allocator::Fourcc::Xrgb8888,
];

type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;

// Fixed to `GlesRenderer` (the `<=GlesRenderer>` form, no remaining
// generic parameter at all) rather than generic over a renderer type `R`
// the way this used to be declared: water-glass (Phase R1, see
// water_glass.rs) draws via a custom `GlesTexProgram`, which has no
// generic-renderer equivalent, so a `WaterGlass` variant here can only
// ever implement `RenderElement<GlesRenderer>`. This type was already only
// ever instantiated with `GlesRenderer` (and its `E` parameter always
// `WaylandSurfaceRenderElement<GlesRenderer>`) throughout this codebase --
// there is no second renderer backend -- so making both concrete costs
// nothing in practice and sidesteps a real limitation in how this macro
// threads a `where` bound through to the generated enum's own field types
// when a generic parameter is still involved.
smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElements<=GlesRenderer>;
    Space = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Cursor = WaylandSurfaceRenderElement<GlesRenderer>,
    /// Toast (see toast.rs) and the udev-only fallback cursor glyph (see
    /// cursor.rs) are both single CPU-composited memory buffers -- no need
    /// for two variants wrapping the same underlying element type.
    Composited = MemoryRenderBufferRenderElement<GlesRenderer>,
    /// `state::LockRenderElement<R>` (blank fill + lock-surface content)
    /// nested as its own variant rather than two separate ones -- avoids
    /// an ambiguous `From<WaylandSurfaceRenderElement<R>>` with `Cursor`
    /// above, and keeps the blank-vs-surface choice in one shared place.
    Lock = LockRenderElement<GlesRenderer>,
    /// Water-glass (Phase R1, see water_glass.rs), the reason this enum
    /// stopped being generic over the renderer -- see the comment above.
    WaterGlass = crate::water_glass::WaterGlassElement,
    /// Frosted-glass mode over the same captured backdrop as water glass.
    FrostGlass = crate::frost_glass::FrostGlassElement,
    /// Fixed-cost analytical shadow inserted directly behind each window.
    Shadow = crate::shadow::ShadowElement,
    /// Client surface tree clipped to compositor-owned rounded geometry.
    RoundedSurface = crate::decoration::RoundedSurfaceElement,
    /// Surface/subsurface or popup scaled around its window's top-left while
    /// a layout resize interpolates toward the new logical geometry.
    AnimatedSurface = smithay::backend::renderer::element::utils::RescaleRenderElement<
        WaylandSurfaceRenderElement<GlesRenderer>
    >,
    /// Rounded main surface with the same allocation-free resize transform.
    AnimatedRoundedSurface = smithay::backend::renderer::element::utils::RescaleRenderElement<
        crate::decoration::RoundedSurfaceElement
    >,
    /// Last imported client textures retained for the bounded close
    /// animation after the live Wayland surface has unmapped.
    WindowSnapshot = crate::window_animation::WindowSnapshotElement,
    /// Analytical solid/gradient border above its own window.
    Border = crate::decoration::BorderElement,
    /// Impulse ripple (Phase R1, see ripple.rs), drawn over windows but
    /// below toast/overview/picker/tab-strip chrome. Same renderer-
    /// concrete-ness reason as `WaterGlass` above.
    Ripple = crate::ripple::RippleElement,
    /// Cool-depth wash and urgent bioluminescent border (Phase R1).
    DepthOverlay = crate::depth::DepthOverlayElement,
    /// Allocation-free vertical pressure wave for direct Classic depth moves.
    DepthTransition = crate::depth_transition::DepthTransitionElement,
    /// World-anchored Ocean reference grid behind windows and above wallpaper.
    OceanCanvas = crate::ocean_canvas::OceanCanvasElement,
    /// Ambient caustic light over the wallpaper, below windows. Engine-
    /// agnostic; gated by `water_effects` plus its own enable.
    Caustics = crate::caustics::CausticsElement,
    /// Bioluminescent edge-glow cue for an off-screen urgent/deep Ocean
    /// window (spatial roadmap S5). Above windows, below chrome.
    Compass = crate::compass::CompassElement,
    /// Captured outgoing workspace peeled away over the live incoming
    /// workspace (Phase R1, see workspace_transition.rs).
    WorkspaceTransition = crate::workspace_transition::WorkspaceTransitionElement,
    /// Full-output dim fill for a `layer_rule { dim_around = true }` layer
    /// surface -- Hyprland's `dimaround`. Pushed directly behind the
    /// Overlay/Top layer pass so it darkens every window and lower layer
    /// without needing its own capture step.
    Dim = SolidColorRenderElement,
}

struct SurfaceData {
    compositor: GbmDrmCompositor,
    output: Output,
    /// The `wl_output` global's own id, kept so it can be retracted on
    /// disconnect -- `create_surface` used to discard this (`let _global =
    /// ...`), which left the global advertised to every client forever
    /// after an unplug, with a replug adding a second one on top rather
    /// than replacing it.
    global: GlobalId,
    /// A frame is queued and we're waiting for its VBlank; the KMS API
    /// doesn't allow submitting another one to the same CRTC until then.
    pending: bool,
    /// Content changed since this surface last actually rendered. Distinct
    /// from `Smallvil::needs_redraw` (which the Timer tick consumes as soon
    /// as it's observed) so a surface skipped this tick because it was
    /// still `pending` doesn't lose the update -- its VBlank handler will
    /// pick this back up.
    dirty: bool,
    /// `queue_frame` produces no VBlank when it returns `EmptyFrame`. Keep
    /// one estimated-VBlank timer armed in that case so animations get
    /// another render opportunity without turning the regular poll into a
    /// busy retry loop.
    empty_frame_retry_pending: Option<u64>,
    /// Set by `wlr-output-power-management-v1` (see `Smallvil::set_output_power`
    /// below), via `GbmDrmCompositor::clear()` -- DPMS off, every plane
    /// disabled, pending/queued/next frame cleared. The render loop skips
    /// this surface entirely while set; `clear()`'s own doc comment says
    /// calling `queue_frame` again (the ordinary render path) re-enables,
    /// so turning back on is just "stop skipping it, mark dirty."
    powered_off: bool,
}

struct DeviceData {
    drm: DrmDevice,
    /// Shared with `Smallvil::udev_renderer` so `DmabufHandler::dmabuf_imported`
    /// can use it too -- see handlers/mod.rs.
    renderer: Rc<RefCell<GlesRenderer>>,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    libinput: Libinput,
    /// Kept around (not just used once at startup) so a udev "device
    /// changed" event -- a monitor plugged or unplugged into a port on
    /// this GPU -- can rescan and create/tear down surfaces at runtime; see
    /// `handle_connector_change`.
    gbm: GbmDevice<DrmDeviceFd>,
    render_formats: Vec<Format>,
    scanner: DrmScanner,
    /// Source of unique timer generations. This lives on the device rather
    /// than `SurfaceData` so disconnecting and recreating a surface on the
    /// same CRTC cannot let an old timer match the new surface by accident.
    next_empty_frame_retry_generation: u64,
}

pub fn init_udev(
    event_loop: &mut EventLoop<Smallvil>,
    state: &mut Smallvil,
) -> Result<(), Box<dyn std::error::Error>> {
    let display_handle = state.display_handle.clone();

    // Unlike the winit backend, there's no host compositor to connect to
    // here (this backend *is* the compositor), so this can be exported
    // immediately: nothing downstream depends on reading the old value
    // first. Lets anything spawned later (terminal, xwayland-satellite)
    // find this socket -- previously only the winit backend exported this.
    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);

    let (mut session, session_notifier) = LibSeatSession::new()
        .map_err(|e| format!("Failed to create a session (are you running from a TTY?): {e}"))?;
    let seat_name = session.seat();
    tracing::info!(seat_name, "Session created");
    // So input.rs's VT-switch keybind detection has something to call
    // change_vt() on; None under winit, where a host compositor already
    // owns VT switching.
    state.session = Some(session.clone());
    state.cursor_theme = cursor::Theme::load();

    let udev_backend = UdevBackend::new(&seat_name)?;
    let primary_gpu_path = udev::primary_gpu(&seat_name).ok().flatten();

    // Primary GPU first, then whatever else udev enumerates -- on hybrid
    // graphics the "primary" GPU may not be the one with a connected
    // display, so we try each in order until one works.
    let gpu_paths: Vec<PathBuf> = {
        let mut paths = Vec::new();
        if let Some(p) = primary_gpu_path {
            paths.push(p);
        }
        for (_, path) in udev_backend.device_list() {
            let p = path.to_path_buf();
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        paths
    };
    if gpu_paths.is_empty() {
        return Err("No GPUs found".into());
    }
    tracing::info!(?gpu_paths, "GPU candidates");

    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;

    let (mut drm, drm_notifier, gbm, renderer, render_formats, render_node) = 'found: {
        for path in &gpu_paths {
            let node = match DrmNode::from_path(path) {
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!(path = %path.display(), %e, "Not a DRM node, skipping");
                    continue;
                }
            };
            if node.ty() != NodeType::Primary {
                continue;
            }

            let fd = match session.open(path, open_flags) {
                Ok(fd) => fd,
                Err(e) => {
                    tracing::warn!(path = %path.display(), %e, "Failed to open GPU");
                    continue;
                }
            };
            let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

            let (drm, drm_notifier) = match DrmDevice::new(device_fd.clone(), true) {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(path = %path.display(), %e, "Failed to create DRM device");
                    continue;
                }
            };

            if !gpu_has_connected_display(&drm) {
                tracing::info!(path = %path.display(), "No connected displays, trying next GPU");
                continue;
            }

            let gbm = match GbmDevice::new(device_fd.clone()) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!(path = %path.display(), %e, "Failed to create GBM device");
                    continue;
                }
            };

            // Safety: `gbm` is a valid GBM device for the lifetime of this
            // EGLDisplay, and we keep it alive on `DeviceData` alongside it.
            let egl_display = match unsafe { EGLDisplay::new(gbm.clone()) } {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(path = %path.display(), %e, "Failed to create EGL display");
                    continue;
                }
            };
            let egl_context =
                match EGLContext::new_with_priority(&egl_display, ContextPriority::High) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), %e, "Failed to create EGL context");
                        continue;
                    }
                };
            let render_formats: Vec<Format> = egl_context
                .dmabuf_render_formats()
                .iter()
                .copied()
                .collect();

            // On split KMS/render-node systems (the common case on AMD and
            // Intel: card0 + renderD128) `node` above is the *display* node
            // we opened for modesetting, not the one Mesa actually renders
            // through. Advertising the wrong one to clients via dmabuf
            // feedback makes them crash trying to use a node they can't
            // render with. Ask EGL for the node it's actually using; fall
            // back to `node` itself only if that fails.
            let render_node = EGLDevice::device_for_display(&egl_display)
                .ok()
                .and_then(|d| d.try_get_render_node().ok().flatten())
                .or_else(|| node.node_with_type(NodeType::Render).and_then(|n| n.ok()))
                .unwrap_or_else(|| {
                    tracing::warn!(
                        path = %path.display(),
                        "Could not resolve a DRM render node, falling back to the KMS node; \
                         GPU clients may misbehave"
                    );
                    node
                });

            // Safety: `egl_context` was just created above and isn't used
            // anywhere else.
            let renderer = match unsafe { GlesRenderer::new(egl_context) } {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(path = %path.display(), %e, "Failed to create GLES renderer");
                    continue;
                }
            };

            tracing::info!(path = %path.display(), "Using GPU");
            break 'found (
                drm,
                drm_notifier,
                gbm,
                renderer,
                render_formats,
                render_node,
            );
        }
        return Err("No GPU with a connected display found (are you running from a TTY?)".into());
    };

    // Client-facing zwp_linux_dmabuf_v1 global: lets GPU-accelerated
    // clients hand us dmabuf-backed buffers instead of falling back to
    // slower shm. Formats here are import formats (what we can accept from
    // clients), distinct from `render_formats` above (what we can use for
    // our own scanout swapchain).
    {
        let dmabuf_formats = renderer.dmabuf_formats();
        match DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats).build() {
            Ok(default_feedback) => {
                let global = state
                    .dmabuf_state
                    .create_global_with_default_feedback::<Smallvil>(
                        &display_handle,
                        &default_feedback,
                    );
                state.dmabuf_global = Some(global);
            }
            Err(e) => {
                tracing::warn!(%e, "Failed to build dmabuf feedback; GPU clients will fall back to shm");
            }
        }
    }

    // Do not advertise linux-drm-syncobj-v1 yet. Smithay's protocol state
    // validates acquire/release points but does not wait for acquire points
    // automatically; advertising it without a DrmSyncPointBlocker would let
    // client buffers reach rendering or scanout before they are ready. Niri
    // and DriftWM likewise leave this protocol disabled. Re-enable it only
    // together with the pre-commit acquire blocker and hardware tests.

    // Libinput, fed by the same seat as the DRM session.
    let libinput_session = LibinputSessionInterface::from(session.clone());
    let mut libinput = Libinput::new_with_udev(libinput_session);
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|_| "Failed to assign libinput seat")?;
    let libinput_backend = LibinputInputBackend::new(libinput.clone());
    event_loop
        .handle()
        .insert_source(libinput_backend, |mut event, _, state: &mut Smallvil| {
            if let InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. } = &event {
                tracing::debug!(?event, "Input device topology changed");
            }
            if let InputEvent::DeviceAdded { device } = &mut event {
                crate::input::apply_touchpad_config(&state.config.input.touchpad, device);
                if device.config_tap_finger_count() > 0 {
                    state.known_touchpads.push(device.clone());
                }
            }
            if let InputEvent::DeviceRemoved { device } = &event {
                state.known_touchpads.retain(|d| d != device);
            }
            state.process_input_event(event);
        })?;

    // Scan connectors and create a DRM surface (+ Output) per connected one.
    let mut drm_scanner: DrmScanner = DrmScanner::new();
    let scan_result = drm_scanner.scan_connectors(&drm)?;
    let mut surfaces: HashMap<crtc::Handle, SurfaceData> = HashMap::new();

    for event in scan_result.iter() {
        if let DrmScanEvent::Connected {
            connector,
            crtc: Some(crtc),
        } = event
        {
            match create_surface(
                &mut drm,
                &gbm,
                &render_formats,
                &connector,
                crtc,
                &display_handle,
                state,
            ) {
                Some(surface) => {
                    surfaces.insert(crtc, surface);
                }
                None => tracing::warn!(?crtc, "Failed to create DRM surface for connector"),
            }
        }
    }

    if surfaces.is_empty() {
        return Err("Display connected but failed to create any DRM surface".into());
    }

    let renderer = Rc::new(RefCell::new(renderer));
    state.udev_renderer = Some(Rc::clone(&renderer));

    let device = Rc::new(RefCell::new(DeviceData {
        drm,
        renderer,
        surfaces,
        libinput,
        gbm,
        render_formats,
        scanner: drm_scanner,
        next_empty_frame_retry_generation: 0,
    }));

    // Damage propagation is event-driven on DRM. `request_redraw()` writes
    // this coalescing eventfd; the callback marks every CRTC dirty and renders
    // any one that is not already waiting for VBlank. A render which keeps an
    // animation alive pings again, but the pending flag prevents a second KMS
    // submission until that output's own VBlank, independently pacing mixed
    // refresh-rate outputs from their advertised modes.
    let (redraw_wakeup, redraw_source) = make_ping()?;
    let device_for_redraw = Rc::clone(&device);
    let loop_handle_for_redraw = event_loop.handle();
    event_loop
        .handle()
        .insert_source(redraw_source, move |(), _, state: &mut Smallvil| {
            state.update_window_depths();
            state.update_urgent_pulses();
            state.update_float_physics_full();
            let retries = render_requested_surfaces(state, &device_for_redraw);
            for (crtc, delay) in retries {
                schedule_empty_frame_retry(
                    &loop_handle_for_redraw,
                    Rc::clone(&device_for_redraw),
                    crtc,
                    delay,
                );
            }
        })?;
    state.install_redraw_wakeup(redraw_wakeup);

    // wlr-output-power-management-v1's real backend hook (see
    // `Smallvil::set_output_power`, `handlers/wlr_output_power_management.rs`).
    // Off: `GbmDrmCompositor::clear()` -- DPMS off, planes disabled, no
    // further page-flips attempted on this CRTC until turned back on. On:
    // just stop skipping it and mark dirty; the ordinary render path's own
    // `queue_frame` is what actually re-enables per `clear()`'s own doc
    // comment. `find` rather than a direct crtc lookup: the hook is only
    // ever given an `Output`, not a `crtc::Handle`.
    let device_for_power = Rc::clone(&device);
    state.set_output_power = Some(Box::new(move |target: &Output, on: bool| {
        let mut dev = device_for_power.borrow_mut();
        let Some(surface) = dev.surfaces.values_mut().find(|s| &s.output == target) else {
            // Not one of ours -- a stale Output from an already-unplugged
            // monitor, or a request racing a disconnect. Not a hard error.
            return false;
        };
        if on {
            surface.powered_off = false;
            surface.dirty = true;
            true
        } else {
            match surface.compositor.clear() {
                Ok(()) => {
                    surface.powered_off = true;
                    surface.pending = false;
                    true
                }
                Err(e) => {
                    tracing::warn!(%e, "Failed to power off output via DRM clear()");
                    false
                }
            }
        }
    }));

    // zwlr_gamma_control_manager_v1's real backend hooks (see
    // `Smallvil::gamma_size`/`set_gamma`, `handlers/wlr_gamma_control.rs`).
    // Both go through the legacy `DRM_IOCTL_MODE_{GET,SET}GAMMA` ioctls
    // (`drm::control::Device::{get_crtc,set_gamma}`) rather than the atomic
    // GAMMA_LUT blob property -- the kernel already shims the legacy path
    // onto atomic-only drivers, so this works uniformly without touching
    // atomic properties directly.
    let device_for_gamma_size = Rc::clone(&device);
    state.gamma_size = Some(Box::new(move |target: &Output| {
        let dev = device_for_gamma_size.borrow();
        let crtc = dev
            .surfaces
            .iter()
            .find(|(_, s)| &s.output == target)
            .map(|(&c, _)| c)?;
        ControlDevice::get_crtc(&dev.drm, crtc)
            .ok()
            .map(|info| info.gamma_length())
    }));

    let device_for_set_gamma = Rc::clone(&device);
    state.set_gamma = Some(Box::new(
        move |target: &Output, red: &[u16], green: &[u16], blue: &[u16]| {
            let dev = device_for_set_gamma.borrow();
            let Some(crtc) = dev
                .surfaces
                .iter()
                .find(|(_, s)| &s.output == target)
                .map(|(&c, _)| c)
            else {
                return false;
            };
            match ControlDevice::set_gamma(&dev.drm, crtc, red, green, blue) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(%e, "Failed to set gamma ramp via DRM");
                    false
                }
            }
        },
    ));

    // Do the initial render after `device` exists so an unexpectedly empty
    // first frame can use the same estimated-VBlank path as every later
    // render.
    let initial_retries = {
        let mut dev = device.borrow_mut();
        let DeviceData {
            surfaces, renderer, ..
        } = &mut *dev;
        let mut renderer = renderer.borrow_mut();
        surfaces
            .iter_mut()
            .filter_map(|(&crtc, surface)| {
                render_surface(state, surface, &mut renderer).map(|delay| (crtc, delay))
            })
            .collect::<Vec<_>>()
    };
    for (crtc, delay) in initial_retries {
        schedule_empty_frame_retry(&event_loop.handle(), Rc::clone(&device), crtc, delay);
    }

    // VBlank / DRM error events.
    let device_for_drm = Rc::clone(&device);
    let loop_handle_for_drm = event_loop.handle();
    event_loop.handle().insert_source(
        drm_notifier,
        move |event, _meta, state: &mut Smallvil| {
            let retry = match event {
                DrmEvent::VBlank(crtc) => {
                    let mut dev = device_for_drm.borrow_mut();
                    let DeviceData {
                        surfaces, renderer, ..
                    } = &mut *dev;
                    let Some(surface) = surfaces.get_mut(&crtc) else {
                        return;
                    };
                    match surface.compositor.frame_submitted() {
                        // The submitted frame carried the wp_presentation
                        // feedback collected at render time as its user
                        // data; this VBlank is the moment that content
                        // actually hit the display. This smithay revision's
                        // DrmEvent carries no hardware timestamp/sequence,
                        // so the clock read and seq 0 are as precise as it
                        // gets -- HwClock/HwCompletion flags are
                        // deliberately not claimed.
                        Ok(user_data) => {
                            if let Some(mut feedback) = user_data.flatten() {
                                feedback.presented(
                                    state.clock.now(),
                                    surface
                                        .output
                                        .current_mode()
                                        .map(|mode| {
                                            Refresh::fixed(Duration::from_secs_f64(
                                                1_000f64 / mode.refresh as f64,
                                            ))
                                        })
                                        .unwrap_or(Refresh::Unknown),
                                    0,
                                    wp_presentation_feedback::Kind::Vsync,
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(%e, "frame_submitted error");
                        }
                    }
                    surface.pending = false;
                    // Defensive: `clear()` cancels any in-flight frame, so
                    // a VBlank shouldn't fire for a powered-off surface in
                    // practice, but a power-off racing an already-queued
                    // frame is exactly the kind of timing this guard is
                    // cheap insurance against.
                    if surface.dirty && !surface.powered_off {
                        state.update_window_depths();
                        state.update_urgent_pulses();
                        state.update_float_physics_full();
                        render_surface(state, surface, &mut renderer.borrow_mut())
                            .map(|delay| (crtc, delay))
                    } else {
                        None
                    }
                }
                DrmEvent::Error(e) => {
                    tracing::error!(%e, "DRM error");
                    None
                }
            };

            if let Some((crtc, delay)) = retry {
                schedule_empty_frame_retry(
                    &loop_handle_for_drm,
                    Rc::clone(&device_for_drm),
                    crtc,
                    delay,
                );
            }
        },
    )?;

    // VT switching: give up DRM master and pause libinput while switched
    // away, reclaim and force a full re-render on the way back.
    let device_for_session = Rc::clone(&device);
    let loop_handle_for_session = event_loop.handle();
    event_loop.handle().insert_source(
        session_notifier,
        move |event, _, state: &mut Smallvil| {
            let retries = match event {
                SessionEvent::PauseSession => {
                    let mut dev = device_for_session.borrow_mut();
                    tracing::info!("Session paused (VT switch away)");
                    dev.libinput.suspend();
                    dev.drm.pause();
                    Vec::new()
                }
                SessionEvent::ActivateSession => {
                    let mut dev = device_for_session.borrow_mut();
                    tracing::info!("Session resumed (VT switch back)");
                    if dev.libinput.resume().is_err() {
                        tracing::warn!("Failed to resume libinput");
                    }
                    if let Err(e) = dev.drm.activate(false) {
                        tracing::error!(%e, "Failed to reactivate DRM device");
                        return;
                    }
                    let DeviceData {
                        surfaces, renderer, ..
                    } = &mut *dev;
                    let mut retries = Vec::new();
                    for (&crtc, surface) in surfaces.iter_mut() {
                        if let Err(e) = surface.compositor.reset_state() {
                            tracing::warn!(%e, "Failed to reset DRM surface state");
                        }
                        surface.pending = false;
                        surface.dirty = true;
                        // A VT-switch-away/back always wakes every output,
                        // deliberately not trying to preserve a
                        // wlr-output-power-management-v1 blank across it:
                        // `reset_state()` above already re-primes the DRM
                        // surface from scratch, so continuing to treat it
                        // as off here would just be internally
                        // inconsistent with what `reset_state()` already
                        // did.
                        surface.powered_off = false;
                        state
                            .wlr_output_power_management_state
                            .force_on(&surface.output);
                        if let Some(delay) =
                            render_surface(state, surface, &mut renderer.borrow_mut())
                        {
                            retries.push((crtc, delay));
                        }
                    }
                    retries
                }
            };

            for (crtc, delay) in retries {
                schedule_empty_frame_retry(
                    &loop_handle_for_session,
                    Rc::clone(&device_for_session),
                    crtc,
                    delay,
                );
            }
        },
    )?;

    // Hotplug: a monitor plugged/unplugged into a port on the GPU we're
    // already driving fires `Changed`, which is the only case handled --
    // see `handle_connector_change`. `Added`/`Removed` mean a whole GPU
    // appeared or disappeared, out of scope for the single-GPU design this
    // backend deliberately uses (see module docs); logged so it's at least
    // visible rather than silently doing nothing.
    let device_for_udev = Rc::clone(&device);
    let display_handle_for_udev = display_handle.clone();
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, state: &mut Smallvil| match event {
            UdevEvent::Added { device_id, path } => {
                tracing::info!(
                    ?device_id, ?path,
                    "udev device added; hot-added GPUs aren't supported (single-GPU design), ignoring"
                );
            }
            UdevEvent::Changed { device_id } => {
                let mut dev = device_for_udev.borrow_mut();
                if device_id != dev.drm.device_id() {
                    tracing::debug!(?device_id, "udev change on a different device, ignoring");
                    return;
                }
                tracing::info!(?device_id, "udev device changed, rescanning connectors");
                handle_connector_change(&mut dev, &display_handle_for_udev, state);
            }
            UdevEvent::Removed { device_id } => {
                let dev = device_for_udev.borrow();
                if device_id == dev.drm.device_id() {
                    tracing::error!(
                        ?device_id,
                        "The GPU TideWM is driving was removed; can't continue rendering \
                         (single-GPU design, no fallback GPU to switch to)"
                    );
                } else {
                    tracing::debug!(?device_id, "udev device removed (not the one we're driving)");
                }
            }
        })?;

    // Slow maintenance fallback for clocks which can become actionable with
    // no external event (urgent repeats, automatic depth, cleanup). Rendering
    // itself is driven by the redraw eventfd above and each CRTC's VBlank.
    // Continuous animation derives its wake period from the fastest live mode;
    // scheduled caustics uses its configured deadline directly.
    let device_for_timer = Rc::clone(&device);
    event_loop
        .handle()
        .insert_source(Timer::immediate(), move |_, _, state: &mut Smallvil| {
            state.update_window_depths();
            state.update_urgent_pulses();
            state.update_float_physics_full();
            let caustics_delay = state.caustics_redraw_delay();
            if caustics_delay.is_some_and(|delay| delay.is_zero()) {
                state.request_redraw();
            }
            let active = state.has_active_animation();

            state.space.refresh();
            state.popups.cleanup();
            state.refresh_popup_grab();
            state.cleanup_capture();
            state.cleanup_wlr_foreign_toplevels();
            let _ = state.display_handle.flush_clients();

            let next = if active {
                device_for_timer
                    .borrow()
                    .surfaces
                    .values()
                    .filter(|surface| !surface.powered_off)
                    .map(|surface| output_refresh_period(&surface.output))
                    .min()
                    .unwrap_or_else(|| Duration::from_secs(1))
            } else {
                caustics_delay.unwrap_or_else(|| Duration::from_secs(1))
            };
            TimeoutAction::ToDuration(next)
        })?;

    Ok(())
}

/// Consume the compositor-wide damage bit and render every ready CRTC once.
/// A CRTC already waiting for VBlank keeps its own `dirty` bit and is picked
/// up by the DRM event handler, so a slow output never blocks a faster one.
fn render_requested_surfaces(
    state: &mut Smallvil,
    device: &Rc<RefCell<DeviceData>>,
) -> Vec<(crtc::Handle, Duration)> {
    let mut dev = device.borrow_mut();
    if state.take_needs_redraw() {
        for surface in dev.surfaces.values_mut() {
            surface.dirty = true;
        }
    }

    // While VT-switched away, preserve dirty state for ActivateSession's
    // reset/re-render path instead of logging DeviceInactive in a loop.
    if !dev.drm.is_active() {
        return Vec::new();
    }

    let DeviceData {
        surfaces, renderer, ..
    } = &mut *dev;
    let mut renderer = renderer.borrow_mut();
    let mut retries = Vec::new();
    for (&crtc, surface) in surfaces.iter_mut() {
        if surface.powered_off {
            state.fail_captures_for_output(&surface.output);
            continue;
        }
        if surface_redraw_ready(
            surface.dirty,
            surface.pending,
            surface.empty_frame_retry_pending.is_some(),
        ) {
            if let Some(delay) = render_surface(state, surface, &mut renderer) {
                retries.push((crtc, delay));
            }
        }
        // Capture requests use the same redraw wakeup and need the active EGL
        // renderer even if scanout damage collapsed to an empty frame.
        state.render_pending_captures(&mut renderer, &surface.output, true);
        state.capture_pending_workspace_transition(&mut renderer, &surface.output);
    }
    retries
}

fn gpu_has_connected_display(drm: &DrmDevice) -> bool {
    let Ok(res) = ControlDevice::resource_handles(drm) else {
        return false;
    };
    res.connectors().iter().any(|&handle| {
        ControlDevice::get_connector(drm, handle, true)
            .is_ok_and(|c| c.state() == connector::State::Connected)
    })
}

fn connector_type_name(connector: &connector::Info) -> String {
    format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    )
}

fn pick_preferred_mode(modes: &[DrmMode]) -> Option<DrmMode> {
    modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| {
            modes
                .iter()
                .max_by_key(|m| (m.size().0 as u32 * m.size().1 as u32, m.vrefresh()))
        })
        .copied()
}

/// Config-driven mode selection: `requested` (an `OutputConfig::mode` string
/// like `"1920x1080@60"`) is matched against every mode the connector
/// actually reports. Refresh is matched to the nearest whole Hz since KMS
/// modes carry an exact millihertz value that rarely matches a
/// hand-typed integer exactly. Falls back to the connector's preferred
/// mode if `requested` is `None` or matches nothing.
fn pick_mode(modes: &[DrmMode], requested: Option<&str>) -> Option<DrmMode> {
    if let Some(requested) = requested {
        if let Some((w, h, refresh)) = crate::config::parse_mode_str(requested) {
            let found = modes.iter().find(|m| {
                let (mw, mh) = m.size();
                mw as i32 == w
                    && mh as i32 == h
                    && refresh.is_none_or(|r| (m.vrefresh() as f64 - r).abs() < 0.5)
            });
            if let Some(m) = found {
                return Some(*m);
            }
            tracing::warn!(
                requested,
                "Configured output mode not found; using preferred mode"
            );
        } else {
            tracing::warn!(
                requested,
                "Failed to parse configured output mode; using preferred mode"
            );
        }
    }
    pick_preferred_mode(modes)
}

fn create_surface(
    drm: &mut DrmDevice,
    gbm: &GbmDevice<DrmDeviceFd>,
    render_formats: &[Format],
    connector: &connector::Info,
    crtc: crtc::Handle,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    state: &mut Smallvil,
) -> Option<SurfaceData> {
    let connector_name = connector_type_name(connector);
    let output_config = state
        .config
        .outputs
        .iter()
        .find(|o| o.name == connector_name)
        .cloned();

    if let Some(cfg) = &output_config {
        if !cfg.enabled {
            tracing::info!(connector_name, "Output disabled in config; skipping");
            return None;
        }
    }

    let mode = pick_mode(
        connector.modes(),
        output_config.as_ref().and_then(|c| c.mode.as_deref()),
    )?;
    tracing::info!(
        connector_name,
        size = ?mode.size(),
        refresh = mode.vrefresh(),
        "Setting up output"
    );

    let drm_surface = match drm.create_surface(crtc, mode, &[connector.handle()]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, connector_name, "Failed to create DRM surface");
            return None;
        }
    };

    // Config surface plus a real-capability query only, for now -- no
    // `drm_surface.use_vrr()` call yet. See `AdaptiveSync`'s own doc for
    // why the actual toggle is deliberately deferred to a session where
    // this can be verified on real hardware, not just compiled.
    match drm_surface.vrr_supported(connector.handle()) {
        Ok(support) => tracing::info!(
            connector_name,
            configured = ?state.adaptive_sync_for(&connector_name),
            hardware_support = ?support,
            "Adaptive-sync (VRR): capability queried, not yet toggled"
        ),
        Err(e) => tracing::debug!(%e, connector_name, "Could not query VRR support"),
    }

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        connector_name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: Subpixel::Unknown,
            make: "Unknown".into(),
            model: connector_name.clone(),
            serial_number: "Unknown".into(),
        },
    );
    let output_mode = Mode {
        size: (mode.size().0 as i32, mode.size().1 as i32).into(),
        refresh: mode.vrefresh() as i32 * 1000,
    };
    // Rightmost edge of every currently-mapped output, not the sum of their
    // widths: summing assumes outputs are only ever appended in order, which
    // hotplug breaks. Two 1920-wide outputs A (x=0) and B (x=1920);
    // disconnect A, then connect C -- summing only B's width gives 1920,
    // landing C directly on top of the still-mapped B. Taking the max right
    // edge instead always lands a new output past every existing one,
    // regardless of gaps left by earlier disconnects. Only used as the
    // fallback when config doesn't pin an explicit position.
    let auto_x = state
        .space
        .outputs()
        .filter_map(|output| state.space.output_geometry(output))
        .fold(0, |max_edge, geo| max_edge.max(geo.loc.x + geo.size.w));
    let position = output_config
        .as_ref()
        .and_then(|c| c.position)
        .unwrap_or((auto_x, 0));
    let scale = Scale::Fractional(output_config.as_ref().map(|c| c.scale).unwrap_or(1.0));
    let transform = match output_config
        .as_ref()
        .map(|c| c.transform)
        .unwrap_or_default()
    {
        OutputTransformConfig::Normal => Transform::Normal,
        OutputTransformConfig::Rotate90 => Transform::_90,
        OutputTransformConfig::Rotate180 => Transform::_180,
        OutputTransformConfig::Rotate270 => Transform::_270,
        OutputTransformConfig::Flipped => Transform::Flipped,
        OutputTransformConfig::Flipped90 => Transform::Flipped90,
        OutputTransformConfig::Flipped180 => Transform::Flipped180,
        OutputTransformConfig::Flipped270 => Transform::Flipped270,
    };
    output.change_current_state(
        Some(output_mode),
        Some(transform),
        Some(scale),
        Some(position.into()),
    );
    output.set_preferred(output_mode);

    let mut planes = match drm.planes(&crtc) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(%e, connector_name, "Failed to query CRTC planes");
            return None;
        }
    };
    // Using an overlay plane on an Nvidia card breaks scanout.
    if let Ok(driver) = drm.get_driver() {
        let is_nvidia = driver
            .name()
            .to_string_lossy()
            .to_lowercase()
            .contains("nvidia")
            || driver
                .description()
                .to_string_lossy()
                .to_lowercase()
                .contains("nvidia");
        if is_nvidia {
            planes.overlay = vec![];
        }
    }

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let compositor = match DrmCompositor::new(
        &output,
        drm_surface,
        Some(planes),
        allocator,
        GbmFramebufferExporter::new(gbm.clone(), None.into()),
        SUPPORTED_COLOR_FORMATS.iter().copied(),
        render_formats.iter().copied(),
        drm.cursor_size(),
        Some(gbm.clone()),
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(%e, connector_name, "Failed to create DRM compositor");
            return None;
        }
    };

    // Publish the output only after every fallible DRM setup step succeeds.
    // A failed plane query or compositor construction must not leave a
    // mapped `wl_output` global that has no scanout surface behind it.
    let global = output.create_global::<Smallvil>(display_handle);
    state.space.map_output(&output, position);
    state.adopt_orphaned_output_windows(&output.name());
    #[cfg(feature = "screencast")]
    if let Some(screencast) = &state.screencast {
        screencast.refresh_outputs(state.space.outputs());
    }
    state.wlr_output_management_state.refresh(&state.space);

    Some(SurfaceData {
        compositor,
        output,
        global,
        pending: false,
        dirty: true,
        empty_frame_retry_pending: None,
        powered_off: false,
    })
}

/// Rescans connectors on `dev`'s GPU and reconciles `dev.surfaces` with
/// whatever changed: a newly connected display gets a fresh `create_surface`
/// call (same path startup uses), a disconnected one gets torn down and
/// unmapped from `state.space`. Called on the udev `Changed` event for this
/// device -- see `init_udev`'s hotplug registration.
fn handle_connector_change(
    dev: &mut DeviceData,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    state: &mut Smallvil,
) {
    let scan_result = match dev.scanner.scan_connectors(&dev.drm) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%e, "Failed to rescan DRM connectors");
            return;
        }
    };

    for event in scan_result.iter() {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => {
                if dev.surfaces.contains_key(&crtc) {
                    continue;
                }
                match create_surface(
                    &mut dev.drm,
                    &dev.gbm,
                    &dev.render_formats,
                    &connector,
                    crtc,
                    display_handle,
                    state,
                ) {
                    Some(surface) => {
                        tracing::info!(?crtc, "New output connected");
                        state.retile();
                        dev.surfaces.insert(crtc, surface);
                    }
                    None => tracing::warn!(
                        ?crtc,
                        "Failed to create DRM surface for newly connected output"
                    ),
                }
            }
            DrmScanEvent::Disconnected {
                crtc: Some(crtc), ..
            } => {
                if let Some(surface) = dev.surfaces.remove(&crtc) {
                    tracing::info!(?crtc, "Output disconnected");
                    // Retract the wl_output global explicitly -- letting
                    // `surface` (and its `GlobalId`) just drop does not do
                    // this; a client that already bound it would otherwise
                    // keep seeing a global for an output that no longer
                    // exists, and a later replug would advertise a second
                    // global on top of the still-live stale one.
                    display_handle.remove_global::<Smallvil>(surface.global);
                    // Every window owned by this output migrates to a
                    // still-connected fallback (same workspace numbers) so
                    // neither tiled nor floating content becomes permanently
                    // unreachable. No-op if this was the only output.
                    let disconnected_name = surface.output.name();
                    state.remove_workspace_transition_output(&disconnected_name);
                    let fallback: Option<String> = state
                        .space
                        .outputs()
                        .find(|o| o.name() != disconnected_name)
                        .map(|o| o.name());
                    state
                        .ocean
                        .remove_output(&disconnected_name, fallback.as_deref());
                    if let Some(fallback) = fallback.as_deref() {
                        state.migrate_output_windows(&disconnected_name, fallback);
                    }
                    state.space.unmap_output(&surface.output);
                    state.lock_surfaces.remove(&surface.output);
                    state.lock_blank.remove(&surface.output);
                    state.layer_dim_buffers.remove(&surface.output);
                    #[cfg(feature = "screencast")]
                    if let Some(screencast) = &state.screencast {
                        screencast.refresh_outputs(state.space.outputs());
                    }
                    state.wlr_output_management_state.refresh(&state.space);
                    state
                        .wlr_output_power_management_state
                        .output_removed(&surface.output);
                    state.retile();
                    state.repair_keyboard_focus(
                        fallback.as_deref(),
                        smithay::utils::SERIAL_COUNTER.next_serial(),
                    );
                }
            }
            _ => {}
        }
    }
}

/// Estimate one retrace from the output's live millihertz refresh value. If a
/// mode is temporarily absent, retry slowly instead of assuming hardware.
fn output_refresh_period(output: &Output) -> Duration {
    output
        .current_mode()
        .and_then(|mode| {
            u64::try_from(mode.refresh)
                .ok()
                .filter(|refresh| *refresh > 0)
        })
        .map(|refresh_millihz| Duration::from_nanos(1_000_000_000_000 / refresh_millihz))
        .unwrap_or_else(|| Duration::from_secs(1))
}

fn surface_redraw_ready(dirty: bool, pending: bool, retry_pending: bool) -> bool {
    dirty && !pending && !retry_pending
}

/// `FrameError::EmptyFrame` does not produce a page flip, and therefore no
/// VBlank event to drive the next render. Arm at most one estimated-VBlank
/// timer per CRTC. The timer renders once directly; it re-arms only when that
/// render still has an active animation (`surface.dirty` was set again), so a
/// damage-free client commit cannot create a permanent wake-up loop.
fn schedule_empty_frame_retry<'l>(
    loop_handle: &LoopHandle<'l, Smallvil>,
    device: Rc<RefCell<DeviceData>>,
    crtc: crtc::Handle,
    delay: Duration,
) {
    let generation = {
        let mut dev = device.borrow_mut();
        let Some(surface) = dev.surfaces.get(&crtc) else {
            return;
        };
        if surface.empty_frame_retry_pending.is_some() {
            return;
        }

        dev.next_empty_frame_retry_generation =
            dev.next_empty_frame_retry_generation.wrapping_add(1);
        let generation = dev.next_empty_frame_retry_generation;
        dev.surfaces
            .get_mut(&crtc)
            .unwrap()
            .empty_frame_retry_pending = Some(generation);
        generation
    };

    let weak_device = Rc::downgrade(&device);
    let retry_loop_handle = loop_handle.clone();
    let insert_result = loop_handle.insert_source(
        Timer::from_duration(delay),
        move |_, _, state: &mut Smallvil| {
            let Some(device) = weak_device.upgrade() else {
                return TimeoutAction::Drop;
            };

            let retry = {
                let mut dev = device.borrow_mut();
                if !dev.drm.is_active() {
                    if let Some(surface) = dev.surfaces.get_mut(&crtc) {
                        if surface.empty_frame_retry_pending == Some(generation) {
                            surface.empty_frame_retry_pending = None;
                            surface.dirty = true;
                        }
                    }
                    return TimeoutAction::Drop;
                }

                let DeviceData {
                    surfaces, renderer, ..
                } = &mut *dev;
                let Some(surface) = surfaces.get_mut(&crtc) else {
                    return TimeoutAction::Drop;
                };
                // A successful queue may have cancelled this timer and a
                // later EmptyFrame may already have armed its replacement.
                // Only the exact timer generation that is still registered
                // may consume the pending retry.
                if surface.empty_frame_retry_pending != Some(generation) {
                    return TimeoutAction::Drop;
                }

                surface.empty_frame_retry_pending = None;
                // Powered off via wlr-output-power-management-v1 since this
                // retry was armed (`compositor.clear()`, see
                // `Smallvil::set_output_power`, does not itself cancel an
                // already-scheduled retry timer) -- don't render or queue a
                // frame on a CRTC that's supposed to be dark. `dirty` is
                // left alone: powering back on is what sets it, not this.
                if surface.powered_off {
                    return TimeoutAction::Drop;
                }
                let retry = render_surface(state, surface, &mut renderer.borrow_mut());
                retry.filter(|_| surface.dirty)
            };

            if let Some(delay) = retry {
                schedule_empty_frame_retry(&retry_loop_handle, Rc::clone(&device), crtc, delay);
            }

            TimeoutAction::Drop
        },
    );

    if let Err(e) = insert_result {
        // Preserve the redraw if calloop rejects the timer. The regular
        // bounded poll will retry instead of silently losing the frame.
        if let Some(surface) = device.borrow_mut().surfaces.get_mut(&crtc) {
            if surface.empty_frame_retry_pending == Some(generation) {
                surface.empty_frame_retry_pending = None;
                surface.dirty = true;
            }
        }
        tracing::warn!(%e, ?crtc, "Failed to schedule empty-frame retry");
    }
}

fn render_surface(
    state: &mut Smallvil,
    surface: &mut SurfaceData,
    renderer: &mut GlesRenderer,
) -> Option<Duration> {
    surface.dirty = false;

    let locked = !matches!(state.session_lock, SessionLock::Unlocked);

    let output = &surface.output;
    // The renderer has a current EGL context here, but the DRM target has
    // not been bound by `render_frame` yet. Capture now so glass uses the
    // current window position in this visible frame; capturing afterward
    // made interactive moves visibly trail and flicker. Same-sized captures
    // reuse their existing window texture.
    if !locked {
        state.capture_floating_backdrops(renderer, output);
        state.capture_layer_backdrops(renderer, output);
    }
    let size = output.current_mode().map(|m| m.size).unwrap_or_default();
    let scale = output.current_scale().fractional_scale();
    let output_loc = state
        .space
        .output_geometry(output)
        .map(|geo| geo.loc)
        .unwrap_or_default();

    let toast_element = (!locked)
        .then(|| state.toast_element(output, renderer))
        .flatten();
    if state.toast.as_ref().is_some_and(|toast| toast.expired()) {
        state.toast = None;
        #[cfg(feature = "accessibility")]
        state.sync_accessibility_tree();
    }

    // `cursor_hide_after_ms` (`Smallvil::note_pointer_motion` arms the
    // wake-up timer that gets a render to actually run at this moment on an
    // otherwise-static desktop) takes priority over everything else below --
    // independent of `cursor_always_visible`, which only concerns a
    // *client's* own hide request, not this compositor-driven timer.
    let idle_hidden = state.config.cursor_hide_after_ms > 0
        && state.last_pointer_motion.elapsed()
            >= Duration::from_millis(state.config.cursor_hide_after_ms as u64);
    // A locked pointer (`wp-pointer-constraints-v1`, e.g. Minecraft's mouse
    // look) never receives absolute motion, so its last on-screen position
    // is permanently stale -- always hide the system cursor while locked
    // rather than render a frozen arrow, regardless of `cursor_always_visible`
    // (which only concerns a client's own hide request, not this).
    let pointer_locked = state.pointer_is_locked();
    // Otherwise, `cursor_always_visible` overrides a client's own hide
    // request (`CursorImageStatus::Hidden`, e.g. a terminal hiding its
    // pointer glyph after inactivity) -- falls back to the plain default
    // arrow, the same as an unrecognized named icon would.
    let forced_visible_status = CursorImageStatus::Named(CursorIcon::Default);
    let hidden_status = CursorImageStatus::Hidden;
    // While locked, never reuse a client-provided cursor surface from the
    // desktop underneath. A compositor-owned default glyph is safe to draw
    // over the lock and keeps the pointer usable before the lock client sets
    // its own state.
    let effective_cursor_status = if locked {
        &forced_visible_status
    } else if idle_hidden || pointer_locked {
        &hidden_status
    } else if matches!(state.cursor_status, CursorImageStatus::Hidden)
        && state.config.cursor_always_visible
    {
        &forced_visible_status
    } else {
        &state.cursor_status
    };

    let (cursor_surface_element, cursor_glyph_element) = match effective_cursor_status {
        CursorImageStatus::Surface(cursor_surface) => {
            let hotspot = with_states(cursor_surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|data| data.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            let pointer_loc = state
                .seat
                .get_pointer()
                .map(|p| p.current_location())
                .unwrap_or_default();
            let local = (pointer_loc - output_loc.to_f64()).to_physical(scale)
                - hotspot.to_f64().to_physical(scale);
            let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer,
                    cursor_surface,
                    local.to_i32_round(),
                    scale,
                    1.0,
                    Kind::Unspecified,
                );
            (elements, None)
        }
        CursorImageStatus::Hidden => (Vec::new(), None),
        CursorImageStatus::Named(icon) => {
            let icon = *icon;
            let pointer_loc = state
                .seat
                .get_pointer()
                .map(|p| p.current_location())
                .unwrap_or_default();
            let local = (pointer_loc - output_loc.to_f64()).to_physical(scale);
            let elapsed = state.start_time.elapsed();
            let glyph = state
                .cursor_theme
                .as_mut()
                .and_then(|theme| {
                    theme.render_element(renderer, local, scale as u32, elapsed, icon)
                })
                .or_else(|| cursor::fallback_glyph_element(renderer, local.into()));
            (Vec::new(), glyph)
        }
    };

    // Locked: skip window/layer-shell rendering (and the group tab strip,
    // which would otherwise leak window titles over the lock screen)
    // entirely, rather than rendering them and relying on the lock
    // elements below to opaquely cover them -- `space_render_elements`
    // pulls layer-shell content from `layer_map_for_output(output)`
    // unconditionally, independent of whatever `spaces` it's given, so an
    // empty space alone would not have hidden a panel/bar.
    let tab_strip_elements = if locked {
        Vec::new()
    } else {
        state.tab_strip_elements(renderer, output)
    };

    // The overview shows window titles too -- same lock-gating as the tab
    // strip above, same reasoning.
    let overview_element = if locked {
        None
    } else {
        state
            .overview
            .as_ref()
            .filter(|overview| overview.output_name() == output.name())
            .and_then(|overview| overview.render_element(renderer))
    };
    // The minimap can show window titles too -- same lock-gating, same
    // reasoning as the overview above it.
    let minimap_element = if locked {
        Vec::new()
    } else {
        state.minimap_frame_element(renderer, output)
    };
    let depth_deck_element = if locked {
        None
    } else {
        state
            .depth_deck_overlay
            .as_ref()
            .filter(|deck| deck.output_name() == output.name())
            .and_then(|deck| deck.render_element(renderer))
    };
    let picker_element = if locked {
        None
    } else {
        state.screencast_picker_element(output, renderer)
    };

    let error_element = if locked {
        None
    } else {
        state.config_error_element(output, renderer)
    };

    let placements = if locked {
        Vec::new()
    } else {
        state.render_placements(output)?
    };
    let (depth_elements, depth_surfaces) = if locked {
        (Vec::new(), Vec::new())
    } else {
        state.depth_frame_elements(renderer, output, &placements)
    };
    #[allow(clippy::mutable_key_type)]
    let mut glass_layers = if locked {
        HashMap::new()
    } else {
        let surfaces = state.glass_eligible_surfaces(&placements);
        let mut layers = state.glass_layer_elements(renderer, output, &placements, &surfaces);
        layers.extend(state.layer_glass_elements(renderer, output));
        layers
    };
    // Glass windows render in their normal z-slot (the walk inserts each
    // glass layer behind its own surface); only depth-replaced windows are
    // skipped.
    let replaced_surfaces = depth_surfaces;
    // The canvas grid, caustics, and BelowWindows ripples all sit between
    // windows and the wallpaper. They are passed INTO
    // `desktop_render_elements` so they land *above* whatever wallpaper
    // engine is attached -- awww/swww/swaybg/hyprpaper are layer-shell
    // Background surfaces inside that walk, and anything spliced after it
    // would render behind the wallpaper.
    let ripple_layers = if locked {
        Default::default()
    } else {
        state.ripple_frame_elements(renderer, output)
    };
    let ocean_canvas = (!locked)
        .then(|| state.ocean_canvas_frame_element(renderer, output))
        .flatten();
    let caustics = (!locked)
        .then(|| state.caustics_frame_element(renderer, output))
        .flatten();
    let backdrop: Vec<OutputRenderElements> = ocean_canvas
        .into_iter()
        .chain(caustics)
        .chain(ripple_layers.below_windows)
        .collect();
    let space_elements = if locked {
        Vec::new()
    } else {
        state.desktop_render_elements(
            renderer,
            output,
            &placements,
            &replaced_surfaces,
            &mut glass_layers,
            backdrop,
        )?
    };

    let lock_elements = if locked {
        state.lock_render_elements(output, renderer)
    } else {
        Vec::new()
    };

    // Background-level placeholder, not transient UI -- behind
    // toast/overview/tab-strip/cursor in the chain below. Never shown
    // while locked, same as the tab strip/overview above.
    let welcome_element = if locked || !state.should_show_welcome_hint() {
        None
    } else {
        state
            .welcome_hint
            .as_ref()
            .and_then(|hint| hint.render_element(renderer, size))
    };

    let wallpaper_element = if locked {
        None
    } else {
        state.wallpaper_element(output, renderer)
    };

    let workspace_transition = if locked {
        None
    } else {
        state.workspace_transition_frame_element(renderer, output)
    };
    let depth_transition = if locked {
        None
    } else {
        state.depth_transition_frame_element(renderer, output)
    };
    let compass_elements = if locked {
        Vec::new()
    } else {
        state.compass_frame_elements(renderer, output)
    };
    let closing_windows = if locked {
        Vec::new()
    } else {
        state.closing_window_frame_elements(renderer, output)
    };
    let mut elements: Vec<OutputRenderElements> = ripple_layers.above_all;
    elements.extend(
        picker_element
            .into_iter()
            .chain(minimap_element)
            .chain(depth_deck_element)
            .chain(overview_element)
            .chain(toast_element)
            .chain(error_element)
            .chain(cursor_glyph_element)
            .chain(tab_strip_elements)
            .chain(welcome_element)
            .map(OutputRenderElements::Composited),
    );
    elements.extend(
        cursor_surface_element
            .into_iter()
            .map(OutputRenderElements::Cursor),
    );
    elements.extend(depth_transition);
    elements.extend(ripple_layers.above_windows);
    elements.extend(compass_elements);
    elements.extend(workspace_transition);
    elements.extend(closing_windows);
    elements.extend(depth_elements);
    elements.extend(space_elements);
    elements.extend(wallpaper_element.map(OutputRenderElements::Composited));
    elements.extend(lock_elements.into_iter().map(OutputRenderElements::Lock));
    elements.extend(ripple_layers.below_all);

    let render_result = surface.compositor.render_frame::<_, OutputRenderElements>(
        renderer,
        &elements,
        [0.05, 0.05, 0.05, 1.0],
        FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY,
    );

    let empty_frame_retry = match render_result {
        Ok(render_result) => {
            // KMS can consume the renderer's fence directly on the normal
            // path. If Smithay says it cannot, wait for the swapchain image
            // here before handing it to KMS. Niri, DriftWM and Smithay's own
            // DrmOutputManager all follow this contract.
            if render_result.needs_sync() {
                if let PrimaryPlaneElement::Swapchain(ref element) = render_result.primary_element {
                    // `Fence::wait` blocks on `eglClientWaitSyncKHR` with an
                    // infinite timeout; per that call's own spec (see
                    // Smithay's safety comment on `EGLFence::client_wait`),
                    // the only error it can return is an invalid sync
                    // object, not a transient interruption despite the
                    // error type's name -- retrying forever, as this used
                    // to, would hang the only event-loop thread on a single
                    // bad fence permanently, the same class of freeze the
                    // 0.15.1 `TileMoveGrab` deadlock caused. One attempt,
                    // then present whatever the swapchain already has
                    // rather than never presenting again.
                    if let Err(e) = element.sync.wait() {
                        tracing::error!(%e, "GPU fence wait failed (non-retryable); presenting without waiting");
                    }
                }
            }

            // wp_presentation feedback travels as the frame's user data so
            // the VBlank handler can present it with the flip's timing. If
            // `queue_frame` fails (including EmptyFrame) the value is
            // dropped, which discards the callbacks -- the correct answer
            // for content that never reached the display.
            let feedback = state.take_presentation_feedback(output, &render_result.states);
            match surface.compositor.queue_frame(Some(feedback)) {
                Ok(()) => {
                    surface.pending = true;
                    // A previously armed timer may still fire, but it will
                    // observe this flag and become a no-op.
                    surface.empty_frame_retry_pending = None;
                    // Only counts once the locked frame actually queued --
                    // see `mark_output_locked_frame`'s doc comment. No-op
                    // while unlocked or already `Locked`.
                    state.mark_output_locked_frame(output);
                    None
                }
                Err(FrameError::EmptyFrame) => Some(output_refresh_period(output)),
                Err(e) => {
                    tracing::warn!(%e, "Failed to queue DRM frame");
                    surface.dirty = true;
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!(%e, "Failed to render DRM frame");
            surface.dirty = true;
            None
        }
    };

    // Locked windows/layers aren't rendered above, so telling them a frame
    // was presented would just burn client CPU redrawing content nobody
    // sees; the lock surface gets its own frame callback instead.
    if locked {
        state.send_lock_frames(output, state.start_time.elapsed());
    } else {
        state.send_window_frames(output, state.start_time.elapsed());
        state.send_layer_frames(output, state.start_time.elapsed());
    }

    // Keep this CRTC's animation chain local to its own VBlank cadence. A
    // successful queue leaves `pending` set until that VBlank; an empty
    // frame uses the mode-derived retry timer. Going through the global
    // redraw eventfd here would immediately retry an EmptyFrame and turn an
    // otherwise damage-free animation gate into a busy loop.
    if state.has_active_animation() {
        surface.dirty = true;
    }

    empty_frame_retry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_retry_period_comes_from_the_live_mode() {
        let output = Output::new(
            "test".to_string(),
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
                // Deliberately arbitrary: scheduling must not depend on a
                // familiar monitor resolution.
                size: (37, 23).into(),
                refresh: 165_000,
            }),
            None,
            None,
            None,
        );

        assert_eq!(
            output_refresh_period(&output),
            Duration::from_nanos(1_000_000_000_000 / 165_000)
        );
    }

    #[test]
    fn estimated_vblank_wait_blocks_immediate_redraw() {
        assert!(surface_redraw_ready(true, false, false));
        assert!(!surface_redraw_ready(true, true, false));
        assert!(!surface_redraw_ready(true, false, true));
        assert!(!surface_redraw_ready(false, false, false));
    }
}
