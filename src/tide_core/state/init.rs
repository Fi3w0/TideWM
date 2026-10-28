use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    desktop::PopupManager,
    input::{pointer::CursorImageStatus, Seat, SeatState},
    utils::{Clock, Monotonic},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        cursor_shape::CursorShapeManagerState,
        dmabuf::DmabufState,
        foreign_toplevel_list::ForeignToplevelListState,
        fractional_scale::FractionalScaleManagerState,
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        image_capture_source::{
            ImageCaptureSourceState, OutputCaptureSourceState, ToplevelCaptureSourceState,
        },
        image_copy_capture::ImageCopyCaptureState,
        input_method::InputMethodManagerState,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        pointer_gestures::PointerGesturesState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        security_context::SecurityContextState,
        selection::{
            data_device::DataDeviceState, primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState,
        },
        session_lock::SessionLockManagerState,
        shell::{
            kde::decoration::KdeDecorationState,
            wlr_layer::WlrLayerShellState,
            xdg::{decoration::XdgDecorationState, XdgShellState},
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        tablet_manager::TabletManagerState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_activation::XdgActivationState,
        xdg_toplevel_icon::XdgToplevelIconManager,
    },
};

use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_manager_v1::WpTearingControlManagerV1;
use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use crate::{config::Config, layout::Layouts};

use super::{
    trusted_client, ClientState, KeyboardFocusTarget, SessionLock, Smallvil, SpatialState,
};

use smithay::{
    input::keyboard::XkbConfig,
    reexports::{
        calloop::{generic::Generic, EventLoop, Interest, Mode, PostAction},
        wayland_server::Display,
    },
    wayland::socket::ListeningSocketSource,
};

