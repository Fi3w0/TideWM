//! zwlr_gamma_control_manager_v1: per-output gamma ramp control (wlsunset/
//! gammastep night-light). Hand-rolled on `wayland-protocols-wlr`, same
//! reason as the other two wlr- protocols in this project -- no Smithay
//! module for it.
//!
//! Real DRM gamma-LUT access only exists on the udev backend
//! (`Smallvil::gamma_size`/`set_gamma`, installed by
//! `backend::udev::init_udev`, both `None` under winit). A nested winit
//! output has no real color pipeline to adjust at all -- unlike DPMS,
//! there's no meaningful "logical-only" fallback here, so a request under
//! winit is refused outright with `failed`, matching the protocol's own
//! "the output doesn't support gamma tables" reason.

use std::{collections::HashMap, os::fd::AsFd, sync::Mutex};

use smithay::output::{Output, WeakOutput};
use smithay::reexports::rustix;
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

use crate::Smallvil;

/// Weak output identity plus the gamma size while this control is valid.
/// Invalidating takes the target out, making every later request and the
/// eventual destructor a harmless no-op. Keeping this weak is important:
/// a client is only required to destroy the object after receiving `failed`,
/// so its resource data must not keep a disconnected Smithay `Output` alive.
struct ControlData(Mutex<Option<(WeakOutput, u32)>>);

impl ControlData {
    fn new(target: Option<(WeakOutput, u32)>) -> Self {
        Self(Mutex::new(target))
    }

    fn target(&self) -> Option<(WeakOutput, u32)> {
        self.0.lock().expect("gamma control data poisoned").clone()
    }

    fn invalidate(&self) -> Option<(WeakOutput, u32)> {
        self.0.lock().expect("gamma control data poisoned").take()
    }
}

fn fail_control(control: &ZwlrGammaControlV1) {
    let was_valid = control
        .data::<ControlData>()
        .and_then(ControlData::invalidate)
        .is_some();
    if was_valid {
        control.failed();
    }
}

/// At most one live control per output. A second `get_gamma_control` for
/// an already-controlled output transfers control to the new client,
/// sending `failed` to whoever held it before -- matches the protocol's own
/// "the compositor has transferred gamma control to another client" reason,
/// and the actual behavior wlsunset/gammastep expect when one replaces the
/// other (e.g. a restart).
pub struct WlrGammaControlState {
    #[allow(dead_code)]
    global: GlobalId,
    controls: HashMap<WeakOutput, ZwlrGammaControlV1>,
}

impl WlrGammaControlState {
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<Smallvil, ZwlrGammaControlManagerV1, ()>(1, ());
        Self {
            global,
            controls: HashMap::new(),
        }
    }

    fn remove_if_current(&mut self, output: &WeakOutput, resource: &ZwlrGammaControlV1) -> bool {
        if self.controls.get(output) != Some(resource) {
            return false;
        }
        self.controls.remove(output);
        true
    }

    /// Invalidates the exclusive control for a disconnected output. The
    /// protocol resource can outlive `failed` if its client ignores the event,
    /// so both its target and the map key are weak references.
    pub fn output_removed(&mut self, output: &Output) {
        if let Some(control) = self.controls.remove(&output.downgrade()) {
            fail_control(&control);
        }
    }
}

/// A linear (identity) ramp of `size` steps -- what a freshly-booted CRTC's
/// gamma LUT already looks like, and what "restore the original value" (the
/// protocol's own wording for what happens on control destroy) means in
/// practice: this project doesn't read back the hardware's own ramp before
/// a client first touches it, matching wlroots' own reference behavior of
/// resetting to linear on last-client-disconnect rather than snapshotting.
fn identity_ramp(size: u32) -> Vec<u16> {
    let last = size.saturating_sub(1).max(1) as u64;
    (0..size)
        .map(|i| ((i as u64 * u16::MAX as u64) / last) as u16)
        .collect()
}

#[derive(Debug)]
enum GammaTableReadError {
    InvalidSource,
    InvalidLength,
    Io(rustix::io::Errno),
}

