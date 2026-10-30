use std::time::{Duration, Instant};

use smithay::{
    output::Output,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_server::protocol::wl_surface::WlSurface,
    },
};

use crate::{
    config::Config,
    toast::{Toast, ToastKind},
};

use super::{tide_workspace_value, Smallvil};

impl Smallvil {
    pub(super) fn has_lua_handler(&self, name: &str) -> bool {
        self.config_lua
            .globals()
            .get::<mlua::Table>("_handlers")
            .and_then(|handlers| handlers.get::<mlua::Value>(name))
            .is_ok_and(|value| matches!(value, mlua::Value::Table(_)))
    }

    /// Records one relevant filesystem event and ensures a single trailing
    /// reload runs after the event stream has been quiet for 150ms. Editors
    /// that truncate then rewrite a file can otherwise make the first event
    /// load a partial file and have the completed write discarded by a
    /// leading-edge debounce.
    pub fn note_config_event(&mut self) {
        const QUIET: Duration = Duration::from_millis(150);
        self.last_config_event = Instant::now();
        if self.config_reload_timer_armed {
            return;
        }
        self.config_reload_timer_armed = true;
        let result = self.loop_handle.insert_source(
            Timer::from_duration(QUIET),
            move |_, _, state: &mut Smallvil| {
                let elapsed = state.last_config_event.elapsed();
                if elapsed >= QUIET {
                    state.config_reload_timer_armed = false;
                    state.reload_config();
                    TimeoutAction::Drop
                } else {
                    TimeoutAction::ToDuration(QUIET - elapsed)
                }
            },
        );
        if let Err(err) = result {
            self.config_reload_timer_armed = false;
            tracing::warn!(%err, "Failed to register config debounce timer; reloading immediately");
            self.reload_config();
        }
    }

    /// Re-anchor watches after a filesystem event may have created a
    /// previously missing include directory, before the debounced parse.
    pub(crate) fn refresh_config_watch_anchors(&mut self) {
        if let Some(watcher) = &mut self.config_watcher {
            watcher.refresh_anchors();
        }
    }

    fn sync_config_watch_paths(&mut self) {
        if let Some(watcher) = &mut self.config_watcher {
            watcher.update_paths(&self.config.source_paths);
        }
    }

    /// Runs `on "event"` handlers registered in the session Lua (the
    /// config's `_handlers` table), then drains the actions they queued
    /// through `spawn(...)`/`action(...)`. The tide table is refreshed
    /// first so handlers see the live workspace/outputs. Guarded by a
    /// nesting cap: a handler whose queued action emits another event
    /// cannot recurse unboundedly.
    pub(crate) fn run_lua_handlers(&mut self, event: &crate::ipc::IpcEvent) {
        // Most configs have no handler for most events; skip the `tide`
        // table sync entirely unless one exists.
        let registered = self
            .config_lua
            .globals()
            .get::<mlua::Table>("_handlers")
            .and_then(|handlers| handlers.get::<mlua::Value>(event.event_name()))
            .is_ok_and(|value| matches!(value, mlua::Value::Table(_)));
        if !registered {
            return;
        }
        if self.handler_depth >= 16 {
            tracing::warn!("Wave event handler nesting cap reached, skipping");
            return;
        }
        self.handler_depth += 1;
        self.sync_tide();
        let name = event.event_name();
        let run_result = (|| -> Result<(), mlua::Error> {
            let handlers: mlua::Table = self.config_lua.globals().get("_handlers")?;
            let mlua::Value::Table(for_event) = handlers.get::<mlua::Value>(name)? else {
                return Ok(());
            };
            for pair in for_event.sequence_values::<mlua::Function>() {
                let f = pair?;
                crate::wave::set_handler_active(&self.config_lua, true)?;
                let result = crate::wave::with_execution_budget(
                    &self.config_lua,
                    crate::wave::ExecutionBudget::Handler,
                    || f.call::<()>(()),
                );
                crate::wave::set_handler_active(&self.config_lua, false)?;
                if let Err(e) = result {
                    tracing::warn!(event = name, error = %e, "Wave event handler failed");
                }
            }
            Ok(())
        })();
        if let Err(e) = run_result {
            tracing::warn!(event = name, error = %e, "Wave event handlers could not run");
        }
        let queued: Vec<String> = self
            .config_lua
            .globals()
            .get::<mlua::Table>("_actions")
            .ok()
            .and_then(|actions| {
                actions
                    .sequence_values::<String>()
                    .take(crate::wave::MAX_QUEUED_ACTIONS + 1)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()
            })
            .unwrap_or_default();
        let queue_overflowed = queued.len() > crate::wave::MAX_QUEUED_ACTIONS;
        let queued = queued
            .into_iter()
            .take(crate::wave::MAX_QUEUED_ACTIONS)
            .collect::<Vec<_>>();
        if let Ok(actions) = self.config_lua.globals().get::<mlua::Table>("_actions") {
            let _ = actions.clear();
        }
        if queue_overflowed {
            tracing::warn!(
                limit = crate::wave::MAX_QUEUED_ACTIONS,
                "Wave handler action queue exceeded its limit; dropping excess actions"
            );
        }
        for action in queued {
            match crate::config::parse_action(&action) {
                Some(parsed) => self.run_action(parsed),
                None => tracing::warn!(action, "Wave handler queued an unknown action"),
            }
        }
        self.handler_depth -= 1;
    }

