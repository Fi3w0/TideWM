use std::collections::HashMap;

use smithay::{
    backend::renderer::{
        element::{
            memory::MemoryRenderBufferRenderElement, solid::SolidColorRenderElement,
            surface::WaylandSurfaceRenderElement, AsRenderElements, Kind,
        },
        gles::{GlesPixelProgram, GlesRenderer, GlesTexture},
    },
    desktop::layer_map_for_output,
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Physical, Point, Rectangle, Scale},
    wayland::shell::wlr_layer::Layer as WlrLayer,
};

use super::{selected_glass_mode, surface_shader, KeyboardFocusTarget, SessionLock, Smallvil};

impl Smallvil {
    pub(crate) fn screencast_picker_element(
        &self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        #[cfg(feature = "screencast")]
        {
            self.screencast_picker
                .as_ref()
                .filter(|picker| picker.output_name() == output.name())
                .and_then(|picker| picker.render_element(renderer))
        }
        #[cfg(not(feature = "screencast"))]
        {
            let _ = (output, renderer);
            None
        }
    }

    /// Re-derives `window_opacity`/`window_glass_modes`/
    /// `window_shader_assignments` for one window from
    /// its currently-resolved rule. Shared by the full post-reload battery
    /// (`reload_config`, one call per mapped window) and by anything that
    /// needs a single window's opacity/glass to react to a live
    /// rule-affecting change without a full reload: urgency transitions and
    /// Smithay's authoritative xdg-toplevel title/app-id callbacks.
    pub(crate) fn refresh_window_opacity_and_glass_for(&mut self, surface: &WlSurface) {
        let rule = self.resolve_window_rules_for(surface);
        self.enforce_floating_size_constraints_with_rule(surface, &rule);
        // Identity and urgency participate in rule matching, including the
        // per-app weight. Re-resolve on the next sync; the retained record
        // lets a changed weight settle smoothly rather than snapping.
        self.physics.buoyancy_dirty = true;
        match crate::config::WindowOpacity::from_rule(&rule) {
            Some(opacity) => {
                self.window_opacity.insert(surface.clone(), opacity);
            }
            None => {
                self.window_opacity.remove(surface);
            }
        }
        match rule.glass {
            Some(mode) => {
                self.window_glass_modes.insert(surface.clone(), mode);
            }
            None => {
                self.window_glass_modes.remove(surface);
            }
        }
        match rule.shader {
            Some(assignment) => {
                self.window_shader_assignments
                    .insert(surface.clone(), assignment);
            }
            None => {
                self.window_shader_assignments.remove(surface);
            }
        }
        if !self.backdrop_effect_for_surface(surface, self.fullscreen.contains_key(surface)) {
            self.backdrop_textures.remove(surface);
            self.glass_anim.remove(surface);
        }
    }

