//! Static welcome card shown while `show_welcome_hint` is enabled and no real
//! window is mapped. Its buffer is rebuilt from the live config (binds, config
//! path) on load/reload; render call sites decide current visibility from live
//! config and Space state.

use fontdue::Font;
use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    input::keyboard::{xkb, Keysym},
    utils::{Physical, Size, Transform},
};

use crate::config::{Action, Config, Keybind};

const CARD_W: i32 = 600;
const CARD_RADIUS: f32 = 14.0;
const CARD_BG: (u8, u8, u8, u8) = (22, 30, 34, 235);
const CARD_BORDER: (u8, u8, u8) = (60, 170, 200); // same water-palette accent Toast/Overview use
const BORDER_PX: i32 = 2;
const TITLE_SIZE: f32 = 20.0;
const BODY_SIZE: f32 = 15.0;
const TEXT_RGB: (u8, u8, u8) = (225, 225, 225);
const KEY_RGB: (u8, u8, u8) = (120, 210, 230); // accent, so binds scan first
const DIM_RGB: (u8, u8, u8) = (150, 160, 165);
const PAD: i32 = 24;
const LINE_GAP: i32 = 10;

/// Static card data; callers own its visibility policy.
pub struct WelcomeHint {
    buffer: MemoryRenderBuffer,
    size: (i32, i32),
}

impl WelcomeHint {
    pub fn build(config: &Config) -> Self {
        let content = WelcomeContent::new(&config.terminal, &config.keybinds, &Config::path());
        let rows = content.binds.len() as i32;
        let key_col = PAD + content.key_column_width(crate::toast::font());
        let height = PAD * 2
            + TITLE_SIZE as i32
            + LINE_GAP * 2
            + rows * (BODY_SIZE as i32 + LINE_GAP)
            + LINE_GAP
            + 2 * (BODY_SIZE as i32 + LINE_GAP);
        let width = CARD_W;
        let mut pixels = vec![0u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                let edge_alpha = rounded_rect_coverage(x, y, width, height, CARD_RADIUS);
                if edge_alpha <= 0.0 {
                    continue;
                }
                let (r, g, b, a) = CARD_BG;
                put_pixel(
                    &mut pixels,
                    width,
                    x,
                    y,
                    (r, g, b, (a as f32 * edge_alpha) as u8),
                );
            }
        }
        stroke_rect(&mut pixels, width, height, BORDER_PX, CARD_BORDER);

        let font = crate::toast::font();
        let canvas = (width, height);
        let mut y = PAD + TITLE_SIZE as i32;
        draw_line(
            &mut pixels,
            canvas,
            font,
            "Welcome to TideWM",
            (PAD, y),
            TITLE_SIZE,
            TEXT_RGB,
        );
        y += LINE_GAP * 2;
        for (keys, label) in &content.binds {
            y += BODY_SIZE as i32 + LINE_GAP;
            let rgb = if keys.is_empty() { DIM_RGB } else { KEY_RGB };
            let keys = if keys.is_empty() {
                "(unbound)"
            } else {
                keys.as_str()
            };
            draw_line(&mut pixels, canvas, font, keys, (PAD, y), BODY_SIZE, rgb);
            draw_line(
                &mut pixels,
                canvas,
                font,
                label,
                (key_col, y),
                BODY_SIZE,
                TEXT_RGB,
            );
        }
        y += LINE_GAP + BODY_SIZE as i32 + LINE_GAP;
        draw_line(
            &mut pixels,
            canvas,
            font,
            &format!("Config: {} (reloads on save)", content.config_path),
            (PAD, y),
            BODY_SIZE,
            TEXT_RGB,
        );
        y += BODY_SIZE as i32 + LINE_GAP;
        draw_line(
            &mut pixels,
            canvas,
            font,
            "Set welcome_hint = false there to hide this card",
            (PAD, y),
            BODY_SIZE,
            DIM_RGB,
        );

        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (width, height),
            1,
            Transform::Normal,
            None,
        );
        Self {
            buffer,
            size: (width, height),
        }
    }

    /// Centered on whatever output/render area it's drawn into.
    pub fn render_element(
        &self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Physical>,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let location = (
            ((output_size.w - self.size.0) / 2) as f64,
            ((output_size.h - self.size.1) / 2) as f64,
        );
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &self.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }
}

