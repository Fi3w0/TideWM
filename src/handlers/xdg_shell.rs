use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    delegate_xdg_shell,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Window,
        WindowSurfaceType,
    },
    input::{
        pointer::{Focus, GrabStartData as PointerGrabStartData},
        Seat,
    },
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            protocol::{wl_output, wl_seat, wl_surface::WlSurface},
            Resource,
        },
    },
    utils::{Rectangle, Serial, Size, SERIAL_COUNTER},
    wayland::shell::xdg::{
        PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    },
};

use crate::{
    grabs::{resize_grab, MoveSurfaceGrab, ResizeSurfaceGrab},
    state::{FullscreenEntry, MaximizedEntry, PopupGrabState},
    Smallvil,
};

impl XdgShellHandler for Smallvil {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        let wl_surface = window.toplevel().unwrap().wl_surface().clone();

        // Creating an xdg_toplevel role does not map a window. The client
        // first performs an empty commit, receives and acknowledges an
        // initial configure, and only becomes mapped once it commits a
        // non-null buffer. Niri follows the same explicit lifecycle; doing
        // layout/focus work here made empty roles visible to WM policy and
        // made a later null-buffer unmap indistinguishable from a hidden
        // workspace.
        self.unmapped_toplevels.insert(wl_surface, window);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let preferred_output = self.preferred_output_for_toplevel(surface.wl_surface());

        // Many clients destroy the xdg role directly without first
        // committing a null buffer. The last-frame snapshot is independent
        // of the live role, so this path can animate exactly like unmap.
        self.start_window_close_animation(surface.wl_surface());
        self.restore_swallowed(surface.wl_surface());
        self.unmapped_toplevels.remove(surface.wl_surface());
        self.detach_mapped_toplevel(surface.wl_surface());
        self.forget_window_focus(surface.wl_surface());
        self.retile();
        self.repair_keyboard_focus(preferred_output.as_deref(), SERIAL_COUNTER.next_serial());
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        self.unmapped_popup_surfaces
            .insert(surface.wl_surface().clone());
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!(?err, "Failed to track xdg popup");
        }
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        self.unmapped_popup_surfaces.remove(surface.wl_surface());
        // The popup pixels are part of its root Window/LayerSurface render
        // element. Removing the role does not otherwise dirty that output.
        self.request_redraw();
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        // Every window is tiled for now (no floating support yet), so a free
        // drag would just be overwritten by the next retile. Revisit once
        // floating windows exist: this check will then only block tiled ones.
        if self.layout.contains(surface.wl_surface())
            || self.ocean.is_tiled(surface.wl_surface())
            || self.fullscreen.contains_key(surface.wl_surface())
            || self.maximized.contains_key(surface.wl_surface())
        {
            return;
        }

        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        if seat != self.seat {
            return;
        }

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            // The grab is correctly scoped to this surface, but it may
            // legitimately no longer be mapped (floated-and-hidden on
            // another workspace, or destroyed, between the click and this
            // request) -- bail instead of panicking.
            let Some(window) = self
                .space
                .elements()
                .find(|w| w.toplevel().unwrap().wl_surface() == wl_surface)
                .cloned()
            else {
                return;
            };
            let Some(initial_window_location) = self.space.element_location(&window) else {
                return;
            };

            let last_location = start_data.location;
            let grab = MoveSurfaceGrab {
                start_data,
                window,
                initial_window_location,
                view_scale: self
                    .ocean
                    .entry_output(wl_surface)
                    .map(|output| self.ocean.camera(output).zoom)
                    .unwrap_or(1.0),
                smart_attach_ocean: self.config.spatial_engine
                    == crate::config::SpatialEngine::Ocean
                    && self.config.ocean.smart_tiling,
                last_location,
            };

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        // See move_request: tiled windows don't free-resize yet.
        if self.layout.contains(surface.wl_surface())
            || self.ocean.is_tiled(surface.wl_surface())
            || self.fullscreen.contains_key(surface.wl_surface())
            || self.maximized.contains_key(surface.wl_surface())
        {
            return;
        }

        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        if seat != self.seat {
            return;
        }

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            // See move_request: the grab can outlive the surface being mapped.
            let Some(window) = self
                .space
                .elements()
                .find(|w| w.toplevel().unwrap().wl_surface() == wl_surface)
                .cloned()
            else {
                return;
            };
            let Some(initial_window_location) = self.space.element_location(&window) else {
                return;
            };
            let initial_window_size = window.geometry().size;

            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });

            surface.send_pending_configure();

            let grab = ResizeSurfaceGrab::start(
                start_data,
                window,
                edges.into(),
                Rectangle::new(initial_window_location, initial_window_size),
                self.ocean
                    .entry_output(wl_surface)
                    .map(|output| self.ocean.camera(output).zoom)
                    .unwrap_or(1.0),
            );

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        wl_output: Option<wl_output::WlOutput>,
    ) {
        let wl_surface = surface.wl_surface().clone();
        self.do_fullscreen_request(surface, wl_output);
        self.refresh_wlr_toplevel_state(&wl_surface);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.do_unfullscreen(&surface);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        self.do_maximize_request(surface);
        self.refresh_wlr_toplevel_state(&wl_surface);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        self.do_unmaximize_request(surface);
        self.refresh_wlr_toplevel_state(&wl_surface);
    }

    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        let popup = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&popup) else {
            tracing::debug!("Ignoring popup grab without a live root surface");
            return;
        };

        let Some(has_keyboard_grab) = self.popup_grab_policy(&root) else {
            tracing::debug!("Dismissing popup grab whose root does not own focus");
            let _ = PopupManager::dismiss_popup(&root, &popup);
            return;
        };

        let mut grab = match self
            .popups
            .grab_popup(root.clone(), popup, &self.seat, serial)
        {
            Ok(grab) => grab,
            Err(err) => {
                tracing::debug!(?err, "Ignoring invalid popup grab");
                return;
            }
        };

        let keyboard = self.seat.get_keyboard().unwrap();
        let pointer = self.seat.get_pointer().unwrap();
        let keyboard_mismatch = has_keyboard_grab
            && keyboard.is_grabbed()
            && !(keyboard.has_grab(serial)
                || grab
                    .previous_serial()
                    .is_some_and(|previous| keyboard.has_grab(previous)));
        let pointer_mismatch = pointer.is_grabbed()
            && !(pointer.has_grab(serial)
                || grab
                    .previous_serial()
                    .is_some_and(|previous| pointer.has_grab(previous)));
        if keyboard_mismatch || pointer_mismatch {
            tracing::debug!("Dismissing popup grab that conflicts with an unrelated grab");
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }

        if has_keyboard_grab {
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        self.popup_grab = Some(PopupGrabState {
            root,
            grab,
            has_keyboard_grab,
        });
        // Keep logical focus/activation on the root while immediately moving
        // real wl_keyboard focus to the topmost popup, as Smithay's Anvil
        // example does. Waiting for the first key event leaves menus briefly
        // focused on their parent.
        self.reconcile_keyboard_focus(serial);
    }
}

