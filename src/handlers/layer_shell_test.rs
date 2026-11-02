//! Exercise real layer commits against Smallvil, without a renderer or DRM seat.
use crate::{state::ClientState, Smallvil};
use smithay::{
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_server::{Display, Resource},
    },
    utils::Point,
    wayland::compositor::CompositorClientState,
};
use std::{
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn string(value: &str) -> Vec<u8> {
    let mut bytes = ((value.len() + 1) as u32).to_ne_bytes().to_vec();
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes
}

struct Fixture {
    event_loop: EventLoop<'static, Smallvil>,
    state: Smallvil,
    peer: UnixStream,
}

impl Fixture {
    fn request(&mut self, object: u32, opcode: u16, words: &[u32], tail: &[u8]) {
        let size = 8 + words.len() * 4 + tail.len();
        let mut bytes = object.to_ne_bytes().to_vec();
        bytes.extend_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
        for word in words {
            bytes.extend_from_slice(&word.to_ne_bytes());
        }
        bytes.extend_from_slice(tail);
        self.peer.write_all(&bytes).unwrap();
    }

    fn dispatch(&mut self) -> Vec<(u32, u16, Vec<u8>)> {
        self.event_loop
            .dispatch(Some(Duration::ZERO), &mut self.state)
            .unwrap();
        self.state.display_handle.flush_clients().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match self.peer.read(&mut buffer) {
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
}

#[test]
fn wallpaper_frames_preserve_drag_and_exclusive_zone_changes_retile() {
    // Smallvil loads config and creates a Wayland listener. Run the fixture
    // in a child with private XDG directories rather than mutate process-wide
    // environment or read/write the developer's live configuration.
    const CHILD: &str = "TIDEWM_LAYER_COMMIT_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("tidewm-layer-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), "engine = classic\nwelcome_hint = false\nwater_effects = false\nanimations {\n    enabled = false\n}\n").unwrap();
        let test_name = concat!(
            module_path!(),
            "::wallpaper_frames_preserve_drag_and_exclusive_zone_changes_retile"
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
    let state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let (server, peer) = UnixStream::pair().unwrap();
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
    let mut fixture = Fixture {
        event_loop,
        state,
        peer,
    };
    let outputs: Vec<_> = ["wallpaper-output", "drag-output"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let output = Output::new(
                name.into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                    serial_number: "test".into(),
                },
            );
            output.change_current_state(
                Some(Mode {
                    size: (1801, 1001).into(),
                    refresh: 71_347,
                }),
                None,
                Some(if index == 0 {
                    Scale::Integer(1)
                } else {
                    Scale::Fractional(1.25)
                }),
                None,
            );
            output.create_global::<Smallvil>(&fixture.state.display_handle);
            fixture
                .state
                .space
                .map_output(&output, (index as i32 * 1801, 0));
            output
        })
        .collect();

    fixture.request(1, 1, &[2], &[]); // get_registry
    let globals: Vec<_> = fixture
        .dispatch()
        .into_iter()
        .filter_map(|(object, opcode, body)| {
            if object != 2 || opcode != 0 {
                return None;
            }
            let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
            let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            Some((
                String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap(),
                name,
            ))
        })
        .collect();
    for (interface, id, version) in [
        ("wl_compositor", 3_u32, 4_u32),
        ("xdg_wm_base", 4, 1),
        ("zwlr_layer_shell_v1", 5, 1),
        ("wp_single_pixel_buffer_manager_v1", 6, 1),
        ("wl_output", 7, 3),
    ] {
        let name = globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap()
            .1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[name], &tail);
    }
    fixture.dispatch();
    for surface in [8, 11] {
        fixture.request(3, 0, &[surface], &[]);
        fixture.request(4, 2, &[surface + 1, surface], &[]);
        fixture.request(surface + 1, 1, &[surface + 2], &[]);
    }
    fixture.dispatch();
    let windows: Vec<_> = fixture.state.unmapped_toplevels.values().cloned().collect();
    assert_eq!(windows.len(), 2);
    for window in &windows {
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .insert(&outputs[1].name(), 1, window.clone(), None);
    }
    fixture.state.retile();

    // A real layer-shell wallpaper on the OTHER output, including its
    // initial empty commit/configure/ack/buffer-map handshake.
    fixture.request(3, 0, &[14], &[]);
    fixture.request(5, 0, &[15, 14, 7, 0], &string("test-wallpaper"));
    fixture.request(15, 1, &[15], &[]); // anchor all edges
    fixture.request(14, 6, &[], &[]);
    let serial = fixture
        .dispatch()
        .into_iter()
        .find(|(object, opcode, _)| *object == 15 && *opcode == 0)
        .map(|(_, _, body)| u32::from_ne_bytes(body[..4].try_into().unwrap()))
        .unwrap();
    fixture.request(15, 6, &[serial], &[]);
    fixture.request(6, 1, &[16, 0, 0, 0, u32::MAX], &[]);
    fixture.request(14, 1, &[16, 0, 0], &[]);
    fixture.request(14, 6, &[], &[]);
    fixture.dispatch();

    // Match TileMoveGrab's visual move/raise while keeping layout membership.
    let dragged = &windows[0];
    let location: Point<i32, _> = (1978, 137).into();
    fixture
        .state
        .space
        .map_element(dragged.clone(), location, false);
    fixture.state.take_redraw_requests();
    for _ in 0..8 {
        fixture.request(14, 2, &[0, 0, 1, 1], &[]); // damage next frame
        fixture.request(14, 6, &[], &[]);
        fixture.dispatch();
        assert_eq!(
            fixture.state.space.element_location(dragged),
            Some(location),
            "a wallpaper frame snapped the drag back"
        );
        assert_eq!(
            fixture.state.space.elements().last(),
            Some(dragged),
            "a wallpaper frame changed drag stacking"
        );
        let redraw = fixture.state.take_redraw_requests();
        assert!(redraw.includes(&outputs[0]));
        assert!(
            !redraw.includes(&outputs[1]),
            "a wallpaper frame dirtied the other output"
        );
    }

    // A panel changes the reserved area without changing its configured size:
    // checking only LayerMap::arrange()'s return value would miss this case.
    fixture.request(15, 1, &[13], &[]); // top/left/right
    fixture.request(15, 0, &[0, 40], &[]);
    fixture.request(15, 2, &[40], &[]);
    fixture.request(14, 6, &[], &[]);
    fixture.dispatch();
    assert_eq!(
        fixture.state.output_tiling_area(&outputs[0]).unwrap().loc.y,
        40
    );
    fixture
        .state
        .space
        .map_element(dragged.clone(), location, false);
    fixture.request(15, 2, &[67], &[]);
    fixture.request(14, 6, &[], &[]);
    fixture.dispatch();
    assert_eq!(
        fixture.state.output_tiling_area(&outputs[0]).unwrap().loc.y,
        67
    );
    assert_ne!(
        fixture.state.space.element_location(dragged),
        Some(location),
        "an exclusive-zone change must still retile"
    );

    // Null-buffer unmap releases the reservation; a fresh initial commit
    // and configure acknowledgement must still allow the layer to remap.
    fixture.request(14, 1, &[0, 0, 0], &[]);
    fixture.request(14, 6, &[], &[]);
    fixture.dispatch();
    assert_eq!(fixture.state.unmapped_layer_surfaces.len(), 1);
    assert_eq!(
        fixture.state.output_tiling_area(&outputs[0]).unwrap().loc.y,
        0
    );
    fixture.request(15, 1, &[15], &[]);
    fixture.request(14, 6, &[], &[]);
    let serial = fixture
        .dispatch()
        .into_iter()
        .find(|(object, opcode, _)| *object == 15 && *opcode == 0)
        .map(|(_, _, body)| u32::from_ne_bytes(body[..4].try_into().unwrap()))
        .unwrap();
    fixture.request(15, 6, &[serial], &[]);
    fixture
        .state
        .space
        .map_element(dragged.clone(), location, false);
    fixture.request(14, 1, &[16, 0, 0], &[]);
    fixture.request(14, 6, &[], &[]);
    fixture.dispatch();
    assert!(fixture.state.unmapped_layer_surfaces.is_empty());
    assert_ne!(
        fixture.state.space.element_location(dragged),
        Some(location)
    );
    // A live drag may overlap another monitor without changing layout
    // ownership. Clearing the preview restores ordinary ownership filtering.
    let surface = dragged.toplevel().unwrap().wl_surface().clone();
    fixture.state.unmapped_toplevels.remove(&surface);
    let id = surface.id().protocol_id();
    let serial = dragged.toplevel().unwrap().send_configure();
    fixture.request(id + 1, 4, &[serial.into()], &[]);
    fixture.request(id, 1, &[16, 0, 0], &[]);
    fixture.request(id, 6, &[], &[]);
    fixture.dispatch();
    let destination = &outputs[0];
    let destination_geo = fixture.state.space.output_geometry(destination).unwrap();
    fixture
        .state
        .space
        .map_element(dragged.clone(), destination_geo.loc, false);
    fixture.state.refresh_space();
    fixture.state.input.classic_tile_drag = Some(surface.clone());
    assert!(fixture
        .state
        .render_placements(destination)
        .unwrap()
        .iter()
        .any(|p| p.surface() == Some(&surface)));
    fixture.state.input.classic_tile_drag = None;
    assert!(!fixture
        .state
        .render_placements(destination)
        .unwrap()
        .iter()
        .any(|p| p.surface() == Some(&surface)));
    fixture.state.retile();
    assert!(fixture
        .state
        .move_tiled_window_to_output(&surface, destination, None));
    fixture.state.retile();
    assert_eq!(
        fixture.state.spatial.workspaces.layout.output_of(&surface),
        Some(destination.name().as_str())
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .workspace_of(&surface),
        Some(
            fixture
                .state
                .spatial
                .workspaces
                .layout
                .active_workspace(&destination.name())
        )
    );
    assert!(!fixture
        .state
        .render_placements(&outputs[1])
        .unwrap()
        .iter()
        .any(|p| p.surface() == Some(&surface)));
    let other = windows[1].toplevel().unwrap().wl_surface().clone();
    assert!(fixture
        .state
        .move_tiled_window_to_output(&other, destination, None));
    fixture.state.retile();
    fixture.state.group_with(&surface, &other);
    let index = fixture.state.group_of(&surface).unwrap();
    assert_eq!(fixture.state.groups[index].members.len(), 2);
    fixture
        .state
        .spatial
        .workspaces
        .layout
        .set_active_workspace(&outputs[1].name(), 2);
    fixture
        .state
        .spatial
        .workspaces
        .output_pins
        .insert(other.clone(), destination.name());
    assert!(fixture
        .state
        .move_tiled_window_to_output(&surface, &outputs[1], None));
    fixture.state.retile();
    let index = fixture.state.group_of(&surface).unwrap();
    assert_eq!(fixture.state.groups[index].output, outputs[1].name());
    assert_eq!(fixture.state.groups[index].workspace, 2);
    assert_eq!(
        fixture.state.spatial.workspaces.output_pins.get(&other),
        Some(&outputs[1].name())
    );
    assert!(
        !fixture.state.spatial.workspaces.layout.contains(&other),
        "parked tabs must stay outside the tree"
    );
    fixture.state.ungroup(&other);
    assert_eq!(
        fixture.state.spatial.workspaces.layout.output_of(&other),
        Some(outputs[1].name().as_str())
    );
    assert_eq!(
        fixture.state.spatial.workspaces.layout.workspace_of(&other),
        Some(2)
    );
    crate::ipc::navigation_regression(&mut fixture.state, &windows, &outputs[1]);
    if let Some(node) = std::env::var_os("TIDEWM_SYNC_TEST_NODE") {
        native_syncobj_commit_lifecycle(&mut fixture, &surface, &node);
    }
}

