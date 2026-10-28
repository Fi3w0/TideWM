use super::*;

impl Smallvil {
    /// Finds the topmost surface under `pos`, checking the same manually
    /// interleaved order used by `desktop_render_elements`: fullscreen,
    /// Overlay/Top, regular windows, then Bottom/Background.
    /// Walk up through `surface`'s parent chain and return the first
    /// ancestor (including `surface` itself) that has a pointer constraint
    /// registered for `pointer`. Constraints are typically created on the
    /// xdg-toplevel's main surface, but `surface_under` may return a
    /// subsurface of that toplevel (a popup, a sub-surface the client uses
    /// for its own cursor, etc.) -- this resolves the disconnect.
    ///
    /// Returns `None` if no constraint is found anywhere in the chain.
    pub(crate) fn root_with_constraint(
        &self,
        surface: &WlSurface,
        pointer: &PointerHandle<Smallvil>,
    ) -> Option<WlSurface> {
        let mut current = Some(surface.clone());
        while let Some(s) = current.clone() {
            let mut found: Option<WlSurface> = None;
            with_pointer_constraint(&s, pointer, |c| {
                if c.is_some() {
                    found = Some(s.clone());
                }
            });
            if let Some(root) = found {
                return Some(root);
            }
            current = get_parent(&s);
        }
        None
    }

    /// Walk up through `surface`'s parent chain and return its root
    /// (the topmost ancestor with no parent). Used to compare two surfaces
    /// for "same window" without caring whether either is a subsurface.
    pub(crate) fn surface_root(&self, surface: &WlSurface) -> Option<WlSurface> {
        let mut current = surface.clone();
        loop {
            match get_parent(&current) {
                Some(p) => current = p,
                None => return Some(current),
            }
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.output_for_point(pos)?;
        let output_geo = self.space.output_geometry(&output)?;

        // While locked, only the lock surface is hit-testable -- never the
        // windows/layers underneath, even if there is no lock surface yet
        // for this output (in which case there's simply nothing here).
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            let lock_surface = self.lock_surfaces.get(&output)?;
            let output_local = pos - output_geo.loc.to_f64();
            return under_from_surface_tree(
                lock_surface.wl_surface(),
                output_local,
                (0, 0),
                WindowSurfaceType::ALL,
            )
            .map(|(s, p)| (s, p.to_f64() + output_geo.loc.to_f64()));
        }

        self.fullscreen_surface_under(&output, pos)
            .or_else(|| {
                self.layer_surface_under(
                    &output,
                    output_geo,
                    pos,
                    &[WlrLayer::Overlay, WlrLayer::Top],
                )
            })
            .or_else(|| {
                if self.config.spatial_engine != crate::config::SpatialEngine::Classic {
                    self.render_placements(&output).and_then(|placements| {
                        placements.into_iter().find_map(|placement| {
                            let rect = self.placement_input_rect(&placement);
                            let scale = placement.view_scale.max(0.05);
                            let origin =
                                surface_origin_for_view(&placement.window, rect.loc, scale);
                            let local = Point::from((
                                (pos.x - origin.x) / scale,
                                (pos.y - origin.y) / scale,
                            ));
                            let bbox = placement.window.bbox_with_popups().to_f64();
                            if local.x < bbox.loc.x
                                || local.y < bbox.loc.y
                                || local.x >= bbox.loc.x + bbox.size.w
                                || local.y >= bbox.loc.y + bbox.size.h
                            {
                                return None;
                            }
                            placement
                                .window
                                .surface_under(local, WindowSurfaceType::ALL)
                                .map(|(surface, point)| {
                                    (
                                        surface,
                                        origin
                                            + Point::from((
                                                point.x as f64 * scale,
                                                point.y as f64 * scale,
                                            )),
                                    )
                                })
                                // FitPlacement can temporarily draw a
                                // committed client buffer at a different
                                // compositor-owned size. The visual rect is
                                // still authoritative for stacking: if the
                                // client's stale surface tree misses here,
                                // focus the topmost root instead of clicking
                                // through a floating window into a tile.
                                .or_else(|| {
                                    placement
                                        .window
                                        .toplevel()
                                        .map(|toplevel| (toplevel.wl_surface().clone(), origin))
                                })
                        })
                    })
                } else {
                    // `Space::element_under` already returns the surface
                    // render location (`map location - geometry().loc`), so
                    // this branch needs no geometry correction.
                    self.space
                        .element_under(pos)
                        .and_then(|(window, location)| {
                            window
                                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                                .map(|(s, p)| (s, (p + location).to_f64()))
                        })
                }
            })
            .or_else(|| {
                self.layer_surface_under(
                    &output,
                    output_geo,
                    pos,
                    &[WlrLayer::Bottom, WlrLayer::Background],
                )
            })
    }

    /// Topmost ordinary window and its current presentation location. Ocean
    /// resolves this through the same camera placement used by rendering;
    /// Classic keeps Smithay Space as the direct source.
    pub(crate) fn window_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(Window, Point<i32, Logical>)> {
        if self.config.spatial_engine == crate::config::SpatialEngine::Classic {
            return self
                .space
                .element_under(pos)
                .map(|(window, location)| (window.clone(), location));
        }
        let output = self.output_for_point(pos)?;
        self.render_placements(&output)?
            .into_iter()
            .find_map(|placement| {
                let rect = self.placement_input_rect(&placement);
                let local = pos - rect.loc.to_f64();
                (local.x >= 0.0
                    && local.y >= 0.0
                    && local.x < rect.size.w as f64
                    && local.y < rect.size.h as f64)
                    .then(|| {
                        (
                            placement.window.clone(),
                            surface_origin(&placement.window, rect.loc),
                        )
                    })
            })
    }