impl Smallvil {
    fn do_fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        wl_output: Option<wl_output::WlOutput>,
    ) {
        let wl_surface = surface.wl_surface().clone();
        let Some(window) = self.toplevel_window(&wl_surface) else {
            Self::send_forced_configure(&surface);
            return;
        };
        resize_grab::cancel(&wl_surface);
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Resizing);
        });

        let requested_output = wl_output.as_ref().and_then(Output::from_resource);
        // A mapped window's Layouts/FloatingTag ownership is authoritative.
        // Merely changing FullscreenEntry to a different client-hinted output
        // left the window owned by A but sized for B. Tide has no individual
        // send-to-output primitive yet, and xdg-shell explicitly allows the
        // compositor to ignore this hint, so honor it only before first map.
        let owned_output = self
            .preferred_output_for_toplevel(&wl_surface)
            .and_then(|name| self.output_by_name(&name));
        let output = if self.unmapped_toplevels.contains_key(&wl_surface) {
            requested_output.or_else(|| self.primary_output())
        } else {
            if requested_output
                .as_ref()
                .is_some_and(|requested| owned_output.as_ref() != Some(requested))
            {
                tracing::debug!(
                    "Ignoring fullscreen output hint until individual window output moves exist"
                );
            }
            owned_output
                .or(requested_output)
                .or_else(|| self.primary_output())
        };
        let Some(output) = output else {
            Self::send_forced_configure(&surface);
            return;
        };

        // Toolkits may reassert fullscreen on focus changes. Re-sending the
        // target state is required, but exiting and re-entering here would
        // recapture the fullscreen viewport as the windowed restore geometry.
        if self
            .fullscreen
            .get(&wl_surface)
            .is_some_and(|entry| entry.output == output.name())
        {
            let Some(output_geo) = self.space.output_geometry(&output) else {
                Self::send_forced_configure(&surface);
                return;
            };
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.states.unset(xdg_toplevel::State::Maximized);
                state.states.unset(xdg_toplevel::State::Resizing);
                state.size = Some(output_geo.size);
            });
            Self::send_forced_configure(&surface);
            self.retile();
            return;
        }

        // Validate that the output is still mapped before changing the
        // existing fullscreen owner. A stale wl_output hint must not evict a
        // valid fullscreen window and then abort for missing geometry.
        let Some(output_geo) = self.space.output_geometry(&output) else {
            Self::send_forced_configure(&surface);
            return;
        };

        // At most one fullscreen window per output: pre-empt any existing
        // one, telling it (not just our own bookkeeping) that it's no
        // longer fullscreen.
        let existing = self
            .fullscreen
            .iter()
            .find(|(candidate, entry)| *candidate != &wl_surface && entry.output == output.name())
            .map(|(surface, _)| surface.clone());
        if let Some(existing_surface) = existing {
            let existing_toplevel = self
                .toplevel_window(&existing_surface)
                .as_ref()
                .and_then(|w| w.toplevel().cloned());
            match existing_toplevel {
                Some(existing_toplevel) => self.do_unfullscreen(&existing_toplevel),
                // The window is between states (its toplevel couldn't be
                // resolved right now) and do_unfullscreen never ran, so its
                // FullscreenEntry would otherwise survive untouched --
                // leaving two entries claiming the same output once the
                // insert below adds this window's own, violating "at most
                // one fullscreen per output" (checked by
                // assert_state_invariants in debug builds, silently
                // violated in release). Drop the stale bookkeeping
                // directly since there's no live toplevel left to send an
                // unfullscreen configure to anyway.
                None => {
                    self.fullscreen.remove(&existing_surface);
                }
            }
        }

        // Only a floating window needs its rect remembered -- a tiled one
        // never leaves its `Layouts` slot (see `Smallvil::retile`), so it
        // has somewhere to fall back to for free.
        let (previous_restore_rect, previous_was_pinned) = self
            .fullscreen
            .remove(&wl_surface)
            .map(|entry| (entry.restore_rect, entry.was_pinned))
            .unwrap_or((None, false));
        let restore_rect = previous_restore_rect
            .or_else(|| {
                self.maximized
                    .get(&wl_surface)
                    .map(|entry| entry.restore_rect)
            })
            .or_else(|| {
                if !self.unmapped_toplevels.contains_key(&wl_surface)
                    && !self.layout.contains(&wl_surface)
                    && !self.ocean.is_tiled(&wl_surface)
                {
                    self.space.element_geometry(&window)
                } else {
                    None
                }
            });

        let was_pinned = previous_was_pinned || self.pinned.remove(&wl_surface);
        if was_pinned {
            let active_workspace = self.layout.active_workspace(&output.name());
            if let Some(tag) = self.floating_workspace.get_mut(&wl_surface) {
                tag.output = output.name();
                tag.workspace = active_workspace;
            }
        }
        self.fullscreen.insert(
            wl_surface,
            FullscreenEntry {
                output: output.name(),
                restore_rect,
                was_pinned,
                pin_floated_it: false,
            },
        );

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.states.unset(xdg_toplevel::State::Maximized);
            state.states.unset(xdg_toplevel::State::Resizing);
            state.size = Some(output_geo.size);
        });
        // Before the client's initial empty commit, keep this in pending
        // state; `handle_commit` will include it in the initial configure.
        Self::send_forced_configure(&surface);

        self.retile();
    }

    fn do_maximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        // Only meaningful for a floating window -- a tiled one already
        // fills its slot. The protocol still requires a reply either way.
        if self.layout.contains(wl_surface) || self.ocean.is_tiled(wl_surface) {
            self.maximized.remove(wl_surface);
            // No `state.size` write here: it's a documented no-op for a
            // tiled window, which already has the correct size from
            // whichever last set it -- retile()'s own configure for the
            // ordinary case, or do_fullscreen_request's explicit
            // `state.size = Some(output_geo.size)` if this window is also
            // fullscreen (pending state carries forward, it isn't reset
            // per call). Writing it again here just re-asserts a size the
            // client already has, forcing a configure round-trip and
            // redraw for a value that never actually changed.
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Maximized);
                state.states.unset(xdg_toplevel::State::Resizing);
                if self.fullscreen.contains_key(wl_surface) {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                }
            });
            Self::send_forced_configure(&surface);
            return;
        }
        if self.unmapped_toplevels.contains_key(wl_surface) {
            self.maximized.remove(wl_surface);
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Maximized);
            });
            Self::send_forced_configure(&surface);
            return;
        }
        let Some(window) = self.toplevel_window(wl_surface) else {
            Self::send_forced_configure(&surface);
            return;
        };
        resize_grab::cancel(wl_surface);
        let output = self
            .preferred_output_for_toplevel(wl_surface)
            .and_then(|name| self.output_by_name(&name))
            .or_else(|| self.primary_output());
        let Some(output) = output else {
            Self::send_forced_configure(&surface);
            return;
        };
        let Some(area) = self.output_tiling_area(&output) else {
            Self::send_forced_configure(&surface);
            return;
        };
        // Same area (and gap) tiled windows use, so a maximized floating
        // window looks visually consistent with the tiled layer around it.
        let workspace = self.layout.active_workspace(&output.name());
        let rect = crate::layout::inset(area, self.gaps_for(&output.name(), workspace));

        if !self.maximized.contains_key(wl_surface) {
            let restore_rect = self
                .fullscreen
                .get(wl_surface)
                .and_then(|entry| entry.restore_rect)
                .or_else(|| self.space.element_geometry(&window))
                .or_else(|| self.floating_workspace.get(wl_surface).map(|tag| tag.rect));
            let Some(restore_rect) = restore_rect else {
                Self::send_forced_configure(&surface);
                return;
            };
            self.maximized.insert(
                wl_surface.clone(),
                MaximizedEntry {
                    output: output.name(),
                    restore_rect,
                },
            );
        }

        // While fullscreen, maximize is return-mode intent only. The two
        // protocol states and their competing geometries must never be active
        // simultaneously.
        if !self.fullscreen.contains_key(wl_surface) {
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Maximized);
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.states.unset(xdg_toplevel::State::Resizing);
                state.size = Some(rect.size);
            });
            if self.window_is_visible(wl_surface) {
                self.space.map_element(window, rect.loc, false);
            }
        }
        Self::send_forced_configure(&surface);
        self.retile();
    }

    fn do_unmaximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        let entry = self.maximized.remove(wl_surface);
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
        });

        // Fullscreen remains the current mode; this request only cancels the
        // maximized mode that would otherwise be restored on fullscreen exit.
        if self.fullscreen.contains_key(wl_surface) {
            Self::send_forced_configure(&surface);
            return;
        }

        if let Some(entry) = entry {
            surface.with_pending_state(|state| {
                state.size = Some(entry.restore_rect.size);
            });
            if let Some(tag) = self.floating_workspace.get_mut(wl_surface) {
                tag.rect = entry.restore_rect;
            }
            let window = self
                .space
                .elements()
                .find(|window| {
                    window
                        .toplevel()
                        .is_some_and(|toplevel| toplevel.wl_surface() == wl_surface)
                })
                .cloned();
            if let Some(window) = window {
                self.space
                    .map_element(window, entry.restore_rect.loc, false);
            }
        }
        Self::send_forced_configure(&surface);
        self.retile();
    }
}

// Xdg Shell
delegate_xdg_shell!(Smallvil);