    /// Refreshes the `tide` table's live facts (outputs and, for Classic,
    /// the first output's active workspace) from the current scene, then
    /// rebuilds the Lua `tide` table so `tidectl eval` sees the live session.
    /// Ocean and a zero-output session publish the documented `0` sentinel;
    /// neither has a meaningful numbered active workspace.
    pub(crate) fn sync_tide(&mut self) {
        self.tide.backend = self.backend_name;
        self.tide.outputs = self
            .space
            .outputs()
            .filter_map(|output| {
                let mode = output.current_mode()?;
                Some((
                    output.name().to_string(),
                    mode.size.w as u32,
                    mode.size.h as u32,
                ))
            })
            .collect();
        let first_workspace = self.space.outputs().next().map(|output| {
            let name = output.name().to_string();
            self.spatial.workspaces.layout.active_workspace(&name)
        });
        self.tide.workspace = tide_workspace_value(self.spatial.engine, first_workspace);
        if let Ok(tide_table) = crate::wave::build_tide_table(&self.config_lua, &self.tide) {
            let _ = self.config_lua.globals().set("tide", tide_table);
        }
    }

    /// Evaluates a Wave expression on the session Lua (the `tide` table
    /// is refreshed first), returning the value as JSON for `tidectl
    /// eval`. Config globals and section tables are visible, so
    /// `theme.primary` and `mod` both answer.
    pub(crate) fn eval_expression(
        &mut self,
        expression: &str,
    ) -> Result<serde_json::Value, String> {
        self.sync_tide();
        let lua_expr = crate::wave::compile_eval_expression(expression)?;
        let chunk = self.config_lua.load(&lua_expr).set_name("eval");
        let value: mlua::Value = crate::wave::with_execution_budget(
            &self.config_lua,
            crate::wave::ExecutionBudget::Eval,
            || chunk.eval(),
        )
        .map_err(|e| e.to_string())?;
        crate::wave::lua_value_to_json(value)
    }

