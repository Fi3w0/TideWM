//! Cache maintenance on backend ticks. Resource owners still clean up on close.
use super::{render::RenderGpuState, Smallvil};
use crate::{
    config::EbbMode,
    ebb::{
        pressure::PressureWatch,
        trim::{self, PassKind, PassReport},
    },
    ipc::{read_self_proc, tide_texture_estimate},
};
use smithay::{
    desktop::layer_map_for_output,
    reexports::{
        calloop::{generic::Generic, Interest, Mode, PostAction},
        wayland_server::{protocol::wl_surface::WlSurface, Resource},
    },
};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

impl RenderGpuState {
    /// Only rebuildable caches. Frame/close snapshots and client buffers are
    /// intentionally absent. Smithay queues GPU deletion on handle drop.
    fn trim_surface_cache(&mut self, surface: &WlSurface, dead: bool) -> (usize, u64) {
        let mut entries = 0;
        let mut bytes = 0_u64;
        if let Some(cache) = self.backdrop_textures.remove(surface) {
            entries += 1;
            bytes = bytes.saturating_add(cache.estimated_texture_bytes());
        }
        if let Some(cache) = self.layer_alpha_masks.remove(surface) {
            entries += 1;
            bytes = bytes.saturating_add(cache.estimated_texture_bytes());
        }
        if self.depth_schematics.remove(surface).is_some() {
            entries += 1;
        }
        if dead {
            if let Some(instance) = self.custom_shader_instances.remove(surface) {
                entries += 1;
                bytes = bytes.saturating_add(instance.target_bytes());
            }
            self.custom_shader_bypassed.remove(surface);
        } else if let Some(instance) = self.custom_shader_instances.get_mut(surface) {
            let released = instance.trim_targets();
            if released > 0 {
                entries += 1;
                bytes = bytes.saturating_add(released);
            }
        }
        (entries, bytes)
    }
}

