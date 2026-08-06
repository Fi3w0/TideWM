//! Wave: the Lua-backed config surface.
//!
//! Implements the desugaring contract in `WAVE.md`'s "The desugaring
//! contract (for implementers)" section. The surface syntax compiles to
//! a Lua chunk, the chunk runs against a small registration environment,
//! and evaluation produces the same [`Entry`] list the line-based parser
//! in [`super::waves`] produces, so `config.rs`'s lowering and
//! `merge_into`'s merge policies work unchanged. Keeping the split this
//! way means this module can be tested against its own syntax rules
//! alone, without dragging in every config field this project has today.
//!
//! This is the W1 prototype: the grammar core. `on "event"` handlers
//! (W7), section globals so `theme.primary` reads as an expression (W4),
//! and duration math (`600ms * 2`, W4's typed values) are deliberately
//! not here yet.
//!
//! Dead-code allowance: `config.rs` still lowers through the line-based
//! `waves` parser; wiring `evaluate` in is the W2/W3 step, so the whole
//! module is unreferenced outside its tests until then.
#![allow(dead_code)]

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use mlua::{Function, Lua, StdLib, Value, Variadic};

use super::waves::Entry;

// ---------------------------------------------------------------------------
// Compile: surface syntax -> Lua source
// ---------------------------------------------------------------------------

/// The words a line may not start with as a config key.
fn is_reserved(word: &str) -> bool {
    matches!(
        word,
        "bind" | "include" | "fn" | "script" | "on" | "if" | "elseif" | "else" | "for"
            | "while" | "do" | "end" | "local" | "function" | "return"
    )
}

/// Removes `--[[ ... ]]` spans, replacing them with newlines so line
/// numbers in error messages stay correct. Quote-aware for the opener.
fn strip_block_comments(source: &str) -> String {
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

/// Strips a `#` or `--` comment, but not outside a quoted string, and
/// not a `#` that starts a color token (six or eight hex digits followed
/// by end of line, whitespace, `,`, `]`, `)`, or `}`).
fn strip_line_comment(line: &str) -> &str {
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
                return &line[..i];
            }
            '-' if !in_quotes && line[i..].starts_with("--") => return &line[..i],
            _ => {}
        }
    }
    line
}

fn lua_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Undoes `\"` and `\\` escapes inside a quoted string's inner text, so
/// a quoted literal can be re-emitted with clean escaping.
fn unescape_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_number(word: &str) -> bool {
    word.parse::<f64>().is_ok()
}

fn is_color(word: &str) -> bool {
    let hex = word.strip_prefix('#').unwrap_or_default();
    (hex.len() == 6 || hex.len() == 8) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

fn split_duration(word: &str) -> Option<(&str, &str)> {
    for unit in ["ms", "s", "m"] {
        if let Some(num) = word.strip_suffix(unit) {
            if !num.is_empty()
                && num.chars().all(|c| c.is_ascii_digit() || c == '.')
                && num.parse::<f64>().is_ok()
            {
                return Some((num, unit));
            }
        }
    }
    None
}

fn is_duration(word: &str) -> bool {
    split_duration(word).is_some()
}

fn is_lua_keyword(word: &str) -> bool {
    matches!(
        word,
        "and" | "break" | "do" | "else" | "elseif" | "end" | "false" | "for" | "function"
            | "goto" | "if" | "in" | "local" | "nil" | "not" | "or" | "repeat" | "return"
            | "then" | "true" | "until" | "while"
    )
}

/// The names visible as Lua identifiers: variables defined with
/// `@name = value` (statically known or runtime), `fn` names, and
/// in-scope loop variables / `fn` parameters.
///
/// `@` is the definition marker, `$` the string-reference marker, and
/// expressions use plain identifiers: `@extra = 4` defines a variable,
/// `bind $mod+Num$i { ... }` references it in a string, and
/// `gaps = 8 * extra` uses it in an expression.
///
/// The scope stack holds one entry per opened Lua block: `Some(vars)` for
/// `for` and `fn` bodies, `None` for `if`/`while`/`do`. `end` pops the
/// top entry, so a loop variable stops being visible exactly when its
/// loop's `end` runs.
#[derive(Default)]
struct Symbols {
    /// `@name = <literal>` definitions, name -> the literal text.
    statics: std::collections::HashMap<String, String>,
    /// `@name = <expression>` definitions: the Lua global exists at
    /// runtime, reachable as the plain identifier `name`.
    runtime_vars: std::collections::HashSet<String>,
    fns: std::collections::HashSet<String>,
    scopes: Vec<Option<std::collections::HashSet<String>>>,
}

impl Symbols {
    fn in_scope(&self, name: &str) -> bool {
        self.statics.contains_key(name)
            || self.runtime_vars.contains(name)
            || self.fns.contains(name)
            || self.scope_contains(name)
    }

    fn scope_contains(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .filter_map(|s| s.as_ref())
            .any(|s| s.contains(name))
    }

    /// String-position `$name`: statically known -> its literal text;
    /// a runtime variable, loop var, or `fn` parameter -> the plain
    /// identifier (a concatenation at runtime); otherwise -> None, and
    /// the caller reports the loud "not defined" error.
    fn resolve_string(&self, name: &str) -> Option<String> {
        if let Some(lit) = self.statics.get(name) {
            Some(lit.clone())
        } else if self.runtime_vars.contains(name) || self.scope_contains(name) {
            Some(name.to_string())
        } else {
            None
        }
    }
}

/// The surface -> Lua rewriter for value expressions.
struct Rewriter<'a> {
    sym: &'a Symbols,
}

