//! Waves: the shared config model and merge policy for the Wave format.
//!
//! The Wave engine (`wave.rs`) evaluates config into these [`Entry`]
//! values; this module knows nothing about parsing. It owns the generic
//! shape (assignments, `@name` variable definitions, bind statements,
//! handler registrations, and `keyword [header] { ... }` blocks), how
//! multiple files merge together, and the include-path resolution the
//! engine walks with. It has no idea what a valid top-level key is --
//! that's `config.rs`'s job when it lowers a merged [`Entry`] list into a
//! [`crate::config`]-internal `RawConfig`. Keeping the split this way
//! means this module can be tested against its merge rules alone,
//! without dragging in every config field this project happens to have
//! today.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// One parsed line (or block) from a `.wave` file. Deliberately untyped
/// beyond this shape -- `config.rs` decides what a given `keyword`/`key`
/// means when lowering a fully-merged list of these into `RawConfig`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Entry {
    /// `key = value`
    Assign(String, String),
    /// `$name = value`
    VarDef(String, String),
    /// `bind <combo> = <action>`
    Bind(String, String),
    /// `include "path"`
    Include(String),
    /// `keyword [header] { ...body... }` -- e.g. `Block("output",
    /// "eDP-1", [...])`, or `Block("input", "", [...])` for a header-less
    /// section.
    Block(String, String, Vec<Entry>),
    /// `on "event" { ... }` -- the event name and the transpiled Lua
    /// function source (a `function() ... end` literal).
    Handler(String, String),
}

/// Strips a `#` comment, but only outside a quoted string -- a spawn
/// command or window title can legitimately contain `#`.
fn block_is_keyed(keyword: &str) -> bool {
    matches!(
        keyword,
        "input" | "touchpad" | "env" | "switch_events" | "mode" | "ripple_preset"
    )
}

/// Plain `key = value` keys that are list-shaped instead of scalar --
/// repeating one accumulates rather than overwriting, matching Hyprland's
/// own `exec-once = foo` convention (list one thing per line, not one line
/// holding an array). A small allowlist rather than a general "list keys"
/// mechanism, because this is a property of what each key *means*, not
/// something a `.wave` file should ever need to say for itself.
fn assign_is_multi(key: &str) -> bool {
    matches!(key, "spawn" | "workspace_name" | "workspace_gaps")
}

