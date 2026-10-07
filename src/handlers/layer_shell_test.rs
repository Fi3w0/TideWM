//! Exercise real layer commits against Smallvil, without a renderer or DRM seat.
use crate::{state::ClientState, Smallvil};
use smithay::{
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{calloop::EventLoop, wayland_server::Display},
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
}
