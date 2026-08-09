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

/// Removes `--[[ ... ]]` spans while preserving source offsets and line
/// numbers. Quote-aware for the opener.
pub(crate) fn strip_block_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    let mut in_quotes = false;
    let mut escaped = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if in_quotes {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_quotes = false;
            }
            i += 1;
            continue;
        }
        if byte == b'"' {
            in_quotes = true;
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"--[[") {
            let end = bytes[i + 4..]
                .windows(2)
                .position(|pair| pair == b"]]")
                .map(|offset| i + 4 + offset + 2)
                .unwrap_or(bytes.len());
            for byte in &mut out[i..end] {
                if !matches!(*byte, b'\n' | b'\r') {
                    *byte = b' ';
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    String::from_utf8(out).expect("replacing UTF-8 bytes with ASCII preserves valid UTF-8")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommentStart {
    Line(usize),
    Block(usize),
}

fn color_token_len(line: &str, hash: usize) -> Option<usize> {
    let hex_len = line[hash + 1..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .count();
    if !matches!(hex_len, 6 | 8) {
        return None;
    }
    let after = line[hash + 1 + hex_len..].chars().next();
    after
        .map(|c| c.is_whitespace() || matches!(c, ',' | ']' | ')' | '}'))
        .unwrap_or(true)
        .then_some(hex_len)
}

fn find_comment_start(line: &str) -> Option<CommentStart> {
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if in_quotes {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quotes = false;
            }
            continue;
        }
        match ch {
            '"' => in_quotes = true,
            '#' if color_token_len(line, i).is_none() => return Some(CommentStart::Line(i)),
            '-' if line[i..].starts_with("--[[") => return Some(CommentStart::Block(i)),
            '-' if line[i..].starts_with("--") => return Some(CommentStart::Line(i)),
            _ => {}
        }
    }
    None
}

/// Splits a line into its content and its trailing comment (`#` or `--`),
/// respecting quoted strings and the color-token lookahead: `#` starts a
/// comment unless it starts a color (six or eight hex digits followed by
/// end of line, whitespace, `,`, `]`, `)`, or `}`). A line with no
/// comment returns the whole line and an empty comment.
pub(crate) fn split_line_comment(line: &str) -> (&str, &str) {
    if let Some(start) = find_comment_start(line) {
        let i = match start {
            CommentStart::Line(i) | CommentStart::Block(i) => i,
        };
        (&line[..i], &line[i..])
    } else {
        (line, "")
    }
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
        if let Some(CommentStart::Block(start)) = find_comment_start(raw) {
            // Preserve block-comment lines exactly. Braces and assignments
            // inside the comment must not affect formatter state.
            out.push_str(raw);
            out.push('\n');
            if !raw[start + 4..].contains("]]") {
                in_block_comment = true;
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
        let src = "--[[ a multi\ninput {\n  fake=value\n}\nline block ]]\ngaps=8\n";
        assert_eq!(
            format_source(src),
            "--[[ a multi\ninput {\n  fake=value\n}\nline block ]]\ngaps = 8\n"
        );
    }

    #[test]
    fn strips_block_comments_without_inflating_lines_or_offsets() {
        let src =
            "gaps = 8\n--[[ one long comment\nwith ütf8 and { code = false }\n]]\nborder = 2\n";
        let stripped = strip_block_comments(src);
        assert_eq!(stripped.len(), src.len());
        assert_eq!(
            stripped
                .match_indices('\n')
                .map(|(i, _)| i)
                .collect::<Vec<_>>(),
            src.match_indices('\n').map(|(i, _)| i).collect::<Vec<_>>()
        );
        assert!(stripped.contains("gaps = 8"));
        assert!(stripped.contains("border = 2"));
        assert!(!stripped.contains("one long comment"));
        assert!(!stripped.contains("code = false"));
    }

    #[test]
    fn escaped_quotes_protect_comment_markers() {
        let hash = r##"title = "say \"# still text\"" # real comment"##;
        let dash = r##"title = "say \"-- still text\"" -- real comment"##;
        assert_eq!(
            split_line_comment(hash),
            (r##"title = "say \"# still text\"" "##, "# real comment")
        );
        assert_eq!(
            split_line_comment(dash),
            (r##"title = "say \"-- still text\"" "##, "-- real comment")
        );

        let quoted_block = r##"title = "say \"--[[ still text\"""##;
        assert_eq!(strip_block_comments(quoted_block), quoted_block);
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