    /// Re-reads the config file and applies what can be applied live
    /// (keybinds, input repeat rate). Shows a toast either way so a reload
    /// is never silent, success or failure.
    /// Re-applies `output <name> { }` blocks to connected outputs after a
    /// reload: transform, scale and position go through the same validated
    /// transaction as wlr-output-management. Outputs without a block keep
    /// their current state. A different `mode` or `enabled = false` needs a
    /// DRM modeset, which only happens at output setup, so those produce a
    /// notice instead of being ignored silently.
    fn apply_output_blocks_live(&mut self) -> Vec<String> {
        use crate::handlers::wlr_output_management::{apply_output_changes, OutputChange};
        let mut notices = Vec::new();
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let changes: Vec<(Output, OutputChange)> = outputs
            .into_iter()
            .map(|output| {
                let name = output.name();
                let change = match self.config.outputs.iter().find(|cfg| cfg.name == name) {
                    None => OutputChange::default(),
                    Some(cfg) => {
                        if !cfg.enabled {
                            notices.push(format!(
                                "output {name}: enabled = false applies after restarting TideWM"
                            ));
                        }
                        if let (Some(requested), Some(current)) =
                            (cfg.mode.as_deref(), output.current_mode())
                        {
                            let differs = match crate::config::parse_mode_str(requested) {
                                Some((w, h, refresh)) => {
                                    w != current.size.w
                                        || h != current.size.h
                                        || refresh.is_some_and(|r| {
                                            (f64::from(current.refresh) / 1000.0 - r).abs() >= 0.5
                                        })
                                }
                                None => false,
                            };
                            if differs {
                                notices.push(format!(
                                    "output {name}: mode {requested} applies after restarting TideWM"
                                ));
                            }
                        }
                        OutputChange {
                            position: cfg.position.map(Into::into),
                            transform: Some(cfg.transform.to_transform()),
                            scale: Some(cfg.scale),
                            custom_mode: None,
                        }
                    }
                };
                (output, change)
            })
            .collect();
        if !apply_output_changes(self, &changes) {
            notices.push(
                "output blocks: the new layout is invalid (overlap or out of range); kept the current one"
                    .to_string(),
            );
        }
        notices
    }

