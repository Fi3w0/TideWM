use std::collections::{HashMap, HashSet};

use smithay::{
    backend::renderer::{
        element::{
            solid::{SolidColorBuffer, SolidColorRenderElement},
            surface::WaylandSurfaceRenderElement,
            AsRenderElements, Kind,
        },
        gles::{GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture},
    },
    desktop::{layer_map_for_output, space::SpaceRenderElements, WindowSurfaceType},
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Physical, Point, Rectangle, Scale, Size},
    wayland::shell::wlr_layer::Layer as WlrLayer,
};

use super::{SessionLock, Smallvil};

impl Smallvil {
    fn base_window_visual_sample(
        &self,
        surface: Option<&WlSurface>,
    ) -> crate::window_animation::VisualSample {
        let mut sample = crate::window_animation::VisualSample::default();
        if let Some(surface) = surface {
            let (buoyancy_sink, buoyancy_flow_scale) = self
                .window_buoyancy
                .get(surface)
                .map(|state| {
                    let config = self.config.buoyancy;
                    state.sample(config.max_sink as f64, config.flow_reduction as f64)
                })
                .unwrap_or((0.0, 1.0));
            // Weight changes flow only in Ocean. Classic deliberately keeps
            // its existing physics feel and receives just the visual sink.
            let flow_scale = if self.config.spatial_engine.ownership()
                == crate::config::SpatialOwnership::Ocean
            {
                buoyancy_flow_scale
            } else {
                1.0
            };
            if let Some(open) = self.window_open_animations.get(surface) {
                let current = open.sample();
                sample.offset += current.offset;
                sample.opacity *= current.opacity;
            }
            if let Some(viscosity) = self.window_viscosity.get(surface) {
                if let Some(window) = self.mapped_toplevel_window(surface) {
                    if let Some(location) = self.space.element_location(&window) {
                        let current = viscosity.sample();
                        sample.offset += Point::from((
                            current.loc.x - location.x as f64,
                            current.loc.y - location.y as f64,
                        ));
                        sample.size = Some(current.size);
                    }
                }
            } else if let Some(movement) = self.window_move_animations.get(surface) {
                let current = movement.sample();
                sample.offset += current.offset;
                sample.size = current.size;
                sample.opacity *= current.opacity;
            }
            if let Some(sway) = self.window_sway.get(surface) {
                sample.offset.x += sway.sample();
            }
            if let Some(physics) = self.window_float_physics.get(surface) {
                let (ox, oy) = physics.sample();
                sample.offset.x += ox * flow_scale;
                sample.offset.y += oy * flow_scale;
            }
            if let Some(body) = self.window_float_bodies.get(surface) {
                sample.offset.x += body.offset.0 * flow_scale;
                sample.offset.y += body.offset.1 * flow_scale;
            }
            // Ambient sway is a continuous function of elapsed time, not a
            // pruned/decaying record like the two above -- sampled fresh
            // every frame straight from `Instant::now()`, no per-frame tick
            // needed to drive it. `is_floating` re-checked here (not just
            // at toggle time) so a window that stops floating after being
            // toggled on doesn't keep swaying off its tile.
            if let Some(start) = self.window_float_ambient.get(surface) {
                if self.is_floating(surface) {
                    let config = self.config.float_physics;
                    let (ax, ay) = crate::float_physics::ambient_sample(
                        start.elapsed().as_secs_f64(),
                        config.max_offset as f64,
                        config.ambient_period_s as f64,
                    );
                    sample.offset.x += ax * flow_scale;
                    sample.offset.y += ay * flow_scale;
                }
            }
            if self.config.water_effects
                && self.config.spatial_engine.ownership() == crate::config::SpatialOwnership::Ocean
                && self.config.currents.enabled
            {
                if let Some(current) = self.window_currents.get(surface) {
                    let config = self.config.currents;
                    let (cx, cy) = current.sample(
                        config.direction_degrees as f64,
                        config.strength as f64,
                        config.period_ms as f64 / 1000.0,
                    );
                    sample.offset.x += cx * flow_scale;
                    sample.offset.y += cy * flow_scale;
                }
            }
            sample.offset.y += buoyancy_sink;
        }
        sample
    }

    /// Combines lifecycle/mechanical animation with the spatial engine's
    /// model-neutral camera transform for one render placement.
    pub(super) fn placement_visual_sample(
        &self,
        placement: &crate::placement::PlacedWindow,
    ) -> crate::window_animation::VisualSample {
        let mut sample = self.base_window_visual_sample(placement.surface());
        // Ocean's interactive viscosity is sampled in world coordinates,
        // while this shared renderer consumes view pixels. The ordinary
        // placement already applies the camera transform to `rect`; apply
        // the same transform to the temporary viscosity correction or a
        // zoomed-out drag makes the window grow and a zoomed-in drag makes it
        // shrink for the duration of the grab.
        if self.config.spatial_engine.ownership() == crate::config::SpatialOwnership::Ocean {
            if let Some(surface) = placement.surface() {
                if let Some(viscosity) = self.window_viscosity.get(surface) {
                    if let Some(window) = self.mapped_toplevel_window(surface) {
                        if let Some(location) = self.space.element_location(&window) {
                            let current = viscosity.sample();
                            let scale = placement.view_scale.max(0.05);
                            let world_offset: Point<f64, Logical> = Point::from((
                                current.loc.x - location.x as f64,
                                current.loc.y - location.y as f64,
                            ));
                            sample.offset += Point::from((
                                world_offset.x * (scale - 1.0),
                                world_offset.y * (scale - 1.0),
                            ));
                            sample.size =
                                Some(Size::from((current.size.w * scale, current.size.h * scale)));
                        }
                    }
                }
            }
        }
        sample.offset += placement.view_offset;
        if placement.fits_content_to_placement() && sample.size.is_none() {
            sample.size = Some(placement.rect.size.to_f64());
        }
        sample
    }

