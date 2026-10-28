use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::{
        element::{
            solid::SolidColorBuffer, surface::render_elements_from_surface_tree,
            utils::RescaleRenderElement, Kind,
        },
        gles::{GlesRenderer, GlesTexProgram},
    },
    desktop::{space::SpaceRenderElements, PopupManager, Window},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Scale, Size, SERIAL_COUNTER},
};

use crate::config::Direction;

use super::{
    clamp_rect_visible, direction_delta, fit_floating_rect_visible, initial_floating_rect,
    is_window, keyboard_resized_size, monitor_resized_rect, nearest_point_in_output_rects,
    saturating_translate, stable_output_by_name, CompositorGesture, FloatBodyContext, FloatingTag,
    SessionLock, Smallvil, SnapPreview,
};

impl Smallvil {
    /// Gathers every fact `WindowRule::matches`/`matches_initial` need for
    /// one surface (identity, PID, XWayland-ness, current urgency, spawn-time
    /// identity, startup-window-ness) and folds the matching
    /// `rule { }` entries into one effective rule. The single
    /// chokepoint every other resolver in this file goes through, so a new
    /// match criterion only needs wiring here once instead of at each call
    /// site.
    pub(crate) fn resolve_window_rules_for(
        &self,
        surface: &WlSurface,
    ) -> crate::config::WindowRule {
        self.resolve_window_rules_on_workspace(surface, self.workspace_of_surface(surface))
    }

    /// Safe during a pointer grab: rules and committed hints require no
    /// pointer-handle query or backend/output assumption.
    pub(crate) fn floating_size_constraints_for(
        &self,
        surface: &WlSurface,
    ) -> crate::window_size::FloatingSizeConstraints {
        self.floating_size_constraints_with_rule(surface, &self.resolve_window_rules_for(surface))
    }

    fn floating_size_constraints_with_rule(
        &self,
        surface: &WlSurface,
        rule: &crate::config::WindowRule,
    ) -> crate::window_size::FloatingSizeConstraints {
        let (min, max) = smithay::wayland::compositor::with_states(surface, |states| {
            use smithay::wayland::shell::xdg::SurfaceCachedState;
            let mut guard = states.cached_state.get::<SurfaceCachedState>();
            let data = guard.current();
            (data.min_size, data.max_size)
        });
        crate::window_size::FloatingSizeConstraints::from_hints(min, max, rule)
    }

    pub(crate) fn enforce_floating_size_constraints(&mut self, surface: &WlSurface) {
        self.enforce_floating_size_constraints_with_rule(
            surface,
            &self.resolve_window_rules_for(surface),
        );
    }

    pub(super) fn enforce_floating_size_constraints_with_rule(
        &mut self,
        surface: &WlSurface,
        rule: &crate::config::WindowRule,
    ) {
        if !self.is_floating(surface)
            || self.fullscreen.contains_key(surface)
            || self.maximized.contains_key(surface)
            || crate::grabs::resize_grab::ongoing(surface)
        {
            return;
        }
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        let current = self
            .ocean
            .floating_rect(surface)
            .map(|rect| rect.size)
            .or_else(|| {
                self.floating_workspace
                    .get(surface)
                    .map(|tag| tag.rect.size)
            })
            .unwrap_or_else(|| window.geometry().size);
        let requested = window
            .toplevel()
            .and_then(|toplevel| toplevel.with_pending_state(|state| state.size))
            .unwrap_or(current);
        let constrained = self
            .floating_size_constraints_with_rule(surface, rule)
            .clamp(requested);
        if constrained != requested {
            self.apply_floating_placement(surface, None, Some((constrained.w, constrained.h)));
        }
    }

    pub(crate) fn connected_resize_handles(
        &self,
        hit: &crate::layout::SplitHit,
    ) -> Vec<crate::layout::SplitResizeHandle> {
        let config = self.config.vessels;
        let (falloff, max_splits) = if self.config.water_effects && config.enabled {
            (config.falloff, config.max_splits)
        } else {
            // The primary split is still returned, preserving ordinary BSP
            // resize when the water identity or this mechanic is disabled.
            (0.0, 1)
        };
        self.layout
            .connected_resize_handles(hit, falloff, max_splits)
    }

    pub(super) fn is_floating(&self, surface: &WlSurface) -> bool {
        self.floating_workspace.contains_key(surface) || self.ocean.floating_rect(surface).is_some()
    }

    /// Toggles ambient "sitting on water" sway for `surface`: once on, a
    /// continuous sine-wave offset (`float_physics::ambient_sample`, sampled
    /// fresh every frame in `base_window_visual_sample`) runs until toggled
    /// off again, independent of the drag/map/wave kick sources -- it never
    /// settles the way a kick does, by design. Floating windows only -- a
    /// tiled window has no offset render slot to sway in without visibly
    /// drifting off its own tile boundary, so this is a silent no-op on one.
    pub(crate) fn toggle_float_ambient(&mut self, surface: &WlSurface) {
        if !self.is_floating(surface) {
            return;
        }
        if self.physics.window_float_ambient.remove(surface).is_some() {
            self.request_redraw();
            return;
        }
        // The value is the time origin `ambient_sample`'s `elapsed` measures
        // from, not a due time -- there's nothing to schedule.
        self.physics
            .window_float_ambient
            .insert(surface.clone(), Instant::now());
        self.request_redraw();
    }

