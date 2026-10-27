//! Exercise KDE server-decoration negotiation against a real Wayland client.
//! The per-decoration `mode` event announces the compositor's policy once on
//! creation; a client that re-requests client-side decorations after every
//! `mode` event must not be answered with another `mode`, or the negotiation
//! becomes the CPU feedback loop from the audit.

use crate::{state::ClientState, Smallvil};
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
fn kde_decoration_announces_server_side_once() {
    // Smallvil loads config and creates a Wayland listener. Run the fixture
    // in a child with private XDG directories rather than mutate process-wide
    // environment or read/write the developer's live configuration.
    const CHILD: &str = "TIDEWM_KDE_DECORATION_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidewm-kde-decoration-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
        std::fs::create_dir(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::write(
            root.join("config/tidewm/config.wave"),
            "engine = classic\nwelcome_hint = false\nwater_effects = false\nanimations {\n    enabled = false\n}\n",
        )
        .unwrap();
        let test_name = concat!(
            module_path!(),
            "::kde_decoration_announces_server_side_once"
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

    fixture.request(1, 1, &[2], &[]); // wl_display.get_registry(2)
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
    let global = |interface: &str| {
        globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap()
            .1
    };

    let mut tail = string("wl_compositor");
    tail.extend_from_slice(&4u32.to_ne_bytes());
    tail.extend_from_slice(&3u32.to_ne_bytes());
    fixture.request(2, 0, &[global("wl_compositor")], &tail);

    let mut tail = string("org_kde_kwin_server_decoration_manager");
    tail.extend_from_slice(&1u32.to_ne_bytes());
    tail.extend_from_slice(&4u32.to_ne_bytes());
    fixture.request(
        2,
        0,
        &[global("org_kde_kwin_server_decoration_manager")],
        &tail,
    );
    let events = fixture.dispatch();
    assert!(
        events.iter().any(|(object, opcode, body)| {
            *object == 4 && *opcode == 0 && body.as_slice() == 2u32.to_ne_bytes().as_slice()
        }),
        "manager must announce the server-side default mode"
    );

    fixture.request(3, 0, &[5], &[]); // wl_compositor.create_surface(5)
    fixture.request(4, 0, &[6, 5], &[]); // manager.create(decoration 6, surface 5)
    let mut events = fixture.dispatch();

    for _ in 0..16 {
        fixture.request(6, 1, &[1], &[]); // org_kde_kwin_server_decoration.request_mode(Client)
    }
    events.extend(fixture.dispatch());

    let modes: Vec<_> = events
        .iter()
        .filter(|(object, opcode, _)| *object == 6 && *opcode == 0)
        .collect();
    assert_eq!(
        modes.len(),
        1,
        "a decoration must announce server-side once and ignore repeat client-side requests"
    );
    assert_eq!(
        modes[0].2.as_slice(),
        2u32.to_ne_bytes().as_slice(),
        "the announced mode must be server-side"
    );

    for _ in 0..16 {
        fixture.request(6, 1, &[2], &[]); // request_mode(Server)
    }
    let events = fixture.dispatch();
    assert!(
        events
            .iter()
            .all(|(object, opcode, _)| !(*object == 6 && *opcode == 0)),
        "repeat server-side requests must not be answered either"
    );

    // A replacement decoration object gets its own initial announcement.
    fixture.request(6, 0, &[], &[]); // release
    fixture.request(4, 0, &[7, 5], &[]); // manager.create(decoration 7, surface 5)
    let events = fixture.dispatch();
    let modes: Vec<_> = events
        .iter()
        .filter(|(object, opcode, _)| *object == 7 && *opcode == 0)
        .collect();
    assert_eq!(
        modes.len(),
        1,
        "a replacement decoration must announce its mode exactly once"
    );
    assert_eq!(
        modes[0].2.as_slice(),
        2u32.to_ne_bytes().as_slice(),
        "a replacement decoration must announce server-side"
    );
}
