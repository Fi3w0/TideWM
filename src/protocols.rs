//! Wayland protocols that no crate in the dependency tree ships, generated
//! from the XML vendored under `protocols/` the same way
//! `wayland-protocols-wlr` generates its own.

/// hyprland-toplevel-export-v1 (hyprwm/hyprland-protocols, BSD-3-Clause, see
/// `protocols/LICENSE-hyprland-protocols`): per-window capture addressed by a
/// `zwlr_foreign_toplevel_handle_v1`. Quickshell's `ScreencopyView` and other
/// shell/screenshot tools capture individual windows through it.
pub mod hyprland_toplevel_export {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
    #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
    #![allow(missing_docs, clippy::all)]

    pub mod server {
        use smithay::reexports::wayland_server;
        use smithay::reexports::wayland_server::protocol::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::server::*;

        pub mod __interfaces {
            use smithay::reexports::wayland_server::backend as wayland_backend;
            use smithay::reexports::wayland_server::protocol::__interfaces::*;
            use wayland_protocols_wlr::foreign_toplevel::v1::server::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_server_code!("protocols/hyprland-toplevel-export-v1.xml");
    }
}