/// Read a protocol gamma table without ever treating the client-provided fd
/// as a stream. The protocol requires a memory-mappable object whose length is
/// exactly three `gamma_size` arrays of native-endian u16 values. Accepting a
/// pipe/socket here would let an untrusted client leave the compositor's event
/// loop blocked waiting for bytes that never arrive.
fn read_gamma_table(fd: impl AsFd, gamma_size: u32) -> Result<Vec<u8>, GammaTableReadError> {
    let expected_len = usize::try_from(gamma_size)
        .ok()
        .and_then(|size| size.checked_mul(3))
        .and_then(|size| size.checked_mul(std::mem::size_of::<u16>()))
        .ok_or(GammaTableReadError::InvalidLength)?;

    let stat = rustix::fs::fstat(&fd).map_err(GammaTableReadError::Io)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(GammaTableReadError::InvalidSource);
    }
    if u64::try_from(stat.st_size).ok() != Some(expected_len as u64) {
        return Err(GammaTableReadError::InvalidLength);
    }

    let mut buf = vec![0; expected_len];
    let mut filled = 0;
    while filled < expected_len {
        let read = rustix::io::pread(&fd, &mut buf[filled..], filled as u64)
            .map_err(GammaTableReadError::Io)?;
        if read == 0 {
            return Err(GammaTableReadError::InvalidLength);
        }
        filled += read;
    }

    // Catch ordinary truncate/extend races as invalid input. `pread` also
    // leaves the shared file offset untouched, as expected for passed fds.
    let final_stat = rustix::fs::fstat(fd).map_err(GammaTableReadError::Io)?;
    if u64::try_from(final_stat.st_size).ok() != Some(expected_len as u64) {
        return Err(GammaTableReadError::InvalidLength);
    }

    Ok(buf)
}

impl GlobalDispatch<ZwlrGammaControlManagerV1, ()> for Smallvil {
    fn can_view(client: Client, _data: &()) -> bool {
        crate::state::trusted_client(&client)
    }

    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = request else {
            return;
        };

        let target = Output::from_resource(&output);
        let size = target
            .as_ref()
            .and_then(|o| state.gamma_size.as_mut().and_then(|hook| hook(o)));

        let control_target = target
            .as_ref()
            .zip(size)
            .map(|(output, size)| (output.downgrade(), size));
        let control = data_init.init(id, ControlData::new(control_target));

        match (target, size) {
            (Some(output), Some(size)) => {
                if let Some(old) = state
                    .wlr_gamma_control_state
                    .controls
                    .insert(output.downgrade(), control.clone())
                {
                    fail_control(&old);
                }
                control.gamma_size(size);
            }
            _ => control.failed(),
        }
    }
}

