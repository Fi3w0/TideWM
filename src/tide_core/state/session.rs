use std::{collections::HashSet, time::Duration};

use smithay::{
    backend::renderer::{
        element::{
            default_primary_scanout_output_compare,
            solid::{SolidColorBuffer, SolidColorRenderElement},
            surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
            Id, Kind, RenderElementStates,
        },
        gles::GlesRenderer,
    },
    desktop::{
        self, layer_map_for_output,
        utils::{
            send_frames_surface_tree, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, update_surface_primary_scanout_output,
            OutputPresentationFeedback,
        },
    },
    input::pointer::CursorImageStatus,
    output::Output,
    reexports::{
        calloop::RegistrationToken,
        wayland_server::{backend::ClientId, protocol::wl_surface::WlSurface, Resource},
    },
    utils::SERIAL_COUNTER,
    wayland::{
        compositor::with_states,
        fractional_scale::with_fractional_scale,
        pointer_constraints::{with_pointer_constraint, PointerConstraint},
        session_lock::{LockSurface, SessionLocker},
    },
};

#[cfg(feature = "accessibility")]
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

#[cfg(feature = "accessibility")]
use crate::toast::ToastKind;

use super::{prefer_presentation_output, ClientState, LockRenderElement, SessionLock, Smallvil};

impl Smallvil {
    /// Whether the pointer sits under an active `wp-pointer-constraints-v1`
    /// lock right now. A locked pointer never receives absolute motion
    /// (`input.rs`'s motion handler skips the regular `motion()` call
    /// entirely once locked), so its on-screen position is frozen wherever
    /// the lock engaged -- rendering the system cursor glyph there just
    /// shows a static arrow sitting on top of whatever the client (a game)
    /// is drawing its own crosshair for. `cursor_position_hint`'s own doc
    /// comment already claims the system cursor is hidden during a lock;
    /// this is what actually makes that true.
    pub(crate) fn pointer_is_locked(&self) -> bool {
        let Some(pointer) = self.seat.get_pointer() else {
            return false;
        };
        let Some(focus) = pointer.current_focus() else {
            return false;
        };
        let Some(root) = self.root_with_constraint(&focus, &pointer) else {
            return false;
        };
        let mut locked = false;
        with_pointer_constraint(&root, &pointer, |c| {
            if let Some(c) = c {
                if c.is_active() {
                    locked = matches!(&*c, PointerConstraint::Locked(_));
                }
            }
        });
        locked
    }

    /// Which lock surface should own keyboard focus while locked: the
    /// pointer's current output's, falling back to any registered one --
    /// same focus-follows-mouse convention `focus_follows_mouse` already
    /// uses elsewhere. `None` while no lock surface has been registered for
    /// any output yet; `reconcile_keyboard_focus` then clears keyboard
    /// focus entirely rather than leaking it to a window.
    pub(super) fn lock_focus_target(&self) -> Option<WlSurface> {
        self.pointer_output()
            .and_then(|output| self.lock_surfaces.get(&output))
            .or_else(|| self.lock_surfaces.values().next())
            .map(|lock_surface| lock_surface.wl_surface().clone())
    }

