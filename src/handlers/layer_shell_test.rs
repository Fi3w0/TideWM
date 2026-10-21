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
    fixture.state.space.refresh();
    fixture.state.classic_tile_drag = Some(surface.clone());
    assert!(fixture
        .state
        .render_placements(destination)
        .unwrap()
        .iter()
        .any(|p| p.surface() == Some(&surface)));
    fixture.state.classic_tile_drag = None;
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
        fixture.state.layout.output_of(&surface),
        Some(destination.name().as_str())
    );
    assert_eq!(
        fixture.state.layout.workspace_of(&surface),
        Some(fixture.state.layout.active_workspace(&destination.name()))
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
        .layout
        .set_active_workspace(&outputs[1].name(), 2);
    fixture
        .state
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
        fixture.state.output_pins.get(&other),
        Some(&outputs[1].name())
    );
    assert!(
        !fixture.state.layout.contains(&other),
        "parked tabs must stay outside the tree"
    );
    fixture.state.ungroup(&other);
    assert_eq!(
        fixture.state.layout.output_of(&other),
        Some(outputs[1].name().as_str())
    );
    assert_eq!(fixture.state.layout.workspace_of(&other), Some(2));
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