impl Dispatch<ZwlrGammaControlV1, ControlData> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        data: &ControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let zwlr_gamma_control_v1::Request::SetGamma { fd } = request else {
            return;
        };
        let Some((output, size)) = data.target() else {
            return;
        };
        let Some(live_output) = output.upgrade() else {
            state
                .wlr_gamma_control_state
                .remove_if_current(&output, resource);
            fail_control(resource);
            return;
        };

        let buf = match read_gamma_table(&fd, size) {
            Ok(buf) => buf,
            Err(error) => {
                match error {
                    GammaTableReadError::Io(error) => {
                        tracing::warn!(%error, "Failed to read gamma table fd");
                    }
                    GammaTableReadError::InvalidSource => {
                        tracing::warn!("Rejected non-file gamma table fd");
                    }
                    GammaTableReadError::InvalidLength => {}
                }
                resource.post_error(
                    zwlr_gamma_control_v1::Error::InvalidGamma,
                    "gamma table fd must be a regular file containing exactly size*3 u16 values",
                );
                return;
            }
        };
        let size = size as usize;

        let channel = |bytes: &[u8]| -> Vec<u16> {
            bytes
                .chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect()
        };
        let red = channel(&buf[..size * 2]);
        let green = channel(&buf[size * 2..size * 4]);
        let blue = channel(&buf[size * 4..size * 6]);

        let applied = state
            .set_gamma
            .as_mut()
            .map(|hook| hook(&live_output, &red, &green, &blue))
            .unwrap_or(false);
        if !applied {
            state
                .wlr_gamma_control_state
                .remove_if_current(&output, resource);
            fail_control(resource);
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrGammaControlV1,
        data: &ControlData,
    ) {
        let Some((output, size)) = data.invalidate() else {
            return;
        };
        // Only reset gamma if this resource is still the tracked owner --
        // if a newer client already transferred control away (see
        // `GetGammaControl` above), the map entry no longer points at
        // `resource`, and resetting here would incorrectly clobber
        // whatever the new owner has already set.
        if !state
            .wlr_gamma_control_state
            .remove_if_current(&output, resource)
        {
            return;
        }
        let Some(output) = output.upgrade() else {
            return;
        };
        if let Some(hook) = state.set_gamma.as_mut() {
            let ramp = identity_ramp(size);
            hook(&output, &ramp, &ramp, &ramp);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use smithay::{
        output::{Output, PhysicalProperties, Subpixel},
        reexports::rustix,
    };

    use super::{identity_ramp, read_gamma_table, ControlData, GammaTableReadError};

    fn test_output() -> Output {
        Output::new(
            "gamma-control-test".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".to_string(),
                model: "test".to_string(),
                serial_number: "test".to_string(),
            },
        )
    }

    #[test]
    fn control_data_does_not_keep_output_alive() {
        let output = test_output();
        let weak = output.downgrade();
        let data = ControlData::new(Some((weak.clone(), 37)));

        drop(output);

        assert!(weak.upgrade().is_none());
        assert!(data
            .target()
            .is_some_and(|(output, size)| output.upgrade().is_none() && size == 37));
    }

    #[test]
    fn control_data_invalidation_is_idempotent() {
        let output = test_output();
        let data = ControlData::new(Some((output.downgrade(), 19)));

        assert!(data.invalidate().is_some());
        assert!(data.invalidate().is_none());
        assert!(data.target().is_none());
    }

    #[test]
    fn identity_ramp_spans_full_range_monotonically() {
        let ramp = identity_ramp(256);
        assert_eq!(ramp.len(), 256);
        assert_eq!(ramp[0], 0);
        assert_eq!(ramp[255], u16::MAX);
        assert!(ramp.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn identity_ramp_handles_degenerate_sizes() {
        assert_eq!(identity_ramp(0), Vec::<u16>::new());
        assert_eq!(identity_ramp(1), vec![0]);
    }

    #[test]
    fn gamma_table_accepts_exact_sized_memfd() {
        let fd = rustix::fs::memfd_create("tidewm-gamma-test", rustix::fs::MemfdFlags::CLOEXEC)
            .expect("create memfd");
        let bytes = [1_u8, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0];
        rustix::fs::ftruncate(&fd, bytes.len() as u64).expect("size memfd");
        assert_eq!(
            rustix::io::pwrite(&fd, &bytes, 0).expect("populate memfd"),
            bytes.len()
        );

        assert_eq!(read_gamma_table(&fd, 2).expect("valid table"), bytes);
    }

    #[test]
    fn gamma_table_rejects_wrong_sized_memfd() {
        let fd = rustix::fs::memfd_create("tidewm-gamma-test", rustix::fs::MemfdFlags::CLOEXEC)
            .expect("create memfd");
        rustix::fs::ftruncate(&fd, 10).expect("size memfd");

        assert!(matches!(
            read_gamma_table(&fd, 2),
            Err(GammaTableReadError::InvalidLength)
        ));
    }

    #[test]
    fn gamma_table_rejects_stream_without_reading() {
        let (reader, _writer) = UnixStream::pair().expect("create socket pair");

        assert!(matches!(
            read_gamma_table(&reader, 2),
            Err(GammaTableReadError::InvalidSource)
        ));
    }
}
