//! Pointer glyphs for the udev backend. `Theme` loads the system's real
//! xcursor theme (`XCURSOR_THEME`/`XCURSOR_SIZE`, same env vars every
//! xcursor-aware client honors) so `CursorImageStatus::Named` renders the
//! actual system pointer instead of a placeholder. `fallback_glyph_element`
//! is the last-resort dot below that, CPU-composited the same way
//! `toast.rs` does its pill, used only when no theme could be loaded at
//! all -- the point of that one isn't to look right, it's so the pointer is
//! never fully invisible on real display hardware.
//!
//! Not used by the winit backend: there, the host compositor's own OS
//! cursor is what's actually on screen (TideWM's nested window is just
//! client content to it), so drawing one here would just overlay a second,
//! redundant cursor.

use std::collections::HashMap;
use std::time::Duration;

use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    input::pointer::CursorIcon,
    utils::{Physical, Point, Transform},
};
use xcursor::{
    parser::{parse_xcursor, Image},
    CursorTheme,
};

const SIZE: i32 = 14;
const RADIUS: f32 = 5.0;

/// Rebuilt fresh on every call rather than cached: the glyph is a handful
/// of pixels, and this only runs when the fallback cursor is actually
/// being rendered (not every frame -- see `backend/udev.rs`).
pub fn fallback_glyph_element(
    renderer: &mut GlesRenderer,
    location: (f64, f64),
) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
    let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = SIZE as f32 / 2.0;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let dist = ((fx - center).powi(2) + (fy - center).powi(2)).sqrt();
            let coverage = (RADIUS - dist + 0.5).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let i = ((y * SIZE + x) * 4) as usize;
            let a = (255.0 * coverage) as u8;
            // Fourcc::Argb8888 in memory (little-endian) is B, G, R, A.
            pixels[i] = 255;
            pixels[i + 1] = 255;
            pixels[i + 2] = 255;
            pixels[i + 3] = a;
        }
    }

    let buffer = MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (SIZE, SIZE),
        1,
        Transform::Normal,
        None,
    );

    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        location,
        &buffer,
        None,
        None,
        None,
        Kind::Unspecified,
    )
    .ok()
}

/// A loaded xcursor theme. `None` from `load()` means no theme was found
/// (or the "default" cursor is missing from it) -- not a hard error, just
/// the exact case `fallback_glyph_element` above exists to cover.
///
/// Per-icon frames are loaded lazily and cached by canonical name (`None`
/// cached too, for an icon this theme genuinely doesn't ship), rather than
/// eagerly loading every `CursorIcon` variant at startup -- a themed
/// desktop only ever touches a handful of shapes in a session, and a real
/// xcursor theme can ship dozens of animated, multi-size icons each.
pub struct Theme {
    theme: CursorTheme,
    size: u32,
    cache: HashMap<&'static str, Option<Vec<Image>>>,
}

impl Theme {
    pub fn load() -> Option<Theme> {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);

