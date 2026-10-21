//! hyprland-toplevel-export-v1: capture of a single window, addressed by its
//! `zwlr_foreign_toplevel_handle_v1`. Quickshell's `ScreencopyView` (window
//! previews in a task switcher or overview) and other shell tools use it for
//! per-window capture; the ext-image-copy-capture toplevel source in
//! `handlers/capture.rs` is the standard route for clients that speak it.
//!
//! Hand-rolled on bindings generated in `src/protocols.rs`. Only the
//! wlr-handle request (version 2) can address a TideWM window; the version-1
//! request names a Hyprland window address, which does not exist here, and
//! fails its frame. Frames are SHM-only and go through the same queue,
//! render, privacy and lock rules as every other capture (`src/capture.rs`).

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::{
    output::Output,
    reexports::wayland_server::{
        protocol::{wl_shm, wl_surface::WlSurface},
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
    },
    utils::{Buffer as BufferCoords, Size},
    wayland::shm::with_buffer_contents,
};

use crate::{
    capture::{CaptureCompletion, PendingCapture},
    protocols::hyprland_toplevel_export::server::{
        hyprland_toplevel_export_frame_v1::{self, HyprlandToplevelExportFrameV1},
        hyprland_toplevel_export_manager_v1::{self, HyprlandToplevelExportManagerV1},
    },
    Smallvil,
};

/// Per-frame state carried from the capture request to `copy`.
pub struct ToplevelExportFrameData {
    /// `None` when the request named no live, capturable window; such frames
    /// fail on creation.
    target: Option<ToplevelExportTarget>,
    /// A frame is single-shot: only the first `copy` is acted on.
    copied: AtomicBool,
}

struct ToplevelExportTarget {
    surface: WlSurface,
    output: Output,
    size: Size<i32, BufferCoords>,
}

/// Whether an SHM buffer can hold a capture of `size`: an advertised format,
/// at least the advertised dimensions. Untrusted client input, so anything
/// else fails the frame rather than risking a bad write later.
fn shm_buffer_fits(
    width: i32,
    height: i32,
    format: wl_shm::Format,
    size: Size<i32, BufferCoords>,
) -> bool {
    width >= size.w
        && height >= size.h
        && matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888)
}

impl GlobalDispatch<HyprlandToplevelExportManagerV1, ()> for Smallvil {
    fn can_view(client: Client, _data: &()) -> bool {
        crate::state::trusted_client(&client)
    }

    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<HyprlandToplevelExportManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<HyprlandToplevelExportManagerV1, ()> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &HyprlandToplevelExportManagerV1,
        request: hyprland_toplevel_export_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let (frame, target) = match request {
            hyprland_toplevel_export_manager_v1::Request::CaptureToplevelWithWlrToplevelHandle {
                frame,
                handle,
                ..
            } => {
                // The session lock hides every window; toplevel capture has no
                // lock composition, so it is refused up front as well as at
                // render time.
                let target = (matches!(state.session_lock, crate::state::SessionLock::Unlocked))
                    .then(|| state.surface_for_wlr_toplevel(&handle))
                    .flatten()
                    .and_then(|surface| {
                        let (output, size) = state.window_capture_target(&surface)?;
                        Some(ToplevelExportTarget {
                            surface,
                            output,
                            size,
                        })
                    });
                (frame, target)
            }
            // A Hyprland window address: nothing in TideWM is named that way.
            hyprland_toplevel_export_manager_v1::Request::CaptureToplevel { frame, .. } => {
                (frame, None)
            }
            hyprland_toplevel_export_manager_v1::Request::Destroy => return,
        };

        let size = target.as_ref().map(|target| target.size);
        let frame = data_init.init(
            frame,
            ToplevelExportFrameData {
                target,
                copied: AtomicBool::new(false),
            },
        );
        match size {
            Some(size) => {
                frame.buffer(
                    wl_shm::Format::Argb8888,
                    size.w as u32,
                    size.h as u32,
                    (size.w * 4) as u32,
                );
                frame.buffer_done();
            }
            None => frame.failed(),
        }
    }
}

impl Dispatch<HyprlandToplevelExportFrameV1, ToplevelExportFrameData> for Smallvil {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &HyprlandToplevelExportFrameV1,
        request: hyprland_toplevel_export_frame_v1::Request,
        data: &ToplevelExportFrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let (buffer, ignore_damage) = match request {
            hyprland_toplevel_export_frame_v1::Request::Copy {
                buffer,
                ignore_damage,
            } => (buffer, ignore_damage != 0),
            hyprland_toplevel_export_frame_v1::Request::Destroy => return,
        };

        if data.copied.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(target) = &data.target else {
            resource.failed();
            return;
        };
        let fits = with_buffer_contents(&buffer, |_, _, meta| {
            shm_buffer_fits(meta.width, meta.height, meta.format, target.size)
        })
        .unwrap_or(false);
        if !fits {
            resource.failed();
            return;
        }