fn native_syncobj_commit_lifecycle(
    fixture: &mut Fixture,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    node: &std::ffi::OsStr,
) {
    use smithay::{
        backend::{
            allocator::{
                dmabuf::AsDmabuf,
                gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
                Allocator, Fourcc, Modifier,
            },
            drm::DrmDeviceFd,
            renderer::utils::RendererSurfaceStateUserData,
        },
        reexports::{drm::control::Device, wayland_server::protocol::wl_buffer::WlBuffer},
        utils::DeviceFd,
        wayland::{
            compositor::{with_states, BufferAssignment, SurfaceAttributes},
            drm_syncobj::{supports_syncobj_eventfd, DrmSyncobjState},
        },
    };
    use std::os::fd::{AsRawFd, OwnedFd};
    let fd: OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .unwrap()
        .into();
    let device = DrmDeviceFd::new(DeviceFd::from(fd));
    assert!(
        supports_syncobj_eventfd(&device),
        "requested test device lacks syncobj eventfd"
    );
    let syncobj = device.create_syncobj(false).unwrap();
    let timeline_fd = device.syncobj_to_fd(syncobj, false).unwrap();
    fixture.state.drm_syncobj_state = Some(DrmSyncobjState::new::<Smallvil>(
        &fixture.state.display_handle,
        device.clone(),
    ));
    let name = fixture
        .dispatch()
        .into_iter()
        .find_map(|(object, opcode, body)| {
            if object != 2 || opcode != 0 {
                return None;
            }
            let len = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            (std::str::from_utf8(&body[8..8 + len - 1]).unwrap()
                == "wp_linux_drm_syncobj_manager_v1")
                .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
        })
        .unwrap();
    let mut tail = string("wp_linux_drm_syncobj_manager_v1");
    tail.extend_from_slice(&1_u32.to_ne_bytes());
    tail.extend_from_slice(&17_u32.to_ne_bytes());
    fixture.request(2, 0, &[name], &tail);
    fixture.request(17, 1, &[18, surface.id().protocol_id()], &[]);
    // Import the real kernel timeline through SCM_RIGHTS, as a client does.
    let mut wire = 17_u32.to_ne_bytes().to_vec();
    wire.extend_from_slice(&((12_u32 << 16) | 2).to_ne_bytes());
    wire.extend_from_slice(&19_u32.to_ne_bytes());
    unsafe {
        let space = libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as usize;
        let mut control = vec![0_usize; space.div_ceil(std::mem::size_of::<usize>())];
        let mut iov = libc::iovec {
            iov_base: wire.as_mut_ptr().cast(),
            iov_len: wire.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space;
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        libc::CMSG_DATA(header)
            .cast::<i32>()
            .write_unaligned(timeline_fd.as_raw_fd());
        assert_eq!(
            libc::sendmsg(fixture.peer.as_raw_fd(), &msg, libc::MSG_NOSIGNAL),
            wire.len() as isize
        );
    }
    fixture.dispatch();
    let gbm = GbmDevice::new(device.clone()).unwrap();
    let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
    let allocation = allocator
        .create_buffer(64, 64, Fourcc::Xrgb8888, &[Modifier::Invalid])
        .unwrap();
    let dmabuf = allocation.export().unwrap();
    let client = surface.client().unwrap();
    let buffer: WlBuffer = client
        .create_resource::<WlBuffer, _, Smallvil>(&fixture.state.display_handle, 1, dmabuf)
        .unwrap();
    let set_points = |fixture: &mut Fixture, acquire, release| {
        fixture.request(18, 1, &[19, 0, acquire], &[]);
        fixture.request(18, 2, &[19, 0, release], &[]);
        fixture.dispatch();
    };
    let serial = fixture
        .state
        .spatial
        .workspaces
        .layout
        .window_of(surface)
        .unwrap()
        .toplevel()
        .unwrap()
        .send_configure();
    fixture.request(surface.id().protocol_id() + 1, 4, &[serial.into()], &[]);
    fixture.dispatch();
    set_points(fixture, 1, 2);
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceAttributes>()
            .pending()
            .buffer = Some(BufferAssignment::NewBuffer(buffer.clone()))
    });
    fixture.request(surface.id().protocol_id(), 6, &[], &[]);
    fixture.dispatch();
    assert_eq!(
        fixture.state.dmabuf_blocker_source_count, 1,
        "unsignalled acquire must defer commit"
    );
    assert!(with_states(surface, |states| states
        .data_map
        .get::<RendererSurfaceStateUserData>()
        .unwrap()
        .lock()
        .unwrap()
        .buffer()
        .unwrap()
        != buffer));
    device.syncobj_timeline_signal(&[syncobj], &[1]).unwrap();
    fixture
        .event_loop
        .dispatch(Some(Duration::from_millis(20)), &mut fixture.state)
        .unwrap();
    fixture.dispatch();
    assert_eq!(fixture.state.dmabuf_blocker_source_count, 0);
    let in_flight = with_states(surface, |states| {
        states
            .data_map
            .get::<RendererSurfaceStateUserData>()
            .unwrap()
            .lock()
            .unwrap()
            .buffer()
            .unwrap()
            .clone()
    });
    assert_eq!(in_flight, buffer);
    let old_damage = with_states(surface, |states| {
        states
            .data_map
            .get::<RendererSurfaceStateUserData>()
            .unwrap()
            .lock()
            .unwrap()
            .damage()
            .current_commit()
    });
    // Reattaching the same wl_buffer still describes a new explicit lifetime.
    set_points(fixture, 3, 4);
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceAttributes>()
            .pending()
            .buffer = Some(BufferAssignment::NewBuffer(buffer.clone()))
    });
    fixture.request(surface.id().protocol_id(), 6, &[], &[]);
    fixture.dispatch();
    assert_eq!(fixture.state.dmabuf_blocker_source_count, 1);
    device.syncobj_timeline_signal(&[syncobj], &[3]).unwrap();
    fixture
        .event_loop
        .dispatch(Some(Duration::from_millis(20)), &mut fixture.state)
        .unwrap();
    fixture.dispatch();
    let reattached = with_states(surface, |states| {
        let renderer = states
            .data_map
            .get::<RendererSurfaceStateUserData>()
            .unwrap()
            .lock()
            .unwrap();
        assert_ne!(
            renderer.damage().current_commit(),
            old_damage,
            "reattachment must retain damage progression"
        );
        renderer.buffer().unwrap().clone()
    });

    fixture.request(surface.id().protocol_id(), 1, &[0, 0, 0], &[]);
    fixture.request(surface.id().protocol_id(), 6, &[], &[]);
    fixture.dispatch();
    let mut signalled = [0];
    device
        .syncobj_timeline_query(&[syncobj], &mut signalled, false)
        .unwrap();
    assert_eq!(
        signalled[0], 3,
        "release must wait while a renderer/KMS buffer reference remains"
    );
    drop(reattached);
    drop(in_flight);
    device
        .syncobj_timeline_query(&[syncobj], &mut signalled, false)
        .unwrap();
    assert_eq!(signalled[0], 4);
    let toplevel = fixture
        .state
        .unmapped_toplevels
        .get(surface)
        .unwrap()
        .toplevel()
        .unwrap();
    let serial = toplevel.send_configure();
    fixture.request(surface.id().protocol_id() + 1, 4, &[serial.into()], &[]);
    fixture.dispatch();
    set_points(fixture, 5, 6);
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceAttributes>()
            .pending()
            .buffer = Some(BufferAssignment::NewBuffer(buffer))
    });
    fixture.request(surface.id().protocol_id(), 6, &[], &[]);
    fixture.dispatch();
    assert_eq!(fixture.state.dmabuf_blocker_source_count, 1);
    fixture.request(surface.id().protocol_id(), 0, &[], &[]);
    fixture.dispatch();
    assert_eq!(
        fixture.state.dmabuf_blocker_source_count, 0,
        "destroyed surfaces must remove their fence watchers"
    );
    device
        .syncobj_timeline_query(&[syncobj], &mut signalled, false)
        .unwrap();
    assert_eq!(
        signalled[0], 6,
        "a destroyed unused pending buffer must release its point"
    );
    device.destroy_syncobj(syncobj).unwrap();
}

