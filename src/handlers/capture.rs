//! ext-image-copy-capture-v1 (screenshot) protocol handling.
//!
//! Uses Smithay's `image_copy_capture` + `image_capture_source` modules,
//! which cover the protocol boilerplate including buffer validation against
//! the advertised constraints. TideWM's own work is the actual capture: a
//! frame request is validated, queued as a `PendingCapture` (see
//! `src/capture.rs`), and drained by a backend render loop where a GL
//! renderer is available. Output and foreign-toplevel sources share the
//! same bounded render/readback queue.

use smithay::{
    output::{Output, WeakOutput},
    reexports::wayland_server::protocol::wl_shm,
    utils::{IsAlive, Transform},
    wayland::{
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState, ToplevelCaptureSourceHandler, ToplevelCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, Frame, ImageCopyCaptureHandler,
            ImageCopyCaptureState, Session, SessionRef,
        },
    },
};

use crate::{
    capture::{CaptureCompletion, PendingCapture},
    Smallvil,
};

/// Bounds compositor-owned session handles even if a client creates them
/// faster than the normal protocol cleanup tick can prune dead sessions.
const MAX_CAPTURE_SESSIONS: usize = 64;

impl ImageCaptureSourceHandler for Smallvil {
    fn source_destroyed(&mut self, _source: ImageCaptureSource) {
        // Sources only carry a WeakOutput (see `output_source_created`), so
        // there is nothing to tear down: capture of a destroyed output
        // already fails through the weak reference, at constraint time or
        // in `cleanup_capture`.
    }
}
smithay::delegate_image_capture_source!(Smallvil);

impl OutputCaptureSourceHandler for Smallvil {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}
smithay::delegate_output_capture_source!(Smallvil);

impl ToplevelCaptureSourceHandler for Smallvil {
    fn toplevel_capture_source_state(&mut self) -> &mut ToplevelCaptureSourceState {
        &mut self.toplevel_capture_source_state
    }

    fn toplevel_source_created(
        &mut self,
        source: ImageCaptureSource,
        toplevel: smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle,
    ) {
        let identifier = toplevel.identifier();
        if let Some(surface) = self.foreign_toplevels.iter().find_map(|(surface, handle)| {
            (handle.identifier() == identifier).then(|| surface.clone())
        }) {
            source.user_data().insert_if_missing(|| surface);
        }
    }
}
smithay::delegate_toplevel_capture_source!(Smallvil);

impl ImageCopyCaptureHandler for Smallvil {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        if let Some(surface) = source
            .user_data()
            .get::<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>(
        ) {
            let window = self.mapped_toplevel_window(surface)?;
            let output = self.capture_output_for_screencast(surface)?;
            let scale = output.current_scale().fractional_scale();
            let geometry = window.geometry();
            return Some(BufferConstraints {
                size: (
                    (geometry.size.w as f64 * scale).round().max(1.0) as i32,
                    (geometry.size.h as f64 * scale).round().max(1.0) as i32,
                )
                    .into(),
                shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
                dma: None,
            });
        }
        let weak_output = source.user_data().get::<WeakOutput>()?;
        let output = weak_output.upgrade()?;
        let mode = output.current_mode()?;

        Some(BufferConstraints {
            size: mode.size.to_logical(1).to_buffer(1, Transform::Normal),
            shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
            dma: None,
        })
    }

    fn new_session(&mut self, session: Session) {
        // The owned `Session` stops itself on drop, so it must be kept as
        // long as the client wants it. Dead sessions are filtered out on
        // the backend cleanup ticks (`cleanup_capture`).
        self.capture_sessions.retain(|session| session.alive());
        if self.capture_sessions.len() >= MAX_CAPTURE_SESSIONS {
            tracing::warn!(
                limit = MAX_CAPTURE_SESSIONS,
                "Capture session limit reached; stopping new session"
            );
            session.stop();
        } else {
            self.capture_sessions.push(session);
        }
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        if let Some(surface) = session
            .source()
            .user_data()
            .get::<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>()
            .cloned()
        {
            match self.capture_output_for_screencast(&surface) {
                Some(output) => self.queue_capture(PendingCapture {
                    output,
                    window: Some(surface),
                    draw_cursor: false,
                    region: None,
                    completion: CaptureCompletion::Ext(frame),
                }),
                None => frame.fail(CaptureFailureReason::Unknown),
            }
            return;
        }
        let output = session
            .source()
            .user_data()
            .get::<WeakOutput>()
            .and_then(WeakOutput::upgrade);
        match output {
            Some(output) => {
                self.queue_capture(PendingCapture {
                    output,
                    window: None,
                    draw_cursor: session.draw_cursor(),
                    region: None,
                    completion: CaptureCompletion::Ext(frame),
                });
            }
            None => frame.fail(CaptureFailureReason::Unknown),
        }
    }
}
smithay::delegate_image_copy_capture!(Smallvil);

impl Smallvil {
    pub(crate) fn capture_output_for_screencast(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<Output> {
        self.rendered_output_for_surface(surface)
    }
}
