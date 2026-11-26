//! Virtual outputs against a real Wayland client: a `virtual_output` block
//! becomes a real `wl_output` global and a place in the desktop, the pointer
//! and directional monitor moves never land on it, a window sent there by
//! name comes back to a visible monitor when it is removed, and config
//! reload removes a block's output.

use crate::{config::parse_action, state::ClientState, Smallvil};
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

/// Registry `global` events naming `wl_output`, as `(name, global id)`.
fn wl_output_globals(events: &[(u32, u16, Vec<u8>)]) -> Vec<u32> {
    events
        .iter()
        .filter(|(object, opcode, _)| (*object, *opcode) == (2, 0))
        .filter_map(|(_, _, body)| {
            let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
            let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
            (&body[8..8 + length - 1] == b"wl_output").then_some(name)
        })
        .collect()
}

const CONFIG: &str = "engine = classic\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nanimations {\n enabled = false\n}\nvirtual_output TV {\n mode = 1280x720@30\n}\n";

#[test]
fn virtual_outputs_join_and_leave_the_desktop() {
    const CHILD: &str = "TIDEWM_VIRTUAL_OUTPUT_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidewm-virtual-output-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), CONFIG).unwrap();
        let name = concat!(
            module_path!(),
            "::virtual_outputs_join_and_leave_the_desktop"
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
    let config_dir = std::path::PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap());

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

    // One real monitor, as a backend would create it.
    let real = Output::new(
        "REAL-1".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
            serial_number: "test".into(),
        },
    );
    real.change_current_state(
        Some(Mode {
            size: (1000, 600).into(),
            refresh: 60_000,
        }),
        None,
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    real.create_global::<Smallvil>(&fixture.state.display_handle);
    fixture.state.space.map_output(&real, (0, 0));

    fixture.request(1, 1, &[2], &[]); // get_registry
    let before = wl_output_globals(&fixture.dispatch());
    assert_eq!(before.len(), 1, "only the real monitor is advertised");

    // Startup creates config-owned virtual outputs after the backend.
    fixture.state.reconcile_config_virtual_outputs();
    let added = wl_output_globals(&fixture.dispatch());
    assert_eq!(added.len(), 1, "the virtual output gets its own wl_output");
    let tv = fixture.state.output_by_name("TV").expect("TV exists");
    assert!(fixture.state.is_virtual_output(&tv));
    assert!(!fixture.state.is_virtual_output(&real));
    let mode = tv.current_mode().unwrap();
    assert_eq!(
        (mode.size.w, mode.size.h, mode.refresh),
        (1280, 720, 30_000)
    );
    let tv_geometry = fixture.state.space.output_geometry(&tv).unwrap();
    assert!(
        !tv_geometry.overlaps(fixture.state.space.output_geometry(&real).unwrap()),
        "automatic placement keeps the virtual output beside the monitor"
    );
    let outputs = crate::ipc::outputs_json(&fixture.state);
    let flags: Vec<_> = outputs
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap().to_owned(),
                entry["virtual"].clone(),
            )
        })
        .collect();
    assert!(flags.contains(&("TV".to_owned(), serde_json::json!(true))));
    assert!(flags.contains(&("REAL-1".to_owned(), serde_json::json!(false))));

    // A monitor that reconnects is appended to Space's insertion-ordered
    // output list, which can leave a virtual output first. With no focused
    // window or output under the pointer, the fallback must still pick a
    // monitor a person can see.
    fixture.state.space.unmap_output(&real);
    fixture.state.space.map_output(&real, (5000, 0));
    assert_eq!(
        fixture.state.space.outputs().next().map(Output::name),
        Some("TV".to_owned())
    );
    assert_eq!(
        fixture
            .state
            .primary_output()
            .map(|output| output.name().to_string()),
        Some("REAL-1".to_owned()),
        "the new-window fallback chose a virtual output"
    );

    // The pointer and directional moves stay on the visible monitor.
    let inside_tv = tv_geometry.loc.to_f64() + Point::from((10.0, 10.0));
    let clamped = fixture.state.clamp_to_outputs(inside_tv);
    assert!(
        fixture
            .state
            .space
            .output_geometry(&real)
            .unwrap()
            .to_f64()
            .contains(clamped),
        "the pointer was allowed onto the virtual output: {clamped:?}"
    );
    assert_eq!(
        fixture
            .state
            .monitor_in_direction_from(&real, crate::config::Direction::Right),
        None
    );

    // Actions create and remove runtime outputs; names stay unique.
    fixture
        .state
        .run_action(parse_action("create-virtual-output:1600x900").unwrap());
    let runtime = fixture
        .state
        .output_by_name("VIRTUAL-1")
        .expect("auto name");
    assert_eq!(runtime.current_mode().unwrap().refresh, 60_000);
    assert!(fixture
        .state
        .create_virtual_output(
            Some("TV".into()),
            (640, 480),
            None,
            crate::state::VirtualOutputOrigin::Runtime,
        )
        .is_err());
    fixture
        .state
        .run_action(parse_action("remove-virtual-output:VIRTUAL-1").unwrap());
    assert!(fixture.state.output_by_name("VIRTUAL-1").is_none());
    assert!(parse_action("remove-virtual-output:bad:name").is_none());
    fixture.dispatch();

    // Map one window (surface 7, xdg_surface 8, toplevel 9, viewport 10,
    // buffer 11). Client object ids are allocated in order.
    let globals: Vec<(String, u32)> = {
        fixture.request(1, 1, &[3], &[]); // a fresh registry lists everything
        fixture
            .dispatch()
            .into_iter()
            .filter(|(object, opcode, _)| (*object, *opcode) == (3, 0))
            .map(|(_, _, body)| {
                let name = u32::from_ne_bytes(body[..4].try_into().unwrap());
                let length = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
                (
                    String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap(),
                    name,
                )
            })
            .collect()
    };
    for (interface, id, version) in [
        ("wl_compositor", 4_u32, 4_u32),
        ("xdg_wm_base", 5, 1),
        ("wp_single_pixel_buffer_manager_v1", 6, 1),
        ("wp_viewporter", 7, 1),
    ] {
        let name = globals.iter().find(|(i, _)| i == interface).unwrap().1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(3, 0, &[name], &tail);
    }
    let surface = 8_u32;
    fixture.request(4, 0, &[surface], &[]);
    fixture.request(5, 2, &[surface + 1, surface], &[]);
    fixture.request(surface + 1, 1, &[surface + 2], &[]);
    fixture.request(7, 1, &[surface + 3, surface], &[]);
    fixture.request(surface + 3, 2, &[400, 300], &[]);
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
    fixture.request(6, 1, &[surface + 4, u32::MAX, 0, 0, u32::MAX], &[]);
    fixture.request(surface, 1, &[surface + 4, 0, 0], &[]);
    fixture.request(surface, 6, &[], &[]);
    fixture.dispatch();
    let window = fixture.state.space.elements().next().cloned().unwrap();
    let wl_surface = window.toplevel().unwrap().wl_surface().clone();
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&wl_surface),
        Some("REAL-1")
    );

    // A window reaches the virtual output by name; the cursor stays put.
    let pointer_before = fixture.state.seat.get_pointer().unwrap().current_location();
    fixture
        .state
        .run_action(parse_action("move-window-to-monitor:TV").unwrap());
    fixture.dispatch();
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&wl_surface),
        Some("TV")
    );
    assert_eq!(
        fixture.state.seat.get_pointer().unwrap().current_location(),
        pointer_before,
        "the cursor followed the window onto an output nobody can see"
    );

    // Removing the block on reload removes the output and brings the window
    // back to the monitor a person can see.
    std::fs::write(
        config_dir.join("tidewm/config.wave"),
        CONFIG.split("virtual_output").next().unwrap(),
    )
    .unwrap();
    fixture.state.reload_config();
    let events = fixture.dispatch();
    assert!(fixture.state.output_by_name("TV").is_none());
    assert!(
        events
            .iter()
            .any(|(object, opcode, body)| (*object, *opcode) == (2, 1)
                && u32::from_ne_bytes(body[..4].try_into().unwrap()) == added[0]),
        "the virtual wl_output global was not retracted"
    );
    assert_eq!(
        fixture
            .state
            .spatial
            .workspaces
            .layout
            .output_of(&wl_surface),
        Some("REAL-1")
    );
}
