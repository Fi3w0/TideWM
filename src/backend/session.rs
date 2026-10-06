use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use smithay::{
    backend::drm::DrmDevice,
    reexports::{drm::Device as DrmRawDevice, input::Libinput},
};

/// `DRM_IOCTL_AUTH_MAGIC`: `_IOW('d', 0x11, struct drm_auth)`.
const DRM_IOCTL_AUTH_MAGIC: u32 = (1 << 30) | (4 << 16) | ((b'd' as u32) << 8) | 0x11;

/// Whether `fd` is the device's current DRM master, the same probe as
/// libdrm's `drmIsMaster`: AUTH_MAGIC is a master-only ioctl, so a non-master
/// caller gets EACCES, while a master is refused the invalid magic 0 with
/// EINVAL. Works without CAP_SYS_ADMIN.
fn is_master(fd: BorrowedFd<'_>) -> std::io::Result<bool> {
    let mut magic: u32 = 0;
    // SAFETY: AUTH_MAGIC reads one u32 (struct drm_auth) from the pointer,
    // which stays valid for the duration of the call.
    let ret = unsafe { libc::ioctl(fd.as_raw_fd(), DRM_IOCTL_AUTH_MAGIC as _, &mut magic) };
    if ret == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EINVAL) => Ok(true),
        Some(libc::EACCES) => Ok(false),
        _ => Err(error),
    }
}

/// The selected scanout device owns KMS and therefore requires DRM master.
/// Render/import-only devices do not go through this ownership check.
pub(super) fn resume_scanout(drm: &mut DrmDevice, input: &mut Libinput) -> Result<(), String> {
    resume(&mut (drm, input))
}

// Keep the activation transaction independent of live hardware so its
// failure ordering can be exercised without switching the user's VT.
trait ScanoutSession {
    fn acquire_master(&mut self) -> Result<(), String>;
    fn activate(&mut self) -> Result<(), String>;
    fn resume_input(&mut self) -> Result<(), String>;
    fn pause(&mut self);
}

impl ScanoutSession for (&mut DrmDevice, &mut Libinput) {
    fn acquire_master(&mut self) -> Result<(), String> {
        // Smithay ff5fa7d swallows this error in DrmDevice::activate().
        // Check it explicitly before that call can mark scanout active.
        // Once acquired, this fd retains master across activate's repeated
        // ioctl. Remove the guard after adopting Smithay's 85f83ab6 fix.
        //
        // SET_MASTER needs CAP_SYS_ADMIN for an fd that logind or seatd
        // opened, so an ordinary login always gets EACCES here. Those
        // session managers restore master themselves before announcing the
        // resume, so the real requirement is "this fd is master", not "this
        // process could take it".
        match self.0.acquire_master_lock() {
            Ok(()) => Ok(()),
            Err(error) => match is_master(self.0.as_fd()) {
                Ok(true) => Ok(()),
                Ok(false) => Err(format!("DRM master not held after resume: {error}")),
                Err(probe) => Err(format!(
                    "Failed to acquire DRM master ({error}) or query it ({probe})"
                )),
            },
        }
    }

    fn activate(&mut self) -> Result<(), String> {
        self.0
            .activate(false)
            .map_err(|error| format!("Failed to activate DRM device: {error}"))
    }

    fn resume_input(&mut self) -> Result<(), String> {
        self.1
            .resume()
            .map_err(|()| "Failed to resume libinput".into())
    }

    fn pause(&mut self) {
        self.1.suspend();
        self.0.pause();
    }
}

fn resume(session: &mut impl ScanoutSession) -> Result<(), String> {
    let result = session
        .acquire_master()
        .and_then(|()| session.activate())
        .and_then(|()| session.resume_input());
    if result.is_err() {
        session.pause();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeSession {
        fail_at: Option<&'static str>,
        calls: Vec<&'static str>,
        drm_active: bool,
        input_active: bool,
    }

    impl FakeSession {
        fn step(&mut self, name: &'static str) -> Result<(), String> {
            self.calls.push(name);
            if self.fail_at == Some(name) {
                Err(name.into())
            } else {
                Ok(())
            }
        }
    }

    impl ScanoutSession for FakeSession {
        fn acquire_master(&mut self) -> Result<(), String> {
            self.step("master")
        }

        fn activate(&mut self) -> Result<(), String> {
            // Model a backend that can partially change state before failing.
            self.drm_active = true;
            self.step("drm")
        }

        fn resume_input(&mut self) -> Result<(), String> {
            self.input_active = true;
            self.step("input")
        }

        fn pause(&mut self) {
            self.calls.push("pause");
            self.drm_active = false;
            self.input_active = false;
        }
    }

    #[test]
    fn failures_stop_activation_and_roll_back_partial_state() {
        for (failure, expected) in [
            ("master", vec!["master", "pause"]),
            ("drm", vec!["master", "drm", "pause"]),
            ("input", vec!["master", "drm", "input", "pause"]),
        ] {
            let mut session = FakeSession {
                fail_at: Some(failure),
                ..Default::default()
            };
            assert_eq!(resume(&mut session), Err(failure.into()));
            assert_eq!(session.calls, expected);
            assert!(!session.drm_active);
            assert!(!session.input_active);
            session.fail_at = None;
            assert!(
                resume(&mut session).is_ok(),
                "next activation must be retryable"
            );
            assert!(session.drm_active && session.input_active);
        }
    }

    #[test]
    fn input_only_resumes_after_scanout_owns_master_and_activates() {
        let mut session = FakeSession::default();
        assert!(resume(&mut session).is_ok());
        assert_eq!(session.calls, ["master", "drm", "input"]);
        assert!(session.drm_active && session.input_active);
    }
}