impl<'a> Rewriter<'a> {
    fn classify_word(&self, word: &str) -> Result<String, String> {
        // `$` and `@` only mean "a config name inside a bare string" /
        // "a variable definition line". In an expression both are errors
        // pointing at the identifier form.
        if let Some(name) = word.strip_prefix('@') {
            return Err(format!(
                "`@{name}` only defines a variable on its own line (`@{name} = value`); in an expression use `{name}`"
            ));
        }
        if let Some(name) = word.strip_prefix('$') {
            if name == "wave" {
                return Err(
                    "`$wave(...)` is for strings; in an expression call `wave(...)`".to_string(),
                );
            }
            if self.sym.in_scope(name) {
                return Err(format!(
                    "`${name}` cannot be used in an expression; use `{name}` (no `$`)"
                ));
            }
            return Err(format!(
                "`${name}` is not defined; define it with `@{name} = value`, and use `{name}` (no `$`) in expressions"
            ));
        }
        if is_number(word) || is_lua_keyword(word) || matches!(word, "true" | "false" | "nil") {
            return Ok(word.to_string());
        }
        if is_color(word) {
            let hex = word.strip_prefix('#').unwrap();
            return Ok(format!("_color({})", lua_quote(&hex[..6.min(hex.len())])));
        }
        if is_duration(word) {
            let (num, unit) = split_duration(word).unwrap();
            return Ok(format!("_dur({}, {})", num, lua_quote(unit)));
        }
        if let Some((base, _)) = word.split_once('.') {
            if self.sym.in_scope(base) {
                return Ok(word.to_string()); // member access on a known identifier
            }
        }
        if self.sym.in_scope(word) {
            return Ok(word.to_string()); // defined @variable, fn name, loop var, or fn param
        }
        Ok(lua_quote(word))
    }

    /// Rewrites an expression string into Lua with canonical spacing:
    /// `(` `[` `{` attach to what precedes, `)` `]` `}` attach to what
    /// follows, `,` gets one space after. A word followed by `(` is a
    /// call name and stays an identifier; an unknown function is a loud
    /// runtime error, not a silently quoted string. The `$` marker never
    /// appears in expressions: a `$name` token is an error suggesting the
    /// identifier form.
    fn rewrite(&self, expr: &str) -> Result<String, String> {
        let mut out = String::new();
        let mut need_space = false;
        let tokens = tokenize(expr);
        let mut it = tokens.iter().peekable();
        while let Some(tok) = it.next() {
            let is_call_name = it.peek() == Some(&&"(".to_string());
            let text = if is_operator(tok) || is_quoted_string(tok) || is_call_name {
                tok.clone()
            } else {
                self.classify_word(tok)?
            };
            if need_space && !matches!(tok.as_str(), "(" | "[" | "{" | ")" | "]" | "}" | ",") {
                out.push(' ');
            }
            out.push_str(&text);
            match tok.as_str() {
                "(" | "[" | "{" | ")" | "]" | "}" => need_space = false,
                "," => {
                    out.push(' ');
                    need_space = false;
                }
                _ => need_space = true,
            }
        }
        Ok(out)
    }

    /// String-position text: build a quoted string or a concatenation.
    ///
    /// A fully quoted string is literal: no `$` processing at all, so
    /// `"$HOME"` in a spawn command is verbatim text and quoting finally
    /// protects a value. `$` processing happens only in bare strings, and
    /// an undefined `$name` is a loud error, never silent text.
    fn rewrite_string(&self, text: &str) -> Result<String, String> {
        if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
            return Ok(lua_quote(&unescape_quotes(inner)));
        }
        let mut out = String::new();
        let mut lit = String::new();
        let mut has_dynamic = false;
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '$' {
                let rest = &text[i + 1..];
                if let Some(inner) = rest.strip_prefix("wave(") {
                    // $wave(a, b) splice: raw comma-split segments
                    let Some(close) = find_matching_paren(inner) else {
                        continue;
                    };
                    let args = inner[..close]
                        .split(',')
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty());
                    let call = format!(
                        "wave({})",
                        args.map(lua_quote).collect::<Vec<_>>().join(", ")
                    );
                    if !lit.is_empty() {
                        out.push_str(&lua_quote(&lit));
                        out.push_str(" .. ");
                        lit.clear();
                    }
                    out.push_str(&call);
                    has_dynamic = true;
                    // consumed since `$`: "wave(" (5) + args + ")"
                    for _ in 0..(close + 6) {
                        chars.next();
                    }
                    continue;
                }
                let name_end = rest
                    .char_indices()
                    .find(|(_, c)| !c.is_ascii_alphanumeric() && *c != '_')
                    .map(|(j, _)| j)
                    .unwrap_or(rest.len());
                let name = &rest[..name_end];
                match self.sym.resolve_string(name) {
                    Some(repl) if repl == name => {
                        // runtime variable, loop variable, or fn parameter:
                        // splice the identifier
                        if !lit.is_empty() {
                            out.push_str(&lua_quote(&lit));
                            out.push_str(" .. ");
                            lit.clear();
                        }
                        out.push_str(name);
                        has_dynamic = true;
                    }
                    Some(repl) => {
                        // statically known literal: fold into the literal text
                        lit.push_str(&repl);
                    }
                    None => {
                        return Err(format!(
                            "`${name}` is not defined; define it with `@{name} = value`, or quote the string if you mean literal text"
                        ));
                    }
                }
                for _ in 0..name_end {
                    chars.next();
                }
            } else {
                lit.push(c);
            }
        }
        if !lit.is_empty() {
            if has_dynamic {
                out.push_str(&lua_quote(&lit));
            } else {
                return Ok(lua_quote(&lit));
            }
        }
        if has_dynamic {
            Ok(out)
        } else {
            Ok(lua_quote(""))
        }
    }
}

fn is_always_operator(c: char) -> bool {
    matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',')
}

fn is_math_operator(c: char) -> bool {
    matches!(c, '+' | '-' | '*' | '/' | '%' | '<' | '>' | '=' | '~')
}

