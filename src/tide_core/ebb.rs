//! Bounded, report-only resource monitoring. Owners still clean up on destruction.
use serde::Serialize;
use std::collections::VecDeque;

const RECENT_LIMIT: usize = 1440;
const HOURLY_LIMIT: usize = 168;
const DAY: u64 = 86_400;
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Sample {
    pub uptime_secs: u64,
    pub pss_bytes: Option<u64>,
    pub texture_bytes: u64,
    pub fd_count: Option<u64>,
    pub system_available_bytes: Option<u64>,
    pub pressure_some_avg10: Option<f64>,
    pub windows: usize,
    pub away: bool,
    pub away_since_secs: Option<u64>,
    pub leftover_count: usize,
    pub leftovers: Vec<Leftover>,
    pub cost_us: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Leftover {
    pub owner: &'static str,
    pub count: usize,
}

#[derive(Clone, Debug, Serialize)]
struct Hour {
    hour: u64,
    samples: u64,
    pss_bytes_mean: Option<u64>,
    pss_samples: u64,
    texture_bytes_mean: u64,
    away_samples: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Warning {
    pub code: &'static str,
    pub message: String,
    pub active: bool,
    pub first_seen_secs: u64,
    pub last_seen_secs: u64,
    last_notified_secs: u64,
}

#[derive(Default)]
pub(crate) struct Ebb {
    pub recent: VecDeque<Sample>,
    hourly: VecDeque<Hour>,
    warnings: Vec<Warning>,
    pub last_sample_secs: Option<u64>,
    pub deferred_since_secs: Option<u64>,
    pub away_since_secs: Option<u64>,
    pub last_trim_secs: Option<u64>,
    pub trim_calls: u64,
    pub trim_released: u64,
    pub last_trim_cost_us: u64,
    cap_settings: [Option<u64>; 3],
    cap_active: [bool; 3],
    pub trim_blocked: bool,
}

impl Ebb {
    /// Prefer quiet moments, but continuous decorative animation must not
    /// starve telemetry. Sampling is still outside rendering and grabs.
    pub fn note_away(&mut self, now: u64, away: bool) {
        if away {
            self.away_since_secs.get_or_insert(now);
        } else {
            self.away_since_secs = None;
        }
    }

    pub fn due(&mut self, now: u64, interval: u64, animating: bool) -> bool {
        if self
            .last_sample_secs
            .is_some_and(|last| now.saturating_sub(last) < interval)
        {
            return false;
        }
        if animating {
            let deferred = *self.deferred_since_secs.get_or_insert(now);
            if now.saturating_sub(deferred) < interval {
                return false;
            }
        }
        self.deferred_since_secs = None;
        true
    }

    /// Caps are soft gates, with 90% hysteresis. Unknown values never
    /// trigger a cap; the VRAM gate explicitly uses our texture estimate.
    pub fn caps_allow_trim(
        &mut self,
        config: &crate::config::EbbConfig,
        pss: Option<u64>,
        textures: u64,
    ) -> bool {
        if config.mode == crate::config::EbbMode::Off {
            self.cap_active = [false; 3];
            self.trim_blocked = false;
            return false;
        }
        let caps = [config.ram_cap, config.cache_cap, config.vram_cap];
        if caps != self.cap_settings {
            self.cap_settings = caps;
            self.cap_active = [false; 3];
            self.trim_blocked = false;
        }
        for (index, value) in [pss, Some(textures), Some(textures)]
            .into_iter()
            .enumerate()
        {
            match caps[index].zip(value) {
                Some((cap, value)) if value >= cap => self.cap_active[index] = true,
                Some((cap, value)) if u128::from(value) * 10 <= u128::from(cap) * 9 => {
                    self.cap_active[index] = false
                }
                None => self.cap_active[index] = false,
                _ => {}
            }
        }
        let active = self.cap_active.iter().any(|active| *active);
        if !active {
            self.trim_blocked = false;
        }
        (caps.iter().all(Option::is_none) || active) && !self.trim_blocked
    }

    pub fn record(&mut self, sample: Sample, leak_window: u64) -> Vec<Warning> {
        let record_started = std::time::Instant::now();
        let now = sample.uptime_secs;
        self.last_sample_secs = Some(now);
        // Keep ownership detail only for the latest sample. History keeps
        // the count, without multiplying per-owner allocations by 1440.
        if let Some(previous) = self.recent.back_mut() {
            previous.leftovers = Vec::new();
        }
        self.recent.push_back(sample);
        if self.recent.len() > RECENT_LIMIT {
            let old = self.recent.pop_front().unwrap();
            self.archive(old);
        }
        let sample = self.recent.back().unwrap();
        let mut findings = Vec::new();
        if self.trim_blocked {
            findings.push(("cap-unreclaimable", "Ebb reached a cap but libc could release no free heap pages; no repeated trim attempts until the cap clears or changes".into()));
        }
        if !sample.leftovers.is_empty() {
            findings.push(("leftovers", format!(
                "TideWM retains {} references or cache entries for destroyed resources; see tidectl ebb stats",
                sample.leftovers.iter().map(|entry| entry.count).sum::<usize>(),
            )));
        }
        // A contiguous recent window only: no trend across activity/away
        // changes, unknown data, output sleep gaps, or a changed window count.
        let all: Vec<_> = self.recent.iter().collect();
        for (code, away_only, texture) in [
            ("memory-growth", false, false),
            ("texture-growth", false, true),
            ("away-memory-growth", true, false),
            ("away-texture-growth", true, true),
        ] {
            let horizon = if away_only {
                leak_window.min(600)
            } else {
                leak_window
            };
            if let Some(rate) = growth(
                &all,
                horizon,
                away_only,
                |s| {
                    if texture {
                        Some(s.texture_bytes)
                    } else {
                        s.pss_bytes
                    }
                },
                8 * MIB,
            ) {
                findings.push((code, format!(
                    "TideWM {} grew about {:.1} MiB/hour{}; growth observed, attribution needs investigation",
                    if texture { "texture estimate" } else { "PSS" },
                    rate / MIB as f64, if away_only { " while away" } else { "" },
                )));
            }
        }
        if let Some(rate) = growth(&all, leak_window, false, |s| s.fd_count, 16) {
            findings.push((
                "fd-growth",
                format!("TideWM open files grew about {rate:.0}/hour; see tidectl ebb stats"),
            ));
        }
        // Include history maintenance and trend checks, as well as the
        // kernel reads and owner audit timed by the caller. Notification
        // rendering is exceptional and is not part of normal sampling.
        let sample = self.recent.back_mut().unwrap();
        sample.cost_us = sample.cost_us.saturating_add(
            record_started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64,
        );
        if sample.cost_us > 2_000 {
            findings.push((
                "sample-cost",
                format!(
                    "Ebb sample took {} microseconds; main-loop sampling needs review",
                    sample.cost_us,
                ),
            ));
        }
        let mut notify = Vec::new();
        for warning in &mut self.warnings {
            warning.active = false;
        }
        // There are exactly eight fixed finding codes; reports cannot grow
        // with window count, arbitrary client strings or session duration.
        for (code, message) in findings {
            if let Some(warning) = self.warnings.iter_mut().find(|w| w.code == code) {
                warning.active = true;
                warning.last_seen_secs = now;
                warning.message = message;
                if now.saturating_sub(warning.last_notified_secs) >= DAY {
                    warning.last_notified_secs = now;
                    notify.push(warning.clone());
                }
            } else {
                let warning = Warning {
                    code,
                    message,
                    active: true,
                    first_seen_secs: now,
                    last_seen_secs: now,
                    last_notified_secs: now,
                };
                notify.push(warning.clone());
                self.warnings.push(warning);
            }
        }
        notify
    }

    fn archive(&mut self, sample: Sample) {
        let hour = sample.uptime_secs / 3600;
        if self.hourly.back().is_none_or(|last| last.hour != hour) {
            if self.hourly.len() == HOURLY_LIMIT {
                self.hourly.pop_front();
            }
            self.hourly.push_back(Hour {
                hour,
                samples: 0,
                pss_bytes_mean: None,
                pss_samples: 0,
                texture_bytes_mean: 0,
                away_samples: 0,
            });
        }
        let bucket = self.hourly.back_mut().unwrap();
        bucket.texture_bytes_mean = mean(
            bucket.texture_bytes_mean,
            bucket.samples,
            sample.texture_bytes,
        );
        bucket.samples += 1;
        if let Some(pss) = sample.pss_bytes {
            bucket.pss_bytes_mean = Some(mean(
                bucket.pss_bytes_mean.unwrap_or(0),
                bucket.pss_samples,
                pss,
            ));
            bucket.pss_samples += 1;
        }
        bucket.away_samples += u64::from(sample.away);
    }

    pub fn stats(&self, config: &crate::config::EbbConfig) -> serde_json::Value {
        serde_json::json!({
            "phase": 1,
            "config": config,
            "sample_count": self.recent.len(),
            "latest": self.recent.back(),
            "history": self.recent,
            "hourly": self.hourly,
            "history_limits": { "recent": RECENT_LIMIT, "hourly": HOURLY_LIMIT },
            "warnings": self.warnings,
            "vram_bytes": null,
            "vram_scope": "unknown: no portable per-process GPU-memory reader; vram_cap uses the TideWM texture estimate",
            "texture_scope": "estimate of TideWM-owned textures; excludes client buffers and driver allocations",
            "cache_eviction_available": false,
            "heap_trim": { "supported": cfg!(target_env = "gnu"), "calls": self.trim_calls,
                "released_calls": self.trim_released, "last_cost_us": self.last_trim_cost_us,
                "blocked": self.trim_blocked },
        })
    }
}

fn mean(previous: u64, count: u64, next: u64) -> u64 {
    ((u128::from(previous) * u128::from(count) + u128::from(next)) / u128::from(count + 1)) as u64
}

/// Positive trend with at least six samples, a full horizon, >=80% rising
/// or flat steps, and a meaningful hourly slope. Gaps invalidate the trend.
fn growth(
    samples: &[&Sample],
    horizon: u64,
    away_only: bool,
    value: impl Fn(&Sample) -> Option<u64>,
    minimum_per_hour: u64,
) -> Option<f64> {
    let last = *samples.last()?;
    if away_only && !last.away {
        return None;
    }
    let mut points = Vec::new();
    for sample in samples.iter().rev() {
        if sample.windows != last.windows
            || (away_only && (!sample.away || sample.away_since_secs != last.away_since_secs))
        {
            break;
        }
        let number = value(sample)?;
        points.push((sample.uptime_secs, number));
        if last.uptime_secs.saturating_sub(sample.uptime_secs) >= horizon {
            break;
        }
    }
    let (first_time, first_value) = *points.last()?;
    let elapsed = last.uptime_secs.checked_sub(first_time)?;
    if points.len() < 6 || elapsed < horizon || elapsed == 0 {
        return None;
    }
    if points
        .windows(2)
        .any(|p| p[0].0.saturating_sub(p[1].0) > 1200)
    {
        return None;
    }
    let rising = points.windows(2).filter(|p| p[0].1 >= p[1].1).count();
    if rising * 5 < (points.len() - 1) * 4 {
        return None;
    }
    let rate = value(last)?.checked_sub(first_value)? as f64 * 3600.0 / elapsed as f64;
    (rate >= minimum_per_hour as f64).then_some(rate)
}

/// GNU libc can return free heap pages; musl simply has no such operation.
/// No byte saving is claimed: the return value only says pages were released.
pub(crate) fn trim_heap() -> bool {
    #[cfg(target_env = "gnu")]
    {
        // Safety: malloc_trim is a thread-safe libc allocator operation with
        // no pointers or ownership transfer. 0 requests no retained top pad.
        unsafe { libc::malloc_trim(0) != 0 }
    }
    #[cfg(not(target_env = "gnu"))]
    {
        false
    }
}

pub(crate) fn kb_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?
            .checked_mul(1024)
    })
}