#[test]
fn scroll_navigation_refreshes_stationary_pointer_without_stealing_keyboard_focus() {
    use smithay::{
        input::pointer::{AxisFrame, MotionEvent},
        utils::SERIAL_COUNTER,
    };
    const CHILD: &str = "TIDEWM_SCROLL_POINTER_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidewm-pointer-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), "engine = scroll\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nscroll {\n tile_proportion = 1.0\n}\nanimations {\n enabled = false\n}\n").unwrap();
        let name = concat!(
            module_path!(),
            "::scroll_navigation_refreshes_stationary_pointer_without_stealing_keyboard_focus"
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
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
    let state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let (server, peer) = UnixStream::pair().unwrap();
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
    let mut fixture = Fixture {
        event_loop,
        state,
        peer,
    };
    let output = Output::new(
        "pointer-output".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
            serial_number: "test".into(),
        },
    );
    output.change_current_state(
        Some(Mode {
            size: (1000, 600).into(),
            refresh: 60_000,
        }),
        None,
        Some(Scale::Integer(1)),
        None,
    );
    fixture.state.space.map_output(&output, (0, 0));
    fixture.request(1, 1, &[2], &[]);
    let globals: Vec<_> = fixture
        .dispatch()
        .into_iter()
        .filter_map(|(object, opcode, body)| {
            if object != 2 || opcode != 0 {
                return None;
            }
            let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
            let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            Some((
                String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap(),
                name,
            ))
        })
        .collect();
    for (interface, id, version) in [
        ("wl_compositor", 3_u32, 4_u32),
        ("xdg_wm_base", 4, 1),
        ("wp_single_pixel_buffer_manager_v1", 5, 1),
        ("wp_viewporter", 6, 1),
        ("wl_seat", 7, 5),
    ] {
        let name = globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap()
            .1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[name], &tail);
    }
    fixture.request(7, 0, &[8], &[]); // get_pointer
    fixture.dispatch();
    for surface in [9_u32, 14] {
        fixture.request(3, 0, &[surface], &[]);
        fixture.request(4, 2, &[surface + 1, surface], &[]);
        fixture.request(surface + 1, 1, &[surface + 2], &[]);
        fixture.request(6, 1, &[surface + 3, surface], &[]);
        fixture.request(surface + 3, 2, &[1000, 600], &[]);
        // A GTK-style CSD shadow margin: the window geometry is inset from the
        // surface, exactly like Firefox. Pointer input must subtract it.
        fixture.request(surface + 1, 3, &[19, 17, 962, 566], &[]);
        fixture.request(surface, 6, &[], &[]);
        let serial = fixture
            .dispatch()
            .into_iter()
            .find_map(|(object, opcode, body)| {
                (object == surface + 1 && opcode == 0)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        fixture.request(surface + 1, 4, &[serial], &[]);
        fixture.request(5, 1, &[surface + 4, u32::MAX, 0, 0, u32::MAX], &[]);
        fixture.request(surface, 1, &[surface + 4, 0, 0], &[]);
        fixture.request(surface, 6, &[], &[]);
        fixture.dispatch();
    }
    let mut windows: Vec<_> = fixture.state.space.elements().cloned().collect();
    windows.sort_by_key(|window| window.toplevel().unwrap().wl_surface().id().protocol_id());
    assert_eq!(windows.len(), 2);
    let surfaces: Vec<_> = windows
        .iter()
        .map(|window| window.toplevel().unwrap().wl_surface().clone())
        .collect();

    // A client-declared window-geometry offset must not shift pointer input.
    // The surface sits at `rect.loc - geometry.loc` and the coordinates the
    // client receives are relative to that surface; dropping the offset is
    // what made GTK/Firefox clicks land 19x17 px up-left of the cursor.
    // The second column is the revealed one here (mapping it focused it), so
    // probe that window; its neighbor sits off-screen at x - 1000.
    let location = fixture.state.space.element_location(&windows[1]).unwrap();
    let probe = (location.x as f64 + 300.0, location.y as f64 + 200.0).into();
    let under = fixture.state.surface_under(probe);
    assert_eq!(
        under
            .as_ref()
            .map(|(surface, origin)| (surface.clone(), *origin)),
        Some((
            surfaces[1].clone(),
            (f64::from(location.x - 19), f64::from(location.y - 17)).into()
        ))
    );
    fixture.state.seat.get_pointer().unwrap().motion(
        &mut fixture.state,
        under,
        &MotionEvent {
            location: probe,
            serial: SERIAL_COUNTER.next_serial(),
            time: smithay::backend::input::InputTime::from_millis(1),
        },
    );
    fixture
        .state
        .seat
        .get_pointer()
        .unwrap()
        .frame(&mut fixture.state);
    let events = fixture.dispatch();
    let (_, _, body) = events
        .iter()
        .find(|(object, opcode, _)| (*object, *opcode) == (8, 2))
        .expect("no wl_pointer.motion after the geometry-offset probe");
    let client_x = f64::from(i32::from_ne_bytes(body[4..8].try_into().unwrap())) / 256.0;
    let client_y = f64::from(i32::from_ne_bytes(body[8..12].try_into().unwrap())) / 256.0;
    assert!(
        (client_x - 319.0).abs() < 0.02,
        "client pointer x {client_x}, expected 319 (300 + the 19px margin)"
    );
    assert!(
        (client_y - 217.0).abs() < 0.02,
        "client pointer y {client_y}, expected 217 (200 + the 17px margin)"
    );

    let pointer = fixture.state.seat.get_pointer().unwrap();
    fixture.state.activate_toplevel(&surfaces[0]);
    fixture.dispatch();
    let pos = (500.0, 300.0).into();
    let under = fixture.state.surface_under(pos);
    pointer.motion(
        &mut fixture.state,
        under,
        &MotionEvent {
            location: pos,
            serial: SERIAL_COUNTER.next_serial(),
            time: smithay::backend::input::InputTime::from_millis(1),
        },
    );
    pointer.frame(&mut fixture.state);
    fixture.dispatch();
    assert_eq!(pointer.current_focus(), Some(surfaces[0].clone()));
    fixture.state.activate_toplevel(&surfaces[1]);
    fixture.dispatch();
    // No real motion between focusing the other column and sending the wheel.
    assert_eq!(pointer.current_location(), pos);
    assert_eq!(pointer.current_focus(), Some(surfaces[1].clone()));
    assert_eq!(
        fixture.state.focused_window_surface(),
        Some(surfaces[1].clone())
    );
    pointer.axis(
        &mut fixture.state,
        AxisFrame::new(smithay::backend::input::InputTime::from_millis(2))
            .value(smithay::backend::input::Axis::Vertical, 15.0),
    );
    pointer.frame(&mut fixture.state);
    let events = fixture.dispatch();
    assert!(
        events
            .iter()
            .any(|(object, opcode, _)| (*object, *opcode) == (8, 4)),
        "the stationary pointer received no wheel event"
    );
    // Animated navigation keeps pointer input on the app visibly underneath,
    // while keyboard focus stays on the selected destination.
    fixture.state.config.animations.enabled = true;
    fixture.state.config.animations.movement.enabled = true;
    fixture.state.config.animations.movement.duration_ms = 50;
    fixture.state.config.animations.slowdown = 1.0;
    fixture.state.activate_toplevel(&surfaces[0]);
    fixture.dispatch();
    assert_eq!(
        fixture.state.focused_window_surface(),
        Some(surfaces[0].clone())
    );
    assert_eq!(pointer.current_focus(), Some(surfaces[1].clone()));
    std::thread::sleep(Duration::from_millis(400));
    fixture.state.has_active_animation();
    fixture.dispatch();
    assert_eq!(pointer.current_focus(), Some(surfaces[0].clone()));
    assert_eq!(
        fixture.state.focused_window_surface(),
        Some(surfaces[0].clone())
    );

    // Scroll reveals only for navigation. Pointer-driven focus (hover, press,
    // touch, tablet) must leave the viewport alone: crossing to another
    // monitor focuses the window under the cursor, and recentering it there
    // would drag the strip out from under the pointer that just arrived.
    let strip_offset = |state: &Smallvil| {
        state
            .spatial
            .workspaces
            .layout
            .scroll
            .offset("pointer-output", 1)
    };
    assert_eq!(strip_offset(&fixture.state), 0.0);
    fixture
        .state
        .focus_window_from_pointer(Some(surfaces[1].clone()), SERIAL_COUNTER.next_serial());
    assert_eq!(
        fixture.state.focused_window_surface(),
        Some(surfaces[1].clone())
    );
    assert_eq!(
        strip_offset(&fixture.state),
        0.0,
        "pointer focus moved the Scroll viewport"
    );
    fixture
        .state
        .focus_window(Some(surfaces[1].clone()), SERIAL_COUNTER.next_serial());
    assert_eq!(
        strip_offset(&fixture.state),
        1000.0,
        "navigation focus did not reveal the off-screen column"
    );
}

#[test]
fn scroll_columns_enter_only_their_own_output() {
    const CHILD: &str = "TIDEWM_SCROLL_ENTER_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("tidewm-enter-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), "engine = scroll\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nscroll {\n tile_proportion = 1.0\n}\nanimations {\n enabled = false\n}\n").unwrap();
        let name = concat!(
            module_path!(),
            "::scroll_columns_enter_only_their_own_output"
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
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
    let state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let (server, peer) = UnixStream::pair().unwrap();
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
    let mut fixture = Fixture {
        event_loop,
        state,
        peer,
    };
    // Two outputs side by side: the strip's off-screen column on the left
    // output lies exactly inside the right output's rectangle.
    let outputs: Vec<_> = ["strip-output", "neighbour-output"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let output = Output::new(
                name.into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                    serial_number: "test".into(),
                },
            );
            output.change_current_state(
                Some(Mode {
                    size: (1000, 600).into(),
                    refresh: 60_000,
                }),
                None,
                Some(Scale::Integer(1)),
                Some((index as i32 * 1000, 0).into()),
            );
            output.create_global::<Smallvil>(&fixture.state.display_handle);
            fixture
                .state
                .space
                .map_output(&output, (index as i32 * 1000, 0));
            output
        })
        .collect();
    fixture.request(1, 1, &[2], &[]);
    let globals: Vec<_> = fixture
        .dispatch()
        .into_iter()
        .filter_map(|(object, opcode, body)| {
            if object != 2 || opcode != 0 {
                return None;
            }
            let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
            let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            Some((
                String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap(),
                name,
            ))
        })
        .collect();
    for (interface, id, version) in [
        ("wl_compositor", 3_u32, 4_u32),
        ("xdg_wm_base", 4, 1),
        ("wp_single_pixel_buffer_manager_v1", 5, 1),
        ("wp_viewporter", 6, 1),
    ] {
        let name = globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap()
            .1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[name], &tail);
    }
    // Bind every wl_output; the client-side id of each tells the outputs apart.
    let mut output_ids = Vec::new();
    for (index, (_, name)) in globals
        .iter()
        .filter(|(interface, _)| interface == "wl_output")
        .enumerate()
    {
        let id = 7 + index as u32;
        let mut tail = string("wl_output");
        tail.extend_from_slice(&3_u32.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[*name], &tail);
        output_ids.push(id);
    }
    assert_eq!(output_ids.len(), 2);
    let mut events = fixture.dispatch();
    // wl_output.name (opcode 4, version 4+) is not bound here; match each
    // binding to its output through the advertised geometry x instead.
    let id_for_x = |events: &[(u32, u16, Vec<u8>)], x: i32| {
        events
            .iter()
            .find(|(object, opcode, body)| {
                output_ids.contains(object)
                    && *opcode == 0
                    && i32::from_ne_bytes(body[..4].try_into().unwrap()) == x
            })
            .map(|(object, _, _)| *object)
            .unwrap()
    };
    let strip_id = id_for_x(&events, 0);
    let neighbour_id = id_for_x(&events, 1000);

    let mut surfaces = Vec::new();
    for surface in [9_u32, 14] {
        fixture.request(3, 0, &[surface], &[]);
        fixture.request(4, 2, &[surface + 1, surface], &[]);
        fixture.request(surface + 1, 1, &[surface + 2], &[]);
        fixture.request(6, 1, &[surface + 3, surface], &[]);
        fixture.request(surface + 3, 2, &[1000, 600], &[]);
        fixture.request(surface, 6, &[], &[]);
        let serial = fixture
            .dispatch()
            .into_iter()
            .find_map(|(object, opcode, body)| {
                (object == surface + 1 && opcode == 0)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        fixture.request(surface + 1, 4, &[serial], &[]);
        fixture.request(5, 1, &[surface + 4, u32::MAX, 0, 0, u32::MAX], &[]);
        fixture.request(surface, 1, &[surface + 4, 0, 0], &[]);
        fixture.request(surface, 6, &[], &[]);
        events.extend(fixture.dispatch());
        surfaces.push(surface);
    }
    let windows: Vec<_> = fixture.state.space.elements().cloned().collect();
    assert_eq!(windows.len(), 2);
    for window in &windows {
        assert_eq!(
            fixture.state.output_for_window(window),
            Some(outputs[0].clone()),
            "both columns must live on the strip output"
        );
    }
    // Focus the first column: the second now sits at x = 1000..2000, i.e.
    // entirely inside the neighbour output's rectangle, never drawn there.
    let first = windows
        .iter()
        .find(|window| window.toplevel().unwrap().wl_surface().id().protocol_id() == surfaces[0])
        .unwrap()
        .toplevel()
        .unwrap()
        .wl_surface()
        .clone();
    fixture.state.activate_toplevel(&first);
    fixture.state.refresh_space();
    events.extend(fixture.dispatch());
    let second = windows
        .iter()
        .find(|window| window.toplevel().unwrap().wl_surface().id().protocol_id() == surfaces[1])
        .unwrap();
    let second_loc = fixture.state.space.element_location(second).unwrap();
    assert!(
        second_loc.x >= 1000,
        "setup: the second column should be off-screen right, got {second_loc:?}"
    );

    let entered = |surface: u32, output: u32| {
        events.iter().any(|(object, opcode, body)| {
            *object == surface
                && *opcode == 0
                && u32::from_ne_bytes(body[..4].try_into().unwrap()) == output
        })
    };
    assert!(
        entered(surfaces[0], strip_id),
        "first column never entered its output"
    );
    for surface in &surfaces {
        assert!(
            !entered(*surface, neighbour_id),
            "surface {surface} entered the neighbour output it is not drawn on"
        );
    }
}

#[test]
fn monitor_focus_uses_live_geometry_and_restores_recent_windows() {
    use crate::config::{parse_action, Direction, SpatialEngine};
    use crate::state::SessionLock;
    const CHILD: &str = "TIDEWM_MONITOR_FOCUS_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidewm-monitor-focus-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), "engine = scroll\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nscroll {\n tile_proportion = 1.0\n}\nanimations {\n enabled = false\n}\n").unwrap();
        let name = concat!(
            module_path!(),
            "::monitor_focus_uses_live_geometry_and_restores_recent_windows"
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
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
    let state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let (server, peer) = UnixStream::pair().unwrap();
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
    let mut fixture = Fixture {
        event_loop,
        state,
        peer,
    };
    let mut outputs = Vec::new();
    // Registration order differs from physical order. Right has a fractional
    // scale: selection and pointer placement must use logical geometry.
    for (name, location, size, scale) in [
        ("origin", (0, 0), (1000, 600), Scale::Integer(1)),
        ("diagonal", (1000, -800), (1000, 600), Scale::Integer(1)),
        ("right", (1000, 0), (1250, 750), Scale::Fractional(1.25)),
        ("below", (0, 600), (1000, 600), Scale::Integer(1)),
    ] {
        let output = Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: size.into(),
                refresh: 60_000,
            }),
            None,
            Some(scale),
            Some(location.into()),
        );
        fixture.state.space.map_output(&output, location);
        outputs.push(output);
    }
    fixture.request(1, 1, &[2], &[]);
    let globals: Vec<_> = fixture
        .dispatch()
        .into_iter()
        .filter_map(|(object, opcode, body)| {
            if object != 2 || opcode != 0 {
                return None;
            }
            let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
            let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            Some((
                String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap(),
                name,
            ))
        })
        .collect();
    for (interface, id, version) in [
        ("wl_compositor", 3_u32, 4_u32),
        ("xdg_wm_base", 4, 1),
        ("wp_single_pixel_buffer_manager_v1", 5, 1),
        ("wp_viewporter", 6, 1),
    ] {
        let name = globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap()
            .1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[name], &tail);
    }
    let mut surfaces = Vec::new();
    for surface in [7_u32, 12, 17] {
        fixture.request(3, 0, &[surface], &[]);
        fixture.request(4, 2, &[surface + 1, surface], &[]);
        fixture.request(surface + 1, 1, &[surface + 2], &[]);
        fixture.request(6, 1, &[surface + 3, surface], &[]);
        fixture.request(surface + 3, 2, &[1000, 600], &[]);
        fixture.request(surface, 6, &[], &[]);
        let serial = fixture
            .dispatch()
            .into_iter()
            .find_map(|(object, opcode, body)| {
                (object == surface + 1 && opcode == 0)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        fixture.request(surface + 1, 4, &[serial], &[]);
        fixture.request(5, 1, &[surface + 4, u32::MAX, 0, 0, u32::MAX], &[]);
        fixture.request(surface, 1, &[surface + 4, 0, 0], &[]);
        fixture.request(surface, 6, &[], &[]);
        fixture.dispatch();
        surfaces.push(surface);
    }
    let surfaces: Vec<_> = surfaces
        .into_iter()
        .map(|id| {
            fixture
                .state
                .space
                .elements()
                .find_map(|window| {
                    let surface = window.toplevel()?.wl_surface();
                    (surface.id().protocol_id() == id).then(|| surface.clone())
                })
                .unwrap()
        })
        .collect();
    for surface in &surfaces[1..] {
        assert!(fixture
            .state
            .move_tiled_window_to_output(surface, &outputs[2], None));
    }
    fixture.state.retile();
    fixture.state.toggle_floating(&surfaces[2]);
    let run = |fixture: &mut Fixture, action: &str| {
        fixture.state.run_action(parse_action(action).unwrap());
        fixture.dispatch();
    };
    // Keyboard navigation can leave focus on a different output from the
    // pointer. A named action must still perform the requested handoff.
    fixture.state.config.input.focus_follows_mouse = true;
    fixture.state.activate_toplevel(&surfaces[1]);
    run(&mut fixture, "focus-monitor:right");
    fixture.state.activate_toplevel(&surfaces[0]);
    assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
    run(&mut fixture, "focus-monitor:right");
    assert_eq!(
        fixture.state.focused_window_surface(),
        Some(surfaces[1].clone())
    );
    for engine in [
        SpatialEngine::Scroll,
        SpatialEngine::Classic,
        SpatialEngine::Ocean,
    ] {
        assert!(fixture.state.migrate_spatial_engine(engine));
        fixture.state.config.input.focus_follows_mouse = false;
        fixture.state.activate_toplevel(&surfaces[1]);
        fixture.state.activate_toplevel(&surfaces[0]);
        // Pointer policy is independent of the saved window selection.
        let camera = fixture.state.spatial.ocean.camera("right");
        run(&mut fixture, "focus-monitor-right");
        assert_eq!(
            fixture.state.focused_window_surface(),
            Some(surfaces[1].clone()),
            "{engine:?}"
        );
        assert_eq!(
            fixture.state.spatial.ocean.camera("right"),
            camera,
            "monitor focus must not recenter the Ocean camera"
        );
        assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
        assert_eq!(
            fixture.state.seat.get_pointer().unwrap().current_location(),
            (1500.0, 300.0).into()
        );
        run(&mut fixture, "focus-monitor-left");
        assert_eq!(
            fixture.state.focused_window_surface(),
            Some(surfaces[0].clone())
        );
        fixture.state.config.input.focus_follows_mouse = true;
        run(&mut fixture, "focus-monitor-right");
        assert_eq!(
            fixture.state.focused_window_surface(),
            Some(surfaces[1].clone())
        );
        assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
        // Floating windows participate in the same per-monitor history.
        fixture.state.activate_toplevel(&surfaces[2]);
        run(&mut fixture, "focus-monitor:origin");
        run(&mut fixture, "focus-monitor-right");
        assert_eq!(
            fixture.state.focused_window_surface(),
            Some(surfaces[2].clone())
        );
        // No output on that side: no wrap and no focus or cursor changes.
        run(&mut fixture, "focus-monitor-right");
        assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
        run(&mut fixture, "focus-monitor:origin");
        run(&mut fixture, "focus-monitor-down");
        assert_eq!(fixture.state.focused_window_surface(), None);
        assert_eq!(fixture.state.primary_output(), Some(outputs[3].clone()));
        assert_eq!(
            fixture.state.seat.get_pointer().unwrap().current_location(),
            (500.0, 900.0).into()
        );
        run(&mut fixture, "focus-monitor-up");
        assert_eq!(
            fixture.state.focused_window_surface(),
            Some(surfaces[0].clone())
        );
        run(&mut fixture, "focus-monitor:missing-output");
        assert_eq!(fixture.state.primary_output(), Some(outputs[0].clone()));
    }
    // In shared Ocean, the destination can show the same window even when
    // its largest rendered share is on the source monitor.
    let right_camera = fixture.state.spatial.ocean.camera("right");
    let mut origin = fixture.state.spatial.ocean.camera("origin").origin;
    origin.x += 800.0;
    fixture.state.config.ocean.shared_canvas = true;
    fixture.state.spatial.ocean.set_shared_canvas(true);
    fixture
        .state
        .spatial
        .ocean
        .set_camera_origin("right", origin);
    fixture.state.retile();
    fixture.state.activate_toplevel(&surfaces[0]);
    for follows_mouse in [false, true] {
        fixture.state.config.input.focus_follows_mouse = follows_mouse;
        run(&mut fixture, "focus-monitor:origin");
        run(&mut fixture, "focus-monitor:right");
        assert_eq!(
            fixture.state.focused_window_surface(),
            Some(surfaces[0].clone())
        );
        assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
        assert!(
            fixture
                .state
                .seat
                .get_pointer()
                .unwrap()
                .current_location()
                .x
                < 1200.0,
            "pointer must land on the visible portion of the restored window"
        );
    }
    run(&mut fixture, "focus-monitor:origin");
    fixture.state.config.ocean.shared_canvas = false;
    fixture.state.spatial.ocean.set_shared_canvas(false);
    fixture
        .state
        .spatial
        .ocean
        .set_camera_origin("right", right_camera.origin);
    fixture.state.retile();
    // Read current output locations every time, including negative positions.
    fixture.state.space.map_output(&outputs[2], (-1000, 0));
    assert_eq!(
        fixture.state.monitor_in_direction(Direction::Left),
        Some(outputs[2].clone())
    );
    assert_eq!(fixture.state.monitor_in_direction(Direction::Right), None);
    run(&mut fixture, "focus-monitor-left");
    assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
    let pointer = fixture.state.seat.get_pointer().unwrap();
    let location = pointer.current_location();
    pointer.button(
        &mut fixture.state,
        &smithay::input::pointer::ButtonEvent {
            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
            time: smithay::backend::input::InputTime::now(),
            button: 0x110,
            state: smithay::backend::input::ButtonState::Pressed,
        },
    );
    assert!(pointer.is_grabbed());
    run(&mut fixture, "focus-monitor:origin");
    assert_eq!(pointer.current_location(), location);
    pointer.button(
        &mut fixture.state,
        &smithay::input::pointer::ButtonEvent {
            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
            time: smithay::backend::input::InputTime::now(),
            button: 0x110,
            state: smithay::backend::input::ButtonState::Released,
        },
    );
    fixture.state.session_lock = SessionLock::Locked;
    let location = fixture.state.seat.get_pointer().unwrap().current_location();
    run(&mut fixture, "focus-monitor:origin");
    assert_eq!(
        fixture.state.seat.get_pointer().unwrap().current_location(),
        location
    );
    fixture.state.session_lock = SessionLock::Unlocked;
    fixture.state.space.unmap_output(&outputs[0]);
    run(&mut fixture, "focus-monitor:origin");
    assert_eq!(
        fixture.state.seat.get_pointer().unwrap().current_location(),
        location
    );
}

#[test]
fn focus_direction_or_monitor_hands_off_at_strip_ends() {
    use crate::config::{parse_action, SpatialEngine};
    use crate::state::SessionLock;
    const CHILD: &str = "TIDEWM_FOCUS_OR_MONITOR_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidewm-focus-or-monitor-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), "engine = scroll\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nscroll {\n tile_proportion = 1.0\n}\nanimations {\n enabled = false\n}\n").unwrap();
        let name = concat!(
            module_path!(),
            "::focus_direction_or_monitor_hands_off_at_strip_ends"
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
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
    let state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let (server, peer) = UnixStream::pair().unwrap();
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
    let mut fixture = Fixture {
        event_loop,
        state,
        peer,
    };
    let mut outputs = Vec::new();
    for (name, location, size, scale) in [
        ("left", (0, 0), (1000, 600), Scale::Integer(1)),
        ("right", (1000, 0), (1000, 600), Scale::Integer(1)),
    ] {
        let output = Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: size.into(),
                refresh: 60_000,
            }),
            None,
            Some(scale),
            Some(location.into()),
        );
        fixture.state.space.map_output(&output, location);
        outputs.push(output);
    }
    fixture.request(1, 1, &[2], &[]);
    let globals: Vec<_> = fixture
        .dispatch()
        .into_iter()
        .filter_map(|(object, opcode, body)| {
            if object != 2 || opcode != 0 {
                return None;
            }
            let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
            let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            Some((
                String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap(),
                name,
            ))
        })
        .collect();
    for (interface, id, version) in [
        ("wl_compositor", 3_u32, 4_u32),
        ("xdg_wm_base", 4, 1),
        ("wp_single_pixel_buffer_manager_v1", 5, 1),
        ("wp_viewporter", 6, 1),
    ] {
        let name = globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap()
            .1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[name], &tail);
    }
    let mut surfaces = Vec::new();
    for surface in [7_u32, 12, 17] {
        fixture.request(3, 0, &[surface], &[]);
        fixture.request(4, 2, &[surface + 1, surface], &[]);
        fixture.request(surface + 1, 1, &[surface + 2], &[]);
        fixture.request(6, 1, &[surface + 3, surface], &[]);
        fixture.request(surface + 3, 2, &[1000, 600], &[]);
        fixture.request(surface, 6, &[], &[]);
        let serial = fixture
            .dispatch()
            .into_iter()
            .find_map(|(object, opcode, body)| {
                (object == surface + 1 && opcode == 0)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        fixture.request(surface + 1, 4, &[serial], &[]);
        fixture.request(5, 1, &[surface + 4, u32::MAX, 0, 0, u32::MAX], &[]);
        fixture.request(surface, 1, &[surface + 4, 0, 0], &[]);
        fixture.request(surface, 6, &[], &[]);
        fixture.dispatch();
        surfaces.push(surface);
    }
    let surfaces: Vec<_> = surfaces
        .into_iter()
        .map(|id| {
            fixture
                .state
                .space
                .elements()
                .find_map(|window| {
                    let surface = window.toplevel()?.wl_surface();
                    (surface.id().protocol_id() == id).then(|| surface.clone())
                })
                .unwrap()
        })
        .collect();
    assert!(fixture
        .state
        .move_tiled_window_to_output(&surfaces[2], &outputs[1], None));
    fixture.state.retile();
    fixture.state.config.input.focus_follows_mouse = false;
    let run = |fixture: &mut Fixture, action: &str| {
        fixture.state.run_action(parse_action(action).unwrap());
        fixture.dispatch();
    };
    let focused = |fixture: &Fixture| fixture.state.focused_window_surface();
    let pointer = |fixture: &Fixture| fixture.state.seat.get_pointer().unwrap().current_location();

    // Scroll: two columns on the left monitor, one on the right.
    fixture.state.activate_toplevel(&surfaces[1]);
    fixture.state.activate_toplevel(&surfaces[0]);
    let start = pointer(&fixture);
    // Nothing to the left of the first column and no monitor there.
    run(&mut fixture, "focus-column-or-monitor-left");
    assert_eq!(focused(&fixture), Some(surfaces[0].clone()));
    assert_eq!(pointer(&fixture), start);
    // Inside the strip it is ordinary column focus; the pointer stays.
    run(&mut fixture, "focus-column-or-monitor-right");
    assert_eq!(focused(&fixture), Some(surfaces[1].clone()));
    assert_eq!(pointer(&fixture), start);
    // At the strip end it hands off to the right monitor.
    run(&mut fixture, "focus-column-or-monitor-right");
    assert_eq!(focused(&fixture), Some(surfaces[2].clone()));
    assert_eq!(fixture.state.primary_output(), Some(outputs[1].clone()));
    // The single right column is both ends: back to the left monitor's
    // most recently used window.
    run(&mut fixture, "focus-column-or-monitor-left");
    assert_eq!(focused(&fixture), Some(surfaces[1].clone()));
    assert_eq!(fixture.state.primary_output(), Some(outputs[0].clone()));
    // No row and no monitor below: unchanged.
    run(&mut fixture, "focus-window-or-monitor-down");
    assert_eq!(focused(&fixture), Some(surfaces[1].clone()));
    run(&mut fixture, "focus-window-or-monitor-up");
    assert_eq!(focused(&fixture), Some(surfaces[1].clone()));
    // Without a focused window the pointer output decides the side.
    fixture
        .state
        .focus_window(None, smithay::utils::SERIAL_COUNTER.next_serial());
    run(&mut fixture, "focus-column-or-monitor-right");
    assert_eq!(focused(&fixture), Some(surfaces[2].clone()));
    assert_eq!(fixture.state.primary_output(), Some(outputs[1].clone()));
    // Lock blocks the handoff entirely.
    fixture.state.session_lock = SessionLock::Locked;
    let location = pointer(&fixture);
    run(&mut fixture, "focus-column-or-monitor-left");
    assert_eq!(pointer(&fixture), location);
    fixture.state.session_lock = SessionLock::Unlocked;
    run(&mut fixture, "focus-column-or-monitor-left");
    assert_eq!(focused(&fixture), Some(surfaces[1].clone()));

    // Classic geometry can reach the other monitor directly; the pointer
    // still follows so a later hover cannot undo the handoff.
    assert!(fixture.state.migrate_spatial_engine(SpatialEngine::Classic));
    fixture.state.retile();
    fixture.state.refresh_space();
    fixture.state.activate_toplevel(&surfaces[0]);
    fixture.state.activate_toplevel(&surfaces[1]);
    let right_of =
        |fixture: &Fixture,
         surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface| {
            let window = fixture
                .state
                .space
                .elements()
                .find(|window| window.toplevel().map(|t| t.wl_surface()) == Some(surface))
                .unwrap();
            fixture.state.space.element_geometry(window).unwrap().loc.x
        };
    let (rightmost, other) = if right_of(&fixture, &surfaces[1]) > right_of(&fixture, &surfaces[0])
    {
        (surfaces[1].clone(), surfaces[0].clone())
    } else {
        (surfaces[0].clone(), surfaces[1].clone())
    };
    fixture.state.activate_toplevel(&other);
    fixture.state.activate_toplevel(&rightmost);
    let location = pointer(&fixture);
    run(&mut fixture, "focus-column-or-monitor-right");
    assert_eq!(focused(&fixture), Some(surfaces[2].clone()));
    assert_eq!(fixture.state.primary_output(), Some(outputs[1].clone()));
    assert_ne!(pointer(&fixture), location);
    run(&mut fixture, "focus-column-or-monitor-left");
    assert_eq!(focused(&fixture), Some(rightmost.clone()));
    assert_eq!(fixture.state.primary_output(), Some(outputs[0].clone()));
    run(&mut fixture, "focus-column-or-monitor-left");
    assert_eq!(focused(&fixture), Some(other));
    assert_eq!(fixture.state.primary_output(), Some(outputs[0].clone()));
}

#[test]
fn monitor_window_moves_keep_ownership_and_return_modes() {
    use crate::config::{parse_action, SpatialEngine};
    use crate::state::SessionLock;
    const CHILD: &str = "TIDEWM_MONITOR_MOVE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidewm-monitor-move-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), "engine = scroll\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nscroll {\n tile_proportion = 1.0\n}\nanimations {\n enabled = false\n}\n").unwrap();
        let name = concat!(
            module_path!(),
            "::monitor_window_moves_keep_ownership_and_return_modes"
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
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
    let state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let (server, peer) = UnixStream::pair().unwrap();
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
    let mut fixture = Fixture {
        event_loop,
        state,
        peer,
    };
    let mut outputs = Vec::new();
    // Registration order differs from physical order. Right has a fractional
    // scale: selection and pointer placement must use logical geometry.
    for (name, location, size, scale) in [
        ("origin", (0, 0), (1000, 600), Scale::Integer(1)),
        ("diagonal", (1000, -800), (1000, 600), Scale::Integer(1)),
        ("right", (1000, 0), (1125, 875), Scale::Fractional(1.25)),
        ("below", (0, 800), (1000, 600), Scale::Integer(1)),
    ] {
        let output = Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: size.into(),
                refresh: 60_000,
            }),
            None,
            Some(scale),
            Some(location.into()),
        );
        fixture.state.space.map_output(&output, location);
        outputs.push(output);
    }
    fixture.request(1, 1, &[2], &[]);
    let globals: Vec<_> = fixture
        .dispatch()
        .into_iter()
        .filter_map(|(object, opcode, body)| {
            if object != 2 || opcode != 0 {
                return None;
            }
            let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
            let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            Some((
                String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap(),
                name,
            ))
        })
        .collect();
    for (interface, id, version) in [
        ("wl_compositor", 3_u32, 4_u32),
        ("xdg_wm_base", 4, 1),
        ("wp_single_pixel_buffer_manager_v1", 5, 1),
        ("wp_viewporter", 6, 1),
    ] {
        let name = globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap()
            .1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[name], &tail);
    }
    let mut surfaces = Vec::new();
    for surface in [7_u32, 12, 17, 22] {
        fixture.request(3, 0, &[surface], &[]);
        fixture.request(4, 2, &[surface + 1, surface], &[]);
        fixture.request(surface + 1, 1, &[surface + 2], &[]);
        fixture.request(6, 1, &[surface + 3, surface], &[]);
        fixture.request(surface + 3, 2, &[1000, 600], &[]);
        fixture.request(surface, 6, &[], &[]);
        let serial = fixture
            .dispatch()
            .into_iter()
            .find_map(|(object, opcode, body)| {
                (object == surface + 1 && opcode == 0)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        fixture.request(surface + 1, 4, &[serial], &[]);
        fixture.request(5, 1, &[surface + 4, u32::MAX, 0, 0, u32::MAX], &[]);
        fixture.request(surface, 1, &[surface + 4, 0, 0], &[]);
        fixture.request(surface, 6, &[], &[]);
        fixture.dispatch();
        surfaces.push(surface);
    }
    let surfaces: Vec<_> = surfaces
        .into_iter()
        .map(|id| {
            fixture
                .state
                .space
                .elements()
                .find_map(|window| {
                    let surface = window.toplevel()?.wl_surface();
                    (surface.id().protocol_id() == id).then(|| surface.clone())
                })
                .unwrap()
        })
        .collect();
    fixture.state.config.ocean.camera_animation_ms = 0;
    fixture.state.toggle_floating(&surfaces[2]);
    fixture
        .state
        .spatial
        .workspaces
        .layout
        .set_active_workspace("right", 3);
    let run = |fixture: &mut Fixture, action: &str| {
        let cameras = [
            fixture.state.spatial.ocean.camera("origin"),
            fixture.state.spatial.ocean.camera("right"),
        ];
        fixture.state.run_action(parse_action(action).unwrap());
        // Behave like real clients: ack the last configure and commit its
        // viewport size, so a refused old buffer cannot appear to overlap a
        // neighbor after its tile moved into a smaller destination slot.
        let mut events = fixture.dispatch();
        for _ in 0..16 {
            let mut acknowledged = false;
            for surface in [7_u32, 12, 17, 22] {
                let size = events.iter().rev().find_map(|(object, opcode, body)| {
                    (*object == surface + 2 && *opcode == 0).then(|| {
                        (
                            u32::from_ne_bytes(body[..4].try_into().unwrap()),
                            u32::from_ne_bytes(body[4..8].try_into().unwrap()),
                        )
                    })
                });
                let serial = events.iter().rev().find_map(|(object, opcode, body)| {
                    (*object == surface + 1 && *opcode == 0)
                        .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
                });
                if let Some(serial) = serial {
                    if let Some((width, height)) = size.filter(|(w, h)| *w > 0 && *h > 0) {
                        fixture.request(surface + 3, 2, &[width, height], &[]);
                    }
                    fixture.request(surface + 1, 4, &[serial], &[]);
                    fixture.request(surface, 6, &[], &[]);
                    acknowledged = true;
                }
            }
            if !acknowledged {
                break;
            }
            events = fixture.dispatch();
        }
        fixture.state.refresh_space();
        if action.starts_with("move-window-to-monitor") {
            assert_eq!(
                fixture.state.spatial.ocean.camera("origin"),
                cameras[0],
                "{action}"
            );
            assert_eq!(
                fixture.state.spatial.ocean.camera("right"),
                cameras[1],
                "{action}"
            );
        }
    };
    let owner = |fixture: &Fixture, index: usize| {
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&surfaces[index])
            .or_else(|| {
                fixture
                    .state
                    .spatial
                    .workspaces
                    .floating_workspace
                    .get(&surfaces[index])
                    .map(|tag| tag.output.as_str())
            })
            .or_else(|| fixture.state.spatial.ocean.entry_output(&surfaces[index]))
            .unwrap()
            .to_string()
    };
    for engine in [
        SpatialEngine::Scroll,
        SpatialEngine::Classic,
        SpatialEngine::Ocean,
    ] {
        assert!(fixture.state.migrate_spatial_engine(engine));
        fixture.state.activate_toplevel(&surfaces[0]);
        // With pointer policy on, leave the pointer on a different monitor.
        // A window move still starts from the focused window's own monitor.
        fixture.state.config.input.focus_follows_mouse = true;
        run(&mut fixture, "focus-monitor:right");
        fixture.state.activate_toplevel(&surfaces[0]);
        assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
        if engine == SpatialEngine::Scroll {
            fixture
                .state
                .spatial
                .workspaces
                .layout
                .scroll
                .set_stacked(&surfaces[1], true);
            fixture
                .state
                .spatial
                .workspaces
                .layout
                .scroll
                .resize(surfaces[0].clone(), 0.7);
            fixture
                .state
                .spatial
                .workspaces
                .layout
                .scroll
                .set_weight(&surfaces[1], 2.0);
            fixture.state.retile();
        }
        run(&mut fixture, "move-window-to-monitor-right");
        assert_eq!(owner(&fixture, 0), "right", "{engine:?}");
        assert_eq!(
            fixture.state.focused_window_surface(),
            Some(surfaces[0].clone())
        );
        assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
        assert_eq!(
            fixture.state.seat.get_pointer().unwrap().current_focus(),
            Some(surfaces[0].clone()),
            "{engine:?}; pos={:?}; placements={:?}",
            fixture.state.seat.get_pointer().unwrap().current_location(),
            fixture
                .state
                .render_placements(&outputs[2])
                .unwrap()
                .iter()
                .map(|p| (p.surface().map(|s| s.id().protocol_id()), p.rect))
                .collect::<Vec<_>>()
        );
        if engine != SpatialEngine::Ocean {
            assert_eq!(
                fixture
                    .state
                    .spatial
                    .workspaces
                    .layout
                    .workspace_of(&surfaces[0]),
                Some(3)
            );
            assert_eq!(
                fixture
                    .state
                    .spatial
                    .workspaces
                    .layout
                    .workspace_of(&surfaces[1]),
                Some(1)
            );
        }
        if engine == SpatialEngine::Scroll {
            let layout = &fixture.state.spatial.workspaces.layout;
            assert!(!layout.scroll.is_stacked(&surfaces[1]));
            assert_eq!(layout.scroll.proportion(&surfaces[1]), 0.7);
            assert_eq!(layout.scroll.proportion(&surfaces[0]), 0.7);
            assert_eq!(layout.scroll.weight(&surfaces[1]), 2.0);
        }
        // Same monitor, missing monitor and strip edge are complete no-ops.
        let pointer = fixture.state.seat.get_pointer().unwrap();
        let location = pointer.current_location();
        for action in [
            "move-window-to-monitor-right",
            "move-window-to-monitor:right",
            "move-window-to-monitor:missing",
        ] {
            run(&mut fixture, action);
            assert_eq!(owner(&fixture, 0), "right");
            assert_eq!(pointer.current_location(), location);
        }
        fixture.state.config.input.focus_follows_mouse = false;
        run(&mut fixture, "move-window-to-monitor-left");
        assert_eq!(
            owner(&fixture, 0),
            "origin",
            "{engine:?}; durable={:?}; pointer={:?}; focus={:?}",
            fixture
                .state
                .spatial
                .workspaces
                .layout
                .output_of(&surfaces[0]),
            fixture.state.seat.get_pointer().unwrap().current_location(),
            fixture.state.focused_window_surface()
        );
        assert_eq!(fixture.state.primary_output(), Some(outputs[0].clone()));
        run(&mut fixture, "move-window-to-monitor-down");
        assert_eq!(owner(&fixture, 0), "below");
        run(&mut fixture, "move-window-to-monitor-up");
        assert_eq!(owner(&fixture, 0), "origin");
        // Group tabs are one tiling slot. A transfer carries the parked tab,
        // its output pin and group workspace, without inserting it as a tile.
        fixture.state.group_with(&surfaces[0], &surfaces[1]);
        fixture
            .state
            .spatial
            .workspaces
            .output_pins
            .insert(surfaces[1].clone(), "origin".into());
        run(&mut fixture, "move-window-to-monitor:right");
        let group = fixture.state.group_of(&surfaces[0]).unwrap();
        assert_eq!(fixture.state.groups[group].output, "right");
        assert_eq!(fixture.state.groups[group].workspace, 3);
        assert_eq!(
            fixture
                .state
                .spatial
                .workspaces
                .output_pins
                .get(&surfaces[1])
                .map(String::as_str),
            Some("right")
        );
        fixture.state.ungroup(&surfaces[1]);
        assert_eq!(owner(&fixture, 1), "right");
        for surface in &surfaces[..2] {
            fixture.state.activate_toplevel(surface);
            run(&mut fixture, "move-window-to-monitor:origin");
        }
        // A fullscreen tile moves without leaving fullscreen and preempts
        // the existing destination fullscreen owner. Configure uses the
        // destination logical mode, not physical pixels or the old mode.
        fixture.state.activate_toplevel(&surfaces[3]);
        run(&mut fixture, "move-window-to-monitor:right");
        run(&mut fixture, "toggle-fullscreen");
        fixture.state.activate_toplevel(&surfaces[0]);
        run(&mut fixture, "move-window-to-monitor:right");
        assert_eq!(
            fixture.state.fullscreen.contains_key(&surfaces[3]),
            engine == SpatialEngine::Scroll
        );
        assert_eq!(
            fixture.state.focused_window_surface(),
            Some(surfaces[0].clone())
        );
        assert_eq!(
            fixture.state.seat.get_pointer().unwrap().current_focus(),
            Some(surfaces[0].clone())
        );
        run(&mut fixture, "move-window-to-monitor:origin");
        if engine != SpatialEngine::Scroll {
            fixture.state.activate_toplevel(&surfaces[3]);
            run(&mut fixture, "toggle-fullscreen");
        }
        fixture.state.activate_toplevel(&surfaces[0]);
        run(&mut fixture, "toggle-fullscreen");
        run(&mut fixture, "move-window-to-monitor:right");
        assert_eq!(fixture.state.fullscreen[&surfaces[0]].output, "right");
        assert!(!fixture.state.fullscreen.contains_key(&surfaces[3]));
        let toplevel = fixture
            .state
            .mapped_toplevel_window(&surfaces[0])
            .unwrap()
            .toplevel()
            .unwrap()
            .clone();
        assert_eq!(
            toplevel.with_pending_state(|state| state.size),
            Some((900, 700).into())
        );
        run(&mut fixture, "toggle-fullscreen");
        assert_eq!(owner(&fixture, 0), "right");
        run(&mut fixture, "move-window-to-monitor:origin");
        fixture.state.activate_toplevel(&surfaces[3]);
        run(&mut fixture, "move-window-to-monitor:origin");
        // Ordinary floaters, workspace/screen pins and output pins survive.
        fixture.state.activate_toplevel(&surfaces[2]);
        fixture
            .state
            .spatial
            .workspaces
            .output_pins
            .insert(surfaces[2].clone(), "origin".into());
        let floating_size = if engine == SpatialEngine::Ocean {
            fixture
                .state
                .spatial
                .ocean
                .floating_rect(&surfaces[2])
                .unwrap()
                .size
        } else {
            let window = fixture.state.mapped_toplevel_window(&surfaces[2]).unwrap();
            fixture.state.space.element_geometry(&window).unwrap().size
        };
        run(&mut fixture, "move-window-to-monitor:right");
        assert_eq!(owner(&fixture, 2), "right");
        assert_eq!(
            fixture
                .state
                .spatial
                .workspaces
                .output_pins
                .get(&surfaces[2])
                .map(String::as_str),
            Some("right")
        );
        if engine == SpatialEngine::Ocean {
            assert_eq!(
                fixture
                    .state
                    .spatial
                    .ocean
                    .floating_rect(&surfaces[2])
                    .unwrap()
                    .size,
                floating_size
            );
        } else {
            let tag = &fixture.state.spatial.workspaces.floating_workspace[&surfaces[2]];
            assert_eq!(tag.workspace, 3);
            assert_eq!(tag.rect.size, floating_size);
        }
        run(&mut fixture, "move-window-to-monitor:origin");
        run(&mut fixture, "toggle-pin");
        run(&mut fixture, "move-window-to-monitor:right");
        assert!(fixture.state.window_is_pinned(&surfaces[2]));
        assert_eq!(owner(&fixture, 2), "right");
        run(&mut fixture, "move-window-to-monitor:origin");
        assert!(fixture.state.window_is_pinned(&surfaces[2]));
        for fullscreen in [false, true] {
            run(&mut fixture, "toggle-border-fullscreen");
            if fullscreen {
                run(&mut fixture, "toggle-fullscreen");
            }
            let saved_size = fixture.state.maximized[&surfaces[2]].restore_rect.size;
            run(&mut fixture, "move-window-to-monitor:right");
            assert_eq!(fixture.state.maximized[&surfaces[2]].output, "right");
            assert_eq!(
                fixture.state.maximized[&surfaces[2]].restore_rect.size,
                saved_size
            );
            if fullscreen {
                assert_eq!(fixture.state.fullscreen[&surfaces[2]].output, "right");
                assert_eq!(
                    fixture.state.fullscreen[&surfaces[2]]
                        .restore_rect
                        .unwrap()
                        .size,
                    saved_size
                );
                run(&mut fixture, "toggle-fullscreen");
            }
            assert!(fixture.state.window_is_pinned(&surfaces[2]));
            assert!(fixture.state.maximized.contains_key(&surfaces[2]));
            run(&mut fixture, "toggle-border-fullscreen");
            assert!(!fixture.state.maximized.contains_key(&surfaces[2]));
            assert_eq!(owner(&fixture, 2), "right");
            let window = fixture.state.mapped_toplevel_window(&surfaces[2]).unwrap();
            assert_eq!(
                fixture.state.space.element_geometry(&window).unwrap().size,
                floating_size
            );
            run(&mut fixture, "move-window-to-monitor:origin");
        }
        run(&mut fixture, "toggle-pin");
        fixture.state.session_lock = SessionLock::Locked;
        run(&mut fixture, "move-window-to-monitor:right");
        assert_eq!(owner(&fixture, 2), "origin");
        fixture.state.session_lock = SessionLock::Unlocked;
        let pointer = fixture.state.seat.get_pointer().unwrap();
        pointer.button(
            &mut fixture.state,
            &smithay::input::pointer::ButtonEvent {
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: smithay::backend::input::InputTime::now(),
                button: 0x110,
                state: smithay::backend::input::ButtonState::Pressed,
            },
        );
        assert!(pointer.is_grabbed());
        run(&mut fixture, "move-window-to-monitor:right");
        assert_eq!(owner(&fixture, 2), "origin");
        pointer.button(
            &mut fixture.state,
            &smithay::input::pointer::ButtonEvent {
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: smithay::backend::input::InputTime::now(),
                button: 0x110,
                state: smithay::backend::input::ButtonState::Released,
            },
        );
    }

    // Shared Ocean with unequal camera zoom: an actual screen pin retains
    // its viewport size/scale and follows ownership onto the new monitor.
    fixture.state.config.ocean.shared_canvas = true;
    fixture.state.spatial.ocean.set_shared_canvas(true);
    fixture.state.spatial.ocean.set_camera_origin(
        "right",
        crate::ocean::OceanPoint {
            x: 10000.0,
            y: 2000.0,
        },
    );
    fixture
        .state
        .spatial
        .ocean
        .zoom_at("right", (0.0, 0.0).into(), 0.5, Duration::ZERO);
    fixture.state.retile();
    fixture.state.activate_toplevel(&surfaces[2]);
    run(&mut fixture, "toggle-pin");
    let pin = fixture
        .state
        .render_placements(&outputs[0])
        .unwrap()
        .into_iter()
        .find(|p| p.surface() == Some(&surfaces[2]))
        .unwrap();
    run(&mut fixture, "move-window-to-monitor:right");
    let moved = fixture
        .state
        .render_placements(&outputs[2])
        .unwrap()
        .into_iter()
        .find(|p| p.surface() == Some(&surfaces[2]))
        .unwrap();
    assert_eq!(moved.rect.size, pin.rect.size);
    assert_eq!(moved.view_scale, pin.view_scale);
    assert_eq!(
        fixture.state.spatial.ocean.entry_output(&surfaces[2]),
        Some("right")
    );
    assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
    run(&mut fixture, "toggle-pin");
    run(&mut fixture, "move-window-to-monitor:origin");
    // A panned-away destination reef must be revealed, without moving the
    // source camera. This uses the existing Ocean navigation animation.
    fixture.state.activate_toplevel(&surfaces[0]);
    let source_camera = fixture.state.spatial.ocean.camera("origin");
    fixture
        .state
        .run_action(parse_action("move-window-to-monitor:right").unwrap());
    fixture.dispatch();
    assert_eq!(
        fixture.state.spatial.ocean.entry_output(&surfaces[0]),
        Some("right")
    );
    assert_eq!(fixture.state.spatial.ocean.camera("origin"), source_camera);
    assert!(fixture
        .state
        .render_placements(&outputs[2])
        .unwrap()
        .iter()
        .any(|p| {
            p.surface() == Some(&surfaces[0])
                && crate::placement::translated_rect(p.rect, p.view_offset)
                    .overlaps(fixture.state.space.output_geometry(&outputs[2]).unwrap())
        }));
}