/// Is `chars[i]` the start of an operator token here? Parentheses,
/// brackets, braces, and commas always are. Math operators only count as
/// operators when a non-word character touches them on at least one side:
/// `8 * scale` computes, but `SUPER+Return` and `no-such-binary` are
/// single words (the operators-need-spaces rule from the WAVE.md
/// contract).
fn op_token_len(chars: &[char], i: usize) -> Option<usize> {
    let c = chars[i];
    if is_always_operator(c) {
        return Some(1);
    }
    if !is_math_operator(c) {
        return None;
    }
    let len = if i + 1 < chars.len()
        && matches!(
            [c, chars[i + 1]].iter().collect::<String>().as_str(),
            ".." | "==" | "~=" | "<=" | ">="
        ) {
        2
    } else {
        1
    };
    let is_word = |j: usize| {
        j < chars.len()
            && !chars[j].is_whitespace()
            && chars[j] != '"'
            && !is_always_operator(chars[j])
            && !is_math_operator(chars[j])
    };
    if is_word(i.saturating_sub(1)) && is_word(i + len) {
        None // squeezed between two words: part of a word, not an operator
    } else {
        Some(len)
    }
}

/// Splits an expression into tokens: words, quoted strings, and
/// operator/separator tokens. Dots inside words are part of the word
/// (`theme.primary` is one token).
fn tokenize(expr: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            toks.push(chars[start..i].iter().collect());
            continue;
        }
        if let Some(len) = op_token_len(&chars, i) {
            toks.push(chars[i..i + len].iter().collect());
            i += len;
            continue;
        }
        let start = i;
        while i < chars.len()
            && !chars[i].is_whitespace()
            && chars[i] != '"'
            && op_token_len(&chars, i).is_none()
        {
            i += 1;
        }
        toks.push(chars[start..i].iter().collect());
    }
    toks
}

fn is_operator(tok: &str) -> bool {
    matches!(
        tok,
        "(" | ")" | "[" | "]" | "{" | "}" | "," | "+" | "-" | "*" | "/" | "%" | "<" | ">"
            | "=" | "~" | ".." | "==" | "~=" | "<=" | ">="
    )
}

fn is_quoted_string(tok: &str) -> bool {
    tok.starts_with('"')
}

fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// The surface parser: `source` -> Lua chunk text.
pub(crate) fn compile(source: &str, path: &Path) -> Result<String, String> {
    let pre = strip_block_comments(source);
    let lines: Vec<&str> = pre.lines().collect();
    let mut pos = 0usize;
    let mut sym = Symbols::default();
    let mut out = String::from("-- wave: compiled surface (generated; do not edit)\n");
    let mut depth = 0usize;
    emit_body(&lines, &mut pos, &mut sym, &mut out, path, &mut depth)?;
    if depth != 0 {
        return Err(format!(
            "in file {}: unexpected end of file, missing a closing `}}`",
            path.display()
        ));
    }
    Ok(out)
}

