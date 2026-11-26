//! Output lifecycle shared by DRM monitors and virtual outputs.
//!
//! A virtual output is a compositor-only `Output`: a real `wl_output` global,
//! a place in the space with its own workspaces, but no display. Nothing
//! draws it on a vblank. It renders only through the capture path, when a
//! screencast or screenshot asks for it, and its clients get a throttled
//! frame callback otherwise so they do not freeze completely (niri throttles
//! invisible surfaces the same way). The pointer and directional monitor
//! navigation never land on one; windows and workspaces reach it by name.

use std::time::{Duration, Instant};

use smithay::{
    desktop::layer_map_for_output,
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            RegistrationToken,
        },
        wayland_server::backend::GlobalId,
    },
    utils::Transform,
};

use super::Smallvil;
use crate::output_layout::{logical_output_size, resolve_output_position};

/// How long a removed output's disabled `wl_output` global stays bindable,
/// so clients that raced a bind still find it (niri uses the same).
const OUTPUT_GLOBAL_REMOVE_DELAY: Duration = Duration::from_secs(10);

/// Frame callbacks for clients on an unwatched virtual output.
const VIRTUAL_FRAME_THROTTLE: Duration = Duration::from_millis(995);

/// Largest virtual mode edge, in pixels.
const VIRTUAL_MAX_EDGE: i32 = 16384;

/// Default virtual refresh, in millihertz.
const VIRTUAL_DEFAULT_REFRESH_MHZ: u32 = 60_000;

/// Who owns a virtual output's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VirtualOutputOrigin {
    /// A `virtual_output` config block: created at startup, reconciled on
    /// reload, removed when the block goes away.
    Config,
    /// An action or IPC call: lives until removed or TideWM exits.
    Runtime,
}

pub(crate) struct VirtualOutput {
    pub(crate) output: Output,
    global: GlobalId,
    pub(crate) origin: VirtualOutputOrigin,
    frame_timer: Option<RegistrationToken>,
    last_frames: Option<Instant>,
}

impl Smallvil {
    pub(crate) fn is_virtual_output(&self, output: &Output) -> bool {
        self.virtual_outputs
            .get(&output.name())
            .is_some_and(|virtual_output| virtual_output.output == *output)
    }

    /// Outputs a person can see, in space order.
    pub(crate) fn physical_outputs(&self) -> impl Iterator<Item = &Output> {
        self.space
            .outputs()
            .filter(|output| !self.virtual_outputs.contains_key(&output.name()))
    }

    fn next_virtual_output_name(&self) -> String {
        (1..)
            .map(|index| format!("VIRTUAL-{index}"))
            .find(|name| self.space.outputs().all(|output| output.name() != *name))
            .expect("an unbounded range always has a free name")
    }

