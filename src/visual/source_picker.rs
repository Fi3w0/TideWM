//! TideWM's compositor-owned screen-share source picker.
//!
//! The portal method runs on the D-Bus thread, while this UI and all window
//! inspection stay on the compositor thread. A bounded response channel is
//! the only value crossing that boundary.
//!
//! The panel is CPU-composited (same technique as `toast.rs`): one
//! in-memory BGRA buffer rebuilt on state changes, rounded and bordered
//! to match the rest of the water identity. Layout and interaction follow
//! the KDE portal picker's shape: a header with a hint, one row per
//! source with an icon, a hover highlight, click-to-share, arrow keys,
//! Enter/Escape, and an explicit Cancel button. Pointer input is modal --
//! clicks and motion are consumed here, never leaked to a client.

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
const ROW_HEIGHT: i32 = 60;
const PAD: i32 = 22;
const HEADER_HEIGHT: i32 = 46;
const FOOTER_HEIGHT: i32 = 52;
const CORNER_RADIUS: i32 = 14;

/// Palette -- the rice's ink/cyan, in toast-style tuples.
const BG: (u8, u8, u8, u8) = (10, 15, 20, 240);
const TEXT: (u8, u8, u8) = (228, 237, 242);
const DIM: (u8, u8, u8) = (107, 126, 138);
const CYAN: (u8, u8, u8) = (140, 199, 220);
const CYAN_TINT: (u8, u8, u8, u8) = (140, 199, 220, 42);
const HOVER_TINT: (u8, u8, u8, u8) = (255, 255, 255, 18);
const ALERT: (u8, u8, u8) = (232, 117, 107);

