//! Waves: TideWM's own config file format (`.wave`).
//!
//! Line-based on purpose, not a general expression grammar -- this is
//! genuinely how Hyprland's own parser works too (split on the first `=`,
//! everything after is one opaque value), and it's what makes
//! `bind $mod+R = spawn:$wave(rofi -show drun, wofi --show drun)` work
//! with no string-quoting needed for the common case (a command with its
//! own spaces/flags). The tradeoff this buys: **every block is real
//! multi-line, always** -- `output eDP-1 { position = 0x0 }` all on one
//! line is not valid, because a value is "the rest of the line," and a
//! line ending in `{` is unambiguously a block header. No exceptions, one
//! rule, easy to explain.
//!
//! This module only knows the generic shape (assignments, `$var`
//! definitions, `bind combo = action` statements, `include "path"`
//! statements, and `keyword [header] { ... }` blocks) and how multiple
//! files merge together. It has no idea what a valid top-level key is --
//! that's `config.rs`'s job when it lowers a merged [`Entry`] list into a
//! [`crate::config`]-internal `RawConfig`. Keeping the split this way
//! means this module can be tested against its own syntax rules alone,
//! without dragging in every config field this project happens to have
//! today.

use std::fs;
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
    /// function source (a `function() ... end` literal). Produced only by
    /// the Wave engine; the line-based grammar has no handlers.
    Handler(String, String),
}

/// Strips a `#` comment, but only outside a quoted string -- a spawn
/// command or window title can legitimately contain `#`.
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Strips one layer of surrounding `"..."` if present. Quoting a value is
/// always optional (rest-of-line already captures everything, `#` and
/// leading/trailing whitespace aside) -- this exists for cases like
/// `include "monitors.wave"` or a title match with meaningful leading/
/// trailing spaces, not because plain values need it.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Parses `contents` into a flat list of top-level [`Entry`] values.
/// `path` is only used to prefix error messages with something the user
/// can actually locate.
pub(crate) fn parse(contents: &str, path: &Path) -> Result<Vec<Entry>, String> {
    let lines: Vec<&str> = contents.lines().collect();
    let mut pos = 0usize;
    parse_block(&lines, &mut pos, path, false)
}

fn parse_block(
    lines: &[&str],
    pos: &mut usize,
    path: &Path,
    nested: bool,
) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    while *pos < lines.len() {
        let line_no = *pos + 1;
        let raw = lines[*pos];
        *pos += 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line == "}" {
            if !nested {
                return Err(format!(
                    "in file {} at line {line_no}: unexpected `}}` with no open block",
                    path.display()
                ));
            }
            return Ok(entries);
        }

        if let Some(header) = line.strip_suffix('{') {
            let header = header.trim();
            let (keyword, rest) = header
                .split_once(char::is_whitespace)
                .unwrap_or((header, ""));
            if keyword.is_empty() {
                return Err(format!(
                    "in file {} at line {line_no}: expected a block name before `{{`",
                    path.display()
                ));
            }
            let body = parse_block(lines, pos, path, true)?;
            entries.push(Entry::Block(
                keyword.to_string(),
                rest.trim().to_string(),
                body,
            ));
            continue;
        }

        if let Some(rest) = line
            .strip_prefix("include")
            .and_then(|r| (r.is_empty() || r.starts_with(char::is_whitespace)).then_some(r.trim()))
        {
            if rest.is_empty() {
                return Err(format!(
                    "in file {} at line {line_no}: `include` needs a path",
                    path.display()
                ));
            }
            entries.push(Entry::Include(unquote(rest)));
            continue;
        }

        let Some((lhs, rhs)) = line.split_once('=') else {
            return Err(format!(
                "in file {} at line {line_no}: expected `key = value`, a block ending in `{{`, `include \"path\"`, or `}}` -- got `{line}`",
                path.display()
            ));
        };
        let lhs = lhs.trim();
        let value = unquote(rhs.trim());
        if let Some(name) = lhs.strip_prefix('$') {
            let name = name.trim();
            if name.is_empty() {
                return Err(format!(
                    "in file {} at line {line_no}: `$` needs a variable name before `=`",
                    path.display()
                ));
            }
            entries.push(Entry::VarDef(name.to_string(), value));
        } else if let Some(combo) = lhs.strip_prefix("bind ") {
            let combo = combo.trim();
            if combo.is_empty() {
                return Err(format!(
                    "in file {} at line {line_no}: `bind` needs a key combo before `=`",
                    path.display()
                ));
            }
            entries.push(Entry::Bind(combo.to_string(), value));
        } else if lhs.is_empty() {
            return Err(format!(
                "in file {} at line {line_no}: missing key before `=`",
                path.display()
            ));
        } else {
            entries.push(Entry::Assign(lhs.to_string(), value));
        }
    }
    if nested {
        return Err(format!(
            "in file {}: unexpected end of file, missing a closing `}}`",
            path.display()
        ));
    }
    Ok(entries)
}

