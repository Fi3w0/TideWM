//! Themed pointer glyphs for the udev backend, loaded from standard xcursor
//! environment settings with a process-stable fallback dot. Winit relies on
//! the host compositor's cursor and does not render these elements.

use std::{collections::HashMap, sync::OnceLock, time::Duration};

use smithay::{
    backend::allocator::Fourcc,
    backend::renderer::element::{
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        Kind,
    },
    backend::renderer::gles::GlesRenderer,
    input::pointer::CursorIcon,
    output::Scale as OutputScale,
    utils::{Physical, Point, Transform},
};
use xcursor::{
    parser::{parse_xcursor, Image},
    CursorTheme,
};

const SIZE: i32 = 14;
const RADIUS: f32 = 5.0;

/// Process-stable fallback buffer, retaining lazy imports per renderer context.
fn fallback_glyph_buffer() -> &'static MemoryRenderBuffer {
    static BUFFER: OnceLock<MemoryRenderBuffer> = OnceLock::new();

    BUFFER.get_or_init(|| {
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

        MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (SIZE, SIZE),
            1,
            Transform::Normal,
            None,
        )
    })
}

pub fn fallback_glyph_element(
    renderer: &mut GlesRenderer,
    location: (f64, f64),
) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        location,
        fallback_glyph_buffer(),
        None,
        None,
        None,
        Kind::Unspecified,
    )
    .ok()
}

/// Lazily loaded xcursor theme. Missing icon results are cached by canonical
/// name; prepared buffers are cached separately by live integer asset scale.
pub struct Theme {
    theme: CursorTheme,
    size: u32,
    cache: HashMap<&'static str, Option<Vec<Image>>>,
    prepared: PreparedCache,
}

struct PreparedFrame {
    buffer: MemoryRenderBuffer,
    xhot: u32,
    yhot: u32,
    delay: u32,
}

struct PreparedCursor {
    asset_scale: i32,
    frames: Vec<PreparedFrame>,
    total_delay: u128,
}

impl PreparedCursor {
    fn new(frames: &[Image], size: u32, asset_scale: i32) -> Option<Self> {
        let scale = u64::try_from(asset_scale).ok().filter(|scale| *scale > 0)?;
        let target = u64::from(size) * scale;
        let nearest = frames
            .iter()
            .min_by_key(|image| target.abs_diff(u64::from(image.size)))?;

        let frames: Vec<PreparedFrame> = frames
            .iter()
            .filter(|image| image.width == nearest.width && image.height == nearest.height)
            .filter_map(|image| {
                let width = i32::try_from(image.width).ok()?;
                let height = i32::try_from(image.height).ok()?;
                // Fourcc::Argb8888 is B,G,R,A per pixel in memory
                // (little-endian); xcursor gives R,G,B,A, so convert once
                // before the stable buffer enters the renderer cache.
                let mut bgra = image.pixels_rgba.clone();
                for pixel in bgra.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                Some(PreparedFrame {
                    buffer: MemoryRenderBuffer::from_slice(
                        &bgra,
                        Fourcc::Argb8888,
                        (width, height),
                        asset_scale,
                        Transform::Normal,
                        None,
                    ),
                    xhot: image.xhot,
                    yhot: image.yhot,
                    delay: image.delay.max(1),
                })
            })
            .collect();
        if frames.is_empty() {
            return None;
        }
        let total_delay = frames.iter().map(|frame| u128::from(frame.delay)).sum();
        Some(Self {
            asset_scale,
            frames,
            total_delay,
        })
    }

    fn frame(&self, time: Duration) -> &PreparedFrame {
        let mut millis = time.as_millis() % self.total_delay;
        for frame in &self.frames {
            let delay = u128::from(frame.delay);
            if millis < delay {
                return frame;
            }
            millis -= delay;
        }
        &self.frames[0]
    }
}

