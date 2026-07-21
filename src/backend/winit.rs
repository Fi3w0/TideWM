use std::time::Duration;

use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker, element::memory::MemoryRenderBufferRenderElement,
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent, WinitEventLoop, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop,
        },
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    },
    utils::{Rectangle, Transform},
    wayland::presentation::Refresh,
};

use crate::{state::SessionLock, Smallvil};

/// One simulated output: a winit window standing in for a monitor.
struct WinitOutput {
    output: Output,
    backend: WinitGraphicsBackend<GlesRenderer>,
    winit_evt: WinitEventLoop,
    damage_tracker: OutputDamageTracker,
    /// Set whenever `Smallvil::take_needs_redraw()` observes dirty state
    /// (see the shared `Timer` below) and cleared once this output actually
    /// renders. Per-output so N independent windows sharing one global
    /// dirty flag don't race each other -- without this, only whichever
    /// output's turn it is when the flag gets consumed would ever redraw,
    /// same class of bug `backend/udev.rs`'s per-surface `dirty` flag
    /// exists to avoid for its multiple CRTCs.
    dirty: bool,
}

pub fn init_winit(
    event_loop: &mut EventLoop<Smallvil>,
    state: &mut Smallvil,
) -> Result<(), Box<dyn std::error::Error>> {
    // Nested multi-monitor testing (multiple simulated outputs, each its
    // own winit window) was tried and reverted: winit 0.30 enforces a
    // process-global "at most one EventLoop, ever" (`EVENT_LOOP_CREATED`,
    // an `AtomicBool` in winit's own `event_loop.rs`, no bypass) -- a
    // second `winit::init()` call fails outright with `RecreationAttempt`,
    // confirmed empirically, not just a sandbox limitation. Real
    // multi-monitor testing needs actual hardware and the udev backend.
    //
    // The loop below still only ever runs once (`output_count` is always
    // 1), but keeps the per-output `dirty` flag / shared-`Timer`
    // structure below rather than the simpler single-output form this had
    // before: it's the correct pattern regardless of N (see `WinitOutput`),
    // matches `backend/udev.rs`'s own multi-CRTC loop, and means this
    // doesn't need reshaping again if winit ever lifts the restriction.
    let output_count = 1;

    let mut outputs = Vec::with_capacity(output_count);
    let mut x_offset = 0;
    for index in 0..output_count {
        let (backend, winit_evt) = winit::init()?;

        let mode = Mode {
            size: backend.window_size(),
            refresh: 60_000,
        };

        let output = Output::new(
            format!("winit-{index}"),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Smithay".into(),
                model: "Winit".into(),
                serial_number: "Unknown".into(),
            },
        );
        // Unlike udev.rs's per-connector globals (see `SurfaceData::global`
        // there), discarding this `GlobalId` is fine: winit has exactly one
        // simulated output that lives for the whole process and is never
        // individually disconnected -- there is no runtime removal path to
        // retract it from, only process exit tearing down the display
        // (and every global on it) as a whole.
        let _global = output.create_global::<Smallvil>(&state.display_handle);
        output.change_current_state(
            Some(mode),
            Some(Transform::Flipped180),
            None,
            Some((x_offset, 0).into()),
        );
        output.set_preferred(mode);

        state.space.map_output(&output, (x_offset, 0));
        state.wlr_output_management_state.refresh(&state.space);
        x_offset += mode.size.w;

        let damage_tracker = OutputDamageTracker::from_output(&output);
        outputs.push(WinitOutput {
            output,
            backend,
            winit_evt,
            damage_tracker,
            dirty: true,
        });
    }

    // Only safe to overwrite our own env now, after every winit window in
    // `outputs` has already been created: winit's own backend init reads
    // WAYLAND_DISPLAY to find the *host* compositor to connect to, so
    // overwriting it any earlier makes winit try to connect to TideWM's own
    // (not yet running) socket instead and hang forever. Confirmed by
    // hitting exactly that hang after moving this into `Smallvil::new()`.
    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);

    // A bounded ~60Hz calloop Timer drives the loop, not
    // WinitEvent::Redraw/backend.window().request_redraw() (the pattern
    // smallvil used): nothing throttles how fast request_redraw() re-fires
    // on its own, and it was spinning the CPU at 100% even fully idle. A
    // Timer physically can't fire faster than its re-arm duration, so this
    // is safe by construction (same pattern DriftWM's winit backend uses,
    // and the same one `backend/udev.rs` uses for its own render loop). One
    // shared Timer drives every simulated output, exactly like udev.rs
    // drives every CRTC from its one Timer -- see `WinitOutput::dirty`.
    let timer = Timer::immediate();
    event_loop
        .handle()
        .insert_source(timer, move |_, _, state| {
            let mut closing = false;
            for entry in &mut outputs {
                let output = &entry.output;
                entry.winit_evt.dispatch_new_events(|event| match event {
                    WinitEvent::Resized { size, .. } => {
                        output.change_current_state(
                            Some(Mode {
                                size,
                                refresh: 60_000,
                            }),
                            None,
                            None,
                            None,
                        );
                        state.wlr_output_management_state.refresh(&state.space);
                        state.retile();
                    }
                    WinitEvent::Input(event) => state.process_input_event(event),
                    WinitEvent::CloseRequested => closing = true,
                    _ => (),
                });
            }

            if closing {
                // Closing any one simulated output's window ends the whole
                // session, same as the single-output case always did --
                // this is dev-testing scaffolding, not output hotplug (see
                // backend/udev.rs for the real thing).
                state.loop_signal.stop();
                return TimeoutAction::Drop;
            }

            if state.take_needs_redraw() {
                for entry in &mut outputs {
                    entry.dirty = true;
                }
            }

            for entry in &mut outputs {
                if !entry.dirty {
                    continue;
                }
                entry.dirty = false;

                tracing::trace!(output = entry.output.name(), "Compositing frame");
                let size = entry.backend.window_size();
                let damage = Rectangle::from_size(size);

                let locked = !matches!(state.session_lock, SessionLock::Unlocked);

                let render_result = {
                    let (renderer, mut framebuffer) = entry.backend.bind().unwrap();

                    if locked {
                        // Bypass the `render_output` convenience below: it
                        // pulls layer-shell content from
                        // `layer_map_for_output` unconditionally (regardless
                        // of what `spaces` it's given), so an empty window
                        // space alone would not have hidden a panel/bar.
                        // Rendering only `lock_render_elements` skips
                        // windows and layers entirely.
                        let lock_elements = state.lock_render_elements(&entry.output, renderer);
                        let clear_color = [0.0, 0.0, 0.0, 1.0];
                        entry
                            .damage_tracker
                            .render_output(renderer, &mut framebuffer, 0, &lock_elements, clear_color)
                            .unwrap()
                    } else {
                        let toast_element = state
                            .toast
                            .as_ref()
                            .and_then(|toast| toast.render_element(renderer, size));
                        if state.toast.is_some() && toast_element.is_none() {
                            // Fully faded out.
                            state.toast = None;
                        }
                        // Drawn first so it ends up topmost (index 0 in a
                        // render element list is the front, this codebase's
                        // established convention) -- it's meant to sit over
                        // everything else, toast included.
                        let overview_element = state
                            .overview
                            .as_ref()
                            .filter(|overview| overview.output_name() == entry.output.name())
                            .and_then(|overview| overview.render_element(renderer));
                        // Behind toast/overview/tab-strip in the chain (see
                        // above for why index 0 is frontmost) -- it's a
                        // background-level placeholder, not transient UI.
                        let welcome_element = state
                            .should_show_welcome_hint()
                            .then_some(state.welcome_hint.as_ref())
                            .flatten()
                            .and_then(|hint| hint.render_element(renderer, size));
                        let custom_elements: Vec<MemoryRenderBufferRenderElement<GlesRenderer>> =
                            overview_element
                                .into_iter()
                                .chain(toast_element)
                                .chain(state.tab_strip_elements(renderer))
                                .chain(welcome_element)
                                .collect();

                        smithay::desktop::space::render_output::<
                            _,
                            MemoryRenderBufferRenderElement<GlesRenderer>,
                            _,
                            _,
                        >(
                            &entry.output,
                            renderer,
                            &mut framebuffer,
                            1.0,
                            0,
                            [&state.space],
                            &custom_elements,
                            &mut entry.damage_tracker,
                            [0.1, 0.1, 0.1, 1.0],
                        )
                        .unwrap()
                    }
                };
                entry.backend.submit(Some(&[damage])).unwrap();
                state.mark_output_locked_frame(&entry.output);

                // wp_presentation feedback: collected from the same render
                // element states the frame was just drawn with, timestamped
                // at submit time (no real vblank event exists on this
                // backend -- the host compositor owns the actual scanout
                // timing). A frame the damage tracker reported nothing for
                // carried no new client content, so its feedback is dropped
                // here, which discards the callbacks (see
                // `take_presentation_feedback`).
                let mut feedback =
                    state.take_presentation_feedback(&entry.output, &render_result.states);
                if render_result.damage.is_some() {
                    feedback.presented(
                        state.clock.now(),
                        entry
                            .output
                            .current_mode()
                            .map(|mode| {
                                Refresh::fixed(Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
                            })
                            .unwrap_or(Refresh::Unknown),
                        0,
                        wp_presentation_feedback::Kind::Vsync,
                    );
                }

                // Screen captures drain here, after the visible frame is
                // submitted rather than between bind and submit: the
                // capture's offscreen render re-targets the GL context
                // (FBO + surfaceless make_current), which breaks the winit
                // surface's bind/submit lifecycle when interleaved with it
                // (submit failed with ContextLost, confirmed live). FBO
                // work needs no EGL surface, so the renderer is used
                // directly here. No cursor compositing under winit: the
                // pointer on screen is the host compositor's, never part
                // of TideWM's own frame.
                let renderer = entry.backend.renderer();
                state.render_pending_captures(renderer, &entry.output, false);

                let output = &entry.output;
                if locked {
                    state.send_lock_frames(output, state.start_time.elapsed());
                } else {
                    state.space.elements().for_each(|window| {
                        window.send_frame(
                            output,
                            state.start_time.elapsed(),
                            Some(Duration::ZERO),
                            |_, _| Some(output.clone()),
                        )
                    });
                    state.send_layer_frames(output, state.start_time.elapsed());
                }
            }

            // A toast still fading needs another frame even though nothing
            // else marked itself dirty in the meantime. Goes through the
            // normal request_redraw() -> take_needs_redraw() path (checked
            // at the top of *next* tick), which re-dirties every output,
            // not just whichever one(s) just rendered. Skipped for a
            // persistent toast (see Toast::needs_continued_redraw) -- its
            // pixels never change after the first render, so looping this
            // forever would just burn cycles on identical frames.
            if state.toast.as_ref().is_some_and(|toast| toast.needs_continued_redraw()) {
                state.request_redraw();
            }

            state.space.refresh();
            state.popups.cleanup();
            state.refresh_popup_grab();
            state.cleanup_capture();
            state.cleanup_wlr_foreign_toplevels();
            let _ = state.display_handle.flush_clients();

            TimeoutAction::ToDuration(Duration::from_millis(16))
        })?;

    Ok(())
}
