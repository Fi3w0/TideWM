//! Real xdg toplevels for placement/redraw tests, without a GPU or desktop socket.
use smithay::{
    desktop::Window,
    reexports::wayland_server::{
        protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
        Client, Display,
    },
    utils::Serial,
    wayland::{
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
    },
};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::Arc,
};

struct State {
    compositor: CompositorState,
    shell: XdgShellState,
    windows: Vec<Window>,
}
struct ClientState(CompositorClientState);
impl smithay::reexports::wayland_server::backend::ClientData for ClientState {}
impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().0
    }
    fn commit(&mut self, _: &WlSurface) {}
}
impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.shell
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.windows.push(Window::new_wayland_window(surface));
    }
    fn new_popup(&mut self, _: PopupSurface, _: PositionerState) {}
    fn grab(&mut self, _: PopupSurface, _: WlSeat, _: Serial) {}
    fn reposition_request(&mut self, _: PopupSurface, _: PositionerState, _: u32) {}
}
smithay::delegate_compositor!(State);
smithay::delegate_xdg_shell!(State);

pub(crate) struct ProtocolFixture {
    display: Display<State>,
    state: State,
    peer: UnixStream,
    next_id: u32,
}
fn wire_string(text: &str) -> Vec<u8> {
    let mut data = ((text.len() + 1) as u32).to_ne_bytes().to_vec();
    data.extend_from_slice(text.as_bytes());
    data.push(0);
    while data.len() % 4 != 0 {
        data.push(0);
    }
    data
}
impl ProtocolFixture {
    fn request(&mut self, object: u32, opcode: u16, payload: &[u8]) {
        let mut data = object.to_ne_bytes().to_vec();
        data.extend_from_slice(
            &(((payload.len() + 8) as u32) << 16 | u32::from(opcode)).to_ne_bytes(),
        );
        data.extend_from_slice(payload);
        self.peer.write_all(&data).unwrap();
    }
    fn dispatch(&mut self) {
        self.display.dispatch_clients(&mut self.state).unwrap();
        self.display.flush_clients().unwrap();
    }
    pub(crate) fn new() -> Self {
        let display = Display::new().unwrap();
        let mut handle = display.handle();
        let state = State {
            compositor: CompositorState::new::<State>(&handle),
            shell: XdgShellState::new::<State>(&handle),
            windows: Vec::new(),
        };
        let (server, peer) = UnixStream::pair().unwrap();
        handle
            .insert_client(
                server,
                Arc::new(ClientState(CompositorClientState::default())),
            )
            .unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut fixture = Self {
            display,
            state,
            peer,
            next_id: 5,
        };
        fixture.request(1, 1, &2_u32.to_ne_bytes()); // wl_display.get_registry
        fixture.dispatch();
        let mut events = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match fixture.peer.read(&mut buffer) {
                Ok(n) if n > 0 => events.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected registry read: {other:?}"),
            }
        }
        let mut offset = 0;
        let mut globals = std::collections::HashMap::new();
        while offset + 8 <= events.len() {
            let header = u32::from_ne_bytes(events[offset + 4..offset + 8].try_into().unwrap());
            let size = (header >> 16) as usize;
            if header & 0xffff == 0 {
                // wl_registry.global
                let name = u32::from_ne_bytes(events[offset + 8..offset + 12].try_into().unwrap());
                let length =
                    u32::from_ne_bytes(events[offset + 12..offset + 16].try_into().unwrap())
                        as usize;
                let interface =
                    std::str::from_utf8(&events[offset + 16..offset + 16 + length - 1]).unwrap();
                globals.insert(interface.to_string(), name);
            }
            assert!(size >= 8);
            offset += size;
        }
        for (interface, id, version) in [("wl_compositor", 3_u32, 4_u32), ("xdg_wm_base", 4, 1)] {
            let mut payload = globals[interface].to_ne_bytes().to_vec();
            payload.extend(wire_string(interface));
            payload.extend(version.to_ne_bytes());
            payload.extend(id.to_ne_bytes());
            fixture.request(2, 0, &payload);
        }
        fixture.dispatch();
        fixture
    }
    pub(crate) fn window(&mut self) -> Window {
        let surface = self.next_id;
        let xdg_surface = surface + 1;
        let toplevel = surface + 2;
        self.next_id += 3;
        self.request(3, 0, &surface.to_ne_bytes());
        let mut payload = xdg_surface.to_ne_bytes().to_vec();
        payload.extend(surface.to_ne_bytes());
        self.request(4, 2, &payload);
        self.request(xdg_surface, 1, &toplevel.to_ne_bytes());
        self.dispatch();
        self.state.windows.pop().expect("xdg toplevel callback")
    }
}