impl Smallvil {
    pub(crate) fn queue_ebb_pass(&mut self, deep: bool) -> Result<(), &'static str> {
        if self.config.ebb.mode == EbbMode::Off {
            return Err("Ebb is off; monitoring remains active but trimming is disabled");
        }
        self.ebb.trim_blocked = false;
        self.ebb.trim.request(if deep {
            PassKind::Deep
        } else {
            PassKind::Light
        });
        Ok(())
    }

    pub(crate) fn ebb_trim_busy(&self) -> bool {
        #[cfg(feature = "screencast")]
        if self
            .screencast
            .as_ref()
            .is_some_and(|cast| cast.has_live_targets())
        {
            return true;
        }
        let motion = &self.render.motion;
        self.seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
            || self.input.compositor_gesture.is_some()
            || !self.pending_captures.is_empty()
            || !self.capture_sessions.is_empty()
            || !motion.window_open_animations.is_empty()
            || !motion.window_move_animations.is_empty()
            || !motion.cascade_window_animations.is_empty()
            || !motion.closing_window_animations.is_empty()
            || !motion.workspace_transitions.is_empty()
            || !motion.workspace_glides.is_empty()
            || !motion.depth_transitions.is_empty()
            || !motion.pending_workspace_transitions.is_empty()
            || motion.swim_cameras.values().any(|camera| !camera.at_rest())
            || motion
                .workspace_scroll_cameras
                .values()
                .any(|camera| !camera.at_rest())
            || self.spatial.ocean.has_active_camera_motion()
    }

    /// Uses the same placements as rendering, across every output. Occluded
    /// windows and all mapped layers stay protected conservatively. Off-screen
    /// Scroll/Ocean windows are absent from this common render contract.
    #[allow(clippy::mutable_key_type)] // temporary existing-resource identity set, never retained
    fn ebb_protected_surfaces(&self, screens_off: bool) -> HashSet<WlSurface> {
        let mut protected = HashSet::new();
        if !screens_off {
            for output in self.space.outputs() {
                // Unknown placements mean visibility is unknown: protect every
                // cache rather than guessing that an output is empty.
                let Some(placements) = self.render_placements(output) else {
                    return self.ebb_cache_surfaces();
                };
                for placement in placements {
                    if let Some(surface) = placement.surface() {
                        protected.insert(surface.clone());
                    }
                }
                for layer in layer_map_for_output(output).layers() {
                    protected.insert(layer.wl_surface().clone());
                }
            }
        }
        protected.extend(self.spatial.workspaces.pinned.iter().cloned());
        protected.extend(self.fullscreen.keys().cloned());
        protected
    }

    #[allow(clippy::mutable_key_type)] // callback-local resource identities only
    fn ebb_cache_surfaces(&self) -> HashSet<WlSurface> {
        self.render
            .gpu
            .backdrop_textures
            .keys()
            .chain(self.render.gpu.layer_alpha_masks.keys())
            .chain(self.render.gpu.depth_schematics.keys())
            .chain(
                self.render
                    .gpu
                    .custom_shader_instances
                    .iter()
                    .filter(|(_, instance)| instance.target_bytes() > 0)
                    .map(|(surface, _)| surface),
            )
            .cloned()
            .collect()
    }

    #[allow(clippy::mutable_key_type)] // stored clocks use weak resources, never Window handles
    pub(super) fn ebb_observe_hidden(&mut self, screens_off: bool) {
        self.refresh_space();
        let now = self.start_time.elapsed().as_secs();
        let cached = self.ebb_cache_surfaces();
        let protected = self.ebb_protected_surfaces(screens_off);
        self.ebb.trim.hidden_since.retain(|weak, _| {
            weak.upgrade()
                .is_ok_and(|surface| cached.contains(&surface) && !protected.contains(&surface))
        });
        for surface in cached {
            if surface.is_alive() && !protected.contains(&surface) {
                self.ebb
                    .trim
                    .hidden_since
                    .entry(surface.downgrade())
                    .or_insert(now);
            }
        }
    }

    #[allow(clippy::mutable_key_type)] // short-lived resource sets, dropped before returning
    pub(crate) fn ebb_maintain(&mut self, screens_off: bool) {
        let now = self.start_time.elapsed().as_secs();
        let config = self.config.ebb.clone();
        let interval = u64::from(config.check_every_ms).div_ceil(1000);
        let Some(kind) = self.ebb.trim.due(
            now,
            config.mode,
            self.input.last_activity.elapsed().as_millis(),
            self.ebb.away_since_secs.is_some(),
            config.light_after_ms,
            interval,
        ) else {
            return;
        };
        // Prune finished one-shot animations through the existing owner path.
        // Continuous decorative animation cannot hold hidden caches hostage;
        // its visible surfaces are protected by placements below.
        self.has_active_animation();
        if self.ebb_trim_busy() {
            self.ebb.trim.deferred_reason =
                Some("grab, structural animation, camera movement or capture consumer active");
            return;
        }
        let started = Instant::now();
        // Classic placements consume Space's output-membership cache. A map
        // may precede the backend's normal refresh, so renew it before any
        // visibility decision, including manual passes immediately after map.
        self.refresh_space();
        let budget = Duration::from_millis(u64::from(config.pass_budget_ms));
        let protected = self.ebb_protected_surfaces(screens_off);
        let mut candidates: Vec<_> = self.ebb_cache_surfaces().into_iter().collect();
        if !candidates.is_empty() {
            let rotate = self.ebb.trim.cursor % candidates.len();
            candidates.rotate_left(rotate);
        }
        let textures = tide_texture_estimate(self).tide_texture_estimate_bytes;
        let pss = read_self_proc().map(|(pss, _, _)| pss.saturating_mul(1024));
        let audit_only = self.ebb.trim.requested.is_none()
            && kind == PassKind::Light
            && self.input.last_activity.elapsed().as_millis() < u128::from(config.light_after_ms)
            && !(self.ebb.trim.pressure_pending
                && matches!(config.mode, EbbMode::Balanced | EbbMode::Aggressive));
        let allowed = self.ebb.caps_allow_trim(&config, pss, textures) && !audit_only;
        let mut report = PassReport {
            kind: Some(kind),
            uptime_secs: now,
            ..Default::default()
        };
        for surface in candidates {
            if started.elapsed() >= budget {
                report.budget_exhausted = true;
                break;
            }
            report.examined += 1;
            let alive = surface.is_alive();
            let mode = if alive && self.foreign_toplevels.contains_key(&surface) {
                self.resolve_window_rules_for(&surface)
                    .ebb
                    .unwrap_or(config.mode)
            } else {
                config.mode
            };
            if alive && !allowed {
                continue;
            }
            let hidden = self
                .ebb
                .trim
                .hidden_since
                .get(&surface.downgrade())
                .copied();
            if !trim::eligible(mode, kind, hidden, now, protected.contains(&surface), alive) {
                continue;
            }
            // No event dispatch or frame can interleave this callback. Current
            // visibility is rechecked in this pass, not trusted from the clock.
            let (entries, bytes) = self.render.gpu.trim_surface_cache(&surface, !alive);
            report.entries_evicted += entries;
            report.released_texture_estimate_bytes =
                report.released_texture_estimate_bytes.saturating_add(bytes);
        }
        self.ebb.trim.cursor = self.ebb.trim.cursor.wrapping_add(report.examined.max(1));
        if allowed && kind == PassKind::Deep && started.elapsed() < budget {
            macro_rules! shrink {
                ($map:expr) => {
                    if started.elapsed() < budget
                        && $map.capacity() > $map.len().saturating_mul(4).max(64)
                    {
                        $map.shrink_to($map.len().saturating_mul(2));
                        report.maps_shrunk += 1;
                    }
                };
            }
            shrink!(self.render.gpu.backdrop_textures);
            shrink!(self.render.gpu.layer_alpha_masks);
            shrink!(self.render.gpu.depth_schematics);
            shrink!(self.render.gpu.custom_shader_instances);
            shrink!(self.render.gpu.window_frame_snapshots);
            shrink!(self.render.windows.window_opacity);
            shrink!(self.render.windows.window_glass_modes);
            shrink!(self.render.windows.window_shader_assignments);
            shrink!(self.render.windows.layer_shader_assignments);
            shrink!(self.render.windows.glass_anim);
            shrink!(self.render.windows.window_depths);
            shrink!(self.physics.window_viscosity);
            shrink!(self.physics.window_sway);
            shrink!(self.physics.window_float_physics);
            shrink!(self.physics.window_float_bodies);
            shrink!(self.physics.window_float_ambient);
            shrink!(self.physics.window_currents);
            shrink!(self.physics.window_buoyancy);
            shrink!(self.ebb.trim.hidden_since);
        }
        // Disconnected-output solid buffers are not client or live lock data.
        // They normally disappear in the output owner's teardown already.
        if started.elapsed() < budget {
            let outputs: HashSet<_> = self.space.outputs().cloned().collect();
            let before = self.render.gpu.layer_dim_buffers.len() + self.lock_blank.len();
            self.render
                .gpu
                .layer_dim_buffers
                .retain(|output, _| outputs.contains(output));
            self.lock_blank.retain(|output, _| outputs.contains(output));
            report.entries_evicted +=
                before - self.render.gpu.layer_dim_buffers.len() - self.lock_blank.len();
        }
        report.duration_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        report.budget_exhausted |= started.elapsed() >= budget;
        self.ebb.trim.record(report, allowed);
    }

    pub(super) fn ebb_refresh_pressure(&mut self) {
        let mode = self.config.ebb.mode;
        if self.ebb.pressure.mode == Some(mode) {
            return;
        }
        if let Some(token) = self.ebb.pressure.registration.take() {
            self.loop_handle.remove(token);
        }
        self.ebb.pressure.mode = Some(mode);
        self.ebb.trim_blocked = false;
        if mode == EbbMode::Off {
            self.ebb.trim.requested = None;
            self.ebb.trim.cleanup_pending = false;
            self.ebb.trim.heap_pending = false;
        }
        self.ebb.pressure.source = None;
        self.ebb.trim.pressure_pending = false;
        if !matches!(mode, EbbMode::Balanced | EbbMode::Aggressive) {
            return;
        }
        let Ok(watch) = PressureWatch::open(mode) else {
            return;
        };
        let source = watch.source.clone();
        match self.loop_handle.insert_source(
            Generic::new(watch, Interest::READ, Mode::Level),
            |_, watch, state| {
                match watch.notified() {
                    Ok(true) => state.ebb.trim.pressure_pending = true,
                    Ok(false) => {}
                    Err(_) => {
                        state.ebb.pressure.registration = None;
                        state.ebb.pressure.source = None;
                        return Ok(PostAction::Remove);
                    }
                }
                Ok(PostAction::Continue)
            },
        ) {
            Ok(token) => {
                self.ebb.pressure.registration = Some(token);
                self.ebb.pressure.source = Some(source);
            }
            Err(error) => tracing::debug!(%error,"Ebb PSI watcher unavailable"),
        }
    }
}
