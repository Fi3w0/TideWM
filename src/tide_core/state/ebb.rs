//! Integrate Ebb at maintenance boundaries, never during a frame or grab callback.
use super::{SessionLock, Smallvil};
use crate::{
    ebb::{Leftover, Sample},
    ipc::{read_self_proc, tide_texture_estimate, IpcEvent},
};
use smithay::{reexports::wayland_server::Resource, utils::IsAlive};
use std::time::Instant;

impl Smallvil {
    pub(crate) fn ebb_tick(&mut self, screens_off: bool) {
        let now = self.start_time.elapsed().as_secs();
        let away = screens_off
            || !matches!(self.session_lock, SessionLock::Unlocked)
            || self.input.last_activity.elapsed().as_millis()
                >= u128::from(self.config.ebb.away_after_ms);
        self.ebb.note_away(now, away);
        let interval = u64::from(self.config.ebb.check_every_ms).div_ceil(1000);
        if self
            .ebb
            .last_sample_secs
            .is_some_and(|last| now.saturating_sub(last) < interval)
        {
            return;
        }
        // Never inspect a locked seat handle from a grab callback. Both
        // backend callers are independent maintenance callbacks.
        if self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
            || self.input.compositor_gesture.is_some()
        {
            return;
        }
        let animating =
            self.has_active_animation() || self.render.motion.depth_background.animating();
        if !self.ebb.due(now, interval, animating) {
            return;
        }
        let started = Instant::now();
        let pss_bytes = read_self_proc().map(|(pss, _, _)| pss.saturating_mul(1024));
        let texture_bytes = tide_texture_estimate(self).tide_texture_estimate_bytes;
        let fd_count = std::fs::read_dir("/proc/self/fd").ok().and_then(|mut fds| {
            fds.try_fold(0_u64, |count, entry| entry.map(|_| count + 1))
                .ok()
        });
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok();
        let system_available_bytes = meminfo
            .as_deref()
            .and_then(|text| crate::ebb::kb_field(text, "MemAvailable:"));
        let pressure_some_avg10 = std::fs::read_to_string("/proc/pressure/memory")
            .ok()
            .and_then(|text| crate::ebb::pressure_avg10(&text));
        let leftovers = self.ebb_leftovers();
        let cost_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        let sample = Sample {
            uptime_secs: now,
            pss_bytes,
            texture_bytes,
            fd_count,
            system_available_bytes,
            pressure_some_avg10,
            windows: self.foreign_toplevel_numeric_ids.len(),
            away,
            away_since_secs: self.ebb.away_since_secs,
            leftover_count: leftovers.iter().map(|entry| entry.count).sum(),
            leftovers,
            cost_us,
        };
        self.ebb
            .caps_allow_trim(&self.config.ebb, pss_bytes, texture_bytes);
        let warnings = self.ebb.record(
            sample,
            u64::from(self.config.ebb.leak_window_ms).div_ceil(1000),
        );
        for warning in warnings {
            tracing::warn!(code = warning.code, message = %warning.message, "Ebb finding");
            if self.config.ebb.toast
                && self.toast.is_none()
                && self.config_error_overlay.is_none()
                && matches!(self.session_lock, SessionLock::Unlocked)
                && !screens_off
            {
                self.toast = crate::toast::Toast::new(
                    &format!("Ebb: {}", warning.message),
                    crate::toast::ToastKind::Error,
                    crate::ui_theme::UiTheme::from_config(&self.config),
                    self.toast_output_width(),
                );
                self.request_redraw();
            }
            self.emit_ipc_event(IpcEvent::EbbWarning { warning });
        }
    }

    /// This does not evict caches. It only returns already-free allocator
    /// pages after Smithay's existing release queue has been swept.
    pub(crate) fn ebb_after_texture_cleanup(&mut self) {
        let config = self.config.ebb.clone();
        if config.mode == crate::config::EbbMode::Off
            || self.input.last_activity.elapsed().as_millis() < u128::from(config.light_after_ms)
            || self.has_active_animation()
            || self.seat.get_pointer().is_some_and(|p| p.is_grabbed())
        {
            return;
        }
        let now = self.start_time.elapsed().as_secs();
        let interval =
            u64::from(self.config.ebb.light_after_ms.max(config.check_every_ms)).div_ceil(1000);
        if self
            .ebb
            .last_trim_secs
            .is_some_and(|last| now.saturating_sub(last) < interval)
        {
            return;
        }
        let Some(sample) = self.ebb.recent.back() else {
            return;
        };
        if now.saturating_sub(sample.uptime_secs) > u64::from(config.check_every_ms).div_ceil(1000)
        {
            return;
        }
        let (pss, textures) = (sample.pss_bytes, sample.texture_bytes);
        if !self.ebb.caps_allow_trim(&config, pss, textures) {
            return;
        }
        if !cfg!(target_env = "gnu") {
            return;
        }
        self.ebb.last_trim_secs = Some(now);
        let started = Instant::now();
        let released = crate::ebb::trim_heap();
        self.ebb.last_trim_cost_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.ebb.trim_calls += 1;
        self.ebb.trim_released += u64::from(released);
        if !released
            && (config.ram_cap.is_some() || config.cache_cap.is_some() || config.vram_cap.is_some())
        {
            self.ebb.trim_blocked = true;
        }
    }