pub(crate) struct SourcePicker {
    output_name: String,
    choices: Vec<SourceChoice>,
    selected: usize,
    /// Row under the pointer, or None. `hover_cancel` is the Cancel button.
    hovered: Option<usize>,
    hover_cancel: bool,
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
        let rows = choices.len() as i32;
        let height = (PAD + HEADER_HEIGHT + rows * ROW_HEIGHT + FOOTER_HEIGHT + PAD)
            .min(output_size.1.max(1));
        let width = PANEL_WIDTH.min(output_size.0.max(1));
        let location = ((output_size.0 - width) / 2, (output_size.1 - height) / 2);
        let buffer = build_buffer(width, height, &choices, 0, None, false);
        Self {
            output_name,
            choices,
            selected: 0,
            hovered: None,
            hover_cancel: false,
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

    /// Whether a global logical position is inside the panel.
    pub(crate) fn contains(&self, global: (f64, f64)) -> bool {
        let (x, y) = global;
        x >= self.location.0 as f64
            && x < (self.location.0 + self.size.0) as f64
            && y >= self.location.1 as f64
            && y < (self.location.1 + self.size.1) as f64
    }

    /// Applies a pointer hover at a global position, returning whether the
    /// rendered buffer changed. Rows and the Cancel button highlight.
    pub(crate) fn hover_at(&mut self, global: (f64, f64)) -> bool {
        let local = self.to_local(global);
        let row = self.row_at(local);
        let cancel = self.cancel_at(local);
        if row == self.hovered && cancel == self.hover_cancel {
            return false;
        }
        self.hovered = row;
        self.hover_cancel = cancel;
        self.buffer = build_buffer(
            self.size.0,
            self.size.1,
            &self.choices,
            self.selected,
            self.hovered,
            self.hover_cancel,
        );
        true
    }

    /// Routes a press at a global position. `Some(true)` accepts the
    /// hovered row, `Some(false)` cancels, `None` means the press missed
    /// every interactive element.
    pub(crate) fn click_at(&mut self, global: (f64, f64)) -> Option<bool> {
        let local = self.to_local(global);
        if let Some(row) = self.row_at(local) {
            self.selected = row;
            return Some(true);
        }
        if self.cancel_at(local) {
            return Some(false);
        }
        None
    }

    fn to_local(&self, global: (f64, f64)) -> (f64, f64) {
        (global.0 - self.location.0 as f64, global.1 - self.location.1 as f64)
    }

    fn row_at(&self, local: (f64, f64)) -> Option<usize> {
        let (x, y) = local;
        if x < PAD as f64 || x >= (self.size.0 - PAD) as f64 {
            return None;
        }
        let rows_top = (PAD + HEADER_HEIGHT) as f64;
        let row = ((y - rows_top) / ROW_HEIGHT as f64).floor() as isize;
        (row >= 0 && (row as usize) < self.choices.len()).then_some(row as usize)
    }

    fn cancel_at(&self, local: (f64, f64)) -> bool {
        let (x, y) = local;
        let bottom = self.size.1 - PAD;
        if y < (bottom - 30) as f64 || y >= bottom as f64 {
            return false;
        }
        let right = self.size.0 - PAD;
        x >= (right - 92) as f64 && x < right as f64
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.choices.len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.buffer = build_buffer(
            self.size.0,
            self.size.1,
            &self.choices,
            self.selected,
            self.hovered,
            self.hover_cancel,
        );
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

    /// Returns true when the picker consumed the motion (it is modal: no
    /// motion reaches a client while it is open).
    pub(crate) fn handle_screencast_picker_motion(&mut self, global: (f64, f64)) -> bool {
        let Some(picker) = self.screencast_picker.as_mut() else {
            return false;
        };
        let changed = picker.hover_at(global);
        if changed {
            self.request_redraw();
        }
        true
    }

    /// Returns true when the picker consumed the press (click a row to
    /// share, the Cancel button or the panel's outside to cancel).
    pub(crate) fn handle_screencast_picker_button(&mut self, global: (f64, f64)) -> bool {
        let Some(picker) = self.screencast_picker.as_ref() else {
            return false;
        };
        if !picker.contains(global) {
            // Click outside the panel cancels, same as KDE's picker.
            let picker = self.screencast_picker.take().unwrap();
            picker.complete(false);
            self.request_redraw();
            return true;
        }
        match self.screencast_picker.as_mut().unwrap().click_at(global) {
            Some(true) => {
                let picker = self.screencast_picker.take().unwrap();
                picker.complete(true);
                self.request_redraw();
                true
            }
            Some(false) => {
                let picker = self.screencast_picker.take().unwrap();
                picker.complete(false);
                self.request_redraw();
                true
            }
            None => true,
        }
    }
}

fn build_buffer(
    width: i32,
    height: i32,
    choices: &[SourceChoice],
    selected: usize,
    hovered: Option<usize>,
    hover_cancel: bool,
) -> MemoryRenderBuffer {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    // Panel body: rounded, ink, thin cyan border.
    fill_rounded_rect(&mut pixels, width, height, (0, 0, width, height), CORNER_RADIUS, BG);
    stroke_rounded_rect(
        &mut pixels,
        width,
        height,
        (0, 0, width, height),
        CORNER_RADIUS,
        (CYAN.0, CYAN.1, CYAN.2, 60),
    );

    let font = crate::toast::font();

    // Header: title + hint.
    draw_text(&mut pixels, width, height, font, "Share a source", PAD + 2, PAD + 26, 21.0, TEXT);
    let hint = "↑↓ navigate · Enter share · Esc cancel";
    let hint_w = text_width(font, hint, 12.0);
    draw_text(
        &mut pixels,
        width,
        height,
        font,
        hint,
        width - PAD - hint_w,
        PAD + 26,
        12.0,
        DIM,
    );

    // Rows.
    let rows_top = PAD + HEADER_HEIGHT;
    for (row, choice) in choices.iter().enumerate() {
        let y = rows_top + row as i32 * ROW_HEIGHT;
        let is_selected = row == selected;
        let is_hovered = hovered == Some(row);
        if is_selected {
            fill_rect(
                &mut pixels,
                width,
                height,
                (PAD, y, width - PAD * 2, ROW_HEIGHT - 6),
                CYAN_TINT,
            );
            // Accent bar on the selected row's leading edge.
            fill_rect(
                &mut pixels,
                width,
                height,
                (PAD, y + 6, 4, ROW_HEIGHT - 18),
                (CYAN.0, CYAN.1, CYAN.2, 255),
            );
        } else if is_hovered {
            fill_rect(
                &mut pixels,
                width,
                height,
                (PAD, y, width - PAD * 2, ROW_HEIGHT - 6),
                HOVER_TINT,
            );
        }
        let accent = if is_selected { CYAN } else if is_hovered { TEXT } else { DIM };
        draw_source_icon(
            &mut pixels,
            width,
            height,
            choice.source_type,
            PAD + 16,
            y + (ROW_HEIGHT - 24) / 2,
            accent,
        );
        draw_text(
            &mut pixels,
            width,
            height,
            font,
            &choice.label,
            PAD + 56,
            y + 24,
            16.0,
            TEXT,
        );
        let tag = match choice.source_type {
            MONITOR => "Monitor",
            WINDOW => "Window",
            _ => "Virtual",
        };
        let tag_w = text_width(font, tag, 11.0);
        draw_text(
            &mut pixels,
            width,
            height,
            font,
            tag,
            width - PAD - 14 - tag_w,
            y + 24,
            11.0,
            if is_selected { CYAN } else { DIM },
        );
    }

    // Footer: hint on the left, Cancel button on the right.
    let footer_y = height - PAD - 30;
    draw_text(
        &mut pixels,
        width,
        height,
        font,
        "Share with an app that requests screen access",
        PAD + 2,
        footer_y + 21,
        12.0,
        DIM,
    );
    let cancel_bg = if hover_cancel {
        (ALERT.0, ALERT.1, ALERT.2, 72)
    } else {
        (ALERT.0, ALERT.1, ALERT.2, 40)
    };
    fill_rounded_rect(
        &mut pixels,
        width,
        height,
        (width - PAD - 92, footer_y, 92, 30),
        9,
        cancel_bg,
    );
    let cancel_w = text_width(font, "Cancel", 14.0);
    draw_text(
        &mut pixels,
        width,
        height,
        font,
        "Cancel",
        width - PAD - 46 - cancel_w / 2,
        footer_y + 21,
        14.0,
        ALERT,
    );

    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (width, height),
        1,
        Transform::Normal,
        None,
    )
}

fn text_width(font: &Font, text: &str, size: f32) -> i32 {
    let mut width = 0;
    for ch in text.chars() {
        let (metrics, _) = font.rasterize(ch, size);
        width += metrics.advance_width.round().max(1.0) as i32;
    }
    width
}

/// A small geometric glyph: monitor = outlined screen on a stand, window =
/// outlined screen with a title bar.
fn draw_source_icon(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    source_type: u32,
    x: i32,
    y: i32,
    color: (u8, u8, u8),
) {
    let (r, g, b) = color;
    let stroke = |pixels: &mut [u8], x: i32, y: i32, w: i32, h: i32| {
        fill_rect(pixels, width, height, (x, y, w, 2), (r, g, b, 255));
        fill_rect(pixels, width, height, (x, y + h - 2, w, 2), (r, g, b, 255));
        fill_rect(pixels, width, height, (x, y, 2, h), (r, g, b, 255));
        fill_rect(pixels, width, height, (x + w - 2, y, 2, h), (r, g, b, 255));
    };
    match source_type {
        WINDOW => {
            // Window: rect outline + title bar.
            stroke(pixels, x, y, 24, 18);
            fill_rect(pixels, width, height, (x, y, 24, 5), (r, g, b, 255));
        }
        VIRTUAL => {
            // Virtual: same screen, plus a smaller one behind it.
            stroke(pixels, x + 6, y, 24, 18);
            stroke(pixels, x, y + 4, 24, 18);
        }
        _ => {
            // Monitor: rect outline + stand.
            stroke(pixels, x, y, 24, 16);
            fill_rect(pixels, width, height, (x + 8, y + 16, 8, 3), (r, g, b, 255));
            fill_rect(pixels, width, height, (x + 10, y + 19, 4, 2), (r, g, b, 255));
        }
    }
}

fn in_rounded_rect(px: i32, py: i32, x: i32, y: i32, w: i32, h: i32, r: i32) -> bool {
    if px < x || py < y || px >= x + w || py >= y + h {
        return false;
    }
    let r = r.min(w / 2).min(h / 2);
    let (cx, cy) = (
        px.clamp(x + r, x + w - r - 1),
        py.clamp(y + r, y + h - r - 1),
    );
    let (dx, dy) = (px - cx, py - cy);
    dx * dx + dy * dy <= r * r
}

fn fill_rounded_rect(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    (x, y, w, h): (i32, i32, i32, i32),
    radius: i32,
    color: (u8, u8, u8, u8),
) {
    for py in y.max(0)..(y + h).min(height) {
        for px in x.max(0)..(x + w).min(width) {
            if !in_rounded_rect(px, py, x, y, w, h, radius) {
                continue;
            }
            let index = ((py * width + px) * 4) as usize;
            composite_pixel(pixels, index, color);
        }
    }
}

/// A 1px rounded outline: fill the rounded rect, then re-draw the body
/// with the background color inset by one, leaving only the border.
fn stroke_rounded_rect(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    (x, y, w, h): (i32, i32, i32, i32),
    radius: i32,
    color: (u8, u8, u8, u8),
) {
    fill_rounded_rect(pixels, width, height, (x, y, w, h), radius, color);
    fill_rounded_rect(
        pixels,
        width,
        height,
        (x + 1, y + 1, w - 2, h - 2),
        radius.saturating_sub(1),
        BG,
    );
}

fn fill_rect(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    (x, y, w, h): (i32, i32, i32, i32),
    color: (u8, u8, u8, u8),
) {
    for py in y.max(0)..(y + h).min(height) {
        for px in x.max(0)..(x + w).min(width) {
            let index = ((py * width + px) * 4) as usize;
            composite_pixel(pixels, index, color);
        }
    }
}

/// Straight-alpha composite one pixel into the buffer. This is what keeps
/// translucent fills (the selected/hover row tints, the Cancel button)
/// legible: without it, a 16%-alpha box was *written* at 16% alpha, and
/// the text drawn on top only blended RGB -- so the whole row, text
/// included, rendered at 16% opacity and the label vanished into the
/// background. Compositing the tint over the panel first keeps the pixel
/// nearly opaque while the tint itself stays translucent.
fn composite_pixel(pixels: &mut [u8], index: usize, (r, g, b, a): (u8, u8, u8, u8)) {
    let sa = a as f32 / 255.0;
    if sa <= 0.0 {
        return;
    }
    let da = pixels[index + 3] as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        return;
    }
    let src = [b as f32, g as f32, r as f32];
    for (offset, src_channel) in src.iter().enumerate() {
        let dst = pixels[index + offset] as f32;
        let value = (src_channel * sa + dst * da * (1.0 - sa)) / out_a;
        pixels[index + offset] = value.round().clamp(0.0, 255.0) as u8;
    }
    pixels[index + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[allow(clippy::too_many_arguments)] // CPU text rasterization helper
fn draw_text(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    font: &Font,
    text: &str,
    mut pen_x: i32,
    baseline: i32,
    size: f32,
    color: (u8, u8, u8),
) {
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size);
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
                for (offset, target) in [(0, color.2), (1, color.1), (2, color.0)] {
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
