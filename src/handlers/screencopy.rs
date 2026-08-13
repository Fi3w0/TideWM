//! wlr-screencopy-unstable-v1: grim's native screenshot protocol (and the
//! only one that supports region capture, e.g. `grim -g "$(slurp)"`).
//!
//! Hand-rolled on `wayland-protocols-wlr` -- there is no Smithay convenience
//! module for it (unlike ext-image-copy-capture, see `handlers/capture.rs`).
//! SHM is the portable and region-capture path. On a DRM session, an
//! eligible full-output request is also offered ARGB8888 DMA-BUF and is
//! rendered directly into the client's imported target. Everything after
//! the request is queued through `src/capture.rs`.

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::{
    backend::allocator::{Buffer, Fourcc},
    output::Output,
    reexports::wayland_server::{
        protocol::wl_shm, Client, DataInit, Dispatch, GlobalDispatch, New,
    },
    utils::{Buffer as BufferCoords, Logical, Rectangle, Size, Transform},
    wayland::{dmabuf::get_dmabuf, shm::with_buffer_contents},
};
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

use crate::{
    capture::{output_capture_size, CaptureCompletion, PendingCapture},
    Smallvil,
};

/// Per-frame state carried from the capture request to the `copy` request.
struct WlrFrameData {
    /// `None` when the request targeted a dead output or an out-of-bounds
    /// region; such frames are failed immediately.
    capture: Option<WlrCapture>,
    overlay_cursor: bool,
    /// A frame object is single-shot: only the first `copy`/`copy_with_damage`
    /// is acted on.
    copied: AtomicBool,
}

struct WlrCapture {
    output: Output,
    rect: Rectangle<i32, BufferCoords>,
}

/// The buffer-space rectangle of `output` a capture should copy out: the
/// full output, or `region` (logical, output-local, what slurp reports)
/// converted through the output's scale into the same upright coordinate
/// space as the offscreen capture target, then clamped to its bounds.
/// `None` when the output has no mode or the region does not intersect it.
fn output_capture_rect(
    output: &Output,
    region: Option<Rectangle<i32, Logical>>,
) -> Option<Rectangle<i32, BufferCoords>> {
    let full = output_capture_size(output)?;
    let full_rect = Rectangle::from_size(full);
    let rect = match region {
        None => full_rect,
        Some(logical) => {
            let scale = output.current_scale().fractional_scale();
            let logical_size = full.to_f64().to_logical(scale, Transform::Normal);
            logical
                .to_f64()
                .to_buffer(scale, Transform::Normal, &logical_size)
                .to_i32_round()
        }
    };
    full_rect.intersection(rect).filter(|rect| !rect.is_empty())
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for Smallvil {
    fn can_view(client: Client, _data: &()) -> bool {
        crate::state::trusted_client(&client)
    }

    fn bind(
        _state: &mut Self,
        _dh: &smithay::reexports::wayland_server::DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &smithay::reexports::wayland_server::DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let (overlay_cursor, output, region, frame) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                overlay_cursor,
                output,
                frame,
            } => (overlay_cursor, output, None, frame),
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
                frame,
            } => (
                overlay_cursor,
                output,
                Some(Rectangle::new((x, y).into(), (width, height).into())),
                frame,
            ),
            zwlr_screencopy_manager_v1::Request::Destroy => return,
            _ => return,
        };

        let capture = Output::from_resource(&output)
            .and_then(|output| output_capture_rect(&output, region).map(|rect| (output, rect)))
            .map(|(output, rect)| WlrCapture { output, rect });

        let Some(rect) = capture.as_ref().map(|capture| capture.rect) else {
            let frame = data_init.init(
                frame,
                WlrFrameData {
                    capture: None,
                    overlay_cursor: false,
                    copied: AtomicBool::new(false),
                },
            );
            frame.failed();
            return;
        };

        let full_output = capture
            .as_ref()
            .and_then(|capture| capture.output.current_mode())
            .map(|mode| mode.size.w == rect.size.w && mode.size.h == rect.size.h)
            .unwrap_or(false);
        let frame = data_init.init(
            frame,
            WlrFrameData {
                capture,
                overlay_cursor: overlay_cursor != 0,
                copied: AtomicBool::new(false),
            },
        );
        frame.buffer(
            wl_shm::Format::Argb8888,
            rect.size.w as u32,
            rect.size.h as u32,
            (rect.size.w * 4) as u32,
        );
        if full_output
            && state.dmabuf_global.is_some()
            && !state.config.has_layer_capture_exclusions()
        {
            frame.linux_dmabuf(
                Fourcc::Argb8888 as u32,
                rect.size.w as u32,
                rect.size.h as u32,
            );
        }
        frame.buffer_done();
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, WlrFrameData> for Smallvil {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &WlrFrameData,
        _dh: &smithay::reexports::wayland_server::DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let (buffer, report_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => return,
            _ => return,
        };

        if data.copied.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(capture) = &data.capture else {
            resource.failed();
            return;
        };

        // Untrusted client input: the buffer must be an SHM buffer in one of
        // the advertised formats and at least the offered size. Anything
        // else fails the frame rather than risking a bad write later.
        let dmabuf = get_dmabuf(&buffer).cloned().ok();
        if let Some(dmabuf) = dmabuf {
            let full_size: Option<Size<i32, BufferCoords>> = output_capture_size(&capture.output);
            let direct_valid = full_size.is_some_and(|size| {
                capture.rect.loc == (0, 0).into()
                    && capture.rect.size == size
                    && dmabuf.size() == size
                    && matches!(dmabuf.format().code, Fourcc::Argb8888 | Fourcc::Xrgb8888)
            }) && state.dmabuf_global.is_some()
                && !state.config.has_layer_capture_exclusions();
            if !direct_valid {
                resource.failed();
                return;
            }
            state.queue_capture(PendingCapture {
                client_id: Some(client.id()),
                output: capture.output.clone(),
                window: None,
                draw_cursor: data.overlay_cursor,
                region: None,
                completion: CaptureCompletion::WlrDmabuf {
                    frame: resource.clone(),
                    dmabuf,
                    report_damage,
                },
            });
            return;
        }

        let valid = with_buffer_contents(&buffer, |_, _, meta| {
            meta.width >= capture.rect.size.w
                && meta.height >= capture.rect.size.h
                && matches!(
                    meta.format,
                    wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
                )
        })
        .unwrap_or(false);
        if !valid {
            resource.failed();
            return;
        }

        state.queue_capture(PendingCapture {
            client_id: Some(client.id()),
            output: capture.output.clone(),
            window: None,
            draw_cursor: data.overlay_cursor,
            region: Some(capture.rect),
            completion: CaptureCompletion::Wlr {
                frame: resource.clone(),
                buffer,
                report_damage,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{Mode, PhysicalProperties, Subpixel};

    fn rotated_test_output() -> Output {
        let output = Output::new(
            "screencopy-test".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".to_string(),
                model: "test".to_string(),
                serial_number: "test".to_string(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (113, 71).into(),
                refresh: 73_000,
            }),
            Some(Transform::_90),
            None,
            None,
        );
        output
    }

    #[test]
    fn rotated_output_region_uses_upright_capture_coordinates() {
        let output = rotated_test_output();

        assert_eq!(
            output_capture_rect(&output, None),
            Some(Rectangle::from_size((71, 113).into()))
        );
        assert_eq!(
            output_capture_rect(
                &output,
                Some(Rectangle::new((5, 7).into(), (11, 13).into()))
            ),
            Some(Rectangle::new((5, 7).into(), (11, 13).into()))
        );
    }
}
