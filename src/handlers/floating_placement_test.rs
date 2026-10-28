//! Real xdg-shell first-map placement on two simulated outputs; no native seat.
use crate::{state::ClientState, Smallvil};
use smithay::{
    input::pointer::MotionEvent,
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_server::{Display, Resource},
    },
    utils::{Rectangle, SERIAL_COUNTER},
    wayland::compositor::CompositorClientState,
};
use std::{
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) fn string(value: &str) -> Vec<u8> {
    let mut bytes = ((value.len() + 1) as u32).to_ne_bytes().to_vec();
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes
}

pub(super) struct Fixture {
    pub(super) event_loop: EventLoop<'static, Smallvil>,
    pub(super) state: Smallvil,
    pub(super) peer: UnixStream,
    pub(super) next_id: u32,
}
impl Fixture {
    pub(super) fn request(&mut self, object: u32, opcode: u16, words: &[u32], tail: &[u8]) {
        let size = 8 + words.len() * 4 + tail.len();
        let mut bytes = object.to_ne_bytes().to_vec();
        bytes.extend_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
        for word in words {
            bytes.extend_from_slice(&word.to_ne_bytes());
        }
        bytes.extend_from_slice(tail);
        self.peer.write_all(&bytes).unwrap();
    }
    pub(super) fn dispatch(&mut self) -> Vec<(u32, u16, Vec<u8>)> {
        self.event_loop
            .dispatch(Some(Duration::ZERO), &mut self.state)
            .unwrap();
        self.state.display_handle.flush_clients().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 65536];
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
    pub(super) fn window(&mut self, app: &str, parent: Option<u32>, natural: (i32, i32)) -> u32 {
        let id = self.next_id;
        self.next_id += 5;
        self.request(3, 0, &[id], &[]); // wl_surface
        self.request(4, 2, &[id + 1, id], &[]); // xdg_surface
        self.request(id + 1, 1, &[id + 2], &[]); // toplevel
        self.request(id + 2, 3, &[], &string(app));
        if let Some(parent) = parent {
            self.request(id + 2, 1, &[parent + 2], &[]);
        }
        self.request(6, 1, &[id + 3, id], &[]); // viewport
        self.request(id + 3, 2, &[natural.0 as u32, natural.1 as u32], &[]);
        self.request(id + 1, 3, &[0, 0, natural.0 as u32, natural.1 as u32], &[]);
        self.request(id, 6, &[], &[]); // initial empty commit
        let serial = self
            .dispatch()
            .into_iter()
            .find(|(object, op, _)| *object == id + 1 && *op == 0)
            .map(|(_, _, body)| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            .unwrap();
        self.request(id + 1, 4, &[serial], &[]);
        self.request(5, 1, &[id + 4, 0, 0, 0, u32::MAX], &[]); // single-pixel buffer
        self.request(id, 1, &[id + 4, 0, 0], &[]);
        self.request(id, 6, &[], &[]);
        let events = self.dispatch();
        // Settle the real client's configured geometry before checking IPC/Space.
        let mut size = natural;
        let mut ack = None;
        for (object, opcode, body) in events {
            if object == id + 2 && opcode == 0 {
                let w = i32::from_ne_bytes(body[..4].try_into().unwrap());
                let h = i32::from_ne_bytes(body[4..8].try_into().unwrap());
                if w > 0 && h > 0 {
                    size = (w, h);
                }
            }
            if object == id + 1 && opcode == 0 {
                ack = Some(u32::from_ne_bytes(body[..4].try_into().unwrap()));
            }
        }
        if let Some(serial) = ack {
            self.request(id + 1, 4, &[serial], &[]);
            self.request(id + 3, 2, &[size.0 as u32, size.1 as u32], &[]);
            self.request(id + 1, 3, &[0, 0, size.0 as u32, size.1 as u32], &[]);
            self.request(id, 6, &[], &[]);
            self.dispatch();
        }
        id
    }
    pub(super) fn surface(
        &self,
        id: u32,
    ) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
        self.state
            .space
            .elements()
            .chain(self.state.unmapped_toplevels.values())
            .find_map(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface().clone())
                    .filter(|s| s.id().protocol_id() == id)
            })
            .unwrap()
    }
    pub(super) fn pointer_on(&mut self, x: f64, y: f64) {
        let pointer = self.state.seat.get_pointer().unwrap();
        pointer.motion(
            &mut self.state,
            None,
            &MotionEvent {
                location: (x, y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: smithay::backend::input::InputTime::from_millis(1),
            },
        );
        pointer.frame(&mut self.state);
    }
}

