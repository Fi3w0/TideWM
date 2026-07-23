//! Compositor-owned screen-share source picker.
//!
//! The portal method runs on the D-Bus thread, while this UI and all window
//! inspection stay on the compositor thread. A bounded response channel is
//! the only value crossing that boundary.

use std::sync::mpsc;

use fontdue::Font;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::element::{
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            Kind,
        },
        renderer::gles::GlesRenderer,
    },
    input::keyboard::Keysym,
    utils::Transform,
};

use crate::{
    screencast::{ScreencastSource, SourceChoice},
    state::Smallvil,
};

const MONITOR: u32 = 1;
const WINDOW: u32 = 2;
const VIRTUAL: u32 = 4;
const PANEL_WIDTH: i32 = 720;
const ROW_HEIGHT: i32 = 54;
const PAD: i32 = 22;

pub(crate) struct SourcePicker {
    output_name: String,
    choices: Vec<SourceChoice>,
    selected: usize,
    location: (i32, i32),
    size: (i32, i32),
    buffer: MemoryRenderBuffer,
    response: Option<mpsc::SyncSender<Option<SourceChoice>>>,
}

impl SourcePicker {
    fn new(
        output_name: String,
        output_size: (i32, i32),
        choices: Vec<SourceChoice>,
        response: mpsc::SyncSender<Option<SourceChoice>>,
    ) -> Self {
        let height = (PAD * 2 + ROW_HEIGHT * (choices.len() as i32 + 1)).min(output_size.1.max(1));
        let width = PANEL_WIDTH.min(output_size.0.max(1));
        let location = ((output_size.0 - width) / 2, (output_size.1 - height) / 2);
        let buffer = build_buffer(width, height, &choices, 0);
        Self {
            output_name,
            choices,
            selected: 0,
            location,
            size: (width, height),
            buffer,
            response: Some(response),
        }
    }

    pub(crate) fn output_name(&self) -> &str {
        &self.output_name
    }

    pub(crate) fn render_element(
        &self,
        renderer: &mut GlesRenderer,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (self.location.0 as f64, self.location.1 as f64),
            &self.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.choices.len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.buffer = build_buffer(self.size.0, self.size.1, &self.choices, self.selected);
    }

    fn complete(mut self, accepted: bool) {
        let choice = accepted.then(|| self.choices[self.selected].clone());
        if let Some(response) = self.response.take() {
            let _ = response.send(choice);
        }
    }
}

impl Drop for SourcePicker {
    fn drop(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(None);
        }
    }
}

impl Smallvil {
    pub(crate) fn open_screencast_picker(
        &mut self,
        source_types: u32,
        response: mpsc::SyncSender<Option<SourceChoice>>,
    ) {
        // Replacing a stale picker cancels its pending portal request.
        self.screencast_picker.take();
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            let _ = response.send(None);
            return;
        }
        let Some(ui_output) = self.primary_output() else {
            let _ = response.send(None);
            return;
        };
        let Some(ui_mode) = ui_output.current_mode() else {
            let _ = response.send(None);
            return;
        };

        let mut choices = Vec::new();
        if source_types & MONITOR != 0 {
            for output in self.space.outputs() {
                let Some(mode) = output.current_mode() else {
                    continue;
                };
                choices.push(SourceChoice {
                    source: ScreencastSource::Output(output.name()),
                    source_type: MONITOR,
                    width: mode.size.w.max(1) as u32,
                    height: mode.size.h.max(1) as u32,
                    label: format!(
                        "Monitor — {} ({}×{})",
                        output.name(),
                        mode.size.w,
                        mode.size.h
                    ),
                });
            }
        }
        if source_types & WINDOW != 0 {
            for (surface, id) in &self.foreign_toplevel_numeric_ids {
                let Some(window) = self.mapped_toplevel_window(surface) else {
                    continue;
                };
                let Some(output) = self.capture_output_for_screencast(surface) else {
                    continue;
                };
                let scale = output.current_scale().fractional_scale();
                let size = window.geometry().size;
                choices.push(SourceChoice {
                    source: ScreencastSource::Window(*id),
                    source_type: WINDOW,
                    width: (size.w as f64 * scale).round().max(1.0) as u32,
                    height: (size.h as f64 * scale).round().max(1.0) as u32,
                    label: format!("Window — {}", crate::tab_strip::window_title(surface)),
                });
            }
        }
        if source_types & VIRTUAL != 0 {
            // A virtual source is intentionally isolated from the physical
            // source identity exposed to the requester. Until headless DRM
            // outputs are available, its backing content mirrors the current
            // output while still negotiating the portal's VIRTUAL type.
            choices.push(SourceChoice {
                source: ScreencastSource::Output(ui_output.name()),
                source_type: VIRTUAL,
                width: ui_mode.size.w.max(1) as u32,
                height: ui_mode.size.h.max(1) as u32,
                label: format!("Virtual display — {}×{}", ui_mode.size.w, ui_mode.size.h),
            });
        }