fn emit_body(
    lines: &[&str],
    pos: &mut usize,
    sym: &mut Symbols,
    out: &mut String,
    path: &Path,
    depth: &mut usize,
) -> Result<(), String> {
    while *pos < lines.len() {
        let line_no = *pos + 1;
        let raw = lines[*pos];
        *pos += 1;
        let line = strip_line_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line == "}" {
            if *depth == 0 {
                return Err(format!(
                    "in file {} at line {line_no}: unexpected `}}` with no open block",
                    path.display()
                ));
            }
            *depth -= 1;
            return Ok(());
        }

        let first = line.split_whitespace().next().unwrap_or("");

        // -- Lua statement passthrough --------------------------------------
        if matches!(
            first,
            "if" | "elseif" | "else" | "while" | "do" | "local" | "function" | "return"
        ) {
            if first == "if" || first == "while" || first == "do" {
                sym.scopes.push(None);
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if first == "for" {
            let vars: Vec<String> = line
                .split_whitespace()
                .skip(1)
                .take_while(|w| *w != "=" && *w != "in")
                .filter(|w| {
                    !w.trim_end_matches(',').is_empty()
                        && w.trim_end_matches(',')
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .map(|w| w.trim_end_matches(',').to_string())
                .collect();
            sym.scopes.push(Some(vars.into_iter().collect()));
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if first == "end" {
            if !sym.scopes.is_empty() {
                sym.scopes.pop();
            }
            out.push_str("end\n");
            continue;
        }

        // -- bind -------------------------------------------------------------
        if first == "bind" {
            emit_bind(line, line_no, sym, out, path, lines, pos)?;
            continue;
        }
        if first == "include" {
            let rest = line["include".len()..].trim();
            if rest.is_empty() {
                return Err(format!(
                    "in file {} at line {line_no}: `include` needs a path",
                    path.display()
                ));
            }
            let path_expr = Rewriter { sym }
                .rewrite_string(rest)
                .map_err(|e| format!("in file {} at line {line_no}: {e}", path.display()))?;
            out.push_str(&format!("include({path_expr})\n"));
            continue;
        }
        if first == "fn" {
            emit_fn(line, line_no, sym, out, path, lines, pos, depth)?;
            continue;
        }
        if first == "script" {
            emit_script(line, line_no, out, path, lines, pos)?;
            continue;
        }
        if first == "on" {
            return Err(format!(
                "in file {} at line {line_no}: `on \"event\" {{ }}` handlers are not implemented yet (Wave roadmap W7)",
                path.display()
            ));
        }

        // -- variable definitions ---------------------------------------------
        // `@name = value` defines a variable. `@` never appears anywhere
        // else; references are `$name` in bare strings, and expressions
        // use the plain identifier `name`.
        if let Some(rest) = line.strip_prefix('@') {
            let Some((name, value)) = rest.split_once('=') else {
                return Err(format!(
                    "in file {} at line {line_no}: expected `@name = value`",
                    path.display()
                ));
            };
            let name = name.trim();
            if name.is_empty() {
                return Err(format!(
                    "in file {} at line {line_no}: `@` needs a variable name before `=`",
                    path.display()
                ));
            }
            let value = value.trim();
            // A statically known value (a single literal token, no
            // parentheses) becomes textually substitutable in strings; an
            // expression value is only reachable as the Lua global.
            let is_static = !value.contains(char::is_whitespace)
                && !value.contains(['(', '['])
                && !value.starts_with('@')
                && !value.starts_with('$');
            if is_static {
                let text = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value)
                    .to_string();
                sym.statics.insert(name.to_string(), text);
            } else {
                sym.runtime_vars.insert(name.to_string());
            }
            let value_expr = rewrite_value(value, sym).map_err(|e| {
                format!(
                    "in file {} at line {line_no}: {e}",
                    path.display()
                )
            })?;
            out.push_str(&format!("_vardef({}, {})\n", lua_quote(name), value_expr));
            continue;
        }

        // -- blocks ------------------------------------------------------------
        if line.ends_with('{') {
            let header = line.strip_suffix('{').unwrap().trim();
            let (keyword, rest) = header
                .split_once(char::is_whitespace)
                .unwrap_or((header, ""));
            if keyword.is_empty() {
                return Err(format!(
                    "in file {} at line {line_no}: expected a block name before `{{`",
                    path.display()
                ));
            }
            if is_reserved(keyword) {
                return Err(format!(
                    "in file {} at line {line_no}: `{keyword}` is a reserved word, not a block name",
                    path.display()
                ));
            }
            let header_expr = if rest.is_empty() {
                lua_quote("")
            } else {
                Rewriter { sym }
                    .rewrite_string(rest)
                    .map_err(|e| format!("in file {} at line {line_no}: {e}", path.display()))?
            };
            out.push_str(&format!("_block({}, {}, function()\n", lua_quote(keyword), header_expr));
            *depth += 1;
            emit_body(lines, pos, sym, out, path, depth)?;
            out.push_str("end)\n");
            continue;
        }

        // -- leaves -------------------------------------------------------------
        if let Some((key, value)) = split_leaf(line) {
            let value_expr = rewrite_value(value.trim(), sym).map_err(|e| {
                format!(
                    "in file {} at line {line_no}: {e}",
                    path.display()
                )
            })?;
            out.push_str(&format!("_leaf({}, {})\n", lua_quote(key), value_expr));
            continue;
        }

        // -- expression statement ------------------------------------------------
        // Must be a call: word followed by `(`.
        let call_end = line.find('(');
        let call_name = call_end
            .map(|i| line[..i].trim())
            .filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.'));
        match call_name {
            Some(name) if !is_reserved(name) => {
                let rewritten = Rewriter { sym }.rewrite(line).map_err(|e| {
                    format!(
                        "in file {} at line {line_no}: {e}",
                        path.display()
                    )
                })?;
                out.push_str(&rewritten);
                out.push('\n');
            }
            _ => {
                return Err(format!(
                    "in file {} at line {line_no}: expected `key = value`, a block ending in `{{`, a statement, or a call -- got `{line}`",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

/// A leaf: `key = value` where key is a bare identifier (not reserved).
fn split_leaf(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty()
        || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || is_reserved(key)
    {
        return None;
    }
    Some((key, value))
}

/// Value rule: whitespace/parens/brackets -> expression, else a single
/// token (literal or bare string). A `$name` token in value position is
/// an error: `$` means string reference, expressions use the identifier.
fn rewrite_value(value: &str, sym: &Symbols) -> Result<String, String> {
    let rewriter = Rewriter { sym };
    if value.contains(char::is_whitespace) || value.contains(['(', '[']) {
        rewriter.rewrite(value)
    } else {
        // single token
        if let Some(name) = value.strip_prefix('@') {
            return Err(format!(
                "`@{name}` only defines a variable on its own line (`@{name} = value`); as a value use `{name}`"
            ));
        }
        if let Some(name) = value.strip_prefix('$') {
            if sym.in_scope(name) {
                return Err(format!(
                    "`${name}` cannot be used as a value; use `{name}` (no `$`)"
                ));
            }
            return Err(format!(
                "`${name}` is not defined; define it with `@{name} = value`, and use `{name}` (no `$`) as a value"
            ));
        }
        if is_number(value) || matches!(value, "true" | "false") {
            return Ok(value.to_string());
        }
        if is_color(value) {
            let hex = value.strip_prefix('#').unwrap();
            return Ok(format!("_color({})", lua_quote(&hex[..6.min(hex.len())])));
        }
        if is_duration(value) {
            let (num, unit) = split_duration(value).unwrap();
            return Ok(format!("_dur({}, {})", num, lua_quote(unit)));
        }
        rewriter.rewrite_string(value)
    }
}

/// Splits a one-line bind body into actions on commas at paren depth 0
/// (outside quotes), so `$wave(kitty, alacritty)` stays one action.
fn split_inline_actions(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_quotes = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '(' | '[' | '{' if !in_quotes => depth += 1,
            ')' | ']' | '}' if !in_quotes => depth -= 1,
            ',' if depth == 0 && !in_quotes => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Rewrites a bind action line: `$` splices go through string position
/// (so `spawn:$wave(a, b)` keeps raw-segment semantics), quoted strings
/// and expressions go through the expression rewriter verbatim, and a
/// bare token is a plain string.
fn rewrite_action(action: &str, sym: &Symbols) -> Result<String, String> {
    let rewriter = Rewriter { sym };
    if action.contains('$') {
        rewriter.rewrite_string(action)
    } else if action.contains(char::is_whitespace) || action.contains(['(', '[']) {
        rewriter.rewrite(action)
    } else {
        rewriter.rewrite_string(action)
    }
}

fn emit_bind(
    line: &str,
    line_no: usize,
    sym: &Symbols,
    out: &mut String,
    path: &Path,
    lines: &[&str],
    pos: &mut usize,
) -> Result<(), String> {
    let rest = line["bind".len()..].trim();
    // inline node form: bind X { a, b }
    if let Some(open) = rest.find('{') {
        let combo_text = rest[..open].trim();
        let after = rest[open + 1..].trim();
        if let Some(close) = after.rfind('}') {
            let actions = split_inline_actions(&after[..close])
                .into_iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| rewrite_action(s, sym))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("in file {} at line {line_no}: {e}", path.display()))?;
            return emit_bind_call(combo_text, actions, sym, out, line_no, path);
        }
        if !after.is_empty() {
            return Err(format!(
                "in file {} at line {line_no}: unclosed bind block, expected `}}` on the `bind` line or one `}}` per line below",
                path.display()
            ));
        }
        // multi-line node form
        if combo_text.is_empty() {
            return Err(format!(
                "in file {} at line {line_no}: `bind` needs a key combo",
                path.display()
            ));
        }
        let mut actions = Vec::new();
        let mut closed = false;
        while *pos < lines.len() {
            let a_no = *pos + 1;
            let a_raw = lines[*pos];
            *pos += 1;
            let a = strip_line_comment(a_raw).trim();
            if a.is_empty() {
                continue;
            }
            if a == "}" {
                closed = true;
                break;
            }
            if a.ends_with('{') || a.contains('=') {
                return Err(format!(
                    "in file {} at line {a_no}: expected an action string inside the bind block, got `{a}`",
                    path.display()
                ));
            }
            actions.push(
                rewrite_action(a, sym)
                    .map_err(|e| format!("in file {} at line {a_no}: {e}", path.display()))?,
            );
        }
        if !closed {
            return Err(format!(
                "in file {} at line {line_no}: unclosed bind block, missing `}}`",
                path.display()
            ));
        }
        return emit_bind_call(combo_text, actions, sym, out, line_no, path);
    }
    // deprecated line form: bind X = rest of line
    if let Some((combo_text, action)) = rest.split_once('=') {
        let combo = Rewriter { sym }
            .rewrite_string(combo_text.trim())
            .map_err(|e| format!("in file {} at line {line_no}: {e}", path.display()))?;
        let action = Rewriter { sym }
            .rewrite_string(action.trim())
            .map_err(|e| format!("in file {} at line {line_no}: {e}", path.display()))?;
        out.push_str(&format!("bind({combo}, {action})\n"));
        return Ok(());
    }
    Err(format!(
        "in file {} at line {line_no}: expected `bind <combo> = <action>` or `bind <combo> {{ ... }}`",
        path.display()
    ))
}

fn emit_bind_call(
    combo_text: &str,
    actions: Vec<String>,
    sym: &Symbols,
    out: &mut String,
    line_no: usize,
    path: &Path,
) -> Result<(), String> {
    if actions.is_empty() {
        return Err(format!(
            "in file {} at line {line_no}: a bind needs at least one action",
            path.display()
        ));
    }
    let combo_expr = Rewriter { sym }
        .rewrite_string(combo_text)
        .map_err(|e| format!("in file {} at line {line_no}: {e}", path.display()))?;
    let list = format!("{{{}}}", actions.join(", "));
    out.push_str(&format!("bind({combo_expr}, {list})\n"));
    Ok(())
}

#[allow(clippy::too_many_arguments)] // the shared surface-parser signature; refactor when W2 wires config.rs in
fn emit_fn(
    line: &str,
    line_no: usize,
    sym: &mut Symbols,
    out: &mut String,
    path: &Path,
    lines: &[&str],
    pos: &mut usize,
    depth: &mut usize,
) -> Result<(), String> {
    let rest = line["fn".len()..].trim();
    let Some(open) = rest.find('(') else {
        return Err(format!(
            "in file {} at line {line_no}: `fn` needs a name and parameter list, e.g. `fn name(a, b) {{`",
            path.display()
        ));
    };
    let name = rest[..open].trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "in file {} at line {line_no}: invalid `fn` name `{name}`",
            path.display()
        ));
    }
    let Some(close) = rest.find(')') else {
        return Err(format!(
            "in file {} at line {line_no}: unclosed parameter list in `fn`",
            path.display()
        ));
    };
    let params: Vec<String> = rest[open + 1..close]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let body = rest[close + 1..].trim();
    if !body.ends_with('{') {
        return Err(format!(
            "in file {} at line {line_no}: `fn` body must be a block, `fn {name}(...) {{`",
            path.display()
        ));
    }
    sym.fns.insert(name.to_string());
    sym.scopes.push(Some(params.into_iter().collect()));
    out.push_str(&format!(
        "local function {name}({}) -- fn\n",
        rest[open + 1..close].trim()
    ));
    *depth += 1;
    emit_body(lines, pos, sym, out, path, depth)?;
    sym.scopes.pop();
    out.push_str("end\n");
    Ok(())
}

fn emit_script(
    line: &str,
    line_no: usize,
    out: &mut String,
    path: &Path,
    lines: &[&str],
    pos: &mut usize,
) -> Result<(), String> {
    let rest = line["script".len()..].trim();
    if rest != "{" {
        return Err(format!(
            "in file {} at line {line_no}: `script` must be followed by `{{` on the same line",
            path.display()
        ));
    }
    let mut body = Vec::new();
    let mut closed = false;
    while *pos < lines.len() {
        let raw = lines[*pos];
        *pos += 1;
        if raw.trim() == "}" {
            closed = true;
            break;
        }
        body.push(raw);
    }
    if !closed {
        return Err(format!(
            "in file {} at line {line_no}: unclosed `script` block, missing `}}`",
            path.display()
        ));
    }
    for l in body {
        out.push_str(l);
        out.push('\n');
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Evaluate: run the compiled chunk against the registration environment
// ---------------------------------------------------------------------------

fn serialize_value(value: Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.to_string_lossy()),
        Value::Integer(i) => Ok(i.to_string()),
        Value::Number(f) => Ok(format!("{f}")),
        Value::Boolean(b) => Ok(b.to_string()),
        Value::Table(t) => {
            let mut items = Vec::new();
            for v in t.sequence_values() {
                let v = v.map_err(|e| e.to_string())?;
                items.push(match &v {
                    Value::String(s) => lua_quote(&s.to_string_lossy()),
                    _ => serialize_value(v)?,
                });
            }
            Ok(format!("[{}]", items.join(", ")))
        }
        other => Err(format!(
            "unsupported config value type: {}",
            other.type_name()
        )),
    }
}

/// Does `candidate` resolve to an executable file, directly or via `$PATH`?
fn path_has_exec(candidate: &str) -> bool {
    let check = |p: &Path| p.is_file() && {
        use std::os::unix::fs::PermissionsExt;
        p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    };
    if candidate.contains('/') {
        return check(Path::new(candidate));
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| check(&dir.join(candidate)))
}

/// One capture sink holds the entries produced while a `_block` builder
/// runs; the stack holds one sink per open block.
type EntrySink = Rc<RefCell<Vec<Entry>>>;
type EntryStack = Rc<RefCell<Vec<EntrySink>>>;

/// The entry-sink stack: one sink per open `_block` capture, topmost is
/// the current one. A plain function so every environment closure can
/// call it without sharing a captured closure.
fn top(stack: &EntryStack) -> EntrySink {
    stack.borrow().last().unwrap().clone()
}

/// Compiles and evaluates a Wave file, returning the same [`Entry`] list
/// the line-based parser produces.
pub(crate) fn evaluate(source: &str, path: &Path) -> Result<Vec<Entry>, String> {
    let lua_source = compile(source, path)?;
    // Sandboxed from creation: only math/string/table, no io/os/package.
    let lua = Lua::new_with(
        StdLib::MATH | StdLib::STRING | StdLib::TABLE,
        mlua::LuaOptions::default(),
    )
    .map_err(|e| format!("in file {}: failed to create Lua state: {e}", path.display()))?;

    let stack: EntryStack = Rc::new(RefCell::new(vec![Rc::new(RefCell::new(Vec::new()))]));

    let s1 = stack.clone();
    lua.globals()
        .set(
            "_leaf",
            lua.create_function(move |_, (key, value): (String, Value)| {
                let s = serialize_value(value).map_err(mlua::Error::external)?;
                top(&s1).borrow_mut().push(Entry::Assign(key, s));
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let s2 = stack.clone();
    lua.globals()
        .set(
            "_vardef",
            lua.create_function(move |lua, (name, value): (String, Value)| {
                lua.globals().set(name.clone(), value.clone())?;
                let s = serialize_value(value).map_err(mlua::Error::external)?;
                top(&s2).borrow_mut().push(Entry::VarDef(name, s));
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let s3 = stack.clone();
    lua.globals()
        .set(
            "_block",
            lua.create_function(move |_, (keyword, header, builder): (String, String, Function)| {
                let inner = Rc::new(RefCell::new(Vec::new()));
                s3.borrow_mut().push(inner.clone());
                builder.call::<()>(())?;
                s3.borrow_mut().pop();
                let body = inner.borrow().clone();
                top(&s3).borrow_mut().push(Entry::Block(keyword, header, body));
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let s4 = stack.clone();
    lua.globals()
        .set(
            "bind",
            lua.create_function(move |_, (combo, action): (String, Value)| {
                let sink = top(&s4);
                let mut sink = sink.borrow_mut();
                match action {
                    Value::String(s) => sink.push(Entry::Bind(combo, s.to_string_lossy())),
                    Value::Table(t) => {
                        for v in t.sequence_values() {
                            let v = v?;
                            let Value::String(s) = v else {
                                return Err(mlua::Error::external(
                                    "bind actions must be strings",
                                ));
                            };
                            sink.push(Entry::Bind(combo.clone(), s.to_string_lossy()));
                        }
                    }
                    _ => {
                        return Err(mlua::Error::external(
                            "bind action must be a string or a list of strings",
                        ))
                    }
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let s5 = stack.clone();
    lua.globals()
        .set(
            "include",
            lua.create_function(move |_, path: String| {
                top(&s5).borrow_mut().push(Entry::Include(path));
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    lua.globals()
        .set(
            "wave",
            lua.create_function(move |_, candidates: Variadic<String>| {
                if candidates.is_empty() {
                    return Err(mlua::Error::external(
                        "wave(...) needs at least one candidate",
                    ));
                }
                let mut found = None;
                for c in candidates.iter() {
                    if path_has_exec(c) {
                        found = Some(c.clone());
                        break;
                    }
                }
                Ok(found.unwrap_or_else(|| candidates.last().cloned().unwrap()))
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    lua.globals()
        .set(
            "_color",
            lua.create_function(|_, hex: String| Ok(hex.chars().take(6).collect::<String>()))
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    lua.globals()
        .set(
            "_dur",
            lua.create_function(|_, (n, unit): (f64, String)| {
                let n = if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{n:.0}")
                } else {
                    format!("{n}")
                };
                Ok(format!("{n}{unit}"))
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    lua.globals()
        .set("tide", lua.create_table().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;

    let chunk = lua.load(&lua_source).set_name(path.display().to_string());
    chunk.exec().map_err(|e| format!("in file {}: {e}", path.display()))?;

    let entries = top(&stack).borrow().clone();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn compile_str(s: &str) -> String {
        compile(s, Path::new("test.wave")).expect("compile should succeed")
    }

    #[test]
    fn leaf_bare_token_and_quoted() {
        let lua = compile_str("terminal = kitty\ntitle = \"Firefox # 1\"\n");
        assert_eq!(
            lua,
            "-- wave: compiled surface (generated; do not edit)\n_leaf(\"terminal\", \"kitty\")\n_leaf(\"title\", \"Firefox # 1\")\n"
        );
    }

    #[test]
    fn typed_literals_color_duration_number_bool() {
        let lua = compile_str("color = #8EDDFF\nduration = 600ms\nslow = 1.5s\ngaps = 8\nwater_effects = true\n");
        assert!(lua.contains("_leaf(\"color\", _color(\"8EDDFF\"))"));
        assert!(lua.contains("_leaf(\"duration\", _dur(600, \"ms\"))"));
        assert!(lua.contains("_leaf(\"slow\", _dur(1.5, \"s\"))"));
        assert!(lua.contains("_leaf(\"gaps\", 8)"));
        assert!(lua.contains("_leaf(\"water_effects\", true)"));
    }

    #[test]
    fn hash_is_comment_unless_color() {
        let lua = compile_str("# a comment\ncolor = #8EDDFF # trailing\n");
        assert_eq!(lua, "-- wave: compiled surface (generated; do not edit)\n_leaf(\"color\", _color(\"8EDDFF\"))\n");
    }

    #[test]
    fn dash_dash_and_block_comments() {
        let lua = compile_str("gaps = 8 -- a comment\n--[[ block\ncomment ]]terminal = kitty\n");
        assert!(lua.contains("_leaf(\"gaps\", 8)"));
        assert!(lua.contains("_leaf(\"terminal\", \"kitty\")"));
    }

    #[test]
    fn operators_need_spaces_expression() {
        let lua = compile_str("gaps = 8 * 2\n");
        assert!(lua.contains("_leaf(\"gaps\", 8 * 2)"));
        let lua = compile_str("spawn = kitty + 1\n");
        assert!(lua.contains("_leaf(\"spawn\", \"kitty\" + 1)"));
    }

    #[test]
    fn single_token_bare_string_with_plus() {
        let lua = compile_str("chord = SUPER+Return\n");
        assert!(lua.contains("_leaf(\"chord\", \"SUPER+Return\")"));
    }

    #[test]
    fn dotted_token_unknown_base_is_string() {
        let lua = compile_str("app_id = org.kde.konsole\n");
        assert!(lua.contains("_leaf(\"app_id\", \"org.kde.konsole\")"));
    }

    #[test]
    fn wave_call_bare_words_become_strings() {
        let lua = compile_str("terminal = wave(kitty, alacritty, foot)\n");
        assert!(lua.contains("_leaf(\"terminal\", wave(\"kitty\", \"alacritty\", \"foot\"))"));
    }

    #[test]
    fn var_def_and_use_in_string_and_expression() {
        // @ defines, $ references in strings, plain name in expressions
        let lua = compile_str("@mod = SUPER\nbind $mod+Return { spawn:kitty }\n");
        assert!(lua.contains("_vardef(\"mod\", \"SUPER\")"));
        assert!(lua.contains("bind(\"SUPER+Return\", {\"spawn:kitty\"})"));
        let lua = compile_str("@extra = 4\ngaps = 8 * extra\n");
        assert!(lua.contains("_leaf(\"gaps\", 8 * extra)"));
        // runtime variable: reachable as the identifier in expressions
        let lua = compile_str("@mod = SUPER\n@terminal = wave(kitty, sh)\nbind $mod+Return { spawn:$terminal }\n");
        assert!(lua.contains("_vardef(\"terminal\", wave(\"kitty\", \"sh\"))"));
        assert!(lua.contains("bind(\"SUPER+Return\", {\"spawn:\" .. terminal})"));
    }

    #[test]
    fn markers_have_one_role_each() {
        // `$` in an expression is an error pointing at the identifier
        let err = compile("@extra = 4\ngaps = 8 * $extra\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("use `extra` (no `$`)"), "{err}");
        // `$` as a bare value is an error
        let err = compile("@extra = 4\ngaps = $extra\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("use `extra` (no `$`)"), "{err}");
        // an undefined `$name` is an error, never silent text
        let err = compile("bind $mod+Q { close-window }\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("`$mod` is not defined"), "{err}");
        // `@` in an expression is an error
        let err = compile("gaps = 8 * @extra\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("only defines a variable"), "{err}");
        // `@` as a bare value is an error
        let err = compile("gaps = @extra\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("only defines a variable"), "{err}");
        // quoted strings are literal: no substitution, no errors
        let lua = compile_str("@mod = SUPER\nbind \"$mod+Q\" { close-window }\n");
        assert!(lua.contains("bind(\"$mod+Q\", {\"close-window\"})"));
        let lua = compile_str("spawn = \"sh -c 'echo $HOME'\"\n");
        assert!(lua.contains("_leaf(\"spawn\", \"sh -c 'echo $HOME'\")"));
    }

    #[test]
    fn block_and_nested_block() {
        let lua = compile_str("border {\n    width = 2\n    gradient = [theme.primary, theme.deep]\n}\n");
        assert!(lua.contains("_block(\"border\", \"\", function()"));
        assert!(lua.contains("_leaf(\"width\", 2)"));
        assert!(lua.contains("_leaf(\"gradient\", [\"theme.primary\", \"theme.deep\"])"));
        assert!(lua.contains("end)"));
    }

    #[test]
    fn block_header() {
        let lua = compile_str("output eDP-1 {\n    scale = 1.0\n}\n");
        assert!(lua.contains("_block(\"output\", \"eDP-1\", function()"));
    }

    #[test]
    fn bind_multiline_one_line_and_line_form() {
        let lua = compile_str("@mod = SUPER\nbind $mod+Q {\n    close-window\n}\n");
        assert!(lua.contains("bind(\"SUPER+Q\", {\"close-window\"})"));
        let lua = compile_str("@mod = SUPER\nbind $mod+D { \"spawn:rofi -show drun\" }\n");
        assert!(lua.contains("bind(\"SUPER+D\", {\"spawn:rofi -show drun\"})"));
        let lua = compile_str("@mod = SUPER\nbind $mod+R = spawn:rofi -show drun\n");
        assert!(lua.contains("bind(\"SUPER+R\", \"spawn:rofi -show drun\")"));
    }

    #[test]
    fn bind_one_liner_multiple_actions() {
        let lua = compile_str("@mod = SUPER\nbind $mod+T { close-window, toggle-floating }\n");
        assert!(lua.contains("bind(\"SUPER+T\", {\"close-window\", \"toggle-floating\"})"));
    }

    #[test]
    fn wave_splice_inside_string() {
        let lua = compile_str("@mod = SUPER\nbind $mod+Return { spawn:$wave(kitty, alacritty) }\n");
        assert!(lua.contains("bind(\"SUPER+Return\", {\"spawn:\" .. wave(\"kitty\", \"alacritty\")})"));
        let lua = compile_str("@mod = SUPER\nbind $mod+Return { \"spawn:\" .. wave(\"kitty\") }\n");
        assert!(lua.contains("bind(\"SUPER+Return\", {\"spawn:\" .. wave(\"kitty\")})"));
    }

    #[test]
    fn loop_with_var_concat() {
        let lua = compile_str(
            "@mod = SUPER\nfor i = 1, 9 do\n    bind $mod+Num$i { workspace:$i }\nend\n",
        );
        assert!(lua.contains("for i = 1, 9 do"));
        assert!(lua.contains("bind(\"SUPER+Num\" .. i, {\"workspace:\" .. i})"));
        assert!(lua.contains("end"));
    }

    #[test]
    fn fn_macro_and_call() {
        let lua = compile_str(
            "@mod = SUPER\nfn media(key, app) {\n    bind $mod+$key { spawn:$app }\n}\nmedia(comma, spotify)\n",
        );
        assert!(lua.contains("local function media(key, app)"));
        assert!(lua.contains("bind(\"SUPER+\" .. key, {\"spawn:\" .. app})"));
        assert!(lua.contains("media(\"comma\", \"spotify\")"));
    }

    #[test]
    fn if_passthrough_with_transpiled_body() {
        let lua = compile_str(
            "if tide.backend == \"udev\" then\n    udev {\n        disable_overlay_planes = true\n    }\nend\n",
        );
        assert!(lua.contains("if tide.backend == \"udev\" then"));
        assert!(lua.contains("_block(\"udev\", \"\", function()"));
        assert!(lua.contains("_leaf(\"disable_overlay_planes\", true)"));
    }

    #[test]
    fn script_raw_passthrough() {
        let lua = compile_str("script {\n    local n = 1\n    while n <= 3 do\n        bind(\"$mod+F\" .. n, \"spawn:app\" .. n)\n        n = n + 1\n    end\n}\n");
        assert!(lua.contains("local n = 1"));
        assert!(lua.contains("bind(\"$mod+F\" .. n, \"spawn:app\" .. n)"));
    }

    #[test]
    fn include_statement() {
        let lua = compile_str("include \"keybinds.wave\"\n");
        assert!(lua.contains("include(\"keybinds.wave\")"));
    }

    #[test]
    fn reserved_or_garbage_lines_error() {
        let err = compile("this is not valid\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("line 1"));
        let err = compile("on \"x\" {}\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("W7"));
        let err = compile("}\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("unexpected `}`"));
    }

    #[test]
    fn evaluate_produces_entries() {
        let entries = evaluate(
            "@mod = SUPER\nterminal = kitty\nborder {\n    width = 2\n}\nbind $mod+Q { close-window }\n",
            Path::new("test.wave"),
        )
        .expect("evaluate should succeed");
        assert_eq!(
            entries,
            vec![
                Entry::VarDef("mod".into(), "SUPER".into()),
                Entry::Assign("terminal".into(), "kitty".into()),
                Entry::Block(
                    "border".into(),
                    "".into(),
                    vec![Entry::Assign("width".into(), "2".into())]
                ),
                Entry::Bind("SUPER+Q".into(), "close-window".into()),
            ]
        );
    }

    #[test]
    fn evaluate_color_duration_and_wave() {
        let entries = evaluate(
            "color = #8EDDFF\nduration = 600ms\nterminal = wave(no-such-binary-xyz, sh)\n",
            Path::new("test.wave"),
        )
        .expect("evaluate should succeed");
        assert_eq!(entries[0], Entry::Assign("color".into(), "8EDDFF".into()));
        assert_eq!(entries[1], Entry::Assign("duration".into(), "600ms".into()));
        // "sh" always exists, "no-such-binary-xyz" never does: wave() picks sh
        assert_eq!(entries[2], Entry::Assign("terminal".into(), "sh".into()));
    }

    /// The W1 equivalence gate: the same config written in the old
    /// line-based syntax and the new Wave syntax must produce the same
    /// `Entry` list. Literal combos are used so the comparison is about
    /// grammar fidelity alone; `$name` substitution into combos is
    /// config.rs integration work (W2/W3).
    #[test]
    fn equivalent_to_line_based_parser_on_shared_sample() {
        let old = r#"terminal = kitty
gaps = 8
water_effects = true
color = 8EDDFF
input {
    xkb_layout = us
    touchpad {
        natural_scroll = true
    }
}
bind SUPER+Return = spawn:kitty
spawn_at_startup = waybar
"#;
        let new = r#"terminal = kitty
gaps = 8
water_effects = true
color = #8EDDFF
input {
    xkb_layout = us
    touchpad {
        natural_scroll = true
    }
}
bind SUPER+Return { spawn:kitty }
spawn_at_startup = waybar
"#;
        let old_entries = super::super::waves::parse(old, Path::new("old.wave"))
            .expect("old syntax should parse");
        let new_entries = evaluate(new, Path::new("new.wave")).expect("new syntax should evaluate");
        assert_eq!(old_entries, new_entries);
    }
}