/// Folds `incoming` onto `target` in place, applying the same policy in
/// both directions this gets used for: parsing one file's own entries
/// (fold its lines onto an initially-empty accumulator, so accidental
/// duplication *within* one file is handled the same way as duplication
/// *across* included files) and resolving `include`s (fold the including
/// file's own entries onto whatever its includes already produced, so its
/// own keys win -- see `resolve`).
///
/// - A scalar entry (`Assign`/`VarDef`/`Bind`) replaces any earlier entry
///   of the same kind with the same key/name/combo -- last write wins,
///   matching a TOML table's "duplicate key means override" shape once
///   merged across files (a *literal* duplicate key within one raw TOML
///   file is a hard parse error there; Waves is more forgiving on
///   purpose, since "last bind on this combo wins" is a perfectly
///   sensible thing to want across a multi-file split). The one
///   exception is [`assign_is_multi`]'s keys (`spawn`, `workspace_name`),
///   which accumulate instead -- see its own doc comment.
/// - A block whose keyword is in [`block_is_keyed`] (`input`, `touchpad`,
///   `env`, `switch_events`, `mode`, `ripple_preset`) merges recursively with an
///   existing block of the same keyword *and* header (the header is the
///   mode name for `mode`, empty for the others) -- these are
///   conceptually single named sections, the same as a TOML table
///   merging key-by-key across files.
/// - Every other block (`output`, `rule`, anything not in the allowlist)
///   always appends as a new entry, never merges -- these are
///   conceptually arrays (TOML's `[[output]]`/`[[window_rule]]`
///   concatenate across files, they don't merge by name).
pub(crate) fn merge_into(target: &mut Vec<Entry>, incoming: Vec<Entry>) {
    // Replacements must move to their last-write position, so mutating an
    // entry in place would change observable lowering order. Tombstones let
    // us invalidate the previous positions in O(1), append the replacement,
    // then compact once after the merge instead of retaining the whole
    // target for every incoming scalar.
    let mut slots: Vec<Option<Entry>> = std::mem::take(target).into_iter().map(Some).collect();
    let mut assigns: HashMap<String, Vec<usize>> = HashMap::new();
    let mut variables: HashMap<String, Vec<usize>> = HashMap::new();
    let mut binds: HashMap<String, Vec<usize>> = HashMap::new();
    let mut handlers: HashMap<String, Vec<usize>> = HashMap::new();
    let mut keyed_blocks: HashMap<String, HashMap<String, usize>> = HashMap::new();

    for (index, entry) in slots
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.as_ref().map(|entry| (index, entry)))
    {
        match entry {
            Entry::Assign(key, _) if !assign_is_multi(key) => {
                assigns.entry(key.clone()).or_default().push(index);
            }
            Entry::VarDef(name, _) => variables.entry(name.clone()).or_default().push(index),
            Entry::Bind(combo, _) => binds.entry(combo.clone()).or_default().push(index),
            Entry::Handler(event, _) => handlers.entry(event.clone()).or_default().push(index),
            Entry::Block(keyword, header, _) if block_is_keyed(keyword) => {
                keyed_blocks
                    .entry(keyword.clone())
                    .or_default()
                    .entry(header.clone())
                    .or_insert(index);
            }
            _ => {}
        }
    }

    for entry in incoming {
        match entry {
            Entry::Assign(key, value) => {
                if assign_is_multi(&key) {
                    slots.push(Some(Entry::Assign(key, value)));
                } else {
                    invalidate_slots(&mut slots, assigns.remove(&key));
                    let index = slots.len();
                    assigns.insert(key.clone(), vec![index]);
                    slots.push(Some(Entry::Assign(key, value)));
                }
            }
            Entry::VarDef(name, value) => {
                invalidate_slots(&mut slots, variables.remove(&name));
                let index = slots.len();
                variables.insert(name.clone(), vec![index]);
                slots.push(Some(Entry::VarDef(name, value)));
            }
            Entry::Bind(combo, action) => {
                invalidate_slots(&mut slots, binds.remove(&combo));
                let index = slots.len();
                binds.insert(combo.clone(), vec![index]);
                slots.push(Some(Entry::Bind(combo, action)));
            }
            Entry::Handler(event, body) => {
                invalidate_slots(&mut slots, handlers.remove(&event));
                let index = slots.len();
                handlers.insert(event.clone(), vec![index]);
                slots.push(Some(Entry::Handler(event, body)));
            }
            Entry::Include(_) => {
                // Resolved away before merging ever sees it -- see `resolve`.
            }
            Entry::Block(keyword, header, body) if block_is_keyed(&keyword) => {
                let existing = keyed_blocks
                    .get(&keyword)
                    .and_then(|headers| headers.get(&header))
                    .copied();
                if let Some(index) = existing {
                    let Some(Entry::Block(_, _, existing_body)) = slots[index].as_mut() else {
                        unreachable!("keyed block index must address a live block")
                    };
                    merge_into(existing_body, body);
                } else {
                    let index = slots.len();
                    keyed_blocks
                        .entry(keyword.clone())
                        .or_default()
                        .insert(header.clone(), index);
                    slots.push(Some(Entry::Block(keyword, header, body)));
                }
            }
            entry @ Entry::Block(..) => slots.push(Some(entry)),
        }
    }
    target.extend(slots.into_iter().flatten());
}

fn invalidate_slots(slots: &mut [Option<Entry>], positions: Option<Vec<usize>>) {
    for position in positions.into_iter().flatten() {
        slots[position] = None;
    }
}

