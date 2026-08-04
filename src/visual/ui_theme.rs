//! Automatic palette for compositor-owned chrome.
//!
//! Toasts and diagnostics should look like the currently configured WM, not
//! like a second hard-coded theme. The active/inactive/urgent border gradients
//! already are TideWM's user-facing decoration palette, so first-party UI
//! derives from those colors and chooses readable text from relative
//! luminance. Reloading a theme therefore recolors the next compositor panel
//! without adding another set of color knobs.

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiTheme {
    pub panel_from: [u8; 3],
    pub panel_to: [u8; 3],
    pub accent_from: [u8; 3],
    pub accent_to: [u8; 3],
    pub urgent_from: [u8; 3],
    pub urgent_to: [u8; 3],
    pub text: [u8; 3],
    pub muted_text: [u8; 3],
    pub radius: i32,
    /// Popup border stroke width, logical pixels. Auto matches
    /// `border.width` (the same thickness window borders use) unless
    /// `[popup] { border_width }` pins it.
    pub border_width: f32,
    /// `[popup] { border_color }` pin. `None` (the default/auto case)
    /// means `accent()`'s gradient, same as every other border in TideWM.
    popup_border_color: Option<[u8; 3]>,
}

impl UiTheme {
    pub fn from_config(config: &Config) -> Self {
        let inactive_from = rgb(config.border.inactive_from);
        let inactive_to = rgb(config.border.inactive_to);
        let seed = mix(inactive_from, inactive_to, 0.5);
        let light_panel = luminance(seed) > 0.46;
        let neutral = if light_panel {
            [255, 255, 255]
        } else {
            [0, 0, 0]
        };
        let panel_from = mix(inactive_from, neutral, 0.18);
        let panel_to = mix(inactive_to, neutral, 0.30);
        let panel_mid = mix(panel_from, panel_to, 0.5);
        let text = if luminance(panel_mid) > 0.48 {
            [14, 17, 19]
        } else {
            [245, 249, 250]
        };
        let muted_text = mix(panel_mid, text, 0.72);
        let radius = if config.rounding.enabled {
            (config.rounding.radii.iter().sum::<f32>() / 4.0)
                .round()
                .clamp(8.0, 28.0) as i32
        } else {
            10
        };
        let radius = config
            .popup
            .radius
            .map(|radius| radius.round().clamp(4.0, 40.0) as i32)
            .unwrap_or(radius);
        // A popup pill/panel is small; a window's full configured border
        // width can read as too heavy on it, so auto stays in a narrower
        // band than `border.width` itself is allowed to.
        let border_width = config
            .popup
            .border_width
            .unwrap_or(config.border.width)
            .clamp(1.0, 4.0);
        let popup_border_color = config.popup.border_color.map(rgb);
        Self {
            panel_from,
            panel_to,
            accent_from: rgb(config.border.active_from),
            accent_to: rgb(config.border.active_to),
            urgent_from: rgb(config.border.urgent_from),
            urgent_to: rgb(config.border.urgent_to),
            text,
            muted_text,
            radius,
            border_width,
            popup_border_color,
        }
    }

    pub fn accent(self, urgent: bool, t: f32) -> [u8; 3] {
        if urgent {
            mix(self.urgent_from, self.urgent_to, t)
        } else {
            mix(self.accent_from, self.accent_to, t)
        }
    }

    /// Border color for compositor-owned popup chrome: the `[popup]
    /// { border_color }` pin if set, else the same gradient `accent()`
    /// gives window borders.
    pub fn popup_accent(self, urgent: bool, t: f32) -> [u8; 3] {
        self.popup_border_color.unwrap_or_else(|| self.accent(urgent, t))
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            panel_from: [8, 18, 24],
            panel_to: [3, 9, 12],
            accent_from: [46, 199, 255],
            accent_to: [92, 255, 210],
            urgent_from: [94, 232, 255],
            urgent_to: [71, 117, 255],
            text: [245, 249, 250],
            muted_text: [174, 184, 188],
            radius: 12,
            border_width: 2.0,
            popup_border_color: None,
        }
    }
}

fn rgb(color: [f32; 4]) -> [u8; 3] {
    color[..3]
        .try_into()
        .map(|rgb: [f32; 3]| rgb.map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as u8))
        .unwrap()
}

pub(crate) fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    std::array::from_fn(|index| {
        (a[index] as f32 + (b[index] as f32 - a[index] as f32) * t).round() as u8
    })
}

fn luminance(rgb: [u8; 3]) -> f32 {
    let linear = rgb.map(|channel| {
        let value = channel as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    });
    linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_text_contrasts_dark_and_light_palettes() {
        assert!(luminance([245, 249, 250]) > luminance([4, 8, 12]));
        assert_eq!(mix([0, 0, 0], [255, 255, 255], 0.5), [128; 3]);
    }
}