    fn ebb_leftovers(&self) -> Vec<Leftover> {
        let mut findings = Vec::new();
        let mut add = |owner, count| {
            if count > 0 {
                findings.push(Leftover { owner, count });
            }
        };
        macro_rules! surfaces {
            ($owner:literal, $map:expr) => {
                add($owner, $map.keys().filter(|s| !s.is_alive()).count());
            };
        }
        surfaces!(
            "render.gpu.backdrop_textures",
            self.render.gpu.backdrop_textures
        );
        surfaces!(
            "render.gpu.layer_alpha_masks",
            self.render.gpu.layer_alpha_masks
        );
        surfaces!(
            "render.gpu.custom_shader_instances",
            self.render.gpu.custom_shader_instances
        );
        surfaces!(
            "render.gpu.depth_schematics",
            self.render.gpu.depth_schematics
        );
        surfaces!(
            "render.gpu.window_frame_snapshots",
            self.render.gpu.window_frame_snapshots
        );
        surfaces!(
            "render.windows.window_opacity",
            self.render.windows.window_opacity
        );
        surfaces!(
            "render.windows.window_glass_modes",
            self.render.windows.window_glass_modes
        );
        surfaces!(
            "render.windows.window_shader_assignments",
            self.render.windows.window_shader_assignments
        );
        surfaces!(
            "render.windows.layer_shader_assignments",
            self.render.windows.layer_shader_assignments
        );
        surfaces!("foreign_toplevels", self.foreign_toplevels);
        surfaces!(
            "foreign_toplevel_numeric_ids",
            self.foreign_toplevel_numeric_ids
        );
        surfaces!("unmapped_toplevels", self.unmapped_toplevels);
        surfaces!("render.windows.glass_anim", self.render.windows.glass_anim);
        surfaces!(
            "render.windows.window_depths",
            self.render.windows.window_depths
        );
        surfaces!(
            "render.motion.window_open_animations",
            self.render.motion.window_open_animations
        );
        surfaces!(
            "render.motion.window_move_animations",
            self.render.motion.window_move_animations
        );
        surfaces!(
            "render.motion.cascade_window_animations",
            self.render.motion.cascade_window_animations
        );
        surfaces!("physics.window_viscosity", self.physics.window_viscosity);
        surfaces!("physics.window_sway", self.physics.window_sway);
        surfaces!(
            "physics.window_float_physics",
            self.physics.window_float_physics
        );
        surfaces!(
            "physics.window_float_ambient",
            self.physics.window_float_ambient
        );
        surfaces!(
            "physics.window_float_bodies",
            self.physics.window_float_bodies
        );
        surfaces!("physics.window_currents", self.physics.window_currents);
        surfaces!("physics.window_buoyancy", self.physics.window_buoyancy);
        surfaces!("fullscreen", self.fullscreen);
        surfaces!("maximized", self.maximized);
        surfaces!("lifecycle_flutter", self.lifecycle_flutter);
        surfaces!("expected_tiled_size", self.expected_tiled_size);
        surfaces!("tiled_size_refusals", self.tiled_size_refusals);
        surfaces!("initial_toplevel_identity", self.initial_toplevel_identity);
        surfaces!("urgent_pulse_last", self.urgent_pulse_last);
        surfaces!("window_tags", self.window_tags);
        add(
            "render.gpu.custom_shader_bypassed",
            self.render
                .gpu
                .custom_shader_bypassed
                .iter()
                .filter(|s| !s.is_alive())
                .count(),
        );
        add(
            "workspaces.pinned",
            self.spatial
                .workspaces
                .pinned
                .iter()
                .filter(|s| !s.is_alive())
                .count(),
        );
        add(
            "workspaces.pseudo_tiled",
            self.spatial
                .workspaces
                .pseudo_tiled
                .iter()
                .filter(|s| !s.is_alive())
                .count(),
        );
        let outputs: std::collections::HashSet<_> =
            self.space.outputs().map(|output| output.name()).collect();
        macro_rules! outputs {
            ($owner:literal, $map:expr) => {
                add(
                    $owner,
                    $map.keys().filter(|name| !outputs.contains(*name)).count(),
                );
            };
        }
        outputs!("render.motion.caustics", self.render.motion.caustics);
        outputs!("render.motion.compasses", self.render.motion.compasses);
        outputs!(
            "render.motion.ocean_canvases",
            self.render.motion.ocean_canvases
        );
        outputs!(
            "render.motion.pending_workspace_transitions",
            self.render.motion.pending_workspace_transitions
        );
        outputs!(
            "render.motion.workspace_transitions",
            self.render.motion.workspace_transitions
        );
        outputs!(
            "render.motion.workspace_glides",
            self.render.motion.workspace_glides
        );
        add(
            "render.gpu.layer_dim_buffers",
            self.render
                .gpu
                .layer_dim_buffers
                .keys()
                .filter(|output| !outputs.contains(&output.name()))
                .count(),
        );
        add(
            "lock_blank",
            self.lock_blank
                .keys()
                .filter(|output| !outputs.contains(&output.name()))
                .count(),
        );
        add(
            "workspaces.floating_workspace",
            self.spatial
                .workspaces
                .floating_workspace
                .values()
                .filter(|tag| !tag.window.alive())
                .count(),
        );
        add(
            "swallowed",
            self.swallowed
                .values()
                .filter(|entry| !entry.window.alive())
                .count(),
        );
        add(
            "groups",
            self.groups
                .iter()
                .flat_map(|group| &group.members)
                .filter(|member| {
                    !member.surface.is_alive()
                        || member.parked_window.as_ref().is_some_and(|w| !w.alive())
                })
                .count(),
        );
        add(
            "workspaces.layout",
            self.spatial.workspaces.layout.dead_window_references(),
        );
        add("ocean", self.spatial.ocean.dead_window_references());
        add(
            "workspaces.classic_depth",
            self.spatial
                .workspaces
                .classic_depth
                .dead_window_references(),
        );
        // Detached closing snapshots deliberately retain textures, not Window
        // handles. Their existing lifetime is not diagnosed as a leftover.
        findings
    }
}
