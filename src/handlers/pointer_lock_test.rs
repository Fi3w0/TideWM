//! A locked pointer (wp_pointer_constraints) promises the client that the
//! compositor sends only relative motion until the lock ends. Games such as
//! Minecraft and CS2 turn any absolute `wl_pointer.motion` they receive while
//! locked into a camera jump, so no compositor path may emit one: not real
//! motion and not a synthetic focus refresh ahead of a button press.

use crate::{state::ClientState, Smallvil};
use smithay::{
    backend::input::{
        ButtonState, Device, DeviceCapability, Event, InputBackend, InputEvent, InputTime,
        PointerButtonEvent, PointerMotionEvent, UnusedEvent,
    },
    input::pointer::MotionEvent,
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{calloop::EventLoop, wayland_server::Display},
    utils::SERIAL_COUNTER,
    wayland::compositor::CompositorClientState,
};
use std::{
    collections::HashMap,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A minimal mouse: relative motion and buttons, nothing else.
#[derive(Debug)]
struct TestMouse;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TestMouseDevice;

impl Device for TestMouseDevice {
    fn id(&self) -> String {
        "test-mouse".into()
    }
    fn name(&self) -> String {
        "test mouse".into()
    }
    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(capability, DeviceCapability::Pointer)
    }
    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }
    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}

impl crate::tide_core::input::MappedInputDevice for TestMouseDevice {
    fn configured_output(
        &self,
        _mappings: &HashMap<smithay::reexports::input::Device, Output>,
    ) -> Option<Output> {
        None
    }
}

struct TestMotion(f64, f64);
struct TestButton(u32, ButtonState);

impl Event<TestMouse> for TestMotion {
    fn time(&self) -> InputTime {
        InputTime::from_millis(1)
    }
    fn device(&self) -> TestMouseDevice {
        TestMouseDevice
    }
}

impl PointerMotionEvent<TestMouse> for TestMotion {
    fn delta_x(&self) -> f64 {
        self.0
    }
    fn delta_y(&self) -> f64 {
        self.1
    }
    fn delta_x_unaccel(&self) -> f64 {
        self.0
    }
    fn delta_y_unaccel(&self) -> f64 {
        self.1
    }
}

impl Event<TestMouse> for TestButton {
    fn time(&self) -> InputTime {
        InputTime::from_millis(2)
    }
    fn device(&self) -> TestMouseDevice {
        TestMouseDevice
    }
}

impl PointerButtonEvent<TestMouse> for TestButton {
    fn button_code(&self) -> u32 {
        self.0
    }
    fn state(&self) -> ButtonState {
        self.1
    }
}

impl InputBackend for TestMouse {
    type Device = TestMouseDevice;
    type KeyboardKeyEvent = UnusedEvent;
    type PointerAxisEvent = UnusedEvent;
    type PointerButtonEvent = TestButton;
    type PointerMotionEvent = TestMotion;
    type PointerMotionAbsoluteEvent = UnusedEvent;
    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;
    type TouchDownEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;
    type SwitchToggleEvent = UnusedEvent;
    type SpecialEvent = ();
}

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