    /// Hidden layers stop receiving callbacks so wallpaper clients can pause.
    /// A visible backdrop effect still samples lower layers even when its
    /// opaque capture hides them in the final frame; keep those clients live.
    pub fn send_layer_frames(
        &self,
        output: &Output,
        time: Duration,
        render_states: &RenderElementStates,
    ) {
        let backdrop_visible = self
            .render
            .gpu
            .backdrop_textures
            .values()
            .any(|capture| render_states.element_was_presented(capture.id.clone()));
        for layer in layer_map_for_output(output).layers() {
            // A role remains registered in LayerMap across null-buffer
            // unmap so it can be arranged for a later fresh configure, but
            // it is not eligible for frame callbacks while protocol-
            // unmapped. Filtering also prevents a bufferless client from
            // driving a commit/callback redraw loop.
            if self.unmapped_layer_surfaces.contains(layer.wl_surface()) {
                continue;
            }
            let mut visible = backdrop_visible;
            layer.with_surfaces(|surface, _| {
                visible |= render_states.element_was_presented(Id::from_wayland_resource(surface));
            });
            if !visible {
                continue;
            }
            layer.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    /// The lock-surface equivalent of `send_layer_frames`: a lock surface
    /// is a bare `wl_surface`, not a `desktop::Window`/`LayerSurface`
    /// wrapper, so it has no `.send_frame()` convenience of its own --
    /// `send_frames_surface_tree` is the primitive both of those wrap.
    /// Without this, a lock daemon that redraws on frame callbacks (an
    /// animated clock, an unlock-failure shake) would freeze after its
    /// first commit.
    pub fn send_lock_frames(&self, output: &Output, time: Duration) {
        if let Some(lock_surface) = self.lock_surfaces.get(output) {
            send_frames_surface_tree(
                lock_surface.wl_surface(),
                output,
                time,
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }

    /// Frame callbacks for the active drag'n'drop icon, a bare `wl_surface`
    /// like the lock surface above. Without them a client that animates its
    /// drag image (Firefox tab previews) would freeze after the first commit.
    pub fn send_dnd_icon_frames(&self, output: &Output, time: Duration) {
        if let Some(icon) = self.input.dnd_icon.as_ref() {
            send_frames_surface_tree(icon, output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    /// Entry point for `SessionLockHandler::lock` (`handlers/mod.rs`):
    /// cancels whatever interactive pointer/popup grab was live (grabs
    /// bypass `surface_under`-based routing entirely, so locking alone
    /// would not stop an in-progress window drag), resolves keyboard/
    /// pointer focus onto the lock surface (or nothing yet, if none is
    /// registered), and requests a redraw so every output blanks
    /// immediately. The actual `SessionLocker::lock()` confirmation only
    /// fires once every output has rendered a locked frame -- see
    /// `mark_output_locked_frame`.
    pub(crate) fn lock_session(&mut self, confirmation: SessionLocker) {
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            // Another client is already locking/locked. Dropping
            // `confirmation` here auto-sends `finished` to the new client,
            // matching the protocol's own documented policy.
            return;
        }

        self.session_lock_client = self
            .display_handle
            .get_client(confirmation.ext_session_lock().id())
            .ok()
            .map(|client| client.id());

        #[cfg(feature = "screencast")]
        self.screencast_picker.take();

        // A transition texture contains the unlocked desktop. Release it
        // before the fail-closed lock render path can begin.
        self.pending_workspace_transitions.clear();
        self.workspace_transitions.clear();
        self.workspace_glides.clear();
        self.workspace_scroll_cameras.clear();
        self.depth_transitions.clear();
        // Backdrop textures also contain unlocked client pixels. Unlock
        // recaptures visible glass before composing its first desktop frame.
        self.render.gpu.backdrop_textures.clear();
        self.render.gpu.layer_alpha_masks.clear();
        self.render.gpu.custom_shader_instances.clear();
        self.render.gpu.custom_shader_bypassed.clear();
        // Closing snapshots contain client pixels and normally render above
        // the desktop. They are irrelevant once the security boundary is
        // active and must never survive into a locked composition.
        self.closing_window_animations.clear();
        self.locked_outputs.clear();

        let serial = SERIAL_COUNTER.next_serial();
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.unset_grab(self, serial, smithay::backend::input::InputTime::now());
        }
        self.release_popup_grab();
        // If a minimap peek is open when the lock starts, drop it rather
        // than leaving it stranded: the peek's pointer short-circuit
        // (`process_input_event`'s `PointerMotion`/`PointerButton` arms)
        // checks `minimap_peek.is_some()` ahead of any lock handling, so a
        // still-open peek would keep consuming pointer input meant for the
        // lock surface until the trigger chord eventually releases.
        self.minimap_peek = None;

        if self.space.outputs().next().is_none() {
            // No displays attached -- nothing to render a locked frame on,
            // so there is nothing to wait for either.
            confirmation.lock();
            self.session_lock = SessionLock::Locked;
        } else {
            self.session_lock = SessionLock::Locking(confirmation);
        }

        self.reconcile_keyboard_focus(serial);
        self.refresh_pointer_focus();
        self.request_redraw();
    }

    /// Entry point for `SessionLockHandler::unlock`.
    pub(crate) fn unlock_session(&mut self) {
        self.session_lock = SessionLock::Unlocked;
        self.session_lock_client = None;
        self.lock_surfaces.clear();
        self.locked_outputs.clear();
        let serial = SERIAL_COUNTER.next_serial();
        self.reconcile_keyboard_focus(serial);
        self.refresh_pointer_focus();
        self.request_redraw();
    }

    pub(super) fn handle_client_disconnect(&mut self, client_id: ClientId) {
        self.discard_captures_for_client(&client_id);
        if let Some(sources) = self.dmabuf_blocker_sources.remove(&client_id) {
            self.dmabuf_blocker_source_count = self
                .dmabuf_blocker_source_count
                .saturating_sub(sources.len());
            for (token, _) in sources {
                self.loop_handle.remove(token);
            }
        }
        if self.session_lock_client.as_ref() == Some(&client_id)
            && !matches!(self.session_lock, SessionLock::Unlocked)
        {
            // Fail closed: ending the compositor returns control to the
            // display/login manager. Continuing with an unlocked desktop
            // would make killing the locker a lock bypass.
            tracing::error!(
                "Session-lock client crashed; terminating the compositor session securely"
            );
            self.session_lock_client = None;
            self.loop_signal.stop();
        }
    }

    pub(crate) fn track_dmabuf_blocker_source(
        &mut self,
        client_id: ClientId,
        surface: WlSurface,
        token: RegistrationToken,
    ) {
        self.dmabuf_blocker_sources
            .entry(client_id)
            .or_default()
            .push((token, surface));
        self.dmabuf_blocker_source_count = self.dmabuf_blocker_source_count.saturating_add(1);
    }

    pub(crate) fn untrack_dmabuf_blocker_source(
        &mut self,
        client_id: &ClientId,
        token: RegistrationToken,
    ) {
        let Some(sources) = self.dmabuf_blocker_sources.get_mut(client_id) else {
            return;
        };
        let previous_len = sources.len();
        sources.retain(|(candidate, _)| *candidate != token);
        self.dmabuf_blocker_source_count = self
            .dmabuf_blocker_source_count
            .saturating_sub(previous_len - sources.len());
        if sources.is_empty() {
            self.dmabuf_blocker_sources.remove(client_id);
        }
    }

    pub(crate) fn remove_surface_buffer_blockers(&mut self, surface: &WlSurface) {
        // A resource's client lookup can already fail during destruction.
        // The bounded watcher records retain the owning client identity.
        let tokens: Vec<_> = self
            .dmabuf_blocker_sources
            .iter()
            .flat_map(|(client, sources)| {
                sources
                    .iter()
                    .filter(|(_, owner)| owner == surface)
                    .map(|(token, _)| (client.clone(), *token))
            })
            .collect();
        for (client, token) in tokens {
            self.loop_handle.remove(token);
            self.untrack_dmabuf_blocker_source(&client, token);
            if let Ok(data) = self.display_handle.backend_handle().get_client_data(client) {
                if let Some(client_state) = data.downcast_ref::<ClientState>() {
                    let dh = self.display_handle.clone();
                    client_state.compositor_state.blocker_cleared(self, &dh);
                }
            }
        }
    }

    /// Entry point for `SessionLockHandler::new_surface`: configures the
    /// surface to fill `output` and registers it, then re-resolves focus --
    /// without this a lock surface registered after `lock_session` already
    /// ran (the common case: the client creates its lock surfaces only
    /// after seeing the manager global) would never receive
    /// `wl_keyboard.enter`, and password entry would be impossible.
    pub(crate) fn register_lock_surface(&mut self, output: Output, surface: LockSurface) {
        let Some(size) = self.space.output_geometry(&output).map(|geo| geo.size) else {
            return;
        };
        surface.with_pending_state(|state| {
            state.size = Some((size.w.max(0) as u32, size.h.max(0) as u32).into());
        });
        surface.send_configure();
        self.lock_surfaces.insert(output, surface);

        let serial = SERIAL_COUNTER.next_serial();
        self.reconcile_keyboard_focus(serial);
        self.refresh_pointer_focus();
        self.request_redraw();
    }

    /// Reconfigures an existing lock surface after a live output scale or
    /// transform change. Lock surfaces are compositor-sized, so leaving the
    /// old logical size advertised can expose an uncovered strip.
    pub(crate) fn refresh_lock_surface_geometry(&mut self, output: &Output) {
        let Some(size) = self.space.output_geometry(output).map(|geo| geo.size) else {
            return;
        };
        let Some(surface) = self.lock_surfaces.get(output) else {
            return;
        };
        with_states(surface.wl_surface(), |states| {
            with_fractional_scale(states, |scale| {
                scale.set_preferred_scale(output.current_scale().fractional_scale());
            });
        });
        surface.with_pending_state(|state| {
            state.size = Some((size.w.max(0) as u32, size.h.max(0) as u32).into());
        });
        surface.send_configure();
    }

    /// Closes and forgets all layer-shell state owned by a departing output.
    /// Destroy callbacks cannot find these surfaces after the output leaves
    /// `Space`, so disconnect must perform the cleanup first.
    pub(crate) fn remove_layer_output(&mut self, output: &Output) {
        self.native_render_stats.remove(output);
        self.spatial
            .workspaces
            .layout
            .scroll_section_requests
            .remove(&output.name());
        let layers: Vec<desktop::LayerSurface> =
            layer_map_for_output(output).layers().cloned().collect();
        if layers.is_empty() {
            return;
        }
        let mut map = layer_map_for_output(output);
        for layer in layers {
            let surface = layer.wl_surface().clone();
            layer.layer_surface().send_close();
            map.unmap_layer(&layer);
            self.unmapped_layer_surfaces.remove(&surface);
            self.render.gpu.backdrop_textures.remove(&surface);
            self.render.gpu.layer_alpha_masks.remove(&surface);
            self.forget_layer_focus(&surface);
        }
    }

    /// Called by both backends' render loops right after successfully
    /// presenting a locked (blanked, or real lock-surface) frame on
    /// `output`. No-op once already `Locked`, or while `Unlocked`.
    pub(crate) fn mark_output_locked_frame(&mut self, output: &Output) {
        if !matches!(self.session_lock, SessionLock::Locking(_)) {
            return;
        }
        self.locked_outputs.insert(output.clone());
        self.try_confirm_lock();
    }

    /// Drops all session-lock state owned by a disconnected output. If the
    /// session is still locking, removing an output can satisfy the remaining
    /// locked-frame requirement, including vacuously when the final output is
    /// gone.
    pub(crate) fn remove_lock_output(&mut self, output: &Output) {
        self.lock_surfaces.remove(output);
        self.lock_blank.remove(output);
        self.locked_outputs.remove(output);
        self.try_confirm_lock();
    }

    /// No-op unless `Locking`. Confirmation is normally driven by
    /// `mark_output_locked_frame` from the render loops, but an output
    /// hot-unplug removes a frame source entirely -- with zero surviving
    /// outputs no frame will ever arrive -- so `remove_lock_output` calls
    /// this directly after pruning its lock state. The predicate is
    /// vacuously true at zero outputs, matching niri's behavior: a lock
    /// with nothing left to show it is still confirmed locked.
    fn try_confirm_lock(&mut self) {
        let SessionLock::Locking(_) = &self.session_lock else {
            return;
        };
        if !self
            .space
            .outputs()
            .all(|o| self.locked_outputs.contains(o))
        {
            return;
        }
        let SessionLock::Locking(confirmation) =
            std::mem::replace(&mut self.session_lock, SessionLock::Locked)
        else {
            unreachable!()
        };
        confirmation.lock();
    }

    /// Render elements for whatever should be visible on `output` while
    /// `session_lock` isn't `Unlocked`: the registered lock surface's own
    /// content, if there is one, in front of a full-output blank fill that
    /// guarantees nothing underneath is ever visible -- even before a lock
    /// surface exists, or if it doesn't cover the whole output. Elements
    /// are front-to-back (index 0 topmost, same convention `custom_elements`
    /// uses elsewhere in both backends), so the surface must come *before*
    /// the blank in the returned `Vec`, not after -- reversed once during
    /// development, which put the opaque blank in front and silently
    /// occluded every lock surface. Shared by both backends, same pattern
    /// as `tab_strip_elements`.
    pub(crate) fn lock_render_elements(
        &mut self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Vec<LockRenderElement<GlesRenderer>> {
        let Some(size) = self.space.output_geometry(output).map(|geo| geo.size) else {
            return Vec::new();
        };
        let scale = output.current_scale().fractional_scale();

        let mut elements = Vec::new();
        // `layer_rule { above_lock_screen = true }` -- Hyprland's
        // `abovelock`. Pushed before the lock surface itself so it lands on
        // top of it (front-to-back, index 0 topmost, see this fn's own doc
        // comment). Render-only: `surface_under` still only ever hit-tests
        // the lock surface while locked, so this can't be used to route
        // input to whatever's behind the lock.
        {
            let layer_map = layer_map_for_output(output);
            for layer in layer_map.layers() {
                if self.unmapped_layer_surfaces.contains(layer.wl_surface())
                    || !self.config.layer_above_lock_screen(layer.namespace())
                {
                    continue;
                }
                let Some(geometry) = layer_map.layer_geometry(layer) else {
                    continue;
                };
                let surface_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    render_elements_from_surface_tree(
                        renderer,
                        layer.wl_surface(),
                        geometry.loc.to_physical_precise_round(scale),
                        scale,
                        1.0,
                        Kind::Unspecified,
                    );
                elements.extend(surface_elements.into_iter().map(LockRenderElement::Surface));
            }
        }
        if let Some(lock_surface) = self.lock_surfaces.get(output) {
            let surface_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer,
                    lock_surface.wl_surface(),
                    (0, 0),
                    scale,
                    1.0,
                    Kind::Unspecified,
                );
            elements.extend(surface_elements.into_iter().map(LockRenderElement::Surface));
        }

        let blank_buffer = self
            .lock_blank
            .entry(output.clone())
            .or_insert_with(|| SolidColorBuffer::new((0, 0), [0.0, 0.0, 0.0, 1.0]));
        blank_buffer.update(size, [0.0, 0.0, 0.0, 1.0]);
        let blank = SolidColorRenderElement::from_buffer(
            blank_buffer,
            (0, 0),
            scale,
            1.0,
            Kind::Unspecified,
        );
        elements.push(LockRenderElement::Blank(blank));

        elements
    }

    /// Send frame callbacks only to windows which this output actually rendered.
    pub(crate) fn send_window_frames(&self, output: &Output, time: Duration) {
        let Some(placements) = self.render_placements(output) else {
            return;
        };
        for placement in placements {
            placement
                .window
                .send_frame(output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
        }
    }

    /// Collects the wp_presentation feedback callbacks every visible
    /// window and layer surface on `output` registered since its last
    /// frame. The backend calls `presented` on the returned value once the
    /// frame carrying that content is actually on screen; dropping it
    /// without presenting discards every callback inside
    /// (`SurfacePresentationFeedback`'s own Drop), which is the correct
    /// answer for a frame that never reached the display. Modeled on
    /// anvil's helper of the same name.
    pub(crate) fn take_presentation_feedback(
        &self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);
        let placements = self.render_placements(output).unwrap_or_default();

        // Smithay's feedback collector requires primary-scanout state. Update
        // it from this frame, including windows absent from this output so an
        // old output is cleared when a window moves away (Anvil's pattern).
        // Include placement-only Ocean windows and visit each surface once.
        // Resource identity/hash is stable even though liveness is mutable.
        #[allow(clippy::mutable_key_type)]
        let mut visited = HashSet::new();
        for window in self
            .space
            .elements()
            .chain(placements.iter().map(|placement| &placement.window))
        {
            let Some(surface) = window.toplevel().map(|toplevel| toplevel.wl_surface()) else {
                continue;
            };
            if !visited.insert(surface.clone()) {
                continue;
            }
            let overlaps = self.space.outputs_for_element(window);
            window.with_surfaces(|surface, states| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    states,
                    None,
                    render_element_states,
                    |current, current_state, next, next_state| {
                        // A previous frame's visibility must not pin feedback
                        // to an output the window has already left. Ocean
                        // placement owns its camera/output mapping directly.
                        prefer_presentation_output(
                            current,
                            current_state,
                            next,
                            next_state,
                            self.spatial.engine.ownership()
                                == crate::config::SpatialOwnership::Workspaces
                                && overlaps.contains(current),
                        )
                    },
                );
            });
        }

        for placement in placements {
            placement.window.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        let map = layer_map_for_output(output);
        for layer in map.layers() {
            if self.unmapped_layer_surfaces.contains(layer.wl_surface()) {
                continue;
            }
            layer.with_surfaces(|surface, states| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    states,
                    None,
                    render_element_states,
                    |_, _, next, _| next,
                );
            });
            layer.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        feedback
    }

    pub(crate) fn send_native_dmabuf_feedback(
        &self,
        output: &Output,
        states: &RenderElementStates,
        render: &smithay::wayland::dmabuf::DmabufFeedback,
        scanout: &smithay::wayland::dmabuf::DmabufFeedback,
    ) {
        use smithay::{
            backend::renderer::element::utils::select_dmabuf_feedback,
            desktop::utils::send_dmabuf_feedback_surface_tree,
        };
        let select = |surface: &WlSurface, _: &smithay::wayland::compositor::SurfaceData| {
            select_dmabuf_feedback(surface, states, render, scanout)
        };
        // Presentation collection has just refreshed window/layer primary
        // output state from the actual frame, including decorated IDs.
        if matches!(self.session_lock, SessionLock::Unlocked) {
            for placement in self.render_placements(output).unwrap_or_default() {
                placement.window.send_dmabuf_feedback(
                    output,
                    surface_primary_scanout_output,
                    select,
                );
            }
            for layer in layer_map_for_output(output).layers() {
                if !self.unmapped_layer_surfaces.contains(layer.wl_surface()) {
                    layer.send_dmabuf_feedback(output, surface_primary_scanout_output, select);
                }
            }
        } else if let Some(lock) = self.lock_surfaces.get(output) {
            send_dmabuf_feedback_surface_tree(
                lock.wl_surface(),
                output,
                |_, _| Some(output.clone()),
                select,
            );
        }
        let cursor = match &self.input.cursor_status {
            CursorImageStatus::Surface(surface)
                if matches!(self.session_lock, SessionLock::Unlocked) =>
            {
                Some(surface)
            }
            _ => None,
        };
        for surface in cursor.into_iter().chain(
            self.input
                .dnd_icon
                .as_ref()
                .filter(|_| matches!(self.session_lock, SessionLock::Unlocked)),
        ) {
            smithay::wayland::compositor::with_surface_tree_downward(
                surface,
                (),
                |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
                |surface, data, _| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        data,
                        None,
                        states,
                        default_primary_scanout_output_compare,
                    );
                },
                |_, _, _| true,
            );
            send_dmabuf_feedback_surface_tree(
                surface,
                output,
                surface_primary_scanout_output,
                select,
            );
        }
    }

    #[cfg(feature = "accessibility")]
    pub(super) fn schedule_accessibility_sync(&mut self) {
        const COALESCE: Duration = Duration::from_millis(50);
        if self.accessibility.is_none() || self.accessibility_sync_timer_armed {
            return;
        }
        self.accessibility_sync_timer_armed = true;
        let result = self.loop_handle.insert_source(
            Timer::from_duration(COALESCE),
            move |_, _, state: &mut Smallvil| {
                state.accessibility_sync_timer_armed = false;
                state.sync_accessibility_tree();
                TimeoutAction::Drop
            },
        );
        if let Err(err) = result {
            self.accessibility_sync_timer_armed = false;
            tracing::warn!(%err, "Failed to schedule accessibility tree update");
            self.sync_accessibility_tree();
        }
    }

    /// Publishes compositor-owned UI to AccessKit. Building a small owned
    /// snapshot here keeps the adapter's worker threads completely detached
    /// from Smithay and Wayland object lifetimes.
    #[cfg(feature = "accessibility")]
    pub(crate) fn sync_accessibility_tree(&mut self) {
        if self.accessibility.is_none() {
            return;
        }

        let locked = !matches!(self.session_lock, SessionLock::Unlocked);
        let primary = self.primary_output().map(|output| {
            let output_name = output.name();
            let workspace = self
                .spatial
                .workspaces
                .layout
                .active_workspace(&output_name);
            (output_name, workspace)
        });
        let workspace_label = primary
            .as_ref()
            .map(|(_, workspace)| {
                self.config
                    .workspace_names
                    .iter()
                    .find_map(|(name, number)| (*number == *workspace).then(|| name.clone()))
                    .unwrap_or_else(|| workspace.to_string())
            })
            .unwrap_or_default();

        let workspace_name = |workspace: u32| {
            self.config
                .workspace_names
                .iter()
                .find_map(|(name, number)| (*number == workspace).then(|| name.clone()))
                .unwrap_or_else(|| workspace.to_string())
        };
        let overview_workspaces = self
            .overview
            .as_ref()
            .map(|overview| {
                overview
                    .workspaces()
                    .iter()
                    .map(|&workspace| (workspace, workspace_name(workspace)))
                    .collect()
            })
            .unwrap_or_default();

        let groups = primary
            .as_ref()
            .map(|(output_name, workspace)| {
                self.groups
                    .iter()
                    .filter(|group| &group.output == output_name && group.workspace == *workspace)
                    .map(|group| {
                        let id = group.ui_node_id;
                        crate::accessibility::GroupSnapshot {
                            id,
                            tabs: group
                                .members
                                .iter()
                                .map(|member| {
                                    (
                                        member.ui_node_id,
                                        crate::tab_strip::window_title(&member.surface),
                                    )
                                })
                                .collect(),
                            active: group.active,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let snapshot = crate::accessibility::UiSnapshot {
            workspace: workspace_label,
            locked,
            toast: self.toast.as_ref().map(|toast| {
                (
                    toast.message().to_owned(),
                    matches!(toast.kind(), ToastKind::Error),
                )
            }),
            config_error: self
                .config_error_overlay
                .as_ref()
                .map(|error| error.message().to_owned()),
            overview_workspaces,
            groups,
        };
        if let Some(accessibility) = self.accessibility.as_mut() {
            accessibility.update_ui(snapshot);
        }
    }

    /// Runs the udev backend's real DRM power hook (if any -- `None` under
    /// winit, logical-only there) for `output`, then updates
    /// `wlr_output_power_management_state`'s own tracking/broadcast on
    /// success. The one place that does both together, shared by the
    /// protocol's own `SetMode` handler and `toggle_dpms` below.
    fn set_output_power_state(&mut self, output: &Output, on: bool) -> bool {
        let applied = match &mut self.set_output_power {
            Some(hook) => hook(output, on),
            None => true,
        };
        if applied {
            if !on {
                let output_name = output.name();
                self.remove_workspace_transition_output(&output_name);
                self.remove_backdrop_output(&output_name);
            }
            self.wlr_output_power_management_state.set(output, on);
        }
        applied
    }

    /// `toggle-dpms` (Phase N tier 2): both niri (`power-off-monitors`) and
    /// Hyprland (`dpms`) treat this as an ordinary bindable WM action, not
    /// external-tool-only, even though `wlr-output-power-management-v1`
    /// already exposes the same control to a client like `wlopm`. Toggles
    /// every output together rather than just one -- if any output is
    /// currently on, this turns all of them off, otherwise turns all back
    /// on, so a multi-monitor setup doesn't end up in a mixed on/off state
    /// from one bind press.
    pub fn toggle_dpms(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let any_on = outputs
            .iter()
            .any(|output| self.wlr_output_power_management_state.is_on(output));
        let target_on = !any_on;
        for output in &outputs {
            self.set_output_power_state(output, target_on);
        }
    }

    pub(crate) fn activation_blocked(&self) -> bool {
        self.exclusive_layer().is_some() || !matches!(self.session_lock, SessionLock::Unlocked)
    }
}
