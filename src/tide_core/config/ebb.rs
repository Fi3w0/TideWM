//! Ebb settings live with their lowering and diagnostics.
use super::{values::parse_duration_ms_including_zero, RawConfig};
use crate::waves::Entry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EbbMode {
    Off,
    Low,
    Balanced,
    Aggressive,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EbbConfig {
    pub mode: EbbMode,
    pub check_every_ms: u32,
    pub ram_cap: Option<u64>,
    pub cache_cap: Option<u64>,
    pub vram_cap: Option<u64>,
    pub light_after_ms: u32,
    pub away_after_ms: u32,
    pub pass_budget_ms: u32,
    pub leak_window_ms: u32,
    pub toast: bool,
}

impl Default for EbbConfig {
    fn default() -> Self {
        Self {
            mode: EbbMode::Low,
            check_every_ms: 60_000,
            ram_cap: None,
            cache_cap: None,
            vram_cap: None,
            light_after_ms: 300_000,
            away_after_ms: 900_000,
            pass_budget_ms: 1,
            leak_window_ms: 3_600_000,
            toast: true,
        }
    }
}

/// Parse serialized sizes using the same unit table and exact-byte bound as Wave.
fn parse_size(value: &str) -> Option<u64> {
    crate::wave::parse_size_bytes(value)
}

pub(super) fn apply_ebb_block(raw: &mut RawConfig, body: &[Entry], advanced: bool) {
    for entry in body {
        let Entry::Assign(key, value) = entry else {
            raw.ebb_warnings
                .push("Ebb accepts settings, not nested blocks".into());
            continue;
        };
        let invalid = || format!("Invalid Ebb setting {key} = {value}; keeping previous value");
        let config = &mut raw.ebb;
        let duration =
            |min, max| parse_duration_ms_including_zero(value).map(|ms| ms.clamp(min, max));
        match (advanced, key.as_str()) {
            (false, "mode") => match value.as_str() {
                "off" => config.mode = EbbMode::Off,
                "low" => config.mode = EbbMode::Low,
                "balanced" => config.mode = EbbMode::Balanced,
                "aggressive" => config.mode = EbbMode::Aggressive,
                _ => raw.ebb_warnings.push(invalid()),
            },
            (false, "ram_cap" | "cache_cap" | "vram_cap") => {
                if let Some(bytes) = parse_size(value).filter(|bytes| *bytes > 0) {
                    match key.as_str() {
                        "ram_cap" => config.ram_cap = Some(bytes),
                        "cache_cap" => config.cache_cap = Some(bytes),
                        _ => config.vram_cap = Some(bytes),
                    }
                } else {
                    raw.ebb_warnings.push(invalid());
                }
            }
            (false, "check_every") => match duration(10_000, 600_000) {
                Some(ms) => {
                    config.check_every_ms = ms;
                    if parse_duration_ms_including_zero(value) != Some(ms) {
                        raw.ebb_warnings.push(format!(
                            "Ebb check_every = {value} clamped to {ms}ms (10s minimum, 10m maximum)"
                        ));
                    }
                }
                None => raw.ebb_warnings.push(invalid()),
            },
            (true, "light_after" | "away_after" | "pass_budget" | "leak_window") => {
                let (min, max) = match key.as_str() {
                    "pass_budget" => (1, 10),
                    "leak_window" => (600_000, 86_400_000),
                    _ => (10_000, 86_400_000),
                };
                if let Some(ms) = duration(min, max) {
                    match key.as_str() {
                        "light_after" => config.light_after_ms = ms,
                        "away_after" => config.away_after_ms = ms,
                        "pass_budget" => config.pass_budget_ms = ms,
                        _ => config.leak_window_ms = ms,
                    }
                    if parse_duration_ms_including_zero(value) != Some(ms) {
                        raw.ebb_warnings
                            .push(format!("Ebb {key} = {value} clamped to {ms}ms"));
                    }
                } else {
                    raw.ebb_warnings.push(invalid());
                }
            }
            (true, "toast") => match value.as_str() {
                "true" => config.toast = true,
                "false" => config.toast = false,
                _ => raw.ebb_warnings.push(invalid()),
            },
            _ => raw.ebb_warnings.push(format!("Unknown Ebb setting {key}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_units_and_invalid_caps() {
        assert_eq!(parse_size("400MB"), Some(400_000_000));
        assert_eq!(parse_size("1.5GiB"), Some(1_610_612_736));
        for value in ["-1MB", "NaNMB", "infGB", "1e99GB", "0.1B", "400", "1mb"] {
            assert_eq!(parse_size(value), None, "{value}");
        }
    }

    #[test]
    fn lowering_clamps_interval_and_reports_diagnostics() {
        let entries = crate::wave::evaluate(
            "ebb {\n check_every = 1s\n ram_cap = 400MB * 2\n mode = off\n}\nebb_advanced {\n toast = false\n}\n",
            std::path::Path::new("ebb.wave"),
        ).unwrap();
        let raw = super::super::lower_entries(&entries);
        let (config, warnings) = super::super::Config::from_raw(raw);
        assert_eq!(config.ebb.check_every_ms, 10_000);
        assert_eq!(config.ebb.ram_cap, Some(800_000_000));
        assert_eq!(config.ebb.mode, EbbMode::Off);
        assert!(!config.ebb.toast);
        assert!(warnings.iter().any(|w| w.contains("clamped")));
        let mut raw = RawConfig::default();
        apply_ebb_block(
            &mut raw,
            &[
                Entry::Assign("check_every".into(), "20m".into()),
                Entry::Assign("ram_cap".into(), "-1MB".into()),
                Entry::Assign("mod".into(), "low".into()),
            ],
            false,
        );
        assert_eq!(raw.ebb.check_every_ms, 600_000);
        assert_eq!(raw.ebb.ram_cap, None);
        assert_eq!(raw.ebb_warnings.len(), 3);
    }
}