/// Run `name` in a child process with its own config directory, because the
/// compositor reads its config from `XDG_CONFIG_HOME` once at startup.
fn run_isolated(child_var: &str, name: &str, config: &str) -> bool {
    if std::env::var_os(child_var).is_some() {
        return false;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "tidewm-pointer-lock-test-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
    std::fs::create_dir(root.join("runtime")).unwrap();
    std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.join("config/tidewm/config.wave"), config).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
        .env(child_var, "1")
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
    true
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const WL_POINTER: u32 = 10;
const WL_POINTER_MOTION: u16 = 2;
const WL_POINTER_BUTTON: u16 = 3;
const RELATIVE_POINTER: u32 = 11;
const LOCKED_POINTER: u32 = 17;

/// Map one fullscreen-sized window, park the pointer off-center inside it,
/// lock the pointer like an FPS does, then move the mouse and click.
fn locked_pointer_receives_no_absolute_motion() {
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
        "lock-output".into(),
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
        ("zwp_pointer_constraints_v1", 8, 1),
        ("zwp_relative_pointer_manager_v1", 9, 1),
    ] {
        let name = globals
            .iter()
            .find(|(candidate, _)| candidate == interface)
            .unwrap_or_else(|| panic!("{interface} not advertised"))
            .1;
        let mut tail = string(interface);
        tail.extend_from_slice(&version.to_ne_bytes());
        tail.extend_from_slice(&id.to_ne_bytes());
        fixture.request(2, 0, &[name], &tail);
    }
    fixture.request(7, 0, &[WL_POINTER], &[]); // wl_seat.get_pointer
    fixture.request(9, 1, &[RELATIVE_POINTER, WL_POINTER], &[]); // get_relative_pointer
    fixture.dispatch();

    // One mapped toplevel (surface 12, xdg_surface 13, toplevel 14,
    // viewport 15, buffer 16). Client object ids must be allocated in order.
    let surface = 12_u32;
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
    let window = fixture.state.space.elements().next().cloned().unwrap();
    let location = fixture.state.space.element_location(&window).unwrap();

    // Park the pointer low in the window, as if the player clicked into the
    // game there before it grabbed the mouse.
    let parked = (location.x as f64 + 300.0, location.y as f64 + 500.0).into();
    let under = fixture.state.surface_under(parked);
    assert!(under.is_some(), "no surface under the parked pointer");
    let pointer = fixture.state.seat.get_pointer().unwrap();
    pointer.motion(
        &mut fixture.state,
        under,
        &MotionEvent {
            location: parked,
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::from_millis(1),
        },
    );
    pointer.frame(&mut fixture.state);
    fixture.dispatch();

    // zwp_pointer_constraints_v1.lock_pointer(id, surface, pointer, region=null,
    // lifetime=persistent).
    fixture.request(8, 1, &[LOCKED_POINTER, surface, WL_POINTER, 0, 2], &[]);
    let events = fixture.dispatch();
    assert!(
        events
            .iter()
            .any(|(object, opcode, _)| (*object, *opcode) == (LOCKED_POINTER, 0)),
        "the pointer lock never became active: {events:?}"
    );

    // Play: look around, then fire and aim, then look around again.
    let mut events = Vec::new();
    for (dx, dy) in [(12.0, -3.0), (-4.0, 6.0)] {
        fixture
            .state
            .process_input_event(InputEvent::<TestMouse>::PointerMotion {
                event: TestMotion(dx, dy),
            });
        events.extend(fixture.dispatch());
    }
    for button in [BTN_LEFT, BTN_RIGHT] {
        for state in [ButtonState::Pressed, ButtonState::Released] {
            fixture
                .state
                .process_input_event(InputEvent::<TestMouse>::PointerButton {
                    event: TestButton(button, state),
                });
            events.extend(fixture.dispatch());
        }
        fixture
            .state
            .process_input_event(InputEvent::<TestMouse>::PointerMotion {
                event: TestMotion(7.0, 2.0),
            });
        events.extend(fixture.dispatch());
    }
    // A retile with no click (any window mapping or closing mid-game, such as
    // a toast) refreshes pointer routing too.
    fixture.state.retile();
    events.extend(fixture.dispatch());

    assert!(
        events
            .iter()
            .any(|(object, opcode, _)| (*object, *opcode) == (RELATIVE_POINTER, 0)),
        "the locked client received no relative motion"
    );
    assert_eq!(
        events
            .iter()
            .filter(|(object, opcode, _)| (*object, *opcode) == (WL_POINTER, WL_POINTER_BUTTON))
            .count(),
        4,
        "the locked client did not receive its four button events"
    );
    let absolute: Vec<_> = events
        .iter()
        .filter(|(object, opcode, _)| (*object, *opcode) == (WL_POINTER, WL_POINTER_MOTION))
        .map(|(_, _, body)| {
            (
                f64::from(i32::from_ne_bytes(body[4..8].try_into().unwrap())) / 256.0,
                f64::from(i32::from_ne_bytes(body[8..12].try_into().unwrap())) / 256.0,
            )
        })
        .collect();
    assert!(
        absolute.is_empty(),
        "a locked client received absolute wl_pointer.motion at {absolute:?}"
    );
}

#[test]
fn scroll_locked_pointer_receives_no_absolute_motion() {
    if run_isolated(
        "TIDEWM_POINTER_LOCK_SCROLL_CHILD",
        concat!(
            module_path!(),
            "::scroll_locked_pointer_receives_no_absolute_motion"
        ),
        "engine = scroll\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nscroll {\n tile_proportion = 1.0\n}\nanimations {\n enabled = false\n}\n",
    ) {
        return;
    }
    locked_pointer_receives_no_absolute_motion();
}

#[test]
fn classic_locked_pointer_receives_no_absolute_motion() {
    if run_isolated(
        "TIDEWM_POINTER_LOCK_CLASSIC_CHILD",
        concat!(
            module_path!(),
            "::classic_locked_pointer_receives_no_absolute_motion"
        ),
        "engine = classic\nwelcome_hint = false\nwater_effects = false\ngaps = 0\nanimations {\n enabled = false\n}\n",
    ) {
        return;
    }
    locked_pointer_receives_no_absolute_motion();
}