#[test]
fn monitor_column_moves_preserve_order_weights_and_event_snapshots() {
    use crate::config::{parse_action, SpatialEngine};
    use crate::state::SessionLock;
    const CHILD: &str = "TIDEWM_MONITOR_COLUMN_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidewm-monitor-column-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), "engine = scroll\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nscroll {\n tile_proportion = 1.0\n}\nanimations {\n enabled = false\n}\n").unwrap();
        let name = concat!(
            module_path!(),
            "::monitor_column_moves_preserve_order_weights_and_event_snapshots"
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
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
    let state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let (server, peer) = UnixStream::pair().unwrap();
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
    let mut fixture = Fixture {
        event_loop,
        state,
        peer,
    };
    let mut outputs = Vec::new();
    // Registration order differs from physical order. Right has a fractional
    // scale: selection and pointer placement must use logical geometry.
    for (name, location, size, scale) in [
        ("origin", (0, 0), (1000, 600), Scale::Integer(1)),
        ("diagonal", (1000, -800), (1000, 600), Scale::Integer(1)),
        ("right", (1000, 0), (1125, 875), Scale::Fractional(1.25)),
        ("below", (0, 800), (1000, 600), Scale::Integer(1)),
    ] {
        let output = Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: size.into(),
                refresh: 60_000,
            }),
            None,
            Some(scale),
            Some(location.into()),
        );
        fixture.state.space.map_output(&output, location);
        outputs.push(output);
    }
    fixture.request(1, 1, &[2], &[]);
    let globals: Vec<_> = fixture
        .dispatch()
        .into_iter()
        .filter_map(|(object, opcode, body)| {
            if object != 2 || opcode != 0 {
                return None;
            }
            let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
            let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            Some((
                String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap(),
                name,
            ))
        })
        .collect();
    for (interface, id, version) in [
        ("wl_compositor", 3_u32, 4_u32),
        ("xdg_wm_base", 4, 1),
        ("wp_single_pixel_buffer_manager_v1", 5, 1),
        ("wp_viewporter", 6, 1),
    ] {
        let name = globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap()
            .1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[name], &tail);
    }
    let mut surfaces = Vec::new();
    for surface in [7_u32, 12, 17, 22, 27, 32] {
        fixture.request(3, 0, &[surface], &[]);
        fixture.request(4, 2, &[surface + 1, surface], &[]);
        fixture.request(surface + 1, 1, &[surface + 2], &[]);
        fixture.request(6, 1, &[surface + 3, surface], &[]);
        fixture.request(surface + 3, 2, &[1000, 600], &[]);
        fixture.request(surface, 6, &[], &[]);
        let serial = fixture
            .dispatch()
            .into_iter()
            .find_map(|(object, opcode, body)| {
                (object == surface + 1 && opcode == 0)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        fixture.request(surface + 1, 4, &[serial], &[]);
        fixture.request(5, 1, &[surface + 4, u32::MAX, 0, 0, u32::MAX], &[]);
        fixture.request(surface, 1, &[surface + 4, 0, 0], &[]);
        fixture.request(surface, 6, &[], &[]);
        fixture.dispatch();
        surfaces.push(surface);
    }
    let surfaces: Vec<_> = surfaces
        .into_iter()
        .map(|id| {
            fixture
                .state
                .space
                .elements()
                .find_map(|window| {
                    let surface = window.toplevel()?.wl_surface();
                    (surface.id().protocol_id() == id).then(|| surface.clone())
                })
                .unwrap()
        })
        .collect();
    fixture
        .state
        .spatial
        .workspaces
        .layout
        .set_active_workspace("right", 3);
    for surface in &surfaces[4..] {
        assert!(fixture
            .state
            .move_tiled_window_to_output(surface, &outputs[2], None));
    }
    {
        let layout = &mut fixture.state.spatial.workspaces.layout;
        let mut source = layout.scroll_columns("origin", 1);
        source[0].windows = surfaces[..3].to_vec();
        source[0].width = 0.72;
        source[1].windows = vec![surfaces[3].clone()];
        source[1].width = 0.4;
        source.truncate(2);
        layout.set_scroll_columns("origin", 1, source);
        for (index, surface) in surfaces[..3].iter().enumerate() {
            layout.scroll.set_weight(surface, (index + 1) as f64);
        }
        let mut destination = layout.scroll_columns("right", 3);
        destination[0].windows = vec![surfaces[4].clone()];
        destination[0].width = 0.31;
        destination[1].windows = vec![surfaces[5].clone()];
        destination[1].width = 0.58;
        layout.set_scroll_columns("right", 3, destination);
    }
    fixture.state.retile();
    let run = |fixture: &mut Fixture, action: &str| {
        fixture.state.run_action(parse_action(action).unwrap());
        // Behave like real clients: ack the last configure and commit its
        // viewport size, so a refused old buffer cannot appear to overlap a
        // neighbor after its tile moved into a smaller destination slot.
        let mut events = fixture.dispatch();
        for _ in 0..16 {
            let mut acknowledged = false;
            for surface in [7_u32, 12, 17, 22, 27, 32] {
                let size = events.iter().rev().find_map(|(object, opcode, body)| {
                    (*object == surface + 2 && *opcode == 0).then(|| {
                        (
                            u32::from_ne_bytes(body[..4].try_into().unwrap()),
                            u32::from_ne_bytes(body[4..8].try_into().unwrap()),
                        )
                    })
                });
                let serial = events.iter().rev().find_map(|(object, opcode, body)| {
                    (*object == surface + 1 && *opcode == 0)
                        .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
                });
                if let Some(serial) = serial {
                    if let Some((width, height)) = size.filter(|(w, h)| *w > 0 && *h > 0) {
                        fixture.request(surface + 3, 2, &[width, height], &[]);
                    }
                    fixture.request(surface + 1, 4, &[serial], &[]);
                    fixture.request(surface, 6, &[], &[]);
                    acknowledged = true;
                }
            }
            if !acknowledged {
                break;
            }
            events = fixture.dispatch();
        }
        fixture.state.refresh_space();
        fixture.state.emit_state_change_events();
    };
    fixture.state.activate_toplevel(&surfaces[5]);
    fixture.state.activate_toplevel(&surfaces[4]);
    fixture.state.activate_toplevel(&surfaces[1]);
    run(&mut fixture, "focus-monitor:right");
    fixture.state.activate_toplevel(&surfaces[1]);
    fixture.state.config.input.focus_follows_mouse = true;
    assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
    fixture
        .state
        .spatial
        .workspaces
        .output_pins
        .insert(surfaces[2].clone(), "origin".into());

    // A real IPC subscriber must see only the final rebuilt column, with
    // no intermediate row-focus changes or per-row destination columns.
    let _ipc = crate::ipc::init(&mut fixture.event_loop).unwrap();
    let mut subscriber = UnixStream::connect(std::env::var_os("TIDEWM_SOCKET").unwrap()).unwrap();
    subscriber
        .write_all(b"{\"request\":\"subscribe\",\"events\":[\"window\",\"focus\",\"scroll\"]}\n")
        .unwrap();
    subscriber.set_nonblocking(true).unwrap();
    for _ in 0..8 {
        fixture.dispatch();
        if !fixture.state.ipc.subscribers.is_empty() {
            break;
        }
    }
    fixture.state.emit_state_change_events();
    let drain = |subscriber: &mut UnixStream| {
        let mut bytes = Vec::new();
        match subscriber.read_to_end(&mut bytes) {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("unexpected subscriber read: {other:?}"),
        }
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    let ack = drain(&mut subscriber);
    assert!(ack.iter().any(|line| line["ok"] == true), "{ack:?}");
    let row_ids: Vec<_> = surfaces[..3]
        .iter()
        .map(|surface| fixture.state.foreign_toplevel_numeric_ids[surface])
        .collect();
    run(&mut fixture, "move-column-to-monitor-right");
    let columns = fixture
        .state
        .spatial
        .workspaces
        .layout
        .scroll_columns("right", 3);
    assert_eq!(columns.len(), 3);
    assert_eq!(columns[0].windows, surfaces[4..5]);
    assert_eq!(columns[0].width, 0.31);
    assert_eq!(columns[1].windows, surfaces[..3]);
    assert_eq!(columns[1].width, 0.72);
    assert_eq!(columns[2].windows, surfaces[5..6]);
    assert_eq!(columns[2].width, 0.58);
    let source = fixture
        .state
        .spatial
        .workspaces
        .layout
        .scroll_columns("origin", 1);
    assert_eq!(source.len(), 1);
    assert_eq!(source[0].windows, surfaces[3..4]);
    assert_eq!(source[0].width, 0.4);
    assert_eq!(
        fixture.state.focused_window_surface(),
        Some(surfaces[1].clone())
    );
    assert_eq!(
        fixture.state.seat.get_pointer().unwrap().current_focus(),
        Some(surfaces[1].clone())
    );
    assert_eq!(
        fixture.state.spatial.workspaces.output_pins[&surfaces[2]],
        "right"
    );
    for (row, surface) in surfaces[..3].iter().enumerate() {
        assert_eq!(
            fixture
                .state
                .spatial
                .workspaces
                .layout
                .scroll
                .weight(surface),
            (row + 1) as f64
        );
    }
    let events = drain(&mut subscriber);
    assert!(!events.iter().any(|event| event["event"] == "focus-changed"));
    for (row, id) in row_ids.iter().enumerate() {
        let snapshots: Vec<_> = events
            .iter()
            .filter(|event| event["event"] == "window-changed" && event["data"]["window_id"] == *id)
            .collect();
        assert!(!snapshots.is_empty(), "missing row event");
        for event in snapshots {
            assert_eq!(event["data"]["output"], "right");
            assert_eq!(event["data"]["workspace"], 3);
            assert_eq!(event["data"]["scroll"]["column"], 1);
            assert_eq!(event["data"]["scroll"]["row"], row);
            assert_eq!(event["data"]["scroll"]["column_width"], 0.72);
            assert_eq!(event["data"]["scroll"]["row_weight"], (row + 1) as f64);
        }
    }
    assert!(events
        .iter()
        .any(|event| event["event"] == "scroll-changed"));
    drop(subscriber);
    fixture.dispatch();
    // Same/missing monitor, strip edge and active grabs/lock are no-ops.
    let saved = fixture
        .state
        .spatial
        .workspaces
        .layout
        .scroll_columns("right", 3);
    let location = fixture.state.seat.get_pointer().unwrap().current_location();
    for action in [
        "move-column-to-monitor-right",
        "move-column-to-monitor:right",
        "move-column-to-monitor:missing",
    ] {
        run(&mut fixture, action);
        assert_eq!(
            fixture
                .state
                .spatial
                .workspaces
                .layout
                .scroll_columns("right", 3),
            saved
        );
        assert_eq!(
            fixture.state.seat.get_pointer().unwrap().current_location(),
            location
        );
    }
    fixture.state.session_lock = SessionLock::Locked;
    run(&mut fixture, "move-column-to-monitor-left");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .scroll_columns("right", 3),
        saved
    );
    fixture.state.session_lock = SessionLock::Unlocked;
    let pointer = fixture.state.seat.get_pointer().unwrap();
    pointer.button(
        &mut fixture.state,
        &smithay::input::pointer::ButtonEvent {
            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
            time: smithay::backend::input::InputTime::now(),
            button: 0x110,
            state: smithay::backend::input::ButtonState::Pressed,
        },
    );
    assert!(pointer.is_grabbed());
    run(&mut fixture, "move-column-to-monitor-left");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .scroll_columns("right", 3),
        saved
    );
    pointer.button(
        &mut fixture.state,
        &smithay::input::pointer::ButtonEvent {
            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
            time: smithay::backend::input::InputTime::now(),
            button: 0x110,
            state: smithay::backend::input::ButtonState::Released,
        },
    );
    fixture.state.config.input.focus_follows_mouse = false;
    run(&mut fixture, "move-column-to-monitor-left");
    run(&mut fixture, "move-column-to-monitor-down");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&surfaces[1]),
        Some("below")
    );
    run(&mut fixture, "move-column-to-monitor-up");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&surfaces[1]),
        Some("origin")
    );
    // A tabbed column remains one group, preserving tab order and active tab.
    run(&mut fixture, "toggle-column-tabbed");
    run(&mut fixture, "move-column-to-monitor:right");
    let index = fixture.state.group_of(&surfaces[1]).unwrap();
    assert_eq!(fixture.state.groups[index].output, "right");
    assert_eq!(fixture.state.groups[index].workspace, 3);
    assert_eq!(
        fixture.state.groups[index]
            .members
            .iter()
            .map(|member| member.surface.clone())
            .collect::<Vec<_>>(),
        surfaces[..3]
    );
    assert_eq!(
        fixture.state.groups[index].members[fixture.state.groups[index].active].surface,
        surfaces[1]
    );
    assert!(!fixture
        .state
        .spatial
        .workspaces
        .layout
        .contains(&surfaces[0]));
    assert!(!fixture
        .state
        .spatial
        .workspaces
        .layout
        .contains(&surfaces[2]));
    run(&mut fixture, "toggle-column-tabbed");
    let restored = fixture
        .state
        .spatial
        .workspaces
        .layout
        .scroll_columns("right", 3)
        .into_iter()
        .find(|c| c.windows.contains(&surfaces[1]))
        .unwrap();
    assert_eq!(restored.windows, surfaces[..3]);
    assert_eq!(restored.width, 0.72);
    // Fullscreen in an incoming stacked column preempts the target owner.
    fixture.state.activate_toplevel(&surfaces[4]);
    run(&mut fixture, "toggle-fullscreen");
    fixture.state.activate_toplevel(&surfaces[1]);
    run(&mut fixture, "move-column-to-monitor:origin");
    run(&mut fixture, "toggle-fullscreen");
    run(&mut fixture, "move-column-to-monitor:right");
    assert_eq!(fixture.state.fullscreen[&surfaces[1]].output, "right");
    assert!(!fixture.state.fullscreen.contains_key(&surfaces[4]));
    assert_eq!(
        fixture
            .state
            .mapped_toplevel_window(&surfaces[1])
            .unwrap()
            .toplevel()
            .unwrap()
            .with_pending_state(|state| state.size),
        Some((900, 700).into())
    );
    run(&mut fixture, "toggle-fullscreen");
    // Overflowing stack keeps its ordered rows and reveals the selected one.
    run(&mut fixture, "move-column-to-monitor:origin");
    fixture.state.config.scroll.stack = crate::scroll::StackMode::Scroll;
    fixture.state.config.scroll.stack_height = 0.8;
    fixture.state.retile();
    fixture.state.activate_toplevel(&surfaces[2]);
    run(&mut fixture, "move-column-to-monitor:right");
    assert_eq!(
        fixture.state.focused_window_surface(),
        Some(surfaces[2].clone())
    );
    assert!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .scroll
            .stack_offset(&surfaces[0])
            > 0.0
    );
    assert_eq!(
        fixture.state.seat.get_pointer().unwrap().current_focus(),
        Some(surfaces[2].clone())
    );
    // The same transfer works across sectioned workspaces and with a
    // vertical strip: physical monitor directions do not rotate with it.
    fixture.state.config.scroll.workspaces = crate::scroll::WorkspaceMode::Sections;
    fixture.state.config.scroll.orientation = crate::scroll::Orientation::Vertical;
    fixture.state.retile();
    run(&mut fixture, "move-column-to-monitor:origin");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .workspace_of(&surfaces[2]),
        Some(1)
    );
    run(&mut fixture, "move-column-to-monitor-right");
    let vertical = fixture
        .state
        .spatial
        .workspaces
        .layout
        .scroll_columns("right", 3)
        .into_iter()
        .find(|column| column.windows.contains(&surfaces[2]))
        .unwrap();
    assert_eq!(vertical.windows, surfaces[..3]);
    assert_eq!(vertical.width, 0.72);
    assert_eq!(
        fixture.state.focused_window_surface(),
        Some(surfaces[2].clone())
    );
    fixture.state.config.scroll.workspaces = crate::scroll::WorkspaceMode::Separate;
    fixture.state.config.scroll.orientation = crate::scroll::Orientation::Horizontal;
    fixture.state.retile();
    // A focused floater uses the single-window transfer path.
    fixture.state.activate_toplevel(&surfaces[3]);
    run(&mut fixture, "toggle-floating");
    let columns_before = fixture
        .state
        .spatial
        .workspaces
        .layout
        .scroll_columns("right", 3);
    run(&mut fixture, "move-column-to-monitor:right");
    assert_eq!(
        fixture.state.spatial.workspaces.floating_workspace[&surfaces[3]].output,
        "right"
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .scroll_columns("right", 3),
        columns_before
    );
    // Other engines do not reinterpret their layout as a Scroll column.
    for engine in [SpatialEngine::Classic, SpatialEngine::Ocean] {
        assert!(fixture.state.migrate_spatial_engine(engine));
        let before = fixture.state.preferred_output_for_toplevel(&surfaces[3]);
        run(&mut fixture, "move-column-to-monitor:origin");
        assert_eq!(
            fixture.state.preferred_output_for_toplevel(&surfaces[3]),
            before
        );
    }
}