#[test]
fn first_map_floats_follow_focus_or_parent_and_keep_visible_geometry() {
    const CHILD: &str = "TIDEWM_FLOAT_PLACEMENT_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("tidewm-float-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(
            root.join("config/tidewm/config.wave"),
            r#"engine = scroll
welcome_hint = false
water_effects = false
gaps = 12
focus_follows_mouse = true
animations {
 enabled = false
}
scroll {
 float_anchor = screen
 tile_proportion = 0.45
 center_focused = never
}
rule {
 app_id = anchor
 output = A
}
rule {
 app_id = floater
 float = true
 output = focused
}
rule {
 app_id = explicit
 float = true
 output = current
 position = "90x110"
 size = "420x300"
}
rule {
 app_id = offscreen
 float = true
 output = camera
 position = "3907x9000"
 size = "5000x4000"
}
rule {
 app_id = pin
 pin = true
 output = focused
}
rule {
 app_id = literal-pin
 float = true
 pin_output = focused
}
rule {
 app_id = edge
 float = true
 output = focused
 position = "0x0"
 size = "1200x900"
}
rule {
 app_id = override-parent
 output = B
}
rule {
 app_id = hidden
 float = true
 output = focused
 workspace = 3
}
"#,
        )
        .unwrap();
        let test_name = concat!(
            module_path!(),
            "::first_map_floats_follow_focus_or_parent_and_keep_visible_geometry"
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                test_name.split_once("::").unwrap().1,
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
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
    let mut f = Fixture {
        event_loop,
        state,
        peer,
        next_id: 7,
    };
    let outputs: Vec<_> = ["A", "B"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            let o = Output::new(
                name.into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                    serial_number: "test".into(),
                },
            );
            o.change_current_state(
                Some(Mode {
                    size: (1200, 900).into(),
                    refresh: 60_000,
                }),
                None,
                Some(Scale::Integer(1)),
                None,
            );
            o.create_global::<Smallvil>(&f.state.display_handle);
            f.state.space.map_output(&o, (i as i32 * 1200, 0));
            o
        })
        .collect();
    f.state.retile();
    assert!(f.state.config.input.focus_follows_mouse);
    f.request(1, 1, &[2], &[]);
    let globals = f.dispatch();
    for (interface, id) in [
        ("wl_compositor", 3u32),
        ("xdg_wm_base", 4),
        ("wp_single_pixel_buffer_manager_v1", 5),
        ("wp_viewporter", 6),
    ] {
        let name = globals
            .iter()
            .find_map(|(object, op, body)| {
                if (*object, *op) != (2, 0) {
                    return None;
                }
                let len = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
                (std::str::from_utf8(&body[8..8 + len - 1]).unwrap() == interface)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        let mut tail = string(interface);
        tail.extend_from_slice(
            &(if interface == "wl_compositor" {
                4u32
            } else {
                1u32
            })
            .to_ne_bytes(),
        );
        tail.extend_from_slice(&id.to_ne_bytes());
        f.request(2, 0, &[name], &tail);
    }
    f.dispatch();
    f.pointer_on(1700.0, 400.0);
    // No focused window: aliases must fall back to the existing primary policy.
    assert_eq!(
        f.state.initial_rule_output("focused"),
        f.state.primary_output()
    );
    let parent = f.window("anchor", None, (640, 480));
    let parent_surface = f.surface(parent);
    f.state.activate_toplevel(&parent_surface);
    assert_eq!(f.state.focused_output(), Some(outputs[0].clone()));
    assert_eq!(f.state.primary_output(), Some(outputs[1].clone()));
    for name in ["focused", "camera", "current"] {
        assert_eq!(f.state.initial_rule_output(name), Some(outputs[0].clone()));
    }
    let floater = f.window("floater", None, (640, 480));
    let surface = f.surface(floater);
    let tag = &f.state.spatial.workspaces.floating_workspace[&surface];
    assert_eq!(tag.output, "A");
    assert_eq!(
        tag.rect,
        Rectangle::new((280, 210).into(), (640, 480).into())
    );
    // Space (and therefore windows IPC geometry) agrees after configure/ack.
    assert_eq!(f.state.space.element_geometry(&tag.window), Some(tag.rect));
    let explicit = f.window("explicit", None, (700, 500));
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&f.surface(explicit)].rect,
        Rectangle::new((90, 110).into(), (420, 300).into())
    );
    let offscreen = f.window("offscreen", None, (640, 480));
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&f.surface(offscreen)].rect,
        Rectangle::new((0, 0).into(), (1200, 900).into())
    );
    let pin = f.window("pin", None, (400, 300));
    let pin_surface = f.surface(pin);
    assert!(f.state.window_is_pinned(&pin_surface));
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&pin_surface].rect,
        Rectangle::new((400, 300).into(), (400, 300).into())
    );
    let edge = f.window("edge", None, (640, 480));
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&f.surface(edge)].rect,
        Rectangle::new((0, 0).into(), (1200, 900).into())
    );
    let literal_pin = f.window("literal-pin", None, (400, 300));
    let literal_surface = f.surface(literal_pin);
    assert_eq!(
        f.state.spatial.workspaces.output_pins[&literal_surface],
        "focused"
    );
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&literal_surface].output,
        "B"
    );
    // Focus a window on B, then open a child of the old A window.
    let other = f.window("override-parent", Some(parent), (320, 240));
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&f.surface(other)].output,
        "B"
    );
    assert_eq!(f.state.focused_output(), Some(outputs[1].clone()));
    let child = f.window("dialog", Some(parent), (400, 300));
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&f.surface(child)].output,
        "A"
    );
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&f.surface(child)].rect,
        Rectangle::new((400, 300).into(), (400, 300).into())
    );
    // Interactive untile retains the tile rect instead of centering it.
    let current = f
        .state
        .space
        .element_geometry(
            &f.state
                .spatial
                .workspaces
                .layout
                .window_of(&parent_surface)
                .unwrap(),
        )
        .unwrap();
    f.state.toggle_floating(&parent_surface);
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&parent_surface].rect,
        current
    );
    f.pointer_on(900.0, 750.0);
    // layout=floating takes the same first-map placement path.
    f.state.spatial.workspaces.layout.set_algorithm(
        "A",
        1,
        crate::config::LayoutAlgorithm::Floating,
    );
    let layout = f.window("layout-float", None, (360, 260));
    assert_eq!(
        f.state.spatial.workspaces.floating_workspace[&f.surface(layout)].rect,
        Rectangle::new((420, 320).into(), (360, 260).into())
    );
    // A hidden workspace has no Space element; its durable rect still centers.
    f.window("hidden", None, (360, 260));
    let tag = f
        .state
        .spatial
        .workspaces
        .floating_workspace
        .values()
        .find(|t| t.workspace == 3)
        .unwrap();
    assert_eq!(tag.output, "A");
    assert_eq!(
        tag.rect,
        Rectangle::new((420, 320).into(), (360, 260).into())
    );
    assert!(f.state.space.element_geometry(&tag.window).is_none());
}