/// Which merge policy a block keyword uses when folding one file's
/// entries onto another's -- see `merge_into`'s doc comment. A small,
/// fixed allowlist rather than something declared per-block, because it's
/// a property of what each keyword *means*, not something a `.wave` file
/// should ever need to say for itself.
fn block_is_keyed(keyword: &str) -> bool {
    matches!(
        keyword,
        "input" | "touchpad" | "env" | "switch_events" | "submap" | "ripple_preset"
    )
}

/// Plain `key = value` keys that are list-shaped instead of scalar --
/// repeating one accumulates rather than overwriting, matching Hyprland's
/// own `exec-once = foo` convention (list one thing per line, not one line
/// holding an array). A small allowlist rather than a general "list keys"
/// mechanism, because this is a property of what each key *means*, not
/// something a `.wave` file should ever need to say for itself.
fn assign_is_multi(key: &str) -> bool {
    matches!(
        key,
        "spawn_at_startup" | "spawn" | "workspace_name" | "workspace_gaps"
    )
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
///   exception is [`assign_is_multi`]'s keys (`spawn_at_startup`,
///   `workspace_name`), which accumulate instead -- see its own doc comment.
/// - A block whose keyword is in [`block_is_keyed`] (`input`, `touchpad`,
///   `env`, `switch_events`, `submap`, `ripple_preset`) merges recursively with an
///   existing block of the same keyword *and* header (the header is the
///   submap name for `submap`, empty for the others) -- these are
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
pub(crate) fn resolve(path: &Path) -> Result<(Vec<Entry>, Vec<String>), String> {
    resolve_with(path, parse)
}

/// `resolve`, parameterized over how a file's text becomes entries, so the
/// Wave engine can reuse the exact same include/merge/cycle machinery with
/// `wave::evaluate` as the parser.
pub(crate) fn resolve_with(
    path: &Path,
    parse_file: fn(&str, &Path) -> Result<Vec<Entry>, String>,
) -> Result<(Vec<Entry>, Vec<String>), String> {
    let mut ancestors = Vec::new();
    let mut warnings = Vec::new();
    let entries = resolve_inner(path, parse_file, &mut ancestors, &mut warnings)?;
    Ok((entries, warnings))
}

fn resolve_inner(
    path: &Path,
    parse_file: fn(&str, &Path) -> Result<Vec<Entry>, String>,
    ancestors: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Entry>, String> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if ancestors.contains(&canonical) {
        return Err(format!("include cycle detected in file {}", path.display()));
    }
    ancestors.push(canonical);
    let result = resolve_uncycled(path, parse_file, ancestors, warnings);
    ancestors.pop();
    result
}

fn resolve_uncycled(
    path: &Path,
    parse_file: fn(&str, &Path) -> Result<Vec<Entry>, String>,
    ancestors: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Entry>, String> {
    let contents =
        fs::read_to_string(path).map_err(|err| format!("in file {}: {err}", path.display()))?;
    let entries = parse_file(&contents, path)?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut merged = Vec::new();
    let mut own_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry {
            Entry::Include(include) => {
                let include_path = resolve_include_path(parent, &include);
                match resolve_inner(&include_path, parse_file, ancestors, warnings) {
                    Ok(included) => merge_into(&mut merged, included),
                    Err(err) => {
                        tracing::warn!(path = %include_path.display(), %err, "Failed to load included config file, skipping");
                        warnings.push(format!(
                            "Failed to load included config file: {err}, skipping"
                        ));
                    }
                }
            }
            other => own_entries.push(other),
        }
    }
    merge_into(&mut merged, own_entries);
    Ok(merged)
}

/// Expands a leading `~/` against `$HOME` and resolves the result against
/// `base_dir` (the including file's own directory) if it's not already
/// absolute.
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

    fn parse_str(s: &str) -> Vec<Entry> {
        parse(s, Path::new("test.wave")).expect("parse should succeed")
    }

    #[test]
    fn parses_flat_assignments_and_strips_comments() {
        let entries = parse_str("# a comment\nterminal = kitty # trailing comment\ngaps = 8\n");
        assert_eq!(
            entries,
            vec![
                Entry::Assign("terminal".into(), "kitty".into()),
                Entry::Assign("gaps".into(), "8".into()),
            ]
        );
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        let entries = parse_str(r#"title = "Firefox # 1""#);
        assert_eq!(
            entries,
            vec![Entry::Assign("title".into(), "Firefox # 1".into())]
        );
    }

    #[test]
    fn parses_var_def_bind_and_include() {
        let entries =
            parse_str("$mod = SUPER\nbind $mod+Return = spawn:kitty\ninclude \"monitors.wave\"\n");
        assert_eq!(
            entries,
            vec![
                Entry::VarDef("mod".into(), "SUPER".into()),
                Entry::Bind("$mod+Return".into(), "spawn:kitty".into()),
                Entry::Include("monitors.wave".into()),
            ]
        );
    }

    #[test]
    fn bind_value_with_commas_and_spaces_needs_no_quoting() {
        let entries = parse_str("bind $mod+R = spawn:$wave(rofi -show drun, wofi --show drun)");
        assert_eq!(
            entries,
            vec![Entry::Bind(
                "$mod+R".into(),
                "spawn:$wave(rofi -show drun, wofi --show drun)".into()
            )]
        );
    }

    #[test]
    fn parses_nested_blocks() {
        let entries = parse_str(
            "input {\n    xkb_layout = us\n    touchpad {\n        natural_scroll = true\n    }\n}\n",
        );
        assert_eq!(
            entries,
            vec![Entry::Block(
                "input".into(),
                "".into(),
                vec![
                    Entry::Assign("xkb_layout".into(), "us".into()),
                    Entry::Block(
                        "touchpad".into(),
                        "".into(),
                        vec![Entry::Assign("natural_scroll".into(), "true".into())]
                    ),
                ]
            )]
        );
    }

    #[test]
    fn block_header_is_captured() {
        let entries = parse_str("output eDP-1 {\n    scale = 1.0\n}\n");
        assert_eq!(
            entries,
            vec![Entry::Block(
                "output".into(),
                "eDP-1".into(),
                vec![Entry::Assign("scale".into(), "1.0".into())]
            )]
        );
    }

    #[test]
    fn unclosed_block_is_an_error() {
        assert!(parse_str_result("input {\n    xkb_layout = us\n").is_err());
    }

    #[test]
    fn stray_closing_brace_is_an_error() {
        assert!(parse_str_result("}\n").is_err());
    }

    #[test]
    fn garbage_line_is_an_error() {
        assert!(parse_str_result("this is not valid\n").is_err());
    }

    fn parse_str_result(s: &str) -> Result<Vec<Entry>, String> {
        parse(s, Path::new("test.wave"))
    }

    #[test]
    fn repeated_key_within_one_file_last_wins_after_merge() {
        let mut acc = Vec::new();
        merge_into(&mut acc, parse_str("gaps = 8\ngaps = 12\n"));
        assert_eq!(acc, vec![Entry::Assign("gaps".into(), "12".into())]);
    }

    #[test]
    fn spawn_at_startup_accumulates_instead_of_overwriting() {
        let mut acc = Vec::new();
        merge_into(
            &mut acc,
            parse_str("spawn_at_startup = waybar\nspawn_at_startup = swww init\n"),
        );
        assert_eq!(
            acc,
            vec![
                Entry::Assign("spawn_at_startup".into(), "waybar".into()),
                Entry::Assign("spawn_at_startup".into(), "swww init".into()),
            ]
        );
    }

    #[test]
    fn keyed_blocks_merge_field_by_field_but_output_blocks_just_append() {
        let mut acc = Vec::new();
        merge_into(&mut acc, parse_str("input {\n    xkb_layout = us\n}\n"));
        merge_into(&mut acc, parse_str("input {\n    repeat_rate = 30\n}\n"));
        merge_into(&mut acc, parse_str("output eDP-1 {\n    scale = 1.0\n}\n"));
        merge_into(&mut acc, parse_str("output eDP-1 {\n    scale = 2.0\n}\n"));

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
