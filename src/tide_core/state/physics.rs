use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::gles::GlesRenderer,
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Scale},
};

use super::{FloatBodyContext, FocusOrigin, RippleLayers, SessionLock, Smallvil};

impl Smallvil {
    /// Whether continuous lateral swim is active: the master water toggle
    /// and the swim toggle both on. Off falls back to the ordinary discrete
    /// workspace switch, and the camera offset collapses to zero.
    pub(crate) fn swim_enabled(&self) -> bool {
        self.config.spatial_engine == crate::config::SpatialEngine::Classic
            && self.config.water_effects
            && self.config.swim.enabled
    }

    pub(super) fn interactive_motion_half_life(&self, surface: &WlSurface) -> Duration {
        if self.config.animations.enabled && self.config.animations.interactive.enabled {
            return Duration::from_millis(self.config.animations.interactive.half_life_ms as u64);
        }
        if !self.config.water_effects {
            return Duration::ZERO;
        }
        // Preserve the original water-viscosity feel for existing configs.
        // This is a user-facing animation baseline, not a display-cadence or
        // hardware assumption; the follower itself integrates elapsed time.
        let viscosity = self
            .resolve_window_rules_for(surface)
            .viscosity
            .unwrap_or(self.config.viscosity)
            .clamp(0.0, 4.0);
        Duration::from_secs_f64(0.030 * viscosity)
    }

    /// Retargets one mapped window's render-only interactive follower.
    /// Returns `false` when viscosity is bypassed, allowing a tiled resize
    /// to fall back to the ordinary layout-movement animation.
    pub(crate) fn retarget_window_viscosity(
        &mut self,
        surface: &WlSurface,
        target: Rectangle<i32, Logical>,
    ) -> bool {
        let half_life = self.interactive_motion_half_life(surface);
        // Interactive input owns the visual geometry from this point. The
        // generic movement tween must not remain underneath it even when
        // viscosity is disabled and the pointer path becomes immediate.
        self.window_move_animations.remove(surface);
        if half_life.is_zero() {
            self.window_viscosity.remove(surface);
            return false;
        }
        let Some(window) = self.mapped_toplevel_window(surface) else {
            return false;
        };
        let Some(location) = self.space.element_location(&window) else {
            return false;
        };
        let live = Rectangle::new(location, window.geometry().size).to_f64();
        let target = target.to_f64();
        if let Some(motion) = self.window_viscosity.get_mut(surface) {
            motion.retarget(target, half_life);
        } else {
            self.window_viscosity.insert(
                surface.clone(),
                crate::viscosity::ViscousMotion::new(live, target, half_life),
            );
        }
        // The generic movement tween and the physical follower must never
        // both offset the same window. Interactive viscosity owns it until
        // it settles, then a later non-interactive retile may start a fresh
        // ordinary movement animation.
        self.request_redraw();
        true
    }

    pub(super) fn sway_enabled_for_surface(&self, surface: &WlSurface) -> bool {
        if !self.config.water_effects {
            return false;
        }
        self.resolve_window_rules_for(surface)
            .sway
            .unwrap_or(self.config.sway.enabled)
    }

    /// Feeds one horizontal floating-drag delta into the window's lateral
    /// sway. Called from the move grab's motion path; a no-op when the
    /// water identity, the mechanic, or this window's rule disables it.
    pub(crate) fn sway_kick(&mut self, surface: &WlSurface, delta_x: f64) {
        if delta_x.abs() < f64::EPSILON || !self.sway_enabled_for_surface(surface) {
            return;
        }
        let config = self.config.sway;
        let response = config.response as f64;
        let max_offset = config.max_offset as f64;
        match self.window_sway.get_mut(surface) {
            Some(sway) => sway.kick(delta_x, response, max_offset),
            None => {
                self.window_sway.insert(
                    surface.clone(),
                    crate::sway::FloatingSway::kicked(
                        (delta_x * response).clamp(-max_offset, max_offset),
                        config.frequency as f64,
                        config.damping as f64,
                    ),
                );
            }
        }
        self.request_redraw();
    }

    /// Resolves the exclusive floating-physics tier used instead of sway.
    pub(crate) fn float_physics_tier_for_surface(
        &self,
        surface: &WlSurface,
    ) -> crate::config::FloatPhysicsTier {
        if !self.config.water_effects {
            return crate::config::FloatPhysicsTier::Off;
        }
        self.resolve_window_rules_for(surface)
            .float_physics
            .unwrap_or(self.config.float_physics.tier)
    }

    /// Convenience for call sites that only care whether F1 is active at
    /// all (either tier), not which one.
    pub(crate) fn float_physics_enabled_for_surface(&self, surface: &WlSurface) -> bool {
        self.float_physics_tier_for_surface(surface) != crate::config::FloatPhysicsTier::Off
    }

    /// Applies a 2D disturbance at the resolved physics tier. A kick also
    /// refreshes depth attention so the window cannot sink mid-motion.
    pub(crate) fn float_physics_kick(&mut self, surface: &WlSurface, impulse: (f64, f64)) {
        if impulse.0.abs() < f64::EPSILON && impulse.1.abs() < f64::EPSILON {
            return;
        }
        let config = self.config.float_physics;
        match self.float_physics_tier_for_surface(surface) {
            crate::config::FloatPhysicsTier::Off => return,
            crate::config::FloatPhysicsTier::Light => {
                let response = config.response as f64;
                let max_offset = config.max_offset as f64;
                let bob_ratio = config.bob_ratio as f64;
                match self.window_float_physics.get_mut(surface) {
                    Some(physics) => physics.kick(impulse, response, max_offset, bob_ratio),
                    None => {
                        self.window_float_physics.insert(
                            surface.clone(),
                            crate::float_physics::FloatPhysics::kicked(
                                impulse,
                                response,
                                max_offset,
                                bob_ratio,
                                config.frequency as f64,
                                config.damping as f64,
                            ),
                        );
                    }
                }
            }
            crate::config::FloatPhysicsTier::Full => {
                self.window_float_bodies
                    .entry(surface.clone())
                    .or_insert_with(crate::float_physics::FloatBody::at_rest)
                    .kick(impulse, config.response as f64);
            }
        }
        self.note_depth_attention(surface);
        self.request_redraw();
    }