#[derive(Default)]
struct PreparedCache(HashMap<(&'static str, i32), PreparedCursor>);

impl PreparedCache {
    #[cfg(test)]
    fn get_or_insert_with(
        &mut self,
        key: (&'static str, i32),
        prepare: impl FnOnce() -> Option<PreparedCursor>,
    ) -> Option<&PreparedCursor> {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.0.entry(key) {
            entry.insert(prepare()?);
        }
        self.0.get(&key)
    }

    fn retain_scales(&mut self, mut is_live: impl FnMut(i32) -> bool) {
        self.0.retain(|(_, scale), _| is_live(*scale));
    }
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
            prepared: PreparedCache::default(),
        };
        // A usable theme must provide its default cursor.
        theme.frames(CursorIcon::Default)?;
        Some(theme)
    }

    /// Loads and caches frames, trying legacy xcursor aliases after the CSS name.
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

    /// Releases prepared frames for scales no live output uses. Parsed theme
    /// images stay lazy-cached per icon; the heavier BGRA buffers and their
    /// per-renderer textures are bounded by the live output-scale set.
    pub fn retain_scales(&mut self, is_live: impl FnMut(i32) -> bool) {
        self.prepared.retain_scales(is_live);
    }

    /// `local` is the pointer's output-local physical position (hotspot not
    /// yet subtracted -- the chosen frame's own `xhot`/`yhot` is what gets
    /// subtracted here, since it's specific to that frame's bitmap).
    pub fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        local: Point<f64, Physical>,
        output_scale: OutputScale,
        time: Duration,
        icon: CursorIcon,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let fractional_scale = output_scale.fractional_scale();
        if !fractional_scale.is_finite() || fractional_scale <= 0.0 {
            return None;
        }
        let asset_scale = output_scale.integer_scale();
        if asset_scale <= 0 {
            return None;
        }
        // Resolve in two steps to release the first mutable borrow before
        // loading the visible fallback cursor.
        let icon = if self.frames(icon).is_some() {
            icon
        } else {
            CursorIcon::Default
        };
        let key = (icon.name(), asset_scale);
        if !self.prepared.0.contains_key(&key) {
            let size = self.size;
            let prepared = {
                let frames = self.frames(icon)?;
                PreparedCursor::new(frames, size, asset_scale)?
            };
            self.prepared.0.insert(key, prepared);
        }
        let cursor = self.prepared.0.get(&key)?;
        let frame = cursor.frame(time);
        // The selected xcursor bitmap is tagged with its integer asset scale,
        // while pointer placement stays at the output's precise fractional
        // scale. Convert the bitmap-pixel hotspot through logical space before
        // subtracting it from the already-physical pointer coordinate.
        let hotspot_scale = fractional_scale / f64::from(cursor.asset_scale);
        let location = (
            local.x - f64::from(frame.xhot) * hotspot_scale,
            local.y - f64::from(frame.yhot) * hotspot_scale,
        );
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &frame.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }
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

        let cursor = PreparedCursor::new(&frames, 24, 1).unwrap();
        assert_eq!(cursor.frame(Duration::from_millis(0)).delay, 100);
        assert_eq!(cursor.frame(Duration::from_millis(50)).delay, 100);
        assert_eq!(cursor.frame(Duration::from_millis(150)).delay, 200);
        // Wraps past the 300ms total back to frame 0.
        assert_eq!(cursor.frame(Duration::from_millis(350)).delay, 100);
        // Nearest to size*scale = 48 picks the other size group entirely.
        let scaled = PreparedCursor::new(&frames, 24, 2).unwrap();
        assert_eq!(scaled.frames.len(), 1);
        assert_eq!(scaled.asset_scale, 2);
    }

    #[test]
    fn fractional_output_uses_integer_asset_scale_and_fractional_hotspot() {
        let scale = OutputScale::Fractional(1.5);
        assert_eq!(scale.integer_scale(), 2);
        let mut image = icon(48, 48, 1);
        image.xhot = 8;
        image.yhot = 4;
        let cursor = PreparedCursor::new(&[image], 24, scale.integer_scale()).unwrap();
        let frame = cursor.frame(Duration::ZERO);
        let hotspot_scale = scale.fractional_scale() / f64::from(cursor.asset_scale);

        assert_eq!(f64::from(frame.xhot) * hotspot_scale, 6.0);
        assert_eq!(f64::from(frame.yhot) * hotspot_scale, 3.0);
        assert_eq!(OutputScale::Fractional(1.25).integer_scale(), 2);
        assert_eq!(OutputScale::Fractional(2.0).integer_scale(), 2);
        assert_eq!(
            OutputScale::Custom {
                advertised_integer: 3,
                fractional: 1.5,
            }
            .integer_scale(),
            3
        );
    }

    #[test]
    fn prepared_cache_reuses_entries_and_prunes_non_live_scales() {
        let frames = [icon(24, 24, 1), icon(48, 48, 1)];
        let mut cache = PreparedCache::default();
        let mut builds = 0;
        let key = (CursorIcon::Default.name(), 2);
        let first = cache
            .get_or_insert_with(key, || {
                builds += 1;
                PreparedCursor::new(&frames, 24, 2)
            })
            .unwrap() as *const PreparedCursor;
        let second = cache
            .get_or_insert_with(key, || {
                builds += 1;
                PreparedCursor::new(&frames, 24, 2)
            })
            .unwrap() as *const PreparedCursor;

        assert_eq!(first, second);
        assert_eq!(builds, 1);
        cache
            .get_or_insert_with((CursorIcon::Default.name(), 1), || {
                PreparedCursor::new(&frames, 24, 1)
            })
            .unwrap();
        cache.retain_scales(|scale| scale == 1);
        assert_eq!(cache.0.len(), 1);
        assert!(cache.0.contains_key(&(CursorIcon::Default.name(), 1)));
    }

    #[test]
    fn fallback_buffer_is_stable() {
        assert!(std::ptr::eq(
            fallback_glyph_buffer(),
            fallback_glyph_buffer()
        ));
    }
}