        if choices.len() == 1 {
            let _ = response.send(choices.pop());
            return;
        }
        if choices.is_empty() {
            let _ = response.send(None);
            return;
        }
        self.screencast_picker = Some(SourcePicker::new(
            ui_output.name(),
            (ui_mode.size.w, ui_mode.size.h),
            choices,
            response,
        ));
        self.request_redraw();
    }

    /// Returns true when the picker owns the key and it must not reach the
    /// focused client.
    pub(crate) fn handle_screencast_picker_key(&mut self, keysym: Keysym) -> bool {
        let Some(picker) = self.screencast_picker.as_mut() else {
            return false;
        };
        match keysym {
            Keysym::Up | Keysym::Left => picker.move_selection(-1),
            Keysym::Down | Keysym::Right | Keysym::Tab => picker.move_selection(1),
            Keysym::Return | Keysym::KP_Enter => {
                let picker = self.screencast_picker.take().unwrap();
                picker.complete(true);
            }
            Keysym::Escape => {
                let picker = self.screencast_picker.take().unwrap();
                picker.complete(false);
            }
            _ => return true,
        }
        self.request_redraw();
        true
    }
}

fn build_buffer(
    width: i32,
    height: i32,
    choices: &[SourceChoice],
    selected: usize,
) -> MemoryRenderBuffer {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    fill(&mut pixels, width, height, (15, 18, 24, 245));
    draw_text(
        &mut pixels,
        width,
        height,
        crate::toast::font(),
        "Share a source",
        PAD,
        PAD + 22,
    );
    let visible_rows = ((height - PAD * 2) / ROW_HEIGHT - 1).max(1) as usize;
    let first = selected.saturating_sub(visible_rows - 1);
    for (row, (index, choice)) in choices
        .iter()
        .enumerate()
        .skip(first)
        .take(visible_rows)
        .enumerate()
    {
        let y = PAD + ROW_HEIGHT * (row as i32 + 1);
        let color = if index == selected {
            (38, 122, 150, 255)
        } else {
            (35, 39, 48, 255)
        };
        fill_rect(
            &mut pixels,
            width,
            height,
            (PAD, y, width - PAD * 2, ROW_HEIGHT - 6),
            color,
        );
        draw_text(
            &mut pixels,
            width,
            height,
            crate::toast::font(),
            &choice.label,
            PAD + 14,
            y + 31,
        );
    }
    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (width, height),
        1,
        Transform::Normal,
        None,
    )
}

fn fill(pixels: &mut [u8], width: i32, height: i32, color: (u8, u8, u8, u8)) {
    fill_rect(pixels, width, height, (0, 0, width, height), color);
}

fn fill_rect(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    (x, y, w, h): (i32, i32, i32, i32),
    (r, g, b, a): (u8, u8, u8, u8),
) {
    for py in y.max(0)..(y + h).min(height) {
        for px in x.max(0)..(x + w).min(width) {
            let index = ((py * width + px) * 4) as usize;
            pixels[index..index + 4].copy_from_slice(&[b, g, r, a]);
        }
    }
}

fn draw_text(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    font: &Font,
    text: &str,
    mut pen_x: i32,
    baseline: i32,
) {
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, 18.0);
        let x0 = pen_x + metrics.xmin;
        let y0 = baseline - metrics.ymin - metrics.height as i32;
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let coverage = bitmap[gy * metrics.width + gx];
                let x = x0 + gx as i32;
                let y = y0 + gy as i32;
                if coverage == 0 || x < 0 || y < 0 || x >= width || y >= height {
                    continue;
                }
                let i = ((y * width + x) * 4) as usize;
                let t = coverage as f32 / 255.0;
                for (offset, target) in [(0, 240u8), (1, 240), (2, 240)] {
                    pixels[i + offset] = (pixels[i + offset] as f32
                        + (target as f32 - pixels[i + offset] as f32) * t)
                        as u8;
                }
            }
        }
        pen_x += metrics.advance_width.round().max(1.0) as i32;
        if pen_x >= width - PAD {
            break;
        }
    }
}