/// Reads `path`, recursively resolving its own `include "..."` entries
/// (relative to `path`'s own directory, `~` expanded) into one merged
/// [`Entry`] list -- the Waves equivalent of the old `load_toml_merged`.
/// Merge order: every include is folded in left-to-right, then the
/// including file's own (non-`include`) entries are folded on top last,
/// so they always win -- matches this project's established multi-file
/// contract (a later include overlays an earlier one, the including
/// file's own keys always win over anything it included).
///
/// Only the top-level file's own errors (missing, unreadable,
/// unparseable, or a genuine include cycle) propagate. A problem in an
/// *included* file is logged as a warning and that one include is
/// skipped, same resilience convention as everything else in this
/// project's config loading -- the returned `Vec<String>` carries that
/// same message out to the caller so it can reach the compositor-owned
/// warning panel, not just the log file (a broken include used to be
/// invisible on screen entirely).
pub(crate) fn resolve_include_path(base_dir: &Path, include: &str) -> PathBuf {
    let expanded = match include.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(include),
        },
        None => PathBuf::from(include),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(key: &str, value: &str) -> Entry {
        Entry::Assign(key.to_string(), value.to_string())
    }

    fn block(keyword: &str, header: &str, body: Vec<Entry>) -> Entry {
        Entry::Block(keyword.to_string(), header.to_string(), body)
    }

    #[test]
    fn repeated_key_within_one_file_last_wins_after_merge() {
        let mut acc = Vec::new();
        merge_into(&mut acc, vec![assign("gaps", "8"), assign("gaps", "12")]);
        assert_eq!(acc, vec![Entry::Assign("gaps".into(), "12".into())]);
    }

    #[test]
    fn scalar_override_keeps_last_write_order_and_removes_all_old_copies() {
        let mut acc = vec![
            assign("gaps", "4"),
            block("output", "DP-1", vec![]),
            assign("gaps", "8"),
            assign("border_size", "2"),
        ];

        merge_into(&mut acc, vec![assign("gaps", "12")]);

        assert_eq!(
            acc,
            vec![
                block("output", "DP-1", vec![]),
                assign("border_size", "2"),
                assign("gaps", "12"),
            ]
        );
    }

    #[test]
    fn spawn_accumulates_instead_of_overwriting() {
        let mut acc = Vec::new();
        merge_into(
            &mut acc,
            vec![assign("spawn", "waybar"), assign("spawn", "swww init")],
        );
        assert_eq!(
            acc,
            vec![assign("spawn", "waybar"), assign("spawn", "swww init")]
        );
    }

    #[test]
    fn keyed_blocks_merge_field_by_field_but_output_blocks_just_append() {
        let mut acc = Vec::new();
        merge_into(
            &mut acc,
            vec![block("input", "", vec![assign("xkb_layout", "us")])],
        );
        merge_into(
            &mut acc,
            vec![block("input", "", vec![assign("repeat_rate", "30")])],
        );
        merge_into(
            &mut acc,
            vec![block("output", "eDP-1", vec![assign("scale", "1.0")])],
        );
        merge_into(
            &mut acc,
            vec![block("output", "eDP-1", vec![assign("scale", "2.0")])],
        );

        assert_eq!(
            acc,
            vec![
                Entry::Block(
                    "input".into(),
                    "".into(),
                    vec![
                        Entry::Assign("xkb_layout".into(), "us".into()),
                        Entry::Assign("repeat_rate".into(), "30".into()),
                    ]
                ),
                Entry::Block(
                    "output".into(),
                    "eDP-1".into(),
                    vec![Entry::Assign("scale".into(), "1.0".into())]
                ),
                Entry::Block(
                    "output".into(),
                    "eDP-1".into(),
                    vec![Entry::Assign("scale".into(), "2.0".into())]
                ),
            ]
        );
    }
}