fn check_grab(
    seat: &Seat<Smallvil>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<Smallvil>> {
    let pointer = seat.get_pointer()?;

    // Check that this surface has a click grab.
    if !pointer.has_grab(serial) {
        return None;
    }

    let start_data = pointer.grab_start_data()?;

    let (focus, _) = start_data.focus.as_ref()?;
    // If the focus was for a different surface, ignore the request. A
    // client could otherwise get a valid grab serial from clicking one of
    // its own surfaces, then request move/resize naming a *different*
    // surface it also owns -- same-client alone doesn't catch that.
    if focus != surface {
        return None;
    }

    Some(start_data)
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(state: &mut Smallvil, surface: &WlSurface) {
    let tracking = if state.unmapped_toplevels.contains_key(surface) {
        ToplevelTracking::Unmapped
    } else if state.mapped_toplevel_window(surface).is_some() {
        ToplevelTracking::Mapped
    } else {
        ToplevelTracking::Unknown
    };

    let has_buffer =
        with_renderer_surface_state(surface, |renderer_state| renderer_state.buffer().is_some())
            .unwrap_or(false);

    let transition = lifecycle_transition(tracking, has_buffer);
    match transition {
        ToplevelTransition::Map => {
            state.note_toplevel_flutter(surface, true);
            state.map_toplevel(surface);
        }
        ToplevelTransition::Unmap => {
            state.note_toplevel_flutter(surface, false);
            state.unmap_toplevel(surface);
        }
        ToplevelTransition::None => {
            // The map commit itself is the client's natural size, sent
            // before it ever saw the tiled configure -- never count it.
            // From the second commit on, a tiled window that keeps
            // rendering at a different size is refusing the tile.
            if tracking == ToplevelTracking::Mapped {
                state.note_tiled_size_refusal(surface);
            }
        }
    }

    #[cfg(feature = "screencast")]
    state.refresh_screencast_windows();

    // Push title/app_id changes to the foreign-toplevel handle. Compared
    // before sending so an unrelated commit (every frame, potentially)
    // doesn't emit a spurious `done` event to every bar watching.
    if transition != ToplevelTransition::Unmap && state.foreign_toplevels.contains_key(surface) {
        let (app_id, title) = state.toplevel_identity(surface);
        let pid = state.client_pid(surface);
        let is_xwayland = state.is_xwayland_surface(surface);
        let render_rule = state.config.resolve_window_rules(
            app_id.as_deref(),
            title.as_deref(),
            pid,
            is_xwayland,
        );
        if let Some(opacity) = crate::config::WindowOpacity::from_rule(&render_rule) {
            state.window_opacity.insert(surface.clone(), opacity);
        } else {
            state.window_opacity.remove(surface);
        }
        if let Some(mode) = render_rule.glass {
            state.window_glass_modes.insert(surface.clone(), mode);
        } else {
            state.window_glass_modes.remove(surface);
        }
        let title = title.unwrap_or_default();
        let app_id = app_id.unwrap_or_default();
        let handle = &state.foreign_toplevels[surface];
        if handle.title() != title || handle.app_id() != app_id {
            handle.send_title(&title);
            handle.send_app_id(&app_id);
            handle.send_done();
        }
        // Mirror into the older wlr- protocol -- independent client set.
        if let Some(wlr_handle) = state.wlr_foreign_toplevels.get(surface) {
            if wlr_handle.title() != title || wlr_handle.app_id() != app_id {
                wlr_handle.send_title(&title);
                wlr_handle.send_app_id(&app_id);
            }
        }
    }

    // Only a commit that *started* in the unmapped state can be the empty
    // initial commit. The null-buffer commit which just produced `Unmap`
    // ends the old mapped lifetime; the client must make a subsequent empty
    // commit for the fresh configure handshake, as Niri does. The pinned
    // Smithay revision resets `initial_configure_sent` in its xdg role hook
    // before this handler runs.
    if tracking == ToplevelTracking::Unmapped && !has_buffer {
        let Some(window) = state.unmapped_toplevels.get(surface) else {
            return;
        };
        let toplevel = window.toplevel().unwrap();
        if !toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }

    // Handle popup commits. PopupManager tracks role/tree lifetime but does
    // not distinguish a mapped popup from a role kept alive after committing
    // a null buffer, so mirror the explicit toplevel/layer lifecycle here.
    let popup_was_unmapped = state.unmapped_popup_surfaces.contains(surface);
    state.popups.commit(surface);
    if let Some(popup) = state.popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if popup_was_unmapped && has_buffer {
                    state.unmapped_popup_surfaces.remove(surface);
                } else if !popup_was_unmapped && !has_buffer {
                    state.unmapped_popup_surfaces.insert(surface.clone());
                    let root = find_popup_root_surface(&popup).ok();
                    state.unmap_popup_grab(surface, root.as_ref());
                } else if popup_was_unmapped && !has_buffer && !xdg.is_initial_configure_sent() {
                    if let Err(err) = xdg.send_configure() {
                        // A client can disappear between commit dispatch and
                        // configure emission. That ends this popup lifetime;
                        // it must never take the compositor down with it.
                        tracing::debug!(%err, "Popup vanished before initial configure");
                    }
                }
            }
            PopupKind::InputMethod(ref _input_method) => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToplevelTracking {
    Unknown,
    Unmapped,
    Mapped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToplevelTransition {
    None,
    Map,
    Unmap,
}

fn lifecycle_transition(tracking: ToplevelTracking, has_buffer: bool) -> ToplevelTransition {
    match (tracking, has_buffer) {
        (ToplevelTracking::Unmapped, true) => ToplevelTransition::Map,
        (ToplevelTracking::Mapped, false) => ToplevelTransition::Unmap,
        _ => ToplevelTransition::None,
    }
}

/// Whether `map_toplevel`'s placement/state conversion below is guaranteed
/// to send its own protocol configure, making the earlier tiled-size
/// configure `retile()` sends redundant (and, for a client like a terminal,
/// a visible one-frame re-flow of its content). `rule.maximize`,
/// `rule.fullscreen`, and `rule.pseudo_tile` each end in a call that
/// configures unconditionally (`do_maximize_request`/`do_fullscreen_request`/
/// `toggle_pseudo_tile`'s own `retile()`); an explicit rule position/size
/// guarantees `apply_floating_placement`, which also configures
/// unconditionally. A plain `float`/`pin`/auto-float conversion with no
/// rule-provided position/size is deliberately excluded: `toggle_floating`'s
/// tiled-to-floating branch sends no configure of its own, so skipping the
/// only one that exists would leave the window's stored `FloatingTag::rect`
/// out of sync with whatever size the client's buffer actually settles at.
fn skips_first_tile_configure(rule: &crate::config::WindowRule, implicit_float: bool) -> bool {
    rule.maximize
        || rule.fullscreen
        || rule.pseudo_tile
        || ((rule.float || rule.pin || implicit_float)
            && (rule.position.is_some() || rule.size.is_some()))
}

impl Smallvil {
    /// Finds a protocol-mapped toplevel even when its workspace is hidden
    /// and it is therefore absent from `Space`.
    pub(crate) fn mapped_toplevel_window(&self, surface: &WlSurface) -> Option<Window> {
        self.ocean
            .window(surface)
            .or_else(|| self.layout.window_of(surface))
            .or_else(|| {
                self.floating_workspace
                    .get(surface)
                    .map(|tag| tag.window.clone())
            })
            // A parked (inactive) window-group member is mapped from the
            // client's own point of view exactly like a hidden floating
            // window above -- just not in `Layouts` or `space.elements()`
            // either, since it isn't the tab currently occupying its
            // group's leaf. Same reasoning, same fix.
            .or_else(|| {
                self.group_of(surface).and_then(|idx| {
                    self.groups[idx]
                        .members
                        .iter()
                        .find(|m| &m.surface == surface)
                        .and_then(|m| m.parked_window.clone())
                })
            })
            // Classic Depth Deck entries are protocol-mapped but absent
            // from both Layouts and Space until recalled.
            .or_else(|| {
                self.classic_depth
                    .entry(surface)
                    .map(|entry| entry.window.clone())
            })
            // Keep cleanup robust if older state or an interrupted
            // transition left a visible floating window without its tag.
            .or_else(|| {
                self.space
                    .elements()
                    .find(|window| {
                        window
                            .toplevel()
                            .is_some_and(|toplevel| toplevel.wl_surface() == surface)
                    })
                    .cloned()
            })
    }

    fn toplevel_window(&self, surface: &WlSurface) -> Option<Window> {
        self.unmapped_toplevels
            .get(surface)
            .cloned()
            .or_else(|| self.mapped_toplevel_window(surface))
    }

    /// `(app_id, title)` for a toplevel, straight from the xdg-shell role
    /// state a client sets via `set_app_id`/`set_title`. Shared by
    /// `map_toplevel`'s window-rule matching and `ipc.rs`'s `windows`
    /// query -- the one accessor pattern for both, rather than two
    /// `with_states` call sites drifting apart.
    pub(crate) fn toplevel_identity(
        &self,
        surface: &WlSurface,
    ) -> (Option<String>, Option<String>) {
        smithay::wayland::compositor::with_states(surface, |states| {
            let attrs = states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()?
                .lock()
                .ok()?;
            Some((attrs.app_id.clone(), attrs.title.clone()))
        })
        .unwrap_or((None, None))
    }

    /// Tracks the map/unmap "flutter storm": a client that nulls its
    /// buffer and immediately maps again, over and over. A video player
    /// refusing a tiled size does exactly this -- every refusal re-realizes
    /// its surface, which retiles and sends the tiled configure again,
    /// which the player refuses again, with no bound. Each Map that follows
    /// an Unmap inside `FLUTTER_WINDOW` counts a flip; at
    /// `FLUTTER_FLOPS` the surface is marked `flutter_floated`, and the
    /// next `map_toplevel` floats it instead of tiling -- the one outcome
    /// that ends the storm (the window gets its natural size, nothing to
    /// refuse). One real minimize is a single flip and never trips this.
    fn note_toplevel_flutter(&mut self, surface: &WlSurface, mapping: bool) {
        const FLUTTER_WINDOW: std::time::Duration = std::time::Duration::from_millis(1000);
        const FLUTTER_FLOPS: u32 = 3;
        let now = std::time::Instant::now();
        if mapping {
            let record = self.lifecycle_flutter.entry(surface.clone()).or_default();
            record.flips = match record.last_unmapped_at {
                Some(at) if now.saturating_duration_since(at) <= FLUTTER_WINDOW => record.flips + 1,
                _ => 0,
            };
            let flips = record.flips;
            if flips >= FLUTTER_FLOPS && self.flutter_floated.insert(surface.clone()) {
                let (app_id, title) = self.toplevel_identity(surface);
                tracing::info!(
                    ?app_id,
                    ?title,
                    flips,
                    "flutter storm detected, floating surface instead of tiling"
                );
            }
        } else {
            self.lifecycle_flutter
                .entry(surface.clone())
                .or_default()
                .last_unmapped_at = Some(now);
        }
        // Bound the tracking to windows the compositor still knows about:
        // drop records (and float flags) for surfaces that are neither
        // mapped nor waiting in the unmapped set.
        #[allow(clippy::mutable_key_type)] // WlSurface-keyed tracking set
        let tracked: std::collections::HashSet<WlSurface> = self
            .unmapped_toplevels
            .keys()
            .cloned()
            .chain(
                self.space
                    .elements()
                    .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone())),
            )
            .collect();
        self.lifecycle_flutter.retain(|surface, record| {
            record.flips >= FLUTTER_FLOPS
                || record
                    .last_unmapped_at
                    .is_some_and(|at| now.saturating_duration_since(at) <= FLUTTER_WINDOW)
                || tracked.contains(surface)
        });
        self.flutter_floated.retain(|s| tracked.contains(s));
    }

    /// Detects the other half of the tiling-averse-client class: a client
    /// that *stays mapped* (no map/unmap flip, so `note_toplevel_flutter`
    /// never fires) but commits its buffers at a size different from the
    /// tiled size it was configured with. A video player with a locked
    /// aspect is the canonical case -- it renders at its own aspect inside
    /// the tile slot, hanging half-empty and overlapping nothing cleanly,
    /// forever. Called on every toplevel commit after the map handshake.
    ///
    /// A couple of stale commits right after a retile are normal (the
    /// client is still catching up to the new configure), so a streak only
    /// counts consecutive mismatches that survive a full grace window.
    /// When it does trip, the window is floated immediately (not just on
    /// the next map) and remembered via `flutter_floated` so it is never
    /// tiled again -- the one outcome it has nothing left to refuse.
    fn note_tiled_size_refusal(&mut self, surface: &WlSurface) {
        const REFUSAL_LIMIT: u32 = 3;
        const REFUSAL_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
        const SIZE_TOLERANCE: i32 = 8;

        // Only tiled windows promise an expected size, and only ones
        // actually in the layout right now can be floated out of it.
        let Some(expected) = self.expected_tiled_size.get(surface).copied() else {
            return;
        };
        if !self.layout.contains(surface) || self.fullscreen.contains_key(surface) {
            self.tiled_size_refusals.remove(surface);
            return;
        }
        // An active pointer grab (tile resize/drag, move) legitimately
        // lags commits behind the latest configure. Never count those.
        if self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
        {
            return;
        }
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        let actual = window.geometry().size;
        let matches = (actual.w - expected.w).abs() <= SIZE_TOLERANCE
            && (actual.h - expected.h).abs() <= SIZE_TOLERANCE;
        if matches {
            self.tiled_size_refusals.remove(surface);
            return;
        }

        let now = std::time::Instant::now();
        let record = self.tiled_size_refusals.entry(surface.clone()).or_default();
        if record.first_refusal_at.is_none() {
            record.first_refusal_at = Some(now);
        }
        record.refusals += 1;
        let streak_age = record
            .first_refusal_at
            .map(|at| now.saturating_duration_since(at))
            .unwrap_or_default();
        if record.refusals >= REFUSAL_LIMIT && streak_age >= REFUSAL_WINDOW {
            if self.flutter_floated.insert(surface.clone()) {
                let (app_id, title) = self.toplevel_identity(surface);
                tracing::info!(
                    ?app_id,
                    ?title,
                    expected = ?expected,
                    actual = ?actual,
                    "tiled size refused persistently, floating surface instead of tiling"
                );
            }
            self.tiled_size_refusals.remove(surface);
            self.toggle_floating(surface);
            self.request_redraw();
        }
    }

    fn map_toplevel(&mut self, surface: &WlSurface) {
        let (app_id, title) = self.toplevel_identity(surface);
        let pid = self.client_pid(surface);
        let is_xwayland = self.is_xwayland_surface(surface);
        let rule =
            self.config
                .resolve_window_rules(app_id.as_deref(), title.as_deref(), pid, is_xwayland);
        if let Some(opacity) = crate::config::WindowOpacity::from_rule(&rule) {
            self.window_opacity.insert(surface.clone(), opacity);
        } else {
            self.window_opacity.remove(surface);
        }
        if let Some(mode) = rule.glass {
            self.window_glass_modes.insert(surface.clone(), mode);
        } else {
            self.window_glass_modes.remove(surface);
        }

        let output = self
            .fullscreen
            .get(surface)
            .and_then(|entry| self.output_by_name(&entry.output))
            .or_else(|| {
                rule.output
                    .as_deref()
                    .and_then(|name| self.output_by_name(name))
            })
            .or_else(|| self.primary_output());
        let Some(output) = output else {
            tracing::warn!("No output available for mapped toplevel, closing it");
            if let Some(window) = self.unmapped_toplevels.get(surface) {
                window.toplevel().unwrap().send_close();
            }
            return;
        };
        // Auto-float heuristic (Phase N tier 1): a dialog with a parent, or
        // a window with a dimension pinned by min/max size (a splash
        // screen, a fixed-size utility panel, a small bootstrapper), tiling
        // by default is a bad first-run experience. Matches sway's
        // `wants_floating` exactly: either dimension fixed (with both min
        // dimensions nonzero) floats, since a BSP layout varies both
        // dimensions and a pinned one can't take its slot. niri is more
        // conservative (fixed *height* only); the OR version is the
        // defensible match for BSP. `rule.tile` is the one escape hatch
        // back to tiled per app, checked here rather than baked into the
        // heuristic itself.
        let implicit_float = !rule.tile
            && self.unmapped_toplevels.get(surface).is_some_and(|window| {
                let has_parent = window.toplevel().is_some_and(|t| t.parent().is_some());
                let is_pinned_dimension =
                    smithay::wayland::compositor::with_states(surface, |states| {
                        let mut guard = states
                            .cached_state
                            .get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
                        let data = guard.current();
                        is_dimension_pinned(data.min_size, data.max_size)
                    });
                has_parent || is_pinned_dimension
            });
        // A surface caught in a map/unmap flutter storm is floated from now
        // on: tiling it is exactly what the client keeps refusing.
        let flutter_float = self.flutter_floated.contains(surface);

        let Some(window) = self.unmapped_toplevels.remove(surface) else {
            return;
        };
        self.window_depths
            .insert(surface.clone(), crate::depth::WindowDepth::new());
        let focused = self.intended_window_surface();
        let ocean_engine = self.config.spatial_engine == crate::config::SpatialEngine::Ocean;
        let workspace = rule
            .workspace
            .unwrap_or_else(|| self.layout.active_workspace(&output.name()));
        // Window swallowing (`swallow = true` window rule): a tiled window
        // whose process spawned this one gets replaced by it. Inserting
        // targeted at the swallower splits its tile; detaching the
        // swallower right after collapses the split, leaving this window
        // exactly in its place. Skipped when the new window is about to
        // float -- hiding the terminal under a floating child would leave
        // an empty tile behind.
        let swallow_target =
            if ocean_engine || rule.float || rule.pin || implicit_float || flutter_float {
                None
            } else {
                self.swallow_candidate(surface, &output.name(), workspace)
            };
        if ocean_engine {
            let viewport = self
                .space
                .output_geometry(&output)
                .map(|geometry| geometry.size)
                .unwrap_or_else(|| Size::from((1, 1)));
            self.ocean
                .insert(&output.name(), viewport, window, focused.as_ref());
            // Once per window's lifetime, here rather than inside
            // `OceanSpace::insert` itself -- that's also called on every
            // floating<->tiled reattach, which must never reorder or
            // duplicate an app-slot entry for a window that isn't new.
            self.ocean.record_app_opened(surface.clone());
        } else {
            self.layout.insert(
                &output.name(),
                workspace,
                window,
                swallow_target.as_ref().or(focused.as_ref()),
            );
        }
        // `toggle_floating`/`toggle_pseudo_tile` below look the window up
        // via `self.space.elements()`, which `layout.insert` alone does not
        // populate -- only `retile()`'s own `space.map_element` call does.
        // This `retile()` is pure bookkeeping (position tracking), not a
        // render or a present, so applying the rule's placement after it
        // is still invisible to the client: nothing is actually drawn
        // until the backend's own render loop runs, strictly after this
        // function returns. But `retile()` also unconditionally sends this
        // window a tiled-size *protocol* configure -- if a conversion below
        // is guaranteed to resize it again right after, the client (a
        // terminal, say) receives both in quick succession and visibly
        // re-flows its content twice. See `skips_first_tile_configure`'s own
        // doc comment for which conversions actually guarantee that second
        // configure. A flutter-floated window guarantees the same second
        // (floating) configure, so it skips the tiled one too.
        if skips_first_tile_configure(&rule, implicit_float) || flutter_float {
            self.retile_skip_first_configure(surface);
        } else {
            self.retile();
        }

        if let Some(swallower) = swallow_target {
            if let Some(hidden) = self.detach_mapped_toplevel(&swallower) {
                self.swallowed.insert(
                    surface.clone(),
                    crate::state::SwallowedWindow {
                        surface: swallower,
                        window: hidden,
                    },
                );
                self.retile();
            }
        }

        // Tiling it first and immediately converting here (reusing the
        // exact same logic the interactive toggles use, rather than
        // re-deriving floating-rect placement from scratch) is, for the
        // same reason, still applied before the window's first real frame.
        if rule.float || rule.pin || rule.maximize || implicit_float || flutter_float {
            self.toggle_floating(surface);
            if rule.pin {
                if ocean_engine {
                    self.ocean.pin_to_screen(surface, &output.name());
                }
                self.pinned.insert(surface.clone());
            }
            if rule.position.is_some() || rule.size.is_some() {
                self.apply_floating_placement(surface, rule.position, rule.size);
                if rule.pin && ocean_engine {
                    self.ocean.unpin_from_screen(surface);
                    self.ocean.pin_to_screen(surface, &output.name());
                }
            }
        } else if rule.pseudo_tile {
            self.toggle_pseudo_tile(surface);
        }

        if let Some(toplevel) = self
            .mapped_toplevel_window(surface)
            .and_then(|window| window.toplevel().cloned())
        {
            if rule.maximize {
                self.do_maximize_request(toplevel.clone());
            }
            if rule.fullscreen {
                self.do_fullscreen_request(toplevel, None);
            }
        }

        // Role creation alone must never steal focus. A real first buffer
        // does. An Exclusive layer can temporarily own the actual keyboard,
        // while centralized focus retains this window as the restore target.
        // `rule.no_focus` (Phase N tier 2) is the one exception: leaves
        // whatever was focused before completely untouched, rather than
        // picking some other window to focus instead.
        if !rule.no_focus {
            let focus_on_map = self
                .config
                .resolve_ripple_config(rule.ripple.as_ref(), crate::config::RippleTrigger::Focus)
                .focus_on_map
                .unwrap_or(false);
            // Mapping and its automatic focus are one lifecycle transaction.
            // Let the map ripple be its single visual cue; ordinary focus
            // handoffs after this still animate through `focus_window`.
            // `focus_on_map = true` deliberately restores effect stacking.
            self.focus_window_on_map(
                Some(surface.clone()),
                SERIAL_COUNTER.next_serial(),
                focus_on_map,
            );
        }

        // Logical placement/focus above is already final. The open animation
        // only offsets/fades the first rendered frames toward that state.
        self.start_window_open_animation(surface);

        // Droplet-impact ripple at the window's center -- Phase R1, see
        // `ripple.rs`. After the placement/retile/focus block above so the
        // window's `space.element_location` reflects its final spot,
        // including any floating-rule conversion. No-op when `water_effects`
        // is off; see `spawn_window_map_ripple`'s own doc.
        self.spawn_window_map_ripple(surface);

        // Cosmetic float-physics disturbance (F1 `light`, spatial roadmap):
        // a newly mapped floating window "lands in the water" with a
        // downward-biased kick, and any floating neighbors within
        // `float_physics.radius` rock too. Fixed synthetic magnitude --
        // there's no real motion to sample the way a drag has, so this is
        // a starting point, open to the same feel-tuning pass as every
        // other `float_physics` default. `float_physics_kick_near` itself
        // is the no-op gate when the mechanic or this window's rule
        // disables it.
        const MAP_KICK_IMPULSE: f64 = 120.0;
        if let Some(center) = self.window_center_for_kick(surface) {
            if let Some(window) = self.mapped_toplevel_window(surface) {
                if let Some(output) = self.output_for_window(&window) {
                    self.float_physics_kick_near(&output, center, (0.0, MAP_KICK_IMPULSE));
                }
            }
        }

        self.announce_foreign_toplevel(surface);
    }

    /// Announces `surface` to both foreign-toplevel protocols (the newer
    /// read-only ext- list and the older bidirectional wlr- one). Split
    /// out of `map_toplevel` so a swallow restore -- which re-shows a
    /// window without going through the map path -- looks like a fresh
    /// map to bars, matching the handle retirement its hide did in
    /// `detach_mapped_toplevel`.
    fn announce_foreign_toplevel(&mut self, surface: &WlSurface) {
        let (app_id, title) = self.toplevel_identity(surface);
        let handle = self.foreign_toplevel_list_state.new_toplevel::<Self>(
            title.clone().unwrap_or_default(),
            app_id.clone().unwrap_or_default(),
        );
        self.foreign_toplevels.insert(surface.clone(), handle);
        let numeric_id = self.next_foreign_toplevel_numeric_id;
        self.next_foreign_toplevel_numeric_id =
            self.next_foreign_toplevel_numeric_id.saturating_add(1);
        self.foreign_toplevel_numeric_ids
            .insert(surface.clone(), numeric_id);

        // Mirror the lifecycle into the older wlr-foreign-toplevel-management-v1
        // protocol (waybar's `wlr/taskbar`, ags v1). Independent state machine
        // from the newer ext- protocol above; both can be bound simultaneously.
        if let Some(wlr_state) = self.wlr_foreign_toplevel_state.as_mut() {
            let wlr_handle = wlr_state.track(title.unwrap_or_default(), app_id.unwrap_or_default());
            self.wlr_foreign_toplevels
                .insert(surface.clone(), wlr_handle);
            // On a fresh map, `focus_window` already ran and, per this
            // codebase's established "role creation alone must never steal
            // focus, a real first buffer does" rule, a freshly-mapped
            // window is typically activated by this point -- but that
            // activation happened before this handle existed to receive
            // it, so `init_instance`'s empty initial `state` array is
            // stale from the moment it's sent. Push the real state now
            // that the handle is actually registered.
            self.refresh_wlr_toplevel_state(surface);
        }

        // WindowOpened fires here, the single announcement chokepoint, so
        // the swallow-restore path (which re-shows a window without going
        // through `map_toplevel`) looks like a fresh open to subscribers
        // too -- matching the bar-facing semantics of the foreign-toplevel
        // announcement that runs immediately above. The window's
        // numeric_id was just assigned, so the snapshot carries it.
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowOpened {
            surface: surface.clone(),
        });
    }

    fn unmap_toplevel(&mut self, surface: &WlSurface) {
        self.start_window_close_animation(surface);
        self.restore_swallowed(surface);
        resize_grab::cancel(surface);
        let preferred_output = self.preferred_output_for_toplevel(surface);
        let Some(window) = self.detach_mapped_toplevel(surface) else {
            self.closing_window_animations
                .retain(|closing| closing.surface != *surface);
            return;
        };
        self.forget_window_focus(surface);

        // An xdg unmap starts a fresh role lifecycle. Do not leak runtime
        // state such as fullscreen/maximized into its next initial
        // configure unless the client requests it again.
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.states.unset(xdg_toplevel::State::Maximized);
                state.states.unset(xdg_toplevel::State::Resizing);
                state.states.unset(xdg_toplevel::State::Activated);
                state.size = None;
            });
        }
        self.unmapped_toplevels.insert(surface.clone(), window);
        self.retile();
        self.repair_keyboard_focus(preferred_output.as_deref(), SERIAL_COUNTER.next_serial());
    }

    /// Removes every piece of runtime mapped-state and returns the window
    /// handle so an xdg unmap can retain it for a later remap. This is also
    /// used by permanent role destruction, which simply drops the result.
    fn detach_mapped_toplevel(&mut self, surface: &WlSurface) -> Option<Window> {
        // Capture identity before any cleanup drains the per-window maps,
        // so the WindowClosed event carries a useful payload (window_id +
        // app_id + title) rather than None/None. The event itself is
        // emitted at the end of the function so the desktop is already
        // consistent by the time a subscriber receives it.
        let closed_window_id = self.foreign_toplevel_numeric_ids.get(surface).copied();
        let (closed_app_id, closed_title) = self.toplevel_identity(surface);
        // A swallower dying while hidden must be forgotten, or a later
        // child close would re-insert a dead window's handle. (At swallow
        // time this is a no-op: the entry is recorded only after this
        // function returns.)
        self.swallowed.retain(|_, entry| entry.surface != *surface);
        let window = self.mapped_toplevel_window(surface);
        if let Some(window) = &window {
            self.space.unmap_elem(window);
        }
        // Before the unconditional tree removal below: if `surface` is a
        // window-group's *active* member, this promotes the next tab into
        // its leaf (or dissolves the group) so the leaf doesn't collapse
        // the way an ordinary tile's close would. By the time
        // `self.layout.remove(surface)` runs, `surface` no longer occupies
        // any leaf either way, making that call a no-op for it specifically
        // (find-nothing, same as any other surface that isn't tiled).
        self.leave_group_on_close(surface);
        if self.classic_depth.remove(surface).is_some() {
            self.depth_deck_overlay = None;
            self.classic_depth.close();
        }
        self.layout.remove(surface);
        self.ocean.remove(surface);
        self.fullscreen.remove(surface);
        self.maximized.remove(surface);
        self.floating_workspace.remove(surface);
        self.pinned.remove(surface);
        self.pseudo_tiled.remove(surface);
        self.urgent.remove(surface);
        self.window_opacity.remove(surface);
        self.window_glass_modes.remove(surface);
        self.window_open_animations.remove(surface);
        self.window_move_animations.remove(surface);
        self.window_viscosity.remove(surface);
        self.window_sway.remove(surface);
        self.window_float_physics.remove(surface);
        self.window_float_ambient.remove(surface);
        self.window_float_bodies.remove(surface);
        self.window_frame_snapshots.remove(surface);
        self.backdrop_textures.remove(surface);
        self.glass_anim.remove(surface);
        self.window_depths.remove(surface);
        self.depth_schematics.remove(surface);
        self.expected_tiled_size.remove(surface);
        self.tiled_size_refusals.remove(surface);
        self.focus_history.retain(|s| s != surface);
        // Closing the foreign-toplevel handle here (rather than only on
        // role destruction) means an xdg unmap also retires it; a later
        // remap announces a fresh handle from `map_toplevel`, which is what
        // bars expect -- an unmapped window is gone from the list.
        if let Some(handle) = self.foreign_toplevels.remove(surface) {
            self.foreign_toplevel_list_state.remove_toplevel(&handle);
        }
        self.foreign_toplevel_numeric_ids.remove(surface);
        // Mirror into the older wlr- protocol.
        if let Some(handle) = self.wlr_foreign_toplevels.remove(surface) {
            if let Some(wlr_state) = self.wlr_foreign_toplevel_state.as_mut() {
                wlr_state.untrack(&handle);
            }
        }
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowClosed {
            window_id: closed_window_id,
            app_id: closed_app_id,
            title: closed_title,
        });
        window
    }

    /// If `surface` (a closing or unmapping window) swallowed another
    /// window, puts it back: re-inserted targeted at `surface`'s own slot
    /// while `surface` is still in the tree, so the teardown that follows
    /// collapses the split and leaves the restored window exactly where
    /// the pair sat. Must run before `detach_mapped_toplevel(surface)`;
    /// the caller's usual retile + focus repair then remaps it.
    fn restore_swallowed(&mut self, surface: &WlSurface) {
        let Some(entry) = self.swallowed.remove(surface) else {
            return;
        };
        let (output, workspace) = match (
            self.layout.output_of(surface).map(str::to_string),
            self.layout.workspace_of(surface),
        ) {
            (Some(output), Some(workspace)) => (output, workspace),
            // The child left the tiled tree since the swallow (floated or
            // fullscreened into a different shape of teardown) -- restore
            // somewhere sensible instead of guessing its old slot.
            _ => {
                let Some(output) = self.primary_output() else {
                    return;
                };
                let name = output.name();
                let workspace = self.layout.active_workspace(&name);
                (name, workspace)
            }
        };
        self.layout
            .insert(&output, workspace, entry.window, Some(surface));
        // Re-apply identity-derived rule state and re-announce to bars --
        // `detach_mapped_toplevel` cleared both when the window was hidden.
        let (app_id, title) = self.toplevel_identity(&entry.surface);
        let pid = self.client_pid(&entry.surface);
        let is_xwayland = self.is_xwayland_surface(&entry.surface);
        let rule =
            self.config
                .resolve_window_rules(app_id.as_deref(), title.as_deref(), pid, is_xwayland);
        if let Some(opacity) = crate::config::WindowOpacity::from_rule(&rule) {
            self.window_opacity.insert(entry.surface.clone(), opacity);
        }
        if let Some(mode) = rule.glass {
            self.window_glass_modes.insert(entry.surface.clone(), mode);
        }
        self.window_depths
            .insert(entry.surface.clone(), crate::depth::WindowDepth::new());
        self.announce_foreign_toplevel(&entry.surface);
        // Hand focus back to the restored window if the closing child had
        // it, through the normal focus authority -- which requires the
        // window to be visible first, hence the retile (pure `Space`
        // bookkeeping; the caller's own later retile is idempotent).
        if self.focused_window_surface().as_ref() == Some(surface) {
            self.retile();
            self.focus_window(Some(entry.surface), SERIAL_COUNTER.next_serial());
        }
    }

    /// The visible tiled window on (`output`, `workspace`) that `child`
    /// should swallow, if any: it must match a `swallow = true` window
    /// rule and its client process must be an ancestor of `child`'s (the
    /// terminal this app was launched from). Tiled windows only -- a
    /// floating terminal spawning a viewer keeps both visible.
    fn swallow_candidate(
        &self,
        child: &WlSurface,
        output: &str,
        workspace: u32,
    ) -> Option<WlSurface> {
        // Almost every config has no swallow rule; don't touch /proc then.
        if !self.config.window_rules.iter().any(|rule| rule.swallow) {
            return None;
        }
        let child_pid = self.client_pid(child)?;
        let ancestors = ancestor_pids(child_pid);
        self.layout
            .windows_in(output, workspace)
            .into_iter()
            .find_map(|window| {
                let surface = window.toplevel()?.wl_surface().clone();
                if surface == *child {
                    return None;
                }
                let pid = self.client_pid(&surface)?;
                // Strict ancestor: a client opening a second window of its
                // own process (pid == child_pid) isn't a spawned child.
                if pid == child_pid || !ancestors.contains(&pid) {
                    return None;
                }
                let (app_id, title) = self.toplevel_identity(&surface);
                let is_xwayland = self.is_xwayland_surface(&surface);
                self.config
                    .resolve_window_rules(
                        app_id.as_deref(),
                        title.as_deref(),
                        Some(pid),
                        is_xwayland,
                    )
                    .swallow
                    .then_some(surface)
            })
    }

    /// The process ID on the other end of `surface`'s client socket
    /// (`SO_PEERCRED`), or `None` for a dead client.
    pub(crate) fn client_pid(&self, surface: &WlSurface) -> Option<i32> {
        surface
            .client()?
            .get_credentials(&self.display_handle)
            .ok()
            .map(|credentials| credentials.pid)
    }

    /// Whether `surface` belongs to `xwayland-satellite` rather than a
    /// native Wayland client -- every X11 application arrives at TideWM as
    /// one of satellite's own Wayland surfaces (see the `xwayland` module
    /// docs), so a matching client PID is the only way to tell. `false`
    /// whenever xwayland is disabled, satellite never started, or the
    /// surface's client is already gone.
    pub(crate) fn is_xwayland_surface(&self, surface: &WlSurface) -> bool {
        self.xwayland_satellite_pid.is_some()
            && self.client_pid(surface) == self.xwayland_satellite_pid
    }

    pub(crate) fn preferred_output_for_toplevel(&self, surface: &WlSurface) -> Option<String> {
        self.ocean
            .entry_output(surface)
            .map(str::to_string)
            .or_else(|| self.layout.output_of(surface).map(str::to_string))
            .or_else(|| {
                self.floating_workspace
                    .get(surface)
                    .map(|tag| tag.output.clone())
            })
            .or_else(|| {
                self.classic_depth
                    .entry(surface)
                    .map(|entry| entry.output.clone())
            })
            .or_else(|| {
                self.mapped_toplevel_window(surface)
                    .and_then(|window| self.output_for_window(&window))
                    .map(|output| output.name())
            })
    }

    /// Root can be either a window's toplevel surface or a layer surface's
    /// own surface (wlr-layer-shell popups, e.g. a bar's dropdown menu, are
    /// still plain xdg_popups underneath, just parented to a layer instead
    /// of a toplevel) -- try both.
    pub(crate) fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };

        // Resolve both the output the root actually lives on and its
        // geometry in the same pass: a window's output comes from where
        // it's mapped in `space`, a layer surface's from whichever
        // output's `LayerMap` actually holds it (there's no space-element
        // lookup for those).
        let (output, root_geo) = if let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &root)
        {
            let Some(output) = self.output_for_window(window) else {
                return;
            };
            let Some(root_geo) = self.space.element_geometry(window) else {
                return;
            };
            (output, root_geo)
        } else {
            let found = self.space.outputs().find_map(|output| {
                let map = layer_map_for_output(output);
                let layer = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
                let mut geo = map.layer_geometry(layer)?;
                // LayerMap geometry is output-local, while Space output and
                // window geometry are global. Normalize before computing the
                // positioner's parent-relative constraint box.
                geo.loc += self.space.output_geometry(output)?.loc;
                Some((output.clone(), geo))
            });
            let Some(found) = found else {
                return;
            };
            found
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= root_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    /// Repositions reactive popups after their toplevel parent's geometry
    /// changes. This follows the xdg-positioner contract: a reactive popup
    /// tracks parent movement/resize without waiting for the client to issue
    /// an explicit reposition request.
    pub(crate) fn update_reactive_popups(&self, window: &Window) {
        let root = window.toplevel().unwrap().wl_surface();
        for (popup, _) in PopupManager::popups_for_surface(root) {
            let PopupKind::Xdg(popup) = popup else {
                continue;
            };
            if !popup.with_pending_state(|state| state.positioner.reactive) {
                continue;
            }

            self.unconstrain_popup(&popup);
            if let Err(err) = popup.send_pending_configure() {
                tracing::warn!(?err, "Failed to reconfigure reactive popup");
            }
        }
    }

    /// Clears `surface`'s fullscreen protocol state and bookkeeping. If it
    /// is currently floating, restores its exact pre-fullscreen rect; a
    /// still-tiled window needs no restore call here since it has an intact
    /// `Layouts` slot. `restore_rect` can intentionally remain populated
    /// while tiled so a floating -> tiled -> floating round trip during
    /// fullscreen does not forget the original floating rect. Shared by
    /// `unfullscreen_request` and `fullscreen_request`'s own pre-emption.
    pub(crate) fn do_unfullscreen(&mut self, surface: &ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        let entry = self.fullscreen.remove(wl_surface);

        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Fullscreen);
        });
        let Some(entry) = entry else {
            Self::send_forced_configure(surface);
            return;
        };

        let restore_rect = entry
            .restore_rect
            .or_else(|| self.floating_workspace.get(wl_surface).map(|tag| tag.rect))
            .or_else(|| self.ocean.floating_rect(wl_surface));
        let maximized_rect = self.maximized.get(wl_surface).and_then(|maximized| {
            let output = self.output_by_name(&maximized.output)?;
            let area = self.output_tiling_area(&output)?;
            let workspace = self.layout.active_workspace(&maximized.output);
            Some(crate::layout::inset(
                area,
                self.gaps_for(&maximized.output, workspace),
            ))
        });
        let is_floating = !self.layout.contains(wl_surface) && !self.ocean.is_tiled(wl_surface);
        if !is_floating {
            self.maximized.remove(wl_surface);
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Maximized);
                state.size = self
                    .tiled_rect_for_surface(wl_surface)
                    .map(|rect| rect.size);
            });
        }
        if is_floating {
            surface.with_pending_state(|state| {
                if let Some(rect) = maximized_rect {
                    state.states.set(xdg_toplevel::State::Maximized);
                    state.size = Some(rect.size);
                } else {
                    state.states.unset(xdg_toplevel::State::Maximized);
                    state.size = restore_rect.map(|rect| rect.size);
                }
            });
            if let (Some(tag), Some(rect)) =
                (self.floating_workspace.get_mut(wl_surface), restore_rect)
            {
                tag.rect = rect;
            }
            if let Some(rect) = restore_rect {
                self.ocean.set_floating_rect(wl_surface, rect);
            }
        }

        Self::send_forced_configure(surface);

        if is_floating {
            let target_rect = maximized_rect.or(restore_rect);
            let visible_window = self
                .space
                .elements()
                .find(|w| w.toplevel().unwrap().wl_surface() == wl_surface)
                .cloned();
            let window = visible_window.or_else(|| {
                entry
                    .was_pinned
                    .then(|| {
                        self.floating_workspace
                            .get(wl_surface)
                            .map(|tag| tag.window.clone())
                    })
                    .flatten()
            });
            if let (Some(window), Some(rect)) = (window, target_rect) {
                self.space.map_element(window, rect.loc, false);
            }
            if entry.was_pinned {
                self.pinned.insert(wl_surface.clone());
                self.ocean.pin_to_screen(wl_surface, &entry.output);
            }
        }

        self.retile();
        self.refresh_wlr_toplevel_state(wl_surface);
    }

    /// Broadcasts current activated/maximized/fullscreen state to every
    /// wlr-foreign-toplevel-management-v1 client tracking `surface`, if any
    /// is. `self.maximized`/`self.fullscreen` are read fresh here rather
    /// than threaded through as arguments, so this stays correct regardless
    /// of which internal branch of `do_maximize_request`/
    /// `do_fullscreen_request`/`do_unfullscreen` actually changed them --
    /// callers just call this once after the fact instead of reasoning
    /// about which of several early-return paths needs it.
    pub(crate) fn refresh_wlr_toplevel_state(&mut self, surface: &WlSurface) {
        let Some(handle) = self.wlr_foreign_toplevels.get(surface) else {
            return;
        };
        let activated = self.is_window_activated(surface);
        let fullscreen = self.fullscreen.contains_key(surface);
        // `self.maximized` is retained as restore-intent while fullscreen
        // is active (the documented invariant `assert_state_invariants`
        // checks in state.rs -- fullscreen and maximized are never both
        // reported, but a maximized entry can outlive its window going
        // fullscreen), so a bare `contains_key` would report both bits set
        // at once to every bound client -- mask it the same way ipc.rs's
        // `window_json` already does (`maximized && !fullscreen`).
        let maximized = self.maximized.contains_key(surface) && !fullscreen;
        handle.send_state(crate::handlers::wlr_foreign_toplevel::state_bytes(
            activated, maximized, fullscreen,
        ));
    }

    /// XDG state-change requests require a configure response even when the
    /// request is duplicate or denied. `send_pending_configure` intentionally
    /// emits nothing when state is unchanged, so force a plain configure in
    /// that case. Before the initial handshake, the pending state is folded
    /// into the one initial configure instead.
    fn send_forced_configure(surface: &ToplevelSurface) {
        if surface.is_initial_configure_sent() && surface.send_pending_configure().is_none() {
            surface.send_configure();
        }
    }

    /// `Super+F`: toggles fullscreen on the focused window, for apps that
    /// never request it themselves and for manually fullscreening anything.
    /// Drives the exact same path a client's own `xdg_toplevel` request
    /// would (`fullscreen_request`/`do_unfullscreen`), just triggered from a
    /// keybind instead of a protocol request.
    pub(crate) fn toggle_fullscreen(&mut self) {
        // Don't let a keybind escape an exclusive-interactivity layer (e.g.
        // a lock screen) while it's still mapped, same guard `cycle_focus`
        // already uses.
        if self.exclusive_layer().is_some() {
            return;
        }
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &surface))
            .cloned();
        let Some(window) = window else { return };
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };

        if self.fullscreen.contains_key(&surface) {
            self.do_unfullscreen(&toplevel);
        } else {
            self.fullscreen_request(toplevel, None);
        }
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged { surface });
    }

    /// Keybind path to the same maximize/restore geometry a client's own
    /// xdg-shell request or a window rule already produces -- see
    /// `do_maximize_request`'s own doc comment for why it's a no-op for an
    /// already-tiled window. Meant for Ocean, where a floating window has
    /// no output bounds to snap back against otherwise.
    pub(crate) fn toggle_maximize(&mut self) {
        if self.exclusive_layer().is_some() {
            return;
        }
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &surface))
            .cloned();
        let Some(window) = window else { return };
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };

        if self.maximized.contains_key(&surface) {
            self.do_unmaximize_request(toplevel);
        } else {
            self.do_maximize_request(toplevel);
        }
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged { surface });
    }
}

