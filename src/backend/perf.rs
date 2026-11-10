//! Bounded native render counters. Durations measure CPU-side work and waits,
//! not GPU execution time. Each live output owns one aggregate, with no logs
//! or allocation per sample.

use std::time::Duration;

use serde::Serialize;
use smithay::{
    backend::renderer::element::{Element, Id},
    utils::{Physical, Rectangle, Scale},
};

/// Why a fullscreen window's surface could not be handed to the display as
/// it is. Mirrors the conditions Smithay's `DrmCompositor` applies before it
/// tries the primary plane; computed only for frames with a fullscreen window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanoutBlocker {
    /// Nothing TideWM draws stands in the way; the rest is the driver's test.
    Clear,
    /// Another element is drawn above the window on this output.
    ElementAbove,
    /// The window's buffer has transparency and no opaque region covering it.
    NotOpaque,
    /// The window's surface does not cover the whole output.
    NotCovering,
    /// The fullscreen window has no surface element in this frame.
    Missing,
}

pub(crate) fn scanout_blocker<E: Element>(
    elements: &[E],
    target: &Id,
    output: Rectangle<i32, Physical>,
    scale: Scale<f64>,
) -> ScanoutBlocker {
    let mut above = false;
    for element in elements {
        let geometry = element.geometry(scale);
        if geometry
            .intersection(output)
            .is_none_or(|area| area.is_empty())
        {
            continue;
        }
        if element.id() != target {
            above = true;
            continue;
        }
        if above {
            return ScanoutBlocker::ElementAbove;
        }
        if !geometry.contains_rect(output) {
            return ScanoutBlocker::NotCovering;
        }
        let opaque = element.opaque_regions(scale);
        let uncovered = Rectangle::subtract_rects_many_in_place(
            vec![Rectangle::from_size(geometry.size)],
            opaque.iter().copied(),
        );
        return if uncovered.is_empty() {
            ScanoutBlocker::Clear
        } else {
            ScanoutBlocker::NotOpaque
        };
    }
    ScanoutBlocker::Missing
}

#[derive(Default, Serialize)]
pub(crate) struct StageTime {
    samples: u64,
    total_ns: u64,
    max_ns: u64,
}

impl StageTime {
    fn record(&mut self, duration: Duration) {
        let ns = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.samples = self.samples.saturating_add(1);
        self.total_ns = self.total_ns.saturating_add(ns);
        self.max_ns = self.max_ns.max(ns);
    }
}

#[derive(Default)]
pub(crate) struct RenderSample {
    pub scene: Duration,
    pub backdrops: Duration,
    pub render: Duration,
    pub queue: Option<Duration>,
    pub fence_wait: Option<Duration>,
    pub total: Duration,
    pub primary_scanout: bool,
    pub cursor_plane: bool,
    pub needs_sync: bool,
    pub fence_present: bool,
    pub fence_exportable: bool,
    pub submitted: bool,
    pub empty: bool,
    pub failed: bool,
    /// Set on frames whose output has a fullscreen window.
    pub fullscreen: Option<ScanoutBlocker>,
    pub scanout_format_unsupported: bool,
    pub scanout_test_failed: bool,
}

#[derive(Default, Serialize)]
pub(crate) struct OutputRenderStats {
    frames: u64,
    submitted: u64,
    empty: u64,
    failed: u64,
    primary_scanout: u64,
    cursor_plane: u64,
    needs_sync: u64,
    fence_present: u64,
    fence_exportable: u64,
    /// Frames with a fullscreen window, and what kept each off the primary
    /// plane. `fullscreen_clear` frames that still were not scanned out were
    /// refused by the format check or the driver's plane test below.
    fullscreen_frames: u64,
    fullscreen_clear: u64,
    fullscreen_element_above: u64,
    fullscreen_not_opaque: u64,
    fullscreen_not_covering: u64,
    fullscreen_missing: u64,
    scanout_format_unsupported: u64,
    scanout_test_failed: u64,
    backdrop_rendered: u64,
    backdrop_skipped: u64,
    shader_chain_updates: u64,
    scene: StageTime,
    backdrops: StageTime,
    render: StageTime,
    queue: StageTime,
    fence_wait: StageTime,
    total: StageTime,
}

impl OutputRenderStats {
    pub fn record_backdrops(&mut self, rendered: usize, skipped: usize) {
        self.backdrop_rendered = self.backdrop_rendered.saturating_add(rendered as u64);
        self.backdrop_skipped = self.backdrop_skipped.saturating_add(skipped as u64);
    }