        let mut theme = Theme {
            theme: CursorTheme::load(&name),
            size,
            cache: HashMap::new(),
        };
        // Confirms the theme is actually usable at all before returning
        // Some -- matches the previous behavior of only ever loading
        // "default" and treating its absence as "no theme".
        theme.frames(CursorIcon::Default)?;
        Some(theme)
    }

    /// Loads and caches `icon`'s frames, falling back through its legacy
    /// alternate xcursor names (`CursorIcon::alt_names()`) for themes that
    /// don't ship the modern CSS name -- most don't, XDG cursor themes are
    /// still overwhelmingly named the old X11 way (`hand2`, `xterm`, ...).
    fn frames(&mut self, icon: CursorIcon) -> Option<&[Image]> {
        if !self.cache.contains_key(icon.name()) {
            let loaded = std::iter::once(icon.name())
                .chain(icon.alt_names().iter().copied())
                .find_map(|name| {
                    let path = self.theme.load_icon(name)?;
                    let data = std::fs::read(path).ok()?;
                    parse_xcursor(&data)
                });
            self.cache.insert(icon.name(), loaded);
        }
        self.cache.get(icon.name())?.as_deref()
    }

    /// `local` is the pointer's output-local physical position (hotspot not
    /// yet subtracted -- the chosen frame's own `xhot`/`yhot` is what gets
    /// subtracted here, since it's specific to that frame's bitmap).
    pub fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        local: Point<f64, Physical>,
        scale: u32,
        time: Duration,
        icon: CursorIcon,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let size = self.size;
        // Falls back to the theme's default arrow if this specific shape
        // isn't shipped -- a wrong-but-present pointer beats an invisible
        // one, same reasoning `fallback_glyph_element` exists for below.
        // Resolved in two steps (rather than `Option::or` chaining two
        // `self.frames(...)` calls) since the second call would otherwise
        // need to borrow `self` again while the first call's result is
        // still held.
        let icon = if self.frames(icon).is_some() {
            icon
        } else {
            CursorIcon::Default
        };
        let frames = self.frames(icon)?;
        let frame = pick_frame(frames, size, scale, time);

        // Fourcc::Argb8888 is B,G,R,A per pixel in memory (little-endian;
        // confirmed against Smithay's own fourcc_to_gl_formats, which maps
        // it to GL_BGRA_EXT). xcursor's `pixels_rgba` is R,G,B,A per pixel
        // -- the two don't match, so swap R/B rather than assume they do.
        let mut bgra = frame.pixels_rgba.clone();
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }

        let buffer = MemoryRenderBuffer::from_slice(
            &bgra,
            Fourcc::Argb8888,
            (frame.width as i32, frame.height as i32),
            1,
            Transform::Normal,
            None,
        );

        let location = (local.x - frame.xhot as f64, local.y - frame.yhot as f64);
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }
}

/// Picks the animation frame for `time`, nearest to `size * scale` the same
/// way libXcursor resolves a nominal size to the closest bitmap the theme
/// actually ships. A free function rather than a `Theme` method: it only
/// needs one icon's already-resolved frame list, not the cache lookup that
/// produced it, and keeping it that way avoids a self-borrow that would
/// otherwise have to span the whole `render_element` body above.
fn pick_frame(frames: &[Image], size: u32, scale: u32, time: Duration) -> &Image {
    let target = size * scale;
    let nearest = frames
        .iter()
        .min_by_key(|i| (target as i32 - i.size as i32).abs())
        .expect("frames is never empty: only Some(frames) from parse_xcursor is cached");
    let same_size: Vec<&Image> = frames
        .iter()
        .filter(|i| i.width == nearest.width && i.height == nearest.height)
        .collect();

    let total: u32 = same_size.iter().map(|i| i.delay.max(1)).sum();
    let mut millis = (time.as_millis() as u32) % total;
    for icon in &same_size {
        let delay = icon.delay.max(1);
        if millis < delay {
            return icon;
        }
        millis -= delay;
    }
    same_size[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon(size: u32, width: u32, delay: u32) -> Image {
        Image {
            size,
            width,
            height: width,
            xhot: 0,
            yhot: 0,
            delay,
            pixels_rgba: vec![0; (width * width * 4) as usize],
            pixels_argb: vec![],
        }
    }

    #[test]
    fn frame_picks_nearest_size_and_animates_with_wraparound() {
        let frames = vec![
            icon(48, 48, 1),   // wrong size group, never picked at scale 1
            icon(24, 24, 100), // frame 0: [0, 100)
            icon(24, 24, 200), // frame 1: [100, 300)
        ];

        assert_eq!(
            pick_frame(&frames, 24, 1, Duration::from_millis(0)).delay,
            100
        );
        assert_eq!(
            pick_frame(&frames, 24, 1, Duration::from_millis(50)).delay,
            100
        );
        assert_eq!(
            pick_frame(&frames, 24, 1, Duration::from_millis(150)).delay,
            200
        );
        // Wraps past the 300ms total back to frame 0.
        assert_eq!(
            pick_frame(&frames, 24, 1, Duration::from_millis(350)).delay,
            100
        );
        // Nearest to size*scale = 48 picks the other size group entirely.
        assert_eq!(
            pick_frame(&frames, 24, 2, Duration::from_millis(0)).width,
            48
        );
    }
}