    /// Produces Classic's complete front-to-back window scene through the
    /// model-neutral placement boundary. Active `Space` windows are
    /// authoritative; bounded swim neighbors are previews and remain absent
    /// from input/IPC ownership.
    fn classic_render_placements(
        &self,
        output: &Output,
    ) -> Option<Vec<crate::placement::PlacedWindow>> {
        let output_name = output.name();
        let output_geo = self.space.output_geometry(output)?;
        let camera_offset = self
            .swim_cameras
            .get(&output_name)
            .filter(|_| self.swim_enabled())
            .map(|camera| camera.current_offset())
            .unwrap_or(0.0);
        let output_width = output_geo.size.w as f64;
        let output_height = output_geo.size.h as f64;
        let vertical_offset = self
            .workspace_scroll_cameras
            .get(&output_name)
            .map(|camera| camera.current_offset())
            .unwrap_or(0.0);
        let active_view_offset = Point::from((
            -(camera_offset as f64) * output_width,
            -(vertical_offset as f64) * output_height,
        ));
        let scrolling = self.config.spatial_engine.projection()
            == Some(crate::config::WorkspaceProjection::Scroll);
        let mut result: Vec<crate::placement::PlacedWindow> = self
            .space
            .elements_for_output(output)
            .filter(|_| !scrolling)
            .chain(self.space.elements().filter(|_| scrolling))
            .rev()
            .filter_map(|window| {
                let location = self.space.element_location(window)?;
                let surface = window.toplevel().map(|toplevel| toplevel.wl_surface());
                let kind = if surface
                    .is_some_and(|surface| self.floating_workspace.contains_key(surface))
                {
                    crate::placement::PlacementKind::Floating
                } else {
                    crate::placement::PlacementKind::Tiled
                };
                // A tiled window belongs to exactly one output's layout.
                // Its Space geometry is location + last committed size, so
                // until it commits the tile size a large first buffer near
                // an edge overlaps the neighbor output and would flash there.
                if kind == crate::placement::PlacementKind::Tiled
                    && surface != self.classic_tile_drag.as_ref()
                    && surface
                        .and_then(|surface| self.layout.output_of(surface))
                        .is_some_and(|owner| owner != output_name)
                {
                    return None;
                }
                // A strip-anchored floater scrolled off its own output must
                // not appear on the neighbor, exactly like a tile -- unless
                // it is being dragged there.
                if kind == crate::placement::PlacementKind::Floating
                    && scrolling
                    && surface.is_some_and(|surface| {
                        !self.floating_dragging.contains(surface)
                            && self.scroll_strip_floater(surface)
                            && self
                                .floating_workspace
                                .get(surface)
                                .is_some_and(|tag| tag.output != output_name)
                    })
                {
                    return None;
                }
                let stack = if surface.is_some_and(|surface| self.fullscreen.contains_key(surface))
                {
                    crate::placement::PlacementStack::Fullscreen
                } else {
                    crate::placement::PlacementStack::Normal
                };
                // Pinned windows stay on screen through every workspace
                // change, so a vertical workspace slide must not carry them
                // out of view. (Horizontal swim offset is left untouched,
                // matching its existing behavior.)
                let view_offset = if surface.is_some_and(|surface| self.pinned.contains(surface)) {
                    Point::from((active_view_offset.x, 0.0))
                } else {
                    active_view_offset
                };
                let placement = crate::placement::PlacedWindow::authoritative(
                    window.clone(),
                    Rectangle::new(location, window.geometry().size),
                )
                .with_view_offset(view_offset)
                .with_kind(kind)
                .with_stack(stack);
                // Space's cached output membership is refreshed later in the
                // backend loop and tracks final geometry, not moving columns.
                // Scroll resolves its current visible rectangle directly.
                if scrolling && !self.placement_input_rect(&placement).overlaps(output_geo) {
                    return None;
                }
                Some(placement)
            })
            .collect();

        let vertical_camera = self.workspace_scroll_cameras.get(&output_name);
        if !self.swim_enabled() && vertical_camera.is_none() {
            return Some(result);
        }
        let Some(area) = self.output_tiling_area(output) else {
            return Some(result);
        };
        if self.swim_enabled() {
            let neighbors = crate::swim::visible_neighbors(
                self.layout.active_workspace(&output_name),
                camera_offset,
                self.config.swim.neighbors,
            );
            for neighbor in neighbors {
                let view_offset = Point::from((
                    (neighbor.delta as f64 - camera_offset as f64) * output_width,
                    0.0,
                ));
                self.append_workspace_previews(
                    &output_name,
                    neighbor.workspace,
                    area,
                    output_geo,
                    view_offset,
                    &mut result,
                );
            }
        }
        if let Some(camera) = vertical_camera {
            if let Some(previous) = camera.neighbor() {
                let active = self.layout.active_workspace(&output_name);
                let delta = i64::from(previous) - i64::from(active);
                if delta != 0 {
                    let view_offset = Point::from((
                        0.0,
                        (delta as f64 - camera.current_offset() as f64) * output_height,
                    ));
                    self.append_workspace_previews(
                        &output_name,
                        previous,
                        area,
                        output_geo,
                        view_offset,
                        &mut result,
                    );
                }
            }
        }
        Some(result)
    }

