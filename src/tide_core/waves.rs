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
use std::path::{Path, PathBuf};

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
    for entry in incoming {
        match &entry {
            Entry::Assign(key, _) => {
                if !assign_is_multi(key) {
                    target.retain(|e| !matches!(e, Entry::Assign(k, _) if k == key));
                }
                target.push(entry);
            }
            Entry::VarDef(name, _) => {
                target.retain(|e| !matches!(e, Entry::VarDef(n, _) if n == name));
                target.push(entry);
            }
            Entry::Bind(combo, _) => {
                target.retain(|e| !matches!(e, Entry::Bind(c, _) if c == combo));
                target.push(entry);
            }
            Entry::Handler(event, _) => {
                target.retain(|e| !matches!(e, Entry::Handler(ev, _) if ev == event));
                target.push(entry);
            }
            Entry::Include(_) => {
                // Resolved away before merging ever sees it -- see `resolve`.
            }
            Entry::Block(keyword, header, body) if block_is_keyed(keyword) => {
                if let Some(Entry::Block(_, _, existing_body)) = target
                    .iter_mut()
                    .find(|e| matches!(e, Entry::Block(k, h, _) if k == keyword && h == header))
                {
                    merge_into(existing_body, body.clone());
                } else {
                    target.push(entry);
                }
            }
            Entry::Block(..) => target.push(entry),
        }
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