    pub(crate) fn create_virtual_output(
        &mut self,
        name: Option<String>,
        (width, height): (i32, i32),
        refresh_mhz: Option<u32>,
        origin: VirtualOutputOrigin,
    ) -> Result<Output, String> {
        let name = name.unwrap_or_else(|| self.next_virtual_output_name());
        if !crate::config::valid_virtual_output_name(&name) {
            return Err(format!("invalid virtual output name {name:?}"));
        }
        if self.space.outputs().any(|output| output.name() == name) {
            return Err(format!("an output named {name} already exists"));
        }
        if !(1..=VIRTUAL_MAX_EDGE).contains(&width) || !(1..=VIRTUAL_MAX_EDGE).contains(&height) {
            return Err(format!(
                "mode {width}x{height} is outside 1..={VIRTUAL_MAX_EDGE} pixels per edge"
            ));
        }
        let refresh = refresh_mhz.unwrap_or(VIRTUAL_DEFAULT_REFRESH_MHZ);
        let mode = Mode {
            size: (width, height).into(),
            refresh: i32::try_from(refresh).map_err(|_| "refresh out of range".to_string())?,
        };
        let output_config = self
            .config
            .outputs
            .iter()
            .find(|config| config.name == name)
            .cloned();
        let scale = output_config.as_ref().map(|c| c.scale).unwrap_or(1.0);
        let transform: Transform = output_config
            .as_ref()
            .map(|c| c.transform)
            .unwrap_or_default()
            .to_transform();
        let logical_size = logical_output_size(mode.size, transform, scale)
            .ok_or_else(|| "mode and scale do not fit the logical desktop".to_string())?;
        let requested = output_config
            .as_ref()
            .and_then(|config| config.position)
            .map(Into::into);
        let (position, _) = resolve_output_position(
            self.space
                .outputs()
                .filter_map(|output| self.space.output_geometry(output)),
            requested,
            logical_size,
        )
        .ok_or_else(|| "no room for the output in the logical desktop".to_string())?;

        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "TideWM".into(),
                model: "Virtual".into(),
                serial_number: name.clone(),
            },
        );
        output.change_current_state(
            Some(mode),
            Some(transform),
            Some(Scale::Fractional(scale)),
            Some(position),
        );
        output.set_preferred(mode);
        let global = output.create_global::<Smallvil>(&self.display_handle);
        self.space.map_output(&output, position);

        let timer_name = name.clone();
        let frame_timer = self
            .loop_handle
            .insert_source(
                Timer::from_duration(VIRTUAL_FRAME_THROTTLE),
                move |_, _, state| {
                    state.throttled_virtual_output_frames(&timer_name);
                    TimeoutAction::ToDuration(VIRTUAL_FRAME_THROTTLE)
                },
            )
            .map_err(|error| tracing::warn!(%error, "Virtual output frame timer failed"))
            .ok();
        self.virtual_outputs.insert(
            name.clone(),
            VirtualOutput {
                output: output.clone(),
                global,
                origin,
                frame_timer,
                last_frames: None,
            },
        );
        self.output_layout_changed();
        tracing::info!(
            name,
            width,
            height,
            refresh_mhz = refresh,
            scale,
            ?origin,
            "Virtual output created"
        );
        Ok(output)
    }

    pub(crate) fn remove_virtual_output(&mut self, name: &str) -> Result<(), String> {
        let Some(virtual_output) = self.virtual_outputs.remove(name) else {
            return Err(format!("no virtual output named {name}"));
        };
        if let Some(token) = virtual_output.frame_timer {
            self.loop_handle.remove(token);
        }
        self.fail_captures_for_output(&virtual_output.output);
        self.disconnect_output(&virtual_output.output);
        self.retract_output_global(virtual_output.global);
        tracing::info!(name, "Virtual output removed");
        Ok(())
    }

    /// Creates, updates or removes config-owned virtual outputs to match
    /// `config.virtual_outputs`. Runtime ones are left alone unless a config
    /// block claims their name.
    pub(crate) fn reconcile_config_virtual_outputs(&mut self) {
        let wanted = self.config.virtual_outputs.clone();
        let stale: Vec<String> = self
            .virtual_outputs
            .iter()
            .filter(|(name, virtual_output)| {
                virtual_output.origin == VirtualOutputOrigin::Config
                    && !wanted.iter().any(|config| &config.name == *name)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            let _ = self.remove_virtual_output(&name);
        }
        for config in wanted {
            let refresh_mhz = config
                .refresh
                .map(|hz| (hz * 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32);
            if let Some(existing) = self.virtual_outputs.get_mut(&config.name) {
                existing.origin = VirtualOutputOrigin::Config;
                let output = existing.output.clone();
                let mode = Mode {
                    size: (config.width, config.height).into(),
                    refresh: refresh_mhz.unwrap_or(VIRTUAL_DEFAULT_REFRESH_MHZ) as i32,
                };
                if output.current_mode() != Some(mode) {
                    output.change_current_state(Some(mode), None, None, None);
                    output.set_preferred(mode);
                    self.output_layout_changed();
                }
                continue;
            }
            if self
                .physical_outputs()
                .any(|output| output.name() == config.name)
            {
                tracing::warn!(
                    name = config.name,
                    "A connected monitor already has this name; virtual output not created"
                );
                continue;
            }
            if let Err(error) = self.create_virtual_output(
                Some(config.name.clone()),
                (config.width, config.height),
                refresh_mhz,
                VirtualOutputOrigin::Config,
            ) {
                tracing::warn!(name = config.name, %error, "Could not create virtual output");
            }
        }
    }

    /// Publish an output set change to every consumer that lists outputs.
    fn output_layout_changed(&mut self) {
        self.refresh_input_device_outputs();
        #[cfg(feature = "screencast")]
        if let Some(screencast) = &self.screencast {
            screencast.refresh_outputs(self.space.outputs());
        }
        self.wlr_output_management_state.refresh(&self.space);
        self.refresh_ext_workspaces();
        self.retile();
    }

    /// Drains captures for every virtual output with the backend's renderer,
    /// then tells that output's clients a frame was shown.
    pub(crate) fn render_virtual_outputs(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        composite_cursor: bool,
    ) {
        if self.virtual_outputs.is_empty() || self.pending_captures.is_empty() {
            return;
        }
        let outputs: Vec<Output> = self
            .virtual_outputs
            .values()
            .map(|virtual_output| virtual_output.output.clone())
            .filter(|output| {
                self.pending_captures
                    .iter()
                    .any(|capture| &capture.output == output)
            })
            .collect();
        for output in outputs {
            self.render_pending_captures(renderer, &output, composite_cursor);
            self.send_virtual_output_frames(&output);
        }
    }

    fn throttled_virtual_output_frames(&mut self, name: &str) {
        let Some(virtual_output) = self.virtual_outputs.get(name) else {
            return;
        };
        if virtual_output
            .last_frames
            .is_some_and(|last| last.elapsed() < VIRTUAL_FRAME_THROTTLE)
        {
            return;
        }
        let output = virtual_output.output.clone();
        self.send_virtual_output_frames(&output);
    }

    fn send_virtual_output_frames(&mut self, output: &Output) {
        let time = self.start_time.elapsed();
        if matches!(self.session_lock, super::SessionLock::Unlocked) {
            self.send_window_frames(output, time);
            for layer in layer_map_for_output(output).layers() {
                if self.unmapped_layer_surfaces.contains(layer.wl_surface()) {
                    continue;
                }
                layer.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            }
        } else {
            self.send_lock_frames(output, time);
        }
        if let Some(virtual_output) = self.virtual_outputs.get_mut(&output.name()) {
            virtual_output.last_frames = Some(Instant::now());
        }
    }

    /// Everything that must happen when an output leaves the desktop, for a
    /// DRM monitor and a virtual output alike. Windows move to a monitor a
    /// person can see (any output only when no such monitor is left). The
    /// caller retracts the `wl_output` global afterwards.
    pub(crate) fn disconnect_output(&mut self, output: &Output) {
        let disconnected_name = output.name();
        self.remove_workspace_transition_output(&disconnected_name);
        self.remove_backdrop_output(&disconnected_name);
        let fallback: Option<String> = self
            .physical_outputs()
            .find(|o| o.name() != disconnected_name)
            .or_else(|| self.space.outputs().find(|o| o.name() != disconnected_name))
            .map(|o| o.name());
        self.spatial
            .ocean
            .remove_output(&disconnected_name, fallback.as_deref());
        if let Some(fallback) = fallback.as_deref() {
            self.migrate_output_windows(&disconnected_name, fallback);
        }
        self.remove_layer_output(output);
        self.space.unmap_output(output);
        // `unmap_output` only forgets the output; the wl_surface.leave events
        // go out on the next refresh. Send them now, before the global is
        // retracted.
        self.refresh_space();
        self.refresh_input_device_outputs();
        let space = &self.space;
        if let Some(theme) = self.input.cursor_theme.as_mut() {
            theme.retain_scales(|scale| {
                space
                    .outputs()
                    .any(|output| output.current_scale().integer_scale() == scale)
            });
        }
        self.remove_lock_output(output);
        self.render.gpu.layer_dim_buffers.remove(output);
        #[cfg(feature = "screencast")]
        if let Some(screencast) = &self.screencast {
            screencast.refresh_outputs(self.space.outputs());
        }
        self.wlr_output_management_state.refresh(&self.space);
        self.refresh_ext_workspaces();
        self.wlr_output_power_management_state
            .output_removed(output);
        self.wlr_gamma_control_state.output_removed(output);
        self.retile();
        self.refresh_pointer_focus();
        self.repair_keyboard_focus_after_output_change(
            fallback.as_deref(),
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
    }

    /// Retracts a removed output's `wl_output` global. Must run after every
    /// surface has left the output: a client that sees `global_remove`
    /// destroys its proxy, and a later `wl_surface.leave` naming it reaches
    /// the client as a null object (kitty/glfw segfaults on that). Disabling
    /// stops new binds and tells clients now; removal follows after a delay.
    pub(crate) fn retract_output_global(&mut self, global: GlobalId) {
        self.display_handle
            .disable_global::<Smallvil>(global.clone());
        let remover = self.display_handle.clone();
        let delayed = global.clone();
        let scheduled = self.loop_handle.insert_source(
            Timer::from_duration(OUTPUT_GLOBAL_REMOVE_DELAY),
            move |_, _, _| {
                remover.remove_global::<Smallvil>(delayed.clone());
                TimeoutAction::Drop
            },
        );
        if let Err(err) = scheduled {
            tracing::warn!(%err, "Failed to schedule wl_output global removal; removing now");
            self.display_handle.remove_global::<Smallvil>(global);
        }
    }
}