    /// Spreads a disturbance across output floaters with the configured
    /// distance falloff from `point`.
    pub(crate) fn float_physics_kick_near(
        &mut self,
        output: &Output,
        point: (f64, f64),
        impulse: (f64, f64),
    ) {
        if !self.config.water_effects {
            return;
        }
        // `render_placements` (Classic) reads `Space::elements_for_output`,
        // a per-output cache normally kept fresh by each backend's render
        // loop calling `space.refresh()` once a frame -- see
        // `refocus_after_hide`'s identical comment. Both call sites here
        // (a window mapping, a workspace-transition wave) run synchronously
        // right after their own `map_element`/`apply_workspace_switch`,
        // before that frame's render pass, so without this the cache is
        // stale and this always sees zero placements for a window just
        // mapped or just switched in. Cheap and idempotent to call an extra
        // time here.
        self.refresh_space();
        let Some(placements) = self.render_placements(output) else {
            return;
        };
        let radius = self.config.float_physics.radius as f64;
        let falloff = self.config.float_physics.falloff;
        for placement in &placements {
            if !placement.is_floating() {
                continue;
            }
            let Some(surface) = placement.surface() else {
                continue;
            };
            // Not `placement.rect`: a just-mapped window's `PlacedWindow`
            // still carries `window.geometry()`'s pre-commit (often
            // zero) size, the same buffer-commit lag `spawn_ripple`
            // documents -- collapsing its "center" to its top-left corner
            // and pushing it outside its own kick radius. This is the
            // same already-resolved-rect helper `point` itself came from
            // at the call site, so a window kicking itself always
            // measures at distance zero.
            let Some(center) = self.window_center_for_kick(surface) else {
                continue;
            };
            let Some(fraction) = crate::float_physics::falloff_kick(point, center, radius) else {
                continue;
            };
            if !falloff && fraction < 1.0 {
                continue;
            }
            self.float_physics_kick(surface, (impulse.0 * fraction, impulse.1 * fraction));
        }
    }

    /// Best-effort window center in global logical coordinates, used as a
    /// disturbance origin. Mirrors `spawn_ripple`'s own preference order:
    /// the floating tag's already-resolved target rect (set before a
    /// first-map floating conversion's own buffer commit lands) over the
    /// window's possibly still-stale committed geometry.
    pub(crate) fn window_center_for_kick(&self, surface: &WlSurface) -> Option<(f64, f64)> {
        let rect = self.window_rect_for_kick(surface)?;
        Some((
            rect.loc.x as f64 + rect.size.w as f64 / 2.0,
            rect.loc.y as f64 + rect.size.h as f64 / 2.0,
        ))
    }

    /// The rect half of `window_center_for_kick`, also used by F1 `full`
    /// tier's per-tick collision pass, which needs real width/height for
    /// AABB overlap tests, not just a center point.
    pub(crate) fn window_rect_for_kick(
        &self,
        surface: &WlSurface,
    ) -> Option<Rectangle<i32, Logical>> {
        self.floating_workspace
            .get(surface)
            .map(|tag| tag.rect)
            .or_else(|| {
                let window = self.mapped_toplevel_window(surface)?;
                self.space
                    .element_location(&window)
                    .map(|loc| Rectangle::new(loc, window.geometry().size))
            })
    }

    /// Maintains all qualifying full-tier bodies while collision/wave
    /// simulation is active, and prunes the set completely once idle.
    pub(super) fn sync_float_physics_bodies(&mut self) {
        if !self.config.water_effects {
            self.window_float_bodies.clear();
            return;
        }
        let stale: Vec<WlSurface> = self
            .window_float_bodies
            .keys()
            .filter(|surface| {
                !self.is_floating(surface)
                    || self.float_physics_tier_for_surface(surface)
                        != crate::config::FloatPhysicsTier::Full
            })
            .cloned()
            .collect();
        for surface in stale {
            self.window_float_bodies.remove(&surface);
        }
        if !crate::float_physics::bodies_need_simulation(
            self.config.float_physics.wave.enabled,
            self.window_float_bodies.values(),
        ) {
            return;
        }

        let mut candidates: Vec<WlSurface> = self.floating_workspace.keys().cloned().collect();
        candidates.extend(self.ocean.floating_surfaces().cloned());
        for surface in candidates {
            if self.float_physics_tier_for_surface(&surface)
                == crate::config::FloatPhysicsTier::Full
            {
                self.window_float_bodies
                    .entry(surface)
                    .or_insert_with(crate::float_physics::FloatBody::at_rest);
            }
        }
    }

    /// Advances the full floating-physics simulation from either backend's
    /// live frame clock. A fixed-timestep accumulator with capped catch-up
    /// keeps integration stable across variable output cadence.
    #[allow(clippy::mutable_key_type)] // WlSurface identity is stable across body updates.
    pub(crate) fn update_float_physics_full(&mut self) {
        const FIXED_DT: f64 = 1.0 / 120.0;
        const MAX_SUBSTEPS: u32 = 8;

        let now = Instant::now();
        let elapsed = now
            .saturating_duration_since(self.float_physics_last_tick)
            .as_secs_f64();
        self.float_physics_last_tick = now;

        if self.floating_workspace.is_empty() && self.window_float_bodies.is_empty() {
            self.float_physics_accum = 0.0;
            return;
        }
        self.sync_float_physics_bodies();
        if self.window_float_bodies.is_empty() {
            self.float_physics_accum = 0.0;
            return;
        }

        self.float_physics_accum =
            (self.float_physics_accum + elapsed).min(FIXED_DT * MAX_SUBSTEPS as f64);

        let contexts: Vec<FloatBodyContext> = self
            .window_float_bodies
            .keys()
            .filter_map(|surface| self.float_body_context(surface))
            .collect();

        let mut steps = 0;
        while self.float_physics_accum >= FIXED_DT && steps < MAX_SUBSTEPS {
            self.float_physics_accum -= FIXED_DT;
            steps += 1;
            self.step_float_physics_full(&contexts, FIXED_DT);
        }
        let changed = contexts
            .iter()
            .map(|context| context.surface.clone())
            .collect();
        self.request_surface_redraws(&changed);

        // Passive at-rest bodies stay present while another body moves so
        // the collision pass can hit them. Once all motion ends, remove the
        // set atomically; recreating settled entries one-by-one here was the
        // source of the permanent idle redraw loop.
        if !crate::float_physics::bodies_need_simulation(
            self.config.float_physics.wave.enabled,
            self.window_float_bodies.values(),
        ) {
            self.window_float_bodies.clear();
        }
    }

