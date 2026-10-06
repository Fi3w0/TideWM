use smithay::{
    backend::drm::DrmDevice,
    reexports::{drm::Device as DrmRawDevice, input::Libinput},
};

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
        self.0
            .acquire_master_lock()
            .map_err(|error| format!("Failed to acquire DRM master: {error}"))
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
