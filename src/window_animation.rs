//! Window lifecycle and layout-motion animation state (render roadmap R2).
//!
//! Logical placement, focus, and input change immediately. These objects
//! retain only the short-lived visual offset/opacity needed to interpolate
//! toward that state. Movement is interruption-safe: a new target starts
//! from the current on-screen offset rather than snapping back to the prior
//! logical target. Closing retains clones of the surface tree's last
//! already-imported GPU texture handles until its fade completes, avoiding
//! a new framebuffer or texture copy per window.

use std::time::{Duration, Instant};

use smithay::{
    backend::renderer::{
        element::{
            surface::{WaylandSurfaceRenderElement, WaylandSurfaceTexture},
            Element, Id, Kind, RenderElement,
        },
        gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture},
        utils::CommitCounter,
        Color32F,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Transform},
};

use crate::{
    animation::ease_progress,
    config::{
        WindowAnimationConfig, WindowAnimationCurve, WindowAnimationEffect, WindowAnimationOrigin,
    },
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisualSample {
    pub offset: Point<f64, Logical>,
    pub opacity: f32,
}

impl Default for VisualSample {
    fn default() -> Self {
        Self {
            offset: Point::from((0.0, 0.0)),
            opacity: 1.0,
        }
    }
}

pub struct WindowVisualAnimation {
    start: Instant,
    duration: Duration,
    curve: WindowAnimationCurve,
    opacity_duration: Duration,
    opacity_curve: WindowAnimationCurve,
    from_offset: Point<f64, Logical>,
    to_offset: Point<f64, Logical>,
    from_opacity: f32,
    to_opacity: f32,
    effect: WindowAnimationEffect,
    wave_amplitude: f32,
    wave_cycles: f32,
    wave_decay: f32,
}

impl WindowVisualAnimation {
    fn new(
        config: &WindowAnimationConfig,
        slowdown: f32,
        from_offset: Point<f64, Logical>,
        to_offset: Point<f64, Logical>,
    ) -> Self {
        let scaled_duration = |duration_ms: u32| {
            (duration_ms as f32 * slowdown)
                .round()
                .clamp(1.0, 100_000.0) as u64
        };
        Self {
            start: Instant::now(),
            duration: Duration::from_millis(scaled_duration(config.duration_ms)),
            curve: config.curve,
            opacity_duration: Duration::from_millis(scaled_duration(
                config.opacity_duration_ms.unwrap_or(config.duration_ms),
            )),
            opacity_curve: config.opacity_curve.unwrap_or(config.curve),
            from_offset,
            to_offset,
            from_opacity: config.from_opacity,
            to_opacity: config.to_opacity,
            effect: config.effect,
            wave_amplitude: config.wave_amplitude,
            wave_cycles: config.wave_cycles,
            wave_decay: config.wave_decay,
        }
    }

    pub fn open(
        config: &WindowAnimationConfig,
        slowdown: f32,
        offset: Point<f64, Logical>,
    ) -> Self {
        Self::new(config, slowdown, offset, Point::from((0.0, 0.0)))
    }

    pub fn close(
        config: &WindowAnimationConfig,
        slowdown: f32,
        offset: Point<f64, Logical>,
    ) -> Self {
        Self::new(config, slowdown, Point::from((0.0, 0.0)), offset)
    }

    pub fn movement(
        config: &WindowAnimationConfig,
        slowdown: f32,
        from_offset: Point<f64, Logical>,
    ) -> Self {
        Self::new(config, slowdown, from_offset, Point::from((0.0, 0.0)))
    }

    fn progress_for(&self, duration: Duration) -> f32 {
        if duration.is_zero() {
            1.0
        } else {
            (self.start.elapsed().as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
        }
    }

    pub fn sample(&self) -> VisualSample {
        let raw_progress = self.progress_for(self.duration);
        let progress = ease_progress(self.curve, raw_progress);
        let opacity_progress =
            ease_progress(self.opacity_curve, self.progress_for(self.opacity_duration));
        let base_x = self.from_offset.x + (self.to_offset.x - self.from_offset.x) * progress as f64;
        let base_y = self.from_offset.y + (self.to_offset.y - self.from_offset.y) * progress as f64;
        let delta_x = self.to_offset.x - self.from_offset.x;
        let delta_y = self.to_offset.y - self.from_offset.y;
        let length = delta_x.hypot(delta_y);
        let (normal_x, normal_y) = if length > f64::EPSILON {
            (-delta_y / length, delta_x / length)
        } else {
            (1.0, 0.0)
        };
        let envelope = (1.0 - raw_progress).powf(self.wave_decay);
        let wave = match self.effect {
            WindowAnimationEffect::Glide => 0.0,
            WindowAnimationEffect::Tide => {
                (std::f32::consts::PI * raw_progress).sin() * self.wave_amplitude * envelope
            }
            WindowAnimationEffect::Wave => {
                (std::f32::consts::TAU * self.wave_cycles * raw_progress).sin()
                    * self.wave_amplitude
                    * envelope
            }
        } as f64;
        VisualSample {
            offset: Point::from((base_x + normal_x * wave, base_y + normal_y * wave)),
            // An overshooting Bézier is useful for geometry but alpha above
            // one causes driver-dependent bright flashes. Keep opacity
            // physically valid while leaving the spatial overshoot intact.
            opacity: (self.from_opacity + (self.to_opacity - self.from_opacity) * opacity_progress)
                .clamp(0.0, 1.0),
        }
    }

    pub fn finished(&self) -> bool {
        self.start.elapsed() >= self.duration.max(self.opacity_duration)
    }
}

/// Resolves a lifecycle slide path. `NearestEdge` deliberately mirrors
/// Hyprland's unforced `slide` implementation, including its tie order
/// (bottom, left, right, then top) and its width-sized horizontal travel.
pub fn lifecycle_offset(
    config: &WindowAnimationConfig,
    window: Rectangle<i32, Logical>,
    output: Rectangle<i32, Logical>,
) -> Point<f64, Logical> {
    use WindowAnimationOrigin::*;

    let origin = match config.origin {
        NearestEdge => {
            let midpoint_x = window.loc.x as f64 + window.size.w as f64 / 2.0;
            let midpoint_y = window.loc.y as f64 + window.size.h as f64 / 2.0;
            let distances = [
                midpoint_y - output.loc.y as f64,
                (output.loc.x + output.size.w) as f64 - midpoint_x,
                (output.loc.y + output.size.h) as f64 - midpoint_y,
                midpoint_x - output.loc.x as f64,
            ];
            let minimum = distances.into_iter().fold(f64::INFINITY, f64::min);
            if minimum == distances[2] {
                Bottom
            } else if minimum == distances[3] {
                Left
            } else if minimum == distances[1] {
                Right
            } else {
                Top
            }
        }
        origin => origin,
    };

    match origin {
        Offset => Point::from((config.offset.0 as f64, config.offset.1 as f64)),
        Top => Point::from((0.0, (output.loc.y - window.size.h - window.loc.y) as f64)),
        Right => Point::from((window.size.w as f64, 0.0)),
        Bottom => Point::from((0.0, (output.loc.y + output.size.h - window.loc.y) as f64)),
        Left => Point::from((-(window.size.w as f64), 0.0)),
        NearestEdge => unreachable!("nearest edge resolves above"),
    }
}

#[derive(Clone)]
enum SnapshotContent {
    Texture(GlesTexture),
    Solid(Color32F),
}

/// One already-imported surface texture retained from the last frame where
/// the window was mapped. Cloning `GlesTexture` retains the existing GPU
/// resource; it does not allocate or render a second framebuffer.
#[derive(Clone)]
struct SnapshotPart {
    id: Id,
    relative_geometry: Rectangle<i32, Physical>,
    src: Rectangle<f64, Buffer>,
    transform: Transform,
    alpha: f32,
    kind: Kind,
    content: SnapshotContent,
}

/// Last drawable surface tree for one mapped window. This is refreshed from
/// the normal frame walk and transferred into `ClosingWindowAnimation` on a
/// null-buffer commit, so close rendering survives after Smithay correctly
/// releases the live surface state.
pub struct WindowFrameSnapshot {
    pub output: String,
    anchor: Point<i32, Physical>,
    parts: Vec<SnapshotPart>,
}

impl WindowFrameSnapshot {
    pub fn capture(
        output: String,
        anchor: Point<i32, Physical>,
        scale: Scale<f64>,
        elements: &[WaylandSurfaceRenderElement<GlesRenderer>],
    ) -> Option<Self> {
        let parts: Vec<_> = elements
            .iter()
            .map(|element| {
                let mut geometry = element.geometry(scale);
                geometry.loc -= anchor;
                let content = match element.texture() {
                    WaylandSurfaceTexture::Texture(texture) => {
                        SnapshotContent::Texture(texture.clone())
                    }
                    WaylandSurfaceTexture::SolidColor(color) => SnapshotContent::Solid(*color),
                };
                SnapshotPart {
                    id: element.id().clone().namespaced(0x434c_4f53),
                    relative_geometry: geometry,
                    src: element.src(),
                    transform: element.transform(),
                    alpha: element.alpha(),
                    kind: element.kind(),
                    content,
                }
            })
            .collect();
        (!parts.is_empty()).then_some(Self {
            output,
            anchor,
            parts,
        })
    }

    fn elements(
        &self,
        scale: Scale<f64>,
        sample: VisualSample,
        commit: CommitCounter,
    ) -> Vec<WindowSnapshotElement> {
        let offset = Point::<f64, Logical>::from((sample.offset.x, sample.offset.y))
            .to_physical_precise_round(scale);
        self.parts
            .iter()
            .cloned()
            .map(|part| WindowSnapshotElement {
                geometry: Rectangle::new(
                    self.anchor + offset + part.relative_geometry.loc,
                    part.relative_geometry.size,
                ),
                id: part.id,
                commit,
                src: part.src,
                transform: part.transform,
                alpha: part.alpha * sample.opacity,
                kind: part.kind,
                content: part.content,
            })
            .collect()
    }
}

pub struct WindowSnapshotElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<i32, Physical>,
    src: Rectangle<f64, Buffer>,
    transform: Transform,
    alpha: f32,
    kind: Kind,
    content: SnapshotContent,
}

impl Element for WindowSnapshotElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.src
    }

    fn transform(&self) -> Transform {
        self.transform
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl RenderElement<GlesRenderer> for WindowSnapshotElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), GlesError> {
        match &self.content {
            SnapshotContent::Texture(texture) => frame.render_texture_from_to(
                texture,
                src,
                dst,
                damage,
                opaque_regions,
                self.transform,
                self.alpha,
                None,
                &[],
            ),
            SnapshotContent::Solid(color) => frame.draw_solid(dst, damage, *color * self.alpha),
        }
    }
}

