//! Bounded native render counters. Durations measure CPU-side work and waits,
//! not GPU execution time. Each live output owns one aggregate, with no logs
//! or allocation per sample.

use std::time::Duration;

use serde::Serialize;

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
