use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::{
        element::{memory::MemoryRenderBufferRenderElement, solid::SolidColorRenderElement, Kind},
        gles::{GlesRenderer, GlesTexture},
    },
    desktop::Window,
    output::Output,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Physical, Point, Rectangle, SERIAL_COUNTER},
};

use crate::config::Direction;

use super::{
    group_removal_outcome, is_window, surface_shader, GroupMember, SessionLock, Smallvil,
    WindowGroup,
};

impl Smallvil {
    /// `true` when this window's resolved rule pins it buoyant (`depth =
    /// false`), exempting it from the automatic inactivity sink regardless
    /// of the global `depth { enabled }` setting.
    fn depth_exempt(&self, surface: &WlSurface) -> bool {
        self.resolve_window_rules_for(surface).depth == Some(false)
    }

    pub(super) fn depth_live_alpha(&self, surface: &WlSurface) -> f32 {
        if !self.config.water_effects || !self.config.depth.enabled {
            return 1.0;
        }
        self.window_depths
            .get(surface)
            .filter(|depth| depth.tier() == 1)
            .map(|_| self.config.depth.tier_one_alpha)
            .unwrap_or(1.0)
    }

    /// Records direct attention for a mapped window. The next frame renders
    /// it at the surface immediately; its inactivity clock restarts from now.
    pub(crate) fn note_depth_attention(&mut self, surface: &WlSurface) {
        let Some(depth) = self.window_depths.get_mut(surface) else {
            return;
        };
        if depth.note_attention() {
            self.depth_schematics.remove(surface);
            self.request_redraw();
            // The periodic `update_window_depths` tick is the only other
            // place tier changes, and it emits this same event -- without
            // it here, a subscriber sees a window sink but never sees it
            // resurface, since by the next tick `old_tier` and the new
            // tier are already equal (both zero) and no change is detected.
            self.emit_ipc_event(crate::ipc::IpcEvent::DepthChanged {
                surface: surface.clone(),
                tier: 0,
            });
        }
    }

    /// Advances every mapped window's analytical inactivity tier. Both
    /// backends call this from their already-bounded frame timer, so no
    /// per-window calloop source or unbounded timer registry is needed.
    pub(crate) fn update_window_depths(&mut self) {
        if self.depth_last_tick.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.depth_last_tick = Instant::now();
        let enabled = self.config.water_effects && self.config.depth.enabled;
        // Resolved separately, before the mutable loop below: `depth_exempt`
        // needs `&self` (it reads surface identity + config), which can't
        // run while `window_depths` is mutably borrowed. Skipped entirely
        // when depth is globally off, since every window takes the
        // `reset_disabled` branch regardless of exemption in that case.
        // `depth_exempt` is intentionally resolved before the mutable walk,
        // but membership in that result must stay constant-time: this runs
        // at 10 Hz and the old Vec made the two full window walks quadratic.
        #[allow(clippy::mutable_key_type)]
        let exempt: HashSet<WlSurface> = if enabled {
            self.window_depths
                .keys()
                .filter(|surface| self.depth_exempt(surface))
                .cloned()
                .collect()
        } else {
            HashSet::new()
        };
        let focused = self.focused_window_surface();
        let mut changed = false;
        // Collect per-surface tier transitions while we still hold the
        // mutable borrow on `window_depths`, then emit after the loop --
        // `emit_ipc_event` needs `&mut self` and can't run mid-iteration.
        let mut tier_changes: Vec<(WlSurface, u8)> = Vec::new();
        for (surface, depth) in self.window_depths.iter_mut() {
            let old_tier = depth.tier();
            let focused_now = focused.as_ref() == Some(surface);
            let local_changed = if focused_now {
                // Focus is an ongoing form of attention, not just the event
                // that granted it. Keep the timer fresh and stay on surface.
                depth.update(&self.config.depth, true)
            } else if enabled && !exempt.contains(surface) {
                depth.update(&self.config.depth, false)
            } else {
                depth.reset_disabled()
            };
            changed |= local_changed;
            if local_changed && depth.tier() != old_tier {
                tier_changes.push((surface.clone(), depth.tier()));
            }
        }
        if !enabled {
            self.depth_schematics.clear();
        }
        self.poll_depth_background();
        if changed {
            self.request_redraw();
        }
        for (surface, tier) in tier_changes {
            self.emit_ipc_event(crate::ipc::IpcEvent::DepthChanged { surface, tier });
        }
    }

    /// Re-fires the urgent ripple for every still-urgent window whose
    /// resolved ripple config has `urgent_repeat` on, once per
    /// `urgent_repeat_interval_ms`. Both backends call this from their
    /// already-bounded frame timer alongside `update_window_depths`, so
    /// no per-window calloop source is needed; the 100ms gate below keeps
    /// the tick itself cheap. Each pulse is identical (flat repeat, the
    /// agreed design -- no decay), and each window holds at most one
    /// active ripple at a time as long as the interval exceeds the ripple
    /// lifetime, so pulsing can't starve the 16-ripple cap.
    ///
    /// `urgent_pulse_last` is reconciled against `urgent` here rather
    /// than at every clear site: focus, unmap, and `focus-urgent` all
    /// remove from `urgent` through different paths, and a stale entry
    /// left behind would only waste a lookup, never fire -- but dropping
    /// it here keeps the map bounded by the urgent set's actual size.
    pub(crate) fn update_urgent_pulses(&mut self) {
        if self.urgent_pulse_tick.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.urgent_pulse_tick = Instant::now();
        self.urgent_pulse_last
            .retain(|surface, _| self.urgent.contains(surface));
        if !self.config.water_effects || self.urgent.is_empty() {
            return;
        }
        let due: Vec<WlSurface> = self
            .urgent
            .iter()
            .filter(|surface| {
                let rule = self.resolve_window_rules_for(surface);
                let cfg = self.config.resolve_ripple_config(
                    rule.ripple.as_ref(),
                    crate::config::RippleTrigger::Urgent,
                );
                if cfg.enabled == Some(false)
                    || !cfg.urgent_repeat.unwrap_or(true)
                    || !cfg.fires_on(crate::config::RippleTrigger::Urgent)
                {
                    return false;
                }
                let interval = cfg.urgent_repeat_interval_ms.unwrap_or(1500).max(100) as u64;
                self.urgent_pulse_last
                    .get(*surface)
                    .is_none_or(|last| last.elapsed() >= Duration::from_millis(interval))
            })
            .cloned()
            .collect();
        for surface in due {
            // Stamped before the spawn attempt on purpose: a spawn that
            // no-ops (hidden workspace, ripple cap reached) must not be
            // retried on every 100ms tick until it succeeds.
            self.urgent_pulse_last
                .insert(surface.clone(), Instant::now());
            self.spawn_ripple(&surface, crate::config::RippleTrigger::Urgent);
        }
    }