impl Smallvil {
    pub fn new(event_loop: &mut EventLoop<'static, Smallvil>, display: Display<Self>) -> Self {
        let start_time = std::time::Instant::now();
        let config_lua =
            crate::wave::new_lua().expect("creating the config Lua state must not fail");
        let tide = crate::wave::TideInfo {
            backend: "unknown", // main.rs sets backend_name after backend init
            gpu_vendor: crate::wave::detect_gpu_vendor(),
            ..Default::default()
        };
        let (config, startup_config_error, startup_config_warnings) =
            Config::load_with_error_in(&config_lua, &tide);
        // Copied out before `config` moves into the `Self { config, .. }`
        // field below, so `layout: ...` further down can still read it.
        let default_layout = config.default_layout;
        let master_orientation = config.master_orientation;
        let bsp_split_bias = config.bsp_split_bias;
        let workspace_algorithm_overrides = config.workspace_algorithm_overrides();
        let ui_theme = crate::ui_theme::UiTheme::from_config(&config);
        let ocean = if config.spatial_engine.ownership() == crate::config::SpatialOwnership::Ocean {
            crate::ocean::OceanSpace::from_config(&config.ocean)
        } else {
            crate::ocean::OceanSpace::default()
        };

        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let kde_decoration_state = KdeDecorationState::new::<Self>(
            &dh,
            smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode::Server,
        );
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let data_control_state =
            DataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), trusted_client);
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&dh, trusted_client);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let mut xdg_toplevel_icon_manager = XdgToplevelIconManager::new::<Self>(&dh);
        // Advertise the sizes TideWM-owned consumers are most likely to use.
        // Clients remain free to submit other sizes as required by the
        // protocol; these are preferences, not limits.
        xdg_toplevel_icon_manager.replace_icon_sizes([32, 48, 64, 128]);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&dh);
        let clock = Clock::<Monotonic>::new();
        let presentation_state = PresentationState::new::<Self>(&dh, clock.id() as u32);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let foreign_toplevel_list_state =
            ForeignToplevelListState::new_with_filter::<Self>(&dh, trusted_client);
        // The older wlr-foreign-toplevel-management-v1 protocol (hand-rolled,
        // no Smithay module). See `handlers/wlr_foreign_toplevel.rs`.
        let wlr_foreign_toplevel_state =
            crate::handlers::wlr_foreign_toplevel::WlrForeignToplevelState::new(&dh);
        let wlr_output_management_state =
            crate::handlers::wlr_output_management::WlrOutputManagementState::new(&dh);
        let wlr_output_power_management_state =
            crate::handlers::wlr_output_power_management::WlrOutputPowerManagementState::new(&dh);
        let wlr_gamma_control_state =
            crate::handlers::wlr_gamma_control::WlrGammaControlState::new(&dh);
        let ext_workspace_state = crate::handlers::ext_workspace::ExtWorkspaceState::new(&dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        let pointer_gestures_state = PointerGesturesState::new::<Self>(&dh);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&dh);
        let text_input_manager_state = TextInputManagerState::new::<Self>(&dh);
        let input_method_manager_state =
            InputMethodManagerState::new::<Self, _>(&dh, trusted_client);
        let virtual_keyboard_manager_state =
            VirtualKeyboardManagerState::new::<Self, _>(&dh, trusted_client);
        let _security_context_state = SecurityContextState::new::<Self, _>(&dh, trusted_client);
        let popups = PopupManager::default();
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let tablet_manager_state = TabletManagerState::new::<Self>(&dh);
        let image_capture_source_state = ImageCaptureSourceState::new();
        let output_capture_source_state =
            OutputCaptureSourceState::new_with_filter::<Self, _>(&dh, trusted_client);
        let toplevel_capture_source_state =
            ToplevelCaptureSourceState::new_with_filter::<Self, _>(&dh, trusted_client);
        let image_copy_capture_state =
            ImageCopyCaptureState::new_with_filter::<Self, _>(&dh, trusted_client);
        let wlr_screencopy_global = dh.create_global::<Self, ZwlrScreencopyManagerV1, ()>(3, ());
        let tearing_control_global = dh.create_global::<Self, WpTearingControlManagerV1, ()>(1, ());
        let hyprland_toplevel_export_global = dh.create_global::<
            Self,
            crate::protocols::hyprland_toplevel_export::server::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1,
            (),
        >(2, ());

        // A seat is a group of keyboards, pointer and touch devices.
        // A seat typically has a pointer and maintains a keyboard focus and a pointer focus.
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        // Notify clients that we have a keyboard, for the sake of the example we assume that keyboard is always present.
        // You may want to track keyboard hot-plug in real compositor.
        //
        // A bad `xkb_layout`/`xkb_variant`/`xkb_options` in the user's
        // config must not take the whole compositor down with it -- fall
        // back to the default keymap (which always compiles) and log it,
        // rather than unwrapping a config-controlled `Result`.
        if seat
            .add_keyboard(
                config.input.xkb_config(),
                config.input.repeat_delay,
                config.input.repeat_rate,
            )
            .is_err()
        {
            tracing::error!(
                layout = %config.input.xkb_layout,
                variant = %config.input.xkb_variant,
                options = ?config.input.xkb_options,
                "Configured XKB keymap failed to compile, falling back to default layout"
            );
            seat.add_keyboard(
                XkbConfig::default(),
                config.input.repeat_delay,
                config.input.repeat_rate,
            )
            .expect("default XKB keymap must always compile");
        }

        // Notify clients that we have a pointer (mouse)
        // Here we assume that there is always pointer plugged in
        seat.add_pointer();

        // Advertise touch unconditionally too, matching pointer/keyboard
        // above and the same "just advertise it" precedent `PointerGesturesState`
        // already uses for a capability that may or may not have real
        // hardware behind it -- a client simply never gets touch events if
        // there's no touch panel, harmless either way.
        seat.add_touch();

        // A space represents a two-dimensional plane. Windows and Outputs can be mapped onto it.
        //
        // Windows get a position and stacking order through mapping.
        // Outputs become views of a part of the Space and can be rendered via Space::render_output.
        let space = crate::tide_core::window_space::WindowSpace::default();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        // Get the loop signal, used to stop the event loop
        let loop_handle = event_loop.handle();
        let loop_signal = event_loop.get_signal();

        let idle_inhibit_manager_state = IdleInhibitManagerState::new::<Self>(&dh);
        let idle_notifier_state = IdleNotifierState::<Self>::new(&dh, loop_handle.clone());

        let (depth_wake, depth_source) =
            smithay::reexports::calloop::ping::make_ping().expect("creating depth artwork wakeup");
        loop_handle
            .insert_source(depth_source, |(), _, state: &mut Smallvil| {
                state.poll_depth_background();
            })
            .expect("registering depth artwork wakeup");

        Self {
            start_time,
            display_handle: dh,
            xwayland_satellite_pid: None,
            xwayland_display: None,
            backend_name: "unknown",
            classic_tile_drag: None,
            native_render_stats: HashMap::new(),
            config_lua,
            tide,
            handler_depth: 0,
            config_warnings: startup_config_warnings.clone(),

            spatial: SpatialState {
                engine: config.spatial_engine,
            },
            config,
            config_watcher: None,
            known_input_devices: Vec::new(),
            input_device_outputs: HashMap::new(),
            compositor_gesture: None,
            toast: None,
            config_error_overlay: startup_config_error
                .map(|message| {
                    crate::error_overlay::ConfigErrorOverlay::new(
                        message,
                        crate::error_overlay::OverlaySeverity::Error,
                        ui_theme,
                    )
                })
                .or_else(|| {
                    (!startup_config_warnings.is_empty()).then(|| {
                        crate::error_overlay::ConfigErrorOverlay::new(
                            startup_config_warnings.join("; "),
                            crate::error_overlay::OverlaySeverity::Warning,
                            ui_theme,
                        )
                    })
                }),
            builtin_wallpaper: crate::wallpaper::BuiltinWallpaper::build(),
            overview: None,
            minimap_peek: None,
            minimap_trigger_down: false,
            suppressed_pointer_buttons: HashSet::new(),
            classic_depth: crate::classic_depth::ClassicDepthDeck::default(),
            ocean,
            depth_deck_overlay: None,
            #[cfg(feature = "screencast")]
            screencast_picker: None,
            welcome_hint: None,
            last_config_event: Instant::now() - Duration::from_secs(1),
            config_reload_timer_armed: false,
            last_pointer_motion: Instant::now(),
            last_activity: Instant::now(),
            keyboard_layout_state: None,
            cursor_idle_timer_armed: false,
            redraw_requests: crate::redraw::RedrawRequests::global(),
            redraw_wakeup: None,
            active_mode: None,
            rescue_keybinds_active: false,
            helper_keys_down: HashSet::new(),

            ipc_subscribers: HashMap::new(),
            ipc_seen: None,
            ipc_flush_token: None,
            next_ipc_subscriber_id: 1,

            layout: {
                let mut layout = Layouts::default();
                layout.set_default_algorithm(default_layout);
                layout.set_master_orientation(master_orientation);
                layout.set_split_bias(bsp_split_bias);
                layout.set_workspace_algorithm_overrides(workspace_algorithm_overrides);
                layout
            },
            space,
            window_opacity: HashMap::new(),
            window_glass_modes: HashMap::new(),
            window_shader_assignments: HashMap::new(),
            layer_shader_assignments: HashMap::new(),
            custom_shader_compile_budget: 0,
            custom_shader_programs: Default::default(),
            custom_shader_instances: HashMap::new(),
            custom_shader_bypassed: HashSet::new(),
            custom_shader_bypasses: 0,
            backdrop_textures: HashMap::new(),
            layer_alpha_masks: HashMap::new(),
            layer_dim_buffers: HashMap::new(),
            water_glass_program: None,
            glass_anim: HashMap::new(),
            frost_glass_program: None,
            shadow_program: None,
            rounded_surface_program: None,
            border_program: None,
            ripples: Vec::new(),
            ripple_program: None,
            window_open_animations: HashMap::new(),
            cascade_window_animations: HashMap::new(),
            scroll_pointer_refresh_pending: false,
            window_move_animations: HashMap::new(),
            window_viscosity: HashMap::new(),
            window_sway: HashMap::new(),
            window_float_physics: HashMap::new(),
            window_float_ambient: HashMap::new(),
            window_float_bodies: HashMap::new(),
            float_physics_accum: 0.0,
            float_physics_last_tick: Instant::now(),
            float_physics_wave_clock: Instant::now(),
            window_currents: HashMap::new(),
            floating_dragging: HashSet::new(),
            snap_preview: None,
            window_buoyancy: HashMap::new(),
            buoyancy_dirty: true,
            buoyancy_camera_revision: 0,
            buoyancy_camera_was_moving: false,
            swim_cameras: HashMap::new(),
            workspace_scroll_cameras: HashMap::new(),
            window_frame_snapshots: HashMap::new(),
            closing_window_animations: Vec::new(),
            window_depths: HashMap::new(),
            depth_schematics: HashMap::new(),
            depth_background: crate::depth_background::DepthBackground::new(depth_wake),
            depth_background_timer: None,
            depth_background_deadline: None,
            depth_overlay_program: None,
            depth_last_tick: Instant::now(),
            pending_workspace_transitions: HashMap::new(),
            workspace_transitions: HashMap::new(),
            workspace_glides: HashMap::new(),
            workspace_transition_program: None,
            depth_transitions: HashMap::new(),
            depth_transition_program: None,
            ocean_canvases: HashMap::new(),
            ocean_canvas_program: None,
            caustics: HashMap::new(),
            caustics_program: crate::caustics::CausticsProgramCache::default(),
            compasses: HashMap::new(),
            compass_program: None,
            loop_handle,
            loop_signal,
            socket_name,

            compositor_state,
            drm_syncobj_state: None,
            dmabuf_blocker_sources: HashMap::new(),
            dmabuf_blocker_source_count: 0,
            xdg_shell_state,
            xdg_decoration_state,
            kde_decoration_state,
            layer_shell_state,
            shm_state,
            output_manager_state,
            pointer_constraints_state,
            relative_pointer_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            data_control_state,
            session_lock_state,
            session_lock: SessionLock::Unlocked,
            session_lock_client: None,
            lock_surfaces: HashMap::new(),
            locked_outputs: HashSet::new(),
            lock_blank: HashMap::new(),
            xdg_activation_state,
            xdg_toplevel_icon_manager,
            single_pixel_buffer_state,
            presentation_state,
            clock,
            fractional_scale_manager_state,
            foreign_toplevel_list_state,
            foreign_toplevels: HashMap::new(),
            foreign_toplevel_numeric_ids: HashMap::new(),
            next_foreign_toplevel_numeric_id: 1,
            wlr_foreign_toplevel_state: Some(wlr_foreign_toplevel_state),
            ext_workspace_state,
            wlr_output_management_state,
            wlr_output_power_management_state,
            set_output_power: None,
            wlr_gamma_control_state,
            gamma_size: None,
            set_gamma: None,
            wlr_foreign_toplevels: HashMap::new(),
            keyboard_shortcuts_inhibit_state,
            pointer_gestures_state,
            cursor_shape_manager_state,
            text_input_manager_state,
            input_method_manager_state,
            virtual_keyboard_manager_state,
            virtual_keyboard_keymap: None,
            configured_keymap: None,
            popups,
            unmapped_popup_surfaces: HashSet::new(),
            popup_grab: None,
            unmapped_toplevels: HashMap::new(),
            unmapped_layer_surfaces: HashSet::new(),
            viewporter_state,
            tablet_manager_state,
            image_capture_source_state,
            output_capture_source_state,
            toplevel_capture_source_state,
            image_copy_capture_state,
            wlr_screencopy_global,
            hyprland_toplevel_export_global,
            tearing_control_global,
            capture_sessions: Vec::new(),
            pending_captures: Vec::new(),
            idle_inhibit_manager_state,
            idle_notifier_state,
            idle_inhibitors: HashMap::new(),
            seat,
            window_focus: None,
            on_demand_layer_focus: None,
            keyboard_focus: KeyboardFocusTarget::None,

            dmabuf_state: DmabufState::new(),
            native_dmabuf_feedback: None,
            dmabuf_global: None,
            udev_gpu: None,
            session: None,
            cursor_status: CursorImageStatus::default_named(),
            dnd_icon: None,
            cursor_theme: None,
            fullscreen: HashMap::new(),
            maximized: HashMap::new(),
            floating_workspace: HashMap::new(),
            pinned: HashSet::new(),
            scratchpad_previous: HashMap::new(),
            scratchpad_named: HashMap::new(),
            swallowed: HashMap::new(),
            lifecycle_flutter: HashMap::new(),
            expected_tiled_size: HashMap::new(),
            tiled_size_refusals: HashMap::new(),
            flutter_floated: HashSet::new(),
            initial_toplevel_identity: HashMap::new(),
            remembered_floating_sizes: HashMap::new(),
            workspace_previous: HashMap::new(),
            workspace_step_output: None,
            pseudo_tiled: HashSet::new(),
            urgent: HashSet::new(),
            urgent_pulse_last: HashMap::new(),
            urgent_pulse_tick: Instant::now(),
            window_tags: HashMap::new(),
            output_pins: HashMap::new(),
            workspace_created_empty_fired: HashSet::new(),
            focus_history: Vec::new(),
            cycling_focus: false,
            is_lid_closed: false,
            is_tablet_mode: false,
            groups: Vec::new(),
            next_ui_node_id: 1_000,
            #[cfg(feature = "screencast")]
            screencast: None,
            #[cfg(feature = "accessibility")]
            accessibility: None,
            #[cfg(feature = "accessibility")]
            accessibility_sync_timer_armed: false,
        }
    }

    fn init_wayland_listener(
        display: Display<Smallvil>,
        event_loop: &mut EventLoop<Smallvil>,
    ) -> OsString {
        // Creates a new listening socket, automatically choosing the next available `wayland` socket name.
        let listening_socket = ListeningSocketSource::new_auto().unwrap();

        // Get the name of the listening socket.
        // Clients will connect to this socket.
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();

        let (disconnect_sender, disconnect_source) =
            smithay::reexports::calloop::channel::channel();
        loop_handle
            .insert_source(disconnect_source, |event, _, state: &mut Smallvil| {
                if let smithay::reexports::calloop::channel::Event::Msg(client_id) = event {
                    state.handle_client_disconnect(client_id);
                }
            })
            .expect("Failed to init the client-disconnect event source.");

        let sender_for_clients = disconnect_sender.clone();
        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                // Inside the callback, you should insert the client into the display.
                //
                // You may also associate some data with the client when inserting the client.
                if let Err(err) = state.display_handle.insert_client(
                    client_stream,
                    Arc::new(ClientState {
                        compositor_state: CompositorClientState::default(),
                        security_context: None,
                        disconnect_sender: Some(sender_for_clients.clone()),
                    }),
                ) {
                    tracing::warn!(%err, "Failed to register incoming Wayland client");
                }
            })
            .expect("Failed to init the wayland event source.");

        // You also need to add the display itself to the event loop, so that client events will be processed by wayland-server.
        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: we don't drop the display
                    if let Err(err) = unsafe { display.get_mut().dispatch_clients(state) } {
                        tracing::error!(%err, "Wayland display dispatch failed; stopping cleanly");
                        state.loop_signal.stop();
                        return Ok(PostAction::Remove);
                    }
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }
}