/// A detached window with a bounded last-frame GPU snapshot. State is
/// pruned through `Smallvil::has_active_animation`.
pub struct ClosingWindowAnimation {
    pub surface: WlSurface,
    pub snapshot: WindowFrameSnapshot,
    pub animation: WindowVisualAnimation,
    commit: CommitCounter,
}

impl ClosingWindowAnimation {
    pub fn new(
        surface: WlSurface,
        snapshot: WindowFrameSnapshot,
        animation: WindowVisualAnimation,
    ) -> Self {
        Self {
            surface,
            snapshot,
            animation,
            commit: CommitCounter::default(),
        }
    }

    pub fn frame_elements(&mut self, scale: Scale<f64>) -> Vec<WindowSnapshotElement> {
        self.commit.increment();
        self.snapshot
            .elements(scale, self.animation.sample(), self.commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_config() -> WindowAnimationConfig {
        WindowAnimationConfig {
            enabled: true,
            duration_ms: 1000,
            curve: WindowAnimationCurve::Linear,
            opacity_duration_ms: None,
            opacity_curve: None,
            offset: (20, 40),
            from_opacity: 0.0,
            to_opacity: 1.0,
            origin: WindowAnimationOrigin::Offset,
            effect: WindowAnimationEffect::Glide,
            wave_amplitude: 0.0,
            wave_cycles: 1.0,
            wave_decay: 1.5,
        }
    }

    #[test]
    fn open_starts_at_configured_offset_and_opacity() {
        let animation =
            WindowVisualAnimation::open(&linear_config(), 1.0, Point::from((20.0, 40.0)));
        let sample = animation.sample();
        assert!((sample.offset.x - 20.0).abs() < 1.0);
        assert!((sample.offset.y - 40.0).abs() < 1.0);
        assert!(sample.opacity < 0.01);
    }

    #[test]
    fn movement_uses_real_interruption_offset() {
        let animation =
            WindowVisualAnimation::movement(&linear_config(), 1.0, Point::from((-120.0, 35.0)));
        let sample = animation.sample();
        assert!((sample.offset.x + 120.0).abs() < 1.0);
        assert!((sample.offset.y - 35.0).abs() < 1.0);
    }

    #[test]
    fn overshooting_geometry_never_pushes_opacity_out_of_range() {
        let mut config = linear_config();
        config.curve = WindowAnimationCurve::CubicBezier([0.05, 0.9, 0.1, 1.1]);
        let mut animation = WindowVisualAnimation::open(&config, 1.0, Point::from((20.0, 40.0)));
        animation.start = Instant::now() - Duration::from_millis(750);
        assert!((0.0..=1.0).contains(&animation.sample().opacity));
    }

    #[test]
    fn tide_trajectory_returns_to_the_exact_target() {
        let mut config = linear_config();
        config.effect = WindowAnimationEffect::Tide;
        config.wave_amplitude = 20.0;
        let mut animation = WindowVisualAnimation::open(&config, 1.0, Point::from((20.0, 40.0)));
        animation.start = Instant::now() - animation.duration;
        let sample = animation.sample();
        assert!(sample.offset.x.abs() < 0.001);
        assert!(sample.offset.y.abs() < 0.001);
    }

    #[test]
    fn nearest_edge_matches_hyprland_slide_geometry() {
        let mut config = linear_config();
        config.origin = WindowAnimationOrigin::NearestEdge;
        let output = Rectangle::new((0, 0).into(), (1000, 800).into());

        let left = lifecycle_offset(
            &config,
            Rectangle::new((50, 250).into(), (300, 300).into()),
            output,
        );
        assert_eq!(left, Point::from((-300.0, 0.0)));

        let bottom = lifecycle_offset(
            &config,
            Rectangle::new((350, 600).into(), (300, 150).into()),
            output,
        );
        assert_eq!(bottom, Point::from((0.0, 200.0)));
    }
}