        state.queue_capture(PendingCapture {
            client_id: Some(client.id()),
            output: target.output.clone(),
            window: Some(target.surface.clone()),
            draw_cursor: false,
            region: None,
            completion: CaptureCompletion::HyprlandToplevel {
                frame: resource.clone(),
                buffer,
                report_damage: !ignore_damage,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ClientState;
    use smithay::{
        reexports::{calloop::EventLoop, wayland_server::Display},
        wayland::compositor::CompositorClientState,
    };
    use std::{
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixStream},
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    fn wire_request(peer: &mut UnixStream, object: u32, opcode: u16, words: &[u32], tail: &[u8]) {
        let size = 8 + words.len() * 4 + tail.len();
        let mut bytes = object.to_ne_bytes().to_vec();
        bytes.extend_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
        for word in words {
            bytes.extend_from_slice(&word.to_ne_bytes());
        }
        bytes.extend_from_slice(tail);
        peer.write_all(&bytes).unwrap();
    }

    fn wire_string(value: &str) -> Vec<u8> {
        let mut bytes = ((value.len() + 1) as u32).to_ne_bytes().to_vec();
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        bytes
    }

    fn wire_dispatch(
        event_loop: &mut EventLoop<'static, Smallvil>,
        state: &mut Smallvil,
        peer: &mut UnixStream,
    ) -> Vec<(u32, u16, Vec<u8>)> {
        event_loop.dispatch(Some(Duration::ZERO), state).unwrap();
        state.display_handle.flush_clients().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match peer.read(&mut buffer) {
                Ok(n) if n > 0 => bytes.extend_from_slice(&buffer[..n]),
                Ok(0) if !bytes.is_empty() => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected Wayland read: {other:?}"),
            }
        }
        let mut events = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let object = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let header = u32::from_ne_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            let size = (header >> 16) as usize;
            assert!(size >= 8 && offset + size <= bytes.len());
            let body = bytes[offset + 8..offset + size].to_vec();
            assert_ne!((object, header as u16), (1, 0), "protocol error: {body:?}");
            events.push((object, header as u16, body));
            offset += size;
        }
        events
    }

    /// A real client connection: the global is offered at version 2, and a
    /// version-1 capture by Hyprland window address -- which names nothing
    /// in TideWM -- fails its frame instead of erroring the client.
    #[test]
    fn global_is_offered_and_window_addresses_fail_cleanly() {
        // Smallvil loads config and creates a Wayland listener: run in a child
        // with private XDG directories, as layer_shell_test does.
        const CHILD: &str = "TIDEWM_TOPLEVEL_EXPORT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("tidewm-export-test-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
            std::fs::create_dir(root.join("runtime")).unwrap();
            std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            std::fs::write(
                root.join("config/tidewm/config.wave"),
                "welcome_hint = false\nwater_effects = false\n",
            )
            .unwrap();
            let test_name = concat!(
                module_path!(),
                "::global_is_offered_and_window_addresses_fail_cleanly"
            );
            let test_name = test_name.split_once("::").unwrap().1;
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_RUNTIME_DIR", root.join("runtime"))
                .output();
            std::fs::remove_dir_all(root).unwrap();
            let result = result.unwrap();
            assert!(String::from_utf8_lossy(&result.stdout).contains("running 1 test"));
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let mut event_loop = EventLoop::try_new().unwrap();
        let mut state = Smallvil::new(&mut event_loop, Display::new().unwrap());
        let (server, mut peer) = UnixStream::pair().unwrap();
        state
            .display_handle
            .clone()
            .insert_client(
                server,
                Arc::new(ClientState {
                    compositor_state: CompositorClientState::default(),
                    security_context: None,
                    disconnect_sender: None,
                }),
            )
            .unwrap();
        peer.set_nonblocking(true).unwrap();

        wire_request(&mut peer, 1, 1, &[2], &[]); // get_registry
        let (name, version) = wire_dispatch(&mut event_loop, &mut state, &mut peer)
            .into_iter()
            .find_map(|(object, opcode, body)| {
                if object != 2 || opcode != 0 {
                    return None;
                }
                let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
                let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
                let interface = String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap();
                let padded = (length + 3) & !3;
                let version = u32::from_ne_bytes(body[8 + padded..12 + padded].try_into().unwrap());
                (interface == "hyprland_toplevel_export_manager_v1").then_some((name, version))
            })
            .expect("hyprland_toplevel_export_manager_v1 is advertised");
        assert_eq!(version, 2);

        let mut tail = wire_string("hyprland_toplevel_export_manager_v1");
        tail.extend_from_slice(&2_u32.to_ne_bytes());
        tail.extend_from_slice(&3_u32.to_ne_bytes());
        wire_request(&mut peer, 2, 0, &[name], &tail); // bind as id 3
                                                       // capture_toplevel(frame = 4, overlay_cursor = 0, handle = 0xd161e7b0)
        wire_request(&mut peer, 3, 0, &[4, 0, 0xd161_e7b0], &[]);
        let events = wire_dispatch(&mut event_loop, &mut state, &mut peer);
        let frame_events: Vec<u16> = events
            .iter()
            .filter(|(object, _, _)| *object == 4)
            .map(|(_, opcode, _)| *opcode)
            .collect();
        assert_eq!(frame_events, [4], "only `failed`, no buffer offer");
    }

    #[test]
    fn buffer_must_be_large_enough_and_an_offered_format() {
        let size = Size::from((640, 360));
        assert!(shm_buffer_fits(640, 360, wl_shm::Format::Argb8888, size));
        assert!(shm_buffer_fits(800, 600, wl_shm::Format::Xrgb8888, size));
        assert!(!shm_buffer_fits(639, 360, wl_shm::Format::Argb8888, size));
        assert!(!shm_buffer_fits(640, 359, wl_shm::Format::Argb8888, size));
        assert!(!shm_buffer_fits(640, 360, wl_shm::Format::Rgb565, size));
    }
}