#[test]
fn tiled_fullscreen_round_trip_does_not_relock_surface_state() {
    const CHILD: &str = "TIDEWM_TILED_FULLSCREEN_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidewm-fullscreen-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("config/tidewm/config.wave"), "engine = classic\nwelcome_hint = false\nwater_effects = false\nanimations {\n enabled = false\n}\n").unwrap();
        let log = std::fs::File::create(root.join("child.log")).unwrap();
        let test_name = concat!(
            module_path!(),
            "::tiled_fullscreen_round_trip_does_not_relock_surface_state"
        );
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                test_name.split_once("::").unwrap().1,
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        // A self-deadlock must fail this test instead of hanging the test suite.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = std::fs::read_to_string(root.join("child.log")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(status.is_some(), "tiled unfullscreen deadlocked: {output}");
        assert!(status.unwrap().success(), "{output}");
        assert!(output.contains("running 1 test"), "{output}");
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
    let mut f = Fixture {
        event_loop,
        state,
        peer,
        next_id: 7,
    };

    let output = Output::new(
        "fullscreen-output".into(),
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
            size: (1200, 900).into(),
            refresh: 60_000,
        }),
        None,
        Some(Scale::Integer(1)),
        None,
    );
    output.create_global::<Smallvil>(&f.state.display_handle);
    f.state.space.map_output(&output, (0, 0));
    f.request(1, 1, &[2], &[]);
    let globals = f.dispatch();
    for (interface, id) in [
        ("wl_compositor", 3u32),
        ("xdg_wm_base", 4),
        ("wp_single_pixel_buffer_manager_v1", 5),
        ("wp_viewporter", 6),
    ] {
        let name = globals
            .iter()
            .find_map(|(object, op, body)| {
                if (*object, *op) != (2, 0) {
                    return None;
                }
                let len = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
                (std::str::from_utf8(&body[8..8 + len - 1]).unwrap() == interface)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        let mut tail = string(interface);
        tail.extend_from_slice(
            &(if interface == "wl_compositor" {
                4u32
            } else {
                1u32
            })
            .to_ne_bytes(),
        );
        tail.extend_from_slice(&id.to_ne_bytes());
        f.request(2, 0, &[name], &tail);
    }
    f.dispatch();

    let ids = [
        f.window("tile-one", None, (400, 300)),
        f.window("tile-two", None, (400, 300)),
    ];
    for id in ids {
        let surface = f.surface(id);
        assert!(f.state.spatial.workspaces.layout.contains(&surface));
        f.state
            .focus_window(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        let expected = f.state.tiled_rect_for_surface(&surface).unwrap().size;
        for client_request in [false, true] {
            if client_request {
                f.request(id + 2, 11, &[0], &[]); // xdg_toplevel.set_fullscreen
            } else {
                f.state.run_action(crate::config::Action::ToggleFullscreen);
            }
            f.dispatch();
            assert!(f.state.fullscreen.contains_key(&surface));
            if client_request {
                f.request(id + 2, 12, &[], &[]); // xdg_toplevel.unset_fullscreen
            } else {
                f.state.run_action(crate::config::Action::ToggleFullscreen);
            }
            let events = f.dispatch();
            assert!(!f.state.fullscreen.contains_key(&surface));
            assert!(f.state.spatial.workspaces.layout.contains(&surface));
            assert_eq!(f.state.focused_window_surface(), Some(surface.clone()));
            let configure = events
                .iter()
                .rev()
                .find(|(object, op, _)| (*object, *op) == (id + 2, 0))
                .unwrap();
            let width = i32::from_ne_bytes(configure.2[..4].try_into().unwrap());
            let height = i32::from_ne_bytes(configure.2[4..8].try_into().unwrap());
            assert_eq!((width, height), (expected.w, expected.h));
            let callback = f.next_id;
            f.next_id += 1;
            f.request(1, 0, &[callback], &[]); // round-trip after unfullscreen
            assert!(f
                .dispatch()
                .iter()
                .any(|(object, op, _)| (*object, *op) == (callback, 0)));
        }
    }
}