pub(crate) fn pressure_avg10(text: &str) -> Option<f64> {
    let some = text.lines().find(|line| line.starts_with("some "))?;
    let number = some
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()?;
    (number.is_finite() && (0.0..=100.0).contains(&number)).then_some(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(time: u64, memory: u64, away: bool) -> Sample {
        Sample {
            uptime_secs: time,
            pss_bytes: Some(memory),
            texture_bytes: 0,
            fd_count: Some(20),
            system_available_bytes: None,
            pressure_some_avg10: None,
            windows: 2,
            away,
            away_since_secs: away.then_some(0),
            leftover_count: 0,
            leftovers: Vec::new(),
            cost_us: 300,
        }
    }

    #[test]
    fn detects_growth_without_confusing_new_windows_unknown_data_or_gaps() {
        let samples: Vec<_> = (0..=60)
            .map(|i| sample(i * 60, 100 * MIB + i * MIB, false))
            .collect();
        let refs: Vec<_> = samples.iter().collect();
        assert_eq!(
            growth(&refs, 3600, false, |s| s.pss_bytes, 8 * MIB),
            Some(60.0 * MIB as f64)
        );
        let mut changed = samples.clone();
        changed[30].windows = 3;
        assert!(growth(
            &changed.iter().collect::<Vec<_>>(),
            3600,
            false,
            |s| s.pss_bytes,
            8 * MIB
        )
        .is_none());
        changed = samples.clone();
        changed[30].pss_bytes = None;
        assert!(growth(
            &changed.iter().collect::<Vec<_>>(),
            3600,
            false,
            |s| s.pss_bytes,
            8 * MIB
        )
        .is_none());
        changed = samples;
        changed[60].uptime_secs += 3600;
        assert!(growth(
            &changed.iter().collect::<Vec<_>>(),
            3600,
            false,
            |s| s.pss_bytes,
            8 * MIB
        )
        .is_none());
    }

    #[test]
    fn away_growth_requires_an_uninterrupted_away_window() {
        let mut samples: Vec<_> = (0..=10)
            .map(|i| sample(i * 60, 100 * MIB + i * MIB, true))
            .collect();
        assert!(growth(
            &samples.iter().collect::<Vec<_>>(),
            600,
            true,
            |s| s.pss_bytes,
            8 * MIB
        )
        .is_some());
        samples[5].away = false;
        assert!(growth(
            &samples.iter().collect::<Vec<_>>(),
            600,
            true,
            |s| s.pss_bytes,
            8 * MIB
        )
        .is_none());
    }

    #[test]
    fn history_and_reports_stay_bounded_and_unknowns_stay_unknown() {
        let mut ebb = Ebb::default();
        for i in 0..2000 {
            ebb.record(sample(i * 60, 100 * MIB, false), 3600);
        }
        assert_eq!(ebb.recent.len(), RECENT_LIMIT);
        assert!(!ebb.hourly.is_empty());
        for i in 0..HOURLY_LIMIT as u64 + 10 {
            ebb.archive(sample((i + 100) * 3600, 100 * MIB, false));
        }
        assert_eq!(ebb.hourly.len(), HOURLY_LIMIT);
        let mut missing = sample(200_000, 0, false);
        missing.pss_bytes = None;
        missing.fd_count = None;
        ebb.record(missing, 3600);
        let stats = ebb.stats(&crate::config::EbbConfig::default());
        assert!(stats["latest"]["pss_bytes"].is_null());
        assert!(stats["vram_bytes"].is_null());
        assert!(serde_json::to_vec(&stats).unwrap().len() < 512 * 1024);
    }

    #[test]
    fn warning_notifies_once_per_day_and_clears_when_fixed() {
        let mut ebb = Ebb::default();
        let mut s = sample(0, 100 * MIB, false);
        s.leftovers.push(Leftover {
            owner: "test owner",
            count: 1,
        });
        assert_eq!(ebb.record(s.clone(), 3600).len(), 1);
        s.uptime_secs = 60;
        assert!(ebb.record(s.clone(), 3600).is_empty());
        assert_eq!(ebb.warnings.len(), 1);
        s.uptime_secs = DAY;
        assert_eq!(ebb.record(s, 3600).len(), 1);
        ebb.record(sample(DAY + 60, 100 * MIB, false), 3600);
        assert!(!ebb.warnings[0].active);
    }

    #[test]
    fn animation_deferral_is_bounded_and_interval_changes_apply() {
        let mut ebb = Ebb::default();
        assert!(!ebb.due(0, 60, true));
        assert!(!ebb.due(59, 60, true));
        assert!(ebb.due(60, 60, true));
        ebb.record(sample(60, 0, false), 3600);
        assert!(!ebb.due(69, 60, false));
        assert!(ebb.due(70, 10, false));
    }

    #[test]
    fn caps_are_gates_with_hysteresis_unknowns_and_off_are_safe() {
        let mut ebb = Ebb::default();
        let mut config = crate::config::EbbConfig {
            ram_cap: Some(100),
            ..Default::default()
        };
        assert!(!ebb.caps_allow_trim(&config, Some(99), 200));
        assert!(!ebb.caps_allow_trim(&config, None, 200));
        assert!(ebb.caps_allow_trim(&config, Some(100), 200));
        assert!(ebb.caps_allow_trim(&config, Some(95), 200));
        assert!(!ebb.caps_allow_trim(&config, Some(90), 200));
        config.vram_cap = Some(100);
        assert!(ebb.caps_allow_trim(&config, None, 100));
        ebb.trim_blocked = true;
        assert!(!ebb.caps_allow_trim(&config, None, 101));
        assert!(!ebb.caps_allow_trim(&config, None, 80));
        assert!(ebb.caps_allow_trim(&config, None, 100));
        config.mode = crate::config::EbbMode::Off;
        assert!(!ebb.caps_allow_trim(&config, Some(1000), 1000));
    }

    #[test]
    fn proc_parsers_preserve_missing_or_malformed_signals() {
        assert_eq!(
            kb_field("MemAvailable: 123 kB\n", "MemAvailable:"),
            Some(125_952)
        );
        assert_eq!(kb_field("MemFree: 123 kB\n", "MemAvailable:"), None);
        assert_eq!(
            pressure_avg10("some avg10=1.25 avg60=2.00 total=30\nfull avg10=0.00\n"),
            Some(1.25)
        );
        assert_eq!(pressure_avg10("some avg10=NaN"), None);
        assert_eq!(pressure_avg10("some avg10=101"), None);
    }
}