    /// Resolves one body's logical anchor, area-derived mass, and edge bounds
    /// once per outer tick for reuse across fixed substeps.
    pub(super) fn float_body_context(&self, surface: &WlSurface) -> Option<FloatBodyContext> {
        let rect = self.window_rect_for_kick(surface)?;
        let anchor = (
            rect.loc.x as f64,
            rect.loc.y as f64,
            rect.size.w as f64,
            rect.size.h as f64,
        );
        const MIN_MASS: f64 = 1.0;
        let mass = (anchor.2 * anchor.3).max(MIN_MASS);
        let output_bounds = match self.spatial.engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => {
                self.mapped_toplevel_window(surface).and_then(|window| {
                    let output = self.output_for_window(&window)?;
                    let geo = self.space.output_geometry(&output)?;
                    Some((
                        geo.loc.x as f64,
                        geo.loc.y as f64,
                        geo.size.w as f64,
                        geo.size.h as f64,
                    ))
                })
            }
            crate::config::SpatialOwnership::Ocean => self
                .ocean
                .entry_output(surface)
                .and_then(|name| self.output_by_name(name).map(|output| (name, output)))
                .and_then(|(name, output)| {
                    let viewport = self.space.output_geometry(&output)?.size;
                    Some(self.ocean.viewport_world_bounds(name, viewport))
                }),
        };
        Some(FloatBodyContext {
            surface: surface.clone(),
            anchor,
            mass,
            output_bounds,
        })
    }

    /// Marks the exact floating window being directly manipulated. Move
    /// grabs call this from their own motion/unset callbacks; it does not
    /// query the pointer or seat, avoiding the re-entrant grab deadlock class
    /// documented elsewhere in this state module.
    pub(crate) fn set_floating_dragging(&mut self, surface: &WlSurface, dragging: bool) {
        if dragging {
            self.floating_dragging.insert(surface.clone());
        } else {
            self.floating_dragging.remove(surface);
        }
        self.physics.buoyancy_dirty = true;
        self.request_redraw();
    }

    fn snap_eligible_surface(&self, surface: &WlSurface) -> bool {
        if self.spatial.engine.ownership() != crate::config::SpatialOwnership::Workspaces
            || !self.config.snap.enabled
            || !matches!(self.session_lock, SessionLock::Unlocked)
            || self.layout.contains(surface)
            || !self.floating_workspace.contains_key(surface)
            || self.fullscreen.contains_key(surface)
            || self.maximized.contains_key(surface)
        {
            return false;
        }
        self.window_is_visible(surface)
    }

    pub(crate) fn snap_target_at(
        &self,
        surface: &WlSurface,
        point: Point<f64, Logical>,
    ) -> Option<crate::snap::SnapTarget> {
        if !self.snap_eligible_surface(surface) {
            return None;
        }
        let output = self.output_for_point(point)?;
        let output_geometry = self.space.output_geometry(&output)?;
        let zone = crate::snap::zone_at(point, output_geometry, &self.config.snap)?;
        self.snap_target_for_zone(surface, &output, zone)
    }

    /// Resolve policy and geometry at the destination for both input paths.
    /// Safe inside pointer callbacks: output and coordinates come from the
    /// caller, never from a query of the pointer handle.
    fn snap_target_for_zone(
        &self,
        surface: &WlSurface,
        output: &Output,
        zone: crate::snap::SnapZone,
    ) -> Option<crate::snap::SnapTarget> {
        if !self.snap_eligible_surface(surface)
            || !crate::snap::zone_enabled(zone, &self.config.snap)
        {
            return None;
        }
        let output_name = output.name();
        let workspace = self.layout.active_workspace(&output_name);
        let rule = self.resolve_window_rules_on_workspace(surface, Some(workspace));
        // A per-window decision keeps precedence over the destination
        // workspace. Otherwise, crossing an output must honor the active
        // workspace there rather than the workspace where the grab began.
        if !rule
            .snap
            .or(self.config.resolve_workspace_rule(workspace).snap)
            .unwrap_or(true)
        {
            return None;
        }
        let area = self.output_tiling_area(output)?;
        let gap = self
            .config
            .snap
            .gap
            .unwrap_or_else(|| self.gaps_for(&output_name, workspace));
        Some(crate::snap::SnapTarget {
            output: output_name,
            workspace,
            zone,
            rect: crate::snap::constrained_target_rect(
                area,
                zone,
                gap,
                self.floating_size_constraints_with_rule(surface, &rule),
            ),
        })
    }

    pub(crate) fn set_snap_preview(&mut self, target: Option<&crate::snap::SnapTarget>) {
        if !self.config.snap.preview {
            if self.snap_preview.take().is_some() {
                self.request_redraw();
            }
            return;
        }
        let Some(target) = target else {
            if self.snap_preview.take().is_some() {
                self.request_redraw();
            }
            return;
        };
        let color = [
            self.config.snap.preview_color[0],
            self.config.snap.preview_color[1],
            self.config.snap.preview_color[2],
            1.0,
        ];
        match &mut self.snap_preview {
            Some(preview) => {
                preview.output.clone_from(&target.output);
                preview.rect = target.rect;
                preview.buffer.update(target.rect.size, color);
            }
            None => {
                self.snap_preview = Some(SnapPreview {
                    output: target.output.clone(),
                    rect: target.rect,
                    buffer: SolidColorBuffer::new(target.rect.size, color),
                });
            }
        }
        self.request_redraw();
    }

    /// Shows (or with `None` hides) a drag hint rectangle through the
    /// snap-preview element, in the snap preview's color and opacity.
    pub(crate) fn set_drag_preview(&mut self, preview: Option<(String, Rectangle<i32, Logical>)>) {
        let Some((output, rect)) = preview else {
            if self.snap_preview.take().is_some() {
                self.request_redraw();
            }
            return;
        };
        let color = [
            self.config.snap.preview_color[0],
            self.config.snap.preview_color[1],
            self.config.snap.preview_color[2],
            1.0,
        ];
        let unchanged = self
            .snap_preview
            .as_ref()
            .is_some_and(|p| p.output == output && p.rect == rect);
        if unchanged {
            return;
        }
        match &mut self.snap_preview {
            Some(preview) => {
                preview.output = output;
                preview.rect = rect;
                preview.buffer.update(rect.size, color);
            }
            None => {
                self.snap_preview = Some(SnapPreview {
                    output,
                    rect,
                    buffer: SolidColorBuffer::new(rect.size, color),
                });
            }
        }
        self.request_redraw();
    }

    pub(crate) fn apply_snap_target(
        &mut self,
        window: &Window,
        target: &crate::snap::SnapTarget,
    ) -> bool {
        let Some(surface) = window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
        else {
            return false;
        };
        let Some(output) = self.output_by_name(&target.output) else {
            return false;
        };
        // Revalidate against current config, workspace, and output geometry.
        let Some(target) = self.snap_target_for_zone(&surface, &output, target.zone) else {
            return false;
        };
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| state.size = Some(target.rect.size));
            toplevel.send_pending_configure();
        }
        self.set_window_fractional_scale(window, &output);
        self.space
            .map_element(window.clone(), target.rect.loc, false);
        if let Some(tag) = self.floating_workspace.get_mut(&surface) {
            tag.output.clone_from(&target.output);
            tag.workspace = target.workspace;
            tag.rect = target.rect;
        }
        self.refresh_window_opacity_and_glass_for(&surface);
        self.retarget_window_viscosity(&surface, target.rect);
        self.request_redraw();
        true
    }

    /// Scroll input follows the same animated rectangle as the rendered strip.
    /// Other engines retain their existing camera/input policy.
    pub(super) fn placement_input_rect(
        &self,
        placement: &crate::placement::PlacedWindow,
    ) -> Rectangle<i32, Logical> {
        if self.spatial.engine.projection() == Some(crate::config::WorkspaceProjection::Scroll) {
            let visual = self.placement_visual_sample(placement);
            Rectangle::new(
                placement.rect.loc
                    + Point::from((
                        visual.offset.x.round() as i32,
                        visual.offset.y.round() as i32,
                    )),
                visual.rounded_size_or(placement.rect.size),
            )
        } else {
            crate::placement::translated_rect(placement.rect, placement.view_offset)
        }
    }

    /// The output containing `pos`, if any. Overlap is resolved by stable
    /// output name so pointer ownership does not depend on `Space`'s map
    /// iteration order. Used for click, drag, focus, and placement targets.
    pub(crate) fn output_for_point(&self, pos: Point<f64, Logical>) -> Option<Output> {
        stable_output_by_name(self.space.output_under(pos)).cloned()
    }

    /// `Space::refresh` with TideWM's output ownership applied first, so a
    /// window never enters an output it is not drawn on. Use this instead
    /// of calling `self.space.refresh()` directly.
    pub(crate) fn refresh_space(&mut self) {
        for window in self.space.elements() {
            self.space
                .set_output_owner(window, self.space_output_owner(window).as_ref());
        }
        self.space.refresh();
    }

    /// The single output a window may enter, or `None` for geometric
    /// membership. Scroll tiles and strip floaters are positions on one
    /// output's strip: off-screen parts can overlap a neighbouring output's
    /// rectangle without ever being drawn there.
    fn space_output_owner(&self, window: &Window) -> Option<Output> {
        if self.spatial.engine.projection() != Some(crate::config::WorkspaceProjection::Scroll) {
            return None;
        }
        let surface = window.toplevel()?.wl_surface();
        let name = if self.layout.contains(surface) {
            self.layout.output_of(surface)?
        } else if self.scroll_strip_floater(surface) {
            self.floating_workspace.get(surface)?.output.as_str()
        } else {
            return None;
        };
        self.output_by_name(name)
    }

    /// Ocean's independent membership, or the output containing the largest
    /// share of the window's current presentation geometry.
    /// Output overlap storage inside Smithay is a HashMap, so taking its first
    /// entry made straddling-window ownership nondeterministic across runs.
    /// Ties use the stable output name. `None` if the window is not visible.
    pub(crate) fn output_for_window(&self, window: &Window) -> Option<Output> {
        if self.spatial.engine.projection() == Some(crate::config::WorkspaceProjection::Scroll) {
            if let Some(output) = window
                .toplevel()
                .and_then(|t| self.layout.output_of(t.wl_surface()))
                .and_then(|name| self.output_by_name(name))
            {
                return Some(output);
            }
        }
        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            let surface = window.toplevel()?.wl_surface();
            let owner = || {
                self.ocean
                    .entry_output(surface)
                    .and_then(|name| self.output_by_name(name))
            };
            // Space contains Ocean world coordinates, not desktop output
            // coordinates. Comparing those rectangles can change the
            // preferred scale on every commit and corrupt output affinity.
            return if self.config.ocean.shared_canvas {
                self.rendered_output_for_surface(surface).or_else(owner)
            } else {
                owner().or_else(|| self.rendered_output_for_surface(surface))
            };
        }
        let rect = self.space.element_geometry(window)?;
        self.space
            .outputs()
            .filter_map(|output| {
                let intersection = self.space.output_geometry(output)?.intersection(rect)?;
                let area = i64::from(intersection.size.w) * i64::from(intersection.size.h);
                Some((output.clone(), area))
            })
            .max_by(|(output_a, area_a), (output_b, area_b)| {
                area_a
                    .cmp(area_b)
                    .then_with(|| output_b.name().cmp(&output_a.name()))
            })
            .map(|(output, _)| output)
    }

    /// Resolves a spawn-only rule output. Persistent output pins deliberately
    /// bypass this helper: their value is always a real connector name.
    pub(crate) fn initial_rule_output(&self, name: &str) -> Option<Output> {
        match name {
            "focused" | "camera" | "current" => {
                self.focused_output().or_else(|| self.primary_output())
            }
            name => self.output_by_name(name),
        }
    }

    /// Chooses the output for an action with no stronger spatial hint. With
    /// focus-follows-mouse, prefer the pointer output unless an exclusive
    /// layer owns focus; otherwise prefer focused-window ownership.
    pub(crate) fn primary_output(&self) -> Option<Output> {
        let focused_output = self.focused_output();
        let pointer_output = self
            .seat
            .get_pointer()
            .and_then(|p| self.output_for_point(p.current_location()));
        let first_output = || self.space.outputs().next().cloned();
        let prefer_pointer =
            self.config.input.focus_follows_mouse && self.exclusive_layer().is_none();
        if prefer_pointer {
            pointer_output.or(focused_output).or_else(first_output)
        } else {
            focused_output.or(pointer_output).or_else(first_output)
        }
    }

    /// Commits a visible floating window's live Space placement back into
    /// its durable ownership tag after an interactive move. In particular,
    /// crossing an output transfers it to that output's active workspace;
    /// otherwise later workspace actions would still treat it as belonging
    /// to the output where the drag began.
    ///
    /// `dropped` is true only at the end of an interactive move. A
    /// strip-anchored Scroll floater otherwise keeps its output: scrolling
    /// can leave its rect mostly over a neighbor's coordinates without it
    /// being shown there, and a plain commit must not hand it over.
    pub(crate) fn sync_visible_floating_window(&mut self, window: &Window, dropped: bool) {
        let Some(surface) = window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
        else {
            return;
        };
        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            if self.fullscreen.contains_key(&surface)
                || self.maximized.contains_key(&surface)
                || self.ocean.floating_rect(&surface).is_none()
                || !self.window_is_visible(&surface)
            {
                return;
            }
            let Some(rect) = self.space.element_geometry(window) else {
                return;
            };
            self.ocean.set_floating_rect(&surface, rect);
            // Grab teardown already holds the pointer borrow; resolve output
            // from window geometry instead of calling `primary_output()`.
            if let Some(output) = self.output_for_window(window) {
                self.set_window_fractional_scale(window, &output);
                self.ocean.set_entry_output(&surface, output.name());
            }
            self.enforce_floating_size_constraints(&surface);
            return;
        }
        if self.fullscreen.contains_key(&surface)
            || self.maximized.contains_key(&surface)
            || !self.floating_workspace.contains_key(&surface)
            || !self.window_is_visible(&surface)
        {
            return;
        }
        let Some(rect) = self.space.element_geometry(window) else {
            return;
        };
        // Grab teardown holds the pointer mutex. If the window overlaps no
        // output, retain its live owner or use a stable remaining output;
        // primary_output() would re-enter that mutex through its fallback.
        let keep_strip_owner = !dropped && self.scroll_strip_floater(&surface);
        let owner = (!keep_strip_owner)
            .then(|| self.output_for_window(window))
            .flatten()
            .or_else(|| {
                self.floating_workspace
                    .get(&surface)
                    .and_then(|tag| self.output_by_name(&tag.output))
            })
            .or_else(|| stable_output_by_name(self.space.outputs()).cloned())
            .map(|output| {
                // The window may have just been dragged onto a different
                // output; its surfaces need to hear about that output's scale.
                self.set_window_fractional_scale(window, &output);
                let name = output.name();
                let workspace = self.layout.active_workspace(&name);
                (name, workspace)
            });

        // A strip floater is drawn only on its owner, so a drop straddling
        // two outputs would leave part of it clipped: slide it fully onto
        // the owner (as far as it fits).
        let mut rect = rect;
        if dropped && self.scroll_strip_floater(&surface) {
            if let Some(bounds) = owner
                .as_ref()
                .and_then(|(name, _)| self.output_by_name(name))
                .and_then(|output| self.space.output_geometry(&output))
            {
                let fit = |loc: i32, size: i32, origin: i32, extent: i32| {
                    loc.clamp(origin, (origin + extent - size).max(origin))
                };
                let loc = Point::from((
                    fit(rect.loc.x, rect.size.w, bounds.loc.x, bounds.size.w),
                    fit(rect.loc.y, rect.size.h, bounds.loc.y, bounds.size.h),
                ));
                if loc != rect.loc {
                    rect.loc = loc;
                    self.space.map_element(window.clone(), loc, false);
                }
            }
        }
        let Some(tag) = self.floating_workspace.get_mut(&surface) else {
            // The floating tag can vanish mid-commit if an unmap raced this
            // sync; never crash the compositor over a stale rect write.
            return;
        };
        tag.rect = rect;
        if let Some((output, workspace)) = owner {
            tag.output = output;
            tag.workspace = workspace;
        }
        self.refresh_window_opacity_and_glass_for(&surface);
    }

    /// Fits the focused Ocean floater to its live output minus configured
    /// gaps and live floating limits, preserving its world-space center.
    /// This action deliberately keeps its existing zoom-independent sizing.
    pub(crate) fn resize_to_monitor(&mut self) {
        if self.spatial.engine.ownership() != crate::config::SpatialOwnership::Ocean {
            return;
        }
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        if self.fullscreen.contains_key(&surface) || self.maximized.contains_key(&surface) {
            return;
        }
        let Some(current) = self.ocean.floating_rect(&surface) else {
            return;
        };
        let Some(output_name) = self.ocean.entry_output(&surface).map(str::to_string) else {
            return;
        };
        let Some(output) = self.output_by_name(&output_name) else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let rect = monitor_resized_rect(
            current,
            output_geo.size,
            self.config.gaps,
            self.floating_size_constraints_for(&surface),
        );
        self.apply_floating_placement(
            &surface,
            Some((rect.loc.x, rect.loc.y)),
            Some((rect.size.w, rect.size.h)),
        );
        self.retile();
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
    }

    /// `Action::SwitchWorkspace`'s Ocean branch for a numbered ref: centers
    /// the camera on and focuses `index`'s (1-based) app-slot window --
    /// "like focus but faster" than panning manually. No apps open at all
    /// jumps to the world origin specifically, not any particular reef or
    /// bookmark. A slot number past however many apps are actually open is
    /// a no-op, the same way focusing an empty workspace wouldn't do
    /// anything either.
    pub(crate) fn jump_to_app_slot(&mut self, output: &Output, index: usize) {
        let Some(viewport) = self.space.output_geometry(output).map(|geo| geo.size) else {
            return;
        };
        let animation = Duration::from_millis(self.config.ocean.camera_animation_ms);
        match self
            .ocean
            .app_slot_on_output(&output.name(), index)
            .cloned()
        {
            Some(surface) => {
                if let Some(rect) =
                    self.ocean
                        .world_rect(&surface, self.config.gaps, self.config.bsp_split_bias)
                {
                    self.ocean.center_on_rect(
                        &output.name(),
                        viewport,
                        rect,
                        animation,
                        self.config.ocean.camera_sway,
                    );
                }
                self.focus_window(Some(surface.clone()), SERIAL_COUNTER.next_serial());
                self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged { surface });
                self.request_redraw();
            }
            None if !self.ocean.has_open_apps() => {
                self.ocean.animate_to_point(
                    &output.name(),
                    crate::ocean::OceanPoint::default(),
                    animation,
                    self.config.ocean.camera_sway,
                );
                self.request_redraw();
            }
            None => {}
        }
    }

    /// Translates every floating window tagged to `output_name` by `delta`
    /// -- called when wlr-output-management (kanshi, wdisplays, ...) moves
    /// an output's logical position. `retile()` already repositions tiled
    /// windows for free (their tree is recomputed against the output's
    /// fresh area on the very next call); floating windows have no such
    /// automatic step, so without this they'd keep their old absolute
    /// coordinates after the output moved out from under them -- possibly
    /// landing on a different output, or off-screen entirely -- even
    /// though the client requesting the move was told it succeeded.
    ///
    /// Mirrors `swap_workspaces`' own visible-vs-hidden handling: a visible
    /// window's real position lives in `space` right now (`tag.rect` is
    /// stale until the next hide, same as `sync_visible_floating_window`'s
    /// doc comment explains), so that's what gets read and re-mapped; a
    /// hidden window has no `space` presence to read, so `tag.rect` itself
    /// -- the only place its position survives -- is what moves.
    pub(crate) fn translate_floating_windows_on_output(
        &mut self,
        output_name: &str,
        delta: Point<i32, Logical>,
    ) {
        let surfaces: Vec<WlSurface> = self
            .floating_workspace
            .iter()
            .filter(|(_, tag)| tag.output == output_name)
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in surfaces {
            self.translate_floating_window(&surface, delta);
        }
    }

    /// Moves one floating window by `delta`, live position if visible,
    /// remembered one if hidden; see `translate_floating_windows_on_output`.
    pub(super) fn translate_floating_window(
        &mut self,
        surface: &WlSurface,
        delta: Point<i32, Logical>,
    ) {
        let surface = surface.clone();
        let output_override =
            self.fullscreen.contains_key(&surface) || self.maximized.contains_key(&surface);
        if output_override {
            // The live Space rect is the fullscreen/maximized override,
            // not windowed geometry. Retile will derive that override
            // again from the output's new live geometry; only translate
            // the durable windowed restore records here.
            if let Some(tag) = self.floating_workspace.get_mut(&surface) {
                tag.rect.loc = saturating_translate(tag.rect.loc, delta);
            }
        } else if self.window_is_visible(&surface) {
            let window = self
                .floating_workspace
                .get(&surface)
                .unwrap()
                .window
                .clone();
            let Some(mut rect) = self.space.element_geometry(&window) else {
                return;
            };
            rect.loc = saturating_translate(rect.loc, delta);
            self.space.map_element(window, rect.loc, false);
            self.floating_workspace.get_mut(&surface).unwrap().rect = rect;
        } else if let Some(tag) = self.floating_workspace.get_mut(&surface) {
            tag.rect.loc = saturating_translate(tag.rect.loc, delta);
        }
        if let Some(entry) = self.fullscreen.get_mut(&surface) {
            if let Some(rect) = &mut entry.restore_rect {
                rect.loc = saturating_translate(rect.loc, delta);
            }
        }
        if let Some(entry) = self.maximized.get_mut(&surface) {
            entry.restore_rect.loc = saturating_translate(entry.restore_rect.loc, delta);
        }
    }

    pub(super) fn window_at_layout_position(&self, pos: Point<f64, Logical>) -> Option<WlSurface> {
        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            return self.window_under(pos).and_then(|(window, _)| {
                window
                    .toplevel()
                    .map(|toplevel| toplevel.wl_surface().clone())
            });
        }
        let live = self.space.element_under(pos).and_then(|(window, _)| {
            window
                .toplevel()
                .map(|toplevel| toplevel.wl_surface().clone())
        });
        if live.as_ref().is_some_and(|surface| {
            !self.layout.contains(surface) || self.fullscreen.contains_key(surface)
        }) {
            return live;
        }

        let tiled = self.output_for_point(pos).and_then(|output| {
            let workspace = self.layout.active_workspace(&output.name());
            let area = self.output_tiling_area(&output)?;
            self.layout
                .layout(
                    &output.name(),
                    workspace,
                    area,
                    self.gaps_for(&output.name(), workspace),
                )
                .into_iter()
                .find(|(_, rect)| rect.contains(pos.to_i32_round()))
                .and_then(|(window, _)| {
                    window
                        .toplevel()
                        .map(|toplevel| toplevel.wl_surface().clone())
                })
        });
        tiled.or(live)
    }

    /// Popups remain unclipped above compositor decoration; only the
    /// toplevel's own surface/subsurface tree is rounded.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn window_surface_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        window: &Window,
        surface: &WlSurface,
        render_location: Point<i32, Logical>,
        visual_size: Size<i32, Logical>,
        alpha: f32,
        rounded_program: Option<GlesTexProgram>,
        floating: bool,
        fullscreen: bool,
    ) -> (
        Vec<crate::backend::udev::OutputRenderElements>,
        Vec<crate::backend::udev::OutputRenderElements>,
    ) {
        let output_scale = output.current_scale().fractional_scale();
        let scale = Scale::from(output_scale);
        let physical_location = render_location.to_physical_precise_round(output_scale);
        let natural_size = window.geometry().size;
        let resize_scale = Scale::from((
            visual_size.w as f64 / natural_size.w.max(1) as f64,
            visual_size.h as f64 / natural_size.h.max(1) as f64,
        ));
        let resize_active =
            (resize_scale.x - 1.0).abs() > 0.0001 || (resize_scale.y - 1.0).abs() > 0.0001;
        let physical_anchor =
            (render_location + window.geometry().loc).to_physical_precise_round(output_scale);
        let mut popups = Vec::new();
        for (popup, popup_offset) in PopupManager::popups_for_surface(surface) {
            let offset = (window.geometry().loc + popup_offset - popup.geometry().loc)
                .to_physical_precise_round(scale);
            for element in render_elements_from_surface_tree(
                renderer,
                popup.wl_surface(),
                physical_location + offset,
                scale,
                alpha,
                Kind::Unspecified,
            ) {
                if resize_active {
                    popups.push(crate::backend::udev::OutputRenderElements::AnimatedSurface(
                        RescaleRenderElement::from_element(element, physical_anchor, resize_scale),
                    ));
                } else {
                    popups.push(crate::backend::udev::OutputRenderElements::Space(
                        SpaceRenderElements::Surface(element),
                    ));
                }
            }
        }

        let rounding = self.rounding_config_for_surface(surface);
        let clip = self.rounding_applies(&rounding, floating, fullscreen);
        let physical_rect = Rectangle::new(render_location + window.geometry().loc, natural_size)
            .to_physical_precise_round(output_scale);
        let radii = rounding.radii.map(|radius| radius * output_scale as f32);
        let raw_main = render_elements_from_surface_tree(
            renderer,
            surface,
            physical_location,
            scale,
            alpha,
            Kind::Unspecified,
        );
        let cascade = self
            .cascade_window_animations
            .get(surface)
            .map(crate::cascade_transition::CascadeTransition::sample);
        let retain_close_snapshot = (self.config.animations.enabled
            && self.config.animations.close.enabled)
            || (self.cascade_liquid_applies(surface)
                && self.config.cascade.drain != crate::config::CascadeLiquidPreset::None);
        if retain_close_snapshot {
            if let Some(snapshot) = crate::window_animation::WindowFrameSnapshot::capture(
                output.name(),
                physical_anchor,
                physical_rect,
                scale,
                resize_scale,
                &raw_main,
            ) {
                self.window_frame_snapshots
                    .insert(surface.clone(), snapshot);
            }
        }
        let mut main = Vec::with_capacity(raw_main.len());
        for element in raw_main {
            let surface_id = smithay::backend::renderer::element::Element::id(&element).clone();
            if let (true, Some(program)) = (clip || cascade.is_some(), rounded_program.clone()) {
                let rounded = crate::decoration::RoundedSurfaceElement::new(
                    element,
                    program,
                    physical_rect,
                    if clip { radii } else { [0.0; 4] },
                    rounding.power,
                    rounding.antialias * output_scale as f32,
                    scale,
                    cascade,
                );
                if resize_active {
                    main.push(
                        crate::backend::udev::OutputRenderElements::AnimatedRoundedSurface(
                            crate::backend::presentation::SurfaceRenderIdentity::new(
                                RescaleRenderElement::from_element(
                                    rounded,
                                    physical_anchor,
                                    resize_scale,
                                ),
                                surface_id,
                            ),
                        ),
                    );
                } else {
                    main.push(crate::backend::udev::OutputRenderElements::RoundedSurface(
                        crate::backend::presentation::SurfaceRenderIdentity::new(
                            rounded, surface_id,
                        ),
                    ));
                }
            } else if resize_active {
                main.push(crate::backend::udev::OutputRenderElements::AnimatedSurface(
                    RescaleRenderElement::from_element(element, physical_anchor, resize_scale),
                ));
            } else {
                main.push(crate::backend::udev::OutputRenderElements::Space(
                    SpaceRenderElements::Surface(element),
                ));
            }
        }
        (popups, main)
    }

    pub(crate) fn tiled_rect_for_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<Rectangle<i32, Logical>> {
        let output_name = self.layout.output_of(surface)?;
        let workspace = self.layout.workspace_of(surface)?;
        let output = self.output_by_name(output_name)?;
        let area = self.output_tiling_area(&output)?;
        let rect = self
            .layout
            .layout(
                output_name,
                workspace,
                area,
                self.gaps_for(output_name, workspace),
            )
            .into_iter()
            .find_map(|(window, rect)| is_window(&window, surface).then_some(rect))?;
        Some(if self.pseudo_tiled.contains(surface) {
            crate::layout::scale_centered(rect, self.config.pseudo_tile_scale)
        } else {
            rect
        })
    }

    /// Clamps `pos` into the union of every mapped output. Needed for
    /// relative pointer motion (a real mouse's delta accumulates onto the
    /// last known position with nothing else bounding it) -- absolute motion
    /// doesn't need this, it's already transformed against one output's
    /// geometry directly. Rectangle right/bottom edges are exclusive, so
    /// the nearest valid representable point is used when motion reaches an
    /// outer edge or a gap in an L-shaped layout.
    pub(crate) fn clamp_to_outputs(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        nearest_point_in_output_rects(
            pos,
            self.space
                .outputs()
                .filter_map(|output| self.space.output_geometry(output)),
        )
        .unwrap_or(pos)
    }

    /// Recomputes the tiling layout for every output and applies it: sends
    /// each tiled window its new size (`send_pending_configure`) and
    /// updates its position in `space`. Position takes effect immediately;
    /// visible size catches up once the client commits a matching buffer,
    /// same one-frame-lag tradeoff `resize_grab` already makes.
    ///
    /// Each output tiles independently -- see `layout::Layouts` -- so this
    /// just loops every mapped output and applies that output's own tree
    /// into that output's own (exclusive-zone-adjusted) area.
    ///
    /// A fullscreen window (`self.fullscreen`) keeps its `Layouts` slot the
    /// whole time it's fullscreen -- see `handlers/xdg_shell.rs` -- so this
    /// is also where that's reconciled: its slot's rect gets overridden to
    /// the *full* output geometry (ignoring gaps and exclusive zones, unlike
    /// every other rect here) rather than removed from the tree and
    /// reinserted later. A pseudo-tiled window (`self.pseudo_tiled`, see
    /// `toggle_pseudo_tile`) gets the same treatment on a smaller scale:
    /// its slot's rect is shrunk to `config.pseudo_tile_scale` of itself,
    /// centered, rather than filling the tile. Fullscreen always wins if a
    /// window is somehow both.
    pub fn retile(&mut self) {
        self.retile_with_viscosity(false, None);
    }

    /// Interactive tiled resize/drop path. It shares every layout and
    /// protocol-state operation with `retile`, changing only the short-lived
    /// render follower chosen for geometry that moved under the pointer.
    pub(crate) fn retile_viscous(&mut self) {
        self.retile_with_viscosity(true, None);
    }

    /// Same as `retile`, but withholds the tiled-size configure for one
    /// surface. `map_toplevel` calls this instead of `retile` when a
    /// window rule (float/pin/maximize/pseudo-tile/the auto-float
    /// heuristic) is about to immediately re-tile it again: without this,
    /// the client received a tiled-size configure here and a
    /// differently-sized one moments later from that conversion, and a
    /// terminal visibly re-flows its text grid between the two. Bookkeeping
    /// (`space.map_element`, staged pending state) still runs as normal --
    /// `toggle_floating`/`toggle_pseudo_tile` need the former to find the
    /// window at all, and the latter is harmless since the conversion's own
    /// `with_pending_state` call overwrites it before anything is sent.
    pub(crate) fn retile_skip_first_configure(&mut self, skip: &WlSurface) {
        self.retile_with_viscosity(false, Some(skip));
    }

    pub(super) fn retile_filtered(
        &mut self,
        interactive: bool,
        skip_configure_for: Option<&WlSurface>,
        scoped_output: Option<&str>,
    ) {
        self.layout.scroll.config = (self.spatial.engine.projection()
            == Some(crate::config::WorkspaceProjection::Scroll))
        .then_some(self.config.scroll);
        self.layout.scroll.fullscreen_extents = if self.spatial.engine.projection()
            == Some(crate::config::WorkspaceProjection::Scroll)
        {
            self.fullscreen
                .iter()
                .filter_map(|(surface, entry)| {
                    if !self.layout.contains(surface) {
                        return None;
                    }
                    let output = self.output_by_name(&entry.output)?;
                    let geometry = self.space.output_geometry(&output)?;
                    let extent = match self.config.scroll.orientation {
                        crate::scroll::Orientation::Horizontal => geometry.size.w,
                        crate::scroll::Orientation::Vertical => geometry.size.h,
                    };
                    Some((surface.clone(), f64::from(extent)))
                })
                .collect()
        } else {
            HashMap::new()
        };
        self.physics.buoyancy_dirty = true;
        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            self.retile_ocean(skip_configure_for);
            return;
        }
        let outputs: Vec<Output> = self
            .space
            .outputs()
            .filter(|output| scoped_output.is_none_or(|name| output.name() == name))
            .cloned()
            .collect();
        for output in &outputs {
            let Some(area) = self.output_tiling_area(output) else {
                continue;
            };
            let full_output_geo = self.space.output_geometry(output);
            let workspace = self.layout.active_workspace(&output.name());

            let gaps = self.gaps_for(&output.name(), workspace);
            self.layout
                .prepare_scroll(&output.name(), workspace, area, gaps);
            self.anchor_scroll_floaters(&output.name(), workspace);
            self.rescue_scroll_floaters(&output.name(), workspace, area, gaps);
            let window_count = self.layout.window_count(&output.name(), workspace);
            let target_aspect = area.size.w as f32 / (area.size.h as f32).max(1.0);
            self.layout.refresh_cascade_state(
                &output.name(),
                workspace,
                window_count,
                target_aspect,
            );
            for (window, mut rect) in self.layout.layout(&output.name(), workspace, area, gaps) {
                if let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) {
                    if let (Some(entry), Some(full)) =
                        (self.fullscreen.get(&surface), full_output_geo)
                    {
                        if entry.output == output.name() {
                            if self.spatial.engine.projection()
                                == Some(crate::config::WorkspaceProjection::Scroll)
                            {
                                let gap = gaps
                                    .max(0)
                                    .min(area.size.w.min(area.size.h).saturating_sub(1) / 2);
                                let translation = match self.config.scroll.orientation {
                                    crate::scroll::Orientation::Horizontal => (
                                        i64::from(rect.loc.x)
                                            - i64::from(area.loc.x)
                                            - i64::from(gap),
                                        0,
                                    ),
                                    crate::scroll::Orientation::Vertical => (
                                        0,
                                        i64::from(rect.loc.y)
                                            - i64::from(area.loc.y)
                                            - i64::from(gap),
                                    ),
                                };
                                let x = (i64::from(full.loc.x) + translation.0).clamp(
                                    i64::from(i32::MIN),
                                    i64::from(i32::MAX - full.size.w.max(1)),
                                ) as i32;
                                let y = (i64::from(full.loc.y) + translation.1).clamp(
                                    i64::from(i32::MIN),
                                    i64::from(i32::MAX - full.size.h.max(1)),
                                ) as i32;
                                rect = Rectangle::new((x, y).into(), full.size);
                            } else {
                                rect = full;
                            }
                        }
                    } else if self.pseudo_tiled.contains(&surface) {
                        rect = crate::layout::scale_centered(rect, self.config.pseudo_tile_scale);
                    }
                }
                tracing::trace!(?rect, output = output.name(), "Tiling window");
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.size = Some(rect.size);
                    });
                    if skip_configure_for != Some(toplevel.wl_surface()) {
                        toplevel.send_pending_configure();
                    }
                    // Remember the size this tile promises the client, so
                    // `note_tiled_size_refusal` can tell a client that is
                    // still catching up to it from one that will never take
                    // it. A changed expected size also resets any running
                    // refusal streak: a fresh configure deserves a fresh
                    // grace period.
                    let surface = toplevel.wl_surface().clone();
                    let changed = self
                        .expected_tiled_size
                        .insert(surface.clone(), rect.size)
                        .is_some_and(|old| old != rect.size);
                    if changed {
                        self.tiled_size_refusals.remove(&surface);
                    }
                }
                if let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) {
                    if let Some(old_location) = self.space.element_location(&window) {
                        let old_rect = Rectangle::new(old_location, window.geometry().size);
                        if old_rect != rect {
                            let viscous = (interactive
                                || self.physics.window_viscosity.contains_key(&surface))
                                && self.retarget_window_viscosity(&surface, rect);
                            if matches!(
                                self.compositor_gesture,
                                Some(CompositorGesture::Scroll { .. })
                            ) {
                                self.window_move_animations.remove(&surface);
                            } else if !viscous {
                                self.start_window_move_animation(&surface, old_rect, rect);
                            }
                        }
                    }
                }
                self.set_window_fractional_scale(&window, output);
                self.space.map_element(window, rect.loc, false);
            }

            // A *floating* fullscreen window isn't in `self.layout` at all,
            // so the loop above never sees it -- map it to the full output
            // geometry directly.
            if let Some(full) = full_output_geo {
                let floating_fullscreen: Vec<Window> = self
                    .space
                    .elements()
                    .filter(|w| {
                        w.toplevel().is_some_and(|t| {
                            let surface = t.wl_surface();
                            !self.layout.contains(surface)
                                && self
                                    .fullscreen
                                    .get(surface)
                                    .is_some_and(|e| e.output == output.name())
                        })
                    })
                    .cloned()
                    .collect();
                for window in floating_fullscreen {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|state| {
                            state.states.set(xdg_toplevel::State::Fullscreen);
                            state.states.unset(xdg_toplevel::State::Maximized);
                            state.states.unset(xdg_toplevel::State::Resizing);
                            state.size = Some(full.size);
                        });
                        toplevel.send_pending_configure();
                    }
                    self.set_window_fractional_scale(&window, output);
                    self.space.map_element(window, full.loc, false);
                }
            }

            // Floating maximized windows also live outside Layouts. Keep
            // them reconciled to the current non-exclusive tiling area so a
            // bar/output geometry change cannot leave stale size/location.
            let maximized_rect = crate::layout::inset(area, gaps);
            let floating_maximized: Vec<Window> = self
                .space
                .elements()
                .filter(|window| {
                    window.toplevel().is_some_and(|toplevel| {
                        let surface = toplevel.wl_surface();
                        !self.layout.contains(surface)
                            && !self.fullscreen.contains_key(surface)
                            && self
                                .maximized
                                .get(surface)
                                .is_some_and(|entry| entry.output == output.name())
                    })
                })
                .cloned()
                .collect();
            for window in floating_maximized {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.states.set(xdg_toplevel::State::Maximized);
                        state.states.unset(xdg_toplevel::State::Fullscreen);
                        state.states.unset(xdg_toplevel::State::Resizing);
                        state.size = Some(maximized_rect.size);
                    });
                    toplevel.send_pending_configure();
                }
                self.set_window_fractional_scale(&window, output);
                self.space.map_element(window, maximized_rect.loc, false);
            }

            // Floating ownership survives inactive workspaces, unlike
            // `Space`. Reconcile every tagged window so hidden floaters and
            // saved fullscreen/maximize geometry cannot retain a stale scale
            // or become unreachable after the logical output shrinks.
            if let Some(full) = full_output_geo {
                let floating: Vec<(WlSurface, Window)> = self
                    .floating_workspace
                    .iter()
                    .filter(|(_, tag)| tag.output == output.name())
                    .map(|(surface, tag)| (surface.clone(), tag.window.clone()))
                    .collect();
                for (surface, window) in floating {
                    // A shown window takes the scale of the output it is
                    // actually displayed on. A pinned window's tag can
                    // still name the output it was pinned on (see
                    // `toggle_pin`), and using it made a pinned window on
                    // another monitor flip to that monitor's scale on
                    // every retile -- visible as content briefly drawn
                    // too small during workspace switches.
                    let scale_output = self
                        .output_for_window(&window)
                        .unwrap_or_else(|| output.clone());
                    self.set_window_fractional_scale(&window, &scale_output);
                    let bounds = self.floating_bounds(&surface, full);

                    if let Some(tag) = self.floating_workspace.get_mut(&surface) {
                        tag.rect = clamp_rect_visible(tag.rect, bounds);
                    }
                    if let Some(entry) = self.fullscreen.get_mut(&surface) {
                        if let Some(rect) = &mut entry.restore_rect {
                            *rect = clamp_rect_visible(*rect, full);
                        }
                    }
                    if let Some(entry) = self.maximized.get_mut(&surface) {
                        entry.restore_rect = clamp_rect_visible(entry.restore_rect, full);
                    }

                    if !self.fullscreen.contains_key(&surface)
                        && !self.maximized.contains_key(&surface)
                        && self.window_is_visible(&surface)
                    {
                        let current = self
                            .space
                            .element_geometry(&window)
                            .or_else(|| self.floating_workspace.get(&surface).map(|tag| tag.rect));
                        if let Some(current) = current {
                            let clamped = clamp_rect_visible(current, bounds);
                            if let Some(tag) = self.floating_workspace.get_mut(&surface) {
                                tag.rect = clamped;
                            }
                            if clamped.loc != current.loc {
                                self.space.map_element(window, clamped.loc, false);
                            }
                        }
                    }
                }
            }
        }

        // Space::map_element always re-raises the element it touches (even
        // with activate: false), so the loop above just knocked every
        // floating window behind the tiled layer it re-stacked. Floating
        // windows should stay on top, normal WM convention, so restore that
        // (preserving floating windows' own relative order) every time.
        let floating: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                w.toplevel()
                    .map(|t| !self.layout.contains(t.wl_surface()))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for window in floating {
            self.space.raise_element(&window, false);
        }

        // Same invariant one level up: a fullscreen window (tiled or
        // floating) should sit above every other window on its output,
        // including floating ones, so it needs its own re-raise pass after
        // the one above.
        let fullscreen: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                w.toplevel()
                    .is_some_and(|t| self.fullscreen.contains_key(t.wl_surface()))
            })
            .cloned()
            .collect();
        for window in fullscreen {
            self.space.raise_element(&window, false);
        }

        self.queue_scroll_pointer_refresh();
        #[cfg(debug_assertions)]
        self.assert_state_invariants();
        self.request_redraw();
    }

    #[cfg(debug_assertions)]
    fn assert_state_invariants(&self) {
        for surface in &self.pinned {
            debug_assert!(
                self.floating_workspace.contains_key(surface),
                "pinned window must remain floating"
            );
            debug_assert!(
                !self.fullscreen.contains_key(surface),
                "fullscreen must suspend actual pin membership"
            );
        }
        for surface in &self.pseudo_tiled {
            debug_assert!(
                self.layout.contains(surface),
                "pseudo-tiled window must remain in Layouts"
            );
        }
        for (surface, maximized) in &self.maximized {
            debug_assert!(
                self.floating_workspace.contains_key(surface),
                "maximized restore state only applies to floating windows"
            );
            debug_assert_eq!(
                self.floating_workspace.get(surface).map(|tag| &tag.output),
                Some(&maximized.output),
                "maximized output must match floating ownership"
            );
        }

        for surface in self.foreign_toplevels.keys() {
            debug_assert!(
                !self.unmapped_toplevels.contains_key(surface),
                "foreign-toplevel handle must not outlive the mapped state"
            );
        }

        let mut fullscreen_outputs = HashSet::new();
        for (surface, entry) in &self.fullscreen {
            debug_assert!(
                fullscreen_outputs.insert(&entry.output),
                "at most one fullscreen window may own an output"
            );
            if !self.unmapped_toplevels.contains_key(surface) {
                if let Some(owner) = self.preferred_output_for_toplevel(surface) {
                    debug_assert_eq!(
                        owner, entry.output,
                        "fullscreen output must match mapped ownership"
                    );
                }
            }
        }
    }

    /// Toggles `surface` between tiled and floating. A window not tracked by
    /// `self.layout` is, by definition, floating (this is also what gates
    /// `move_request`/`resize_request` in `handlers/xdg_shell.rs`), so there's
    /// no separate floating-window set to keep in sync.
    ///
    /// Untiling keeps the window's current geometry (no jump); retiling
    /// snaps it into whatever slot the layout gives it, same as any other
    /// tiled window.
    pub fn toggle_floating(&mut self, surface: &WlSurface) {
        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            let was_tiled = self.ocean.is_tiled(surface);
            let changed = if was_tiled {
                self.ocean
                    .make_floating(surface, self.config.gaps, self.config.bsp_split_bias)
            } else {
                let Some(output) = self.primary_output() else {
                    return;
                };
                let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size) else {
                    return;
                };
                let focused = self.focused_window_surface();
                self.ocean.make_tiled(
                    surface,
                    &output.name(),
                    viewport,
                    focused.as_ref(),
                    self.config.gaps,
                    self.config.bsp_split_bias,
                )
            };
            if changed {
                self.physics.buoyancy_dirty = true;
                if !was_tiled {
                    self.physics.window_float_ambient.remove(surface);
                    self.physics.window_currents.remove(surface);
                    self.floating_dragging.remove(surface);
                    self.physics.window_buoyancy.remove(surface);
                }
                self.retile();
                if was_tiled {
                    self.enforce_floating_size_constraints(surface);
                    self.rescue_toggled_floating(surface);
                }
                self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
                    surface: surface.clone(),
                });
            }
            return;
        }
        let Some(window) = self
            .space
            .elements()
            .find(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == surface)
                    .unwrap_or(false)
            })
            .cloned()
        else {
            return;
        };

        let was_tiled = self.layout.contains(surface);
        if was_tiled {
            // `space` currently contains the fullscreen override geometry,
            // not the underlying tile geometry. If this is the first time a
            // tiled fullscreen window becomes floating, recover its normal
            // tile rect before removing it from the tree. Niri likewise
            // preserves separate windowed/floating geometry instead of
            // treating the fullscreen viewport as a restoration size.
            let normal_tile_rect = self.layout.output_of(surface).and_then(|output_name| {
                let workspace = self.layout.workspace_of(surface)?;
                let output = self.output_by_name(output_name)?;
                let area = self.output_tiling_area(&output)?;
                self.layout
                    .layout(
                        output_name,
                        workspace,
                        area,
                        self.gaps_for(output_name, workspace),
                    )
                    .into_iter()
                    .find_map(|(candidate, rect)| is_window(&candidate, surface).then_some(rect))
            });

            // It's tiled, so by construction it's on its output's *active*
            // workspace (a hidden workspace's windows are never mapped) --
            // tag it with that before removing it from the tree, since
            // `switch_workspace` needs this tag for a floating window (a
            // tiled one's workspace is implicit in tree membership instead).
            if let Some(output) = self.layout.output_of(surface).map(str::to_string) {
                let workspace = self.layout.active_workspace(&output);
                // Prefer an existing pre-fullscreen floating rect (it may be
                // surviving a floating -> tiled -> floating round trip), then
                // the underlying tile, and only finally the mapped geometry.
                // The final fallback can be fullscreen-sized, but is better
                // than losing the window entirely if output geometry vanished.
                let rect = self
                    .fullscreen
                    .get(surface)
                    .and_then(|entry| entry.restore_rect)
                    .or_else(|| {
                        (self.pseudo_tiled.contains(surface)
                            && !self.fullscreen.contains_key(surface))
                        .then(|| self.space.element_geometry(&window))
                        .flatten()
                    })
                    .or(normal_tile_rect)
                    .or_else(|| self.space.element_geometry(&window))
                    .unwrap_or_else(|| Rectangle::new(Point::default(), window.geometry().size));
                if let Some(entry) = self.fullscreen.get_mut(surface) {
                    entry.remember_restore_rect(rect);
                }
                self.floating_workspace.insert(
                    surface.clone(),
                    FloatingTag {
                        window: window.clone(),
                        output,
                        workspace,
                        rect,
                    },
                );
            }
            // Pseudo-tiling is a tiled placement mode, not a dormant flag to
            // report on a floating/pinned window or silently reactivate later.
            self.pseudo_tiled.remove(surface);
            self.layout.remove(surface);
            self.space.raise_element(&window, false);
        } else {
            // Tile on the output containing the floater, with the action
            // output as fallback when it is fully outside all outputs.
            let Some(output) = self
                .output_for_window(&window)
                .or_else(|| self.primary_output())
            else {
                return;
            };
            let focused = self.focused_window_surface();
            let workspace = self.layout.active_workspace(&output.name());
            // Pinning requires floating state. Explicitly unpin before the
            // inverse transition so `pinned => floating` remains true even
            // when toggle_floating is invoked directly on a pinned window.
            self.pinned.remove(surface);
            if let Some(entry) = self.fullscreen.get_mut(surface) {
                entry.was_pinned = false;
            }
            let was_maximized = self.maximized.remove(surface).is_some();
            crate::grabs::resize_grab::cancel(surface);
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    if was_maximized {
                        state.states.unset(xdg_toplevel::State::Maximized);
                    }
                    state.states.unset(xdg_toplevel::State::Resizing);
                });
            }
            self.layout
                .insert(&output.name(), workspace, window, focused.as_ref());
            // No longer floating, so no longer needs its own workspace tag.
            self.floating_workspace.remove(surface);
            self.physics.window_float_ambient.remove(surface);
            self.physics.window_currents.remove(surface);
            self.floating_dragging.remove(surface);
            self.physics.window_buoyancy.remove(surface);
            self.physics.buoyancy_dirty = true;
        }

        self.physics.buoyancy_dirty = true;
        self.retile();
        if was_tiled {
            self.enforce_floating_size_constraints(surface);
            self.rescue_toggled_floating(surface);
        }
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
    }

    /// Converts a just-mapped tiled window on a *hidden* Classic workspace
    /// to floating. The interactive `toggle_floating` path cannot do this:
    /// it looks the window up in `space.elements()` (a hidden workspace's
    /// windows are never mapped) and tags the float with the output's
    /// *active* workspace rather than the tree it actually came from. The
    /// window handle comes from `Layouts`, the rect from the slot the tree
    /// would have given it, and nothing touches `Space` -- the window
    /// becomes visible through `apply_workspace_switch`'s ordinary
    /// incoming-floating map when its workspace is shown.
    pub(crate) fn float_hidden_tiled_window(
        &mut self,
        surface: &WlSurface,
        output_name: &str,
        workspace: u32,
    ) {
        let Some(window) = self.layout.window_of(surface) else {
            return;
        };
        let rect = self
            .output_by_name(output_name)
            .and_then(|output| self.output_tiling_area(&output))
            .and_then(|area| {
                self.layout
                    .layout(
                        output_name,
                        workspace,
                        area,
                        self.gaps_for(output_name, workspace),
                    )
                    .into_iter()
                    .find_map(|(candidate, rect)| is_window(&candidate, surface).then_some(rect))
            })
            .unwrap_or_else(|| Rectangle::new(Point::default(), window.geometry().size));
        self.pseudo_tiled.remove(surface);
        self.layout.remove(surface);
        self.floating_workspace.insert(
            surface.clone(),
            FloatingTag {
                window,
                output: output_name.to_string(),
                workspace,
                rect,
            },
        );
        self.physics.buoyancy_dirty = true;
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
    }

    /// Toggles `surface` pseudo-tiled: stays in its `Layouts` slot (unlike
    /// floating), but `retile()` shrinks the rect it actually renders at
    /// to `config.pseudo_tile_scale` of the tile, centered within it,
    /// instead of filling it. No-op for a window that isn't tiled --
    /// pseudo-tiling only has meaning as a rect override on a real tile.
    pub fn toggle_pseudo_tile(&mut self, surface: &WlSurface) {
        if !self.layout.contains(surface) {
            return;
        }
        if !self.pseudo_tiled.remove(surface) {
            self.pseudo_tiled.insert(surface.clone());
        }
        self.retile();
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
    }

    /// Moves every window owned by `from_output` onto `to_output`, preserving
    /// workspace numbers. This runs while both outputs are still mapped, so
    /// floating restore rectangles can be translated between their global
    /// logical origins before the disconnected output disappears.
    ///
    /// A migrated workspace is not necessarily `to_output`'s active one. In
    /// that case its windows are explicitly unmapped here and remain
    /// reachable via `workspace:<N>`; leaving an element mapped at the dead
    /// output's old coordinates would make it visible on the wrong workspace
    /// and could leave keyboard focus pointing at unreachable content.
    pub(crate) fn migrate_output_windows(&mut self, from_output: &str, to_output: &str) {
        #[cfg(feature = "screencast")]
        if self
            .screencast_picker
            .as_ref()
            .is_some_and(|picker| picker.output_name() == from_output)
        {
            // The modal UI cannot remain usable after its output vanishes.
            // Dropping it sends a portal cancellation response.
            self.screencast_picker.take();
        }
        let from_geometry = self
            .output_by_name(from_output)
            .and_then(|output| self.space.output_geometry(&output));
        let to_output_handle = self.output_by_name(to_output);
        let to_geometry = to_output_handle
            .as_ref()
            .and_then(|output| self.space.output_geometry(output));
        let to_bounds = to_output_handle
            .as_ref()
            .and_then(|output| self.output_tiling_area(output))
            .or(to_geometry);
        let delta = match (from_geometry, to_geometry) {
            (Some(from), Some(to)) => Some(to.loc - from.loc),
            _ => None,
        };
        let active_workspace = self.layout.active_workspace(to_output);

        // Fullscreen is exclusive per output. If content from the removed
        // output is arriving, demote the destination's current owner before
        // retagging the source; otherwise debug builds trip the invariant and
        // release builds retain two contradictory protocol owners.
        let moving_fullscreen = self
            .fullscreen
            .iter()
            .find(|(_, entry)| entry.output == from_output)
            .map(|(surface, _)| surface.clone());
        if let Some(moving_surface) = moving_fullscreen.as_ref() {
            let destination_owner = self
                .fullscreen
                .iter()
                .find(|(surface, entry)| *surface != moving_surface && entry.output == to_output)
                .map(|(surface, _)| surface.clone());
            if let Some(surface) = destination_owner {
                let toplevel = self
                    .mapped_toplevel_window(&surface)
                    .and_then(|window| window.toplevel().cloned());
                if let Some(toplevel) = toplevel {
                    self.do_unfullscreen(&toplevel);
                } else {
                    self.fullscreen.remove(&surface);
                }
            }
        }
        if let Some(surface) = moving_fullscreen.as_ref() {
            if let Some(entry) = self.fullscreen.get_mut(surface) {
                entry.move_to_output(to_output.to_string(), delta);
                if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, to_bounds) {
                    *rect = clamp_rect_visible(*rect, bounds);
                }
            }
        }

        let moving_maximized: Vec<WlSurface> = self
            .maximized
            .iter()
            .filter(|(_, entry)| entry.output == from_output)
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in &moving_maximized {
            if let Some(entry) = self.maximized.get_mut(surface) {
                entry.move_to_output(to_output.to_string(), delta);
                if let Some(bounds) = to_bounds {
                    entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
                }
            }
        }

        // A deck entry has neither a Space placement nor a Layouts leaf.
        self.classic_depth.migrate_output(from_output, to_output);

        let workspaces: Vec<u32> = self
            .layout
            .populated_workspaces()
            .into_iter()
            .filter(|(name, _)| name == from_output)
            .map(|(_, workspace)| workspace)
            .collect();
        for workspace in workspaces {
            for window in self.layout.windows_in(from_output, workspace) {
                let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) else {
                    continue;
                };
                self.layout.remove(&surface);
                self.layout
                    .insert(to_output, workspace, window.clone(), None);

                if workspace == active_workspace {
                    if let Some(output) = &to_output_handle {
                        self.set_window_fractional_scale(&window, output);
                    }
                } else {
                    self.space.unmap_elem(&window);
                }
            }
        }

        // Parked group members need their durable ownership moved explicitly.
        for group in &mut self.groups {
            if group.output == from_output {
                group.output = to_output.to_string();
            }
        }

        let floating_surfaces: Vec<WlSurface> = self
            .floating_workspace
            .iter()
            .filter(|(_, tag)| tag.output == from_output)
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in floating_surfaces {
            let was_visible = self.window_is_visible(&surface);
            let live_rect = was_visible
                .then(|| {
                    let window = self.floating_workspace.get(&surface)?.window.clone();
                    self.space.element_geometry(&window)
                })
                .flatten();

            let (window, workspace, loc) = {
                let tag = self.floating_workspace.get_mut(&surface).unwrap();
                if !self.fullscreen.contains_key(&surface) && !self.maximized.contains_key(&surface)
                {
                    if let Some(rect) = live_rect {
                        tag.rect = rect;
                    }
                }
                tag.output = to_output.to_string();
                if let Some(delta) = delta {
                    tag.rect.loc += delta;
                }
                if let Some(bounds) = to_bounds {
                    tag.rect = clamp_rect_visible(tag.rect, bounds);
                }
                (tag.window.clone(), tag.workspace, tag.rect.loc)
            };

            if self.pinned.contains(&surface) || workspace == active_workspace {
                self.space.map_element(window.clone(), loc, false);
                if let Some(output) = &to_output_handle {
                    self.set_window_fractional_scale(&window, output);
                }
            } else {
                self.space.unmap_elem(&window);
            }
        }

        self.scratchpad_previous.remove(from_output);
        self.workspace_previous.remove(from_output);

        // Retile reconciles Classic placements. Ocean's camera renderer also
        // needs the client-side protocol size updated immediately because its
        // visual fit can otherwise hide the stale buffer size after hotplug.
        if let (Some(surface), Some(output_geo)) = (moving_fullscreen, to_geometry) {
            if let Some(toplevel) = self
                .mapped_toplevel_window(&surface)
                .and_then(|window| window.toplevel().cloned())
            {
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.states.unset(xdg_toplevel::State::Maximized);
                    state.states.unset(xdg_toplevel::State::Resizing);
                    state.size = Some(output_geo.size);
                });
                toplevel.send_pending_configure();
            }
        }
        if let Some(area) = to_bounds {
            let gaps = if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean
            {
                self.config.gaps
            } else {
                self.gaps_for(to_output, active_workspace)
            };
            let rect = crate::layout::inset(area, gaps);
            for surface in moving_maximized {
                if self.fullscreen.contains_key(&surface) {
                    continue;
                }
                if let Some(toplevel) = self
                    .mapped_toplevel_window(&surface)
                    .and_then(|window| window.toplevel().cloned())
                {
                    toplevel.with_pending_state(|state| {
                        state.states.set(xdg_toplevel::State::Maximized);
                        state.states.unset(xdg_toplevel::State::Fullscreen);
                        state.states.unset(xdg_toplevel::State::Resizing);
                        state.size = Some(rect.size);
                    });
                    toplevel.send_pending_configure();
                }
            }
        }
    }

    /// Completes a cross-monitor tile drop onto the destination's visible
    /// tree. Group tabs travel together; pointer callbacks supply the target
    /// and never need to lock/query the pointer again during release.
    pub(crate) fn move_tiled_window_to_output(
        &mut self,
        surface: &WlSurface,
        to: &Output,
        target: Option<&WlSurface>,
    ) -> bool {
        if !self.window_is_visible(surface)
            || self.fullscreen.contains_key(surface)
            || self.maximized.contains_key(surface)
        {
            return false;
        }
        let Some(geometry) = self.space.output_geometry(to) else {
            return false;
        };
        let name = to.name();
        let workspace = self.layout.active_workspace(&name);
        let window = match self.spatial.engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => {
                let Some(window) = self.layout.window_of(surface) else {
                    return false;
                };
                let target = target.filter(|target| {
                    self.layout.output_of(target) == Some(name.as_str())
                        && self.layout.workspace_of(target) == Some(workspace)
                });
                self.layout.remove(surface);
                self.layout.insert(&name, workspace, window.clone(), target);
                window
            }
            crate::config::SpatialOwnership::Ocean => {
                let Some(window) =
                    self.ocean
                        .move_tiled_to_output(surface, &name, geometry.size, target)
                else {
                    return false;
                };
                window
            }
        };
        let members = if let Some(index) = self.group_of(surface) {
            let group = &mut self.groups[index];
            group.output = name.clone();
            group.workspace = workspace;
            group.strip = None;
            group
                .members
                .iter()
                .map(|member| (member.surface.clone(), member.parked_window.clone()))
                .collect::<Vec<_>>()
        } else {
            vec![(surface.clone(), None)]
        };
        self.set_window_fractional_scale(&window, to);
        for (member, parked) in members {
            if let Some(pin) = self.output_pins.get_mut(&member) {
                *pin = name.clone();
            }
            if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
                self.ocean.set_entry_output(&member, name.clone());
            }
            if let Some(parked) = parked {
                self.set_window_fractional_scale(&parked, to);
            }
            self.refresh_window_opacity_and_glass_for(&member);
            self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged { surface: member });
        }
        self.refresh_ext_workspaces();
        true
    }

    /// Moves exactly `surface` to `to`, wherever it currently lives --
    /// the single-surface counterpart to `migrate_output_windows`'s bulk
    /// per-output move, reusing the same idioms (tiled: `Layouts::remove`
    /// and `insert`, letting the caller's `retile()` reconcile Space the same
    /// way `move_to_workspace` does; floating: retag + translate by the
    /// output-origin delta + explicit map/unmap; fullscreen/maximized:
    /// `move_to_output` plus the same per-output exclusivity demotion
    /// `migrate_output_windows`/`swap_workspaces` already take). Used by
    /// `recall_pinned_windows_to`. No-op if `surface` isn't currently tiled
    /// or floating anywhere, or is already on `to`. Acknowledged gap: unlike
    /// `migrate_output_windows`, this doesn't re-send a fullscreen client's
    /// configure at the destination output's exact size -- a recalled
    /// fullscreen-pinned window keeps its old buffer size until some later
    /// event (focus change, an explicit fullscreen toggle) triggers a
    /// resize. Narrow enough (pinned *and* fullscreen *and* mid-recall) not
    /// to be worth the extra plumbing here; revisit if it's ever hit for
    /// real.
    pub(super) fn move_window_to_output(&mut self, surface: &WlSurface, to: &Output) {
        let to_name = to.name();
        let from_name = self
            .layout
            .output_of(surface)
            .map(str::to_string)
            .or_else(|| {
                self.floating_workspace
                    .get(surface)
                    .map(|tag| tag.output.clone())
            });
        let Some(from_name) = from_name else {
            return;
        };
        if from_name == to_name {
            return;
        }
        let from_geometry = self
            .output_by_name(&from_name)
            .and_then(|output| self.space.output_geometry(&output));
        let to_geometry = self.space.output_geometry(to);
        let to_bounds = self.output_tiling_area(to).or(to_geometry);
        let delta = match (from_geometry, to_geometry) {
            (Some(from), Some(geo)) => Some(geo.loc - from.loc),
            _ => None,
        };

        // Fullscreen is exclusive per output -- demote whatever's already
        // fullscreen on the destination first, same precaution
        // `migrate_output_windows`/`swap_workspaces` take.
        if self.fullscreen.contains_key(surface) {
            let destination_owner = self
                .fullscreen
                .iter()
                .find(|(other, entry)| *other != surface && entry.output == to_name)
                .map(|(other, _)| other.clone());
            if let Some(owner) = destination_owner {
                let toplevel = self
                    .mapped_toplevel_window(&owner)
                    .and_then(|window| window.toplevel().cloned());
                if let Some(toplevel) = toplevel {
                    self.do_unfullscreen(&toplevel);
                } else {
                    self.fullscreen.remove(&owner);
                }
            }
        }
        if let Some(entry) = self.fullscreen.get_mut(surface) {
            entry.move_to_output(to_name.clone(), delta);
            if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, to_bounds) {
                *rect = clamp_rect_visible(*rect, bounds);
            }
        }
        if let Some(entry) = self.maximized.get_mut(surface) {
            entry.move_to_output(to_name.clone(), delta);
            if let Some(bounds) = to_bounds {
                entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
            }
        }

        if let Some(window) = self.layout.window_of(surface) {
            let workspace = self
                .layout
                .workspace_of(surface)
                .unwrap_or_else(|| self.layout.active_workspace(&from_name));
            self.layout.remove(surface);
            self.layout
                .insert(&to_name, workspace, window.clone(), None);
            if workspace == self.layout.active_workspace(&to_name) {
                self.set_window_fractional_scale(&window, to);
            } else {
                self.space.unmap_elem(&window);
            }
        } else if self.floating_workspace.contains_key(surface) {
            let was_visible = self.window_is_visible(surface);
            let live_rect = was_visible
                .then(|| {
                    let window = self.floating_workspace.get(surface)?.window.clone();
                    self.space.element_geometry(&window)
                })
                .flatten();
            let (window, workspace, loc) = {
                let tag = self.floating_workspace.get_mut(surface).unwrap();
                if !self.fullscreen.contains_key(surface) && !self.maximized.contains_key(surface) {
                    if let Some(rect) = live_rect {
                        tag.rect = rect;
                    }
                }
                tag.output = to_name.clone();
                if let Some(delta) = delta {
                    tag.rect.loc += delta;
                }
                if let Some(bounds) = to_bounds {
                    tag.rect = clamp_rect_visible(tag.rect, bounds);
                }
                (tag.window.clone(), tag.workspace, tag.rect.loc)
            };
            if self.pinned.contains(surface) || workspace == self.layout.active_workspace(&to_name)
            {
                self.space.map_element(window.clone(), loc, false);
                self.set_window_fractional_scale(&window, to);
            } else {
                self.space.unmap_elem(&window);
            }
        }
    }

    /// Adopt durable ownership left behind by a zero-output interval. A
    /// connector may return under a different kernel name (for example after
    /// moving the cable), so waiting for the old name would strand the whole
    /// Classic session indefinitely.
    pub(crate) fn adopt_orphaned_output_windows(&mut self, to_output: &str) {
        let live_outputs: HashSet<String> = self.space.outputs().map(Output::name).collect();
        let mut owned_outputs: HashSet<String> = self
            .layout
            .populated_workspaces()
            .into_iter()
            .map(|(output, _)| output)
            .collect();
        owned_outputs.extend(
            self.floating_workspace
                .values()
                .map(|tag| tag.output.clone()),
        );
        owned_outputs.extend(self.classic_depth.output_names().map(str::to_string));
        owned_outputs.extend(self.fullscreen.values().map(|entry| entry.output.clone()));
        owned_outputs.extend(self.maximized.values().map(|entry| entry.output.clone()));
        owned_outputs.extend(self.groups.iter().map(|group| group.output.clone()));

        let mut orphaned: Vec<String> = owned_outputs
            .into_iter()
            .filter(|output| output != to_output && !live_outputs.contains(output))
            .collect();
        orphaned.sort();
        for output in orphaned {
            tracing::info!(
                from = output,
                to = to_output,
                "Adopting orphaned output ownership"
            );
            self.migrate_output_windows(&output, to_output);
        }
    }

    // -- Spatial-engine migration (S6) -----------------------------------------

    /// Translates live ownership between Classic workspaces and Ocean reefs.
    /// Shared window/protocol tags survive; pins change representation. State
    /// with no counterpart is dropped after restoring any parked windows.
    pub(crate) fn migrate_spatial_engine(&mut self, target: crate::config::SpatialEngine) -> bool {
        if target == self.spatial.engine {
            return true;
        }
        if self.activation_blocked()
            || self.classic_tile_drag.is_some()
            || self.compositor_gesture.is_some()
            || self
                .seat
                .get_pointer()
                .is_some_and(|pointer| pointer.is_grabbed())
        {
            tracing::warn!(
                ?target,
                "cannot change spatial engine during lock or an active grab"
            );
            return false;
        }
        if self
            .space
            .outputs()
            .all(|output| self.space.output_geometry(output).is_none())
        {
            tracing::warn!(
                ?target,
                "cannot migrate spatial engine without a live output"
            );
            return false;
        }
        tracing::info!(
            from = ?self.spatial.engine,
            to = ?target,
            "migrating spatial engine"
        );
        // `Space` is a render/input cache, not the ownership model. Clear
        // every old-engine placement first so the new engine can rebuild only
        // what should be visible; Classic retile intentionally does not walk
        // inactive workspaces to unmap stale Ocean placements.
        let mapped: Vec<Window> = self.space.elements().cloned().collect();
        // Floating stacking order, topmost first. Classic keeps it only in
        // `Space` (bottom to top), which the unmap below throws away.
        let float_stack: Vec<WlSurface> =
            if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
                self.ocean.floating_front_to_back().to_vec()
            } else {
                mapped
                    .iter()
                    .rev()
                    .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone()))
                    .filter(|surface| self.floating_workspace.contains_key(surface))
                    .collect()
            };
        // `rule { on_workspace }` is a live match criterion feeding cached
        // opacity/glass, and this migration is the one place a window's
        // workspace-number-or-lack-thereof changes wholesale (Classic
        // numbers vs. Ocean's no-workspace-concept) -- re-resolve everything
        // that was visible going in once the migration lands below.
        let migrated_surfaces: Vec<WlSurface> = mapped
            .iter()
            .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone()))
            .collect();
        for window in mapped {
            self.space.unmap_elem(&window);
        }
        self.refresh_space();
        match (self.spatial.engine.ownership(), target.ownership()) {
            (crate::config::SpatialOwnership::Ocean, _) => self.migrate_ocean_to_classic(),
            (_, crate::config::SpatialOwnership::Ocean) => {
                self.migrate_classic_to_ocean(&float_stack)
            }
            _ => {} // Classic and Scroll share durable workspace membership.
        }
        self.spatial.engine = target;
        self.pending_workspace_transitions.clear();
        self.workspace_transitions.clear();
        self.workspace_glides.clear();
        self.swim_cameras.clear();
        // Rebuild Space as the new engine's render/input cache.
        self.retile_with_viscosity(false, None);
        // Classic retile maps trees only; restore visible migrated floaters.
        if target.ownership() == crate::config::SpatialOwnership::Workspaces {
            let mut floating: Vec<(WlSurface, Rectangle<i32, Logical>, Window)> = self
                .floating_workspace
                .iter()
                .map(|(surface, tag)| (surface.clone(), tag.rect, tag.window.clone()))
                .collect();
            // Each map raises, so map bottom first to keep the old stacking;
            // floaters with no known position go underneath.
            floating.sort_by_key(|(surface, _, _)| {
                match float_stack.iter().position(|s| s == surface) {
                    None => (false, std::cmp::Reverse(0)),
                    Some(index) => (true, std::cmp::Reverse(index)),
                }
            });
            for (surface, rect, window) in floating {
                let Some(tag) = self.floating_workspace.get(&surface) else {
                    continue;
                };
                if self.pinned.contains(&surface)
                    || self.layout.active_workspace(&tag.output) == tag.workspace
                {
                    self.space.map_element(window.clone(), rect.loc, false);
                    if let Some(output) = self.output_by_name(&tag.output) {
                        self.set_window_fractional_scale(&window, &output);
                    }
                }
            }
        }
        for surface in &migrated_surfaces {
            self.refresh_window_opacity_and_glass_for(surface);
        }
        if let Some(focus) = self.window_focus.clone() {
            self.reveal_scroll_window(&focus);
        }
        self.sync_tide();
        self.refresh_ext_workspaces();
        self.emit_ipc_event(crate::ipc::IpcEvent::ConfigReloaded);
        self.request_redraw();
        true
    }

    /// Tiling work area expressed in the active engine's placement coordinates.
    fn floating_placement_area(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let area = self.output_tiling_area(output)?;
        if self.spatial.engine.ownership() != crate::config::SpatialOwnership::Ocean {
            return Some(area);
        }
        let geometry = self.space.output_geometry(output)?;
        let camera = self.ocean.camera(&output.name());
        let zoom = camera.zoom.max(0.05);
        Some(Rectangle::new(
            (
                (camera.origin.x + f64::from(area.loc.x - geometry.loc.x) / zoom).round() as i32,
                (camera.origin.y + f64::from(area.loc.y - geometry.loc.y) / zoom).round() as i32,
            )
                .into(),
            (
                (f64::from(area.size.w) / zoom).round().max(1.0) as i32,
                (f64::from(area.size.h) / zoom).round().max(1.0) as i32,
            )
                .into(),
        ))
    }

    /// New floats are placed in visible output coordinates, never at their
    /// transient tile/Scroll-strip location. Interactive toggles do not use
    /// this centering policy.
    pub(crate) fn place_initial_floating(
        &mut self,
        surface: &WlSurface,
        output: &Output,
        workspace: u32,
        rule: &crate::config::WindowRule,
        size: Option<(i32, i32)>,
    ) {
        let Some(bounds) = self.floating_placement_area(output) else {
            return;
        };
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        let constraints = self.floating_size_constraints_with_rule(surface, rule);
        // Keep explicit geometry exact wherever it fits the work area. Only
        // automatic natural-size placement reserves the usual workspace gap.
        let area = if rule.position.is_some() || rule.size.is_some() {
            bounds
        } else {
            crate::layout::inset(bounds, self.gaps_for(&output.name(), workspace))
        };
        let rect = initial_floating_rect(
            area,
            rule.position.map(Point::from),
            size.map(Size::from).unwrap_or(window.geometry().size),
            constraints,
        );
        self.apply_floating_placement(
            surface,
            Some((rect.loc.x, rect.loc.y)),
            Some((rect.size.w, rect.size.h)),
        );
    }

    /// Rescue only an off-screen interactive untile. A visible window keeps
    /// its existing location and size.
    fn rescue_toggled_floating(&mut self, surface: &WlSurface) {
        if self.fullscreen.contains_key(surface) || self.maximized.contains_key(surface) {
            return;
        }
        let owner = self
            .floating_workspace
            .get(surface)
            .map(|tag| tag.output.as_str())
            .or_else(|| self.ocean.entry_output(surface));
        let Some(output) = owner.and_then(|name| self.output_by_name(name)) else {
            return;
        };
        let Some(bounds) = self.floating_placement_area(&output) else {
            return;
        };
        let current = self
            .ocean
            .floating_rect(surface)
            .or_else(|| self.floating_workspace.get(surface).map(|tag| tag.rect));
        if let Some(current) = current {
            // A tile entirely within the output must not jump on toggle.
            let rect = fit_floating_rect_visible(current, bounds);
            if rect != current {
                self.apply_floating_placement(
                    surface,
                    Some((rect.loc.x, rect.loc.y)),
                    Some((rect.size.w, rect.size.h)),
                );
            }
        }
    }

    /// Applies a `[[rule]]`-specified exact position/size to a floating
    /// window, either of which may be unset (keep whatever `toggle_floating`
    /// already placed it at for that axis). No-op if `surface` isn't
    /// actually floating -- position/size only mean something once it is,
    /// same restriction `pseudo_tile` has in reverse for tiled. Mirrors the
    /// same configure+`map_element` shape `retile()`'s own tiled-window loop
    /// and the interactive resize/move grabs already use, not a new pattern.
    pub(crate) fn apply_floating_placement(
        &mut self,
        surface: &WlSurface,
        position: Option<(i32, i32)>,
        size: Option<(i32, i32)>,
    ) {
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            let Some(current) = self.ocean.floating_rect(surface) else {
                return;
            };
            let rect = Rectangle::new(
                position.map(Point::from).unwrap_or(current.loc),
                self.floating_size_constraints_for(surface)
                    .clamp(size.map(Size::from).unwrap_or(current.size)),
            );
            let pending = window
                .toplevel()
                .and_then(|toplevel| toplevel.with_pending_state(|state| state.size));
            if rect.loc == current.loc
                && !crate::window_size::needs_size_configure(current.size, rect.size, pending)
            {
                return;
            }
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(rect.size));
                toplevel.send_pending_configure();
            }
            self.ocean.set_floating_rect(surface, rect);
            self.space.map_element(window, rect.loc, false);
            self.request_redraw();
            return;
        }
        if !self.floating_workspace.contains_key(surface) {
            return;
        }
        // A floating window on a hidden workspace isn't in `Space`; its
        // durable tag rect is the only geometry it has.
        let Some(current) = self
            .space
            .element_geometry(&window)
            .or_else(|| self.floating_workspace.get(surface).map(|tag| tag.rect))
        else {
            return;
        };
        let loc = position.map(Point::from).unwrap_or(current.loc);
        let size = self
            .floating_size_constraints_for(surface)
            .clamp(size.map(Size::from).unwrap_or(current.size));
        let rect = Rectangle::new(loc, size);
        let pending = window
            .toplevel()
            .and_then(|toplevel| toplevel.with_pending_state(|state| state.size));
        if rect.loc == current.loc
            && !crate::window_size::needs_size_configure(current.size, rect.size, pending)
        {
            return;
        }

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = Some(rect.size);
            });
            toplevel.send_pending_configure();
        }
        if self.window_is_visible(surface) {
            self.space.map_element(window, rect.loc, false);
        }
        if let Some(tag) = self.floating_workspace.get_mut(surface) {
            tag.rect = rect;
        }
        self.request_redraw();
    }

    pub(super) fn spatial_is_tiled(&self, surface: &WlSurface) -> bool {
        match self.spatial.engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => self.layout.contains(surface),
            crate::config::SpatialOwnership::Ocean => self.ocean.is_tiled(surface),
        }
    }

    pub(super) fn spatial_tiled_window(&self, surface: &WlSurface) -> Option<Window> {
        match self.spatial.engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => self.layout.window_of(surface),
            crate::config::SpatialOwnership::Ocean => self.ocean.window(surface),
        }
    }

    pub(super) fn spatial_tiles_share_owner(&self, first: &WlSurface, second: &WlSurface) -> bool {
        match self.spatial.engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => {
                self.layout.output_of(first) == self.layout.output_of(second)
                    && self.layout.workspace_of(first) == self.layout.workspace_of(second)
            }
            crate::config::SpatialOwnership::Ocean => self.ocean.tiled_same_reef(first, second),
        }
    }

    pub(super) fn allocate_ui_node_id(&mut self) -> u64 {
        let id = self.next_ui_node_id;
        self.next_ui_node_id = self.next_ui_node_id.wrapping_add(1).max(1_000);
        id
    }

    /// Looks up protocol-mapped windows without depending on current visibility.
    pub(crate) fn window_surface_by_id(&self, id: u64) -> Option<WlSurface> {
        self.foreign_toplevel_numeric_ids
            .iter()
            .find(|(surface, candidate)| {
                **candidate == id && self.mapped_toplevel_window(surface).is_some()
            })
            .map(|(surface, _)| surface.clone())
    }

    /// Invalidates every compositor-owned texture whose pixels include this
    /// toplevel's title. Called only when the authoritative xdg-shell title
    /// differs from the value already announced to foreign-toplevel clients.
    pub(crate) fn invalidate_window_title_ui(&mut self, surface: &WlSurface) {
        self.depth_schematics.remove(surface);
        for group in &mut self.groups {
            if group
                .members
                .iter()
                .any(|member| &member.surface == surface)
            {
                group.strip = None;
            }
        }
    }

    /// Switches the current output's active workspace to `algorithm` (see
    /// `Action::SetLayout`). `Layouts` keeps the exact same tree either way
    /// -- membership, insertion order, groups, swap/focus-direction are all
    /// tree-shape-based rather than geometry-based, so none of that needs
    /// to know which algorithm is active. Only the rects `retile()`
    /// computes for this workspace change.
    pub fn set_layout_algorithm(&mut self, algorithm: crate::config::LayoutAlgorithm) {
        let Some(output) = self.primary_output() else {
            return;
        };
        let workspace = self.layout.active_workspace(&output.name());
        if self.layout.window_count(&output.name(), workspace) == 0 {
            return;
        }
        self.layout
            .set_algorithm(&output.name(), workspace, algorithm);
        self.retile();
    }

    /// Nudges the current output's active workspace's master/stack ratio
    /// (see `Action::GrowMaster`/`ShrinkMaster`). A visible no-op while BSP
    /// is active -- `layout::Layouts::layout` doesn't consult the ratio in
    /// that mode -- but still recorded against this (output, workspace), so
    /// switching to master later immediately reflects whatever was last set.
    pub fn adjust_master_ratio(&mut self, delta: f32) {
        let Some(output) = self.primary_output() else {
            return;
        };
        let workspace = self.layout.active_workspace(&output.name());
        if self.layout.window_count(&output.name(), workspace) == 0 {
            return;
        }
        self.layout
            .adjust_master_ratio(&output.name(), workspace, delta);
        self.retile();
    }

    /// Resizes the focused window without a pointer grab. Floating windows
    /// change in 24-logical-pixel steps; BSP windows drive the nearest split
    /// and its connected parallel ancestors while preserving the 5% safety
    /// clamp.
    pub fn keyboard_resize(&mut self, direction: Direction) {
        const STEP: i32 = 24;
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        let Some(window) = self.mapped_toplevel_window(&surface) else {
            return;
        };
        if self.fullscreen.contains_key(&surface) || self.maximized.contains_key(&surface) {
            return;
        }

        if self.spatial.engine.projection() == Some(crate::config::WorkspaceProjection::Scroll)
            && self.layout.contains(&surface)
        {
            let Some(owner) = self.layout.output_of(&surface).map(str::to_owned) else {
                return;
            };
            let Some(output) = self.output_by_name(&owner) else {
                return;
            };
            let Some(area) = self.output_tiling_area(&output) else {
                return;
            };
            let (along, _) = self.scroll_axis_step(direction);
            if !along {
                // Across the strip: the window's share of a shared column.
                let Some((column, _)) = self.layout.scroll_position(&surface) else {
                    return;
                };
                let Some(workspace) = self.layout.workspace_of(&surface) else {
                    return;
                };
                let rows = self.layout.scroll_columns(&owner, workspace)[column]
                    .windows
                    .len();
                if !crate::scroll::row_resize_enabled(rows, self.config.scroll.stack) {
                    return;
                }
                let cross = match self.config.scroll.orientation {
                    crate::scroll::Orientation::Horizontal => window.geometry().size.h,
                    crate::scroll::Orientation::Vertical => window.geometry().size.w,
                }
                .max(1);
                let delta = if matches!(direction, Direction::Right | Direction::Down) {
                    STEP
                } else {
                    -STEP
                };
                let weight = self.layout.scroll.weight(&surface)
                    * f64::from((cross + delta).max(1))
                    / f64::from(cross);
                self.layout.scroll.set_weight(&surface, weight);
                self.retile();
                self.reveal_scroll_window(&surface);
                self.request_redraw();
                return;
            }
            let extent = match self.config.scroll.orientation {
                crate::scroll::Orientation::Horizontal => area.size.w,
                crate::scroll::Orientation::Vertical => area.size.h,
            }
            .max(1);
            let delta = if matches!(direction, Direction::Right | Direction::Down) {
                STEP
            } else {
                -STEP
            };
            let proportion =
                self.layout.scroll.proportion(&surface) + f64::from(delta) / f64::from(extent);
            self.layout.scroll.resize(surface.clone(), proportion);
            self.retile();
            self.reveal_scroll_window(&surface);
            self.request_redraw();
            return;
        }

        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            if let Some(mut rect) = self.ocean.floating_rect(&surface) {
                rect.size = keyboard_resized_size(
                    rect.size,
                    direction,
                    STEP,
                    self.floating_size_constraints_for(&surface),
                );
                self.ocean.set_floating_rect(&surface, rect);
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| state.size = Some(rect.size));
                    toplevel.send_pending_configure();
                }
                self.retile();
                return;
            }

            let axis = match direction {
                Direction::Left | Direction::Right => crate::layout::Axis::Horizontal,
                Direction::Up | Direction::Down => crate::layout::Axis::Vertical,
            };
            let delta = if matches!(direction, Direction::Right | Direction::Down) {
                f64::from(STEP)
            } else {
                f64::from(-STEP)
            };
            if self
                .ocean
                .resize_tiled_by(&surface, axis, delta, self.config.bsp_split_bias)
            {
                self.retile();
            }
            return;
        }

        if !self.layout.contains(&surface) {
            // `Window::geometry().loc` is surface-local (usually 0,0), not
            // the element's global position in `Space`. Reusing it here
            // would teleport every keyboard-resized floater to the output's
            // origin.
            let location = self.space.element_location(&window).unwrap_or_default();
            let mut geometry = window.geometry();
            // Accumulate key repeats/batched actions against the latest
            // requested size, as niri's floating actions use expected_size.
            // The committed bbox can lag several configures behind input.
            let requested = window
                .toplevel()
                .and_then(|toplevel| toplevel.with_pending_state(|state| state.size))
                .unwrap_or(geometry.size);
            geometry.size = keyboard_resized_size(
                requested,
                direction,
                STEP,
                self.floating_size_constraints_for(&surface),
            );
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(geometry.size));
                toplevel.send_pending_configure();
            }
            self.space.map_element(window, location, true);
            if let Some(tag) = self.floating_workspace.get_mut(&surface) {
                tag.rect.loc = location;
                tag.rect.size = geometry.size;
            }
            self.request_redraw();
            return;
        }

        let Some(output_name) = self.layout.output_of(&surface).map(str::to_owned) else {
            return;
        };
        let Some(output) = self.output_by_name(&output_name) else {
            return;
        };
        let workspace = self
            .layout
            .workspace_of(&surface)
            .unwrap_or_else(|| self.layout.active_workspace(&output_name));
        let Some(area) = self.output_tiling_area(&output) else {
            return;
        };
        let wanted_axis = match direction {
            Direction::Left | Direction::Right => crate::layout::Axis::Horizontal,
            Direction::Up | Direction::Down => crate::layout::Axis::Vertical,
        };
        let Some(hit) = self
            .layout
            .resize_splits(&output_name, workspace, area, &surface)
            .into_iter()
            .find(|hit| hit.axis == wanted_axis)
        else {
            return;
        };
        let handles = self.connected_resize_handles(&hit);
        let delta_pixels = if matches!(direction, Direction::Right | Direction::Down) {
            f64::from(STEP)
        } else {
            f64::from(-STEP)
        };
        for handle in handles {
            let Some(new_ratio) = handle.ratio_for_delta(delta_pixels) else {
                continue;
            };
            self.layout.set_ratio(
                &handle.hit.output,
                handle.hit.workspace,
                &handle.hit.path,
                new_ratio,
            );
        }
        self.retile();
    }

    /// Keyboard-driven move for the focused *floating* window only (spatial
    /// roadmap Phase 4's escape hatch alongside `toggle-floating`; see
    /// `Action::MoveFloating`). A no-op on a tiled window, mirroring
    /// `keyboard_resize`'s per-engine floating/tiled split just above.
    pub fn keyboard_move_floating(&mut self, direction: Direction) {
        const STEP: i32 = 24;
        let Some(surface) = self.focused_window_surface() else {
            return;
        };

        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            let Some(mut rect) = self.ocean.floating_rect(&surface) else {
                return;
            };
            rect.loc = saturating_translate(rect.loc, direction_delta(direction, STEP));
            self.ocean.set_floating_rect(&surface, rect);
            self.retile();
            return;
        }

        if self.layout.contains(&surface) {
            return;
        }
        let Some(window) = self.mapped_toplevel_window(&surface) else {
            return;
        };
        // Same reasoning `translate_floating_windows_on_output` documents:
        // the live `Space` rect (not `Window::geometry().loc`, surface-local
        // and usually `0,0`) is the window's real global position, and
        // `floating_workspace`'s own `rect` must move with it or a later
        // hidden-workspace/restore read (workspace switch, fullscreen
        // restore) would snap the window back to its pre-move position.
        let Some(mut rect) = self.space.element_geometry(&window) else {
            return;
        };
        rect.loc = saturating_translate(rect.loc, direction_delta(direction, STEP));
        self.space.map_element(window, rect.loc, false);
        if let Some(tag) = self.floating_workspace.get_mut(&surface) {
            tag.rect = rect;
        }
        self.retile();
    }

    /// Keyboard parity for Classic drag-to-snap. The focused floater uses
    /// the same usable-area, gap, rule, and target geometry as a pointer drop.
    pub fn keyboard_snap(&mut self, zone: crate::snap::SnapZone) {
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        if !self.snap_eligible_surface(&surface) {
            return;
        }
        let Some(window) = self.mapped_toplevel_window(&surface) else {
            return;
        };
        let Some(output) = self.output_for_window(&window) else {
            return;
        };
        let Some(target) = self.snap_target_for_zone(&surface, &output, zone) else {
            return;
        };
        self.apply_snap_target(&window, &target);
    }
}