    pub fn reload_config(&mut self) {
        self.sync_tide();
        match Config::reload_staged(&self.tide) {
            Ok((staged_lua, mut new_config, mut warnings)) => {
                let previous_input_device_rules = self.config.input.devices.clone();
                // The staged runtime is complete and valid. Swap it in as
                // one transaction so a failed parse/evaluation can never
                // clear or partially replace the handlers and globals from
                // the configuration that remains active.
                self.config_lua = staged_lua;
                let had_error_overlay = self.config_error_overlay.take().is_some();
                let diff = crate::config::diff_entries(
                    &self.config.loaded_entries,
                    &new_config.loaded_entries,
                );
                let mut migrated_engine = false;
                // The file's engine is a request; `self.spatial.engine` is
                // what runs. Whenever they differ, translate every live
                // window to the requested engine in place (S6). A refused
                // migration keeps the running engine and leaves the request
                // in `config`, so the next reload tries again. The notice
                // goes to a toast, not the persistent warning panel -- this
                // is the user's intent, not a config problem.
                if new_config.spatial_engine != self.spatial.engine {
                    migrated_engine = self.migrate_spatial_engine(new_config.spatial_engine);
                    if !migrated_engine {
                        warnings.push(
                            "Spatial-engine migration needs a live output; keeping the current engine"
                                .to_string(),
                        );
                    }
                }
                if new_config.ocean.reefs != self.config.ocean.reefs
                    || new_config.ocean.bookmarks != self.config.ocean.bookmarks
                {
                    warnings.push(
                        "Ocean reefs/bookmarks are startup-owned; restart TideWM to reshape the world"
                            .to_string(),
                    );
                    new_config.ocean.reefs = self.config.ocean.reefs.clone();
                    new_config.ocean.bookmarks = self.config.ocean.bookmarks.clone();
                }
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.change_repeat_info(
                        new_config.input.repeat_rate,
                        new_config.input.repeat_delay,
                    );
                    // Same bad-config-must-not-break-things guarantee as
                    // the startup path in `Smallvil::new`: on failure this
                    // just logs and leaves the previously-loaded keymap in
                    // place, it doesn't touch `inner.keyboard` at all.
                    if let Err(e) = keyboard.set_xkb_config(self, new_config.input.xkb_config()) {
                        tracing::error!(
                            error = ?e,
                            layout = %new_config.input.xkb_layout,
                            "Reloaded XKB keymap failed to compile, keeping the previous one"
                        );
                    } else {
                        self.configured_keymap_replaced();
                    }
                }
                // Rebuilt on every reload: the card shows live binds and the
                // config path, either of which the reload may have changed.
                self.welcome_hint = new_config
                    .show_welcome_hint
                    .then(|| crate::welcome::WelcomeHint::build(&new_config));
                let outputs_changed =
                    format!("{:?}", self.config.outputs) != format!("{:?}", new_config.outputs);
                // `env { }` is live: children spawned from now on get the new
                // values, the session manager is told, and TideWM's own
                // cursor follows XCURSOR_THEME/XCURSOR_SIZE immediately.
                let env_change = crate::session_env::update(&new_config.env);
                if self.backend_name == "udev"
                    && (env_change.touches("XCURSOR_THEME") || env_change.touches("XCURSOR_SIZE"))
                {
                    self.input.cursor_theme = crate::cursor::Theme::load();
                    if self.input.cursor_theme.is_none() {
                        warnings.push(
                            "Cursor theme from XCURSOR_THEME has no default cursor; using the fallback dot"
                                .to_string(),
                        );
                    }
                    self.request_redraw();
                }
                if self.backend_name == "udev" {
                    crate::export_env_change(env_change);
                }
                self.config = new_config;
                // A changed `xkb_layout` gives a new layout list, not just a
                // new group; subscribers need to hear about it.
                self.notify_keyboard_layout_changed();
                self.sync_config_watch_paths();
                if outputs_changed {
                    warnings.extend(self.apply_output_blocks_live());
                }
                warnings.extend(
                    self.render
                        .gpu
                        .custom_shader_programs
                        .retain_definitions(&self.config.shader_definitions),
                );
                self.input.rescue_keybinds_active = false;
                self.input.helper_keys_down.clear();
                // A live `workspace_count` change should show up in the
                // ext-workspace-v1/IPC workspace listing immediately, same
                // as everything else hot-reloadable. Cheap no-op diff when
                // nothing actually changed.
                self.refresh_ext_workspaces();
                if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
                    if self.config.ocean.zoom_enabled {
                        self.spatial
                            .ocean
                            .clamp_zooms(self.config.ocean.min_zoom, self.config.ocean.max_zoom);
                    } else {
                        self.spatial.ocean.clamp_zooms(1.0, 1.0);
                    }
                }
                if !self.config.classic_depth.enabled {
                    self.restore_all_depth_deck_windows();
                }
                if !self.config.classic_depth.enabled || !self.config.classic_depth.animation {
                    self.render.motion.depth_transitions.clear();
                }
                if !self.config.animations.enabled || !self.config.animations.open.enabled {
                    self.render.motion.window_open_animations.clear();
                }
                if !self.config.water_effects
                    || self.config.cascade.pour == crate::config::CascadeLiquidPreset::None
                {
                    self.render.motion.cascade_window_animations.clear();
                }
                if !self.config.animations.enabled || !self.config.animations.movement.enabled {
                    self.render.motion.window_move_animations.clear();
                }
                // W3: the window-affecting battery below re-derives
                // per-window state from the config. On a reload where
                // nothing actually changed (an editor touched the file,
                // a save rewrote it byte-identically) the result would be
                // identical, so skip the whole battery instead of clearing
                // glass backdrop textures and re-resolving every window
                // for nothing.
                if !diff.is_empty() {
                    let disabled_viscosity: Vec<WlSurface> = self
                        .physics
                        .window_viscosity
                        .keys()
                        .filter(|surface| self.interactive_motion_half_life(surface).is_zero())
                        .cloned()
                        .collect();
                    for surface in disabled_viscosity {
                        self.physics.window_viscosity.remove(&surface);
                    }
                    let disabled_sway: Vec<WlSurface> = self
                        .physics
                        .window_sway
                        .keys()
                        .filter(|surface| !self.sway_enabled_for_surface(surface))
                        .cloned()
                        .collect();
                    for surface in disabled_sway {
                        self.physics.window_sway.remove(&surface);
                    }
                    let disabled_float_physics: Vec<WlSurface> = self
                        .physics
                        .window_float_physics
                        .keys()
                        .filter(|surface| {
                            self.float_physics_tier_for_surface(surface)
                                != crate::config::FloatPhysicsTier::Light
                        })
                        .cloned()
                        .collect();
                    for surface in disabled_float_physics {
                        self.physics.window_float_physics.remove(&surface);
                    }
                    let disabled_float_ambient: Vec<WlSurface> = self
                        .physics
                        .window_float_ambient
                        .keys()
                        .filter(|surface| !self.float_physics_enabled_for_surface(surface))
                        .cloned()
                        .collect();
                    for surface in disabled_float_ambient {
                        self.physics.window_float_ambient.remove(&surface);
                    }
                    self.sync_float_physics_bodies();
                    if !self.currents_enabled() {
                        self.physics.window_currents.clear();
                    }
                    self.physics.buoyancy_dirty = true;
                    let ordinary_close =
                        self.config.animations.enabled && self.config.animations.close.enabled;
                    let cascade_close = self.config.water_effects
                        && self.config.cascade.drain != crate::config::CascadeLiquidPreset::None;
                    if !ordinary_close {
                        for closing in &mut self.render.motion.closing_window_animations {
                            closing.animation = None;
                        }
                    }
                    if !cascade_close {
                        for closing in &mut self.render.motion.closing_window_animations {
                            closing.cascade = None;
                        }
                    }
                    self.render
                        .motion
                        .closing_window_animations
                        .retain(|closing| !closing.finished());
                    if !ordinary_close && !cascade_close {
                        self.render.gpu.window_frame_snapshots.clear();
                    }
                    self.render.gpu.depth_schematics.clear();
                    self.render.motion.depth_background.reset();
                    for depth in self.render.windows.window_depths.values_mut() {
                        depth.artwork_id = smithay::backend::renderer::element::Id::new();
                    }
                    self.render.windows.depth_last_tick =
                        Instant::now() - Duration::from_millis(100);
                    self.update_window_depths();
                    let any_workspace_animation = (self.config.water_effects
                        && self.config.workspace_transition.enabled)
                        || (self.config.animations.enabled
                            && self.config.animations.workspace.enabled);
                    if !self.config.water_effects || !self.config.workspace_transition.enabled {
                        self.render.motion.workspace_transitions.clear();
                    }
                    if !self.config.animations.enabled || !self.config.animations.workspace.enabled
                    {
                        self.render.motion.workspace_glides.clear();
                    }
                    if !any_workspace_animation {
                        let pending =
                            std::mem::take(&mut self.render.motion.pending_workspace_transitions);
                        for (output_name, workspace) in pending {
                            if let Some(output) = self.output_by_name(&output_name) {
                                self.switch_workspace_immediate(&output, workspace);
                            }
                        }
                    }
                    if !self.swim_enabled() {
                        for camera in self.render.motion.swim_cameras.values_mut() {
                            camera.snap_to_rest();
                        }
                    }
                    if self.spatial.engine.projection()
                        != Some(crate::config::WorkspaceProjection::Scroll)
                        || !self.config.scroll.workspace_scroll
                        || self.config.scroll.workspaces != crate::scroll::WorkspaceMode::Separate
                    {
                        self.render.motion.workspace_scroll_cameras.clear();
                    }
                    self.render.windows.window_opacity.clear();
                    self.render.windows.window_glass_modes.clear();
                    self.render.windows.window_shader_assignments.clear();
                    self.render.windows.layer_shader_assignments.clear();
                    let surfaces: Vec<WlSurface> = self.foreign_toplevels.keys().cloned().collect();
                    for surface in surfaces {
                        self.refresh_window_opacity_and_glass_for(&surface);
                    }
                    let unshaded: Vec<WlSurface> = self
                        .render
                        .gpu
                        .custom_shader_instances
                        .keys()
                        .filter(|surface| self.resolved_shader_assignment(surface).is_none())
                        .cloned()
                        .collect();
                    for surface in unshaded {
                        self.render.gpu.custom_shader_instances.remove(&surface);
                    }
                    self.render.gpu.custom_shader_bypassed.clear();
                    // The mode or frost tuning may have changed. Force the
                    // shared pre-frame pipeline to rebuild against the current
                    // window geometry instead of briefly showing stale content.
                    self.render.gpu.backdrop_textures.clear();
                    self.render.gpu.layer_alpha_masks.clear();
                }
                // A reload that dropped or renamed the currently-active
                // mode would otherwise leave every key silently
                // unmatched (falling through as plain input) with no
                // indication why -- exit back to the base keybinds
                // instead, the same table a name that never existed at
                // all would resolve to.
                if let Some(name) = &self.input.active_mode {
                    if !self.config.modes.contains_key(name) {
                        self.input.active_mode = None;
                    }
                }
                self.spatial
                    .workspaces
                    .layout
                    .set_default_algorithm(self.config.default_layout);
                self.spatial
                    .workspaces
                    .layout
                    .set_master_orientation(self.config.master_orientation);
                self.spatial
                    .workspaces
                    .layout
                    .set_split_bias(self.config.bsp_split_bias);
                self.spatial
                    .workspaces
                    .layout
                    .set_workspace_algorithm_overrides(self.config.workspace_algorithm_overrides());
                // DeviceAdded is the only other configuration point for
                // libinput hardware. Re-apply every live handle so touchpad,
                // mapping, and calibration edits take effect immediately;
                // removing a custom matrix restores libinput's own default.
                self.reapply_input_device_config(&previous_input_device_rules);
                tracing::info!(
                    changed = %diff.summary(),
                    reload_toast = self.config.show_config_reload_toast,
                    "Config reloaded"
                );
                let ui_theme = crate::ui_theme::UiTheme::from_config(&self.config);
                let toast_output_width = self.toast_output_width();
                // Unlike a hard parse failure, these diagnostics don't mean
                // the reload was rejected -- `new_config` above is already
                // in effect. Still worth a persistent nudge instead of a
                // toast that scrolls away, since a footgun lint is exactly
                // the kind of thing you want to notice before it bites, not
                // after -- so warnings suppress the reload toast the same
                // way a hard failure already does, instead of stacking a
                // duplicate timed toast over the persistent panel's corner.
                self.config_warnings = warnings.clone();
                // A successful engine migration is an expected, user-intended
                // operation -- informational, not a config problem. It shows
                // a toast like the ordinary reload does (with its own
                // message) instead of the persistent warning panel, which is
                // reserved for footguns and would otherwise sit on screen
                // until a further clean reload.
                let migration_toast = if migrated_engine {
                    let message = format!(
                        "Migrated live windows to the {} engine",
                        match self.spatial.engine {
                            crate::config::SpatialEngine::Classic => "classic",
                            crate::config::SpatialEngine::Scroll => "scroll",
                            crate::config::SpatialEngine::Ocean => "ocean",
                        }
                    );
                    Toast::new(&message, ToastKind::Info, ui_theme, toast_output_width)
                } else {
                    (self.config.show_config_reload_toast && warnings.is_empty())
                        .then(|| {
                            Toast::new(
                                "Configuration reloaded",
                                ToastKind::Info,
                                ui_theme,
                                toast_output_width,
                            )
                        })
                        .flatten()
                };
                self.toast = migration_toast;
                if !warnings.is_empty() {
                    self.config_error_overlay =
                        Some(crate::error_overlay::ConfigErrorOverlay::new(
                            warnings.join("; "),
                            crate::error_overlay::OverlaySeverity::Warning,
                            ui_theme,
                        ));
                }
                self.retile();
                if let Some(focus) = self.input.window_focus.clone() {
                    self.reveal_scroll_window_unless_panned(&focus);
                }
                if !diff.is_empty() || had_error_overlay || !warnings.is_empty() {
                    self.request_redraw();
                }
            }
            Err(err) => {
                tracing::warn!(%err, "Failed to reload config, keeping the previous one");
                // "Config error {err}" (no colon) rather than "Config
                // error: {err}" -- err already starts with "in file ... at
                // line N: ...", matching Hyprland's own on-screen config
                // error phrasing ("config error in file <path> at line
                // <N>: <message>") rather than inventing our own shape.
                let message = format!("Config error {err}");
                let ui_theme = crate::ui_theme::UiTheme::from_config(&self.config);
                self.config_error_overlay = Some(crate::error_overlay::ConfigErrorOverlay::new(
                    &message,
                    crate::error_overlay::OverlaySeverity::Error,
                    ui_theme,
                ));
                // The persistent panel is itself immediate feedback. A
                // second timed failure toast would duplicate the message and
                // overlap the panel's top-right edge.
                self.toast = None;
                self.retile();
                self.request_redraw();
            }
        }
        // Emit on both success and failure: a bar may want to refresh
        // config-derived state either way, and a reload that failed
        // still left the previously-applied config in place.
        self.emit_ipc_event(crate::ipc::IpcEvent::ConfigReloaded);
    }
}