    pub fn record_shader_updates(&mut self, updates: u64) {
        self.shader_chain_updates = self.shader_chain_updates.saturating_add(updates);
    }

    pub fn record(&mut self, sample: RenderSample) {
        self.frames = self.frames.saturating_add(1);
        for (counter, occurred) in [
            (&mut self.submitted, sample.submitted),
            (&mut self.empty, sample.empty),
            (&mut self.failed, sample.failed),
            (&mut self.primary_scanout, sample.primary_scanout),
            (&mut self.cursor_plane, sample.cursor_plane),
            (&mut self.needs_sync, sample.needs_sync),
            (&mut self.fence_present, sample.fence_present),
            (&mut self.fence_exportable, sample.fence_exportable),
        ] {
            *counter = counter.saturating_add(u64::from(occurred));
        }
        if let Some(blocker) = sample.fullscreen {
            self.fullscreen_frames = self.fullscreen_frames.saturating_add(1);
            let counter = match blocker {
                ScanoutBlocker::Clear => &mut self.fullscreen_clear,
                ScanoutBlocker::ElementAbove => &mut self.fullscreen_element_above,
                ScanoutBlocker::NotOpaque => &mut self.fullscreen_not_opaque,
                ScanoutBlocker::NotCovering => &mut self.fullscreen_not_covering,
                ScanoutBlocker::Missing => &mut self.fullscreen_missing,
            };
            *counter = counter.saturating_add(1);
        }
        self.scanout_format_unsupported = self
            .scanout_format_unsupported
            .saturating_add(u64::from(sample.scanout_format_unsupported));
        self.scanout_test_failed = self
            .scanout_test_failed
            .saturating_add(u64::from(sample.scanout_test_failed));
        self.scene.record(sample.scene);
        self.backdrops.record(sample.backdrops);
        self.render.record(sample.render);
        self.total.record(sample.total);
        if let Some(duration) = sample.queue {
            self.queue.record(duration);
        }
        if let Some(duration) = sample.fence_wait {
            self.fence_wait.record(duration);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanout_blocker_names_what_is_in_the_way() {
        use smithay::backend::renderer::{
            element::{solid::SolidColorRenderElement, Kind},
            utils::CommitCounter,
        };
        let output = Rectangle::from_size((1920, 1080).into());
        let solid = |size: (i32, i32), alpha: f32| {
            SolidColorRenderElement::new(
                Id::new(),
                Rectangle::from_size(size.into()),
                CommitCounter::default(),
                [0.0, 0.0, 0.0, alpha],
                Kind::Unspecified,
            )
        };
        let scale = Scale::from(1.0);
        let game = solid((1920, 1080), 1.0);
        let wallpaper = solid((1920, 1080), 1.0);
        let target = game.id().clone();
        let check = |elements: &[SolidColorRenderElement], target: &Id| {
            scanout_blocker(elements, target, output, scale)
        };
        // Whatever sits below an opaque full-output window is not a blocker.
        assert_eq!(
            check(&[game.clone(), wallpaper.clone()], &target),
            ScanoutBlocker::Clear
        );
        let toast = solid((300, 80), 1.0);
        assert_eq!(
            check(&[toast, game.clone(), wallpaper.clone()], &target),
            ScanoutBlocker::ElementAbove
        );
        let translucent = solid((1920, 1080), 0.5);
        assert_eq!(
            check(std::slice::from_ref(&translucent), translucent.id()),
            ScanoutBlocker::NotOpaque
        );
        let small = solid((1280, 720), 1.0);
        assert_eq!(
            check(std::slice::from_ref(&small), small.id()),
            ScanoutBlocker::NotCovering
        );
        assert_eq!(check(&[wallpaper], &target), ScanoutBlocker::Missing);
    }

    #[test]
    fn required_sync_without_a_fence_is_not_reported_as_a_gpu_wait() {
        let mut stats = OutputRenderStats::default();
        stats.record(RenderSample {
            needs_sync: true,
            submitted: true,
            total: Duration::from_micros(200),
            ..Default::default()
        });
        stats.record(RenderSample {
            needs_sync: true,
            fence_present: true,
            fence_wait: Some(Duration::from_micros(150)),
            empty: true,
            ..Default::default()
        });
        assert_eq!(stats.needs_sync, 2);
        assert_eq!(stats.fence_present, 1);
        assert_eq!(stats.fence_wait.samples, 1);
        assert_eq!(stats.fence_wait.total_ns, 150_000);
        assert_eq!(stats.submitted, 1);
        assert_eq!(stats.empty, 1);
    }
}