    pub(super) fn currents_enabled(&self) -> bool {
        self.config.water_effects
            && self.config.spatial_engine == crate::config::SpatialEngine::Ocean
            && self.config.currents.enabled
            && self.config.currents.strength > f32::EPSILON
    }

    /// Advances Ocean currents from the existing backend clock. Only visible
    /// floating placements retain a record, keeping both state and continuous
    /// redraw work proportional to what can actually be seen.
    #[allow(clippy::mutable_key_type)] // WlSurface-keyed bounded visibility set
    pub(crate) fn update_currents(&mut self) {
        if !self.currents_enabled() || self.floating_workspace.is_empty() {
            self.window_currents.clear();
            return;
        }

        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let mut seen = HashSet::new();
        let mut visible = Vec::new();
        for output in outputs {
            let Some(placements) = self.render_placements(&output) else {
                continue;
            };
            for placement in placements {
                if !placement.is_floating()
                    || placement.surface().is_some_and(|surface| {
                        self.fullscreen.contains_key(surface)
                            || self.ocean.is_screen_pinned(surface)
                    })
                {
                    continue;
                }
                let Some(surface) = placement.surface().cloned() else {
                    continue;
                };
                if !seen.insert(surface.clone()) {
                    continue;
                }
                // Stable for the record's lifetime and cheap to derive.
                // Different homes decorrelate neighboring windows without a
                // random-number dependency or global unbounded seed counter.
                let rect = placement.rect;
                let phase = (rect.loc.x as f64 * 0.013
                    + rect.loc.y as f64 * 0.021
                    + rect.size.w as f64 * 0.003
                    + rect.size.h as f64 * 0.005)
                    .rem_euclid(std::f64::consts::TAU);
                visible.push((surface, phase));
            }
        }

        self.window_currents
            .retain(|surface, _| seen.contains(surface));
        let focused = self.focused_window_surface();
        let now = Instant::now();
        let mut changed = HashSet::new();
        for (surface, phase) in visible {
            let paused =
                focused.as_ref() == Some(&surface) || self.floating_dragging.contains(&surface);
            let drift = self
                .window_currents
                .entry(surface.clone())
                .or_insert_with(|| crate::currents::CurrentDrift::new(now, phase, paused));
            drift.tick(now, paused);
            if drift.needs_frame() {
                changed.insert(surface);
            }
        }
        self.request_surface_redraws(&changed);
    }

    fn buoyancy_enabled(&self) -> bool {
        self.config.water_effects && self.config.buoyancy.enabled
    }

    /// Synchronizes visible floating windows only when ownership/config/focus
    /// changed, then ticks just the short focus/drag transitions. A stable
    /// weighted desktop therefore performs no placement walk and asks for no
    /// frames on idle maintenance ticks.
    #[allow(clippy::mutable_key_type)] // WlSurface-keyed bounded visibility set
    pub(crate) fn update_buoyancy(&mut self) {
        if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            let revision = self.ocean.camera_revision();
            let moving = self.ocean.has_active_camera_motion();
            self.buoyancy_dirty |= revision != self.buoyancy_camera_revision
                || moving
                || moving != self.buoyancy_camera_was_moving;
            self.buoyancy_camera_revision = revision;
            self.buoyancy_camera_was_moving = moving;
        }
        if !self.buoyancy_enabled() {
            self.window_buoyancy.clear();
            self.buoyancy_dirty = false;
            return;
        }

        let transitioning = self
            .window_buoyancy
            .values()
            .any(crate::buoyancy::BuoyancyState::needs_frame);
        if !self.buoyancy_dirty && !transitioning {
            return;
        }

        if self.buoyancy_dirty {
            let outputs: Vec<Output> = self.space.outputs().cloned().collect();
            let mut candidates = HashSet::new();
            for output in outputs {
                let Some(placements) = self.render_placements(&output) else {
                    continue;
                };
                for placement in placements {
                    if !placement.replacement_eligible() || !placement.is_floating() {
                        continue;
                    }
                    let Some(surface) = placement.surface() else {
                        continue;
                    };
                    if self.fullscreen.contains_key(surface) || self.window_is_pinned(surface) {
                        continue;
                    }
                    candidates.insert(surface.clone());
                }
            }

            let mut eligible = HashSet::new();
            let now = Instant::now();
            for surface in candidates {
                let weight = self
                    .resolve_window_rules_for(&surface)
                    .weight
                    .unwrap_or(self.config.buoyancy.default_weight)
                    .clamp(0.0, 1.0) as f64;
                if weight <= f64::EPSILON {
                    continue;
                }
                eligible.insert(surface.clone());
                self.window_buoyancy
                    .entry(surface)
                    .and_modify(|state| state.set_configured_weight(weight))
                    .or_insert_with(|| crate::buoyancy::BuoyancyState::new(now, weight));
            }
            self.window_buoyancy
                .retain(|surface, _| eligible.contains(surface));
            self.buoyancy_dirty = false;
        }