    pub(crate) fn apply_depth_settings(
        &mut self,
        next: crate::config::DepthConfig,
        reload_background: bool,
    ) {
        if reload_background || next.background != self.config.depth.background {
            self.depth_background.reset();
            for depth in self.window_depths.values_mut() {
                depth.artwork_id = smithay::backend::renderer::element::Id::new();
            }
        }
        self.config.depth = next;
        self.depth_schematics.clear();
        for depth in self.window_depths.values_mut() {
            depth.visual_changed();
        }
        self.depth_last_tick = Instant::now() - Duration::from_millis(100);
        self.update_window_depths();
        self.request_redraw();
    }

    /// One decoder wakeup/deadline for the entire artwork, regardless of window count.
    pub(super) fn poll_depth_background(&mut self) {
        let enabled = self.config.water_effects && self.config.depth.enabled;
        let path = enabled
            .then(|| self.config.depth.background.clone())
            .flatten();
        let visible = path.is_some()
            && self.depth_background.can_present()
            && matches!(self.session_lock, SessionLock::Unlocked)
            && self.space.outputs().any(|output| {
                if !self.wlr_output_power_management_state.is_on(output) {
                    return false;
                }
                let Some(view) = self.space.output_geometry(output) else {
                    return false;
                };
                self.render_placements(output).is_some_and(|placements| {
                    placements.iter().any(|p| {
                        p.replacement_eligible()
                            && p.rect.overlaps(view)
                            && p.surface()
                                .and_then(|s| self.window_depths.get(s))
                                .is_some_and(|depth| depth.tier() >= 2)
                    })
                })
            });
        let now = Instant::now();
        let old_status = self.depth_background.status();
        self.depth_background
            .configure(path.as_deref(), visible, now);
        if self.depth_background.poll(now) {
            self.request_redraw();
        }
        if self.depth_background.status() != old_status {
            self.emit_ipc_event(crate::ipc::IpcEvent::DepthBackgroundChanged);
        }
        let deadline = self.depth_background.deadline();
        if deadline != self.depth_background_deadline {
            if let Some(token) = self.depth_background_timer.take() {
                self.loop_handle.remove(token);
            }
            self.depth_background_deadline = deadline;
            if let Some(deadline) = deadline {
                self.depth_background_timer = self
                    .loop_handle
                    .insert_source(Timer::from_deadline(deadline), |_, _, state| {
                        state.depth_background_timer = None;
                        state.depth_background_deadline = None;
                        state.poll_depth_background();
                        TimeoutAction::Drop
                    })
                    .ok();
            }
        }
    }