#[test]
fn monitor_workspace_moves_keep_slots_layouts_and_parked_owners() {
    use crate::config::{parse_action, LayoutAlgorithm, SpatialEngine};
    use crate::state::SessionLock;
    const CHILD: &str = "TIDEWM_MONITOR_WORKSPACE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidewm-monitor-workspace-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), "engine = scroll\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nscroll {\n tile_proportion = 1.0\n}\nanimations {\n enabled = false\n}\n").unwrap();
        let name = concat!(
            module_path!(),
            "::monitor_workspace_moves_keep_slots_layouts_and_parked_owners"
        );
        for mode in ["classic", "scroll", "sections", "vertical"] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
                .env(CHILD, "1")
                .env("TIDEWM_WORKSPACE_MODE", mode)
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_RUNTIME_DIR", root.join("runtime"))
                .output()
                .unwrap();

            assert!(String::from_utf8_lossy(&result.stdout).contains("running 1 test"));
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
        std::fs::remove_dir_all(root).unwrap();
        return;
    }
    let mut event_loop = EventLoop::try_new().unwrap();
    let state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let (server, peer) = UnixStream::pair().unwrap();
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
    let mut fixture = Fixture {
        event_loop,
        state,
        peer,
    };
    let mut outputs = Vec::new();
    // Registration order differs from physical order. Right has a fractional
    // scale: selection and pointer placement must use logical geometry.
    for (name, location, size, scale) in [
        ("origin", (0, 0), (1000, 600), Scale::Integer(1)),
        ("diagonal", (1000, -800), (1000, 600), Scale::Integer(1)),
        ("right", (1000, 0), (1125, 875), Scale::Fractional(1.25)),
        ("below", (0, 800), (1000, 600), Scale::Integer(1)),
    ] {
        let output = Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: size.into(),
                refresh: 60_000,
            }),
            None,
            Some(scale),
            Some(location.into()),
        );
        fixture.state.space.map_output(&output, location);
        outputs.push(output);
    }
    fixture.request(1, 1, &[2], &[]);
    let globals: Vec<_> = fixture
        .dispatch()
        .into_iter()
        .filter_map(|(object, opcode, body)| {
            if object != 2 || opcode != 0 {
                return None;
            }
            let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
            let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            Some((
                String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap(),
                name,
            ))
        })
        .collect();
    for (interface, id, version) in [
        ("wl_compositor", 3_u32, 4_u32),
        ("xdg_wm_base", 4, 1),
        ("wp_single_pixel_buffer_manager_v1", 5, 1),
        ("wp_viewporter", 6, 1),
    ] {
        let name = globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap()
            .1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[name], &tail);
    }
    let mut surfaces = Vec::new();
    for surface in [7_u32, 12, 17, 22, 27, 32, 37, 42, 47, 52, 57, 62, 67] {
        fixture.request(3, 0, &[surface], &[]);
        fixture.request(4, 2, &[surface + 1, surface], &[]);
        fixture.request(surface + 1, 1, &[surface + 2], &[]);
        fixture.request(6, 1, &[surface + 3, surface], &[]);
        fixture.request(surface + 3, 2, &[1000, 600], &[]);
        fixture.request(surface, 6, &[], &[]);
        let serial = fixture
            .dispatch()
            .into_iter()
            .find_map(|(object, opcode, body)| {
                (object == surface + 1 && opcode == 0)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        fixture.request(surface + 1, 4, &[serial], &[]);
        fixture.request(5, 1, &[surface + 4, u32::MAX, 0, 0, u32::MAX], &[]);
        fixture.request(surface, 1, &[surface + 4, 0, 0], &[]);
        fixture.request(surface, 6, &[], &[]);
        fixture.dispatch();
        surfaces.push(surface);
    }
    let surfaces: Vec<_> = surfaces
        .into_iter()
        .map(|id| {
            fixture
                .state
                .space
                .elements()
                .find_map(|window| {
                    let surface = window.toplevel()?.wl_surface();
                    (surface.id().protocol_id() == id).then(|| surface.clone())
                })
                .unwrap()
        })
        .collect();
    let mode = std::env::var("TIDEWM_WORKSPACE_MODE").unwrap();
    fixture.state.config.ocean.camera_animation_ms = 0;
    fixture.state.config.classic_depth.enabled = true;
    fixture.state.config.workspace_count = Some(1);
    fixture.state.config.input.focus_follows_mouse = false;
    if mode == "classic" {
        assert!(fixture.state.migrate_spatial_engine(SpatialEngine::Classic));
    }
    if mode == "sections" || mode == "vertical" {
        fixture.state.config.scroll.workspaces = crate::scroll::WorkspaceMode::Sections;
        fixture.state.config.scroll.float_anchor = crate::scroll::FloatAnchor::Strip;
    }
    if mode == "vertical" {
        fixture.state.config.scroll.orientation = crate::scroll::Orientation::Vertical;
    }
    for surface in &surfaces {
        fixture.state.move_to_workspace(surface, 4);
    }
    fixture.state.switch_workspace_immediate(&outputs[0], 4);
    let park =
        |fixture: &mut Fixture,
         surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
         output: &str,
         workspace: u32| {
            let window = fixture
                .state
                .spatial
                .workspaces
                .layout
                .window_of(surface)
                .unwrap();
            fixture.state.spatial.workspaces.layout.remove(surface);
            fixture.state.space.unmap_elem(&window);
            fixture.state.spatial.workspaces.classic_depth.park(
                crate::classic_depth::DepthDeckEntry {
                    surface: surface.clone(),
                    window,
                    output: output.into(),
                    workspace,
                },
            );
        };
    let run = |fixture: &mut Fixture, action: &str| {
        fixture.state.run_action(parse_action(action).unwrap());
        // Behave like real clients: ack the last configure and commit its
        // viewport size, so a refused old buffer cannot appear to overlap a
        // neighbor after its tile moved into a smaller destination slot.
        let mut events = fixture.dispatch();
        for _ in 0..16 {
            let mut acknowledged = false;
            for surface in [7_u32, 12, 17, 22, 27, 32, 37, 42, 47, 52, 57, 62, 67] {
                let size = events.iter().rev().find_map(|(object, opcode, body)| {
                    (*object == surface + 2 && *opcode == 0).then(|| {
                        (
                            u32::from_ne_bytes(body[..4].try_into().unwrap()),
                            u32::from_ne_bytes(body[4..8].try_into().unwrap()),
                        )
                    })
                });
                let serial = events.iter().rev().find_map(|(object, opcode, body)| {
                    (*object == surface + 1 && *opcode == 0)
                        .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
                });
                if let Some(serial) = serial {
                    if let Some((width, height)) = size.filter(|(w, h)| *w > 0 && *h > 0) {
                        fixture.request(surface + 3, 2, &[width, height], &[]);
                    }
                    fixture.request(surface + 1, 4, &[serial], &[]);
                    fixture.request(surface, 6, &[], &[]);
                    acknowledged = true;
                }
            }
            if !acknowledged {
                break;
            }
            events = fixture.dispatch();
        }
        fixture.state.refresh_space();
        fixture.state.emit_state_change_events();
    };
    // Populate destination slots with each ownership kind, including hidden
    // residents. Slot 5 has only a sticky floater and must remain available.
    for (index, workspace) in [(7, 1), (8, 2), (9, 3), (6, 4), (10, 5)] {
        fixture
            .state
            .switch_workspace_immediate(&outputs[2], workspace);
        assert!(fixture
            .state
            .move_tiled_window_to_output(&surfaces[index], &outputs[2], None));
        fixture.state.retile();
        fixture.state.activate_toplevel(&surfaces[index]);
        if index == 7 || index == 10 {
            run(&mut fixture, "toggle-floating");
        }
        if index == 8 {
            park(&mut fixture, &surfaces[index], "right", workspace);
        }
        if index == 9 {
            run(&mut fixture, "toggle-fullscreen");
        }
        if index == 10 {
            run(&mut fixture, "toggle-pin");
        }
    }
    fixture.state.switch_workspace_immediate(&outputs[2], 4);
    assert!(
        fixture.state.advertised_workspaces("right").contains(&2),
        "hidden deck-only workspace above workspace_count must remain advertised"
    );
    fixture.state.move_to_workspace(&surfaces[11], 6);
    park(&mut fixture, &surfaces[5], "origin", 4);
    fixture.state.group_with(&surfaces[0], &surfaces[1]);
    for index in [3, 4] {
        fixture.state.activate_toplevel(&surfaces[index]);
        run(&mut fixture, "toggle-floating");
        let window = fixture
            .state
            .mapped_toplevel_window(&surfaces[index])
            .unwrap();
        window
            .toplevel()
            .unwrap()
            .with_pending_state(|state| state.size = Some((240, 160).into()));
        window.toplevel().unwrap().send_pending_configure();
        fixture
            .state
            .space
            .map_element(window, (80 + index as i32 * 4, 120), false);
        run(&mut fixture, "focus-monitor:origin");
        fixture.state.activate_toplevel(&surfaces[index]);
        if index == 3 {
            run(&mut fixture, "toggle-border-fullscreen");
        }
        if index == 4 {
            run(&mut fixture, "toggle-pin");
        }
    }
    {
        let layout = &mut fixture.state.spatial.workspaces.layout;
        layout.set_algorithm("origin", 4, LayoutAlgorithm::Master);
        layout.adjust_master_ratio("origin", 4, 0.17);
        layout.set_ratio("origin", 4, &[], 0.37);
        if mode != "classic" {
            let mut columns = layout.scroll_columns("origin", 4);
            columns[0].windows = vec![surfaces[0].clone(), surfaces[2].clone()];
            columns[0].width = 0.7;
            columns[1].windows = vec![surfaces[12].clone()];
            columns[1].width = 0.8;
            columns.truncate(2);
            layout.set_scroll_columns("origin", 4, columns);
            layout.scroll.set_weight(&surfaces[2], 2.5);
        }
    }
    fixture.state.retile();
    fixture.state.activate_toplevel(&surfaces[0]);
    run(&mut fixture, "focus-monitor:origin");
    if mode != "classic" {
        let key = fixture.state.spatial.workspaces.layout.scroll_key(4);
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .scroll
            .set_offset("origin", key, 100.0);
        fixture.state.retile();
    }
    run(&mut fixture, "move-workspace-to-monitor:origin"); // self no-op; settle clients
    let sticky = fixture
        .state
        .space
        .element_geometry(&fixture.state.mapped_toplevel_window(&surfaces[4]).unwrap())
        .unwrap();
    let tree_order: Vec<_> = fixture
        .state
        .spatial
        .workspaces
        .layout
        .windows_in("origin", 4)
        .iter()
        .map(|window| window.toplevel().unwrap().wl_surface().clone())
        .collect();
    let saved_ratio = fixture
        .state
        .spatial
        .workspaces
        .layout
        .ratio_at("origin", 4, &[]);
    let saved_max_size = fixture.state.maximized[&surfaces[3]].restore_rect.size;
    let relative_float = |fixture: &Fixture, output: &str, workspace: u32| {
        let geo = fixture
            .state
            .space
            .output_geometry(&fixture.state.output_by_name(output).unwrap())
            .unwrap();
        let rect = fixture.state.maximized[&surfaces[3]].restore_rect;
        let axis = if mode == "vertical" {
            f64::from(rect.loc.y - geo.loc.y)
        } else {
            f64::from(rect.loc.x - geo.loc.x)
        };
        let layout = &fixture.state.spatial.workspaces.layout;
        axis + layout.scroll.offset(output, layout.scroll_key(workspace))
            - layout
                .scroll_workspace_start(output, workspace, geo, 0)
                .unwrap_or(0.0)
    };
    let relative_before = relative_float(&fixture, "origin", 4);
    for index in [1, 5] {
        fixture
            .state
            .spatial
            .workspaces
            .output_pins
            .insert(surfaces[index].clone(), "origin".into());
    }
    run(&mut fixture, "move-workspace-to-monitor-right");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .active_workspace("right"),
        5,
        "{mode}"
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .active_workspace("origin"),
        4
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .window_count("origin", 4),
        0
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .algorithm("right", 5),
        LayoutAlgorithm::Master
    );
    assert!(
        (fixture
            .state
            .spatial
            .workspaces
            .layout
            .master_ratio("right", 5)
            - 0.67)
            .abs()
            < 0.001
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .ratio_at("right", 5, &[]),
        saved_ratio
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .windows_in("right", 5)
            .iter()
            .map(|window| window.toplevel().unwrap().wl_surface().clone())
            .collect::<Vec<_>>(),
        tree_order
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .algorithm("origin", 4),
        LayoutAlgorithm::Bsp
    );
    if mode != "classic" {
        let columns = fixture
            .state
            .spatial
            .workspaces
            .layout
            .scroll_columns("right", 5);
        assert_eq!(
            columns[0].windows,
            vec![surfaces[0].clone(), surfaces[2].clone()]
        );
        assert_eq!(columns[0].width, 0.7);
        assert_eq!(columns[1].width, 0.8);
        assert_eq!(
            fixture
                .state
                .spatial
                .workspaces
                .layout
                .scroll
                .weight(&surfaces[2]),
            2.5
        );
        if mode == "scroll" {
            assert_eq!(
                fixture
                    .state
                    .spatial
                    .workspaces
                    .layout
                    .scroll
                    .offset("right", 5),
                100.0
            );
            assert!(!fixture
                .state
                .spatial
                .workspaces
                .layout
                .scroll
                .has_offset("origin", 4));
        }
    }
    assert_eq!(
        fixture.state.focused_window_surface(),
        Some(surfaces[0].clone())
    );
    assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
    assert_eq!(
        fixture.state.spatial.workspaces.floating_workspace[&surfaces[3]].output,
        "right"
    );
    assert_eq!(
        fixture.state.spatial.workspaces.floating_workspace[&surfaces[3]].workspace,
        5
    );
    assert_eq!(fixture.state.maximized[&surfaces[3]].output, "right");
    assert_eq!(
        fixture.state.maximized[&surfaces[3]].restore_rect.size,
        saved_max_size
    );
    if mode == "sections" || mode == "vertical" {
        assert!(
            (relative_float(&fixture, "right", 5) - relative_before).abs() < 1.0,
            "strip floater's restore position must follow its workspace section: {mode}; before={relative_before}; after={}; tag={:?}; restore={:?}", relative_float(&fixture, "right", 5), fixture.state.spatial.workspaces.floating_workspace[&surfaces[3]].rect, fixture.state.maximized[&surfaces[3]].restore_rect
        );
    }
    let group = fixture.state.group_of(&surfaces[1]).unwrap();
    assert_eq!(fixture.state.groups[group].output, "right");
    assert_eq!(fixture.state.groups[group].workspace, 5);
    assert!(!fixture
        .state
        .spatial
        .workspaces
        .layout
        .contains(&surfaces[1]));
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .classic_depth
            .entry(&surfaces[5])
            .unwrap()
            .output,
        "right"
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .classic_depth
            .entry(&surfaces[5])
            .unwrap()
            .workspace,
        5
    );
    assert!(!fixture.state.window_is_visible(&surfaces[5]));
    for index in [1, 5] {
        assert_eq!(
            fixture.state.spatial.workspaces.output_pins[&surfaces[index]],
            "right"
        );
    }
    assert_eq!(
        fixture.state.spatial.workspaces.floating_workspace[&surfaces[4]].output,
        "origin"
    );
    assert_eq!(
        fixture
            .state
            .space
            .element_geometry(&fixture.state.mapped_toplevel_window(&surfaces[4]).unwrap())
            .unwrap(),
        sticky
    );
    assert_eq!(
        fixture.state.spatial.workspaces.floating_workspace[&surfaces[10]].output,
        "right"
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .workspace_of(&surfaces[6]),
        Some(4)
    );
    assert_eq!(
        fixture.state.spatial.workspaces.floating_workspace[&surfaces[7]].workspace,
        1
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .classic_depth
            .entry(&surfaces[8])
            .unwrap()
            .workspace,
        2
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .workspace_of(&surfaces[9]),
        Some(3)
    );
    assert!(fixture.state.fullscreen.contains_key(&surfaces[9]));
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&surfaces[11]),
        Some("origin")
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .workspace_of(&surfaces[11]),
        Some(6)
    );
    // Guards and monitor edge leave every resident in place.
    let mut owners = fixture
        .state
        .spatial
        .workspaces
        .layout
        .populated_workspaces();
    owners.sort();
    for action in [
        "move-workspace-to-monitor-right",
        "move-workspace-to-monitor:right",
        "move-workspace-to-monitor:missing",
    ] {
        run(&mut fixture, action);
        let mut now = fixture
            .state
            .spatial
            .workspaces
            .layout
            .populated_workspaces();
        now.sort();
        assert_eq!(now, owners);
    }
    fixture.state.session_lock = SessionLock::Locked;
    run(&mut fixture, "move-workspace-to-monitor:origin");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&surfaces[0]),
        Some("right")
    );
    fixture.state.session_lock = SessionLock::Unlocked;
    let pointer = fixture.state.seat.get_pointer().unwrap();
    pointer.button(
        &mut fixture.state,
        &smithay::input::pointer::ButtonEvent {
            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
            time: smithay::backend::input::InputTime::now(),
            button: 0x110,
            state: smithay::backend::input::ButtonState::Pressed,
        },
    );
    assert!(pointer.is_grabbed());
    run(&mut fixture, "move-workspace-to-monitor:origin");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&surfaces[0]),
        Some("right")
    );
    pointer.button(
        &mut fixture.state,
        &smithay::input::pointer::ButtonEvent {
            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
            time: smithay::backend::input::InputTime::now(),
            button: 0x110,
            state: smithay::backend::input::ButtonState::Released,
        },
    );
    // Free destinations keep the workspace number; physical directions work
    // regardless of Scroll orientation. Source sticky windows stay put.
    fixture.state.config.input.focus_follows_mouse = true;
    run(&mut fixture, "move-workspace-to-monitor-left");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .workspace_of(&surfaces[0]),
        Some(5)
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&surfaces[0]),
        Some("origin")
    );
    run(&mut fixture, "move-workspace-to-monitor-down");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&surfaces[0]),
        Some("below")
    );
    run(&mut fixture, "move-workspace-to-monitor-up");
    run(&mut fixture, "move-workspace-to-monitor-right");
    // Put the old hidden fullscreen on the destination, then transfer a
    // fullscreen workspace into a different free slot on that output.
    fixture.state.activate_toplevel(&surfaces[9]);
    run(&mut fixture, "move-window-to-monitor:origin");
    fixture.state.activate_toplevel(&surfaces[0]);
    run(&mut fixture, "toggle-fullscreen");
    run(&mut fixture, "focus-monitor:right");
    run(&mut fixture, "move-workspace-to-monitor:origin");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .active_workspace("origin"),
        1
    );
    assert_eq!(fixture.state.fullscreen[&surfaces[0]].output, "origin");
    assert!(!fixture.state.fullscreen.contains_key(&surfaces[9]));
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .workspace_of(&surfaces[9]),
        Some(5)
    );
    assert_eq!(
        fixture
            .state
            .mapped_toplevel_window(&surfaces[0])
            .unwrap()
            .toplevel()
            .unwrap()
            .with_pending_state(|state| state.size),
        Some((1000, 600).into())
    );
    run(&mut fixture, "toggle-fullscreen");
    fixture.state.ungroup(&surfaces[1]);
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&surfaces[1]),
        Some("origin")
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .workspace_of(&surfaces[1]),
        Some(1)
    );
    fixture.state.activate_toplevel(&surfaces[5]);
    assert!(fixture
        .state
        .spatial
        .workspaces
        .classic_depth
        .entry(&surfaces[5])
        .is_none());
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .workspace_of(&surfaces[5]),
        Some(1)
    );
    // Fullscreen suspends stickiness: this client now belongs to the active
    // workspace, moves with it, then restores its pin on the destination.
    fixture.state.activate_toplevel(&surfaces[4]);
    let window = fixture.state.mapped_toplevel_window(&surfaces[4]).unwrap();
    fixture
        .state
        .space
        .map_element(window.clone(), (-100, 40), false);
    fixture
        .state
        .spatial
        .workspaces
        .floating_workspace
        .get_mut(&surfaces[4])
        .unwrap()
        .rect = fixture.state.space.element_geometry(&window).unwrap();
    run(&mut fixture, "toggle-fullscreen");
    assert!(fixture.state.fullscreen[&surfaces[4]].was_pinned);
    run(&mut fixture, "move-workspace-to-monitor:right");
    assert_eq!(fixture.state.fullscreen[&surfaces[4]].output, "right");
    run(&mut fixture, "toggle-fullscreen");
    assert!(fixture.state.window_is_pinned(&surfaces[4]));
    assert_eq!(
        fixture.state.spatial.workspaces.floating_workspace[&surfaces[4]].output,
        "right"
    );
    // Empty slots do not materialize empty trees or carry sticky floaters.
    fixture.state.switch_workspace_immediate(&outputs[0], 20);
    run(&mut fixture, "focus-monitor:origin");
    let trees = fixture
        .state
        .spatial
        .workspaces
        .layout
        .populated_workspaces()
        .len();
    run(&mut fixture, "move-workspace-to-monitor:right");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .active_workspace("right"),
        20
    );
    assert_eq!(fixture.state.focused_window_surface(), None);
    assert_eq!(fixture.state.primary_output(), Some(outputs[2].clone()));
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .populated_workspaces()
            .len(),
        trees
    );
    fixture.state.switch_workspace_immediate(&outputs[2], 0);
    run(&mut fixture, "move-workspace-to-monitor:origin");
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .active_workspace("right"),
        0
    );
    fixture.state.switch_workspace_immediate(&outputs[2], 20);
    assert!(fixture.state.migrate_spatial_engine(SpatialEngine::Ocean));
    let before = fixture
        .state
        .spatial
        .ocean
        .entry_output(&surfaces[0])
        .map(str::to_owned);
    run(&mut fixture, "move-workspace-to-monitor:origin");
    assert_eq!(
        fixture
            .state
            .spatial
            .ocean
            .entry_output(&surfaces[0])
            .map(str::to_owned),
        before
    );
    assert_eq!(fixture.state.foreign_toplevel_numeric_ids.len(), 13);
}