/// `pid`'s ancestor process chain (nearest first) from `/proc`, stopping
/// at init. Capped well past any realistic terminal -> shell -> app
/// nesting depth, in case of a PPid cycle in a racing/dying process tree.
fn ancestor_pids(pid: i32) -> Vec<i32> {
    let mut ancestors = Vec::new();
    let mut current = pid;
    for _ in 0..16 {
        let Some(ppid) = std::fs::read_to_string(format!("/proc/{current}/status"))
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("PPid:"))
                    .and_then(|value| value.trim().parse::<i32>().ok())
            })
        else {
            break;
        };
        if ppid <= 1 {
            break;
        }
        ancestors.push(ppid);
        current = ppid;
    }
    ancestors
}

/// Whether a toplevel's min/max size pins at least one dimension, sway's
/// `wants_floating` rule: both min dimensions nonzero, and either the
/// width or the height range collapses to a single value. Such a window
/// cannot take an arbitrary BSP slot (which varies both dimensions), so
/// it is floated at map time. A fully unpinned window (both min
/// dimensions zero, the usual case for terminals and browsers) is false.
fn is_dimension_pinned(
    min_size: smithay::utils::Size<i32, smithay::utils::Logical>,
    max_size: smithay::utils::Size<i32, smithay::utils::Logical>,
) -> bool {
    min_size.w > 0 && min_size.h > 0 && (min_size.w == max_size.w || min_size.h == max_size.h)
}

