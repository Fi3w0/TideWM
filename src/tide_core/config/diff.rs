use super::*;

/// What changed between two merged entry lists (W3's reload diff).
/// Computed at the entry level, so it is grammar-agnostic: old and new
/// Wave configs produce the same `waves::Entry` shape.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ConfigDiff {
    /// Top-level keys whose value changed: (key, old, new). Added keys
    /// come through with an empty old, removed with an empty new.
    pub keys_changed: Vec<(String, String, String)>,
    pub binds_added: Vec<(String, String)>,
    pub binds_removed: Vec<(String, String)>,
    /// Combos whose action list changed: (combo, old actions joined, new).
    pub binds_changed: Vec<(String, String, String)>,
    /// Blocks whose bodies changed: (keyword, header).
    pub blocks_changed: Vec<(String, String)>,
}

impl ConfigDiff {
    pub fn is_empty(&self) -> bool {
        self.keys_changed.is_empty()
            && self.binds_added.is_empty()
            && self.binds_removed.is_empty()
            && self.binds_changed.is_empty()
            && self.blocks_changed.is_empty()
    }

    /// A one-line human summary for the reload log: "3 keys, binds
    /// +1 -1 ~2, blocks: border, input".
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.keys_changed.is_empty() {
            let keys: Vec<&str> = self
                .keys_changed
                .iter()
                .map(|(k, _, _)| k.as_str())
                .collect();
            parts.push(format!(
                "{} key{} changed ({})",
                keys.len(),
                if keys.len() == 1 { "" } else { "s" },
                keys.join(", ")
            ));
        }
        if !self.binds_added.is_empty()
            || !self.binds_removed.is_empty()
            || !self.binds_changed.is_empty()
        {
            parts.push(format!(
                "binds +{} -{} ~{}",
                self.binds_added.len(),
                self.binds_removed.len(),
                self.binds_changed.len()
            ));
        }
        if !self.blocks_changed.is_empty() {
            let keywords: Vec<&str> = self
                .blocks_changed
                .iter()
                .map(|(k, _)| k.as_str())
                .collect();
            parts.push(format!("blocks changed ({})", keywords.join(", ")));
        }
        if parts.is_empty() {
            "nothing changed".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Diffs two merged entry lists per section: top-level keys, binds, and
/// blocks. Variables and includes are deliberately skipped: they are
/// substitution/resolution machinery, not configuration.
pub(crate) fn diff_entries(old: &[waves::Entry], new: &[waves::Entry]) -> ConfigDiff {
    let old_keys = collect_assigns(old);
    let new_keys = collect_assigns(new);
    let mut keys_changed = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for key in old_keys.keys().chain(new_keys.keys()) {
        if !seen.insert(key) {
            continue; // present in both lists, already compared
        }
        let o = old_keys.get(key).cloned().unwrap_or_default();
        let n = new_keys.get(key).cloned().unwrap_or_default();
        if o != n {
            keys_changed.push((key.clone(), o, n));
        }
    }
    keys_changed.sort();

    let old_binds = collect_binds(old);
    let new_binds = collect_binds(new);
    let mut binds_added = Vec::new();
    let mut binds_removed = Vec::new();
    let mut binds_changed = Vec::new();
    for (combo, actions) in &new_binds {
        match old_binds.get(combo) {
            None => {
                for a in actions {
                    binds_added.push((combo.clone(), a.clone()));
                }
            }
            Some(old_actions) if old_actions != actions => {
                binds_changed.push((combo.clone(), old_actions.join(" "), actions.join(" ")));
            }
            Some(_) => {}
        }
    }
    for (combo, actions) in &old_binds {
        if !new_binds.contains_key(combo) {
            for a in actions {
                binds_removed.push((combo.clone(), a.clone()));
            }
        }
    }
    binds_added.sort();
    binds_removed.sort();
    binds_changed.sort();

    let old_blocks = collect_blocks(old);
    let new_blocks = collect_blocks(new);
    let mut blocks_changed: Vec<(String, String)> = Vec::new();
    let mut seen_blocks = std::collections::BTreeSet::new();
    for key in old_blocks.keys().chain(new_blocks.keys()) {
        if seen_blocks.insert(key) && old_blocks.get(key) != new_blocks.get(key) {
            blocks_changed.push(key.clone());
        }
    }
    blocks_changed.sort();

    ConfigDiff {
        keys_changed,
        binds_added,
        binds_removed,
        binds_changed,
        blocks_changed,
    }
}

fn collect_assigns(entries: &[waves::Entry]) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for entry in entries {
        if let waves::Entry::Assign(key, value) = entry {
            map.insert(key.clone(), value.clone()); // last write wins, as in lowering
        }
    }
    map
}

fn collect_binds(entries: &[waves::Entry]) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut map: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        if let waves::Entry::Bind(combo, action) = entry {
            map.entry(combo.clone()).or_default().push(action.clone());
        }
    }
    map
}

fn collect_blocks(
    entries: &[waves::Entry],
) -> std::collections::BTreeMap<(String, String), Vec<waves::Entry>> {
    let mut map: std::collections::BTreeMap<(String, String), Vec<waves::Entry>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        if let waves::Entry::Block(keyword, header, body) = entry {
            map.entry((keyword.clone(), header.clone()))
                .or_default()
                .push(waves::Entry::Block(
                    keyword.clone(),
                    header.clone(),
                    body.clone(),
                ));
        }
    }
    map
}
