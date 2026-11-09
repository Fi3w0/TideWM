//! Report-only activity counters and the pressure-event cooldown. A few
//! integer adds per frame or pointer event, read once per Ebb sample. Nothing
//! here changes scheduling, input handling or rendering.
use serde::Serialize;
use std::time::Duration;

/// Shortest gap between two `ebb-pressure` events. The kernel can repeat a
/// PSI trigger every window; listeners only need to hear about an episode.
pub(crate) const PRESSURE_EVENT_COOLDOWN_SECS: u64 = 60;
/// Sampled fallback when PSI is unavailable: system-available memory below
/// this share of the total counts as pressure.
pub(crate) const PRESSURE_AVAILABLE_PERCENT: u128 = 10;

const MIN_PERIOD_SECS: u64 = 30;
const IDLE_REDRAW_FRAMES_PER_SEC: u64 = 20;
const IDLE_REDRAW_EMPTY_PERCENT: u64 = 90;
const POINTER_MIN_EVENTS: u64 = 600;
const POINTER_MEAN_LIMIT_US: u64 = 300;

#[derive(Default)]
pub(crate) struct Watch {
    frames: u64,
    empty_frames: u64,
    pointer_events: u64,
    pointer_ns: u64,
    period_start_secs: u64,
    /// Set by the kernel PSI callback or the sampled fallback; consumed by
    /// the next maintenance tick, never inside the callback.
    pub pressure_signalled: bool,
    pub last_pressure_event_secs: Option<u64>,
    pub pressure_events: u64,
}

/// What happened between two samples. Kept for the latest sample only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Activity {
    pub period_secs: u64,
    pub frames: u64,
    /// Frames the damage tracker found nothing to draw for.
    pub empty_frames: u64,
    pub pointer_events: u64,
    pub pointer_mean_us: Option<u64>,
}

impl Watch {
    pub fn frame(&mut self, empty: bool) {
        self.frames = self.frames.saturating_add(1);
        self.empty_frames = self.empty_frames.saturating_add(u64::from(empty));
    }

    pub fn pointer_motion(&mut self, cost: Duration) {
        self.pointer_events = self.pointer_events.saturating_add(1);
        self.pointer_ns = self
            .pointer_ns
            .saturating_add(cost.as_nanos().min(u128::from(u64::MAX)) as u64);
    }

    /// Closes the current period and starts the next one.
    pub fn take(&mut self, now: u64) -> Activity {
        let activity = Activity {
            period_secs: now.saturating_sub(self.period_start_secs),
            frames: self.frames,
            empty_frames: self.empty_frames,
            pointer_events: self.pointer_events,
            pointer_mean_us: self
                .pointer_ns
                .checked_div(self.pointer_events)
                .map(|ns| ns / 1000),
        };
        self.frames = 0;
        self.empty_frames = 0;
        self.pointer_events = 0;
        self.pointer_ns = 0;
        self.period_start_secs = now;
        activity
    }

    /// One event per pressure episode: true at most once per cooldown.
    pub fn pressure_event_due(&mut self, now: u64) -> bool {
        if !std::mem::take(&mut self.pressure_signalled) {
            return false;
        }
        if self
            .last_pressure_event_secs
            .is_some_and(|last| now.saturating_sub(last) < PRESSURE_EVENT_COOLDOWN_SECS)
        {
            return false;
        }
        self.last_pressure_event_secs = Some(now);
        self.pressure_events = self.pressure_events.saturating_add(1);
        true
    }
}

/// Fixed finding codes; the messages carry numbers, never client strings.
pub(crate) fn findings(activity: &Activity, windows: usize) -> Vec<(&'static str, String)> {
    let mut findings = Vec::new();
    if activity.period_secs < MIN_PERIOD_SECS {
        return findings;
    }
    if activity.frames >= activity.period_secs * IDLE_REDRAW_FRAMES_PER_SEC
        && activity.empty_frames * 100 >= activity.frames * IDLE_REDRAW_EMPTY_PERCENT
    {
        findings.push((
            "idle-redraw",
            format!(
                "TideWM ran about {} frames per second with nothing to draw in {}% of them; something keeps requesting redraws",
                activity.frames / activity.period_secs,
                activity.empty_frames * 100 / activity.frames,
            ),
        ));
    }
    if let Some(mean) = activity.pointer_mean_us.filter(|mean| {
        activity.pointer_events >= POINTER_MIN_EVENTS && *mean >= POINTER_MEAN_LIMIT_US
    }) {
        findings.push((
            "pointer-cost",
            format!(
                "pointer motion took about {mean} microseconds per event over {} events with {windows} windows open; see tidectl ebb stats",
                activity.pointer_events,
            ),
        ));
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_reset_and_empty_periods_report_no_mean() {
        let mut watch = Watch::default();
        watch.frame(true);
        watch.frame(false);
        watch.pointer_motion(Duration::from_micros(40));
        watch.pointer_motion(Duration::from_micros(60));
        assert_eq!(
            watch.take(60),
            Activity {
                period_secs: 60,
                frames: 2,
                empty_frames: 1,
                pointer_events: 2,
                pointer_mean_us: Some(50),
            }
        );
        let quiet = watch.take(120);
        assert_eq!(quiet.period_secs, 60);
        assert_eq!(quiet.frames, 0);
        assert_eq!(quiet.pointer_mean_us, None);
    }

    #[test]
    fn findings_need_a_full_period_and_real_volume() {
        let busy = Activity {
            period_secs: 60,
            frames: 60 * 60,
            empty_frames: 60 * 58,
            pointer_events: 5000,
            pointer_mean_us: Some(450),
        };
        let codes: Vec<_> = findings(&busy, 4).into_iter().map(|f| f.0).collect();
        assert_eq!(codes, ["idle-redraw", "pointer-cost"]);
        // Animation that really draws, slow but rare pointer events, and a
        // period too short to judge are all quiet.
        for quiet in [
            Activity {
                empty_frames: 60 * 30,
                pointer_events: 100,
                ..busy.clone()
            },
            Activity {
                frames: 60 * 5,
                empty_frames: 60 * 5,
                pointer_mean_us: Some(120),
                ..busy.clone()
            },
            Activity {
                period_secs: 10,
                ..busy.clone()
            },
        ] {
            assert!(findings(&quiet, 4).is_empty(), "{quiet:?}");
        }
    }

    #[test]
    fn pressure_events_cool_down_and_need_a_signal() {
        let mut watch = Watch::default();
        assert!(!watch.pressure_event_due(0));
        watch.pressure_signalled = true;
        assert!(watch.pressure_event_due(5));
        watch.pressure_signalled = true;
        assert!(!watch.pressure_event_due(5 + PRESSURE_EVENT_COOLDOWN_SECS - 1));
        assert!(!watch.pressure_signalled);
        watch.pressure_signalled = true;
        assert!(watch.pressure_event_due(5 + PRESSURE_EVENT_COOLDOWN_SECS));
        assert_eq!(watch.pressure_events, 2);
    }
}