#[cfg(test)]
mod tests {
    use super::{
        is_dimension_pinned, lifecycle_transition, skips_first_tile_configure, ToplevelTracking,
        ToplevelTransition,
    };

    #[test]
    fn role_without_buffer_stays_unmapped() {
        assert_eq!(
            lifecycle_transition(ToplevelTracking::Unmapped, false),
            ToplevelTransition::None
        );
    }

    #[test]
    fn first_buffer_maps_and_repeated_buffer_does_not_map_twice() {
        assert_eq!(
            lifecycle_transition(ToplevelTracking::Unmapped, true),
            ToplevelTransition::Map
        );
        assert_eq!(
            lifecycle_transition(ToplevelTracking::Mapped, true),
            ToplevelTransition::None
        );
    }

    #[test]
    fn null_buffer_unmaps_and_a_later_buffer_remaps() {
        assert_eq!(
            lifecycle_transition(ToplevelTracking::Mapped, false),
            ToplevelTransition::Unmap
        );
        assert_eq!(
            lifecycle_transition(ToplevelTracking::Unmapped, false),
            ToplevelTransition::None
        );
        assert_eq!(
            lifecycle_transition(ToplevelTracking::Unmapped, true),
            ToplevelTransition::Map
        );
    }

    #[test]
    fn plain_float_conversion_without_a_size_rule_keeps_the_tile_configure() {
        // toggle_floating's tiled-to-floating branch sends no configure of
        // its own for these, so the first (tiled) configure must not be
        // withheld -- otherwise the client's buffer size and the stored
        // FloatingTag::rect would disagree forever.
        let mut rule = crate::config::WindowRule::default();
        assert!(!skips_first_tile_configure(&rule, false));
        rule.float = true;
        assert!(!skips_first_tile_configure(&rule, false));
        rule.float = false;
        rule.pin = true;
        assert!(!skips_first_tile_configure(&rule, false));
        rule.pin = false;
        assert!(!skips_first_tile_configure(&rule, true)); // implicit_float alone
    }