    fn fullscreen_surface_under(
        &self,
        output: &Output,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.config.spatial_engine != crate::config::SpatialEngine::Classic {
            return self.render_placements(output).and_then(|placements| {
                placements
                    .into_iter()
                    .filter(crate::placement::PlacedWindow::is_fullscreen)
                    .find_map(|placement| {
                        let rect = self.placement_input_rect(&placement);
                        let scale = placement.view_scale.max(0.05);
                        let origin = surface_origin_for_view(&placement.window, rect.loc, scale);
                        let local =
                            Point::from(((pos.x - origin.x) / scale, (pos.y - origin.y) / scale));
                        let bbox = placement.window.bbox_with_popups().to_f64();
                        if local.x < bbox.loc.x
                            || local.y < bbox.loc.y
                            || local.x >= bbox.loc.x + bbox.size.w
                            || local.y >= bbox.loc.y + bbox.size.h
                        {
                            return None;
                        }
                        placement
                            .window
                            .surface_under(local, WindowSurfaceType::ALL)
                            .map(|(surface, point)| {
                                (
                                    surface,
                                    origin
                                        + Point::from((
                                            point.x as f64 * scale,
                                            point.y as f64 * scale,
                                        )),
                                )
                            })
                            // A fullscreen FitPlacement can cover its
                            // compositor-owned visual rect before the
                            // client commits the matching surface size.
                            // Keep the topmost root authoritative during
                            // that interval instead of clicking through.
                            .or_else(|| {
                                placement
                                    .window
                                    .toplevel()
                                    .map(|toplevel| (toplevel.wl_surface().clone(), origin))
                            })
                    })
            });
        }
        self.space
            .elements_for_output(output)
            .rev()
            .filter(|window| {
                window
                    .toplevel()
                    .is_some_and(|toplevel| self.fullscreen.contains_key(toplevel.wl_surface()))
            })
            .find_map(|window| {
                let location = self.space.element_location(window)?;
                let origin = surface_origin(window, location);
                window
                    .surface_under(pos - origin.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, point)| (surface, origin.to_f64() + point.to_f64()))
            })
    }

    fn layer_surface_under(
        &self,
        output: &Output,
        output_geo: Rectangle<i32, Logical>,
        pos: Point<f64, Logical>,
        layers: &[WlrLayer],
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output_local = pos - output_geo.loc.to_f64();
        let map = layer_map_for_output(output);
        for kind in layers {
            for layer in map.layers_on(*kind).rev() {
                if self.unmapped_layer_surfaces.contains(layer.wl_surface()) {
                    continue;
                }
                let Some(layer_geo) = map.layer_geometry(layer) else {
                    continue;
                };
                if let Some((s, p)) = layer.surface_under(
                    output_local - layer_geo.loc.to_f64(),
                    WindowSurfaceType::ALL,
                ) {
                    return Some((
                        s,
                        p.to_f64() + layer_geo.loc.to_f64() + output_geo.loc.to_f64(),
                    ));
                }
            }
        }
        None
    }

    /// The layer surface (if any) whose bounds -- including its popups --
    /// contain `pos` at the frontmost rendered position. Resolving through
    /// `surface_under` is important: Bottom/Background layers must not steal
    /// input from a window rendered above them.
    pub(crate) fn layer_under_pointer(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<desktop::LayerSurface> {
        let output = self.output_for_point(pos)?;
        let (surface, _) = self.surface_under(pos)?;
        let map = layer_map_for_output(&output);
        map.layer_for_surface(&surface, WindowSurfaceType::ALL)
            .filter(|layer| !self.unmapped_layer_surfaces.contains(layer.wl_surface()))
            .cloned()
    }

    /// Prefer the output where the pointer is interacting with this exact
    /// toplevel (including one of its subsurfaces). This is intentionally
    /// stricter than merely using the pointer's output: click-to-focus users
    /// may leave the pointer on an unrelated monitor after focusing a window.
    pub(crate) fn pointer_output_for_surface(&self, surface: &WlSurface) -> Option<Output> {
        let pointer = self.seat.get_pointer()?;
        let focus = pointer.current_focus()?;
        if self.surface_root(&focus)? != self.surface_root(surface)? {
            return None;
        }
        let output = self.output_for_point(pointer.current_location())?;
        self.render_placements(&output)?
            .into_iter()
            .any(|placement| {
                placement
                    .surface()
                    .is_some_and(|candidate| candidate == surface)
            })
            .then_some(output)
    }

    /// Retained window/camera focus, independent of focus-follows-mouse's
    /// preference for an unrelated pointer output. Also used by first-map
    /// `output = focused` rules while a launcher owns keyboard focus.
    pub(crate) fn focused_output(&self) -> Option<Output> {
        let intended_output = self.window_focus.as_ref().and_then(|surface| {
            self.pointer_output_for_surface(surface)
                .or_else(|| self.rendered_output_for_surface(surface))
                .or_else(|| {
                    self.layout
                        .output_of(surface)
                        .and_then(|name| self.output_by_name(name))
                })
                .or_else(|| {
                    self.mapped_toplevel_window(surface)
                        .and_then(|window| self.output_for_window(&window))
                })
                .or_else(|| {
                    self.floating_workspace
                        .get(surface)
                        .and_then(|tag| self.output_by_name(&tag.output))
                })
                .or_else(|| {
                    self.ocean
                        .entry_output(surface)
                        .and_then(|name| self.output_by_name(name))
                })
        });
        let focused_output = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .and_then(|surface| {
                let root = self.surface_root(&surface)?;
                self.rendered_output_for_surface(&root)
            });
        intended_output.or(focused_output)
    }

    /// Requests ordinary window focus. The request becomes the retained
    /// window intent, while `reconcile_keyboard_focus` may temporarily give
    /// actual keyboard focus to a mapped Exclusive layer instead.
    pub(crate) fn focus_window(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
    ) {
        self.focus_window_with_ripple(surface, serial, true, FocusOrigin::Navigation);
    }

    /// Focus caused by the pointer itself: hover (`focus_follows_mouse`), a
    /// press, a touch contact or a tablet tip. Scroll keeps its viewport
    /// still for these -- moving the strip out from under the pointer (or
    /// recentering it because `center_focused` is on) is not a navigation
    /// request, and a monitor change must not scroll the monitor just
    /// because the cursor crossed a neighbor's peek. Keyboard navigation,
    /// IPC activation and notifications still reveal through `focus_window`.
    pub(crate) fn focus_window_from_pointer(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
    ) {
        self.focus_window_with_ripple(surface, serial, true, FocusOrigin::Pointer);
    }

    /// Gives a freshly mapped window focus without also treating that
    /// lifecycle step as a user-visible focus handoff. The map ripple is the
    /// single visual cue for this transaction; later pointer/keyboard focus
    /// changes continue through `focus_window` and animate normally.
    pub(crate) fn focus_window_on_map(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
        animate_handoff: bool,
    ) {
        self.focus_window_with_ripple(surface, serial, animate_handoff, FocusOrigin::Navigation);
    }

    /// Requests focus for an OnDemand/Exclusive layer selected by first map
    /// or click. Exclusive priority itself is derived globally; only an
    /// OnDemand target must be remembered as explicit intent.
    pub(crate) fn focus_layer(&mut self, surface: WlSurface, serial: smithay::utils::Serial) {
        if self.layer_keyboard_interactivity(&surface) == Some(KeyboardInteractivity::OnDemand) {
            self.on_demand_layer_focus = Some(surface);
        }
        self.reconcile_keyboard_focus(serial);
    }

    /// Recomputes actual seat focus from mapped state and applies XDG
    /// Activated in the same transaction. Priority is mapped Exclusive
    /// layer, valid OnDemand intent, valid visible window intent, then none.
    pub(crate) fn reconcile_keyboard_focus(&mut self, serial: smithay::utils::Serial) {
        if self
            .window_focus
            .as_ref()
            .is_some_and(|surface| !self.window_is_visible(surface))
        {
            self.window_focus = None;
        }
        if self.on_demand_layer_focus.as_ref().is_some_and(|surface| {
            self.layer_keyboard_interactivity(surface) != Some(KeyboardInteractivity::OnDemand)
        }) {
            self.on_demand_layer_focus = None;
        }

        // Locked takes absolute priority over everything else -- no window
        // or layer may hold keyboard focus while the session is locked,
        // even if none of them changed their own state. `None` here (no
        // lock surface registered yet) intentionally clears keyboard focus
        // entirely rather than falling through to the normal chain below.
        let resolved = if !matches!(self.session_lock, SessionLock::Unlocked) {
            self.lock_focus_target()
                .map(KeyboardFocusTarget::Lock)
                .unwrap_or(KeyboardFocusTarget::None)
        } else {
            self.exclusive_layer()
                .map(|layer| KeyboardFocusTarget::Layer(layer.wl_surface().clone()))
                .or_else(|| {
                    self.on_demand_layer_focus
                        .clone()
                        .map(KeyboardFocusTarget::Layer)
                })
                .or_else(|| self.window_focus.clone().map(KeyboardFocusTarget::Window))
                .unwrap_or(KeyboardFocusTarget::None)
        };

        self.release_popup_grab_if_focus_leaves(&resolved);

        let popup_keyboard_focus = self.popup_grab.as_ref().and_then(|popup| {
            let owns_keyboard = popup.has_keyboard_grab
                && !popup.grab.has_ended()
                && resolved.surface() == Some(&popup.root);
            owns_keyboard.then(|| popup.grab.current_grab()).flatten()
        });
        let seat_target = popup_keyboard_focus.or_else(|| resolved.surface().cloned());

        let seat_surface = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        let focus_changed =
            resolved != self.keyboard_focus || seat_surface.as_ref() != seat_target.as_ref();

        let old_window = match &self.keyboard_focus {
            KeyboardFocusTarget::Window(surface) => Some(surface.clone()),
            _ => None,
        };
        let new_window = match &resolved {
            KeyboardFocusTarget::Window(surface) => Some(surface.clone()),
            _ => None,
        };

        // Commit the target before mirroring activation to wlr-toplevel state.
        self.keyboard_focus = resolved;

        let mut activation_changed = false;
        if old_window != new_window {
            // Capture the focus target for the IPC event before the
            // `if let Some(surface) = new_window` block below moves it.
            let new_window_for_event = new_window.clone();
            if let Some(surface) = old_window {
                activation_changed |= self.set_window_activated(&surface, false);
                self.refresh_wlr_toplevel_state(&surface);
            }
            if let Some(surface) = new_window {
                activation_changed |= self.set_window_activated(&surface, true);
                self.refresh_wlr_toplevel_state(&surface);
                self.note_depth_attention(&surface);
                if self.urgent.remove(&surface) {
                    if let Some(depth) = self.window_depths.get_mut(&surface) {
                        depth.visual_changed();
                    }
                    // Mirrors `mark_urgent`'s own refresh on the way in --
                    // a rule keyed on `urgent = true` must stop applying
                    // the moment urgency clears, not linger until reload.
                    self.refresh_window_opacity_and_glass_for(&surface);
                    self.emit_ipc_event(crate::ipc::IpcEvent::UrgentChanged {
                        surface: surface.clone(),
                        urgent: false,
                    });
                }
                if !self.cycling_focus {
                    self.focus_history.retain(|s| s != &surface);
                    self.focus_history.insert(0, surface);
                }
            }
            // Focus handoff between two real windows (or window -> none).
            // `reconcile_keyboard_focus` is called from many paths -- focus
            // authority, popup grab release, layer-surface interactivity
            // changes, workspace switches, window map -- all of which are
            // exactly the transitions a bar widget wants to know about.
            self.emit_ipc_event(crate::ipc::IpcEvent::FocusChanged {
                surface: new_window_for_event,
            });
            self.buoyancy_dirty = true;
        }

        if let Some(keyboard) = self.seat.get_keyboard() {
            if keyboard.current_focus() != seat_target {
                keyboard.set_focus(self, seat_target, serial);
            }
        }
        if focus_changed || activation_changed {
            self.request_redraw();
        }
    }

    /// Ends a popup grab when logical focus moves away from its root. The
    /// pointer grab is released from a calloop idle so this remains safe if a
    /// future focus path originates inside a pointer-grab callback.
    fn release_popup_grab_if_focus_leaves(&mut self, target: &KeyboardFocusTarget) {
        let leaving = self
            .popup_grab
            .as_ref()
            .is_some_and(|popup| popup.has_keyboard_grab && target.surface() != Some(&popup.root));
        if !leaving {
            return;
        }

        self.release_popup_grab();
    }

    pub(super) fn release_popup_grab(&mut self) {
        let Some(mut popup) = self.popup_grab.take() else {
            return;
        };
        let grab_serial = popup.grab.serial();
        let previous_serial = popup.grab.previous_serial();
        popup.grab.ungrab(PopupUngrabStrategy::All);

        if let Some(keyboard) = self.seat.get_keyboard() {
            let owns_grab = keyboard.has_grab(grab_serial)
                || previous_serial.is_some_and(|serial| keyboard.has_grab(serial));
            if owns_grab {
                keyboard.unset_grab(self);
            }
        }

        let serial = SERIAL_COUNTER.next_serial();
        let time = smithay::backend::input::InputTime::now();
        self.loop_handle.insert_idle(move |state| {
            let Some(pointer) = state.seat.get_pointer() else {
                return;
            };
            let owns_grab = pointer.has_grab(grab_serial)
                || previous_serial.is_some_and(|serial| pointer.has_grab(serial));
            if owns_grab {
                pointer.unset_grab(state, serial, time);
            }
        });
    }

    fn release_popup_grab_for_root(&mut self, root: &WlSurface) {
        if self
            .popup_grab
            .as_ref()
            .is_some_and(|popup| &popup.root == root)
        {
            self.release_popup_grab();
        }
    }

    /// Removes a bufferless popup from the active grab without discarding a
    /// still-mapped parent menu. A null-buffer commit keeps the xdg_popup
    /// role alive, so Smithay's resource-lifetime cleanup cannot perform this
    /// transition for us. Only the topmost popup can be removed from a valid
    /// nested grab; if a client unmaps a non-topmost ancestor, dismiss the
    /// whole chain rather than retain focus on an invisible hierarchy.
    pub(crate) fn unmap_popup_grab(&mut self, surface: &WlSurface, root: Option<&WlSurface>) {
        if root.is_none()
            || self
                .popup_grab
                .as_ref()
                .is_none_or(|popup| Some(&popup.root) != root)
        {
            return;
        }
        let Some(mut popup) = self.popup_grab.take() else {
            return;
        };
        let topmost = popup.grab.current_grab();
        let strategy = if topmost.as_ref() == Some(surface) {
            PopupUngrabStrategy::Topmost
        } else {
            PopupUngrabStrategy::All
        };
        popup.grab.ungrab(strategy);
        self.popup_grab = Some(popup);
        self.refresh_popup_grab();
        self.reconcile_keyboard_focus(SERIAL_COUNTER.next_serial());
    }

    /// Drops completed popup-grab bookkeeping after `PopupManager::cleanup`
    /// and restores both input devices to the centralized root focus.
    pub(crate) fn refresh_popup_grab(&mut self) {
        let ended = self
            .popup_grab
            .as_ref()
            .is_some_and(|popup| popup.grab.has_ended());
        if !ended {
            return;
        }

        let popup = self.popup_grab.take().unwrap();
        let grab_serial = popup.grab.serial();
        let previous_serial = popup.grab.previous_serial();
        let serial = SERIAL_COUNTER.next_serial();
        let time = smithay::backend::input::InputTime::now();
        if let Some(pointer) = self.seat.get_pointer() {
            let owns_grab = pointer.has_grab(grab_serial)
                || previous_serial.is_some_and(|candidate| pointer.has_grab(candidate));
            if owns_grab {
                pointer.unset_grab(self, serial, time);
            }
        }
        self.reconcile_keyboard_focus(serial);
    }

    /// Returns whether a popup rooted at `root` may grab and, if so, whether
    /// it should install a keyboard grab in addition to the pointer grab.
    /// Ordinary windows and keyboard-interactive layers must already own
    /// logical focus; a mapped `KeyboardInteractivity::None` layer is allowed
    /// a pointer-only menu grab.
    pub(crate) fn popup_grab_policy(&self, root: &WlSurface) -> Option<bool> {
        if self.mapped_toplevel_window(root).is_some() {
            return matches!(&self.keyboard_focus, KeyboardFocusTarget::Window(surface) if surface == root)
                .then_some(true);
        }

        match self.layer_keyboard_interactivity(root)? {
            KeyboardInteractivity::None => Some(false),
            KeyboardInteractivity::OnDemand | KeyboardInteractivity::Exclusive => {
                matches!(&self.keyboard_focus, KeyboardFocusTarget::Layer(surface) if surface == root)
                    .then_some(true)
            }
        }
    }

    /// The ordinary window that owns logical WM focus. During an xdg-popup
    /// keyboard grab the real seat focus is a descendant popup surface, but
    /// compositor actions and IPC must continue to address its root window.
    pub(crate) fn focused_window_surface(&self) -> Option<WlSurface> {
        match &self.keyboard_focus {
            KeyboardFocusTarget::Window(surface) => Some(surface.clone()),
            KeyboardFocusTarget::None
            | KeyboardFocusTarget::Layer(_)
            | KeyboardFocusTarget::Lock(_) => None,
        }
    }

    /// The surface actually holding keyboard focus right now, whatever its
    /// role (window, layer, lock surface). `input.rs`'s shortcuts-inhibit
    /// gate needs the real focused surface -- an inhibitor registered by a
    /// VM client sits on its toplevel, and `focused_window_surface`'s
    /// window-only view would miss the layer case -- but can't match on
    /// the private `KeyboardFocusTarget` itself.
    pub(crate) fn keyboard_focused_surface(&self) -> Option<&WlSurface> {
        match &self.keyboard_focus {
            KeyboardFocusTarget::Window(surface)
            | KeyboardFocusTarget::Layer(surface)
            | KeyboardFocusTarget::Lock(surface) => Some(surface),
            KeyboardFocusTarget::None => None,
        }
    }

    /// Smithay's `WlPointer` converts a logical pointer location into client
    /// coordinates with the client's `client_scale`. Ocean's camera zoom is a
    /// compositor-side scale, so a one-unit logical motion over a window at
    /// zoom 2 must arrive as 0.5 client units. Use the inverse camera zoom as
    /// a temporary client scale while delivering the event; surface commits
    /// run after this event handler returns, so the scale cannot leak into
    /// rendering or damage bookkeeping.
    fn pointer_client_scale_for_surface(&self, surface: &WlSurface) -> Option<f64> {
        if self.config.spatial_engine != crate::config::SpatialEngine::Ocean {
            return None;
        }
        let root = self.surface_root(surface)?;
        let output = self.rendered_output_for_surface(&root)?;
        if self.fullscreen.contains_key(&root) || self.maximized.contains_key(&root) {
            return Some(1.0);
        }
        if let Some(scale) = self.ocean.screen_pin_view_scale(&root) {
            return Some(scale);
        }
        Some(self.ocean.camera(&output.name()).zoom.max(0.05))
    }

    fn set_pointer_client_scale(&self, surface: &WlSurface, scale: f64) -> Option<f64> {
        let client = self.display_handle.get_client(surface.id()).ok()?;
        let state = client.get_data::<ClientState>()?;
        let previous = state.compositor_state.client_scale();
        state.compositor_state.set_client_scale(scale);
        Some(previous)
    }

    /// The camera scale used for pointer coordinates on `surface`, or 1.0
    /// outside Ocean. Constraint regions live in client coordinates and need
    /// the same inverse transform as pointer motion.
    pub(crate) fn pointer_view_scale_for_surface(&self, surface: &WlSurface) -> f64 {
        self.pointer_client_scale_for_surface(surface)
            .unwrap_or(1.0)
    }

    /// Temporarily apply Ocean's inverse camera scale to the client that owns
    /// `surface`. Returns the previous client scale for restoration. `None`
    /// means the client already needs no transform (Classic/Scroll, fullscreen,
    /// or an unscaled placement).
    pub(crate) fn apply_pointer_client_scale(&self, surface: &WlSurface) -> Option<f64> {
        let view_scale = self.pointer_client_scale_for_surface(surface)?;
        let client_scale = client_scale_for_view_scale(view_scale)?;
        self.set_pointer_client_scale(surface, client_scale)
    }

    pub(crate) fn restore_pointer_client_scale(&self, surface: &WlSurface, previous: f64) {
        let Ok(client) = self.display_handle.get_client(surface.id()) else {
            return;
        };
        let Some(state) = client.get_data::<ClientState>() else {
            return;
        };
        state.compositor_state.set_client_scale(previous);
    }

    /// Queue a pointer-target refresh after a camera change. The refresh
    /// reads the pointer handle, so it must run outside a grab callback.
    pub(crate) fn queue_pointer_focus_refresh(&mut self) {
        self.loop_handle.insert_idle(|state| {
            state.refresh_pointer_focus();
        });
    }

    /// Re-resolves enter/leave at the current pointer location after the
    /// surface stack changes without physical motion. Also clamps that
    /// location into the live output union first: an output-management
    /// change or a disconnect can shrink, move, or remove the output the
    /// pointer was last over, and nothing else re-validates its stored
    /// position outside of a real subsequent motion event.
    pub(crate) fn refresh_pointer_focus(&mut self) {
        if self
            .seat
            .get_pointer()
            .is_none_or(|pointer| pointer.is_grabbed())
        {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let pos = self.clamp_to_outputs(pointer.current_location());
        let under = self.surface_under(pos);
        let focus_surface = under.as_ref().map(|(surface, _)| surface.clone());
        let previous = focus_surface
            .as_ref()
            .and_then(|surface| self.apply_pointer_client_scale(surface));
        let serial = SERIAL_COUNTER.next_serial();
        let time = smithay::backend::input::InputTime::now();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        if let (Some(surface), Some(previous)) = (focus_surface.as_ref(), previous) {
            self.restore_pointer_client_scale(surface, previous);
        }
        pointer.frame(self);
    }

    /// The retained visible window underneath any layer focus. Used only for
    /// placement policy, not user actions against the currently focused
    /// client.
    pub(crate) fn intended_window_surface(&self) -> Option<WlSurface> {
        self.window_focus.clone()
    }

    /// Applies XDG Activated to one tracked role. Looking through the
    /// layout/floating registries (via `mapped_toplevel_window`) keeps this
    /// correct for a protocol-mapped window on an inactive workspace without
    /// turning every animated layer commit into an O(window-count) sweep.
    fn set_window_activated(&mut self, surface: &WlSurface, activated: bool) -> bool {
        let mapped = !self.unmapped_toplevels.contains_key(surface);
        let window = self
            .unmapped_toplevels
            .get(surface)
            .cloned()
            .or_else(|| self.mapped_toplevel_window(surface));
        let Some(window) = window else { return false };
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        let changed = window.set_activated(activated && mapped);
        if changed && mapped && toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
        changed
    }

    /// Whether `surface` currently holds keyboard focus. `KeyboardFocusTarget`
    /// is private to this module, so this is the query surface for callers
    /// elsewhere (`handlers/xdg_shell.rs`'s wlr-foreign-toplevel state
    /// mirroring) that need it without exposing the enum itself.
    pub(crate) fn is_window_activated(&self, surface: &WlSurface) -> bool {
        matches!(&self.keyboard_focus, KeyboardFocusTarget::Window(s) if s == surface)
    }

    pub(crate) fn window_is_visible(&self, surface: &WlSurface) -> bool {
        self.space
            .elements()
            .any(|window| is_window(window, surface))
    }

    fn layer_keyboard_interactivity(&self, surface: &WlSurface) -> Option<KeyboardInteractivity> {
        if self.unmapped_layer_surfaces.contains(surface) {
            return None;
        }
        self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .map(|layer| layer.cached_state().keyboard_interactivity)
        })
    }

    /// The output under the pointer.
    pub(crate) fn pointer_output(&self) -> Option<Output> {
        self.seat
            .get_pointer()
            .and_then(|pointer| self.output_for_point(pointer.current_location()))
    }

    /// Selects a deterministic ordinary-window fallback after the current
    /// focus owner disappears. Pointer intent wins when focus-follows-mouse
    /// is enabled; then the topmost visible window on the preferred output,
    /// then the topmost visible window anywhere.
    pub(crate) fn repair_keyboard_focus(
        &mut self,
        preferred_output: Option<&str>,
        serial: smithay::utils::Serial,
    ) {
        if self
            .window_focus
            .as_ref()
            .is_some_and(|surface| !self.window_is_visible(surface))
        {
            self.window_focus = None;
        }
        if self.on_demand_layer_focus.as_ref().is_some_and(|surface| {
            self.layer_keyboard_interactivity(surface) != Some(KeyboardInteractivity::OnDemand)
        }) {
            self.on_demand_layer_focus = None;
        }

        if self.window_focus.is_none() && self.config.input.focus_follows_mouse {
            if let Some(pos) = self
                .seat
                .get_pointer()
                .map(|pointer| pointer.current_location())
            {
                if self.layer_under_pointer(pos).is_none() {
                    if let Some(surface) = self.window_at_layout_position(pos) {
                        self.window_focus = Some(surface);
                    }
                }
            }
        }

        if self.window_focus.is_none() {
            if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
                self.window_focus = self.topmost_ocean_surface(preferred_output);
            } else {
                self.refresh_space();
                self.window_focus = preferred_output
                    .and_then(|output_name| {
                        self.space
                            .elements()
                            .rev()
                            .find(|window| {
                                self.output_for_window(window)
                                    .is_some_and(|output| output.name() == output_name)
                            })
                            .and_then(|window| {
                                window
                                    .toplevel()
                                    .map(|toplevel| toplevel.wl_surface().clone())
                            })
                    })
                    .or_else(|| {
                        self.space.elements().rev().find_map(|window| {
                            window
                                .toplevel()
                                .map(|toplevel| toplevel.wl_surface().clone())
                        })
                    });
            }
        }
        self.reconcile_keyboard_focus(serial);
    }

    /// Repairs Ocean focus after an output disappears. `Space` retains every
    /// Ocean window as a world/input cache, including windows that no
    /// surviving camera can see, so ordinary mapped-state checks are not
    /// sufficient here.
    pub(crate) fn repair_keyboard_focus_after_output_change(
        &mut self,
        preferred_output: Option<&str>,
        serial: smithay::utils::Serial,
    ) {
        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean
            && self
                .window_focus
                .as_ref()
                .is_some_and(|surface| self.rendered_output_for_surface(surface).is_none())
        {
            self.window_focus = None;
        }
        self.repair_keyboard_focus(preferred_output, serial);
    }

    /// Drops a destroyed/unmapped window from retained focus intent before
    /// choosing a fallback. If an Exclusive layer currently owns the seat,
    /// this still repairs the window that should be restored underneath it.
    pub(crate) fn forget_window_focus(&mut self, surface: &WlSurface) {
        self.layout.scroll.forget(surface);
        self.layout
            .scroll
            .prune(&self.layout.populated_workspaces());
        self.release_popup_grab_for_root(surface);
        if self.window_focus.as_ref() == Some(surface) {
            self.window_focus = None;
        }
    }

    /// Drops an unmapped/destroyed OnDemand layer from retained focus
    /// intent. Exclusive focus is derived from mapped layer state and needs
    /// no separate bookkeeping here.
    pub(crate) fn forget_layer_focus(&mut self, surface: &WlSurface) {
        self.release_popup_grab_for_root(surface);
        if self.on_demand_layer_focus.as_ref() == Some(surface) {
            self.on_demand_layer_focus = None;
        }
    }

    /// Pushes `output`'s fractional scale to every surface in `window`'s
    /// tree that bound wp_fractional_scale. Cheap to call from placement
    /// paths: Smithay only emits the protocol event when the value
    /// actually changes.
    pub(crate) fn set_window_fractional_scale(&self, window: &Window, output: &Output) {
        let scale = output.current_scale().fractional_scale();
        window.with_surfaces(|_, states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }

    /// Same as `set_window_fractional_scale` for a layer-shell surface.
    pub(crate) fn set_layer_fractional_scale(
        &self,
        layer: &desktop::LayerSurface,
        output: &Output,
    ) {
        let scale = output.current_scale().fractional_scale();
        layer.with_surfaces(|_, states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }

    /// Refreshes mapped layer surfaces after their output's live scale changes.
    pub(crate) fn refresh_layer_fractional_scales(&self, output: &Output) {
        for layer in layer_map_for_output(output).layers() {
            self.set_layer_fractional_scale(layer, output);
        }
    }

    /// Reassigns keyboard focus to the first mapped window on `output_name`,
    /// or clears focus if there is none -- `unmap_elem` never touches
    /// keyboard focus itself, so without this a window that was just hidden
    /// (workspace switch, or moved to another workspace) would stay
    /// "focused" while mapped nowhere. Shared by `switch_workspace` and
    /// `move_to_workspace`, whose "leaving the visible workspace" cases are
    /// otherwise identical.
    pub(super) fn refocus_after_hide(&mut self, output_name: &str) {
        // `Space::outputs_for_element` (which `output_for_window` below
        // uses) reads a per-element cache that's normally kept fresh by
        // each backend's render loop calling `space.refresh()` once a
        // frame -- but that hasn't run yet since whatever `map_element`/
        // `unmap_elem` calls preceded this, so without an explicit refresh
        // here it would see stale cache data. Cheap and idempotent to call
        // an extra time (it no-ops unless something actually changed).
        self.refresh_space();

        let on_output = |w: &Window| {
            self.output_for_window(w)
                .is_some_and(|o| o.name() == output_name)
        };
        // The last window the user focused here wins. Stacking order is only
        // a fallback: Scroll re-maps columns in strip order, so the topmost
        // element is the last column, not the one the user was on.
        let remembered = self.focus_history.iter().find(|surface| {
            self.space
                .elements()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == *surface))
                .is_some_and(on_output)
        });
        let next_focus = remembered.cloned().or_else(|| {
            self.space
                .elements()
                .rev()
                .find(|w| on_output(w))
                .and_then(|w| w.toplevel())
                .map(|t| t.wl_surface().clone())
        });
        self.focus_window(next_focus, SERIAL_COUNTER.next_serial());
    }

    /// Current XKB layout group and every layout name in the keymap, for
    /// `tidectl keyboard-layouts` (also niri's `keyboard-layouts` shape:
    /// `current_idx` is zero-based).
    pub fn keyboard_layouts(&mut self) -> Option<(u32, Vec<String>)> {
        let keyboard = self.seat.get_keyboard()?;
        Some(keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            (
                xkb.active_layout().0,
                xkb.layouts()
                    .map(|layout| xkb.layout_name(layout).to_string())
                    .collect(),
            )
        }))
    }

    /// The configured `xkb_layout` codes (`us,ru,es` -> `[us, ru, es]`),
    /// positionally matching `keyboard_layouts()`'s names.
    pub fn keyboard_layout_codes(&self) -> Vec<String> {
        self.config
            .input
            .xkb_layout
            .split(',')
            .map(|code| code.trim().to_string())
            .filter(|code| !code.is_empty())
            .collect()
    }

    /// Broadcasts `layout-changed` when the active group (or the layout
    /// list) differs from the last announced one. Called after keyboard
    /// input and after `switch-layout`, so both a `grp:` xkb key and an
    /// IPC/bind action reach subscribers without polling.
    pub fn notify_keyboard_layout_changed(&mut self) {
        let Some((group, names)) = self.keyboard_layouts() else {
            return;
        };
        if self.keyboard_layout_state.as_ref() == Some(&(group, names.clone())) {
            return;
        }
        self.keyboard_layout_state = Some((group, names.clone()));
        self.emit_ipc_event(crate::ipc::IpcEvent::LayoutChanged {
            group,
            names,
            codes: self.keyboard_layout_codes(),
        });
    }

    /// Records real pointer motion and, if `cursor_hide_after_ms` is
    /// configured, makes sure a wake-up timer is running -- damage-gated
    /// rendering means nothing else would naturally trigger a redraw at the
    /// moment the idle threshold passes on an otherwise-static desktop, so
    /// the auto-hide needs its own explicit prod. Only arms a new timer if
    /// one isn't already pending (`cursor_idle_timer_armed`); a real mouse
    /// fires this on every motion event, so spawning a fresh calloop timer
    /// source each time would mean hundreds per second. The one live timer
    /// re-reads `last_pointer_motion` when it fires and reschedules itself
    /// further out (`TimeoutAction::ToDuration`, the same self-extending
    /// idiom `winit.rs`'s own redraw timer uses) if motion happened more
    /// recently than expected, rather than a fresh source per motion event.
    /// The timer only requests a redraw; the actual hide/show decision is
    /// re-derived fresh from `last_pointer_motion` at render time
    /// (`backend/udev.rs`), so a slightly-early/late fire can't itself
    /// cause an incorrect result. No-op under winit (`udev_gpu` is
    /// `None` there) -- only the udev backend composites its own cursor at
    /// all.
    pub(crate) fn note_pointer_motion(&mut self) {
        self.last_pointer_motion = Instant::now();
        if self.config.cursor_hide_after_ms <= 0 || self.udev_gpu.is_none() {
            return;
        }
        if self.cursor_idle_timer_armed {
            return;
        }
        self.cursor_idle_timer_armed = true;
        let delay = Duration::from_millis(self.config.cursor_hide_after_ms as u64);
        let result = self.loop_handle.insert_source(
            Timer::from_duration(delay),
            move |_, _, state: &mut Smallvil| {
                let configured =
                    Duration::from_millis(state.config.cursor_hide_after_ms.max(0) as u64);
                let elapsed = state.last_pointer_motion.elapsed();
                if elapsed >= configured {
                    state.cursor_idle_timer_armed = false;
                    state.request_redraw();
                    TimeoutAction::Drop
                } else {
                    TimeoutAction::ToDuration(configured - elapsed)
                }
            },
        );
        if let Err(err) = result {
            self.cursor_idle_timer_armed = false;
            tracing::warn!(%err, "Failed to register cursor idle timer");
        }
    }

    /// Raises `surface` to the top of the floating stack. No-op on a tiled
    /// window -- tiled windows never overlap by construction, so raising
    /// one has no meaning (same restriction `toggle_pseudo_tile` applies in
    /// reverse). Floating windows are already always rendered above the
    /// tiled layer (see the Z-order invariant `retile()` enforces); this
    /// only changes relative order *within* the floating stack.
    pub fn raise_window(&mut self, surface: &WlSurface) {
        if !self.floating_workspace.contains_key(surface) {
            return;
        }
        if let Some(window) = self.mapped_toplevel_window(surface) {
            self.space.raise_element(&window, false);
            self.request_redraw();
        }
    }

    /// Sends `surface` to the bottom of the floating stack, still above
    /// every tiled window. `Space` has no direct "lower" primitive, so this
    /// raises every *other* floating window instead, preserving their
    /// relative order -- the same trick `retile()` already uses to restore
    /// the floating-above-tiled invariant after a tiling pass, just with
    /// `surface` deliberately left out of the re-raise.
    pub fn lower_window(&mut self, surface: &WlSurface) {
        if !self.floating_workspace.contains_key(surface) {
            return;
        }
        let others: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                w.toplevel().is_some_and(|t| {
                    let s = t.wl_surface();
                    s != surface && self.floating_workspace.contains_key(s)
                })
            })
            .cloned()
            .collect();
        if others.is_empty() {
            return;
        }
        for window in others {
            self.space.raise_element(&window, false);
        }
        self.request_redraw();
    }

    /// Whether an xdg-activation token carries a fresh serial from our seat.
    /// Serial-less tokens remain mintable for compatibility, but this returns
    /// false so consumption takes the urgency path instead of granting focus.
    pub(crate) fn activation_serial_is_fresh(
        &self,
        serial: &Option<(
            smithay::utils::Serial,
            smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        )>,
    ) -> bool {
        let Some((serial, seat)) = serial else {
            return false;
        };
        if Seat::from_resource(seat).as_ref() != Some(&self.seat) {
            return false;
        }
        let keyboard_fresh = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.last_enter())
            .is_some_and(|last_enter| serial.is_no_older_than(&last_enter));
        let pointer_fresh = self
            .seat
            .get_pointer()
            .and_then(|pointer| pointer.last_enter())
            .is_some_and(|last_enter| serial.is_no_older_than(&last_enter));
        keyboard_fresh || pointer_fresh
    }

    pub(crate) fn app_activation_target(&self, app_id: &str) -> Option<WlSurface> {
        let matches = |surface: &WlSurface| {
            self.mapped_toplevel_window(surface).is_some()
                && self.toplevel_identity(surface).0.as_deref() == Some(app_id)
        };
        self.focus_history
            .iter()
            .find(|surface| matches(surface))
            .cloned()
            .or_else(|| {
                self.foreign_toplevel_numeric_ids
                    .iter()
                    .filter(|(surface, _)| matches(surface))
                    .min_by_key(|(_, id)| **id)
                    .map(|(surface, _)| surface.clone())
            })
    }

    /// Grants an xdg-activation request for `surface`: focus its window,
    /// switching to its workspace first if that workspace is hidden,
    /// promoting it to its group's active tab if it's a parked member, and
    /// raising it if it's floating. No-op while an Exclusive layer surface
    /// (e.g. a lock screen) is mapped -- the same guard every other
    /// focus-moving action uses, so a background client can't activate its
    /// way past one.
    pub(crate) fn activate_toplevel(&mut self, surface: &WlSurface) {
        if self.exclusive_layer().is_some() || !matches!(self.session_lock, SessionLock::Unlocked) {
            return;
        }
        if self.mapped_toplevel_window(surface).is_none() {
            return;
        }

        if let Some(entry) = self.classic_depth.remove(surface) {
            self.layout
                .insert(&entry.output, entry.workspace, entry.window, None);
            self.retile();
        }

        // Where does the window live? A parked group member isn't in any
        // tree under its own surface -- the group owns its slot.
        let ownership = (self.config.spatial_engine.uses_workspaces())
            .then(|| {
                self.group_of(surface)
                    .map(|idx| (self.groups[idx].output.clone(), self.groups[idx].workspace))
                    .or_else(|| {
                        let name = self.layout.output_of(surface)?;
                        let workspace = self.layout.workspace_of(surface)?;
                        Some((name.to_string(), workspace))
                    })
                    .or_else(|| {
                        self.floating_workspace
                            .get(surface)
                            .map(|tag| (tag.output.clone(), tag.workspace))
                    })
            })
            .flatten();

        if let Some((output_name, workspace)) = ownership {
            let hidden = self.layout.active_workspace(&output_name) != workspace
                && !self.pinned.contains(surface);
            if hidden {
                if let Some(output) = self.output_by_name(&output_name) {
                    self.switch_workspace_immediate(&output, workspace);
                }
            }
        }

        if let Some(idx) = self.group_of(surface) {
            if let Some(tab) = self.groups[idx]
                .members
                .iter()
                .position(|m| &m.surface == surface)
            {
                self.group_activate_tab(idx, tab);
            }
        }

        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            let target = self
                .group_of(surface)
                .and_then(|idx| self.groups[idx].members.get(self.groups[idx].active))
                .map(|member| member.surface.clone())
                .unwrap_or_else(|| surface.clone());
            let output = self
                .ocean
                .entry_output(&target)
                .and_then(|name| self.output_by_name(name))
                .or_else(|| self.primary_output());
            if let Some(output) = output {
                if let (Some(viewport), Some(rect)) = (
                    self.space.output_geometry(&output).map(|geo| geo.size),
                    self.ocean
                        .world_rect(&target, self.config.gaps, self.config.bsp_split_bias),
                ) {
                    if !self.ocean.is_screen_pinned(&target) {
                        self.ocean.center_on_rect(
                            &output.name(),
                            viewport,
                            rect,
                            Duration::from_millis(self.config.ocean.camera_animation_ms),
                            self.config.ocean.camera_sway,
                        );
                    }
                }
            }
            self.retile_with_viscosity(false, None);
        }

        // Raise only a floating window: raising a tiled one would lift the
        // whole tiled layer above any overlapping floating window, breaking
        // the z-order invariant -- tiled windows never overlap each other,
        // so they never need raising at all.
        if self.floating_workspace.contains_key(surface) {
            if let Some(window) = self.mapped_toplevel_window(surface) {
                self.space.raise_element(&window, false);
            }
        }

        self.focus_window(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        self.request_redraw();
    }

    /// Focuses the window under `pos` without raising it. Layer surfaces and
    /// exclusive-interactivity states are excluded.
    /// `under` is the surface the same motion event already hit-tested, so
    /// the layer check does not walk every placement a second time.
    pub fn focus_follows_mouse(&mut self, pos: Point<f64, Logical>, under: Option<&WlSurface>) {
        if !self.config.input.focus_follows_mouse {
            return;
        }
        if self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
        {
            return;
        }
        if self.exclusive_layer().is_some() {
            return;
        }
        let over_layer = under.is_some_and(|surface| {
            self.output_for_point(pos).is_some_and(|output| {
                layer_map_for_output(&output)
                    .layer_for_surface(surface, WindowSurfaceType::ALL)
                    .is_some_and(|layer| !self.unmapped_layer_surfaces.contains(layer.wl_surface()))
            })
        });
        if over_layer {
            return;
        }
        let surface = self
            .window_under(pos)
            .and_then(|(window, _)| window.toplevel().map(|t| t.wl_surface().clone()));
        let Some(surface) = surface else {
            if self.config.input.unfocus_on_empty && self.focused_window_surface().is_some() {
                self.focus_window_from_pointer(None, SERIAL_COUNTER.next_serial());
            }
            return;
        };
        let current = self.focused_window_surface();
        if current.as_ref() == Some(&surface) {
            return;
        }
        self.focus_window_from_pointer(Some(surface), SERIAL_COUNTER.next_serial());
    }

    /// Cycles keyboard focus to the next mapped window (tiled or floating)
    /// and raises it to the top of the stack if it floats. This is the only
    /// way to reach a window that's fully covered by another one, since you
    /// can't click something you can't see. A tiled window is focused where
    /// it is: raising it would lift it above every floating window and break
    /// the floating-above-tiled z-order until the next retile.
    pub fn cycle_focus(&mut self) {
        // Don't let a keybind tab focus away from an exclusive-interactivity
        // layer (e.g. a lock screen) while it's still mapped.
        if self.exclusive_layer().is_some() {
            return;
        }
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        if windows.is_empty() {
            return;
        }
        let surface_of = |w: &Window| w.toplevel().map(|t| t.wl_surface().clone());

        // MRU windows first, then never-focused visible windows in Space order.
        let mut ordered: Vec<WlSurface> = self
            .focus_history
            .iter()
            .filter(|s| windows.iter().any(|w| surface_of(w).as_ref() == Some(*s)))
            .cloned()
            .collect();
        for window in &windows {
            if let Some(surface) = surface_of(window) {
                if !ordered.contains(&surface) {
                    ordered.push(surface);
                }
            }
        }
        if ordered.is_empty() {
            return;
        }

        let current = self.focused_window_surface();
        let current_index = current
            .as_ref()
            .and_then(|s| ordered.iter().position(|o| o == s));
        let next_index = match current_index {
            Some(i) => (i + 1) % ordered.len(),
            None => 0,
        };
        let next_surface = ordered[next_index].clone();
        let Some(next) = windows
            .iter()
            .find(|w| surface_of(w).as_ref() == Some(&next_surface))
        else {
            return;
        };

        if !self.spatial_is_tiled(&next_surface) {
            self.space.raise_element(next, false);
        }
        self.cycling_focus = true;
        self.focus_window(Some(next_surface), SERIAL_COUNTER.next_serial());
        self.cycling_focus = false;
    }

    /// Moves keyboard focus to the nearest window in `direction`. Classic
    /// uses mapped screen geometry. Ocean uses camera-projected world geometry
    /// and travels the action output's camera only when the selected window is
    /// outside its current view; screen pins use viewport geometry and never
    /// their dormant world rectangle.
    pub fn focus_direction(&mut self, direction: Direction) {
        let Some(focused) = self.focused_window_surface() else {
            return;
        };

        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            let Some(output) = self.primary_output() else {
                return;
            };
            let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size) else {
                return;
            };
            let Some((next, target_rect, already_visible)) =
                self.ocean_focus_target(&focused, &output, direction)
            else {
                return;
            };
            let Some(next_surface) = next.toplevel().map(|t| t.wl_surface().clone()) else {
                return;
            };

            if !already_visible {
                self.ocean.center_on_rect(
                    &output.name(),
                    viewport,
                    target_rect,
                    Duration::from_millis(self.config.ocean.camera_animation_ms),
                    self.config.ocean.camera_sway,
                );
                self.request_redraw();
            }
            if !self.spatial_is_tiled(&next_surface) {
                self.space.raise_element(&next, false);
            }
            self.focus_window(Some(next_surface), SERIAL_COUNTER.next_serial());
            return;
        }

        if self.config.spatial_engine == crate::config::SpatialEngine::Scroll
            && self.layout.contains(&focused)
        {
            if let Some(next) = self.scroll_focus_target(&focused, direction) {
                self.activate_toplevel(&next);
            }
            return;
        }

        let Some(current) = self
            .space
            .elements()
            .find(|w| is_window(w, &focused))
            .cloned()
        else {
            return;
        };
        let Some(next) = self.neighbor_in_direction(&current, direction) else {
            return;
        };
        let Some(next_surface) = next.toplevel().map(|t| t.wl_surface().clone()) else {
            return;
        };

        if !self.spatial_is_tiled(&next_surface) {
            self.space.raise_element(&next, false);
        }
        self.focus_window(Some(next_surface), SERIAL_COUNTER.next_serial());
    }

    /// Swaps the currently focused *tiled* window with its neighbor in
    /// `direction`, keeping focus on the same window (which moves to the
    /// neighbor's former slot). Floating windows aren't part of the tiling
    /// tree, so this only applies to tiled ones -- swapping a floating
    /// window's screen position doesn't mean anything the way it does for
    /// two tiled slots.
    pub fn swap_direction(&mut self, direction: Direction) {
        let Some(focused) = self.focused_window_surface() else {
            return;
        };
        if !self.layout.contains(&focused) {
            return;
        }
        if self.config.spatial_engine == crate::config::SpatialEngine::Scroll {
            let moved = match self.scroll_axis_step(direction) {
                (true, step) => self.layout.scroll_move_column(&focused, step, false),
                (false, step) => self.layout.scroll_move_in_column(&focused, step),
            };
            if moved {
                self.scroll_columns_changed(&focused);
            }
            return;
        }
        let Some(current) = self
            .space
            .elements()
            .find(|w| is_window(w, &focused))
            .cloned()
        else {
            return;
        };
        let Some(neighbor) = self.neighbor_in_direction(&current, direction) else {
            return;
        };
        let Some(neighbor_surface) = neighbor.toplevel().map(|t| t.wl_surface().clone()) else {
            return;
        };
        if !self.layout.contains(&neighbor_surface) {
            // Only swap within the tiling tree; a floating window nearest
            // in that direction isn't a valid swap target.
            return;
        }

        self.layout.swap(&focused, &neighbor_surface);
        self.retile();
    }

    /// Finds the mapped window whose center lies nearest `from`'s center in
    /// `direction`. "Nearest" ranks candidates by distance along that
    /// direction's axis, with a penalty for how far off-axis they sit, so a
    /// neighbor roughly level with `from` wins over one that's technically
    /// closer in raw distance but well off to the side.
    pub(super) fn neighbor_in_direction(
        &self,
        from: &Window,
        direction: Direction,
    ) -> Option<Window> {
        let from_rect = self.space.element_geometry(from)?;

        self.space
            .elements()
            .filter(|w| *w != from)
            .filter_map(|w| {
                let score =
                    directional_score(from_rect, self.space.element_geometry(w)?, direction)?;
                Some((w.clone(), score))
            })
            .min_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(w, _)| w)
    }
}