fn stroke_rect(pixels: &mut [u8], width: i32, height: i32, thickness: i32, rgb: (u8, u8, u8)) {
    let (r, g, b) = rgb;
    let inner_w = width - thickness * 2;
    let inner_h = height - thickness * 2;
    let inner_radius = (CARD_RADIUS - thickness as f32).max(0.0);
    for y in 0..height {
        for x in 0..width {
            let outer = rounded_rect_coverage(x, y, width, height, CARD_RADIUS);
            if outer <= 0.0 {
                continue;
            }
            let inside_inner_bounds =
                x >= thickness && y >= thickness && x < width - thickness && y < height - thickness;
            let inner = if inside_inner_bounds {
                rounded_rect_coverage(x - thickness, y - thickness, inner_w, inner_h, inner_radius)
            } else {
                0.0
            };
            if inner <= 0.0 {
                put_pixel(pixels, width, x, y, (r, g, b, (255.0 * outer) as u8));
            }
        }
    }
}

/// Left-aligned text clipped to the card bounds.
fn draw_line(
    pixels: &mut [u8],
    canvas: (i32, i32),
    font: &Font,
    text: &str,
    (x0, baseline_y): (i32, i32),
    font_size: f32,
    rgb: (u8, u8, u8),
) {
    let (canvas_w, canvas_h) = canvas;
    let mut pen_x = x0;
    for ch in text.chars() {
        if pen_x >= canvas_w - PAD {
            break;
        }
        let (metrics, bitmap) = font.rasterize(ch, font_size);
        let glyph_x0 = pen_x + metrics.xmin;
        let glyph_y0 = baseline_y - metrics.ymin - metrics.height as i32;

        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let coverage = bitmap[gy * metrics.width + gx];
                if coverage == 0 {
                    continue;
                }
                let x = glyph_x0 + gx as i32;
                let y = glyph_y0 + gy as i32;
                if x < 0 || y < 0 || x >= canvas_w || y >= canvas_h {
                    continue;
                }
                blend_text_pixel(pixels, canvas_w, x, y, coverage, rgb);
            }
        }
        pen_x += metrics.advance_width.round() as i32;
    }
}

fn rounded_rect_coverage(x: i32, y: i32, width: i32, height: i32, radius: f32) -> f32 {
    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
    let (fw, fh) = (width as f32, height as f32);

    let dx = (fx - fw / 2.0).abs() - (fw / 2.0 - radius);
    let dy = (fy - fh / 2.0).abs() - (fh / 2.0 - radius);

    if dx <= 0.0 || dy <= 0.0 {
        return 1.0;
    }

    let dist = (dx * dx + dy * dy).sqrt();
    (radius - dist + 0.5).clamp(0.0, 1.0)
}

fn put_pixel(pixels: &mut [u8], width: i32, x: i32, y: i32, (r, g, b, a): (u8, u8, u8, u8)) {
    let i = ((y * width + x) * 4) as usize;
    // Fourcc::Argb8888 in memory (little-endian) is byte order B, G, R, A.
    pixels[i] = b;
    pixels[i + 1] = g;
    pixels[i + 2] = r;
    pixels[i + 3] = a;
}

fn blend_text_pixel(
    pixels: &mut [u8],
    width: i32,
    x: i32,
    y: i32,
    coverage: u8,
    rgb: (u8, u8, u8),
) {
    let i = ((y * width + x) * 4) as usize;
    let t = coverage as f32 / 255.0;
    let (tr, tg, tb) = rgb;
    pixels[i] = (pixels[i] as f32 + (tb as f32 - pixels[i] as f32) * t) as u8;
    pixels[i + 1] = (pixels[i + 1] as f32 + (tg as f32 - pixels[i + 1] as f32) * t) as u8;
    pixels[i + 2] = (pixels[i + 2] as f32 + (tr as f32 - pixels[i + 2] as f32) * t) as u8;
    pixels[i + 3] = pixels[i + 3].max(coverage);
}

/// Card text derived from the live config. Binds are looked up, never
/// assumed, so a rebound or removed action shows what is actually pressed.
struct WelcomeContent {
    /// (key combo, description); an empty combo means nothing is bound.
    binds: Vec<(String, String)>,
    config_path: String,
}