    /// The dim overlay alpha for `output`, if any currently-mapped Overlay
    /// or Top layer surface resolves `layer_rule { dim_around = true }`.
    /// Only those two strata: dimming from a Bottom/Background surface
    /// (already behind everything) has nothing left to darken.
    pub(super) fn layer_dim_amount(&self, output: &Output) -> Option<f32> {
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            return None;
        }
        let layer_map = layer_map_for_output(output);
        let amount = [WlrLayer::Overlay, WlrLayer::Top]
            .into_iter()
            .flat_map(|kind| layer_map.layers_on(kind))
            .filter(|layer| !self.unmapped_layer_surfaces.contains(layer.wl_surface()))
            .filter_map(|layer| self.config.layer_dim_around(layer.namespace()))
            .fold(0.0f32, f32::max);
        (amount > 0.0).then_some(amount)
    }

    /// Smallest live logical width used by every currently mapped output.
    /// A shared toast is rasterized once, so sizing it for the narrowest
    /// transformed/scaled geometry keeps the same buffer on-screen everywhere.
    pub(crate) fn toast_output_width(&self) -> Option<i32> {
        self.space
            .outputs()
            .filter_map(|output| {
                self.space
                    .output_geometry(output)
                    .map(|geometry| geometry.size.w)
            })
            .filter(|width| *width > 0)
            .min()
    }

    pub(crate) fn toast_element(
        &mut self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let logical_width = self.space.output_geometry(output)?.size.w;
        let narrowest_output_width = self.toast_output_width();
        let scale = output.current_scale().fractional_scale();
        self.toast
            .as_mut()?
            .render_element(renderer, logical_width, narrowest_output_width, scale)
    }

    /// Whether the welcome-hint card should actually be drawn this frame:
    /// still enabled in config (`welcome_hint` built at all) and nothing
    /// mapped anywhere yet -- it gives way to the first real window the
    /// same way Hyprland's own hint does, rather than sitting on top of it.
    pub fn should_show_welcome_hint(&self) -> bool {
        self.welcome_hint.is_some() && self.space.elements().next().is_none()
    }

    pub(crate) fn snap_preview_element(
        &mut self,
        output: &Output,
    ) -> Option<SolidColorRenderElement> {
        let preview = self.snap_preview.as_ref()?;
        if preview.output != output.name() || self.config.snap.preview_opacity <= 0.0 {
            return None;
        }
        let output_geometry = self.space.output_geometry(output)?;
        let scale = output.current_scale().fractional_scale();
        Some(SolidColorRenderElement::from_buffer(
            &preview.buffer,
            crate::snap::preview_physical_location(preview.rect.loc, output_geometry.loc, scale),
            Scale::from(scale),
            self.config.snap.preview_opacity,
            Kind::Unspecified,
        ))
    }

    /// Builds the persistent config-error render element at the top of the
    /// layer-shell non-exclusive zone. The matching space reservation lives
    /// in `output_tiling_area` above.
    pub(crate) fn config_error_element(
        &mut self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let mode = output.current_mode()?;
        let scale = output.current_scale().fractional_scale();
        let local_y = layer_map_for_output(output).non_exclusive_zone().loc.y;
        let logical_width = (mode.size.w as f64 / scale).round() as i32;
        self.config_error_overlay
            .as_mut()?
            .render_element(renderer, logical_width, local_y, scale)
    }

    pub(crate) fn wallpaper_element(
        &mut self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Option<smithay::backend::renderer::element::texture::TextureRenderElement<GlesTexture>>
    {
        // Honor the `builtin_wallpaper` config toggle: when off, the embedded
        // 4K artwork is never decoded and no GPU texture is built, so neither
        // CPU nor VRAM is paid for it. Read live so a hot reload to false
        // stops drawing it immediately.
        if !self.config.builtin_wallpaper {
            return None;
        }
        let logical_size = self.space.output_geometry(output)?.size;
        self.builtin_wallpaper
            .render_element(renderer, logical_size)
    }

    /// The wallpaper for an offscreen capture (glass/layer backdrops, custom
    /// shader sources, workspace-transition snapshots). Captures render at
    /// the output's own fractional scale (see `backdrop::BackdropCapture`),
    /// so this is the same logical-sized element the visible frame draws.
    pub(crate) fn wallpaper_capture_element(
        &mut self,
        output: &Output,
        renderer: &mut GlesRenderer,
    ) -> Option<smithay::backend::renderer::element::texture::TextureRenderElement<GlesTexture>>
    {
        self.wallpaper_element(output, renderer)
    }

    /// The custom effect a window's rule assigns, live against the
    /// `shaders { enabled }` master and the current definitions.
    pub(crate) fn resolved_shader_assignment(
        &self,
        surface: &WlSurface,
    ) -> Option<(&str, &crate::config::ShaderDefinition)> {
        surface_shader(
            &self.config,
            &self.layer_shader_assignments,
            &self.window_shader_assignments,
            surface,
            self.workspace_of_surface(surface),
        )
    }

    /// Puts a render-time compile failure on the persistent warning panel.
    /// Deferred to idle: the panel reserves tiling space, so it must not
    /// appear while a frame is being assembled. A hard config error on the
    /// panel stays in front.
    pub(super) fn queue_shader_failure(&mut self, message: String) {
        tracing::warn!(%message, "Custom shader unavailable");
        self.loop_handle.insert_idle(move |state| {
            if state.config_warnings.contains(&message) {
                return;
            }
            state.config_warnings.push(message);
            if state.config_error_overlay.as_ref().is_some_and(|overlay| {
                overlay.severity() == crate::error_overlay::OverlaySeverity::Error
            }) {
                return;
            }
            let ui_theme = crate::ui_theme::UiTheme::from_config(&state.config);
            state.config_error_overlay = Some(crate::error_overlay::ConfigErrorOverlay::new(
                state.config_warnings.join("; "),
                crate::error_overlay::OverlaySeverity::Warning,
                ui_theme,
            ));
            state.toast = None;
            state.retile();
            state.request_redraw();
        });
    }

    /// Whether a custom effect can draw for this window this frame: assigned
    /// with the master on, and a last-good program or an untried source.
    fn custom_shader_drawable(&self, surface: &WlSurface) -> bool {
        !self.custom_shader_bypassed.contains(surface) && self.custom_shader_wanted(surface)
    }

    /// `custom_shader_drawable` before the budget: what admission judges.
    fn custom_shader_wanted(&self, surface: &WlSurface) -> bool {
        self.resolved_shader_assignment(surface)
            .is_some_and(|(name, definition)| {
                self.custom_shader_programs
                    .can_draw(name, &definition.stages)
            })
    }

    /// Admits or bypasses each shaded window on this output against the
    /// instance and payload caps, before any capture allocates. Payload is
    /// what the window's capture will cost at its definition's
    /// `render_scale`, plus every other admitted window's current capture.
    fn admit_custom_shader_captures(
        &mut self,
        candidates: &[(WlSurface, Rectangle<i32, Physical>)],
    ) {
        // Windows admitted earlier in this pass count at their wanted size
        // even though nothing is allocated for them yet.
        let mut admitted_now: Vec<(WlSurface, u64)> = Vec::new();
        if !self.custom_shader_bypassed.is_empty() {
            let unwanted: Vec<WlSurface> = self
                .custom_shader_bypassed
                .iter()
                .filter(|surface| !self.custom_shader_wanted(surface))
                .cloned()
                .collect();
            for surface in unwanted {
                self.custom_shader_bypassed.remove(&surface);
            }
        }
        for (surface, physical_rect) in candidates {
            if !self.custom_shader_wanted(surface) {
                continue;
            }
            let Some((name, render_scale, stage_count)) = self
                .resolved_shader_assignment(surface)
                .map(|(name, definition)| {
                    (
                        name.to_string(),
                        definition.render_scale,
                        definition.stages.len(),
                    )
                })
            else {
                continue;
            };
            // The capture plus one same-sized target per offscreen stage.
            let wanted = crate::shader_effect::capture_payload_bytes(
                physical_rect.size.w,
                physical_rect.size.h,
                render_scale,
            )
            .saturating_mul(stage_count as u64);
            let counted = |other: &WlSurface| {
                other != surface
                    && !self.custom_shader_bypassed.contains(other)
                    && !admitted_now.iter().any(|(admitted, _)| admitted == other)
            };
            let existing = self
                .custom_shader_instances
                .iter()
                .filter(|(other, _)| counted(other));
            let (existing_count, existing_bytes) =
                existing.fold((0usize, 0_u64), |(count, bytes), (other, instance)| {
                    let capture = self
                        .backdrop_textures
                        .get(other)
                        .map_or(0, |capture| capture.estimated_texture_bytes());
                    (
                        count + 1,
                        bytes
                            .saturating_add(capture)
                            .saturating_add(instance.target_bytes()),
                    )
                });
            let pass_bytes = admitted_now
                .iter()
                .fold(0_u64, |total, (_, bytes)| total.saturating_add(*bytes));
            let instances = existing_count + admitted_now.len() + 1;
            let bytes = existing_bytes
                .saturating_add(pass_bytes)
                .saturating_add(wanted);
            if instances <= crate::shader_effect::MAX_ACTIVE_INSTANCES
                && bytes <= crate::shader_effect::MAX_PAYLOAD_BYTES
            {
                self.custom_shader_bypassed.remove(surface);
                admitted_now.push((surface.clone(), wanted));
            } else if self.custom_shader_bypassed.insert(surface.clone()) {
                self.custom_shader_instances.remove(surface);
                self.custom_shader_bypasses += 1;
                self.queue_shader_failure(format!(
                    "Shader {name:?} skipped on a window: custom effects are capped at {} windows and {} MiB of captures",
                    crate::shader_effect::MAX_ACTIVE_INSTANCES,
                    crate::shader_effect::MAX_PAYLOAD_BYTES / (1024 * 1024)
                ));
            }
        }
    }

    /// Whether a window's captured backdrop feeds anything: a custom effect
    /// under its own master, or water/frost glass under `water_effects`.
    fn backdrop_effect_for_surface(&self, surface: &WlSurface, fullscreen: bool) -> bool {
        self.custom_shader_drawable(surface)
            || (self.config.water_effects
                && self.glass_mode_for_surface(surface, fullscreen).is_some())
    }

    fn glass_mode_for_surface(
        &self,
        surface: &WlSurface,
        fullscreen: bool,
    ) -> Option<crate::config::GlassMode> {
        let frost = self.frost_config_for_surface(surface);
        let opacity = self.window_render_alpha_for(surface, fullscreen);
        selected_glass_mode(
            self.window_glass_modes.get(surface).copied(),
            (opacity < 1.0).then_some(opacity),
            frost.enabled,
        )
    }

    pub(crate) fn window_render_alpha(&self, surface: &WlSurface) -> f32 {
        self.window_render_alpha_for(surface, self.fullscreen.contains_key(surface))
    }

    pub(super) fn window_render_alpha_for(&self, surface: &WlSurface, fullscreen: bool) -> f32 {
        let focused = matches!(&self.keyboard_focus, KeyboardFocusTarget::Window(focused) if focused == surface);
        self.window_opacity
            .get(surface)
            .copied()
            .map(|opacity| opacity.alpha(focused, fullscreen))
            .unwrap_or(1.0)
    }

    fn frost_config_for_surface(&self, surface: &WlSurface) -> crate::config::FrostConfig {
        let rule = self.resolve_window_rules_for(surface);
        rule.frost
            .as_ref()
            .map(|overrides| overrides.apply_to(&self.config.frost))
            .unwrap_or_else(|| self.config.frost.clone())
    }

    fn shadow_config_for_surface(&self, surface: &WlSurface) -> crate::config::ShadowConfig {
        let rule = self.resolve_window_rules_for(surface);
        let base = self
            .workspace_of_surface(surface)
            .and_then(|ws| self.config.resolve_workspace_rule(ws).shadow)
            .map(|overrides| overrides.apply_to(&self.config.shadow))
            .unwrap_or_else(|| self.config.shadow.clone());
        rule.shadow
            .as_ref()
            .map(|overrides| overrides.apply_to(&base))
            .unwrap_or(base)
    }

    pub(super) fn rounding_config_for_surface(
        &self,
        surface: &WlSurface,
    ) -> crate::config::RoundingConfig {
        let rule = self.resolve_window_rules_for(surface);
        let base = self
            .workspace_of_surface(surface)
            .and_then(|ws| self.config.resolve_workspace_rule(ws).rounding)
            .map(|overrides| overrides.apply_to(&self.config.rounding))
            .unwrap_or_else(|| self.config.rounding.clone());
        rule.rounding
            .as_ref()
            .map(|overrides| overrides.apply_to(&base))
            .unwrap_or(base)
    }

    fn border_config_for_surface(&self, surface: &WlSurface) -> crate::config::BorderConfig {
        let rule = self.resolve_window_rules_for(surface);
        let base = self
            .workspace_of_surface(surface)
            .and_then(|ws| self.config.resolve_workspace_rule(ws).border)
            .map(|overrides| overrides.apply_to(&self.config.border))
            .unwrap_or_else(|| self.config.border.clone());
        rule.border
            .as_ref()
            .map(|overrides| overrides.apply_to(&base))
            .unwrap_or(base)
    }

    pub(super) fn rounding_applies(
        &self,
        config: &crate::config::RoundingConfig,
        floating: bool,
        fullscreen: bool,
    ) -> bool {
        config.enabled
            && config.clip
            && (!config.floating_only || floating)
            && (!fullscreen || config.fullscreen)
            && config.radii.iter().any(|radius| *radius > 0.0)
    }

    fn rounding_possible(&self) -> bool {
        (self.config.rounding.enabled && self.config.rounding.clip)
            || self.config.window_rules.iter().any(|rule| {
                rule.rounding.as_ref().is_some_and(|rounding| {
                    rounding.enabled == Some(true) || rounding.clip == Some(true)
                })
            })
    }

    pub(super) fn rounding_or_cascade_shader_possible(&self) -> bool {
        self.rounding_possible()
            || !self.cascade_window_animations.is_empty()
            || self
                .closing_window_animations
                .iter()
                .any(|closing| closing.cascade.is_some())
    }

    pub(super) fn borders_possible(&self) -> bool {
        self.config.border.enabled
            || self.config.window_rules.iter().any(|rule| {
                rule.border
                    .as_ref()
                    .and_then(|border| border.enabled)
                    .unwrap_or(false)
            })
    }

    pub(super) fn animated_borders_possible(&self) -> bool {
        self.space.elements().any(|window| {
            window
                .toplevel()
                .is_some_and(|toplevel| self.surface_has_animated_border(toplevel.wl_surface()))
        })
    }

    pub(super) fn surface_has_animated_border(&self, surface: &WlSurface) -> bool {
        let border = self.border_config_for_surface(surface);
        let focused = matches!(
            &self.keyboard_focus,
            KeyboardFocusTarget::Window(focused) if focused == surface
        );
        let urgent = self.urgent.contains(surface);
        let state_animated = if urgent {
            border.animate_urgent
        } else if focused {
            border.animate_focused
        } else {
            border.animate_inactive
        };
        border.enabled
            && border.width > 0.0
            && border.animate
            && state_animated
            && (focused || urgent || border.inactive_enabled)
            && (!border.floating_only || self.is_floating(surface))
            && (!self.fullscreen.contains_key(surface) || border.fullscreen)
    }

    pub(super) fn window_border_element(
        &self,
        output: &Output,
        placement: &crate::placement::PlacedWindow,
        surface: &WlSurface,
        program: GlesPixelProgram,
        visual: crate::window_animation::VisualSample,
    ) -> Option<crate::decoration::BorderElement> {
        let window = &placement.window;
        let floating = placement.is_floating();
        let fullscreen = placement.is_fullscreen();
        let mut border = self.border_config_for_surface(surface);
        border.opacity *= visual.opacity;
        border.inactive_opacity *= visual.opacity;
        border.urgent_opacity *= visual.opacity;
        // A smart-tiling drag's current swap target borrows the active
        // border as a magnet-style highlight -- no real focus change, no
        // new render element, just the existing per-window border reused
        // for a different reason while the drag hint names this surface.
        let focused = matches!(
            &self.keyboard_focus,
            KeyboardFocusTarget::Window(focused) if focused == surface
        ) || self.ocean.drag_hint() == Some(surface);
        let urgent = self.urgent.contains(surface);
        if !border.enabled
            || border.width <= 0.0
            || (!focused && !urgent && !border.inactive_enabled)
            || (border.floating_only && !floating)
            || (fullscreen && !border.fullscreen)
        {
            return None;
        }
        let output_geo = self.space.output_geometry(output)?;
        let output_scale = output.current_scale().fractional_scale();
        let visual_offset = Point::from((
            visual.offset.x.round() as i32,
            visual.offset.y.round() as i32,
        ));
        let visual_size = visual.rounded_size_or(window.geometry().size);
        let logical_rect = Rectangle::new(
            placement.rect.loc - output_geo.loc + visual_offset,
            visual_size,
        );
        let physical_rect = logical_rect.to_physical_precise_round(output_scale);
        let mut rounding = self.rounding_config_for_surface(surface);
        if !rounding.enabled {
            rounding.radii = [0.0; 4];
        }
        let id = smithay::backend::renderer::element::Id::from(surface);
        Some(crate::decoration::BorderElement::new(
            &id,
            physical_rect,
            output_scale,
            program,
            &rounding,
            &border,
            focused,
            urgent,
            self.start_time.elapsed().as_secs_f32(),
        ))
    }

    pub(super) fn shadows_possible(&self) -> bool {
        self.config.shadow.enabled
            || self.config.window_rules.iter().any(|rule| {
                rule.shadow
                    .as_ref()
                    .and_then(|shadow| shadow.enabled)
                    .unwrap_or(false)
            })
    }

    pub(super) fn window_shadow_element(
        &self,
        output: &Output,
        placement: &crate::placement::PlacedWindow,
        surface: &WlSurface,
        program: GlesPixelProgram,
        visual: crate::window_animation::VisualSample,
    ) -> Option<crate::shadow::ShadowElement> {
        let window = &placement.window;
        let floating = placement.is_floating();
        let fullscreen = placement.is_fullscreen();
        let mut config = self.shadow_config_for_surface(surface);
        config.opacity *= visual.opacity;
        config.inactive_opacity *= visual.opacity;
        config.urgent_opacity *= visual.opacity;
        if config.corner_radius == 0.0 {
            let rounding = self.rounding_config_for_surface(surface);
            if rounding.enabled {
                config.corner_radius = rounding.radii[0];
            }
        }
        if !config.enabled
            || (config.floating_only && !floating)
            || (fullscreen && !config.fullscreen)
        {
            return None;
        }

        let output_geo = self.space.output_geometry(output)?;
        let output_scale = output.current_scale().fractional_scale();
        let visual_offset = Point::from((
            visual.offset.x.round() as i32,
            visual.offset.y.round() as i32,
        ));
        let visual_size = visual.rounded_size_or(window.geometry().size);
        let logical_rect = Rectangle::new(
            placement.rect.loc - output_geo.loc + visual_offset,
            visual_size,
        );
        let physical_rect = logical_rect.to_physical_precise_round(output_scale);
        let focused = matches!(
            &self.keyboard_focus,
            KeyboardFocusTarget::Window(focused) if focused == surface
        );
        let urgent = self.urgent.contains(surface);
        let id = crate::shadow::shadow_id(surface, &config, focused, urgent);
        Some(crate::shadow::ShadowElement::new(
            id,
            physical_rect,
            output_scale,
            program,
            config,
            focused,
            urgent,
        ))
    }

    /// Reconciles one class of capture against an output's latest scene.
    /// A texture is retained while any output still presents its surface.
    #[allow(clippy::mutable_key_type)] // Existing WlSurface-keyed ownership registries.
    fn reconcile_backdrop_visibility(
        &mut self,
        output_name: &str,
        visible: &[WlSurface],
        windows: bool,
    ) {
        let foreign_toplevels = &self.foreign_toplevels;
        self.backdrop_textures.retain(|surface, capture| {
            if foreign_toplevels.contains_key(surface) != windows {
                return true;
            }
            let visible = visible.iter().any(|candidate| candidate == surface);
            capture.set_output_visible(output_name, visible)
        });
    }

    /// Drops a disconnected or powered-off output's claims on captures.
    /// Textures still presented through another Ocean camera remain alive.
    pub(crate) fn remove_backdrop_output(&mut self, output_name: &str) {
        self.backdrop_textures
            .retain(|_, capture| capture.set_output_visible(output_name, false));
    }

    /// Captures the backdrop behind every currently-visible window whose
    /// resolved rule selects water/frost glass (or whose compositor opacity
    /// implicitly selects water glass). This includes tiled placements in
    /// both Classic and Ocean: a tile samples the wallpaper and below-window
    /// effects inside its own visible rect. The gaps remain untouched.
    ///
    /// Eligibility is config-driven, so ordinary opaque placements allocate
    /// no capture. Water and frost sample the stored texture in the same frame.
    pub(crate) fn capture_window_backdrops(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        placements: &[crate::placement::PlacedWindow],
    ) {
        self.custom_shader_compile_budget = 1;
        if self.config.shaders_enabled {
            let candidates: Vec<(WlSurface, Rectangle<i32, Physical>)> = placements
                .iter()
                .filter(|placement| placement.replacement_eligible())
                .filter_map(|placement| {
                    let surface = placement.surface()?;
                    self.custom_shader_wanted(surface).then_some(())?;
                    Some((
                        surface.clone(),
                        self.placement_physical_rect(placement, output)?,
                    ))
                })
                .collect();
            self.admit_custom_shader_captures(&candidates);
        }
        let surfaces: Vec<WlSurface> = placements
            .iter()
            .filter(|placement| placement.replacement_eligible())
            .filter_map(|placement| {
                let surface = placement.surface()?;
                (self
                    .window_depths
                    .get(surface)
                    .is_none_or(|depth| depth.tier() < 2)
                    && self.backdrop_effect_for_surface(surface, placement.is_fullscreen()))
                .then(|| surface.clone())
            })
            .collect();
        let output_name = output.name();
        self.reconcile_backdrop_visibility(&output_name, &surfaces, true);
        // A window whose capture was released (hidden, off every output) or
        // that no longer has an effect drops its stage textures.
        let unshaded: Vec<WlSurface> = self
            .custom_shader_instances
            .keys()
            .filter(|surface| {
                !self.backdrop_textures.contains_key(*surface)
                    || self.resolved_shader_assignment(surface).is_none()
            })
            .cloned()
            .collect();
        for surface in unshaded {
            self.custom_shader_instances.remove(&surface);
        }
        if surfaces.is_empty() {
            return;
        }

        // One shared behind-list per output, not one rebuild per glass
        // window: `capture` translates it into each window's own texture
        // space itself. Every glass surface is skipped, so a window never
        // captures itself -- and two overlapping glass windows read what's
        // below the pair rather than each other's raw surfaces, which is
        // the trade that makes the shared list possible.
        let (mut depth_layers, deep_surfaces) =
            self.depth_frame_elements(renderer, output, placements);
        let skip: Vec<WlSurface> = surfaces.iter().cloned().chain(deep_surfaces).collect();
        let Some(space_elements) = self.desktop_render_elements(
            renderer,
            output,
            placements,
            &skip,
            &mut depth_layers,
            &mut HashMap::new(),
            Vec::new(),
        ) else {
            return;
        };
        // Front-to-back, index 0 topmost: the windows come first and the
        // wallpaper goes last, the same order the visible frame uses.
        // Pushing the wallpaper first put it *on top of* every window in
        // the captured texture -- the glass then sampled a texture that
        // was nothing but wallpaper, which read as "the aqua wave only
        // shows the background and ignores everything else."
        let behind: Vec<crate::backend::udev::OutputRenderElements> = space_elements
            .into_iter()
            .chain(
                self.wallpaper_capture_element(output, renderer)
                    .map(crate::backend::udev::OutputRenderElements::Wallpaper),
            )
            .collect();

        let chained: Vec<WlSurface> = surfaces
            .iter()
            .filter(|surface| {
                self.custom_shader_drawable(surface)
                    && self
                        .resolved_shader_assignment(surface)
                        .is_some_and(|(_, definition)| definition.stages.len() > 1)
            })
            .cloned()
            .collect();
        let mut rendered = 0usize;
        let mut skipped = 0usize;
        let mut captured_first_backdrop = false;
        for surface in surfaces {
            let Some(placement) = placements.iter().find(|placement| {
                placement.replacement_eligible()
                    && placement.surface().is_some_and(|placed| placed == &surface)
            }) else {
                continue;
            };
            let Some(physical_rect) = self.placement_physical_rect(placement, output) else {
                continue;
            };

            // A custom effect processes its capture at its own divisor.
            let capture_scale = self
                .custom_shader_drawable(&surface)
                .then(|| self.resolved_shader_assignment(&surface))
                .flatten()
                .map(|(_, definition)| definition.render_scale)
                .unwrap_or(self.config.backdrop_capture_scale);
            let first_capture = !self.backdrop_textures.contains_key(&surface);
            if first_capture {
                let Some(mut capture) = crate::backdrop::BackdropCapture::new(
                    renderer,
                    physical_rect.size,
                    capture_scale,
                    output.current_scale().fractional_scale(),
                ) else {
                    continue;
                };
                capture.set_output_visible(&output_name, true);
                self.backdrop_textures.insert(surface.clone(), capture);
            }
            let Some(capture) = self.backdrop_textures.get_mut(&surface) else {
                continue;
            };
            match capture.capture(
                renderer,
                physical_rect,
                &behind,
                capture_scale,
                output.current_scale().fractional_scale(),
            ) {
                Some(true) => {
                    rendered += 1;
                    captured_first_backdrop |= first_capture;
                }
                Some(false) => skipped += 1,
                None => {}
            }
        }
        tracing::debug!(
            output = output_name,
            rendered,
            skipped,
            "Window backdrop captures"
        );
        if self.backend_name == "udev" {
            self.native_render_stats
                .entry(output.clone())
                .or_default()
                .record_backdrops(rendered, skipped);
        }
        self.run_custom_shader_chains(renderer, output, &chained);
        // The frame that triggered the first capture could otherwise be the
        // last dirty frame on a static desktop. Schedule exactly one more so
        // the newly available texture is actually consumed; later capture
        // replacements do not self-sustain an idle redraw loop.
        if captured_first_backdrop {
            self.request_redraw();
        }
    }

    /// Renders the offscreen stages of every chained effect on this output
    /// from its fresh capture, still before the visible bind.
    fn run_custom_shader_chains(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        surfaces: &[WlSurface],
    ) {
        let mut failures = Vec::new();
        let mut deferred = false;
        let mut updates = 0_u64;
        let work: Vec<(WlSurface, String)> = surfaces
            .iter()
            .filter_map(|surface| {
                self.resolved_shader_assignment(surface)
                    .map(|(name, _)| (surface.clone(), name.to_string()))
            })
            .collect();
        for (surface, name) in &work {
            let (Some(definition), Some(capture)) = (
                self.config.shader_definitions.get(name),
                self.backdrop_textures.get(surface),
            ) else {
                continue;
            };
            let instance = self
                .custom_shader_instances
                .entry(surface.clone())
                .or_insert_with(crate::shader_effect::ShaderInstance::new);
            let before = instance.updates();
            let outcome = crate::shader_effect::run_chain(
                renderer,
                &mut self.custom_shader_programs,
                instance,
                name,
                definition,
                &capture.texture,
                (&capture.id, capture.version),
                &mut self.custom_shader_compile_budget,
            );
            updates = updates.saturating_add(instance.updates().saturating_sub(before));
            failures.extend(outcome.failures);
            deferred |= outcome.deferred;
        }
        if self.backend_name == "udev" {
            self.native_render_stats
                .entry(output.clone())
                .or_default()
                .record_shader_updates(updates);
        }
        if deferred {
            self.request_redraw();
        }
        for message in failures {
            self.queue_shader_failure(message);
        }
    }

    /// Captures backdrops for mapped layer surfaces whose rule enables blur
    /// or names a custom effect. Window and layer surfaces safely share the
    /// `WlSurface`-keyed cache. Layer frost is independent of the
    /// water-effects master toggle; layer shaders follow `shaders.enabled`.
    pub(crate) fn capture_layer_backdrops(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        placements: &[crate::placement::PlacedWindow],
    ) {
        let frost_enabled = self.config.frost.enabled;
        if !frost_enabled && !self.config.shaders_enabled {
            self.layer_alpha_masks.clear();
            return;
        }
        let output_scale = output.current_scale().fractional_scale();
        let mut candidates = Vec::new();
        if self.config.shaders_enabled {
            let layer_map = layer_map_for_output(output);
            for layer in layer_map
                .layers()
                .filter(|layer| !self.unmapped_layer_surfaces.contains(layer.wl_surface()))
            {
                let surface = layer.wl_surface();
                match self.config.layer_shader(layer.namespace()) {
                    Some(name) => {
                        let current = self.layer_shader_assignments.get(surface);
                        if !matches!(current, Some(crate::config::ShaderAssignment::Named(existing)) if existing == name)
                        {
                            self.layer_shader_assignments.insert(
                                surface.clone(),
                                crate::config::ShaderAssignment::Named(name.to_string()),
                            );
                        }
                        if let Some(geometry) = layer_map.layer_geometry(layer) {
                            candidates.push((
                                surface.clone(),
                                geometry.to_physical_precise_round(output_scale),
                            ));
                        }
                    }
                    None => {
                        self.layer_shader_assignments.remove(surface);
                    }
                }
            }
        }
        self.admit_custom_shader_captures(&candidates);
        #[allow(clippy::mutable_key_type)]
        let (surfaces, alpha_surfaces) = {
            let layer_map = layer_map_for_output(output);
            let layers: Vec<_> = layer_map
                .layers()
                .filter(|layer| !self.unmapped_layer_surfaces.contains(layer.wl_surface()))
                .filter(|layer| {
                    self.custom_shader_drawable(layer.wl_surface())
                        || (frost_enabled && self.config.layer_blur(layer.namespace()))
                })
                .filter_map(|layer| {
                    let geometry = layer_map.layer_geometry(layer)?;
                    Some((
                        layer.wl_surface().clone(),
                        layer.clone(),
                        geometry.to_physical_precise_round(output_scale),
                        self.config.layer_ignore_alpha(layer.namespace()),
                    ))
                })
                .collect();
            let surfaces: Vec<(WlSurface, Rectangle<i32, Physical>)> = layers
                .iter()
                .map(|(surface, _, rect, _)| (surface.clone(), *rect))
                .collect();
            let alpha_surfaces = layers
                .into_iter()
                .filter_map(|(surface, layer, rect, threshold)| {
                    threshold.map(|threshold| (surface, layer, rect, threshold))
                })
                .collect::<Vec<_>>();
            (surfaces, alpha_surfaces)
        };
        let output_name = output.name();
        let layer_surfaces: Vec<WlSurface> = surfaces
            .iter()
            .map(|(surface, _)| surface.clone())
            .collect();
        self.reconcile_backdrop_visibility(&output_name, &layer_surfaces, false);
        let current_output_layers: Vec<WlSurface> = layer_map_for_output(output)
            .layers()
            .map(|layer| layer.wl_surface().clone())
            .collect();
        let alpha_eligible: Vec<WlSurface> = alpha_surfaces
            .iter()
            .map(|(surface, _, _, _)| surface.clone())
            .collect();
        self.layer_alpha_masks.retain(|surface, _| {
            !current_output_layers.contains(surface) || alpha_eligible.contains(surface)
        });
        if surfaces.is_empty() {
            return;
        }

        // Alpha masks are rendered from the complete layer surface tree at
        // the same origin and downscale as the backdrop capture. This keeps
        // subsurfaces and viewport transforms aligned with the frost sample.
        let scale = Scale::from(output_scale);
        for (surface, layer, physical_rect, _) in &alpha_surfaces {
            let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                layer.render_elements(renderer, physical_rect.loc, scale, 1.0);
            let capture_scale = self.config.backdrop_capture_scale;
            if !self.layer_alpha_masks.contains_key(surface) {
                let Some(capture) = crate::backdrop::BackdropCapture::new(
                    renderer,
                    physical_rect.size,
                    capture_scale,
                    output.current_scale().fractional_scale(),
                ) else {
                    continue;
                };
                self.layer_alpha_masks.insert(surface.clone(), capture);
            }
            if let Some(capture) = self.layer_alpha_masks.get_mut(surface) {
                let _ = capture.capture(
                    renderer,
                    *physical_rect,
                    &elements,
                    capture_scale,
                    output_scale,
                );
            }
        }

        // Same shared-list shape as the floating-window pass above: blurred
        // layers are all excluded from the list, so one build serves every
        // layer's capture and no blurred layer ever captures itself or a
        // sibling blurred layer.
        let (mut depth_layers, deep_surfaces) =
            self.depth_frame_elements(renderer, output, placements);
        let skip: Vec<WlSurface> = layer_surfaces.into_iter().chain(deep_surfaces).collect();
        let Some(space_elements) = self.desktop_render_elements(
            renderer,
            output,
            placements,
            &skip,
            &mut depth_layers,
            &mut HashMap::new(),
            Vec::new(),
        ) else {
            return;
        };
        let wallpaper = self.wallpaper_capture_element(output, renderer);
        let behind: Vec<crate::backend::udev::OutputRenderElements> = space_elements
            .into_iter()
            .chain(wallpaper.map(crate::backend::udev::OutputRenderElements::Wallpaper))
            .collect();

        let chained: Vec<WlSurface> = surfaces
            .iter()
            .map(|(surface, _)| surface)
            .filter(|surface| {
                self.custom_shader_drawable(surface)
                    && self
                        .resolved_shader_assignment(surface)
                        .is_some_and(|(_, definition)| definition.stages.len() > 1)
            })
            .cloned()
            .collect();
        let mut rendered = 0usize;
        let mut skipped = 0usize;
        let mut captured_first_backdrop = false;
        for (surface, physical_rect) in surfaces {
            let capture_scale = self
                .custom_shader_drawable(&surface)
                .then(|| self.resolved_shader_assignment(&surface))
                .flatten()
                .map(|(_, definition)| definition.render_scale)
                .unwrap_or(self.config.backdrop_capture_scale);
            let first_capture = !self.backdrop_textures.contains_key(&surface);
            if first_capture {
                let Some(mut capture) = crate::backdrop::BackdropCapture::new(
                    renderer,
                    physical_rect.size,
                    capture_scale,
                    output.current_scale().fractional_scale(),
                ) else {
                    continue;
                };
                capture.set_output_visible(&output_name, true);
                self.backdrop_textures.insert(surface.clone(), capture);
            }
            let Some(capture) = self.backdrop_textures.get_mut(&surface) else {
                continue;
            };
            match capture.capture(
                renderer,
                physical_rect,
                &behind,
                capture_scale,
                output.current_scale().fractional_scale(),
            ) {
                Some(true) => {
                    rendered += 1;
                    captured_first_backdrop |= first_capture;
                }
                Some(false) => skipped += 1,
                None => {}
            }
        }
        tracing::debug!(
            output = output_name,
            rendered,
            skipped,
            "Layer backdrop captures"
        );
        if self.backend_name == "udev" {
            self.native_render_stats
                .entry(output.clone())
                .or_default()
                .record_backdrops(rendered, skipped);
        }
        self.run_custom_shader_chains(renderer, output, &chained);
        if captured_first_backdrop {
            self.request_redraw();
        }
    }

    /// Windows on `output` eligible for a captured glass layer this frame:
    /// a drawable custom effect, or `water_effects` on with either an
    /// explicit `glass` mode or the backward-compatible implicit trigger
    /// (`opacity` below 1.0 means water), and a backdrop already captured.
    /// Tiled and floating placements share this path; ordinary opaque tiles
    /// never enter it.
    /// Callers use this list to build glass layers that are inserted directly
    /// behind each surface in its normal z-slot.
    pub(crate) fn glass_eligible_surfaces(
        &self,
        placements: &[crate::placement::PlacedWindow],
    ) -> Vec<WlSurface> {
        placements
            .iter()
            .filter(|placement| placement.replacement_eligible())
            .filter_map(|placement| {
                let surface = placement.surface()?;
                (self
                    .window_depths
                    .get(surface)
                    .is_none_or(|depth| depth.tier() < 2)
                    && self.backdrop_effect_for_surface(surface, placement.is_fullscreen())
                    && self.backdrop_textures.contains_key(surface))
                .then(|| surface.clone())
            })
            .collect()
    }

    /// Builds only the captured glass layer for each eligible surface. The
    /// desktop walk owns client chrome and inserts this layer in its real z-slot.
    #[allow(clippy::mutable_key_type)] // WlSurface keys, same as every other map here
    pub(crate) fn glass_layer_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        placements: &[crate::placement::PlacedWindow],
        surfaces: &[WlSurface],
    ) -> HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>> {
        let mut layers: HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>> =
            HashMap::new();
        if surfaces.is_empty() {
            return layers;
        }
        let needs_water = self.config.water_effects
            && surfaces.iter().any(|surface| {
                self.window_glass_modes
                    .get(surface)
                    .is_none_or(|mode| *mode == crate::config::GlassMode::Water)
            });
        let needs_frost = self.config.water_effects
            && surfaces.iter().any(|surface| {
                self.window_glass_modes.get(surface) == Some(&crate::config::GlassMode::Frost)
            });
        let water_program = needs_water
            .then(|| {
                crate::water_glass::water_glass_program(&mut self.water_glass_program, renderer)
            })
            .flatten();
        let frost_program = needs_frost
            .then(|| {
                crate::frost_glass::frost_glass_program(&mut self.frost_glass_program, renderer)
            })
            .flatten();
        let Some(output_geo) = self.space.output_geometry(output) else {
            return layers;
        };
        for surface in surfaces {
            let Some((capture_id, capture_version, capture_texture)) = self
                .backdrop_textures
                .get(surface)
                .map(|capture| (capture.id.clone(), capture.version, capture.texture.clone()))
            else {
                continue;
            };
            let Some(placement) = placements.iter().find(|placement| {
                placement.replacement_eligible()
                    && placement.surface().is_some_and(|placed| placed == surface)
            }) else {
                continue;
            };
            let Some(physical_rect) = self.placement_physical_rect(placement, output) else {
                continue;
            };
            let location = placement.rect.loc;
            let visual = self.placement_visual_sample(placement);
            // A custom effect replaces the glass pass. Without a program yet
            // (or ever) the window takes its normal glass path instead.
            if let Some(element) = self.custom_shader_last_stage(
                renderer,
                surface,
                (capture_id.clone(), capture_version, &capture_texture),
                physical_rect,
                Some((
                    visual.opacity,
                    output.current_scale().fractional_scale() as f32,
                )),
                None,
            ) {
                self.glass_anim.remove(surface);
                layers.entry(surface.clone()).or_default().push(
                    crate::backend::udev::OutputRenderElements::CustomShader(element),
                );
                continue;
            }
            if !self.config.water_effects
                || self
                    .glass_mode_for_surface(surface, placement.is_fullscreen())
                    .is_none()
            {
                continue;
            }
            match self.window_glass_modes.get(surface).copied() {
                Some(crate::config::GlassMode::Frost) => {
                    if let Some(program) = &frost_program {
                        let mut frost = self.frost_config_for_surface(surface);
                        frost.opacity *= visual.opacity;
                        let rounding = self.rounding_config_for_surface(surface);
                        let output_scale = output.current_scale().fractional_scale() as f32;
                        let (corner_radii, rounding_power, corner_softness) = if rounding.enabled {
                            (
                                rounding.radii.map(|radius| radius * output_scale),
                                rounding.power,
                                rounding.antialias * output_scale,
                            )
                        } else {
                            (
                                [frost.corner_radius * output_scale; 4],
                                2.0,
                                frost.corner_softness * output_scale,
                            )
                        };
                        layers.entry(surface.clone()).or_default().push(
                            crate::backend::udev::OutputRenderElements::FrostGlass(
                                crate::frost_glass::FrostGlassElement::new(
                                    capture_id.clone(),
                                    crate::frost_glass::frost_glass_commit(
                                        capture_version,
                                        &frost,
                                        corner_radii,
                                        rounding_power,
                                        corner_softness,
                                        None,
                                    ),
                                    capture_texture.clone(),
                                    physical_rect,
                                    program.clone(),
                                    frost,
                                    corner_radii,
                                    rounding_power,
                                    corner_softness,
                                ),
                            ),
                        );
                    }
                }
                Some(crate::config::GlassMode::Plain) => {}
                Some(crate::config::GlassMode::Water) | None => {
                    if let Some(program) = &water_program {
                        let rounding = self.rounding_config_for_surface(surface);
                        let corner_radii = if rounding.enabled {
                            rounding.radii.map(|radius| {
                                radius * output.current_scale().fractional_scale() as f32
                            })
                        } else {
                            [0.0; 4]
                        };
                        let anim_cfg = self.config.water_glass;
                        let (phase, amp) = match anim_cfg.animation {
                            crate::config::GlassAnimation::Static => (0.0, anim_cfg.amplitude),
                            mode => {
                                // A ripple passing underneath disturbs the
                                // glass: ripples are output-local logical,
                                // so compare in that space.
                                let local_rect =
                                    Rectangle::new(location - output_geo.loc, placement.rect.size);
                                let output_name = output.name();
                                let ripple_passed = self.ripples.iter().any(|ripple| {
                                    !ripple.finished()
                                        && ripple.output == output_name
                                        && ripple.element_rect().overlaps(local_rect)
                                });
                                let anim =
                                    self.glass_anim.entry(surface.clone()).or_insert_with(|| {
                                        crate::water_glass::GlassAnim::new(physical_rect)
                                    });
                                anim.observe(physical_rect, ripple_passed);
                                let envelope = match mode {
                                    crate::config::GlassAnimation::Ambient => 1.0,
                                    _ => anim.envelope(anim_cfg.settle_ms),
                                };
                                (anim.phase(anim_cfg.speed), anim_cfg.amplitude * envelope)
                            }
                        };
                        let antialias =
                            rounding.antialias * output.current_scale().fractional_scale() as f32;
                        layers.entry(surface.clone()).or_default().push(
                            crate::backend::udev::OutputRenderElements::WaterGlass(
                                crate::water_glass::WaterGlassElement::new(
                                    capture_id.clone(),
                                    crate::water_glass::water_glass_commit(
                                        capture_version,
                                        phase,
                                        amp,
                                        corner_radii,
                                        rounding.power,
                                        antialias,
                                    ),
                                    capture_texture.clone(),
                                    physical_rect,
                                    program.clone(),
                                    corner_radii,
                                    rounding.power,
                                    antialias,
                                    visual.opacity,
                                    phase,
                                    amp,
                                ),
                            ),
                        );
                    }
                }
            }
        }
        layers
    }

    /// The layer-shell analog of `glass_layer_elements`, much smaller:
    /// layers have no water mode, no per-window opacity/rounding to fold
    /// in, and no rule-scoped frost override -- just the global `frost { }`
    /// block, matching how Hyprland's own `layerrule = blur` has no
    /// per-namespace strength knob either. Callers merge the result into
    /// the same `glass_layers` map already threaded through
    /// `desktop_render_elements` for windows -- disjoint `WlSurface` keys,
    /// so a plain `HashMap::extend` is enough, no new parameter needed
    /// anywhere in that call chain.
    #[allow(clippy::mutable_key_type)]
    pub(crate) fn layer_glass_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> HashMap<WlSurface, Vec<crate::backend::udev::OutputRenderElements>> {
        let mut result = HashMap::new();
        let output_scale = output.current_scale().fractional_scale();
        let frost_enabled = self.config.frost.enabled;
        // Surface, physical rect, `ignore_alpha` threshold, frost fallback.
        type LayerEffect = (WlSurface, Rectangle<i32, Physical>, Option<f32>, bool);
        let eligible: Vec<LayerEffect> = {
            let layer_map = layer_map_for_output(output);
            layer_map
                .layers()
                .filter(|layer| !self.unmapped_layer_surfaces.contains(layer.wl_surface()))
                .filter_map(|layer| {
                    let blur = frost_enabled && self.config.layer_blur(layer.namespace());
                    if !blur && !self.custom_shader_drawable(layer.wl_surface()) {
                        return None;
                    }
                    let geometry = layer_map.layer_geometry(layer)?;
                    Some((
                        layer.wl_surface().clone(),
                        geometry.to_physical_precise_round(output_scale),
                        self.config.layer_ignore_alpha(layer.namespace()),
                        blur,
                    ))
                })
                .collect()
        };
        if eligible.is_empty() {
            return result;
        }
        let program = eligible
            .iter()
            .any(|(_, _, _, blur)| *blur)
            .then(|| {
                crate::frost_glass::frost_glass_program(&mut self.frost_glass_program, renderer)
            })
            .flatten();
        let frost = self.config.frost.clone();
        let scale = output_scale as f32;
        let corner_radii = [frost.corner_radius * scale; 4];
        let rounding_power = 2.0;
        let corner_softness = frost.corner_softness * scale;
        for (surface, physical_rect, ignore_alpha, blur) in eligible {
            let Some(capture) = self.backdrop_textures.get(&surface) else {
                continue;
            };
            let (id, version, texture) =
                (capture.id.clone(), capture.version, capture.texture.clone());
            let alpha_mask = ignore_alpha.and_then(|threshold| {
                self.layer_alpha_masks
                    .get(&surface)
                    .map(|mask| (mask.texture.clone(), mask.version, threshold))
            });
            // A layer's own effect replaces its frost; without a program yet
            // it falls back to frost if its rule also blurs.
            if let Some(element) = self.custom_shader_last_stage(
                renderer,
                &surface,
                (id.clone(), version, &texture),
                physical_rect,
                None,
                alpha_mask.as_ref(),
            ) {
                result.entry(surface).or_insert_with(Vec::new).push(
                    crate::backend::udev::OutputRenderElements::CustomShader(element),
                );
                continue;
            }
            let Some(program) = program.as_ref().filter(|_| blur) else {
                continue;
            };
            let commit_mask = alpha_mask
                .as_ref()
                .map(|(_, version, threshold)| (*version, *threshold));
            let mut element = crate::frost_glass::FrostGlassElement::new(
                id,
                crate::frost_glass::frost_glass_commit(
                    version,
                    &frost,
                    corner_radii,
                    rounding_power,
                    corner_softness,
                    commit_mask,
                ),
                texture,
                physical_rect,
                program.clone(),
                frost.clone(),
                corner_radii,
                rounding_power,
                corner_softness,
            );
            if let Some((mask, _, threshold)) = alpha_mask {
                element = element.with_alpha_mask(mask, threshold);
            }
            result.entry(surface).or_insert_with(Vec::new).push(
                crate::backend::udev::OutputRenderElements::FrostGlass(element),
            );
        }
        result
    }
}