    /// Appends one workspace's floating and tiled windows as non-interactive
    /// preview placements at `view_offset`. Shared by horizontal swim and the
    /// vertical workspace slide so both render the same live content.
    fn append_workspace_previews(
        &self,
        output_name: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        output_geo: Rectangle<i32, Logical>,
        view_offset: Point<f64, Logical>,
        result: &mut Vec<crate::placement::PlacedWindow>,
    ) {
        // Floating windows are frontmost among ordinary windows. Their
        // relative order is already not retained while hidden (the
        // workspace-switch map pass reads this same bounded tag map),
        // so using its iteration order matches the existing model.
        result.extend(
            self.floating_workspace
                .iter()
                .filter(|(surface, tag)| {
                    tag.output == output_name
                        && tag.workspace == workspace
                        && !self.pinned.contains(*surface)
                })
                .map(|(surface, tag)| {
                    let mut rect = tag.rect;
                    if self
                        .fullscreen
                        .get(surface)
                        .is_some_and(|entry| entry.output == output_name)
                    {
                        rect = output_geo;
                    } else if self
                        .maximized
                        .get(surface)
                        .is_some_and(|entry| entry.output == output_name)
                    {
                        rect = crate::layout::inset(area, self.gaps_for(output_name, workspace));
                    }
                    let stack = if self
                        .fullscreen
                        .get(surface)
                        .is_some_and(|entry| entry.output == output_name)
                    {
                        crate::placement::PlacementStack::Fullscreen
                    } else {
                        crate::placement::PlacementStack::Normal
                    };
                    crate::placement::PlacedWindow::preview(tag.window.clone(), rect)
                        .with_view_offset(view_offset)
                        .with_kind(crate::placement::PlacementKind::Floating)
                        .with_stack(stack)
                }),
        );

        let mut tiled = self.layout.layout(
            output_name,
            workspace,
            area,
            self.gaps_for(output_name, workspace),
        );
        // Mirror the active `Space` walk's front-to-back direction.
        // Tiled windows usually do not overlap, but cascade does.
        tiled.reverse();
        result.extend(tiled.into_iter().map(|(window, mut rect)| {
            let mut stack = crate::placement::PlacementStack::Normal;
            if let Some(surface) = window.toplevel().map(|toplevel| toplevel.wl_surface()) {
                if self
                    .fullscreen
                    .get(surface)
                    .is_some_and(|entry| entry.output == output_name)
                {
                    rect = output_geo;
                    stack = crate::placement::PlacementStack::Fullscreen;
                } else if self.pseudo_tiled.contains(surface) {
                    rect = crate::layout::scale_centered(rect, self.config.pseudo_tile_scale);
                }
            }
            crate::placement::PlacedWindow::preview(window, rect)
                .with_view_offset(view_offset)
                .with_stack(stack)
        }));
    }

