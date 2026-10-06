use std::{
    num::NonZeroU32,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
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
    utils::Transform,
    wayland::presentation::Refresh,
};

use smithay::reexports::winit::{dpi::LogicalSize, window::Window as WinitWindow};

use crate::{state::SessionLock, Smallvil};

/// Unknown monitor data must not overwrite a previously known rate. The
/// initial fallback is used only until the host identifies the monitor.
fn host_refresh_millihertz(backend: &WinitGraphicsBackend<GlesRenderer>) -> Option<i32> {
    valid_refresh(
        backend
            .window()
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz()),
    )
}

fn valid_refresh(reported: Option<u32>) -> Option<i32> {
    reported
        .and_then(|rate| i32::try_from(rate).ok())
        .filter(|rate| *rate > 0)
}

fn replace_nested_mode(output: &Output, mode: Mode, scale: Option<smithay::output::Scale>) {
    let previous = output.current_mode();
    output.set_preferred(mode);
    output.change_current_state(Some(mode), None, scale, None);
    if let Some(previous) = previous.filter(|previous| *previous != mode) {
        output.delete_mode(previous);
    }
}

fn update_host_refresh(output: &Output, reported: Option<i32>) -> bool {
    let (Some(mut mode), Some(refresh)) = (output.current_mode(), reported) else {
        return false;
    };
    if mode.refresh == refresh {
        return false;
    }
    mode.refresh = refresh;
    replace_nested_mode(output, mode, None);
    true
}

/// One simulated output: a winit window standing in for a monitor.
struct WinitOutput {
    output: Output,
    backend: WinitGraphicsBackend<GlesRenderer>,
    winit_evt: WinitEventLoop,
    damage_tracker: OutputDamageTracker,
    /// Set whenever `Smallvil::take_redraw_requests()` includes this output
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
        // Same window attributes smithay's own `init()` defaults to,
        // minus its "Smithay" title -- the host shows this title bar.
        let (backend, winit_evt) = winit::init_from_attributes(
            WinitWindow::default_attributes()
                .with_inner_size(LogicalSize::new(1280.0, 800.0))
                .with_title("TideWM")
                .with_visible(true),
        )?;

        let mode = Mode {
            size: backend.window_size(),
            refresh: host_refresh_millihertz(&backend).unwrap_or(60_000),
        };
        let scale_factor = backend.scale_factor();

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
        // Forwarding the host's real scale factor (fractional included)
        // means nested clients get told the truth about pixel density via
        // wl_output/fractional-scale instead of a hardcoded 1 -- a 125%
        // host monitor was rendering kitty's image protocol at the wrong
        // size before this. Mode size stays physical pixels; smithay's
        // Space derives the logical size from it and this scale.
        output.change_current_state(
            Some(mode),
            Some(Transform::Flipped180),
            Some(smithay::output::Scale::Fractional(scale_factor)),
            Some((x_offset, 0).into()),
        );
        output.set_preferred(mode);

        state.space.map_output(&output, (x_offset, 0));
        state.wlr_output_management_state.refresh(&state.space);
        state.refresh_ext_workspaces();
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