impl WelcomeContent {
    fn new(terminal: &str, keybinds: &[Keybind], path: &std::path::Path) -> Self {
        let terminal = program_name(terminal);
        let terminal_bind = keybinds.iter().find(|bind| {
            matches!(&bind.action, Action::Spawn(cmd)
                if terminal.is_some() && program_name(cmd) == terminal)
        });
        let find = |pred: fn(&Action) -> bool| keybinds.iter().find(|b| pred(&b.action));
        let label = |bind: Option<&Keybind>| bind.map(format_keybind).unwrap_or_default();
        let terminal_text = match terminal {
            Some(name) => format!("open terminal ({name})"),
            None => "open terminal".to_string(),
        };
        Self {
            binds: vec![
                (label(terminal_bind), terminal_text),
                (
                    label(find(|a| matches!(a, Action::CloseWindow))),
                    "close window".into(),
                ),
                (
                    label(find(|a| matches!(a, Action::Quit))),
                    "exit TideWM".into(),
                ),
            ],
            config_path: home_relative(path),
        }
    }

    fn key_column_width(&self, font: &Font) -> i32 {
        let widest = self
            .binds
            .iter()
            .map(|(keys, _)| {
                text_width(
                    font,
                    if keys.is_empty() { "(unbound)" } else { keys },
                    BODY_SIZE,
                )
            })
            .max()
            .unwrap_or(0);
        widest + PAD
    }
}

/// Basename of a command's program, so `spawn:/usr/bin/kitty -1` matches
/// `terminal = kitty`.
fn program_name(cmd: &str) -> Option<&str> {
    let program = cmd.split_whitespace().next()?;
    program.rsplit('/').next().filter(|name| !name.is_empty())
}

fn format_keybind(bind: &Keybind) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (held, name) in [
        (bind.mods.logo, "Super"),
        (bind.mods.ctrl, "Ctrl"),
        (bind.mods.alt, "Alt"),
        (bind.mods.shift, "Shift"),
    ] {
        if held {
            parts.push(name.to_string());
        }
    }
    parts.extend(bind.held_keysyms.iter().copied().map(key_name));
    parts.push(key_name(bind.keysym));
    parts.join("+")
}

fn key_name(keysym: Keysym) -> String {
    let name = xkb::keysym_get_name(keysym);
    if name.chars().count() == 1 {
        name.to_uppercase()
    } else {
        name
    }
}

fn home_relative(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn text_width(font: &Font, text: &str, font_size: f32) -> i32 {
    text.chars()
        .map(|ch| font.metrics(ch, font_size).advance_width)
        .sum::<f32>()
        .ceil() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(mods: crate::config::Mods, key: &str, action: Action) -> Keybind {
        Keybind {
            mods,
            held_keysyms: Vec::new(),
            keysym: xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE),
            action,
        }
    }

    #[test]
    fn content_reports_live_binds_and_path() {
        let logo = crate::config::Mods {
            logo: true,
            ..Default::default()
        };
        let logo_shift = crate::config::Mods {
            shift: true,
            ..logo
        };
        let keybinds = vec![
            bind(logo, "Return", Action::Spawn("kitty".into())),
            bind(logo, "t", Action::Spawn("/usr/bin/foot --server".into())),
            bind(logo, "q", Action::CloseWindow),
            bind(logo_shift, "e", Action::Quit),
        ];
        let content = WelcomeContent::new(
            "foot",
            &keybinds,
            std::path::Path::new("/etc/tide/config.wave"),
        );
        assert_eq!(
            content.binds[0],
            ("Super+T".into(), "open terminal (foot)".into())
        );
        assert_eq!(content.binds[1].0, "Super+Q");
        assert_eq!(content.binds[2].0, "Super+Shift+E");
        assert_eq!(content.config_path, "/etc/tide/config.wave");
    }

    #[test]
    fn missing_binds_are_reported_unbound() {
        let keybinds = vec![bind(
            Default::default(),
            "F1",
            Action::Spawn("kitty".into()),
        )];
        let content = WelcomeContent::new("alacritty", &keybinds, std::path::Path::new("/x"));
        assert!(content.binds.iter().all(|(keys, _)| keys.is_empty()));
    }
}
