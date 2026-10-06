//! The `env { }` block, kept live across config reloads.
//!
//! The process environment is written once at startup, before any helper
//! thread exists. After that, `set_var` would race other threads' `getenv`,
//! so later changes are tracked here and applied to each spawned child
//! instead. TideWM's own readers (the cursor theme) go through [`get`].

use std::{
    collections::{HashMap, HashSet},
    process::Command,
    sync::{Mutex, MutexGuard},
};

static CHILD_ENV: Mutex<Option<ChildEnv>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<ChildEnv>> {
    CHILD_ENV
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What a reload changed, for exporting to the session manager.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnvChange {
    pub set: Vec<(String, String)>,
    pub unset: Vec<String>,
}

impl EnvChange {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.unset.is_empty()
    }

    pub fn touches(&self, key: &str) -> bool {
        self.set.iter().any(|(k, _)| k == key) || self.unset.iter().any(|k| k == key)
    }
}

#[derive(Debug, Default)]
struct ChildEnv {
    /// Each key's value before `env { }` first touched it (`None` = unset),
    /// restored when the key is removed from the config again.
    baseline: HashMap<String, Option<String>>,
    /// Values from the current config.
    set: HashMap<String, String>,
    /// Pre-config values restored after a key left the config. Kept apart
    /// from `set` because the process environment may still hold the
    /// startup config value.
    restored: HashMap<String, String>,
    /// Keys children must not inherit from the process environment.
    unset: HashSet<String>,
}

impl ChildEnv {
    fn update(
        &mut self,
        config: &HashMap<String, String>,
        process: impl Fn(&str) -> Option<String>,
    ) -> EnvChange {
        let mut change = EnvChange::default();
        for (key, value) in config {
            self.baseline
                .entry(key.clone())
                .or_insert_with(|| process(key));
            if self.effective(key, &process).as_deref() != Some(value.as_str()) {
                change.set.push((key.clone(), value.clone()));
            }
            self.unset.remove(key);
            self.restored.remove(key);
            self.set.insert(key.clone(), value.clone());
        }

        let removed: Vec<String> = self
            .set
            .keys()
            .filter(|key| !config.contains_key(*key))
            .cloned()
            .collect();
        for key in removed {
            self.set.remove(&key);
            match self.baseline.get(&key).cloned().flatten() {
                Some(original) => {
                    change.set.push((key.clone(), original.clone()));
                    self.restored.insert(key, original);
                }
                None => {
                    change.unset.push(key.clone());
                    self.unset.insert(key);
                }
            }
        }

        change.set.sort();
        change.unset.sort();
        change
    }

    fn effective(&self, key: &str, process: impl Fn(&str) -> Option<String>) -> Option<String> {
        if let Some(value) = self.set.get(key).or_else(|| self.restored.get(key)) {
            return Some(value.clone());
        }
        if self.unset.contains(key) {
            return None;
        }
        process(key)
    }
}

fn process_var(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn valid_entries(env: &HashMap<String, String>) -> HashMap<String, String> {
    env.iter()
        .filter(
            |(key, value)| match crate::config::validate_env_entry(key, value) {
                Ok(()) => true,
                Err(reason) => {
                    // Config lowering already filters these with a user-facing
                    // warning; this guards programmatically built configs.
                    tracing::error!(key = ?key, reason, "Skipping invalid environment entry");
                    false
                }
            },
        )
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Startup: records the pre-config values, then writes `env` into this
/// process so the backend (cursor theme, GL drivers) and every child see
/// it. Must run before TideWM starts any thread.
pub fn apply_startup(env: &HashMap<String, String>) {
    let env = valid_entries(env);
    let mut state = ChildEnv::default();
    state.update(&env, process_var);
    for (key, value) in &env {
        std::env::set_var(key, value);
    }
    *lock() = Some(state);
}

/// Reload: makes `env` the environment for children spawned from now on and
/// returns what changed. Never touches the process environment.
pub fn update(env: &HashMap<String, String>) -> EnvChange {
    let env = valid_entries(env);
    lock()
        .get_or_insert_with(ChildEnv::default)
        .update(&env, process_var)
}

/// A variable's current value as configured, falling back to the process
/// environment.
pub fn get(key: &str) -> Option<String> {
    match lock().as_ref() {
        Some(state) => state.effective(key, process_var),
        None => process_var(key),
    }
}

/// Applies reload-time changes to a child before it is spawned.
pub fn apply_to(command: &mut Command) {
    if let Some(state) = lock().as_ref() {
        command.envs(&state.restored);
        command.envs(&state.set);
        for key in &state.unset {
            command.env_remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn process(key: &str) -> Option<String> {
        match key {
            "XCURSOR_SIZE" => Some("24".into()),
            "LANG" => Some("en_US.UTF-8".into()),
            _ => None,
        }
    }

    #[test]
    fn reload_reports_only_real_changes() {
        let mut state = ChildEnv::default();
        let first = state.update(
            &env(&[("XCURSOR_THEME", "Adwaita"), ("XCURSOR_SIZE", "24")]),
            process,
        );
        // XCURSOR_SIZE already had that value in the process environment.
        assert_eq!(first.set, vec![("XCURSOR_THEME".into(), "Adwaita".into())]);

        let same = state.update(
            &env(&[("XCURSOR_THEME", "Adwaita"), ("XCURSOR_SIZE", "24")]),
            process,
        );
        assert!(same.is_empty());

        let changed = state.update(
            &env(&[("XCURSOR_THEME", "breeze_cursors"), ("XCURSOR_SIZE", "24")]),
            process,
        );
        assert_eq!(
            changed.set,
            vec![("XCURSOR_THEME".into(), "breeze_cursors".into())]
        );
        assert!(changed.touches("XCURSOR_THEME") && !changed.touches("XCURSOR_SIZE"));
        assert_eq!(
            state.effective("XCURSOR_THEME", process).as_deref(),
            Some("breeze_cursors")
        );
    }

    #[test]
    fn removed_keys_return_to_their_value_from_before_the_config() {
        let mut state = ChildEnv::default();
        state.update(
            &env(&[("XCURSOR_THEME", "breeze_cursors"), ("XCURSOR_SIZE", "32")]),
            process,
        );

        let removed = state.update(&HashMap::new(), process);
        // XCURSOR_SIZE existed before TideWM touched it; XCURSOR_THEME didn't.
        assert_eq!(removed.set, vec![("XCURSOR_SIZE".into(), "24".into())]);
        assert_eq!(removed.unset, vec!["XCURSOR_THEME".to_string()]);
        assert_eq!(state.effective("XCURSOR_THEME", process), None);
        assert_eq!(
            state.effective("XCURSOR_SIZE", process).as_deref(),
            Some("24")
        );
        assert_eq!(
            state.effective("LANG", process).as_deref(),
            Some("en_US.UTF-8")
        );

        assert!(state.update(&HashMap::new(), process).is_empty());

        let readded = state.update(&env(&[("XCURSOR_THEME", "Adwaita")]), process);
        assert_eq!(
            readded.set,
            vec![("XCURSOR_THEME".into(), "Adwaita".into())]
        );
        assert_eq!(
            state.effective("XCURSOR_THEME", process).as_deref(),
            Some("Adwaita")
        );
    }

    #[test]
    fn children_receive_overrides_and_removals() {
        let mut state = ChildEnv::default();
        state.update(&env(&[("TIDEWM_TEST_ENV", "on")]), |_| None);
        state.update(&HashMap::new(), |_| None);
        assert!(state.unset.contains("TIDEWM_TEST_ENV"));
        assert!(!state.set.contains_key("TIDEWM_TEST_ENV"));
    }
}
