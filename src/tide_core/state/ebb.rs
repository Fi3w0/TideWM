//! Integrate Ebb at maintenance boundaries, never during a frame or grab callback.
use super::{SessionLock, Smallvil};
use crate::{
    ebb::{Leftover, Sample},
    ipc::{read_self_proc, tide_texture_estimate, IpcEvent},
};
use smithay::{reexports::wayland_server::Resource, utils::IsAlive};
use std::time::Instant;

/// Bounds the per-app report whatever the client count or their strings.
const APP_REPORT_LIMIT: usize = 32;
const APP_NAME_LIMIT: usize = 96;

/// Pixels of every buffer attached in `surface`'s tree, at 4 bytes each.
fn attached_buffer_bytes(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> u64 {
    use smithay::{
        backend::renderer::utils::with_renderer_surface_state,
        wayland::compositor::{with_surface_tree_downward, TraversalAction},
    };
    let mut pixels = 0_u64;
    let mut surfaces = Vec::new();
    with_surface_tree_downward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |child, _, _| surfaces.push(child.clone()),
        |_, _, _| true,
    );
    for child in surfaces {
        if let Some(size) =
            with_renderer_surface_state(&child, |state| state.buffer_size()).flatten()
        {
            pixels = pixels.saturating_add(size.w.max(0) as u64 * size.h.max(0) as u64);
        }
    }
    pixels.saturating_mul(4)
}