    #[test]
    fn conversions_with_a_guaranteed_second_configure_skip_the_first() {
        use crate::config::WindowRule;

        let maximize = WindowRule {
            maximize: true,
            ..Default::default()
        };
        assert!(skips_first_tile_configure(&maximize, false));

        let fullscreen = WindowRule {
            fullscreen: true,
            ..Default::default()
        };
        assert!(skips_first_tile_configure(&fullscreen, false));

        let pseudo_tile = WindowRule {
            pseudo_tile: true,
            ..Default::default()
        };
        assert!(skips_first_tile_configure(&pseudo_tile, false));

        // A plain float/pin/implicit-float conversion only skips once
        // apply_floating_placement is guaranteed too, i.e. a rule-provided
        // position or size is set.
        let sized_float = WindowRule {
            float: true,
            size: Some((640, 480)),
            ..Default::default()
        };
        assert!(skips_first_tile_configure(&sized_float, false));

        let positioned_pin = WindowRule {
            pin: true,
            position: Some((0, 0)),
            ..Default::default()
        };
        assert!(skips_first_tile_configure(&positioned_pin, false));

        let positioned = WindowRule {
            position: Some((10, 10)),
            ..Default::default()
        };
        assert!(skips_first_tile_configure(&positioned, true)); // implicit_float + explicit position
    }