    /// Spatial-engine boundary consumed by the shared renderer. Classic and
    /// Ocean own different spatial state but produce the same scene contract.
    pub(crate) fn render_placements(
        &self,
        output: &Output,
    ) -> Option<Vec<crate::placement::PlacedWindow>> {
        match self.config.spatial_engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => self.classic_render_placements(output),
            crate::config::SpatialOwnership::Ocean => {
                let output_geo = self.space.output_geometry(output)?;
                let mut placements = self.ocean.placements(
                    &output.name(),
                    output_geo,
                    self.config.gaps,
                    self.config.bsp_split_bias,
                );
                placements.retain_mut(|placement| {
                    let Some(surface) = placement.surface() else {
                        return true;
                    };
                    if let Some(fullscreen) = self.fullscreen.get(surface) {
                        if fullscreen.output != output.name() {
                            return false;
                        }
                        placement.rect = output_geo;
                        placement.view_offset = Point::from((0.0, 0.0));
                        placement.view_scale = 1.0;
                        placement.stack = crate::placement::PlacementStack::Fullscreen;
                        return true;
                    }
                    if self
                        .maximized
                        .get(surface)
                        .is_some_and(|entry| entry.output == output.name())
                    {
                        let area = self.output_tiling_area(output).unwrap_or(output_geo);
                        placement.rect = crate::layout::inset(area, self.config.gaps);
                        placement.view_offset = Point::from((0.0, 0.0));
                        placement.view_scale = 1.0;
                    }
                    true
                });
                Some(placements)
            }
        }
    }

    /// Builds the front-to-back desktop stack with explicit fullscreen/layer
    /// ordering. `skip` removes window surfaces during backdrop capture;
    /// `glass_layers` are reinserted immediately behind their owning windows.
    #[allow(clippy::mutable_key_type, clippy::too_many_arguments)] // WlSurface-keyed scene layers
    pub(crate) fn desktop_render_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        placements: &[crate::placement::PlacedWindow],
        skip: &[WlSurface],
        depth_layers: &mut HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>>,
        glass_layers: &mut HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>>,
        backdrop: Vec<crate::backend::udev::OutputRenderElements>,
    ) -> Option<Vec<crate::backend::udev::OutputRenderElements>> {
        self.space.output_geometry(output)?;
        let mut result = Vec::new();
        let shadow_program = self
            .shadows_possible()
            .then(|| crate::shadow::shadow_program(&mut self.shadow_program, renderer))
            .flatten();
        let rounded_program = self
            .rounding_or_cascade_shader_possible()
            .then(|| {
                crate::decoration::rounded_surface_program(
                    &mut self.rounded_surface_program,
                    renderer,
                )
            })
            .flatten();
        let border_program = self
            .borders_possible()
            .then(|| crate::decoration::border_program(&mut self.border_program, renderer))
            .flatten();
        #[allow(clippy::too_many_arguments, clippy::mutable_key_type)]
        fn append_windows(
            state: &mut Smallvil,
            renderer: &mut GlesRenderer,
            output: &Output,
            fullscreen: bool,
            skip: &[WlSurface],
            shadow_program: Option<&GlesPixelProgram>,
            rounded_program: Option<&GlesTexProgram>,
            border_program: Option<&GlesPixelProgram>,
            placements: &[crate::placement::PlacedWindow],
            depth_layers: &mut HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>>,
            glass_layers: &mut HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>>,
            result: &mut Vec<crate::backend::udev::OutputRenderElements>,
        ) {
            let Some(output_geo) = state.space.output_geometry(output) else {
                return;
            };
            let output_scale = output.current_scale().fractional_scale();
            let scale = Scale::from(output_scale);
            for entry in placements {
                let window = &entry.window;
                let surface = entry.surface();
                if entry.is_fullscreen() != fullscreen {
                    continue;
                }
                if entry.replacement_eligible() {
                    if let Some(layers) = surface.and_then(|surface| depth_layers.remove(surface)) {
                        result.extend(layers);
                    }
                }
                // `skip` contains active-space surfaces rebuilt by depth or
                // glass passes. Neighbor previews have no replacement pass;
                // a stale cached glass surface can still appear in that list,
                // so never suppress a render-only neighbor with it.
                if entry.replacement_eligible()
                    && surface.is_some_and(|surface| skip.contains(surface))
                {
                    continue;
                }
                let visual = state.placement_visual_sample(entry);
                let alpha = surface
                    .map(|surface| state.window_render_alpha_for(surface, entry.is_fullscreen()))
                    .unwrap_or(1.0)
                    * surface
                        .map(|surface| state.depth_live_alpha(surface))
                        .unwrap_or(1.0)
                    * visual.opacity;
                let visual_offset: Point<i32, Logical> = Point::from((
                    visual.offset.x.round() as i32,
                    visual.offset.y.round() as i32,
                ));
                let visual_size = visual.rounded_size_or(window.geometry().size);
                let render_location =
                    entry.rect.loc - output_geo.loc - window.geometry().loc + visual_offset;
                if let Some(surface) = surface {
                    let (popups, main) = state.window_surface_elements(
                        renderer,
                        output,
                        window,
                        surface,
                        render_location,
                        visual_size,
                        alpha,
                        rounded_program.cloned(),
                        entry.is_floating(),
                        entry.is_fullscreen(),
                    );
                    result.extend(popups);
                    if let Some(program) = border_program {
                        if let Some(border) = state.window_border_element(
                            output,
                            entry,
                            surface,
                            program.clone(),
                            visual,
                        ) {
                            result.push(crate::backend::udev::OutputRenderElements::Border(border));
                        }
                    }
                    result.extend(main);
                } else {
                    result.extend(
                        window
                            .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                                renderer,
                                render_location.to_physical_precise_round(output_scale),
                                scale,
                                alpha,
                            )
                            .into_iter()
                            .map(SpaceRenderElements::Surface)
                            .map(crate::backend::udev::OutputRenderElements::Space),
                    );
                }
                // The glass layer sits directly behind its own window's
                // surface, in the window's real z-slot -- not hoisted above
                // everything. Front-to-back (index 0 topmost): surface,
                // then glass, then shadow. Skipped windows (depth-replaced)
                // have no glass layer here; their replacement pass owns them.
                if let Some(layers) = surface.and_then(|s| glass_layers.remove(s)) {
                    result.extend(layers);
                }
                if let (Some(surface), Some(program)) = (surface, shadow_program) {
                    if let Some(shadow) =
                        state.window_shadow_element(output, entry, surface, program.clone(), visual)
                    {
                        result.push(crate::backend::udev::OutputRenderElements::Shadow(shadow));
                    }
                }
            }
        }

        #[allow(clippy::too_many_arguments, clippy::mutable_key_type)]
        fn append_layers(
            state: &Smallvil,
            renderer: &mut GlesRenderer,
            output: &Output,
            kinds: &[WlrLayer],
            skip: &[WlSurface],
            glass_layers: &mut HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>>,
            result: &mut Vec<crate::backend::udev::OutputRenderElements>,
        ) {
            let output_scale = output.current_scale().fractional_scale();
            let scale = Scale::from(output_scale);
            let layer_map = layer_map_for_output(output);
            for kind in kinds {
                // Natural stacking order (last-mapped topmost) unless a
                // `layer_rule { z_order = N }` overrides it -- stable sort
                // so untouched namespaces (the common case, all default 0)
                // keep exactly the order `.rev()` already gave them.
                let mut layers: Vec<_> = layer_map.layers_on(*kind).rev().collect();
                layers.sort_by_key(|layer| {
                    std::cmp::Reverse(state.config.layer_z_order(layer.namespace()).unwrap_or(0))
                });
                for layer in layers {
                    if state.unmapped_layer_surfaces.contains(layer.wl_surface())
                        || skip.contains(layer.wl_surface())
                    {
                        continue;
                    }
                    let Some(geometry) = layer_map.layer_geometry(layer) else {
                        continue;
                    };
                    result.extend(
                        layer
                            .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                                renderer,
                                geometry.loc.to_physical_precise_round(output_scale),
                                scale,
                                1.0,
                            )
                            .into_iter()
                            .map(SpaceRenderElements::Surface)
                            .map(crate::backend::udev::OutputRenderElements::Space),
                    );
                    // The frost layer for `layer_rule { blur = true }` sits
                    // directly behind the surface's own real per-pixel
                    // alpha, same "surface, then glass" order windows
                    // already use -- ordinary GL blending shows it through
                    // wherever the surface itself is translucent.
                    if let Some(layers) = glass_layers.remove(layer.wl_surface()) {
                        result.extend(layers);
                    }
                }
            }
        }

        append_windows(
            self,
            renderer,
            output,
            true,
            skip,
            shadow_program.as_ref(),
            rounded_program.as_ref(),
            border_program.as_ref(),
            placements,
            &mut *depth_layers,
            &mut *glass_layers,
            &mut result,
        );
        append_layers(
            self,
            renderer,
            output,
            &[WlrLayer::Overlay, WlrLayer::Top],
            skip,
            &mut *glass_layers,
            &mut result,
        );
        if let Some(amount) = self.layer_dim_amount(output) {
            if let Some(size) = self.space.output_geometry(output).map(|geo| geo.size) {
                let scale = output.current_scale().fractional_scale();
                let buffer = self
                    .layer_dim_buffers
                    .entry(output.clone())
                    .or_insert_with(|| SolidColorBuffer::new((0, 0), [0.0, 0.0, 0.0, 1.0]));
                buffer.update(size, [0.0, 0.0, 0.0, 1.0]);
                result.push(crate::backend::udev::OutputRenderElements::Dim(
                    SolidColorRenderElement::from_buffer(
                        buffer,
                        (0, 0),
                        Scale::from(scale),
                        amount,
                        Kind::Unspecified,
                    ),
                ));
            }
        }
        append_windows(
            self,
            renderer,
            output,
            false,
            skip,
            shadow_program.as_ref(),
            rounded_program.as_ref(),
            border_program.as_ref(),
            placements,
            &mut *depth_layers,
            &mut *glass_layers,
            &mut result,
        );
        // The canvas grid and caustics sit between windows and the
        // layer-shell wallpaper: awww/swaybg attach at the Background layer
        // inside the layer map, so they must be pushed BEFORE that layer
        // here (front-to-back, index 0 topmost) or the wallpaper would
        // cover them -- exactly what happened when these were spliced after
        // `space_elements` by the backends.
        result.extend(backdrop);
        append_layers(
            self,
            renderer,
            output,
            &[WlrLayer::Bottom, WlrLayer::Background],
            skip,
            &mut *glass_layers,
            &mut result,
        );
        Some(result)
    }

    /// The live output displaying the largest visible share of `surface`.
    /// Classic can use its authoritative `Space` geometry directly. Ocean
    /// must resolve through camera placements because world coordinates and
    /// physical output coordinates intentionally do not share an origin.
    pub(crate) fn rendered_output_for_surface(&self, surface: &WlSurface) -> Option<Output> {
        if self.config.spatial_engine.ownership() == crate::config::SpatialOwnership::Workspaces {
            let window = self.mapped_toplevel_window(surface)?;
            return self.output_for_window(&window);
        }

        self.space
            .outputs()
            .filter_map(|output| {
                let output_geo = self.space.output_geometry(output)?;
                let placement = self
                    .render_placements(output)?
                    .into_iter()
                    .find(|placement| {
                        placement
                            .surface()
                            .is_some_and(|candidate| candidate == surface)
                    })?;
                let rect = crate::placement::translated_rect(placement.rect, placement.view_offset);
                let intersection = output_geo.intersection(rect)?;
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

    /// The compositor-owned rectangle at which `surface` is actually drawn
    /// on `output`. In Ocean this includes camera translation and screen-pin
    /// placement; callers must not substitute the window's world `Space`
    /// geometry when anchoring output-local effects.
    pub(super) fn rendered_rect_on_output(
        &self,
        surface: &WlSurface,
        output: &Output,
    ) -> Option<Rectangle<i32, Logical>> {
        self.render_placements(output)?
            .into_iter()
            .find(|placement| {
                placement
                    .surface()
                    .is_some_and(|candidate| candidate == surface)
            })
            .map(|placement| {
                crate::placement::translated_rect(placement.rect, placement.view_offset)
            })
    }

    /// Marks that something changed and a frame needs to be composited.
    /// Backends should render only when this is true (or a toast is active)
    /// rather than redrawing every frame regardless of damage.
    pub fn request_redraw(&mut self) {
        self.redraw_requests.request_all();
        if let Some(wakeup) = &self.redraw_wakeup {
            wakeup.ping();
        }
        #[cfg(feature = "accessibility")]
        self.schedule_accessibility_sync();
    }

    /// Scope backend work to a live output. Capture and animation wakeups
    /// change pixels, not the accessibility tree. Coalescing is bounded by
    /// the live output set rather than a hardware-specific output count.
    pub(crate) fn request_output_redraw(&mut self, output: &Output) {
        if !self.space.outputs().any(|mapped| mapped == output) {
            return;
        }
        self.redraw_requests.request_output(output);
        if let Some(wakeup) = &self.redraw_wakeup {
            wakeup.ping();
        }
    }

    /// Damage a committed surface's current scene. The compositor calls this
    /// before and after role/geometry handling so both old and new extents
    /// are cleared. Lifecycle/layout/focus changes may still request global
    /// damage through their existing paths; unknown roles retain that fallback.
    #[allow(clippy::mutable_key_type)] // Stable Wayland protocol identity.
    pub(crate) fn request_surface_commit_redraw(&mut self, root: &WlSurface) {
        #[cfg(feature = "accessibility")]
        self.schedule_accessibility_sync();
        if self.mapped_toplevel_window(root).is_some() {
            self.request_surface_redraws(&HashSet::from([root.clone()]));
            return;
        }
        let output = self
            .space
            .outputs()
            .find(|output| {
                layer_map_for_output(output)
                    .layer_for_surface(root, WindowSurfaceType::TOPLEVEL)
                    .is_some()
            })
            .cloned();
        if let Some(output) = output {
            self.request_output_redraw(&output);
        } else {
            self.request_redraw();
        }
    }

    /// Queue cosmetic physics changes on every output that renders an
    /// affected window. Placement filtering preserves independent/shared Ocean
    /// views, screen pins, and Classic floating overlap without an owner cache.
    #[allow(clippy::mutable_key_type)] // Bounded set of stable Wayland protocol identities.
    pub(super) fn request_surface_redraws(&mut self, surfaces: &HashSet<WlSurface>) {
        if surfaces.is_empty()
            || self.redraw_requests.is_global()
            || !matches!(self.session_lock, SessionLock::Unlocked)
        {
            return;
        }
        // Mapping may have happened since the last backend frame; Classic's
        // per-output Space cache must reflect those live windows before use.
        self.refresh_space();
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let mut requested = false;
        for output in outputs {
            let Some(placements) = self.render_placements(&output) else {
                continue;
            };
            requested |=
                self.redraw_requests
                    .request_surface_changes(&output, &placements, surfaces);
        }
        if requested {
            if let Some(wakeup) = &self.redraw_wakeup {
                wakeup.ping();
            }
        }
    }

    pub(crate) fn install_redraw_wakeup(
        &mut self,
        wakeup: smithay::reexports::calloop::ping::Ping,
    ) {
        self.redraw_wakeup = Some(wakeup);
    }

    /// Whether a frame was explicitly marked dirty. Timed toasts request
    /// their next frame from the backend after each successful render;
    /// treating every live toast as implicitly dirty here would make a
    /// persistent error toast redraw forever at the timer's full rate.
    /// Clears the flag as a side effect, since the caller is about to render.
    pub(crate) fn take_redraw_requests(&mut self) -> crate::redraw::RedrawRequests {
        self.redraw_requests.take()
    }

    /// The area tiled windows lay out into on `output`: the output's own
    /// geometry with any layer-shell exclusive zone (a bar's height, say)
    /// subtracted, translated from `non_exclusive_zone()`'s output-local
    /// coordinates into `space`-global ones. Not yet gap-inset -- callers
    /// that want a single rect filling this whole area (maximize) apply
    /// `layout::inset` themselves; `Layouts::layout` does it per-leaf.
    pub(crate) fn output_tiling_area(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let output_geo = self.space.output_geometry(output)?;
        let mut area = layer_map_for_output(output).non_exclusive_zone();
        if let Some(error) = &self.config_error_overlay {
            let reserved = error.reserved_height().min(area.size.h.max(0));
            area.loc.y += reserved;
            area.size.h -= reserved;
        }
        area.loc += output_geo.loc;
        Some(area)
    }

    /// The output-local physical rectangle produced by the shared placement
    /// and visual-animation contract. Backdrop capture and glass rendering
    /// use this same helper so they cannot drift from the window itself when
    /// the active spatial engine applies a camera transform.
    pub(super) fn placement_physical_rect(
        &self,
        placement: &crate::placement::PlacedWindow,
        output: &Output,
    ) -> Option<Rectangle<i32, Physical>> {
        let output_geo = self.space.output_geometry(output)?;
        let output_scale = output.current_scale().fractional_scale();
        let visual = self.placement_visual_sample(placement);
        let visual_offset: Point<i32, Logical> = Point::from((
            visual.offset.x.round() as i32,
            visual.offset.y.round() as i32,
        ));
        let size = visual.rounded_size_or(placement.window.geometry().size);
        let logical_rect =
            Rectangle::new(placement.rect.loc - output_geo.loc + visual_offset, size);
        Some(logical_rect.to_physical_precise_round(output_scale))
    }

    fn capture_workspace_desktop(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        geometry: Rectangle<i32, Physical>,
    ) -> Option<GlesTexture> {
        let placements = self.render_placements(output)?;
        let glass_surfaces = self.glass_eligible_surfaces(&placements);
        #[allow(clippy::mutable_key_type)]
        let mut glass_layers =
            self.glass_layer_elements(renderer, output, &placements, &glass_surfaces);
        let (mut depth_elements, depth_surfaces) =
            self.depth_frame_elements(renderer, output, &placements);
        // Glass windows render in their normal z-slot; only depth-replaced
        // windows are skipped.
        let skip = depth_surfaces;
        // The canvas grid, caustics, and BelowWindows ripples ride inside
        // the walk so a layer-shell wallpaper engine can't cover them.
        let ocean_canvas = self.ocean_canvas_frame_element(renderer, output);
        let caustics = self.caustics_frame_element(renderer, output);
        let backdrop: Vec<crate::backend::udev::OutputRenderElements> =
            ocean_canvas.into_iter().chain(caustics).collect();
        let space_elements = self.desktop_render_elements(
            renderer,
            output,
            &placements,
            &skip,
            &mut depth_elements,
            &mut glass_layers,
            backdrop,
        )?;
        let closing_windows = self.closing_window_frame_elements(renderer, output);
        let elements: Vec<crate::backend::udev::OutputRenderElements> = closing_windows
            .into_iter()
            .chain(space_elements)
            .chain(
                self.wallpaper_capture_element(output, renderer)
                    .map(crate::backend::udev::OutputRenderElements::Wallpaper),
            )
            .collect();
        crate::backdrop::capture_once(
            renderer,
            geometry,
            &elements,
            output.current_scale().fractional_scale(),
        )
    }

    /// Captures the currently-visible desktop for a queued workspace
    /// switch, applies the switch, optionally captures the incoming desktop
    /// for synchronized motion, then starts the transition.
    ///
    /// Both backends call this only after submitting the visible outgoing
    /// frame. Captures contain the desktop and wallpaper, while compositor
    /// chrome (cursor, toast, overview, tab strip) remains live above the
    /// transition. If the optional incoming capture fails, the effect falls
    /// back to the one-texture wipe. If the outgoing capture fails, the
    /// workspace still switches immediately rather than leaving input
    /// apparently ignored.
    pub(crate) fn capture_pending_workspace_transition(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) {
        let output_name = output.name();
        let Some(target) = self.pending_workspace_transitions.remove(&output_name) else {
            return;
        };
        let current = self.layout.active_workspace(&output_name);
        if current == target {
            return;
        }
        let water_transition =
            self.config.water_effects && self.config.workspace_transition.enabled;
        let smooth_transition =
            self.config.animations.enabled && self.config.animations.workspace.enabled;
        if !water_transition && !smooth_transition {
            self.apply_workspace_switch(output, current, target);
            return;
        }

        let direction_mode = if water_transition {
            self.config.workspace_transition.direction
        } else {
            crate::config::WorkspaceTransitionDirectionMode::Auto
        };
        let direction = match direction_mode {
            crate::config::WorkspaceTransitionDirectionMode::Auto if target > current => {
                crate::workspace_transition::WorkspaceTransitionDirection::RightToLeft
            }
            crate::config::WorkspaceTransitionDirectionMode::Auto => {
                crate::workspace_transition::WorkspaceTransitionDirection::LeftToRight
            }
            crate::config::WorkspaceTransitionDirectionMode::LeftToRight => {
                crate::workspace_transition::WorkspaceTransitionDirection::LeftToRight
            }
            crate::config::WorkspaceTransitionDirectionMode::RightToLeft => {
                crate::workspace_transition::WorkspaceTransitionDirection::RightToLeft
            }
        };
        let geometry = output
            .current_mode()
            .map(|mode| Rectangle::from_size(mode.size));

        let outgoing_texture = geometry
            .and_then(|geometry| self.capture_workspace_desktop(renderer, output, geometry));
        let workspace_motion =
            water_transition && self.config.workspace_transition.workspace_motion;

        self.apply_workspace_switch(output, current, target);

        let incoming_texture = if workspace_motion && outgoing_texture.is_some() {
            geometry.and_then(|geometry| self.capture_workspace_desktop(renderer, output, geometry))
        } else {
            None
        };

        if let (Some(outgoing_texture), Some(geometry)) = (outgoing_texture, geometry) {
            if water_transition {
                self.workspace_transitions.insert(
                    output_name,
                    crate::workspace_transition::WorkspaceTransition::new(
                        outgoing_texture,
                        incoming_texture,
                        direction,
                        geometry,
                        &self.config.workspace_transition,
                    ),
                );
            } else {
                self.workspace_glides.insert(
                    output_name,
                    crate::workspace_transition::WorkspaceGlide::new(
                        outgoing_texture,
                        direction,
                        geometry,
                        &self.config.animations.workspace,
                        self.config.animations.slowdown,
                    ),
                );
                return;
            }
            // Cosmetic float-physics disturbance (F1 `light`): every float
            // on the output rocks as the wave passes, kicked from the
            // output's center in the wave's travel direction. Only fires
            // alongside the real wave visual above, not on the early-return
            // discrete-switch path. Fixed synthetic magnitude, same
            // tuning-owed caveat as the map kick.
            const WAVE_KICK_IMPULSE: f64 = 120.0;
            if let Some(output_geo) = self.space.output_geometry(output) {
                let center = (
                    output_geo.loc.x as f64 + output_geo.size.w as f64 / 2.0,
                    output_geo.loc.y as f64 + output_geo.size.h as f64 / 2.0,
                );
                let dx = match direction {
                    crate::workspace_transition::WorkspaceTransitionDirection::LeftToRight => {
                        WAVE_KICK_IMPULSE
                    }
                    crate::workspace_transition::WorkspaceTransitionDirection::RightToLeft => {
                        -WAVE_KICK_IMPULSE
                    }
                };
                self.float_physics_kick_near(output, center, (dx, 0.0));
            }
        }
    }

    /// Builds this output's active wipe element and eagerly evicts its
    /// texture once the animation has finished.
    pub(crate) fn workspace_transition_frame_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Option<crate::backend::udev::OutputRenderElements> {
        if !self.config.water_effects
            || !self.config.workspace_transition.enabled
            || !matches!(self.session_lock, SessionLock::Unlocked)
        {
            return None;
        }
        let output_name = output.name();
        if self
            .workspace_transitions
            .get(&output_name)
            .is_some_and(|transition| transition.finished())
        {
            self.workspace_transitions.remove(&output_name);
            return None;
        }
        if !self.workspace_transitions.contains_key(&output_name) {
            return None;
        }
        let program = crate::workspace_transition::workspace_transition_program(
            &mut self.workspace_transition_program,
            renderer,
        )?;
        self.workspace_transitions
            .get_mut(&output_name)
            .map(|transition| {
                crate::backend::udev::OutputRenderElements::WorkspaceTransition(
                    transition.frame_element(program),
                )
            })
    }

    pub(crate) fn workspace_glide_frame_element(
        &mut self,
        output: &Output,
    ) -> Option<crate::backend::udev::OutputRenderElements> {
        if !self.config.animations.enabled
            || !self.config.animations.workspace.enabled
            || !matches!(self.session_lock, SessionLock::Unlocked)
        {
            return None;
        }
        let output_name = output.name();
        if self
            .workspace_glides
            .get(&output_name)
            .is_some_and(|transition| transition.finished())
        {
            self.workspace_glides.remove(&output_name);
            return None;
        }
        self.workspace_glides
            .get_mut(&output_name)
            .map(|transition| {
                crate::backend::udev::OutputRenderElements::WorkspaceGlide(
                    transition.frame_element(),
                )
            })
    }

    /// Drops transient render state for a disconnected output. In
    /// particular, this releases a full-output texture immediately instead
    /// of retaining VRAM under an orphaned connector name, and drops that
    /// output's swim camera rather than leaving a dead entry keyed by a
    /// connector name that will never come back (or means something else
    /// entirely if a differently-wired connector reuses the same name).
    pub(crate) fn remove_workspace_transition_output(&mut self, output_name: &str) {
        self.pending_workspace_transitions.remove(output_name);
        self.workspace_transitions.remove(output_name);
        self.workspace_glides.remove(output_name);
        self.depth_transitions.remove(output_name);
        self.ocean_canvases.remove(output_name);
        self.compasses.remove(output_name);
        self.swim_cameras.remove(output_name);
        self.workspace_scroll_cameras.remove(output_name);
        if self.workspace_step_output.as_deref() == Some(output_name) {
            self.workspace_step_output = None;
        }
        self.caustics.remove(output_name);
        // Same "connector name might mean something else entirely on
        // replug" reasoning as the rest of this function: a fired
        // on_created_empty for this output's workspace numbers shouldn't
        // silently suppress the command if a different monitor is later
        // plugged into the same port and gets the same connector name.
        self.workspace_created_empty_fired
            .retain(|(o, _)| o != output_name);
        if self
            .minimap_peek
            .as_ref()
            .is_some_and(|peek| peek.output_name() == output_name)
        {
            self.minimap_peek = None;
        }
    }
}