impl Smallvil {
    pub(crate) fn ebb_tick(&mut self, screens_off: bool) {
        self.ebb_refresh_pressure();
        let now = self.start_time.elapsed().as_secs();
        let away = screens_off
            || !matches!(self.session_lock, SessionLock::Unlocked)
            || self.input.last_activity.elapsed().as_millis()
                >= u128::from(self.config.ebb.away_after_ms);
        self.ebb.note_away(now, away);
        if self.ebb.watch.pressure_event_due(now) {
            self.emit_ebb_pressure();
        }
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
        if leftovers
            .iter()
            .any(|entry| entry.owner.starts_with("render.gpu") || entry.owner == "lock_blank")
        {
            self.ebb.trim.audit_pending = true;
        }
        self.ebb_observe_hidden(screens_off);
        // Without a kernel PSI watch, low available memory is the pressure
        // signal. Every mode tells listeners; only the reacting modes trim.
        if self.ebb.pressure.source.is_none() {
            let total = meminfo
                .as_deref()
                .and_then(|text| crate::ebb::kb_field(text, "MemTotal:"));
            let below = |percent: u128| {
                total
                    .zip(system_available_bytes)
                    .is_some_and(|(total, available)| {
                        total > 0 && u128::from(available) * 100 < u128::from(total) * percent
                    })
            };
            if below(crate::ebb::watch::PRESSURE_AVAILABLE_PERCENT) {
                self.ebb.watch.pressure_signalled = true;
            }
            let threshold = match self.config.ebb.mode {
                crate::config::EbbMode::Aggressive => Some(15),
                crate::config::EbbMode::Balanced => Some(10),
                _ => None,
            };
            if threshold.is_some_and(below) {
                self.ebb.trim.pressure_pending = true;
            }
        }
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
            activity: Some(self.ebb.watch.take(now)),
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

    /// Tells listeners the system is short of memory so they can drop their
    /// own caches. Read at emit time only; nothing is sampled for it.
    fn emit_ebb_pressure(&mut self) {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok();
        let available = meminfo
            .as_deref()
            .and_then(|text| crate::ebb::kb_field(text, "MemAvailable:"));
        let some_avg10 = std::fs::read_to_string("/proc/pressure/memory")
            .ok()
            .and_then(|text| crate::ebb::pressure_avg10(&text));
        let source = self
            .ebb
            .pressure
            .source
            .clone()
            .unwrap_or_else(|| "/proc/meminfo".to_string());
        tracing::info!(%source, ?available, ?some_avg10, "Ebb: memory pressure");
        self.emit_ipc_event(IpcEvent::EbbPressure {
            source,
            system_available_bytes: available,
            pressure_some_avg10: some_avg10,
        });
    }

    /// Per app: what TideWM holds for its surfaces and how large its attached
    /// buffers are. Built on request from live state; nothing is stored.
    pub(crate) fn ebb_apps(&self) -> serde_json::Value {
        use smithay::desktop::layer_map_for_output;
        #[derive(Default)]
        struct App {
            windows: usize,
            layers: usize,
            held_entries: usize,
            held_bytes: u64,
            buffer_bytes: u64,
        }
        let gpu = &self.render.gpu;
        let mut apps: std::collections::HashMap<(Option<i32>, String), App> = Default::default();
        let mut add =
            |surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
             name: String,
             layer: bool| {
                let app = apps.entry((self.client_pid(surface), name)).or_default();
                if layer {
                    app.layers += 1;
                } else {
                    app.windows += 1;
                }
                for bytes in [
                    gpu.backdrop_textures
                        .get(surface)
                        .map(|cache| cache.estimated_texture_bytes()),
                    gpu.layer_alpha_masks
                        .get(surface)
                        .map(|cache| cache.estimated_texture_bytes()),
                    gpu.custom_shader_instances
                        .get(surface)
                        .map(|instance| instance.target_bytes())
                        .filter(|bytes| *bytes > 0),
                    gpu.depth_schematics.contains_key(surface).then_some(0),
                ]
                .into_iter()
                .flatten()
                {
                    app.held_entries += 1;
                    app.held_bytes = app.held_bytes.saturating_add(bytes);
                }
                app.buffer_bytes = app
                    .buffer_bytes
                    .saturating_add(attached_buffer_bytes(surface));
            };
        for surface in self.foreign_toplevel_numeric_ids.keys() {
            if self.mapped_toplevel_window(surface).is_some() {
                let name = self.toplevel_identity(surface).0.unwrap_or_default();
                add(surface, name, false);
            }
        }
        for output in self.space.outputs() {
            for layer in layer_map_for_output(output).layers() {
                add(layer.wl_surface(), layer.namespace().to_string(), true);
            }
        }
        let mut entries: Vec<_> = apps.into_iter().collect();
        entries.sort_by(|a, b| {
            let weight = |app: &App| app.held_bytes.saturating_add(app.buffer_bytes);
            weight(&b.1).cmp(&weight(&a.1)).then_with(|| a.0.cmp(&b.0))
        });
        let omitted = entries.len().saturating_sub(APP_REPORT_LIMIT);
        entries.truncate(APP_REPORT_LIMIT);
        let entries: Vec<_> = entries
            .into_iter()
            .map(|((pid, name), app)| {
                serde_json::json!({
                    "app_id": name.chars().take(APP_NAME_LIMIT).collect::<String>(),
                    "pid": pid,
                    "windows": app.windows,
                    "layers": app.layers,
                    "held_entries": app.held_entries,
                    "held_texture_estimate_bytes": app.held_bytes,
                    "client_buffer_estimate_bytes": app.buffer_bytes,
                })
            })
            .collect();
        serde_json::json!({
            "entries": entries,
            "omitted": omitted,
            "held_scope": "TideWM-owned caches kept for this app's surfaces (backdrops, layer masks, shader stages, depth schematics)",
            "client_buffer_scope": "estimate of the buffers currently attached to this app's mapped surfaces at 4 bytes per pixel; the client owns them, and its buffer pools and off-screen surfaces are not counted",
        })
    }

    /// Called by backends after draining Smithay's existing texture release queue.
    pub(crate) fn ebb_after_texture_cleanup(&mut self) {
        if self.ebb_trim_busy() {
            return;
        }
        if !self.ebb.trim.cleanup_pending {
            return;
        }
        self.ebb.trim.cleanup_pending = false;
        if !std::mem::take(&mut self.ebb.trim.heap_pending) {
            return;
        }
        if self.config.ebb.mode == crate::config::EbbMode::Off {
            return;
        }
        let now = self.start_time.elapsed().as_secs();
        self.ebb.last_trim_secs = Some(now);
        let started = Instant::now();
        let released = cfg!(target_env = "gnu") && crate::ebb::trim_heap();
        self.ebb.last_trim_cost_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        if cfg!(target_env = "gnu") {
            self.ebb.trim_calls += 1;
            self.ebb.trim_released += u64::from(released);
        }
        let exhausted = self.ebb.trim.last_pass.as_ref().is_some_and(|pass| {
            pass.entries_evicted == 0 && pass.maps_shrunk == 0 && !pass.budget_exhausted
        });
        if exhausted
            && !released
            && (self.config.ebb.ram_cap.is_some()
                || self.config.ebb.cache_cap.is_some()
                || self.config.ebb.vram_cap.is_some())
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