        let focused = self.focused_window_surface();
        let now = Instant::now();
        let settle = self.config.buoyancy.settle_ms as f64 / 1000.0;
        let mut changed = HashSet::new();
        for (surface, state) in &mut self.window_buoyancy {
            let paused =
                focused.as_ref() == Some(surface) || self.floating_dragging.contains(surface);
            let target = if paused {
                0.0
            } else {
                state.configured_weight()
            };
            state.tick(now, target, settle);
            if state.needs_frame() {
                changed.insert(surface.clone());
            }
        }
        self.request_surface_redraws(&changed);
    }

    /// One fixed `dt` physics substep across every F1 `full` body: wave/
    /// spring forcing, then pairwise inter-window collisions, then
    /// home-output edge bounces -- all velocity-only updates layered on
    /// top of the position integration `step_body` already did for this
    /// substep. Bodies are pulled into a plain local `Vec` for the
    /// duration (a `HashMap` can't safely hand out two simultaneous `&mut`
    /// entries for an arbitrary pair of keys without extra ceremony this
    /// realistically single-digit window count doesn't need) and written
    /// back at the end.
    fn step_float_physics_full(&mut self, contexts: &[FloatBodyContext], dt: f64) {
        let cfg = self.config.float_physics;
        let wave = cfg.wave;
        // Reuses `light` tier's two feel knobs as a damped spring's
        // stiffness/drag instead of adding a parallel pair for one more
        // tier -- see `FloatPhysicsConfig::frequency`/`damping`'s own doc
        // comments.
        let stiffness = (std::f64::consts::TAU * cfg.frequency as f64).powi(2);
        let drag = 2.0 * cfg.damping as f64;
        let elapsed = self.float_physics_wave_clock.elapsed().as_secs_f64();

        let mut bodies: Vec<crate::float_physics::FloatBody> = contexts
            .iter()
            .map(|ctx| {
                self.window_float_bodies
                    .get(&ctx.surface)
                    .copied()
                    .unwrap_or_default()
            })
            .collect();

        for (i, ctx) in contexts.iter().enumerate() {
            let target = if wave.enabled {
                crate::float_physics::wave_target(
                    ctx.anchor.0 + ctx.anchor.2 / 2.0,
                    elapsed,
                    wave.amplitude as f64,
                    wave.wavelength as f64,
                    wave.speed as f64,
                )
            } else {
                (0.0, 0.0)
            };
            crate::float_physics::step_body(
                &mut bodies[i],
                target,
                stiffness,
                drag,
                cfg.max_offset as f64,
                dt,
            );
        }

        for i in 0..contexts.len() {
            for j in (i + 1)..contexts.len() {
                let a_rect = (
                    contexts[i].anchor.0 + bodies[i].offset.0,
                    contexts[i].anchor.1 + bodies[i].offset.1,
                    contexts[i].anchor.2,
                    contexts[i].anchor.3,
                );
                let b_rect = (
                    contexts[j].anchor.0 + bodies[j].offset.0,
                    contexts[j].anchor.1 + bodies[j].offset.1,
                    contexts[j].anchor.2,
                    contexts[j].anchor.3,
                );
                let (left, right) = bodies.split_at_mut(j);
                crate::float_physics::resolve_collision(
                    a_rect,
                    b_rect,
                    &mut left[i].velocity,
                    &mut right[0].velocity,
                    contexts[i].mass,
                    contexts[j].mass,
                    cfg.restitution as f64,
                );
            }
        }

        if cfg.bounce_off_edges {
            for (i, ctx) in contexts.iter().enumerate() {
                if let Some(bounds) = ctx.output_bounds {
                    let rect = (
                        ctx.anchor.0 + bodies[i].offset.0,
                        ctx.anchor.1 + bodies[i].offset.1,
                        ctx.anchor.2,
                        ctx.anchor.3,
                    );
                    crate::float_physics::resolve_edge_collision(
                        rect,
                        &mut bodies[i].velocity,
                        bounds,
                        cfg.restitution as f64,
                    );
                }
            }
        }

        for (ctx, body) in contexts.iter().zip(bodies) {
            self.window_float_bodies.insert(ctx.surface.clone(), body);
        }
    }

    pub(crate) fn start_window_open_animation(&mut self, surface: &WlSurface) {
        self.closing_window_animations
            .retain(|closing| closing.surface != *surface);
        let cascade = self
            .cascade_liquid_applies(surface)
            .then(|| crate::cascade_transition::CascadeTransition::pour(&self.config.cascade))
            .flatten();
        if let Some(cascade) = cascade {
            self.cascade_window_animations
                .insert(surface.clone(), cascade);
        } else {
            self.cascade_window_animations.remove(surface);
        }
        let animations = self.config.animations.clone();
        let ordinary = animations.enabled
            && animations.open.enabled
            && !(self.cascade_window_animations.contains_key(surface)
                && self.config.cascade.replace_motion);
        if !ordinary {
            self.window_open_animations.remove(surface);
            if self.cascade_window_animations.contains_key(surface) {
                self.request_redraw();
            }
            return;
        }
        let offset = self
            .window_lifecycle_offset(surface, &animations.open)
            .unwrap_or_else(|| {
                Point::from((
                    animations.open.offset.0 as f64,
                    animations.open.offset.1 as f64,
                ))
            });
        self.window_open_animations.insert(
            surface.clone(),
            crate::window_animation::WindowVisualAnimation::open(
                &animations.open,
                animations.slowdown,
                offset,
            ),
        );
        self.request_redraw();
    }

    pub(super) fn cascade_liquid_applies(&self, surface: &WlSurface) -> bool {
        if !self.config.water_effects || !self.config.spatial_engine.uses_workspaces() {
            return false;
        }
        let (Some(output), Some(workspace)) = (
            self.layout.output_of(surface),
            self.layout.workspace_of(surface),
        ) else {
            return false;
        };
        self.layout.algorithm(output, workspace) == crate::config::LayoutAlgorithm::Cascade
    }

    fn window_lifecycle_offset(
        &self,
        surface: &WlSurface,
        config: &crate::config::WindowAnimationConfig,
    ) -> Option<Point<f64, Logical>> {
        let window = self.mapped_toplevel_window(surface)?;
        let output = self
            .rendered_output_for_surface(surface)
            .or_else(|| self.output_for_window(&window))?;
        let output_rect = self.space.output_geometry(&output)?;
        let window_rect = if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            self.rendered_rect_on_output(surface, &output)?
        } else {
            self.tiled_rect_for_surface(surface)
                .or_else(|| self.floating_workspace.get(surface).map(|tag| tag.rect))
                .or_else(|| self.space.element_geometry(&window))?
        };
        Some(crate::window_animation::lifecycle_offset(
            config,
            window_rect,
            output_rect,
        ))
    }

    /// Starts/re-targets visual movement while preserving the current
    /// on-screen position. Input and layout already use `new_location`.
    pub(super) fn start_window_move_animation(
        &mut self,
        surface: &WlSurface,
        old_rect: Rectangle<i32, Logical>,
        new_rect: Rectangle<i32, Logical>,
    ) {
        let animations = &self.config.animations;
        if !animations.enabled || !animations.movement.enabled {
            self.window_move_animations.remove(surface);
            return;
        }
        let current = self
            .window_move_animations
            .get(surface)
            .filter(|animation| !animation.finished())
            .map(|animation| animation.sample());
        let current_offset = current
            .map(|sample| sample.offset)
            .unwrap_or_else(|| Point::from((0.0, 0.0)));
        let current_size = current
            .and_then(|sample| sample.size)
            .unwrap_or_else(|| old_rect.size.to_f64());
        let from_offset = Point::from((
            (f64::from(old_rect.loc.x) - f64::from(new_rect.loc.x)) + current_offset.x,
            (f64::from(old_rect.loc.y) - f64::from(new_rect.loc.y)) + current_offset.y,
        ));
        let size_changed = animations.movement.animate_size
            && ((current_size.w - new_rect.size.w as f64).abs() >= 0.01
                || (current_size.h - new_rect.size.h as f64).abs() >= 0.01);
        if from_offset.x.abs() < 0.01 && from_offset.y.abs() < 0.01 && !size_changed {
            return;
        }
        self.window_move_animations.insert(
            surface.clone(),
            crate::window_animation::WindowVisualAnimation::movement(
                &animations.movement,
                animations.slowdown,
                from_offset,
                current_size,
                new_rect.size.to_f64(),
            ),
        );
        let scroll_output = (self.config.spatial_engine == crate::config::SpatialEngine::Scroll)
            .then(|| {
                self.layout
                    .output_of(surface)
                    .and_then(|name| self.output_by_name(name))
            })
            .flatten();
        if let Some(output) = scroll_output {
            self.request_output_redraw(&output);
        } else {
            self.request_redraw();
        }
    }

    /// Transfers the last drawable surface snapshot into a short close
    /// animation. Smithay's null-buffer commit still proceeds normally.
    pub(crate) fn start_window_close_animation(&mut self, surface: &WlSurface) {
        let animations = self.config.animations.clone();
        let cascade = self
            .cascade_liquid_applies(surface)
            .then(|| crate::cascade_transition::CascadeTransition::drain(&self.config.cascade))
            .flatten();
        let ordinary_enabled = animations.enabled
            && animations.close.enabled
            && !(cascade.is_some() && self.config.cascade.replace_motion);
        if !ordinary_enabled && cascade.is_none() {
            return;
        }
        let offset = self
            .window_lifecycle_offset(surface, &animations.close)
            .unwrap_or_else(|| {
                Point::from((
                    animations.close.offset.0 as f64,
                    animations.close.offset.1 as f64,
                ))
            });
        let Some(snapshot) = self.window_frame_snapshots.remove(surface) else {
            return;
        };
        let pixel_budget = self.close_snapshot_pixel_budget();
        let snapshot_pixels = snapshot.estimated_pixels();
        let count_budget = animations.max_closing_snapshots;
        self.window_open_animations.remove(surface);
        self.cascade_window_animations.remove(surface);
        self.window_move_animations.remove(surface);
        self.window_viscosity.remove(surface);
        self.closing_window_animations
            .retain(|closing| closing.surface != *surface && !closing.finished());
        let retained_pixels: Vec<u64> = self
            .closing_window_animations
            .iter()
            .map(|closing| closing.snapshot.estimated_pixels())
            .collect();
        let Some(evictions) = crate::window_animation::close_snapshot_evictions(
            &retained_pixels,
            snapshot_pixels,
            count_budget,
            pixel_budget,
        ) else {
            return;
        };
        self.closing_window_animations.drain(..evictions);
        self.closing_window_animations
            .push(crate::window_animation::ClosingWindowAnimation::new(
                surface.clone(),
                snapshot,
                ordinary_enabled.then(|| {
                    crate::window_animation::WindowVisualAnimation::close(
                        &animations.close,
                        animations.slowdown,
                        offset,
                    )
                }),
                cascade,
            ));
        self.request_redraw();
    }

    /// Converts the configured output-area multiple into a live physical
    /// pixel budget. Mode sizes come from Smithay's current outputs, so this
    /// scales with nested, HiDPI, rotated, and multi-output sessions without
    /// assuming any particular monitor resolution.
    fn close_snapshot_pixel_budget(&self) -> u64 {
        let output_pixels =
            self.space
                .outputs()
                .filter_map(Output::current_mode)
                .fold(0_u64, |total, mode| {
                    let width = u64::try_from(mode.size.w.max(0)).unwrap_or(u64::MAX);
                    let height = u64::try_from(mode.size.h.max(0)).unwrap_or(u64::MAX);
                    total.saturating_add(width.saturating_mul(height))
                });
        ((output_pixels as f64) * f64::from(self.config.animations.close_snapshot_output_budget))
            .round()
            .clamp(0.0, u64::MAX as f64) as u64
    }

    pub(crate) fn closing_window_frame_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<crate::backend::udev::OutputRenderElements> {
        let output_scale = output.current_scale().fractional_scale();
        let scale = Scale::from(output_scale);
        let needs_cascade = self
            .closing_window_animations
            .iter()
            .any(|closing| closing.snapshot.output == output.name() && closing.cascade.is_some());
        let cascade_program = needs_cascade
            .then(|| {
                crate::decoration::rounded_surface_program(
                    &mut self.rounded_surface_program,
                    renderer,
                )
            })
            .flatten();
        let mut result = Vec::new();
        for closing in self
            .closing_window_animations
            .iter_mut()
            .filter(|closing| closing.snapshot.output == output.name())
        {
            result.extend(
                closing
                    .frame_elements(scale, cascade_program.clone())
                    .into_iter()
                    .map(crate::backend::udev::OutputRenderElements::WindowSnapshot),
            );
        }
        result
    }

    pub(super) fn focus_window_with_ripple(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
        animate_handoff: bool,
        origin: FocusOrigin,
    ) {
        if origin == FocusOrigin::Navigation {
            if let Some(surface) = surface.as_ref() {
                self.reveal_scroll_window(surface);
            }
        }
        let previous_focus = self.window_focus.clone();
        self.on_demand_layer_focus = None;
        self.window_focus = surface.filter(|surface| self.window_is_visible(surface));
        if let Some(surface) = self.window_focus.clone() {
            self.note_depth_attention(&surface);
        }
        self.reconcile_keyboard_focus(serial);

        // Focus-change ripple: only on a user-visible transition between two
        // real windows, not on the very first focus (from None), focus being
        // dropped (to None), or a map transaction. A freshly mapped window
        // receives focus and its map ripple in the same transaction; stacking
        // a focus ripple there makes two presets fight over the same frame.
        if let Some(new_surface) = self.window_focus.clone() {
            if animate_handoff
                && previous_focus.as_ref() != Some(&new_surface)
                && previous_focus.is_some()
            {
                self.spawn_ripple(&new_surface, crate::config::RippleTrigger::Focus);
            }
        }
    }

    /// Prunes completed animations and reports whether another frame is
    /// needed even when no client or input event produced damage.
    pub fn has_active_animation(&mut self) -> bool {
        // Completion cleanup must not depend on an output actually being
        // rendered. A DPMS-off/minimized output can skip its frame path;
        // pruning here still releases the full-output transition texture
        // on the event-loop tick that notices the animation ended.
        self.workspace_transitions
            .retain(|_, transition| !transition.finished());
        self.workspace_glides
            .retain(|_, transition| !transition.finished());
        self.depth_transitions
            .retain(|_, transition| !transition.finished());
        self.window_open_animations
            .retain(|_, animation| !animation.finished());
        self.cascade_window_animations
            .retain(|_, animation| !animation.finished());
        let scroll_was_moving = self.config.spatial_engine == crate::config::SpatialEngine::Scroll
            && !self.window_move_animations.is_empty();
        self.window_move_animations
            .retain(|_, animation| !animation.finished());
        if scroll_was_moving {
            self.queue_scroll_pointer_refresh();
        }
        self.window_viscosity.retain(|_, motion| !motion.finished());
        self.window_sway.retain(|_, sway| !sway.finished());
        self.window_float_physics
            .retain(|_, physics| !physics.finished());
        let stale_ambient: Vec<WlSurface> = self
            .window_float_ambient
            .keys()
            .filter(|surface| !self.is_floating(surface))
            .cloned()
            .collect();
        for surface in stale_ambient {
            self.window_float_ambient.remove(&surface);
        }
        self.swim_cameras.retain(|_, camera| !camera.at_rest());
        if self
            .workspace_scroll_cameras
            .values()
            .any(crate::swim::WorkspaceScrollCamera::at_rest)
            && matches!(self.session_lock, SessionLock::Unlocked)
        {
            // The camera samples its final zero offset on the tick that
            // finishes it. Ask for one more frame before dropping the entry
            // so the last presented frame is not left a fraction of a
            // workspace off.
            self.request_redraw();
        }
        self.workspace_scroll_cameras
            .retain(|_, camera| !camera.at_rest());
        let camera_was_active = self.ocean.has_active_camera_motion();
        self.ocean.prune_completed_camera_motions();
        if camera_was_active && !self.ocean.has_active_camera_motion() {
            self.queue_pointer_focus_refresh();
        }
        self.closing_window_animations
            .retain(|closing| !closing.finished());
        self.ripples.retain(|ripple| !ripple.finished());
        // Desktop content is invisible behind a session lock. Suppress the
        // animation predicate so the fast timer, per-CRTC dirty re-arm, and
        // global redraw request all stand down; lock surfaces render through
        // their own commit path. Pruning above still runs so finished
        // animations are cleaned up regardless of lock state.
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            return false;
        }
        self.toast
            .as_ref()
            .is_some_and(|toast| toast.needs_continued_redraw())
            || !self.ripples.is_empty()
            || !self.workspace_transitions.is_empty()
            || !self.workspace_glides.is_empty()
            || !self.depth_transitions.is_empty()
            || !self.window_open_animations.is_empty()
            || !self.cascade_window_animations.is_empty()
            || !self.window_move_animations.is_empty()
            || !self.window_viscosity.is_empty()
            || !self.window_sway.is_empty()
            || !self.window_float_physics.is_empty()
            // Deliberately not pruned by a `finished()` check like the
            // physics map above -- ambient entries persist until
            // `toggle_float_ambient` removes them, so their mere presence
            // (not decay state) is what should keep the pump alive.
            || !self.window_float_ambient.is_empty()
            // Pruned inside `update_float_physics_full` itself (which also
            // needs to know whether the wave field is still forcing a
            // given body, not just whether it's numerically settled), not
            // here -- see that function's own retain call.
            || !self.window_float_bodies.is_empty()
            || self
                .window_currents
                .values()
                .any(crate::currents::CurrentDrift::needs_frame)
            || self
                .window_buoyancy
                .values()
                .any(crate::buoyancy::BuoyancyState::needs_frame)
            || !self.swim_cameras.is_empty()
            || !self.workspace_scroll_cameras.is_empty()
            || self.ocean.has_active_camera_motion()
            || (self.config.spatial_engine == crate::config::SpatialEngine::Ocean
                && self.config.ocean.canvas_marker
                && self.ocean_canvases.values().any(|canvas| {
                    canvas.marker_active(Duration::from_millis(
                        self.config.ocean.canvas_marker_fade_ms,
                    ))
                }))
            || !self.closing_window_animations.is_empty()
            || self.animated_borders_possible()
            || self.glass_animation_active()
            || self.caustics_active()
    }

    /// Continue only the animations present in this output's actual scene.
    /// The backend passes its frame-owned placements, including previews;
    /// durable workspace ownership and Space overlap are not camera visibility.
    /// Completion cleanup remains global in `has_active_animation`.
    pub(crate) fn output_has_active_animation(
        &self,
        output: &Output,
        placements: &[crate::placement::PlacedWindow],
    ) -> bool {
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            return false;
        }
        let name = output.name();
        self.toast
            .as_ref()
            .is_some_and(|toast| toast.needs_continued_redraw())
            || self.ripples.iter().any(|ripple| ripple.output == name)
            || self.workspace_transitions.contains_key(&name)
            || self.workspace_glides.contains_key(&name)
            || self.depth_transitions.contains_key(&name)
            || self.swim_cameras.contains_key(&name)
            || self.workspace_scroll_cameras.contains_key(&name)
            || self.ocean.has_active_camera_motion_on(&name)
            || (self.config.spatial_engine == crate::config::SpatialEngine::Ocean
                && self.config.ocean.canvas_marker
                && self.ocean_canvases.get(&name).is_some_and(|canvas| {
                    canvas.marker_active(Duration::from_millis(
                        self.config.ocean.canvas_marker_fade_ms,
                    ))
                }))
            || self
                .closing_window_animations
                .iter()
                .any(|closing| closing.snapshot.output == name)
            || placements
                .iter()
                .filter_map(|placed| placed.surface())
                .any(|surface| {
                    self.window_open_animations.contains_key(surface)
                        || self.cascade_window_animations.contains_key(surface)
                        || self.window_move_animations.contains_key(surface)
                        || self.window_viscosity.contains_key(surface)
                        || self.window_sway.contains_key(surface)
                        || self.window_float_physics.contains_key(surface)
                        || self.window_float_ambient.contains_key(surface)
                        || self.window_float_bodies.contains_key(surface)
                        || self
                            .window_currents
                            .get(surface)
                            .is_some_and(crate::currents::CurrentDrift::needs_frame)
                        || self
                            .window_buoyancy
                            .get(surface)
                            .is_some_and(crate::buoyancy::BuoyancyState::needs_frame)
                        || self.surface_has_animated_border(surface)
                        || self.surface_has_glass_animation(surface)
                })
            || self
                .caustics_redraw_delay_for_output(output)
                .is_some_and(|delay| delay.is_zero())
    }

    /// Whether any water-glass window's refraction is currently animating
    /// and needs the frame pump kept alive: always in `ambient` mode, only
    /// during the post-disturbance settle tail in `reactive` mode, never
    /// in `static` mode. Entries are also cross-checked against the water
    /// glass branch's own eligibility (backdrop captured, mode resolves to
    /// water) so a window hot-reloaded to frost or plain glass can't keep
    /// the compositor ticking through its leftover entry.
    fn glass_animation_active(&self) -> bool {
        self.glass_anim
            .keys()
            .any(|surface| self.surface_has_glass_animation(surface))
    }

    fn surface_has_glass_animation(&self, surface: &WlSurface) -> bool {
        if !self.config.water_effects
            || !self.backdrop_textures.contains_key(surface)
            || matches!(
                self.window_glass_modes.get(surface),
                Some(crate::config::GlassMode::Frost) | Some(crate::config::GlassMode::Plain)
            )
        {
            return false;
        }
        let Some(anim) = self.glass_anim.get(surface) else {
            return false;
        };
        match self.config.water_glass.animation {
            crate::config::GlassAnimation::Static => false,
            crate::config::GlassAnimation::Ambient => true,
            crate::config::GlassAnimation::Reactive => {
                anim.envelope(self.config.water_glass.settle_ms) > 0.0
            }
        }
    }

    /// Spawns an impulse ripple (`ripple.rs`, Phase R1) on `trigger`,
    /// centered per the resolved `ripple { }` config (and any per-rule
    /// `ripple { }` override matched by `surface`'s app_id/title). The
    /// Map, focus-change, and urgent triggers all reuse this path.
    ///
    /// No-op when:
    /// - `water_effects` is off (the master identity toggle),
    /// - the resolved ripple config's `enabled` is `false` (e.g. a rule
    ///   set `ripple = none` for this app),
    /// - the resolved config's `triggers` doesn't include `trigger`,
    /// - the window has no current position (not yet mapped, on an
    ///   inactive workspace),
    /// - the ripple cap is reached (bounding the worst-case cost of
    ///   rapid window mapping so `self.ripples` can't grow unboundedly).
    pub(crate) fn spawn_ripple(
        &mut self,
        surface: &WlSurface,
        trigger: crate::config::RippleTrigger,
    ) {
        use crate::config::RippleAnchor;
        if !self.config.water_effects {
            return;
        }
        const MAX_ACTIVE_RIPPLES: usize = 16;
        if self.ripples.iter().filter(|r| !r.finished()).count() >= MAX_ACTIVE_RIPPLES {
            return;
        }
        // Resolve the per-window-rule ripple override and merge over the
        // global `ripple { }` block. `resolve_window_rules` returns a
        // WindowRule whose `ripple` field is `Some(...)` only if at
        // least one matching rule declared a ripple sub-block (or the
        // `ripple = none` shorthand).
        let rule = self.resolve_window_rules_for(surface);
        let mut cfg = self
            .config
            .resolve_ripple_config(rule.ripple.as_ref(), trigger);
        if cfg.enabled == Some(false) || !cfg.fires_on(trigger) {
            return;
        }

        let Some(window) = self.mapped_toplevel_window(surface) else {
            return;
        };
        // A Classic tiled window belongs to its layout's output even while
        // its committed size still overlaps a neighbor.
        let layout_owner = (self.config.spatial_engine.uses_workspaces())
            .then(|| self.layout.output_of(surface))
            .flatten()
            .and_then(|name| self.output_by_name(name));
        let Some(output) = layout_owner
            .or_else(|| self.rendered_output_for_surface(surface))
            .or_else(|| self.output_for_window(&window))
        else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        // A window's `window.geometry()` can lag one client
        // round-trip behind retile()'s own just-computed target rect:
        // retile() only *sends* a configure proposing the new size, it
        // doesn't block on the client acking and committing a matching
        // buffer -- so reading geometry() synchronously right after (as
        // the map trigger does, straight after retile() in map_toplevel)
        // sees the window's pre-tile size at its already-updated post-tile
        // location. Confirmed live: a freshly mapped tiled window's map
        // ripple anchored at (8, 8) -- the gap offset, i.e. loc with a
        // near-zero size -- while a later ripple on the same settled
        // window anchored at its real center. Same "space reflects where
        // this is being moved to, not its real slot" gap
        // `TileMoveGrab::drop` already hit; its own doc comment is why
        // this reads from the retained placement state instead when possible.
        // A first-map floating conversion has the same lag: its
        // `FloatingTag::rect` already records the configure target while the
        // committed buffer may still have the old size. Use that rect for an
        // ordinary floater so its map ripple lands at the settled center,
        // including an exact rule-provided position/size.
        // Skipped for fullscreen/pseudo-tiled windows, whose rect
        // `retile()` overrides after reading it from `layout()` -- rarer,
        // and reusing the plain path here is a no-op change for them, not
        // a regression.
        let win = if self.config.spatial_engine == crate::config::SpatialEngine::Ocean {
            self.rendered_rect_on_output(surface, &output)
        } else {
            self.layout
                .workspace_of(surface)
                .filter(|_| {
                    !self.fullscreen.contains_key(surface) && !self.pseudo_tiled.contains(surface)
                })
                .zip(self.output_tiling_area(&output))
                .and_then(|(workspace, area)| {
                    self.layout
                        .layout(
                            &output.name(),
                            workspace,
                            area,
                            self.gaps_for(&output.name(), workspace),
                        )
                        .into_iter()
                        .find(|(w, _)| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
                        .map(|(_, rect)| rect)
                })
                .or_else(|| {
                    (!self.fullscreen.contains_key(surface)
                        && !self.maximized.contains_key(surface))
                    .then(|| self.floating_workspace.get(surface).map(|tag| tag.rect))
                    .flatten()
                })
                .or_else(|| {
                    self.space
                        .element_location(&window)
                        .map(|loc| Rectangle::new(loc, window.geometry().size))
                })
        };
        let Some(win) = win else {
            return;
        };
        // Ripple geometry is output-local. Ocean's authoritative placement
        // above already includes its camera/pin transform.
        let win = Rectangle::new(win.loc - output_geo.loc, win.size);
        cfg.peak_radius = Some(cfg.radius_for_window(win.size.w as f32, win.size.h as f32));
        let anchor = cfg.anchor.unwrap_or(RippleAnchor::Center);
        let (dx, dy) = cfg.offset.unwrap_or((0, 0));
        let pointer_local = self
            .seat
            .get_pointer()
            .map(|p| p.current_location())
            .map(|g| Point::from((g.x - output_geo.loc.x as f64, g.y - output_geo.loc.y as f64)));
        let base = crate::ripple::anchor_point(
            win,
            anchor,
            pointer_local,
            cfg.edge_position.unwrap_or(0.5),
            cfg.edge_offset.unwrap_or(0.0),
        );
        let center = Point::from((base.x + dx as f64, base.y + dy as f64));
        tracing::debug!(
            trigger = ?trigger,
            output = output.name(),
            center = ?center,
            preset = ?cfg.preset,
            shapes = ?cfg.shapes,
            color = ?cfg.color,
            layer = ?cfg.layer,
            "spawning ripple"
        );
        self.ripples
            .push(crate::ripple::Ripple::new(output.name(), center, cfg));
        self.request_redraw();
    }

    /// Back-compat alias for the first call site, kept while focus/urgent
    /// triggers are being wired up -- new call sites should use
    /// `spawn_ripple` with an explicit trigger directly.
    pub(crate) fn spawn_window_map_ripple(&mut self, surface: &WlSurface) {
        self.spawn_ripple(surface, crate::config::RippleTrigger::Map);
    }

    /// Builds, for every not-yet-finished ripple on `output`, a
    /// `RippleElement` ready to prepend into a frame's render-element
    /// list. Prunes finished ripples as a side effect, so this Vec stays
    /// bounded by the in-flight count between frames even if a caller
    /// forgets to drain it. Lazily compiles the shader on first call.
    ///
    /// Ripples are grouped by their configured `layer` (`RippleLayer`)
    /// so the backend element-chain builders can splice each group into
    /// the right z-position: `AboveWindows` ripples go over windows but
    /// under chrome, `BelowWindows` go between wallpaper and windows,
    /// `AboveAll` goes at the very front, `BelowAll` at the very back.
    pub(crate) fn ripple_frame_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> RippleLayers {
        let mut layers = RippleLayers::default();
        if !self.config.water_effects || self.ripples.is_empty() {
            return layers;
        }
        self.ripples.retain(|r| !r.finished());
        if self.ripples.is_empty() {
            return layers;
        }
        let Some(program) = crate::ripple::ripple_program(&mut self.ripple_program, renderer)
        else {
            return layers;
        };
        let output_name = output.name();
        for ripple in self.ripples.iter_mut() {
            if ripple.output != output_name {
                continue;
            }
            for element in crate::ripple::RippleElement::from_ripple(ripple, program.clone()) {
                layers.push(ripple.layer(), element);
            }
        }
        layers
    }

    pub(super) fn retile_with_viscosity(
        &mut self,
        interactive: bool,
        skip_configure_for: Option<&WlSurface>,
    ) {
        self.retile_filtered(interactive, skip_configure_for, None);
    }
}
