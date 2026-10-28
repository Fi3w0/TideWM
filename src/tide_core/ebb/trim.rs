//! Timing, weak visibility clocks and bounded pass reports. No resource ownership.
use crate::config::EbbMode;
use serde::Serialize;
use smithay::reexports::wayland_server::{protocol::wl_surface::WlSurface, Weak};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PassKind {
    Light,
    Deep,
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct PassReport {
    pub kind: Option<PassKind>,
    pub uptime_secs: u64,
    pub examined: usize,
    pub entries_evicted: usize,
    pub released_texture_estimate_bytes: u64,
    pub maps_shrunk: usize,
    pub budget_exhausted: bool,
    pub duration_us: u64,
}

#[derive(Default)]
pub(crate) struct TrimState {
    pub hidden_since: HashMap<Weak<WlSurface>, u64>,
    pub requested: Option<PassKind>,
    pub pressure_pending: bool,
    pub cleanup_pending: bool,
    pub heap_pending: bool,
    pub audit_pending: bool,
    pub deferred_reason: Option<&'static str>,
    pub last_pass: Option<PassReport>,
    pub last_light: Option<u64>,
    pub last_deep: Option<u64>,
    pub passes: u64,
    pub entries_evicted: u64,
    pub released_texture_estimate_bytes: u64,
    pub cursor: usize,
}

impl TrimState {
    pub fn request(&mut self, kind: PassKind) {
        if self.requested != Some(PassKind::Deep) {
            self.requested = Some(kind);
        }
    }

    pub fn due(
        &self,
        now: u64,
        mode: EbbMode,
        idle_ms: u128,
        away: bool,
        light_after: u32,
        interval_secs: u64,
    ) -> Option<PassKind> {
        if mode == EbbMode::Off {
            return None;
        }
        if self
            .last_light
            .is_some_and(|last| now.saturating_sub(last) < interval_secs)
        {
            return None;
        }
        if self.audit_pending && self.requested.is_none() {
            return Some(PassKind::Light);
        }
        if let Some(kind) = self.requested {
            return Some(kind);
        }
        if away
            && self
                .last_deep
                .is_none_or(|last| now.saturating_sub(last) >= deep_interval(mode))
        {
            return Some(PassKind::Deep);
        }
        if (self.pressure_pending && matches!(mode, EbbMode::Balanced | EbbMode::Aggressive))
            || idle_ms >= u128::from(light_after)
        {
            return Some(PassKind::Light);
        }
        None
    }

    pub fn record(&mut self, report: PassReport, allow_heap: bool) {
        self.last_light = Some(report.uptime_secs);
        if report.kind == Some(PassKind::Deep) {
            self.last_deep = Some(report.uptime_secs);
        }
        self.passes += 1;
        self.entries_evicted += report.entries_evicted as u64;
        self.released_texture_estimate_bytes = self
            .released_texture_estimate_bytes
            .saturating_add(report.released_texture_estimate_bytes);
        self.requested = None;
        self.pressure_pending = false;
        self.cleanup_pending |= allow_heap || report.entries_evicted > 0;
        self.heap_pending |= allow_heap;
        self.audit_pending = false;
        self.deferred_reason = None;
        self.last_pass = Some(report);
    }
}

pub(crate) fn hidden_delay(mode: EbbMode) -> u64 {
    match mode {
        EbbMode::Off => u64::MAX,
        EbbMode::Low => 1800,
        EbbMode::Balanced => 600,
        EbbMode::Aggressive => 120,
    }
}

pub(crate) fn deep_interval(mode: EbbMode) -> u64 {
    match mode {
        EbbMode::Off => u64::MAX,
        EbbMode::Low => 10_800,
        EbbMode::Balanced => 3600,
        EbbMode::Aggressive => 1800,
    }
}

pub(crate) fn eligible(
    mode: EbbMode,
    kind: PassKind,
    hidden_since: Option<u64>,
    now: u64,
    protected: bool,
    alive: bool,
) -> bool {
    if mode == EbbMode::Off || protected {
        return false;
    }
    if !alive {
        return true;
    }
    kind == PassKind::Deep
        || hidden_since.is_some_and(|since| now.saturating_sub(since) >= hidden_delay(mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn safety_wins_over_deep_pressure_and_age() {
        for mode in [
            EbbMode::Off,
            EbbMode::Low,
            EbbMode::Balanced,
            EbbMode::Aggressive,
        ] {
            for kind in [PassKind::Light, PassKind::Deep] {
                assert!(!eligible(mode, kind, Some(0), 100_000, true, true));
                assert!(!eligible(mode, kind, Some(0), 100_000, true, false));
            }
        }
        assert!(!eligible(
            EbbMode::Off,
            PassKind::Deep,
            Some(0),
            100_000,
            false,
            false
        ));
        assert!(!eligible(
            EbbMode::Low,
            PassKind::Light,
            Some(0),
            1799,
            false,
            true
        ));
        assert!(eligible(
            EbbMode::Low,
            PassKind::Light,
            Some(0),
            1800,
            false,
            true
        ));
        assert!(eligible(EbbMode::Low, PassKind::Deep, None, 1, false, true));
    }
    #[test]
    fn manual_requests_coalesce_and_cooldown_prevents_hammering() {
        let mut state = TrimState::default();
        state.request(PassKind::Deep);
        state.request(PassKind::Light);
        assert_eq!(
            state.due(0, EbbMode::Low, 0, false, 300_000, 60),
            Some(PassKind::Deep)
        );
        state.record(
            PassReport {
                kind: Some(PassKind::Deep),
                ..Default::default()
            },
            true,
        );
        state.request(PassKind::Light);
        assert_eq!(state.due(59, EbbMode::Low, 0, false, 300_000, 60), None);
        assert_eq!(state.due(60, EbbMode::Off, 0, true, 300_000, 60), None);
        assert_eq!(
            state.due(60, EbbMode::Low, 0, false, 300_000, 60),
            Some(PassKind::Light)
        );
    }
}