    // A bounded host-cadence calloop Timer drives the loop, not
    // WinitEvent::Redraw/backend.window().request_redraw() (the pattern
    // smallvil used): nothing throttles how fast request_redraw() re-fires
    // on its own, and it was spinning the CPU at 100% even fully idle.
    // Absolute deadlines on a fixed epoch skip missed frames without
    // catch-up bursts or adding rendering time to the frame period. One
    // shared Timer pumps every simulated output; udev uses redraw wakeups
    // and independent VBlank chains. Both preserve per-output dirty state.
    let timer = Timer::immediate();
    let frame_epoch = Instant::now();
    event_loop
        .handle()
        .insert_source(timer, move |_, _, state| {
            state.update_window_depths();
            state.update_urgent_pulses();
            state.update_float_physics_full();
            state.update_currents();
            state.update_buoyancy();
            let mut closing = false;
            for entry in &mut outputs {
                let output = &entry.output;
                let backend = &entry.backend;
                entry.winit_evt.dispatch_new_events(|event| match event {
                    WinitEvent::Resized { size, scale_factor } => {
                        replace_nested_mode(
                            output,
                            Mode {
                                size,
                                refresh: host_refresh_millihertz(backend)
                                    .or_else(|| output.current_mode().map(|mode| mode.refresh))
                                    .unwrap_or(60_000),
                            },
                            Some(smithay::output::Scale::Fractional(scale_factor)),
                        );
                        entry.dirty = true;
                        // The layer map's cached non_exclusive_zone only
                        // recomputes on arrange(), which otherwise happens
                        // solely on layer-surface events -- without this,
                        // the retile below faithfully lays tiles out into
                        // the *old* output size (confirmed live: wallpaper
                        // filled a grown window while the tiled window
                        // stayed clipped at its old geometry).
                        smithay::desktop::layer_map_for_output(output).arrange();
                        #[cfg(feature = "screencast")]
                        if let Some(screencast) = &state.screencast {
                            screencast.refresh_outputs(state.space.outputs());
                        }
                        state.wlr_output_management_state.refresh(&state.space);
                        state.retile();
                    }
                    WinitEvent::Input(event) => state.process_input_event(event),
                    // The host taking keyboard focus swallows any key
                    // releases that happen while it holds the keyboard;
                    // reset our (and every client's) idea of what's held.
                    WinitEvent::Focus(false) => state.release_stuck_keys(),
                    WinitEvent::CloseRequested => closing = true,
                    _ => (),
                });
                // Smithay exposes no move event. Query after pumping host
                // events so same-size/same-scale monitor moves still update
                // mode metadata, presentation feedback and timer cadence.
                if update_host_refresh(output, host_refresh_millihertz(backend)) {
                    entry.dirty = true;
                    #[cfg(feature = "screencast")]
                    if let Some(screencast) = &state.screencast {
                        screencast.refresh_outputs(state.space.outputs());
                    }
                    state.wlr_output_management_state.refresh(&state.space);
                }
            }

            if closing {
                // Closing any one simulated output's window ends the whole
                // session, same as the single-output case always did --
                // this is dev-testing scaffolding, not output hotplug (see
                // backend/udev.rs for the real thing).
                state.loop_signal.stop();
                return TimeoutAction::Drop;
            }

            super::scene::prepare_render_scene(&mut state.space);
            // Expiration must run even if no output rendered this tick.
            state.has_active_animation();
            let redraw = state.take_redraw_requests();
            for entry in &mut outputs {
                if redraw.includes(&entry.output) {
                    entry.dirty = true;
                }
            }

            for entry in &mut outputs {
                if state
                    .caustics_redraw_delay_for_output(&entry.output)
                    .is_some_and(|delay| delay.is_zero())
                {
                    entry.dirty = true;
                }
                if !entry.dirty {
                    continue;
                }
                entry.dirty = false;

                tracing::trace!(output = entry.output.name(), "Compositing frame");
                let size = entry.backend.window_size();

                let locked = !matches!(state.session_lock, SessionLock::Unlocked);
                let placements = if locked {
                    Vec::new()
                } else {
                    match state.render_placements(&entry.output) {
                        Some(placements) => placements,
                        None => {
                            tracing::warn!("Failed to gather nested output placements");
                            entry.dirty = true;
                            continue;
                        }
                    }
                };

                // Backdrop capture is FBO-only and must happen outside the
                // visible bind/submit lifetime. Running it immediately
                // before `bind()` gives glass the current drag geometry in
                // this frame instead of a post-submit texture one frame
                // behind. Same-sized recaptures reuse their window texture.
                if !locked {
                    let renderer = entry.backend.renderer();
                    state.capture_window_backdrops(renderer, &entry.output, &placements);
                    state.capture_layer_backdrops(renderer, &entry.output, &placements);
                }

                let render_result = {
                    let (renderer, mut framebuffer) = match entry.backend.bind() {
                        Ok(bound) => bound,
                        Err(err) => {
                            tracing::warn!(
                                output = entry.output.name(),
                                ?err,
                                "Failed to bind nested output; retrying next frame"
                            );
                            entry.dirty = true;
                            continue;
                        }
                    };
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
                        entry.damage_tracker.render_output(
                            renderer,
                            &mut framebuffer,
                            0,
                            &lock_elements,
                            clear_color,
                        )
                    } else {
                        let error_element = state.config_error_element(&entry.output, renderer);
                        let toast_element = state.toast_element(&entry.output, renderer);
                        if state.toast.as_ref().is_some_and(|toast| toast.expired()) {
                            state.toast = None;
                            #[cfg(feature = "accessibility")]
                            state.sync_accessibility_tree();
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
                        let minimap_element = state.minimap_frame_element(renderer, &entry.output);
                        let depth_deck_element = state
                            .depth_deck_overlay
                            .as_ref()
                            .filter(|deck| deck.output_name() == entry.output.name())
                            .and_then(|deck| deck.render_element(renderer));
                        let picker_element =
                            state.screencast_picker_element(&entry.output, renderer);
                        let snap_preview = state.snap_preview_element(&entry.output);
                        // Behind toast/overview/tab-strip in the chain (see
                        // above for why index 0 is frontmost) -- it's a
                        // background-level placeholder, not transient UI.
                        let welcome_element = state
                            .should_show_welcome_hint()
                            .then_some(state.welcome_hint.as_ref())
                            .flatten()
                            .and_then(|hint| hint.render_element(renderer, size));
                        let wallpaper_element = state.wallpaper_element(&entry.output, renderer);
                        // Pulled out of their normal z-slot (skip) and
                        // rebuilt as their own element plus a selected glass
                        // layer, prepended ahead of everything else in
                        // space_elements -- see glass_frame_elements'
                        // own doc comment for why this means "topmost
                        // among windows," not real multi-window z-order.
                        let glass_surfaces = state.glass_eligible_surfaces(&placements);
                        #[allow(clippy::mutable_key_type)]
                        let mut glass_layers = state.glass_layer_elements(
                            renderer,
                            &entry.output,
                            &placements,
                            &glass_surfaces,
                        );
                        glass_layers.extend(state.layer_glass_elements(renderer, &entry.output));
                        let (depth_elements, depth_surfaces) =
                            state.depth_frame_elements(renderer, &entry.output, &placements);
                        // Only skip from the normal walk what actually got a
                        // replacement element built -- a shader-compile
                        // failure or missing output geometry makes
                        // glass_layer_elements return empty, and
                        // skipping windows the empty result won't draw would
                        // make them vanish from the frame entirely rather
                        // than just losing the effect.
                        // Glass windows render in their normal z-slot (the
                        // walk inserts each glass layer behind its own
                        // surface); only depth-replaced windows are skipped.
                        let skip = depth_surfaces;
                        // Ripple layers are grouped by `RippleLayer` so
                        // each backend can splice them at the right z
                        // position in the front-to-back list: AboveAll
                        // at the very front, AboveWindows between chrome
                        // and windows, BelowWindows between windows and
                        // wallpaper, BelowAll at the very back. Built as
                        // a Vec directly rather than `.chain()`ed because
                        // four distinct insertion points don't fit the
                        // chain's single-insertion-point shape.
                        let ripple_layers = state.ripple_frame_elements(renderer, &entry.output);
                        // The canvas grid, caustics, and BelowWindows ripples
                        // all sit between windows and the wallpaper. They are
                        // passed INTO `desktop_render_elements` so they land
                        // *above* whatever wallpaper engine is attached --
                        // awww/swww/swaybg/hyprpaper are layer-shell
                        // Background surfaces inside that walk, and anything
                        // spliced after it would render behind the wallpaper.
                        let ocean_canvas =
                            state.ocean_canvas_frame_element(renderer, &entry.output);
                        let caustics = state.caustics_frame_element(renderer, &entry.output);
                        let backdrop: Vec<crate::backend::udev::OutputRenderElements> =
                            ocean_canvas
                                .into_iter()
                                .chain(caustics)
                                .chain(ripple_layers.below_windows)
                                .collect();
                        let space_elements = match state.desktop_render_elements(
                            renderer,
                            &entry.output,
                            &placements,
                            &skip,
                            &mut glass_layers,
                            backdrop,
                        ) {
                            Some(elements) => elements,
                            None => {
                                tracing::warn!("Failed to gather nested output elements");
                                entry.dirty = true;
                                continue;
                            }
                        };
                        // Ripple layers are grouped by `RippleLayer` so
                        // each backend can splice them at the right z
                        // position in the front-to-back list: AboveAll
                        // at the very front, AboveWindows between chrome
                        // and windows, BelowWindows between windows and
                        // wallpaper, BelowAll at the very back. Built as
                        // a Vec directly rather than `.chain()`ed because
                        // four distinct insertion points don't fit the
                        // chain's single-insertion-point shape.
                        let workspace_transition =
                            state.workspace_transition_frame_element(renderer, &entry.output);
                        let workspace_glide = state.workspace_glide_frame_element(&entry.output);
                        let depth_transition =
                            state.depth_transition_frame_element(renderer, &entry.output);
                        let compass_elements =
                            state.compass_frame_elements(renderer, &entry.output);
                        let closing_windows =
                            state.closing_window_frame_elements(renderer, &entry.output);
                        let mut elements: Vec<crate::backend::udev::OutputRenderElements> =
                            ripple_layers.above_all;
                        elements.extend(
                            picker_element
                                .into_iter()
                                .chain(minimap_element)
                                .chain(depth_deck_element)
                                .chain(overview_element)
                                .chain(toast_element)
                                .chain(error_element)
                                .chain(state.tab_strip_elements(
                                    renderer,
                                    &entry.output,
                                    &placements,
                                ))
                                .chain(welcome_element)
                                .map(crate::backend::udev::OutputRenderElements::Composited),
                        );
                        elements.extend(depth_transition);
                        elements.extend(ripple_layers.above_windows);
                        elements.extend(
                            snap_preview.map(crate::backend::udev::OutputRenderElements::Dim),
                        );
                        elements.extend(compass_elements);
                        elements.extend(workspace_transition);
                        elements.extend(workspace_glide);
                        elements.extend(closing_windows);
                        elements.extend(depth_elements);
                        elements.extend(space_elements);
                        elements.extend(
                            wallpaper_element
                                .map(crate::backend::udev::OutputRenderElements::Wallpaper),
                        );
                        elements.extend(ripple_layers.below_all);

                        let result = entry.damage_tracker.render_output(
                            renderer,
                            &mut framebuffer,
                            0,
                            &elements,
                            [0.05, 0.05, 0.05, 1.0],
                        );
                        result.map(|mut result| {
                            super::presentation::report_surface_identities(
                                &elements,
                                &mut result.states,
                            );
                            result
                        })
                    }
                };
                let render_result = match render_result {
                    Ok(result) => result,
                    Err(err) => {
                        tracing::warn!(
                            output = entry.output.name(),
                            ?err,
                            "Failed to render nested output; retrying next frame"
                        );
                        entry.dirty = true;
                        continue;
                    }
                };
                let submitted = if let Some(damage) = render_result.damage {
                    if let Err(err) = entry.backend.submit(Some(damage)) {
                        tracing::warn!(
                            output = entry.output.name(),
                            ?err,
                            "Failed to submit nested output; retrying next frame"
                        );
                        entry.dirty = true;
                        continue;
                    }
                    true
                } else {
                    false
                };
                if submitted {
                    state.mark_output_locked_frame(&entry.output);
                }

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
                if submitted {
                    feedback.presented(
                        state.clock.now(),
                        entry
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
                state.capture_pending_workspace_transition(renderer, &entry.output);
                let output = &entry.output;
                if locked {
                    state.send_lock_frames(output, state.start_time.elapsed());
                    state.send_dnd_icon_frames(output, state.start_time.elapsed());
                } else {
                    state.send_window_frames(output, state.start_time.elapsed());
                    state.send_layer_frames(output, state.start_time.elapsed());
                }
                entry.dirty = state.output_has_active_animation(output, &placements);
            }

            state.popups.cleanup();
            state.refresh_popup_grab();
            state.cleanup_capture();

            // Use a rational frame grid shared with the screencast producer.
            // Include all work above when skipping past missed deadlines.
            let refresh = outputs
                .iter()
                .filter_map(|entry| entry.output.current_mode())
                .filter_map(|mode| u32::try_from(mode.refresh).ok().and_then(NonZeroU32::new))
                .max()
                .unwrap_or(NonZeroU32::new(60_000).unwrap());
            let now = Instant::now();
            TimeoutAction::ToInstant(
                now + crate::frame_clock::next_frame_delay(
                    now.saturating_duration_since(frame_epoch),
                    refresh,
                ),
            )
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> Output {
        let output = Output::new(
            "test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        replace_nested_mode(
            &output,
            Mode {
                size: (1280, 800).into(),
                refresh: 60_000,
            },
            Some(smithay::output::Scale::Fractional(1.25)),
        );
        output
    }

    #[test]
    fn same_size_same_scale_monitor_move_updates_current_and_preferred_rate() {
        let output = output();
        let size = output.current_mode().unwrap().size;
        for refresh in [144_000, 59_940, 500_000, 60_000] {
            assert!(update_host_refresh(&output, valid_refresh(Some(refresh))));
            let mode = output.current_mode().unwrap();
            assert_eq!(mode.refresh, refresh as i32);
            assert_eq!(mode.size, size);
            assert_eq!(output.current_scale().fractional_scale(), 1.25);
            assert_eq!(output.preferred_mode(), Some(mode));
            assert_eq!(output.modes(), [mode]);
            assert!(!update_host_refresh(&output, valid_refresh(Some(refresh))));
        }
    }

    #[test]
    fn missing_or_invalid_monitor_data_retains_last_known_refresh() {
        let output = output();
        assert!(update_host_refresh(&output, valid_refresh(Some(75_437))));
        let mode = output.current_mode().unwrap();
        for reported in [None, Some(0), Some(u32::MAX)] {
            assert!(!update_host_refresh(&output, valid_refresh(reported)));
            assert_eq!(output.current_mode(), Some(mode));
            assert_eq!(output.preferred_mode(), Some(mode));
            assert_eq!(output.modes(), [mode]);
        }
        assert_eq!(valid_refresh(Some(24_000)), Some(24_000));
        assert_eq!(valid_refresh(Some(500_000)), Some(500_000));
    }
}