    /// Builds the depth-specific replacement/overlay elements for one output.
    /// The returned surface list contains only deep windows whose schematic
    /// replacement was successfully built, so callers can safely omit those
    /// live surfaces from `desktop_render_elements`.
    pub(crate) fn depth_frame_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        placements: &[crate::placement::PlacedWindow],
    ) -> (
        HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>>,
        Vec<WlSurface>,
    ) {
        self.poll_depth_background();
        if !self.config.water_effects || !self.config.depth.enabled {
            return (HashMap::new(), Vec::new());
        }
        let Some(output_geo) = self.space.output_geometry(output) else {
            return (HashMap::new(), Vec::new());
        };
        let visible: Vec<(WlSurface, Rectangle<i32, Logical>)> = placements
            .iter()
            .filter(|placement| placement.replacement_eligible())
            .filter_map(|placement| {
                let surface = placement.surface()?.clone();
                let mut rect = placement.rect;
                let visual = self.placement_visual_sample(placement);
                rect.loc += Point::from((
                    visual.offset.x.round() as i32,
                    visual.offset.y.round() as i32,
                ));
                rect.size = visual.rounded_size_or(placement.window.geometry().size);
                if !rect.overlaps(output_geo) {
                    return None;
                }
                Some((
                    surface,
                    Rectangle::new(rect.loc - output_geo.loc, rect.size),
                ))
            })
            .collect();
        let program =
            crate::depth::depth_overlay_program(&mut self.depth_overlay_program, renderer);
        #[allow(clippy::mutable_key_type)]
        let mut layers = HashMap::new();
        let mut replaced = Vec::new();

        for (surface, area) in visible {
            let mut elements = Vec::new();
            let Some(depth) = self.window_depths.get(&surface) else {
                continue;
            };
            let tier = depth.tier();
            let urgent = self.urgent.contains(&surface);

            let artwork = if tier >= 2 {
                self.depth_background.render_element(
                    renderer,
                    area,
                    self.config.depth.background_fit,
                    output.current_scale().fractional_scale(),
                    depth.artwork_id.clone(),
                )
            } else {
                None
            };
            let has_artwork = artwork.is_some();
            if let Some(program) = program
                .as_ref()
                .filter(|_| tier == 1 || urgent || has_artwork)
            {
                elements.push(crate::backend::udev::OutputRenderElements::DepthOverlay(
                    crate::depth::DepthOverlayElement::new(
                        depth,
                        area,
                        program.clone(),
                        &self.config.depth,
                        urgent,
                        has_artwork,
                    ),
                ));
            }

            if tier < 2 {
                self.depth_schematics.remove(&surface);
                if !elements.is_empty() {
                    layers.insert(surface, elements);
                }
                continue;
            }

            let size = (area.size.w.max(1), area.size.h.max(1));
            // Artwork only needs a short title strip, never a window-sized CPU copy.
            if !has_artwork || self.config.depth.show_title {
                let rebuild = self.depth_schematics.get(&surface).is_none_or(|schematic| {
                    !schematic.matches(size, tier, &self.config.depth, has_artwork)
                });
                if rebuild {
                    // Identity-change callbacks invalidate this cache; avoid
                    // cloning/rasterizing a title on every GIF frame.
                    let title = if self.config.depth.show_title {
                        crate::tab_strip::window_title(&surface)
                    } else {
                        String::new()
                    };
                    self.depth_schematics.insert(
                        surface.clone(),
                        crate::depth::DepthSchematic::build(
                            size,
                            &title,
                            tier,
                            &self.config.depth,
                            has_artwork,
                        ),
                    );
                }
                let Some(element) = self.depth_schematics.get(&surface).and_then(|schematic| {
                    schematic.render_element(
                        renderer,
                        (
                            area.loc.x as f64 * output.current_scale().fractional_scale(),
                            area.loc.y as f64 * output.current_scale().fractional_scale(),
                        ),
                    )
                }) else {
                    continue;
                };
                elements.push(crate::backend::udev::OutputRenderElements::Composited(
                    element,
                ));
            } else {
                self.depth_schematics.remove(&surface);
            }
            if let Some(artwork) = artwork {
                elements.push(crate::backend::udev::OutputRenderElements::DepthImage(
                    artwork,
                ));
                let alpha = self.config.depth.schematic_alpha;
                let color = self.config.depth.schematic_color;
                elements.push(crate::backend::udev::OutputRenderElements::Dim(
                    SolidColorRenderElement::new(
                        depth.artwork_id.namespaced(1),
                        area.to_physical_precise_round(output.current_scale().fractional_scale()),
                        depth.commit,
                        [color[0] * alpha, color[1] * alpha, color[2] * alpha, alpha],
                        Kind::Unspecified,
                    ),
                ));
            }
            layers.insert(surface.clone(), elements);
            replaced.push(surface);
        }

        (layers, replaced)
    }

    /// A shaded window's or layer's last stage, or `None` when it has no
    /// drawable effect this frame (unassigned, over budget, no program yet,
    /// or a chain that hasn't rendered). `window` carries a window's visual
    /// opacity and output scale for its rounding; layers draw unrounded at
    /// full opacity, masked by their `ignore_alpha` capture when set.
    pub(super) fn custom_shader_last_stage(
        &mut self,
        renderer: &mut GlesRenderer,
        surface: &WlSurface,
        (capture_id, capture_version, capture_texture): (
            smithay::backend::renderer::element::Id,
            usize,
            &GlesTexture,
        ),
        physical_rect: Rectangle<i32, Physical>,
        window: Option<(f32, f32)>,
        alpha_mask: Option<&(GlesTexture, usize, f32)>,
    ) -> Option<crate::shader_effect::CustomShaderElement> {
        if self.custom_shader_bypassed.contains(surface) {
            return None;
        }
        let workspace = self.workspace_of_surface(surface);
        let (name, definition) = surface_shader(
            &self.config,
            &self.layer_shader_assignments,
            &self.window_shader_assignments,
            surface,
            workspace,
        )?;
        let stages = &definition.stages;
        let index = stages.len().checked_sub(1)?;
        let lookup = self.custom_shader_programs.lookup(
            renderer,
            name,
            index,
            stages.len(),
            &stages[index],
            &mut self.custom_shader_compile_budget,
        );
        // The last stage reads the backdrop or the chain's outputs; a chain
        // that hasn't rendered can't draw yet.
        let instance = self.custom_shader_instances.get(surface);
        let targets = instance.map(|instance| instance.targets()).unwrap_or(&[]);
        let chain_version = instance.and_then(|instance| instance.chain_version());
        let resolved = lookup.program.as_ref().and_then(|program| {
            if index > 0 && chain_version.is_none() {
                return None;
            }
            let input = crate::shader_effect::stage_input(index, stages, targets, capture_texture)?;
            let textures =
                crate::shader_effect::stage_textures(program, stages, targets, capture_texture)?;
            let base = crate::shader_effect::with_mask_version(
                chain_version.unwrap_or(capture_version as u64),
                alpha_mask.map(|(_, version, threshold)| (*version, *threshold)),
            );
            let content_version =
                crate::shader_effect::stage_content_version(base, &stages[index], program);
            Some((input, textures, content_version))
        });
        if lookup.deferred {
            self.request_redraw();
        }
        if let Some(message) = lookup.failure {
            self.queue_shader_failure(message);
        }
        let (program, (input, textures, content_version)) = lookup.program.zip(resolved)?;
        let (corner_radii, rounding_power, antialias, opacity) = match window {
            Some((opacity, output_scale)) => {
                let rounding = self.rounding_config_for_surface(surface);
                let corner_radii = if rounding.enabled {
                    rounding.radii.map(|radius| radius * output_scale)
                } else {
                    [0.0; 4]
                };
                (
                    corner_radii,
                    rounding.power,
                    rounding.antialias * output_scale,
                    opacity,
                )
            }
            None => ([0.0; 4], 2.0, 0.0, 1.0),
        };
        let commit = crate::shader_effect::custom_shader_commit(
            content_version,
            program.generation,
            (physical_rect.size.w, physical_rect.size.h),
            &program.params,
            corner_radii,
            rounding_power,
            antialias,
            opacity,
        );
        let instance = self
            .custom_shader_instances
            .entry(surface.clone())
            .or_insert_with(crate::shader_effect::ShaderInstance::new);
        // A chain sampled its clock when its stages rendered.
        let timing = if chain_version.is_some() {
            instance.timing()
        } else {
            instance.sample(commit)
        };
        let mut element = crate::shader_effect::CustomShaderElement::new(
            capture_id,
            commit,
            input,
            physical_rect,
            program,
            corner_radii,
            rounding_power,
            antialias,
            opacity,
            timing,
        )
        .with_textures(textures);
        if let Some((mask, _, threshold)) = alpha_mask {
            element = element.with_alpha_mask(mask.clone(), *threshold);
        }
        Some(element)
    }

    pub(crate) fn depth_transition_frame_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Option<crate::backend::udev::OutputRenderElements> {
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            return None;
        }
        let output_name = output.name();
        if self
            .depth_transitions
            .get(&output_name)
            .is_some_and(|transition| transition.finished())
        {
            self.depth_transitions.remove(&output_name);
            return None;
        }
        let area = Rectangle::from_size(self.space.output_geometry(output)?.size);
        let program = crate::depth_transition::depth_transition_program(
            &mut self.depth_transition_program,
            renderer,
        )?;
        self.depth_transitions
            .get_mut(&output_name)
            .map(|transition| {
                crate::backend::udev::OutputRenderElements::DepthTransition(
                    transition.frame_element(program, area),
                )
            })
    }

    fn detach_tiled_group_member(&mut self, surface: &WlSurface) -> Option<Window> {
        match self.spatial.engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => {
                let window = self.spatial.workspaces.layout.window_of(surface)?;
                self.spatial.workspaces.layout.remove(surface);
                Some(window)
            }
            crate::config::SpatialOwnership::Ocean => {
                self.spatial.ocean.detach_tiled_for_group(surface)
            }
        }
    }

    fn replace_tiled_group_leaf(&mut self, old: &WlSurface, new_window: &Window) -> bool {
        match self.spatial.engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => {
                if !self.spatial.workspaces.layout.contains(old) {
                    return false;
                }
                self.spatial.workspaces.layout.replace_leaf(old, new_window);
                true
            }
            crate::config::SpatialOwnership::Ocean => {
                self.spatial.ocean.replace_tiled_group_leaf(old, new_window)
            }
        }
    }

    /// Which group (if any) `surface` is a member of, active or parked.
    pub fn group_of(&self, surface: &WlSurface) -> Option<usize> {
        self.groups
            .iter()
            .position(|g| g.members.iter().any(|m| &m.surface == surface))
    }

    /// Groups the focused tiled window with its neighbor in `direction` --
    /// i3/sway's "tabbed container" idea: both end up sharing one
    /// Classic-tree or Ocean-reef leaf, cycled via `cycle_tab`. Reuses the same
    /// neighbor lookup `swap_direction` already does, and only ever finds a
    /// *tiled* neighbor (a floating window nearest in that direction isn't
    /// a valid group target, same restriction `swap_direction` applies).
    /// Fullscreen and pseudo-tiled windows are excluded from grouping
    /// entirely for now -- a deliberate v1 scope-out, not an oversight:
    /// both are rect overrides on a tile's rendered size, and neither
    /// interaction with a shared tab slot has been worked out yet.
    pub fn group_direction(&mut self, direction: Direction) {
        let Some(focused) = self.focused_window_surface() else {
            return;
        };
        if !self.spatial_is_tiled(&focused)
            || self.fullscreen.contains_key(&focused)
            || self.spatial.workspaces.pseudo_tiled.contains(&focused)
        {
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
        if !self.spatial_is_tiled(&neighbor_surface)
            || self.fullscreen.contains_key(&neighbor_surface)
            || self
                .spatial
                .workspaces
                .pseudo_tiled
                .contains(&neighbor_surface)
        {
            return;
        }
        self.group_with(&focused, &neighbor_surface);
    }

    /// Merges `b` into `a`'s tiled slot as a new parked tab. Both must
    /// already be tiled in the same spatial owner; `b`'s former leaf
    /// collapses exactly like a normal close, since the BSP removal
    /// removes it from the tree the same way either way. Merging two
    /// windows that are *both* already in (different) groups is out of
    /// scope for now -- no-op rather than picking a side to discard.
    pub fn group_with(&mut self, a: &WlSurface, b: &WlSurface) {
        if self.group_of(b).is_some() {
            tracing::debug!("group-with: target is itself already grouped, skipping");
            return;
        }
        let existing_group = self.group_of(a);
        let anchor = existing_group
            .map(|idx| {
                self.groups[idx].members[self.groups[idx].active]
                    .surface
                    .clone()
            })
            .unwrap_or_else(|| a.clone());
        if !self.spatial_is_tiled(&anchor)
            || !self.spatial_is_tiled(b)
            || !self.spatial_tiles_share_owner(&anchor, b)
        {
            return;
        }
        let new_group_owner = if existing_group.is_none() {
            let owner = match self.spatial.engine.ownership() {
                crate::config::SpatialOwnership::Workspaces => self
                    .spatial
                    .workspaces
                    .layout
                    .output_of(&anchor)
                    .map(|output| {
                        let workspace = self
                            .spatial
                            .workspaces
                            .layout
                            .workspace_of(&anchor)
                            .unwrap_or_else(|| {
                                self.spatial.workspaces.layout.active_workspace(output)
                            });
                        (output.to_string(), workspace)
                    }),
                crate::config::SpatialOwnership::Ocean => {
                    self.spatial.ocean.entry_output(&anchor).map(|name| {
                        let output = name.to_string();
                        let workspace = self.spatial.workspaces.layout.active_workspace(&output);
                        (output, workspace)
                    })
                }
            };
            let Some(owner) = owner else { return };
            Some(owner)
        } else {
            None
        };
        let Some(b_window) = self.detach_tiled_group_member(b) else {
            return;
        };
        self.space.unmap_elem(&b_window);

        if let Some(idx) = existing_group {
            let ui_node_id = self.allocate_ui_node_id();
            self.groups[idx].members.push(GroupMember {
                ui_node_id,
                surface: b.clone(),
                parked_window: Some(b_window),
            });
            self.groups[idx].strip = None;
        } else {
            let group_ui_node_id = self.allocate_ui_node_id();
            let a_ui_node_id = self.allocate_ui_node_id();
            let b_ui_node_id = self.allocate_ui_node_id();
            let (output, workspace) = new_group_owner.expect("new groups resolve an owner");
            self.groups.push(WindowGroup {
                ui_node_id: group_ui_node_id,
                output,
                workspace,
                members: vec![
                    GroupMember {
                        ui_node_id: a_ui_node_id,
                        surface: anchor,
                        parked_window: None,
                    },
                    GroupMember {
                        ui_node_id: b_ui_node_id,
                        surface: b.clone(),
                        parked_window: Some(b_window),
                    },
                ],
                active: 0,
                strip: None,
                strip_width: 0,
            });
        }
        // b's leaf is gone from the tree, so the group's slot just grew;
        // without this the old geometry stays on screen (dead hole where b
        // was) until something else happens to retile.
        self.retile();
        self.request_redraw();
    }

    /// Removes the member at `pos` in group `idx` (`surface` is that
    /// member's own surface), promoting the next member -- wrapping,
    /// following tab order -- into the leaf if the removed one was active,
    /// or dissolving the group entirely if only one member remains
    /// afterward. Returns the removed member's `Window` handle if one was
    /// available: a parked member's is held directly, an active member's
    /// is fetched from the tree before it's overwritten below. The caller
    /// decides what to do with it -- `ungroup` gives it its own new tile,
    /// close-cleanup (`leave_group_on_close`) does nothing further, since
    /// the window is already being destroyed/unmapped by then.
    fn leave_group(&mut self, idx: usize, surface: &WlSurface) -> Option<Window> {
        let pos = self.groups[idx]
            .members
            .iter()
            .position(|m| &m.surface == surface)?;
        let was_active = pos == self.groups[idx].active;
        let active_window = was_active
            .then(|| self.spatial_tiled_window(surface))
            .flatten();
        if was_active && active_window.is_none() {
            tracing::warn!("group active member has no spatial leaf, leaving group unchanged");
            return None;
        }

        let (new_active, dissolves) =
            group_removal_outcome(self.groups[idx].members.len(), self.groups[idx].active, pos);
        let removed = self.groups[idx].members.remove(pos);
        let removed_window = active_window.or(removed.parked_window);

        if dissolves {
            let last_parked = self.groups[idx].members[0].parked_window.take();
            self.groups.remove(idx);
            if let Some(window) = last_parked {
                if !self.replace_tiled_group_leaf(surface, &window) {
                    tracing::warn!("group dissolve lost its spatial leaf replacement");
                }
            }
            self.retile();
            return removed_window;
        }

        self.groups[idx].active = new_active;
        self.groups[idx].strip = None;
        if was_active {
            if let Some(window) = self.groups[idx].members[new_active].parked_window.take() {
                if !self.replace_tiled_group_leaf(surface, &window) {
                    tracing::warn!("group tab promotion lost its spatial leaf replacement");
                }
            }
        }
        self.retile();
        removed_window
    }

    /// Removes `surface` from its group, if grouped. The window keeps
    /// existing (unlike `leave_group_on_close`), so it becomes its own
    /// ordinary tile again. Classic uses its tree fallback; Ocean inserts
    /// beside the surviving member so the tab cannot jump to another reef.
    pub fn ungroup(&mut self, surface: &WlSurface) {
        let Some(idx) = self.group_of(surface) else {
            return;
        };
        let output = self.groups[idx].output.clone();
        let workspace = self.groups[idx].workspace;
        let anchor = self.groups[idx]
            .members
            .iter()
            .find(|member| &member.surface != surface)
            .map(|member| member.surface.clone());
        let Some(window) = self.leave_group(idx, surface) else {
            return;
        };
        match self.spatial.engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => {
                self.spatial
                    .workspaces
                    .layout
                    .insert(&output, workspace, window, None);
            }
            crate::config::SpatialOwnership::Ocean => {
                let inserted = anchor
                    .as_ref()
                    .is_some_and(|anchor| self.spatial.ocean.insert_tiled_next_to(anchor, window));
                if !inserted {
                    tracing::warn!("ungroup could not find the surviving Ocean reef");
                    return;
                }
            }
        }
        self.retile();
        self.request_redraw();
    }

    /// If `surface` belongs to a window group, leaves it (promoting the
    /// next tab or dissolving the group, as `leave_group` describes)
    /// instead of letting `detach_mapped_toplevel`'s ordinary spatial
    /// removal collapse its leaf. No-op for an
    /// ungrouped window. Treats a temporary (null-buffer) unmap the same as
    /// permanent destruction for group membership -- a deliberate v1
    /// scope-out: a parked member being independently hidden-then-remapped
    /// while still "in" the group is its own can of worms this first pass
    /// doesn't open.
    pub(crate) fn leave_group_on_close(&mut self, surface: &WlSurface) {
        if let Some(idx) = self.group_of(surface) {
            self.leave_group(idx, surface);
        }
    }

    /// Cycles the focused window's group to the next (`forward`) or
    /// previous tab, wrapping. No-op if the focused window isn't grouped.
    /// The demoted tab is unmapped and parked exactly like `group_with`
    /// parks a freshly merged one; the promoted tab is mapped back at the
    /// leaf's rect by the `retile()` below -- `retile()` only ever
    /// positions whatever's currently in the tree, so without an explicit
    /// unmap here the demoted window would otherwise stay stuck mapped at
    /// its stale rect (same gotcha `switch_workspace` already has to work
    /// around for hidden floating windows).
    pub fn cycle_tab(&mut self, forward: bool) {
        let Some(focused) = self.focused_window_surface() else {
            return;
        };
        let Some(idx) = self.group_of(&focused) else {
            return;
        };
        let old_active = self.groups[idx].active;
        let len = self.groups[idx].members.len();
        let new_active = if forward {
            (old_active + 1) % len
        } else {
            (old_active + len - 1) % len
        };
        self.group_activate_tab(idx, new_active);
    }

    /// Makes `new_active` the tab occupying group `idx`'s shared leaf,
    /// parking the previously active one. The demoted tab is unmapped and
    /// parked exactly like `group_with` parks a freshly merged one; the
    /// promoted tab is mapped back at the leaf's rect by the `retile()`
    /// below -- `retile()` only ever positions whatever's currently in the
    /// tree, so without an explicit unmap here the demoted window would
    /// otherwise stay stuck mapped at its stale rect (same gotcha
    /// `switch_workspace` already has to work around for hidden floating
    /// windows).
    pub(super) fn group_activate_tab(&mut self, idx: usize, new_active: usize) {
        let old_active = self.groups[idx].active;
        if new_active == old_active {
            return;
        }

        let old_surface = self.groups[idx].members[old_active].surface.clone();
        let Some(old_window) = self.spatial_tiled_window(&old_surface) else {
            return;
        };
        let Some(new_window) = self.groups[idx].members[new_active].parked_window.take() else {
            return;
        };

        if !self.replace_tiled_group_leaf(&old_surface, &new_window) {
            self.groups[idx].members[new_active].parked_window = Some(new_window);
            return;
        }
        self.space.unmap_elem(&old_window);
        self.groups[idx].members[old_active].parked_window = Some(old_window);
        self.groups[idx].active = new_active;
        self.groups[idx].strip = None;
        self.retile();

        let new_surface = self.groups[idx].members[new_active].surface.clone();
        self.focus_window(Some(new_surface), SERIAL_COUNTER.next_serial());
        // `reconcile_keyboard_focus` (triggered by `focus_window` above)
        // only refreshes wlr-foreign-toplevel state for the old/new
        // *keyboard* focus targets. The demoted tab (`old_surface`) is
        // only one of those if it also happened to hold actual keyboard
        // focus before this call -- a group's visible tab and the seat's
        // keyboard focus are independent (the user can click away to a
        // different window entirely without switching tabs), so a demoted
        // tab that wasn't focused would otherwise never get told anything
        // changed. Explicit and safe to call unconditionally: `send_state`
        // (see wlr_foreign_toplevel.rs) already short-circuits when its
        // computed state bytes match what was last broadcast.
        self.refresh_wlr_toplevel_state(&old_surface);
        self.request_redraw();
    }

    /// Marks `surface` urgent instead of stealing focus for it -- the
    /// downgrade path for a stale-serial xdg-activation request (see
    /// `request_activation`). A no-op if the window isn't actually mapped;
    /// urgency only means something for a window the user could plausibly
    /// switch to.
    pub(crate) fn mark_urgent(&mut self, surface: &WlSurface) {
        if self.intended_window_surface().as_ref() == Some(surface) {
            return;
        }
        if self.mapped_toplevel_window(surface).is_none() {
            return;
        }
        self.urgent.insert(surface.clone());
        if let Some(depth) = self.window_depths.get_mut(surface) {
            depth.visual_changed();
        }
        // `rule { urgent = ... }` is the one match criterion that can flip
        // after map time -- re-resolve this window's opacity/glass now so
        // a matching rule takes effect immediately rather than waiting for
        // a reload. Viscosity/sway/depth/border/shadow/rounding/ripple all
        // already resolve fresh on every use (see
        // `Smallvil::resolve_window_rules_for`'s call sites), so they need
        // no equivalent nudge here.
        self.refresh_window_opacity_and_glass_for(surface);
        // The immediate spawn is the first pulse; `update_urgent_pulses`
        // re-fires it every `urgent_repeat_interval_ms` until the hint
        // clears. A no-op if the window's workspace is hidden, since
        // spawn_ripple needs a real on-screen location -- the repeat tick
        // keeps trying, so the pulse starts up on its own once the
        // workspace becomes visible again.
        self.urgent_pulse_last
            .insert(surface.clone(), Instant::now());
        self.spawn_ripple(surface, crate::config::RippleTrigger::Urgent);
        self.emit_ipc_event(crate::ipc::IpcEvent::UrgentChanged {
            surface: surface.clone(),
            urgent: true,
        });
        self.request_redraw();
    }

    /// Focuses whichever window is currently marked urgent, if any --
    /// reuses `activate_toplevel`'s own workspace-switch/group-promote/
    /// floating-raise logic rather than duplicating it, since "focus this
    /// window" means the same thing regardless of why. `activate_toplevel`
    /// itself already clears `urgent` (via `reconcile_keyboard_focus`), so
    /// there's nothing left to clear here.
    pub fn focus_urgent(&mut self) {
        let Some(surface) = self.urgent.iter().next().cloned() else {
            return;
        };
        self.activate_toplevel(&surface);
    }

    /// Adds `tag` to `surface`'s tag set (`tag-window:<name>`) -- the
    /// runtime counterpart to `rule { add_tag = [...] }`. Idempotent, like
    /// `HashSet::insert` itself. `tag` is the one match criterion besides
    /// `urgent` that can flip after map time, so a matching rule's
    /// opacity/glass takes effect immediately rather than waiting for the
    /// next natural resolve -- same precedent as `mark_urgent`.
    pub(crate) fn tag_window(&mut self, surface: &WlSurface, tag: &str) {
        let inserted = self
            .window_tags
            .entry(surface.clone())
            .or_default()
            .insert(tag.to_string());
        if inserted {
            self.refresh_window_opacity_and_glass_for(surface);
            self.request_redraw();
        }
    }

    /// Removes `tag` from `surface`'s tag set (`untag-window:<name>`),
    /// dropping the map entry entirely once its last tag is gone so an
    /// untagged window costs nothing, same invariant `window_tags` keeps
    /// everywhere else.
    pub(crate) fn untag_window(&mut self, surface: &WlSurface, tag: &str) {
        let Some(tags) = self.window_tags.get_mut(surface) else {
            return;
        };
        if !tags.remove(tag) {
            return;
        }
        if tags.is_empty() {
            self.window_tags.remove(surface);
        }
        self.refresh_window_opacity_and_glass_for(surface);
        self.request_redraw();
    }

    /// Focuses a window carrying `tag`, cycling to the next one carrying
    /// it on repeated presses (`toggle-tag:<name>`) -- same MRU-then-
    /// arbitrary-order idea as `cycle_focus`, scoped to just the tagged
    /// subset, and reusing `activate_toplevel` so a match on a hidden
    /// workspace switches there first (`focus_urgent`'s own precedent).
    /// No-op if nothing carries the tag. `cycling_focus` suppresses the
    /// MRU reorder for the duration, the same guard `cycle_focus` uses --
    /// without it, the next press would read back whatever this one just
    /// focused as "current" and oscillate between two windows instead of
    /// visiting every tagged one in turn.
    pub(crate) fn toggle_tag(&mut self, tag: &str) {
        let mut windows: Vec<WlSurface> = self
            .window_tags
            .iter()
            .filter(|(_, tags)| tags.contains(tag))
            .map(|(surface, _)| surface.clone())
            .collect();
        // A tag can outlive the window it was on if some future caller
        // forgets the `detach_mapped_toplevel` cleanup this relies on --
        // filtering here means a stale entry gets skipped once rather than
        // wedging every future press on a dead surface `activate_toplevel`
        // can never focus (its own `mapped_toplevel_window` guard would
        // silently no-op, leaving `current_index` unchanged).
        windows.retain(|s| self.mapped_toplevel_window(s).is_some());
        if windows.is_empty() {
            return;
        }
        let mut ordered: Vec<WlSurface> = self
            .input
            .focus_history
            .iter()
            .filter(|s| windows.contains(s))
            .cloned()
            .collect();
        for surface in &windows {
            if !ordered.contains(surface) {
                ordered.push(surface.clone());
            }
        }
        let current = self.focused_window_surface();
        let current_index = current
            .as_ref()
            .and_then(|s| ordered.iter().position(|o| o == s));
        let next_index = match current_index {
            Some(i) => (i + 1) % ordered.len(),
            None => 0,
        };
        let next = ordered[next_index].clone();
        self.input.cycling_focus = true;
        self.activate_toplevel(&next);
        self.input.cycling_focus = false;
    }

    /// Turns `focused`'s Scroll column into one tab group (tabs in column
    /// order, `focused` stays the visible tab), or a tab group back into a
    /// stacked column in tab order, where the group was.
    pub(super) fn toggle_column_tabbed(&mut self, focused: &WlSurface) {
        if self.fullscreen.contains_key(focused) {
            return;
        }
        if let Some(idx) = self.group_of(focused) {
            let members: Vec<WlSurface> = self.groups[idx]
                .members
                .iter()
                .map(|member| member.surface.clone())
                .collect();
            let leaf = members[self.groups[idx].active].clone();
            for member in members.iter().filter(|member| **member != leaf) {
                self.ungroup(member);
            }
            let (Some(output), Some(workspace)) = (
                self.spatial
                    .workspaces
                    .layout
                    .output_of(&leaf)
                    .map(str::to_owned),
                self.spatial.workspaces.layout.workspace_of(&leaf),
            ) else {
                return;
            };
            let mut columns = self
                .spatial
                .workspaces
                .layout
                .scroll_columns(&output, workspace);
            let Some(at) = columns.iter().position(|c| c.windows.contains(&leaf)) else {
                return;
            };
            let width = columns[at].width;
            for column in &mut columns {
                column.windows.retain(|s| !members.contains(s));
            }
            columns[at].windows = members
                .iter()
                .filter(|s| self.spatial.workspaces.layout.contains(s))
                .cloned()
                .collect();
            columns[at].width = width;
            columns.retain(|c| !c.windows.is_empty());
            self.spatial
                .workspaces
                .layout
                .set_scroll_columns(&output, workspace, columns);
            self.focus_window(Some(focused.clone()), SERIAL_COUNTER.next_serial());
            self.scroll_columns_changed(focused);
            return;
        }
        let (Some(output), Some(workspace)) = (
            self.spatial
                .workspaces
                .layout
                .output_of(focused)
                .map(str::to_owned),
            self.spatial.workspaces.layout.workspace_of(focused),
        ) else {
            return;
        };
        let Some(column) = self
            .spatial
            .workspaces
            .layout
            .scroll_columns(&output, workspace)
            .into_iter()
            .find(|c| c.windows.contains(focused))
        else {
            return;
        };
        if column.windows.len() < 2
            || column
                .windows
                .iter()
                .any(|s| self.group_of(s).is_some() || self.fullscreen.contains_key(s))
        {
            return;
        }
        let head = column.windows[0].clone();
        for other in &column.windows[1..] {
            self.group_with(&head, other);
        }
        if let Some(idx) = self.group_of(&head) {
            if let Some(position) = self.groups[idx]
                .members
                .iter()
                .position(|m| &m.surface == focused)
            {
                self.group_activate_tab(idx, position);
            }
        }
        self.scroll_columns_changed(focused);
    }

    /// Builds visible group tab strips at their active placement. Cached pixels
    /// rebuild only after invalidation or a live width change.
    pub(crate) fn tab_strip_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        scene: &[crate::placement::PlacedWindow],
    ) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let output_name = output.name();
        // Groups are few; a flat vector avoids using Wayland objects (whose
        // liveness flag is interior-mutable) as hash keys for transient
        // per-frame placement data.
        let mut placements: Vec<(WlSurface, Rectangle<i32, Logical>)> = Vec::new();
        for placement in scene {
            let Some(surface) = placement.surface().cloned() else {
                continue;
            };
            let rect = crate::placement::translated_rect(placement.rect, placement.view_offset);
            placements.push((surface, rect));
        }

        // Render elements take output-local physical positions; placements
        // are global logical rects.
        let Some(output_geo) = self.space.output_geometry(output) else {
            return Vec::new();
        };
        let scale = output.current_scale().fractional_scale();
        let mut elements = Vec::new();
        for group in &mut self.groups {
            if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Workspaces
                && group.output != output_name
            {
                continue;
            }
            let active_surface = &group.members[group.active].surface;
            let Some(rect) = placements
                .iter()
                .find_map(|(surface, rect)| (surface == active_surface).then_some(*rect))
            else {
                continue;
            };

            if group.strip.is_none() || group.strip_width != rect.size.w {
                group.strip = None;
                let titles: Vec<String> = group
                    .members
                    .iter()
                    .map(|m| crate::tab_strip::window_title(&m.surface))
                    .collect();
                if let Some((buffer, width)) =
                    crate::tab_strip::build_buffer(&titles, group.active, rect.size.w)
                {
                    group.strip = Some(buffer);
                    group.strip_width = width;
                }
            }

            if let Some(buffer) = &group.strip {
                let location = (
                    f64::from(rect.loc.x - output_geo.loc.x) * scale,
                    f64::from(rect.loc.y - output_geo.loc.y) * scale,
                );
                if let Some(element) = crate::tab_strip::render_element(buffer, renderer, location)
                {
                    elements.push(element);
                }
            }
        }
        elements
    }

    /// Parks the focused ordinary tiled window below its current Classic
    /// workspace. Grouped and rect-override windows are deliberately excluded
    /// from v1 so parking cannot silently break another ownership model.
    pub fn sink_window(&mut self) {
        if !self.config.classic_depth.enabled {
            return;
        }
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        if !self.spatial.workspaces.layout.contains(&surface)
            || self.group_of(&surface).is_some()
            || self.fullscreen.contains_key(&surface)
            || self.maximized.contains_key(&surface)
            || self.spatial.workspaces.pseudo_tiled.contains(&surface)
            || self.spatial.workspaces.pinned.contains(&surface)
        {
            return;
        }
        let (Some(output), Some(workspace), Some(window)) = (
            self.spatial
                .workspaces
                .layout
                .output_of(&surface)
                .map(str::to_string),
            self.spatial.workspaces.layout.workspace_of(&surface),
            self.spatial.workspaces.layout.window_of(&surface),
        ) else {
            return;
        };

        self.spatial.workspaces.layout.remove(&surface);
        self.space.unmap_elem(&window);
        self.spatial
            .workspaces
            .classic_depth
            .park(crate::classic_depth::DepthDeckEntry {
                surface: surface.clone(),
                window,
                output: output.clone(),
                workspace,
            });
        self.close_depth_deck();
        self.retile();
        self.refocus_after_hide(&output);
        self.refresh_wlr_toplevel_state(&surface);
        self.start_depth_transition(&output, true);
        self.request_redraw();
    }

    /// Opens (or closes) the current Classic workspace's deck. It is a static
    /// compositor overlay and does not steal keyboard focus from the tile that
    /// will be used as the default swap target.
    pub fn dive(&mut self) {
        if !self.config.classic_depth.enabled {
            return;
        }
        if self.spatial.workspaces.classic_depth.view().is_some() {
            self.close_depth_deck();
            return;
        }
        self.overview = None;
        let Some(output) = self.primary_output() else {
            return;
        };
        let output_name = output.name();
        let workspace = self
            .spatial
            .workspaces
            .layout
            .active_workspace(&output_name);
        if self
            .spatial
            .workspaces
            .classic_depth
            .open(output_name, workspace)
        {
            self.rebuild_depth_deck_overlay();
            self.request_redraw();
        }
    }

    pub fn cycle_depth_deck(&mut self, delta: isize) {
        if !self.config.classic_depth.enabled || !self.spatial.workspaces.classic_depth.cycle(delta)
        {
            return;
        }
        self.rebuild_depth_deck_overlay();
        self.request_redraw();
    }

    pub fn close_depth_deck(&mut self) {
        let changed = self.spatial.workspaces.classic_depth.close()
            | self.depth_deck_overlay.take().is_some();
        if changed {
            self.request_redraw();
        }
    }

    /// Direct, workspace-like depth movement. Repeated Down walks newest to
    /// oldest through the current workspace's deck; Up walks the same ring in
    /// reverse. The focused tile stays geometrically fixed throughout.
    pub fn switch_depth(&mut self, down: bool) {
        if !self.config.classic_depth.enabled {
            return;
        }
        let Some(surface) = self.focused_window_surface() else {
            return;
        };
        if !self.spatial.workspaces.layout.contains(&surface)
            || self.group_of(&surface).is_some()
            || self.fullscreen.contains_key(&surface)
            || self.maximized.contains_key(&surface)
            || self.spatial.workspaces.pseudo_tiled.contains(&surface)
            || self.spatial.workspaces.pinned.contains(&surface)
        {
            return;
        }
        let (Some(output), Some(workspace), Some(old_window)) = (
            self.spatial
                .workspaces
                .layout
                .output_of(&surface)
                .map(str::to_string),
            self.spatial.workspaces.layout.workspace_of(&surface),
            self.spatial.workspaces.layout.window_of(&surface),
        ) else {
            return;
        };

        if self
            .spatial
            .workspaces
            .classic_depth
            .entries_for(&output, workspace)
            .next()
            .is_none()
        {
            if down
                && self
                    .spatial
                    .workspaces
                    .layout
                    .window_count(&output, workspace)
                    > 1
            {
                self.sink_window();
            }
            return;
        }

        let Some(selected) = self.spatial.workspaces.classic_depth.rotate(
            &output,
            workspace,
            crate::classic_depth::DepthDeckEntry {
                surface: surface.clone(),
                window: old_window.clone(),
                output: output.clone(),
                workspace,
            },
            down,
        ) else {
            return;
        };
        self.close_depth_deck();
        self.spatial
            .workspaces
            .layout
            .replace_leaf(&surface, &selected.window);
        self.space.unmap_elem(&old_window);
        self.retile();
        self.note_depth_attention(&selected.surface);
        self.focus_window(Some(selected.surface.clone()), SERIAL_COUNTER.next_serial());
        self.refresh_wlr_toplevel_state(&surface);
        self.refresh_wlr_toplevel_state(&selected.surface);
        self.start_depth_transition(&output, down);
        self.request_redraw();
    }

    /// Recalls the selected deep window. A compatible focused tile is replaced
    /// exactly and becomes the new occupant of that same deck slot; otherwise
    /// the selected window is inserted as an ordinary tile.
    pub fn select_depth_deck(&mut self) {
        if !self.config.classic_depth.enabled {
            return;
        }
        let Some(view) = self.spatial.workspaces.classic_depth.view().cloned() else {
            return;
        };
        let swap_surface = self.focused_window_surface().filter(|surface| {
            self.spatial.workspaces.layout.output_of(surface) == Some(view.output.as_str())
                && self.spatial.workspaces.layout.workspace_of(surface) == Some(view.workspace)
                && self.group_of(surface).is_none()
                && !self.fullscreen.contains_key(surface)
                && !self.maximized.contains_key(surface)
                && !self.spatial.workspaces.pseudo_tiled.contains(surface)
                && !self.spatial.workspaces.pinned.contains(surface)
        });

        let selected = if let Some(surface) = swap_surface {
            let Some(old_window) = self.spatial.workspaces.layout.window_of(&surface) else {
                return;
            };
            let Some(selected) = self.spatial.workspaces.classic_depth.swap_selected(
                crate::classic_depth::DepthDeckEntry {
                    surface: surface.clone(),
                    window: old_window.clone(),
                    output: view.output.clone(),
                    workspace: view.workspace,
                },
            ) else {
                return;
            };
            self.spatial
                .workspaces
                .layout
                .replace_leaf(&surface, &selected.window);
            self.space.unmap_elem(&old_window);
            self.refresh_wlr_toplevel_state(&surface);
            selected
        } else {
            let Some(selected) = self.spatial.workspaces.classic_depth.take_selected() else {
                return;
            };
            self.spatial.workspaces.layout.insert(
                &selected.output,
                selected.workspace,
                selected.window.clone(),
                None,
            );
            selected
        };

        self.spatial.workspaces.classic_depth.close();
        self.depth_deck_overlay = None;
        self.retile();
        self.note_depth_attention(&selected.surface);
        self.focus_window(Some(selected.surface.clone()), SERIAL_COUNTER.next_serial());
        self.refresh_wlr_toplevel_state(&selected.surface);
        self.start_depth_transition(&selected.output, false);
        self.request_redraw();
    }

    fn rebuild_depth_deck_overlay(&mut self) {
        let Some(view) = self.spatial.workspaces.classic_depth.view().cloned() else {
            self.depth_deck_overlay = None;
            return;
        };
        let Some(output) = self.output_by_name(&view.output) else {
            self.close_depth_deck();
            return;
        };
        let Some(output_geometry) = self.space.output_geometry(&output) else {
            self.close_depth_deck();
            return;
        };
        let titles: Vec<String> = self
            .spatial
            .workspaces
            .classic_depth
            .entries_for(&view.output, view.workspace)
            .map(|entry| crate::tab_strip::window_title(&entry.surface))
            .collect();
        self.depth_deck_overlay = Some(crate::depth_deck::DepthDeckOverlay::build(
            view.output,
            view.workspace,
            &titles,
            view.selected,
            (output_geometry.size.w, output_geometry.size.h),
        ));
    }

    /// Hot-disabling Classic depth must never strand clients in hidden
    /// structural state. Restore every parked window to its owning tree; only
    /// active workspaces are remapped by the normal `retile()` pass.
    pub(super) fn restore_all_depth_deck_windows(&mut self) {
        let entries = self.spatial.workspaces.classic_depth.drain();
        self.depth_deck_overlay = None;
        for entry in entries {
            self.spatial.workspaces.layout.insert(
                &entry.output,
                entry.workspace,
                entry.window,
                None,
            );
        }
    }

    fn start_depth_transition(&mut self, output: &str, down: bool) {
        let config = &self.config.classic_depth;
        if !config.enabled || !config.animation || config.animation_duration_ms == 0 {
            return;
        }
        self.depth_transitions.insert(
            output.to_string(),
            crate::depth_transition::DepthTransition::new(
                if down {
                    crate::depth_transition::DepthTransitionDirection::Down
                } else {
                    crate::depth_transition::DepthTransitionDirection::Up
                },
                Duration::from_millis(config.animation_duration_ms as u64),
                config.wave_color,
                config.wave_alpha,
            ),
        );
    }
}
