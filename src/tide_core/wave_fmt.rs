//! wave_fmt: the canonical Wave formatter, deliberately dependency-free.
//!
//! The standalone `wavefmt` binary includes this exact file via
//! `#[path = "../tide_core/wave_fmt.rs"]`, and `tide_core::wave` imports
//! the same functions, so the comment/color lexing and the formatting
//! rules live in exactly one place until the parser crate extraction.
//!
//! The formatter is conservative by design: it normalizes leading
//! indentation, the spacing around the first `=` on assignment lines,
//! and trailing whitespace, and leaves everything else verbatim,
//! including one-line binds, block comments, and the internal spacing
//! of values.

/// Removes `--[[ ... ]]` spans, replacing them with newlines so line
/// numbers in error messages stay correct. Quote-aware for the opener.
pub(crate) fn strip_block_comments(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let mut in_quotes = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            in_quotes = !in_quotes;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_quotes
            && c == '-'
            && i + 3 < chars.len()
            && chars[i + 1] == '-'
            && chars[i + 2] == '['
            && chars[i + 3] == '['
        {
            let mut j = i + 4;
            let mut closed = false;
            while j + 1 < chars.len() {
                if chars[j] == ']' && chars[j + 1] == ']' {
                    closed = true;
                    break;
                }
                j += 1;
            }
            let end = if closed { j + 2 } else { chars.len() };
            out.extend(std::iter::repeat_n('\n', end - i));
            i = end;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Splits a line into its content and its trailing comment (`#` or `--`),
/// respecting quoted strings and the color-token lookahead: `#` starts a
/// comment unless it starts a color (six or eight hex digits followed by
/// end of line, whitespace, `,`, `]`, `)`, or `}`). A line with no
/// comment returns the whole line and an empty comment.
pub(crate) fn split_line_comment(line: &str) -> (&str, &str) {
    let mut in_quotes = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => {
                let hex_len = line[i + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .count();
                if hex_len == 6 || hex_len == 8 {
                    let after = line[i + 1 + hex_len..].chars().next();
                    let delimited = after
                        .map(|c| c.is_whitespace() || matches!(c, ',' | ']' | ')' | '}'))
                        .unwrap_or(true);
                    if delimited {
                        continue; // a color token, not a comment
                    }
                }
                return (&line[..i], &line[i..]);
            }
            '-' if !in_quotes && line[i..].starts_with("--") => return (&line[..i], &line[i..]),
            _ => {}
        }
    }
    (line, "")
}

/// Strips a `#` or `--` comment from a line (see [`split_line_comment`]).
pub(crate) fn strip_line_comment(line: &str) -> &str {
    split_line_comment(line).0
}

/// The canonical Wave format for `source`. Line-by-line: leading
/// indentation is normalized to two spaces per open block, assignment
/// lines get exactly one space around the first `=`, trailing whitespace
/// is dropped, and everything else (values, one-line binds, comments,
/// `--[[ ]]` regions) is kept verbatim. Idempotent by construction.
#[allow(dead_code)] // the wavefmt binary uses this file via #[path]
pub(crate) fn format_source(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut depth = 0usize;
    let mut in_block_comment = false;
    for raw in source.lines() {
        let raw = raw.trim_end();
        if in_block_comment {
            out.push_str(raw);
            out.push('\n');
            if raw.contains("]]") {
                in_block_comment = false;
            }
            continue;
        }
        let (content, comment) = split_line_comment(raw);
        let trimmed = content.trim();
        if trimmed.is_empty() {
            // A comment-only line: reindent the comment to the depth.
            if comment.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&"  ".repeat(depth));
                out.push_str(comment.trim_start());
                out.push('\n');
            }
            continue;
        }
        if trimmed.contains("--[[") {
            // A block comment begins on this line: keep it verbatim.
            out.push_str(raw);
            out.push('\n');
            if !trimmed.contains("]]") {
                in_block_comment = true;
            }
            continue;
        }
        let is_close = trimmed == "}";
        if is_close {
            depth = depth.saturating_sub(1);
        }
        out.push_str(&"  ".repeat(depth));
        if let Some((key, value)) = trimmed.split_once('=') {
            if !key.trim().is_empty() {
                // exactly one space around the first `=`
                out.push_str(key.trim());
                out.push_str(" = ");
                out.push_str(value.trim());
                if !comment.is_empty() {
                    out.push(' ');
                    out.push_str(comment.trim_start());
                }
                out.push('\n');
                if trimmed.ends_with('{') {
                    depth += 1;
                }
                continue;
            }
        }
        out.push_str(trimmed);
        if !comment.is_empty() {
            out.push(' ');
            out.push_str(comment.trim_start());
        }
        out.push('\n');
        if trimmed.ends_with('{') {
            depth += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indents_blocks_and_normalizes_assignment_spacing() {
        let messy = "input {\n   repeat_rate=25\n      touchpad {\n\ttap_to_click=true\n}\n}\n";
        assert_eq!(
            format_source(messy),
            "input {\n  repeat_rate = 25\n  touchpad {\n    tap_to_click = true\n  }\n}\n"
        );
    }

    #[test]
    fn keeps_colors_and_trailing_comments_intact() {
        let src = "color=#8EDDFF # a trailing note\ngaps = 8\n";
        assert_eq!(
            format_source(src),
            "color = #8EDDFF # a trailing note\ngaps = 8\n"
        );
    }

    #[test]
    fn leaves_values_and_one_line_binds_verbatim() {
        let src = "bind $mod+D { \"spawn:rofi -show drun\" }\ntitle = \"a  b\"\nspawn = [waybar, \"swaybg -i ~/x.png\"]\n";
        assert_eq!(
            format_source(src),
            "bind $mod+D { \"spawn:rofi -show drun\" }\ntitle = \"a  b\"\nspawn = [waybar, \"swaybg -i ~/x.png\"]\n"
        );
    }

    #[test]
    fn preserves_block_comments_verbatim() {
        let src = "--[[ a multi\nline block ]]gaps=8\n";
        let formatted = format_source(src);
        assert!(
            formatted.contains("--[[ a multi\nline block ]]"),
            "{formatted}"
        );
        assert!(formatted.contains("gaps = 8"), "{formatted}");
    }

    #[test]
    fn formatting_is_idempotent() {
        let messy = "  border {\n  width=2\n   }\n";
        let once = format_source(messy);
        assert_eq!(format_source(&once), once);
    }

    #[test]
    fn comment_only_lines_reindent_with_depth() {
        let src = "input {\n   # a note\n}\n";
        assert_eq!(format_source(src), "input {\n  # a note\n}\n");
    }
}