    #[test]
    fn pinned_dimension_matches_sways_wants_floating() {
        use smithay::utils::Size;

        // Fully unpinned: both min dims zero -- the normal case for a
        // terminal or browser, stays tiled.
        assert!(!is_dimension_pinned(Size::from((0, 0)), Size::from((0, 0))));
        // Resizable both ways (min smaller than max): stays tiled.
        assert!(!is_dimension_pinned(
            Size::from((200, 200)),
            Size::from((1600, 1200))
        ));
        // Fixed in one dimension only: floats (a small bootstrapper, an
        // OBS-style picker, a splash screen).
        assert!(is_dimension_pinned(
            Size::from((400, 300)),
            Size::from((400, 300))
        ));
        assert!(is_dimension_pinned(
            Size::from((400, 300)),
            Size::from((400, 800))
        ));
        assert!(is_dimension_pinned(
            Size::from((400, 300)),
            Size::from((900, 300))
        ));
        // Zero min in one dimension: sway requires both min dims nonzero,
        // so a "fixed width, unbounded height" hint without a height min
        // does not float.
        assert!(!is_dimension_pinned(
            Size::from((400, 0)),
            Size::from((400, 800))
        ));
    }

    #[test]
    fn ancestor_pids_walks_up_without_including_self() {
        // The test process really has ancestors (the cargo test runner at
        // minimum), and the walk must never report the process itself.
        let own_pid = std::process::id() as i32;
        let ancestors = super::ancestor_pids(own_pid);
        assert!(!ancestors.is_empty());
        assert!(!ancestors.contains(&own_pid));
        // A PID that can't exist yields an empty chain, not a panic.
        assert!(super::ancestor_pids(-1).is_empty());
    }

    #[test]
    fn unrelated_surface_never_enters_toplevel_lifecycle() {
        assert_eq!(
            lifecycle_transition(ToplevelTracking::Unknown, false),
            ToplevelTransition::None
        );
        assert_eq!(
            lifecycle_transition(ToplevelTracking::Unknown, true),
            ToplevelTransition::None
        );
    }
}
