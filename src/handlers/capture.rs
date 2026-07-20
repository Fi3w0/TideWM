//! ext-image-copy-capture-v1 (screenshot) protocol handling.
//!
//! Uses Smithay's `image_copy_capture` + `image_capture_source` modules,
//! which cover the protocol boilerplate including buffer validation against
//! the advertised constraints. TideWM's own work is the actual capture: a
//! frame request is validated, queued as a `PendingCapture` (see
//! `src/capture.rs`), and drained by a backend render loop where a GL
//! renderer is available. Only output sources are supported; anything else
//! is rejected with no constraints.

use smithay::{
    output::{Output, WeakOutput},
    reexports::wayland_server::protocol::wl_shm,
    utils::Transform,
    wayland::{
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, Frame, ImageCopyCaptureHandler,
            ImageCopyCaptureState, Session, SessionRef,
        },
    },
};

use crate::{
    Smallvil,
    capture::{CaptureCompletion, PendingCapture},
};

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

impl ImageCopyCaptureHandler for Smallvil {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
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
        self.capture_sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let output = session
            .source()
            .user_data()
            .get::<WeakOutput>()
            .and_then(WeakOutput::upgrade);
        match output {
            Some(output) => {
                self.pending_captures.push(PendingCapture {
                    output,
                    draw_cursor: session.draw_cursor(),
                    region: None,
                    completion: CaptureCompletion::Ext(frame),
                });
                self.request_redraw();
            }
            None => frame.fail(CaptureFailureReason::Unknown),
        }
    }
}
smithay::delegate_image_copy_capture!(Smallvil);
