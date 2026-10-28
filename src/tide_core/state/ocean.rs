use std::time::Duration;

use smithay::{
    backend::renderer::{element::memory::MemoryRenderBufferRenderElement, gles::GlesRenderer},
    desktop::Window,
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::config::Direction;

use super::{
    classic_rect_to_ocean, directional_score, is_scratchpad_workspace, ocean_classic_slot,
    ocean_island_origins, ocean_rect_to_classic, ocean_reef_x, FloatingTag, SessionLock, Smallvil,
};

impl Smallvil {
    /// Reattaches an Ocean floater when its final world rectangle is close to
    /// an existing reef tile. The pointer position is supplied by the grab's
    /// last motion event so release handling never re-enters the pointer
    /// handle while Smithay is dispatching the button callback.
    pub(crate) fn smart_attach_ocean_floating(
        &mut self,
        surface: &WlSurface,
        pointer_location: Point<f64, Logical>,
    ) -> bool {
        if self.spatial.engine.ownership() != crate::config::SpatialOwnership::Ocean
            || !self.config.ocean.smart_tiling
            || !self.spatial.ocean.floating_rect(surface).is_some()
        {
            return false;
        }
        let Some(output) = self.output_for_point(pointer_location) else {
            return false;
        };
        let output_name = output.name();
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return false;
        };
        let pointer_view = pointer_location - output_geo.loc.to_f64();
        // Require pointer containment; a radius scaled by the moving window
        // would make large floaters attach on nearly every overlap.
        let Some(target) = self.spatial.ocean.tiled_target_at_view(
            surface,
            &output_name,
            pointer_view,
            self.config.gaps,
            self.config.bsp_split_bias,
        ) else {
            return false;
        };
        let Some(viewport) = self.space.output_geometry(&output).map(|geo| geo.size) else {
            return false;
        };
        if !self.spatial.ocean.make_tiled_with_size(
            surface,
            &output_name,
            viewport,
            Some(&target),
            self.config.ocean.smart_tiling_preserve_size,
            self.config.gaps,
            self.config.bsp_split_bias,
        ) {
            return false;
        }
        self.retile();
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
        true
    }

    pub(super) fn topmost_ocean_surface(
        &self,
        preferred_output: Option<&str>,
    ) -> Option<WlSurface> {
        let mut outputs: Vec<Output> = self.space.outputs().cloned().collect();
        outputs.sort_by_key(Output::name);
        if let Some(preferred) = preferred_output {
            outputs.sort_by_key(|output| output.name() != preferred);
        }
        outputs.into_iter().find_map(|output| {
            self.render_placements(&output)?
                .into_iter()
                .find_map(|placement| placement.surface().cloned())
        })
    }

    pub(crate) fn ocean_canvas_frame_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Option<crate::backend::udev::OutputRenderElements> {
        if !matches!(self.session_lock, SessionLock::Unlocked)
            || self.spatial.engine.ownership() != crate::config::SpatialOwnership::Ocean
        {
            return None;
        }
        let output_name = output.name();
        let area = Rectangle::from_size(self.space.output_geometry(output)?.size);
        let camera = self.spatial.ocean.camera(&output_name);
        let origin = [camera.origin.x as f32, camera.origin.y as f32];
        let zoom = camera.zoom as f32;
        let grid_size = self.config.ocean.canvas_grid_size as f32;
        let opacity = if self.config.ocean.canvas_guides {
            self.config.ocean.canvas_grid_alpha
        } else {
            0.0
        };
        let marker_fade = Duration::from_millis(self.config.ocean.canvas_marker_fade_ms);
        let marker_alpha = self
            .ocean_canvases
            .entry(output_name.clone())
            .or_default()
            .note_camera(origin, zoom, self.config.ocean.canvas_marker, marker_fade);
        if opacity <= 0.0 && marker_alpha <= 0.0 {
            return None;
        }
        let program = crate::ocean_canvas::ocean_canvas_program(
            &mut self.render.gpu.ocean_canvas_program,
            renderer,
        )?;
        let element = self
            .ocean_canvases
            .entry(output_name)
            .or_default()
            .frame_element(
                program,
                area,
                crate::ocean_canvas::OceanCanvasSample {
                    origin,
                    zoom,
                    grid_size,
                    color: [0.25, 0.78, 0.92],
                    opacity,
                    marker_alpha,
                },
            );
        Some(crate::backend::udev::OutputRenderElements::OceanCanvas(
            element,
        ))
    }

    /// Builds the unlocked wallpaper-level caustics element. Zero FPS advances
    /// only with other damage; positive FPS uses the per-output deadline.
    pub(crate) fn caustics_frame_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Option<crate::backend::udev::OutputRenderElements> {
        if !matches!(self.session_lock, SessionLock::Unlocked)
            || !self.config.water_effects
            || !self.config.caustics.enabled
            || self.config.caustics.intensity <= 0.0
        {
            return None;
        }
        let area = Rectangle::from_size(self.space.output_geometry(output)?.size);
        let output_scale = output.current_scale().fractional_scale();
        let effective_fps = self.caustics_effective_fps();
        let retry_cadence = if effective_fps == 0 {
            crate::backend::udev::live_output_refresh_period(output)?
        } else {
            Duration::from_secs_f64(1.0 / f64::from(effective_fps))
        };
        let program = crate::caustics::caustics_program(
            &mut self.render.gpu.caustics_program,
            renderer,
            retry_cadence,
        )?;
        let output_name = output.name();
        let advance = if self.config.caustics.fps == 0 {
            true
        } else if effective_fps == 0 {
            false
        } else {
            self.caustics
                .get(&output_name)
                .and_then(|caustics| caustics.next_frame_in(effective_fps))
                .is_none_or(|delay| delay.is_zero())
        };
        let element = self
            .caustics
            .entry(output_name)
            .or_default()
            .frame_element(
                renderer,
                program,
                area,
                output_scale,
                &self.config.caustics,
                crate::caustics::CausticsFrameTiming {
                    advance,
                    retry_cadence,
                },
            )?;
        Some(crate::backend::udev::OutputRenderElements::Caustics(
            element,
        ))
    }

    /// The frame rate the caustics overlay should currently run at:
    /// `caustics.fps` when the user is active, stepping down through the
    /// configured idle tiers as minutes pass without input. Empty tier
    /// lists (or a short list) leave the rate at `fps` for the remaining
    /// idle time. Both lists are parallel and ascending in time. A tier
    /// of 0 means "stop pumping entirely" -- `caustics_active` treats it
    /// like `fps = 0` (frozen, zero extra frames), never as a 1Hz zombie.
    fn caustics_effective_fps(&self) -> u32 {
        let cfg = &self.config.caustics;
        let mut fps = cfg.fps;
        if cfg.idle_fps.is_empty() || cfg.idle_after_ms.is_empty() {
            return fps;
        }
        let idle_ms = self.input.last_activity.elapsed().as_millis() as u64;
        for (after_ms, tier_fps) in cfg.idle_after_ms.iter().zip(&cfg.idle_fps) {
            if idle_ms >= *after_ms {
                fps = (*tier_fps).min(fps);
            }
        }
        fps
    }

    pub(crate) fn caustics_redraw_delay(&self) -> Option<Duration> {
        self.space
            .outputs()
            .filter_map(|output| self.caustics_redraw_delay_for_output(output))
            .min()
    }

    pub(crate) fn caustics_redraw_delay_for_output(&self, output: &Output) -> Option<Duration> {
        let cfg = &self.config.caustics;
        if !matches!(self.session_lock, SessionLock::Unlocked)
            || !self.config.water_effects
            || !cfg.enabled
            || cfg.intensity <= 0.0
            || cfg.fps == 0
            || cfg.speed <= 0.0
        {
            return None;
        }
        let fps = self.caustics_effective_fps();
        if fps == 0 {
            return None;
        }
        let period = Duration::from_secs_f64(1.0 / f64::from(fps));
        let program_retry = self.render.gpu.caustics_program.retry_in(period);
        Some(
            self.caustics
                .get(&output.name())
                .and_then(|caustics| caustics.next_frame_in(fps))
                .unwrap_or(period)
                .max(program_retry),
        )
    }

    pub(super) fn caustics_active(&self) -> bool {
        self.caustics_redraw_delay()
            .is_some_and(|delay| delay.is_zero())
    }

    /// Builds render-only Ocean edge cues for off-viewport urgent/deep windows.
    /// An empty cue set creates no element or idle work.
    pub(crate) fn compass_frame_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<crate::backend::udev::OutputRenderElements> {
        if !matches!(self.session_lock, SessionLock::Unlocked)
            || self.spatial.engine.ownership() != crate::config::SpatialOwnership::Ocean
            || !self.config.water_effects
            || !self.config.compass.enabled
        {
            return Vec::new();
        }
        let Some(output_geo) = self.space.output_geometry(output) else {
            return Vec::new();
        };
        let output_name = output.name();
        let camera = self.spatial.ocean.camera(&output_name);
        let zoom = camera.zoom.max(0.05);
        let viewport = Rectangle::new(
            Point::from((camera.origin.x, camera.origin.y)),
            Size::from((
                output_geo.size.w as f64 / zoom,
                output_geo.size.h as f64 / zoom,
            )),
        );
        let cfg = self.config.compass;
        let windows: Vec<(Point<f64, Logical>, crate::compass::CueKind)> = self
            .spatial
            .ocean
            .world_layouts(self.config.gaps, self.config.bsp_split_bias)
            .into_iter()
            .filter_map(|(window, rect, _)| {
                let surface = window.toplevel()?.wl_surface().clone();
                if !self.spatial.ocean.visible_on_output(&surface, &output_name) {
                    return None;
                }
                let kind = if self.urgent.contains(&surface) {
                    crate::compass::CueKind::Urgent
                } else {
                    crate::compass::CueKind::Deep
                };
                Some((
                    Point::from((
                        rect.loc.x as f64 + rect.size.w as f64 / 2.0,
                        rect.loc.y as f64 + rect.size.h as f64 / 2.0,
                    )),
                    kind,
                ))
            })
            .collect();
        let cues = crate::compass::compute_cues(
            viewport,
            zoom,
            &windows,
            &crate::compass::CompassParams {
                urgent_color: cfg.urgent_color,
                deep_color: cfg.deep_color,
                max_distance: cfg.max_distance,
                peak_alpha: cfg.alpha,
                size: cfg.size,
                shape: cfg.shape,
            },
        );
        let Some(program) =
            crate::compass::compass_program(&mut self.render.gpu.compass_program, renderer)
        else {
            return Vec::new();
        };
        let compass = self.compasses.entry(output_name).or_default();
        compass
            .frame_elements(program, cues)
            .into_iter()
            .map(crate::backend::udev::OutputRenderElements::Compass)
            .collect()
    }

    /// Opens the Ocean minimap from current `world_layouts`. Active pointer
    /// grabs suppress it because consuming the matching release would strand
    /// their commit; popup grabs are safe and remain eligible. Callers only
    /// intercept the trigger if this method actually opens the peek.
    pub(crate) fn open_minimap_peek(&mut self, output: &Output) {
        if self.minimap_peek.is_some()
            || self.spatial.engine.ownership() != crate::config::SpatialOwnership::Ocean
            || !self.config.minimap.enabled
            || !matches!(self.session_lock, SessionLock::Unlocked)
            || (self.input.popup_grab.is_none()
                && self
                    .seat
                    .get_pointer()
                    .is_some_and(|pointer| pointer.is_grabbed()))
        {
            return;
        }
        let Some(output_geometry) = self.space.output_geometry(output) else {
            return;
        };
        let output_name = output.name();
        let pointer_location = self
            .seat
            .get_pointer()
            .map(|pointer| pointer.current_location())
            .unwrap_or_default();

        let windows: Vec<(Rectangle<i32, Logical>, String, bool)> = self
            .spatial
            .ocean
            .world_layouts(self.config.gaps, self.config.bsp_split_bias)
            .into_iter()
            .filter_map(|(window, rect, _kind)| {
                let surface = window.toplevel()?.wl_surface().clone();
                if !self.spatial.ocean.visible_on_output(&surface, &output_name) {
                    return None;
                }
                let urgent = self.urgent.contains(&surface);
                Some((rect, crate::tab_strip::window_title(&surface), urgent))
            })
            .collect();
        let reef_rects = self.spatial.ocean.reef_rects_on_output(&output_name);
        let viewports: Vec<(String, Rectangle<i32, Logical>)> = self
            .space
            .outputs()
            .filter_map(|candidate| {
                if !self.config.ocean.shared_canvas && candidate.name() != output_name {
                    return None;
                }
                let geo = self.space.output_geometry(candidate)?;
                let camera = self.spatial.ocean.camera(&candidate.name());
                let zoom = camera.zoom.max(0.05);
                let size = Size::from((
                    (geo.size.w as f64 / zoom).round() as i32,
                    (geo.size.h as f64 / zoom).round() as i32,
                ));
                let loc = Point::from((
                    camera.origin.x.round() as i32,
                    camera.origin.y.round() as i32,
                ));
                Some((candidate.name(), Rectangle::new(loc, size)))
            })
            .collect();

        let style = crate::minimap::MinimapStyle::resolve(
            self.config.minimap.preset,
            self.config.minimap.background_color,
            self.config.minimap.window_color,
            self.config.minimap.accent_color,
        );
        self.minimap_peek = Some(crate::minimap::MinimapPeek::build(
            output_name,
            output_geometry,
            pointer_location,
            style,
            &windows,
            &reef_rects,
            &viewports,
        ));
        self.request_redraw();
    }

    /// Dismisses the minimap peek without traveling, if one is open.
    /// Idempotent -- both the chord-release path and the post-click path
    /// in `minimap_click_travel` may call this for the same peek.
    pub(crate) fn close_minimap_peek(&mut self) {
        if self.minimap_peek.take().is_some() {
            self.refresh_pointer_focus();
            self.request_redraw();
        }
    }

    /// Live pointer position feed for the open peek, so a click can resolve
    /// a world point without reading `current_location()` from inside a
    /// grab/button dispatch -- see `MinimapPeek::last_location`'s own doc
    /// for why that specific call shape is what froze this compositor
    /// twice before (`TileMoveGrab` 0.15.1, `sync_visible_floating_window`).
    pub(crate) fn update_minimap_pointer(&mut self, location: Point<f64, Logical>) {
        if let Some(peek) = self.minimap_peek.as_mut() {
            peek.set_last_location(location);
        }
    }

    /// Resolves the peek's last known pointer position to a world point,
    /// travels the triggering output's Ocean camera there, and dismisses.
    /// A no-op if no peek is open (nothing left to click).
    pub(crate) fn minimap_click_travel(&mut self) {
        let Some(peek) = self.minimap_peek.take() else {
            return;
        };
        let world = peek.world_point_at_last_location();
        let output_name = peek.output_name().to_string();
        let Some(viewport) = self
            .space
            .outputs()
            .find(|candidate| candidate.name() == output_name)
            .and_then(|candidate| self.space.output_geometry(candidate))
            .map(|geo| geo.size)
        else {
            self.refresh_pointer_focus();
            self.request_redraw();
            return;
        };
        let target = Rectangle::new(
            Point::from((world.x.round() as i32, world.y.round() as i32)),
            Size::from((0, 0)),
        );
        self.spatial.ocean.center_on_rect(
            &output_name,
            viewport,
            target,
            Duration::from_millis(self.config.ocean.camera_animation_ms),
            self.config.ocean.camera_sway,
        );
        self.refresh_pointer_focus();
        self.request_redraw();
    }

    /// Off-screen render element for the open minimap peek, filtered to the
    /// output it was built for -- same raw `Option<MemoryRenderBufferRenderElement<_>>`
    /// shape as `overview.rs`'s own `render_element`, so both backends'
    /// existing `overview_element`-style `.chain()` assembly can carry it
    /// too, wrapped by the same single `OutputRenderElements::Composited`
    /// map at the end -- no new element type needed.
    pub(crate) fn minimap_frame_element(
        &self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let Some(peek) = self
            .minimap_peek
            .as_ref()
            .filter(|peek| peek.output_name() == output.name())
        else {
            return Vec::new();
        };
        let mut elements = Vec::with_capacity(2);
        // Front-to-back, index 0 topmost: the cursor glyph must come first
        // so it draws *over* the map, not beneath it -- the map's panel is
        // translucent, and a cursor behind it showed as a faint ghost (the
        // same element-order class as the session-lock bug).
        let scale = output.current_scale().fractional_scale();
        let output_loc = self
            .space
            .output_geometry(output)
            .map(|geo| geo.loc)
            .unwrap_or_else(|| Point::from((0, 0)));
        if let Some(cursor) = peek.cursor_element(renderer, output_loc, scale) {
            elements.push(cursor);
        }
        if let Some(map) = peek.render_element(renderer) {
            elements.push(map);
        }
        elements
    }

    pub(super) fn retile_ocean(&mut self, skip_configure_for: Option<&WlSurface>) {
        self.spatial
            .ocean
            .set_shared_canvas(self.config.ocean.shared_canvas);
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let Some(seed_output) = outputs.first() else {
            return;
        };
        let Some(seed_geo) = self.space.output_geometry(seed_output) else {
            return;
        };
        self.spatial
            .ocean
            .ensure_default_reef(&seed_output.name(), seed_geo.size);
        self.spatial
            .ocean
            .clamp_screen_pins(&seed_output.name(), seed_geo.size);
        for output in outputs.iter().skip(1) {
            if let Some(geometry) = self.space.output_geometry(output) {
                self.spatial
                    .ocean
                    .ensure_default_reef(&output.name(), geometry.size);
                self.spatial
                    .ocean
                    .clamp_screen_pins(&output.name(), geometry.size);
            }
        }

        let tiled = self
            .spatial
            .ocean
            .tiled_layouts(self.config.gaps, self.config.bsp_split_bias);
        for (window, rect) in &tiled {
            if let Some(toplevel) = window.toplevel() {
                let target_size = self
                    .fullscreen
                    .get(toplevel.wl_surface())
                    .and_then(|entry| self.output_by_name(&entry.output))
                    .and_then(|output| self.space.output_geometry(&output))
                    .map(|geometry| geometry.size)
                    .unwrap_or(rect.size);
                toplevel.with_pending_state(|state| state.size = Some(target_size));
                if skip_configure_for != Some(toplevel.wl_surface()) {
                    toplevel.send_pending_configure();
                }
            }
        }

        for (window, rect, _) in self.spatial.ocean.world_layouts_from_tiled(tiled) {
            let output = self
                .output_for_window(&window)
                .unwrap_or_else(|| seed_output.clone());
            self.set_window_fractional_scale(&window, &output);
            // Space remains a protocol/input cache in Ocean. The shared
            // renderer uses world rect + camera translation, never this
            // presentation coordinate as spatial ownership.
            self.space.map_element(window, rect.loc, false);
        }

        self.request_redraw();
    }

    /// Classic -> Ocean. Every output gets a disjoint world island and each
    /// populated workspace becomes a reef on that island's lateral line. The
    /// camera points at the previously-active workspace's reef, and floating
    /// windows translate from output-global to reef-local world coordinates.
    ///
    /// Scratchpad workspaces have no place in the world: their trees and
    /// unpinned floaters stay in `layout`/`floating_workspace`, hidden
    /// because Ocean renders neither, and come back as they were.
    /// `float_stack` is the floating stacking order, topmost first.
    pub(super) fn migrate_classic_to_ocean(&mut self, float_stack: &[WlSurface]) {
        // Depth-deck windows have no Ocean counterpart: recall them to
        // their workspace tiles first so the tree move carries them. The
        // deck picker overlay is Classic UI and must not linger.
        for entry in self.spatial.workspaces.classic_depth.drain() {
            self.spatial.workspaces.layout.insert(
                &entry.output,
                entry.workspace,
                entry.window.clone(),
                None,
            );
        }
        self.depth_deck_overlay = None;
        self.spatial.workspaces.classic_depth.close();

        let output_viewports: Vec<(String, Rectangle<i32, Logical>)> = self
            .space
            .outputs()
            .filter_map(|output| Some((output.name(), self.space.output_geometry(output)?)))
            .collect();

        // Snapshot the active workspace per output before draining. A shown
        // scratchpad points the camera at the workspace it was opened from.
        let active: std::collections::HashMap<String, u32> = output_viewports
            .iter()
            .map(|(name, _)| {
                let mut workspace = self.spatial.workspaces.layout.active_workspace(name);
                if is_scratchpad_workspace(workspace) {
                    workspace = self
                        .spatial
                        .workspaces
                        .scratchpad_previous
                        .get(name)
                        .copied()
                        .unwrap_or(1);
                }
                (name.clone(), workspace)
            })
            .collect();

        // Drain the trees first (each is unique per (output, workspace)).
        let mut trees = self
            .spatial
            .workspaces
            .layout
            .drain_for_migration(is_scratchpad_workspace);
        // Pinned floaters show on every workspace, so they always migrate.
        let moving: Vec<WlSurface> = self
            .spatial
            .workspaces
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                !is_scratchpad_workspace(tag.workspace)
                    || self.spatial.workspaces.pinned.contains(*surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        let mut floating: Vec<(WlSurface, FloatingTag)> = moving
            .into_iter()
            .filter_map(|surface| {
                let mut tag = self
                    .spatial
                    .workspaces
                    .floating_workspace
                    .remove(&surface)?;
                if is_scratchpad_workspace(tag.workspace) {
                    tag.workspace = active.get(&tag.output).copied().unwrap_or(1);
                }
                Some((surface, tag))
            })
            .collect();
        // Pushed topmost first, so Ocean's front-to-back stack keeps the
        // Classic order; floaters with no known position go underneath.
        floating.sort_by_key(|(surface, _)| {
            float_stack
                .iter()
                .position(|s| s == surface)
                .unwrap_or(usize::MAX)
        });
        let reef_gap = self.config.gaps.max(0);

        // Assign disjoint islands before placing per-output reef sequences.
        let mut max_workspaces = std::collections::HashMap::new();
        for (output_name, _) in &output_viewports {
            let max_workspace = trees
                .iter()
                .filter(|(name, _, _)| name == output_name)
                .map(|(_, workspace, _)| *workspace)
                .chain(
                    floating
                        .iter()
                        .filter(|(_, tag)| &tag.output == output_name)
                        .map(|(_, tag)| tag.workspace),
                )
                .chain(active.get(output_name).copied())
                .max()
                .unwrap_or(1);
            max_workspaces.insert(output_name.clone(), max_workspace);
        }
        let island_x = ocean_island_origins(&output_viewports, &max_workspaces, reef_gap);

        // Clear Classic-only pseudo-tile state; groups and pin semantics migrate.
        self.spatial.workspaces.pseudo_tiled.clear();

        for (output_name, geometry) in &output_viewports {
            let base_x = island_x.get(output_name).copied().unwrap_or(0);
            let viewport = geometry.size;
            // Move this output's trees into reefs, workspace N at
            // X = (N-1) * stride on the lateral line.
            let mut i = 0;
            while i < trees.len() {
                if trees[i].0 != *output_name {
                    i += 1;
                    continue;
                }
                let (_, workspace, tree) = trees.remove(i);
                let reef_x = ocean_reef_x(base_x, workspace, viewport.w, reef_gap);
                let reef_rect = Rectangle::new(Point::from((reef_x, 0)), viewport);
                let reef_name = self
                    .config
                    .workspace_names
                    .iter()
                    .find_map(|(name, number)| (*number == workspace).then(|| name.clone()))
                    .unwrap_or_else(|| format!("ws-{workspace}"));
                self.spatial
                    .ocean
                    .push_migrated_reef(reef_name, reef_rect, tree, output_name);
            }
            // Point the camera at the previously-active workspace's reef.
            let active_ws = active.get(output_name).copied().unwrap_or(1);
            let cam_x = f64::from(base_x)
                + (active_ws as f64 - 1.0) * (viewport.w as f64 + f64::from(reef_gap));
            self.spatial
                .ocean
                .set_camera_origin(output_name, crate::ocean::OceanPoint { x: cam_x, y: 0.0 });
        }

        // Floating windows translate to world coordinates around their
        // workspace's reef; pinned windows additionally get an Ocean
        // screen pin so the pin semantic survives the crossing. The
        // cameras were set above, which `pin_to_screen` needs to compute
        // its viewport anchor.
        for (surface, tag) in floating {
            let output = output_viewports
                .iter()
                .find(|(name, _)| name == &tag.output);
            let Some((output_name, output_geometry)) = output else {
                // Preserve real geometry instead of guessing another
                // output's dimensions if this output disappeared.
                self.spatial.ocean.push_migrated_floating(
                    surface,
                    tag.window,
                    tag.rect,
                    &tag.output,
                );
                continue;
            };
            let base_x = island_x.get(output_name).copied().unwrap_or(0);
            let reef_x = ocean_reef_x(base_x, tag.workspace, output_geometry.size.w, reef_gap);
            let reef_rect = Rectangle::new((reef_x, 0).into(), output_geometry.size);
            let world_rect = classic_rect_to_ocean(tag.rect, *output_geometry, reef_rect);
            let was_pinned = self.spatial.workspaces.pinned.contains(&surface);
            self.spatial.ocean.push_migrated_floating(
                surface.clone(),
                tag.window,
                world_rect,
                output_name,
            );
            if was_pinned {
                self.spatial.ocean.pin_to_screen(&surface, output_name);
            }
        }
        // `pinned` is a Classic-only set; Ocean represents pin status via
        // screen pins. Every pinned floater migrated above, so clearing it
        // keeps the debug-build invariant (pinned windows stay in
        // `floating_workspace`) true for the parked scratchpad floaters.
        self.spatial.workspaces.pinned.clear();

        // Workspace-switch bookkeeping is Classic-only.
        self.spatial.workspaces.scratchpad_previous.clear();
        self.spatial.workspaces.workspace_previous.clear();
    }

    /// Ocean -> Classic. Reefs sorted left-to-right become workspaces 1..N
    /// on their owner in independent mode, or the nearest camera in shared
    /// mode. The active workspace follows each camera. Floating windows
    /// keep their connected output and select its nearest reef; screen pins
    /// keep their viewport rectangle and re-enter the Classic `pinned` set.
    pub(super) fn migrate_ocean_to_classic(&mut self) {
        let (reefs, floating, cameras, entry_outputs, pinned_surfaces) =
            self.spatial.ocean.drain_for_classic();

        let output_viewports: std::collections::HashMap<String, Rectangle<i32, Logical>> = self
            .space
            .outputs()
            .filter_map(|output| Some((output.name(), self.space.output_geometry(output)?)))
            .collect();
        let output_names: Vec<String> = self
            .space
            .outputs()
            .filter(|output| output_viewports.contains_key(&output.name()))
            .map(|output| output.name())
            .collect();

        // Sort reefs left-to-right so workspace numbering is stable.
        let mut sorted_reefs = reefs;
        sorted_reefs.sort_by_key(|(_, rect, _)| rect.loc.x);

        let camera_center = |output: &str| {
            let camera = cameras.get(output).copied().unwrap_or_default();
            let viewport = output_viewports
                .get(output)
                .map(|rect| rect.size)
                .unwrap_or_default();
            crate::ocean::OceanPoint {
                x: camera.origin.x + viewport.w as f64 / camera.zoom.max(0.05) / 2.0,
                y: camera.origin.y + viewport.h as f64 / camera.zoom.max(0.05) / 2.0,
            }
        };

        // Nearest-output helper for one reef. Ocean is two-dimensional:
        // vertically separated cameras must not tie solely on X.
        let nearest_output = |rect: &Rectangle<i32, Logical>| -> String {
            let center_x = rect.loc.x as f64 + rect.size.w as f64 / 2.0;
            let center_y = rect.loc.y as f64 + rect.size.h as f64 / 2.0;
            output_names
                .iter()
                .min_by(|a, b| {
                    let cam_a = camera_center(a);
                    let cam_b = camera_center(b);
                    let dist_a = (cam_a.x - center_x).powi(2) + (cam_a.y - center_y).powi(2);
                    let dist_b = (cam_b.x - center_x).powi(2) + (cam_b.y - center_y).powi(2);
                    dist_a.total_cmp(&dist_b)
                })
                .cloned()
                .unwrap_or_default()
        };

        // Workspace counter per output, the workspace number each output's
        // camera is currently looking at (for active selection), and the
        // reef rects assigned so floating windows can find their nearest
        // workspace afterward.
        let mut ws_counters: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        let mut nearest_ws: std::collections::HashMap<String, (f64, u32)> =
            std::collections::HashMap::new();
        let mut reef_slots: Vec<(String, u32, Rectangle<i32, Logical>)> = Vec::new();

        for (_, rect, tree) in sorted_reefs {
            let target_output = (!self.config.ocean.shared_canvas)
                .then(|| {
                    tree.windows().iter().find_map(|window| {
                        entry_outputs
                            .get(window.toplevel()?.wl_surface())
                            .filter(|name| output_viewports.contains_key(*name))
                            .cloned()
                    })
                })
                .flatten()
                .unwrap_or_else(|| nearest_output(&rect));
            let ws = ws_counters.entry(target_output.clone()).or_insert(1);
            self.spatial
                .workspaces
                .layout
                .insert_migrated_tree(target_output.clone(), *ws, tree);
            let center_x = rect.loc.x as f64 + rect.size.w as f64 / 2.0;
            let center_y = rect.loc.y as f64 + rect.size.h as f64 / 2.0;
            let camera = camera_center(&target_output);
            let dist = (camera.x - center_x).powi(2) + (camera.y - center_y).powi(2);
            match nearest_ws.get(&target_output) {
                Some((best, _)) if *best <= dist => {}
                _ => {
                    nearest_ws.insert(target_output.clone(), (dist, *ws));
                }
            }
            reef_slots.push((target_output, *ws, rect));
            *ws += 1;
        }

        // Set active workspaces from the cameras.
        for output_name in &output_names {
            let active = nearest_ws.get(output_name).map(|(_, ws)| *ws).unwrap_or(1);
            self.spatial
                .workspaces
                .layout
                .set_active_workspace(output_name, active);
        }

        // Restrict nearest-reef selection to the floater's connected output.
        // An output with only floaters must not lose them to another's reef.
        for (surface, window, world_rect) in floating {
            let preferred_output = pinned_surfaces
                .get(&surface)
                .map(|(output, _)| output)
                .or_else(|| entry_outputs.get(&surface))
                .filter(|output| output_viewports.contains_key(*output));
            let nearest_slot = ocean_classic_slot(
                world_rect,
                &reef_slots,
                preferred_output.map(String::as_str),
            );
            let (target_output, ws, reef_rect) = if let Some((output, ws, rect)) = nearest_slot {
                (output.clone(), *ws, *rect)
            } else {
                let output = preferred_output
                    .cloned()
                    .unwrap_or_else(|| nearest_output(&world_rect));
                let Some(viewport) = output_viewports.get(&output).copied() else {
                    tracing::warn!(%output, "preserving floating world geometry without output geometry");
                    let output = entry_outputs.get(&surface).cloned().unwrap_or(output);
                    self.spatial.workspaces.floating_workspace.insert(
                        surface,
                        FloatingTag {
                            window,
                            output,
                            workspace: 1,
                            rect: world_rect,
                        },
                    );
                    continue;
                };
                let camera = cameras.get(&output).copied().unwrap_or_default();
                (
                    output,
                    1,
                    Rectangle::new(
                        (camera.origin.x as i32, camera.origin.y as i32).into(),
                        viewport.size,
                    ),
                )
            };
            let Some(output_geometry) = output_viewports.get(&target_output).copied() else {
                tracing::warn!(%target_output, "preserving floating world geometry without output geometry");
                let output = entry_outputs
                    .get(&surface)
                    .cloned()
                    .unwrap_or(target_output);
                self.spatial.workspaces.floating_workspace.insert(
                    surface,
                    FloatingTag {
                        window,
                        output,
                        workspace: ws,
                        rect: world_rect,
                    },
                );
                continue;
            };
            let local_rect = if let Some((_, pin_rect)) = pinned_surfaces.get(&surface) {
                ocean_rect_to_classic(
                    *pin_rect,
                    Rectangle::new(Point::default(), output_geometry.size),
                    output_geometry,
                )
            } else {
                ocean_rect_to_classic(world_rect, reef_rect, output_geometry)
            };
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(local_rect.size));
                toplevel.send_pending_configure();
            }
            self.spatial.workspaces.floating_workspace.insert(
                surface,
                FloatingTag {
                    window,
                    output: target_output.clone(),
                    workspace: ws,
                    rect: local_rect,
                },
            );
        }

        // Screen-pinned windows re-enter the Classic `pinned` set (their
        // floating entries were created just above; Classic's invariant is
        // that pinned windows stay floating, which holds).
        for surface in pinned_surfaces.into_keys() {
            if self
                .spatial
                .workspaces
                .floating_workspace
                .contains_key(&surface)
            {
                self.spatial.workspaces.pinned.insert(surface);
            }
        }

        // Ocean groups identify their reef through the active leaf rather
        // than a Classic workspace key. Once reefs have been assigned to
        // Classic trees, refresh the cached owner used for activation,
        // accessibility, and later ungroup operations.
        for group in &mut self.groups {
            let active_surface = &group.members[group.active].surface;
            if let (Some(output), Some(workspace)) = (
                self.spatial.workspaces.layout.output_of(active_surface),
                self.spatial.workspaces.layout.workspace_of(active_surface),
            ) {
                group.output = output.to_string();
                group.workspace = workspace;
            }
        }

        // Remaining Ocean-only entry-output hints have no Classic counterpart.
    }

    /// Ocean directional focus projects every world rectangle through the
    /// current camera before comparing direction, so on-screen and off-screen
    /// candidates share one coordinate space. Screen pins substitute their
    /// real viewport placement instead of their dormant world rectangle.
    pub(super) fn ocean_focus_target(
        &self,
        focused: &WlSurface,
        output: &Output,
        direction: Direction,
    ) -> Option<(Window, Rectangle<i32, Logical>, bool)> {
        let output_geo = self.space.output_geometry(output)?;
        let visible = self.render_placements(output)?;
        let layouts = self
            .spatial
            .ocean
            .world_layouts(self.config.gaps, self.config.bsp_split_bias);
        let current = layouts.iter().find(|(window, _, _)| {
            window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == focused)
        })?;
        let camera = self.spatial.ocean.camera(&output.name());
        let current_view = visible
            .iter()
            .find(|placement| placement.surface() == Some(focused))
            .map(|placement| placement.rect)
            .unwrap_or_else(|| crate::ocean::world_to_view_rect(current.1, camera, output_geo.loc));
        let (window, rect, _) = layouts
            .iter()
            .filter_map(|(window, rect, _)| {
                let surface = window.toplevel().map(|t| t.wl_surface())?;
                if surface == focused
                    || !self
                        .spatial
                        .ocean
                        .visible_on_output(surface, &output.name())
                    || self
                        .fullscreen
                        .get(surface)
                        .is_some_and(|entry| entry.output != output.name())
                {
                    return None;
                }
                let visible_rect = visible
                    .iter()
                    .find(|placement| placement.surface() == Some(surface))
                    .map(|placement| placement.rect);
                let view_rect = match visible_rect {
                    Some(rect) => rect,
                    None if self.spatial.ocean.is_screen_pinned(surface) => return None,
                    None => crate::ocean::world_to_view_rect(*rect, camera, output_geo.loc),
                };
                let score = directional_score(current_view, view_rect, direction)?;
                Some((window.clone(), *rect, score))
            })
            .min_by(|(_, _, a), (_, _, b)| a.total_cmp(b))?;
        let surface = window.toplevel()?.wl_surface();
        let already_visible = visible
            .iter()
            .any(|placement| placement.surface() == Some(surface));
        Some((window, rect, already_visible))
    }
}
