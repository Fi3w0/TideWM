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
//! This is the W1/W2 slice of the rewrite: the grammar core and the
//! config-loading integration. `on "event"` handlers (W7), section
//! globals so `theme.primary` reads as an expression (W4), and duration
//! math (`600ms * 2`, W4's typed values) are deliberately not here yet.

use std::cell::{Cell, RefCell};

use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{FromLua, Function, Lua, StdLib, Value, Variadic};

use super::wave_fmt::{strip_block_comments, strip_line_comment};

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
    /// Block keywords seen so far: `theme { }` makes `theme` a Lua
    /// global table, so `theme.primary` reads as an expression.
    block_globals: std::collections::HashSet<String>,
    /// Leaf keys of the currently open blocks, innermost last. A key in
    /// the innermost set resolves to `_field("key")` (the sibling value
    /// on the block's body table), which is what makes
    /// `deep = primary.darken(0.35)` work inside a `theme { }` block.
    body_fields: Vec<std::collections::HashSet<String>>,
    /// Collect-mode emission: `$name` in strings is left verbatim instead
    /// of erroring, so the resolve pre-pass can evaluate files whose
    /// definitions live in other files. Never used for real configs.
    lenient: bool,
    /// Eval mode: bare words stay identifiers even when nothing is known
    /// about them, and `$`/`@` are errors -- the session Lua decides what
    /// a name is at eval time (`tidectl eval "mod"` answers the `@mod`
    /// global, `theme.primary` answers the section table).
    eval_mode: bool,
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

    /// Is `name` a leaf of the innermost open block? (Sibling access
    /// only: `_field` reads the innermost body table.)
    fn in_innermost_body(&self, name: &str) -> bool {
        self.body_fields.last().is_some_and(|f| f.contains(name))
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
        // pointing at the identifier form (unless collecting, where
        // everything stays literal text).
        if let Some(name) = word.strip_prefix('@') {
            if self.sym.eval_mode {
                return Err(format!(
                    "`@{name}` is a config definition marker; in eval use `{name}`"
                ));
            }
            if self.sym.lenient {
                return Ok(lua_quote(word));
            }
            return Err(format!(
                "`@{name}` only defines a variable on its own line (`@{name} = value`); in an expression use `{name}`"
            ));
        }
        if let Some(name) = word.strip_prefix('$') {
            if self.sym.eval_mode {
                return Err(format!(
                    "`${name}` is a config string reference; in eval use `{name}`"
                ));
            }
            if name == "wave" {
                if self.sym.lenient {
                    return Ok(lua_quote(word));
                }
                return Err(
                    "`$wave(...)` is for strings; in an expression call `wave(...)`".to_string(),
                );
            }
            if self.sym.lenient {
                return Ok(lua_quote(word));
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
        if self.sym.eval_mode {
            // The session Lua decides: unknown names resolve or fail there.
            return Ok(word.to_string());
        }
        if let Some((base, rest)) = word.split_once('.') {
            if self.sym.in_innermost_body(base) {
                // sibling field access: `primary.darken(0.35)` inside a
                // `theme { }` block resolves through the body table
                return Ok(format!("_field({}).{rest}", lua_quote(base)));
            }
            if self.sym.block_globals.contains(base) {
                return Ok(word.to_string()); // section global: theme.primary
            }
            if self.sym.in_scope(base) {
                return Ok(word.to_string()); // member access on a known identifier
            }
        }
        // A statically known @variable folds to its literal value; a
        // runtime variable, fn name, loop var, or fn param stays the
        // identifier; a sibling block field resolves through _field.
        if let Some(text) = self.sym.statics.get(word) {
            return Ok(classify_literal(text));
        }
        if self.sym.in_innermost_body(word) {
            return Ok(format!("_field({})", lua_quote(word)));
        }
        if self.sym.in_scope(word) {
            return Ok(word.to_string());
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
            let text = if is_operator(tok) || is_quoted_string(tok) {
                tok.clone()
            } else if is_call_name {
                // A call name may be a method on a sibling block field
                // (`primary.darken(...)` inside a `theme { }` body). The
                // colon form is required: Lua only prepends `self` for
                // metatable-provided methods with `a:b(...)`, plain
                // `a.b(...)` calls with one argument.
                if let Some((base, rest)) = tok.split_once('.') {
                    if self.sym.in_innermost_body(base) {
                        format!("_field({}):{rest}", lua_quote(base))
                    } else {
                        tok.clone()
                    }
                } else {
                    tok.clone()
                }
            } else {
                self.classify_word(tok)?
            };
            if need_space && !matches!(tok.as_str(), "(" | "[" | "{" | ")" | "]" | "}" | ",") {
                out.push(' ');
            }
            // Surface `[...]` lists are Lua `{...}` table constructors:
            // Lua has no bracket array literal syntax.
            match tok.as_str() {
                "[" => out.push('{'),
                "]" => out.push('}'),
                _ => out.push_str(&text),
            }
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
                        if self.sym.lenient {
                            lit.push('$');
                            lit.push_str(name);
                        } else {
                            return Err(format!(
                                "`${name}` is not defined; define it with `@{name} = value`, or quote the string if you mean literal text"
                            ));
                        }
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

/// Re-classifies a statically known variable's text as a Lua literal:
/// numbers, booleans, colors, and durations stay typed; anything else is
/// a quoted string. This is what makes `@mod = SUPER` usable as
/// `pointer_modifier = mod` in an expression.
fn classify_literal(text: &str) -> String {
    if is_number(text) || matches!(text, "true" | "false") {
        text.to_string()
    } else if is_color(text) {
        let hex = text.strip_prefix('#').unwrap();
        format!("_color({})", lua_quote(&hex[..6.min(hex.len())]))
    } else if is_duration(text) {
        let (num, unit) = split_duration(text).unwrap();
        format!("_dur({}, {})", num, lua_quote(unit))
    } else {
        lua_quote(text)
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
#[allow(dead_code)] // exercised by tests; W6's wavefmt and W7's tidectl eval consume it
pub(crate) fn compile(source: &str, path: &Path) -> Result<String, String> {
    compile_with(source, path, &mut Symbols::default())
}

/// `compile` with a caller-provided symbol table, so the resolve pass can
/// make `@name` definitions from other files visible to this emitter.
fn compile_with(
    source: &str,
    path: &Path,
    sym: &mut Symbols,
) -> Result<String, String> {
    let pre = strip_block_comments(source);
    let lines: Vec<&str> = pre.lines().collect();
    let mut pos = 0usize;
    let mut out = String::from("-- wave: compiled surface (generated; do not edit)\n");
    let mut depth = 0usize;
    emit_body(&lines, &mut pos, sym, &mut out, path, &mut depth)?;
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
            emit_on(line, line_no, sym, out, path, lines, pos, depth)?;
            continue;
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
        // An empty one-liner, `vessels { }` / `output eDP-1 { }`: the
        // natural way to say "all defaults". Unambiguous (nothing between
        // the braces), so it is the one exception to multi-line blocks
        // alongside bind.
        if line.ends_with("}") {
            if let Some(head) = line.strip_suffix('}').map(str::trim_end) {
                if let Some(open) = head.strip_suffix('{').map(str::trim_end) {
                    let (keyword, rest) = open
                        .split_once(char::is_whitespace)
                        .unwrap_or((open, ""));
                    if !keyword.is_empty() && !is_reserved(keyword) {
                        let header_expr = if rest.is_empty() {
                            lua_quote("")
                        } else {
                            Rewriter { sym }
                                .rewrite_string(rest)
                                .map_err(|e| format!("in file {} at line {line_no}: {e}", path.display()))?
                        };
                        sym.block_globals.insert(keyword.to_string());
                        sym.body_fields.push(std::collections::HashSet::new());
                        out.push_str(&format!(
                            "_block({}, {}, function()\nend)\n",
                            lua_quote(keyword),
                            header_expr
                        ));
                        sym.body_fields.pop();
                        continue;
                    }
                }
            }
        }
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
            // Section globals + sibling field access: the block keyword
            // becomes a Lua global table, its leaf keys become readable
            // inside the body via _field.
            sym.block_globals.insert(keyword.to_string());
            sym.body_fields.push(std::collections::HashSet::new());
            out.push_str(&format!("_block({}, {}, function()\n", lua_quote(keyword), header_expr));
            *depth += 1;
            emit_body(lines, pos, sym, out, path, depth)?;
            sym.body_fields.pop();
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
            if let Some(fields) = sym.body_fields.last_mut() {
                fields.insert(key.to_string());
            }
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
            if sym.lenient {
                return Ok(lua_quote(value));
            }
            return Err(format!(
                "`@{name}` only defines a variable on its own line (`@{name} = value`); as a value use `{name}`"
            ));
        }
        if let Some(name) = value.strip_prefix('$') {
            if sym.lenient {
                return Ok(lua_quote(value));
            }
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
        if let Some(text) = sym.statics.get(value) {
            return Ok(classify_literal(text));
        }
        if let Some((base, _)) = value.split_once('.') {
            if sym.in_innermost_body(base) {
                return Ok(format!("_field({}).{}", lua_quote(base), &value[base.len() + 1..]));
            }
            if sym.block_globals.contains(base) {
                return Ok(value.to_string());
            }
        }
        if sym.in_innermost_body(value) {
            return Ok(format!("_field({})", lua_quote(value)));
        }
        if sym.eval_mode {
            return Ok(value.to_string());
        }
        if sym.in_scope(value) {
            // a defined @variable, fn name, loop var, or fn param: the
            // identifier, not the string of its name
            return Ok(value.to_string());
        }
        rewriter.rewrite_string(value)
    }
}

/// Compiles a bare Wave expression for `tidectl eval`: surface literals
/// (durations, colors, lists) and operators work, `@`/`$` markers error
/// with a hint, and any other name stays an identifier for the session
/// Lua to resolve (`mod`, `theme.primary`, `tide.backend`).
pub(crate) fn compile_eval_expression(expr: &str) -> Result<String, String> {
    let sym = Symbols {
        eval_mode: true,
        ..Default::default()
    };
    rewrite_value(expr, &sym)
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
    Err(format!(
        "in file {} at line {line_no}: expected `bind <combo> {{ ... }}`",
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
fn emit_fn(    line: &str,
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

/// `on "event" { ... }`: the body is transpiled as a Lua function, its
/// source is registered via `_on` (which compiles it on the session Lua,
/// stores it in `_handlers`, and records an `Entry::Handler`), so the
/// handler runs when the compositor emits that event.
#[allow(clippy::too_many_arguments)] // the shared surface-parser signature
fn emit_on(
    line: &str,
    line_no: usize,
    sym: &mut Symbols,
    out: &mut String,
    path: &Path,
    lines: &[&str],
    pos: &mut usize,
    depth: &mut usize,
) -> Result<(), String> {
    let rest = line["on".len()..].trim();
    let Some(open) = rest.find('{') else {
        return Err(format!(
            "in file {} at line {line_no}: `on` needs an event name and a block, e.g. `on \"workspace-changed\" {{`",
            path.display()
        ));
    };
    let event = rest[..open].trim();
    let event = event
        .strip_prefix('"')
        .and_then(|e| e.strip_suffix('"'))
        .unwrap_or(event);
    if event.is_empty() {
        return Err(format!(
            "in file {} at line {line_no}: `on` needs an event name",
            path.display()
        ));
    }
    let mut body = String::new();
    *depth += 1;
    emit_body(lines, pos, sym, &mut body, path, depth)?;
    let source = format!("function()\n{body}end");
    out.push_str(&format!("_on({}, {})\n", lua_quote(event), lua_quote(&source)));
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
        Value::UserData(ud) => {
            if let Ok(d) = ud.borrow::<DurationValue>() {
                return Ok(d.serialize());
            }
            if let Ok(c) = ud.borrow::<ColorValue>() {
                return Ok(c.serialize());
            }
            Err(format!("unsupported config value type: {}", ud.type_name().map(|s| s.to_string_lossy()).unwrap_or_else(|_| "?".to_string())))
        }
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

/// A duration literal (`600ms`, `1.5s`, `90m`) as a Lua value with real
/// arithmetic: `600ms * 2` computes, and serializes back to `1200ms`.
/// Internal storage is milliseconds; the literal's own unit is kept so a
/// value that never left its unit serializes as the user wrote it.
#[derive(Clone, Copy, Debug)]
struct DurationValue {
    ms: f64,
    unit: &'static str,
}

impl DurationValue {
    fn serialize(&self) -> String {
        let scaled = match self.unit {
            "ms" => self.ms,
            "s" => self.ms / 1000.0,
            "m" => self.ms / 60_000.0,
            _ => self.ms,
        };
        if scaled.fract() == 0.0 && scaled.abs() < 1e15 {
            format!("{scaled:.0}{}", self.unit)
        } else {
            format!("{scaled}{}", self.unit)
        }
    }
}

impl mlua::FromLua for DurationValue {
    fn from_lua(value: mlua::Value, _lua: &Lua) -> mlua::Result<Self> {
        match value {
            mlua::Value::UserData(ud) => ud.borrow::<DurationValue>().map(|d| *d),
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "DurationValue".to_string(),
                message: Some("expected a duration literal such as `600ms`".to_string()),
            }),
        }
    }
}

impl mlua::UserData for DurationValue {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::Add, |_, a, b: DurationValue| {
            Ok(DurationValue {
                ms: a.ms + b.ms,
                unit: a.unit,
            })
        });
        methods.add_meta_method(mlua::MetaMethod::Sub, |_, a, b: DurationValue| {
            Ok(DurationValue {
                ms: a.ms - b.ms,
                unit: a.unit,
            })
        });
        // `2 * 600ms` arrives with the operands swapped (Lua tries the
        // number's metamethod first, then the userdata's with the
        // operands in original order), so Mul is a meta FUNCTION that
        // handles either side; Add/Sub/Unm always have the duration on
        // the left.
        methods.add_meta_function(
            mlua::MetaMethod::Mul,
            |lua, (a, b): (Value, Value)| {
                if let Ok(d) = DurationValue::from_lua(a.clone(), lua) {
                    let n = f64::from_lua(b, lua)?;
                    return Ok(DurationValue {
                        ms: d.ms * n,
                        unit: d.unit,
                    });
                }
                let d = DurationValue::from_lua(b, lua)?;
                let n = f64::from_lua(a, lua)?;
                Ok(DurationValue {
                    ms: d.ms * n,
                    unit: d.unit,
                })
            },
        );
        methods.add_meta_method(mlua::MetaMethod::Div, |_, a, b: f64| {
            Ok(DurationValue {
                ms: a.ms / b,
                unit: a.unit,
            })
        });
        methods.add_meta_method(mlua::MetaMethod::Unm, |_, a, ()| {
            Ok(DurationValue {
                ms: -a.ms,
                unit: a.unit,
            })
        });
        methods.add_meta_method(mlua::MetaMethod::Eq, |_, a, b: DurationValue| {
            Ok(a.ms == b.ms)
        });
    }
}

/// A color literal as a Lua value with palette math:
/// `primary.darken(0.35)` / `primary.lighten(0.15)` / `alpha(a)`.
/// Serializes back to the bare `RRGGBB` hex the config surface parses.
#[derive(Clone, Copy, Debug)]
struct ColorValue {
    r: f64,
    g: f64,
    b: f64,
}

impl ColorValue {
    fn from_hex(hex: &str) -> Option<Self> {
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        if hex.len() != 6 {
            return None;
        }
        Some(ColorValue {
            r: u8::from_str_radix(&hex[0..2], 16).ok()? as f64,
            g: u8::from_str_radix(&hex[2..4], 16).ok()? as f64,
            b: u8::from_str_radix(&hex[4..6], 16).ok()? as f64,
        })
    }

    fn serialize(&self) -> String {
        format!(
            "{:02X}{:02X}{:02X}",
            self.r.round().clamp(0.0, 255.0) as u8,
            self.g.round().clamp(0.0, 255.0) as u8,
            self.b.round().clamp(0.0, 255.0) as u8
        )
    }
}

impl mlua::FromLua for ColorValue {
    fn from_lua(value: mlua::Value, _lua: &Lua) -> mlua::Result<Self> {
        match value {
            mlua::Value::UserData(ud) => ud.borrow::<ColorValue>().map(|c| *c),
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "ColorValue".to_string(),
                message: Some("expected a color literal such as `#8EDDFF`".to_string()),
            }),
        }
    }
}

impl mlua::UserData for ColorValue {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        // Methods are registered as plain functions taking the receiver
        // explicitly: mlua's `add_method` receiver conversion needs the
        // `macros` feature's generated impls, which this build does not
        // enable.
        methods.add_function("darken", |_, (self_v, amount): (mlua::AnyUserData, f64)| {
            let color = self_v.borrow::<ColorValue>()?;
            let t = amount.clamp(0.0, 1.0);
            Ok(ColorValue {
                r: color.r * (1.0 - t),
                g: color.g * (1.0 - t),
                b: color.b * (1.0 - t),
            })
        });
        methods.add_function("lighten", |_, (self_v, amount): (mlua::AnyUserData, f64)| {
            let color = self_v.borrow::<ColorValue>()?;
            let t = amount.clamp(0.0, 1.0);
            Ok(ColorValue {
                r: color.r + (255.0 - color.r) * t,
                g: color.g + (255.0 - color.g) * t,
                b: color.b + (255.0 - color.b) * t,
            })
        });
        // Accepted and dropped, matching the config surface's current
        // "alpha in the color form is ignored" behavior.
        methods.add_function("alpha", |_, (self_v, _amount): (mlua::AnyUserData, f64)| {
            self_v.borrow::<ColorValue>().map(|c| *c)
        });
        methods.add_function("with_alpha", |_, (self_v, _amount): (mlua::AnyUserData, f64)| {
            self_v.borrow::<ColorValue>().map(|c| *c)
        });
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

/// Installs the registration environment into `lua`. All closures share
/// the entry-sink `stack` and the `collect` flag: in collect mode (the
/// resolve pre-pass) only `_vardef` has an effect, so `@name = value`
/// definitions from every reachable file land in `statics_out` without
/// producing entries.
///
/// The `bodies` stack tracks the Lua table each open `_block` builds:
/// `_leaf` writes raw values into the innermost body, `_field` reads
/// them back (sibling access like `primary.darken(0.35)` inside a
/// `theme { }` block), and `_block` exposes the finished body as the
/// block's global (`theme.primary` outside the block).
/// The live-compositor facts exposed to config evaluation as the `tide`
/// table: hardware conditionals (`if tide.backend == "udev" and
/// tide.gpu.vendor == "nvidia"`), output-aware values, and the current
/// workspace for event handlers and `tidectl eval`.
#[derive(Debug, Clone, Default)]
pub(crate) struct TideInfo {
    pub backend: &'static str,
    pub gpu_vendor: &'static str,
    /// (connector name, width, height) of the connected outputs.
    pub outputs: Vec<(String, u32, u32)>,
    /// The active workspace on the first output (0 when not meaningful).
    pub workspace: i64,
}

/// The GPU vendor from sysfs: the first DRM card's PCI vendor id, mapped
/// to a stable lowercase name for `tide.gpu.vendor`. `"unknown"` when
/// there is no DRM card to read (the nested winit backend).
pub(crate) fn detect_gpu_vendor() -> &'static str {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return "unknown";
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") || name.len() != 5 {
            continue;
        }
        let Ok(vendor) = std::fs::read_to_string(entry.path().join("device/vendor")) else {
            continue;
        };
        return match vendor.trim().to_lowercase().as_str() {
            "0x10de" => "nvidia",
            "0x1002" | "0x1022" => "amd",
            "0x8086" => "intel",
            _ => "unknown",
        };
    }
    "unknown"
}

fn install_env(
    lua: &Lua,
    stack: &EntryStack,
    collect: Rc<Cell<bool>>,
    statics_out: Rc<RefCell<std::collections::HashMap<String, String>>>,
    bodies: Rc<RefCell<Vec<mlua::Table>>>,
    tide: &TideInfo,
) -> Result<Rc<Cell<bool>>, String> {
    let c1 = collect.clone();
    let s1 = stack.clone();
    let b1 = bodies.clone();
    lua.globals()
        .set(
            "_leaf",
            lua.create_function(move |_, (key, value): (String, Value)| {
                if let Some(body) = b1.borrow().last() {
                    let _ = body.set(key.as_str(), value.clone());
                }
                if !c1.get() {
                    let s = serialize_value(value).map_err(mlua::Error::external)?;
                    top(&s1).borrow_mut().push(Entry::Assign(key, s));
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let c2 = collect.clone();
    let s2 = stack.clone();
    let st2 = statics_out.clone();
    lua.globals()
        .set(
            "_vardef",
            lua.create_function(move |lua, (name, value): (String, Value)| {
                // Always: the global and the textually substitutable form.
                lua.globals().set(name.clone(), value.clone())?;
                lua.globals()
                    .get::<mlua::Table>("_vars")?
                    .set(name.as_str(), true)?;
                let s = serialize_value(value).map_err(mlua::Error::external)?;
                st2.borrow_mut().insert(name.clone(), s.clone());
                if !c2.get() {
                    top(&s2).borrow_mut().push(Entry::VarDef(name, s));
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let c3 = collect.clone();
    let s3 = stack.clone();
    let b3 = bodies.clone();
    lua.globals()
        .set(
            "_block",
            lua.create_function(
                move |lua, (keyword, header, builder): (String, String, Function)| {
                    if c3.get() {
                        return Ok(());
                    }
                    let body_table = lua.create_table()?;
                    let inner = Rc::new(RefCell::new(Vec::new()));
                    s3.borrow_mut().push(inner.clone());
                    b3.borrow_mut().push(body_table.clone());
                    builder.call::<()>(())?;
                    s3.borrow_mut().pop();
                    b3.borrow_mut().pop();
                    // Section globals: `theme { }` makes `theme` a Lua
                    // table so `theme.primary` reads as an expression.
                    // Never clobber an existing global (math, string, the
                    // registration functions themselves).
                    let globals = lua.globals();
                    if globals.get::<Value>(keyword.as_str())?.is_nil() {
                        globals.set(keyword.as_str(), body_table)?;
                        globals
                            .get::<mlua::Table>("_blocks")?
                            .set(keyword.as_str(), true)?;
                    }
                    let body = inner.borrow().clone();
                    top(&s3).borrow_mut().push(Entry::Block(keyword, header, body));
                    Ok(())
                },
            )
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let b4 = bodies.clone();
    lua.globals()
        .set(
            "_field",
            lua.create_function(move |_, name: String| {
                let bodies = b4.borrow();
                let Some(body) = bodies.last() else {
                    return Err(mlua::Error::external(
                        "a block field was referenced outside any block",
                    ));
                };
                body.get::<Value>(name.as_str())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let c4 = collect.clone();
    let s4 = stack.clone();
    lua.globals()
        .set(
            "bind",
            lua.create_function(move |_, (combo, action): (String, Value)| {
                if c4.get() {
                    return Ok(());
                }
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

    // `include` is active in both rounds: the resolve walk needs the
    // include entries from collect mode to know which files to recurse
    // into for definitions.
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
            lua.create_function(|_, hex: String| {
                ColorValue::from_hex(&hex)
                    .ok_or_else(|| mlua::Error::external(format!("invalid color `{hex}`")))
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    lua.globals()
        .set(
            "_dur",
            lua.create_function(|_, (n, unit): (f64, String)| {
                let (scale, unit): (f64, &'static str) = match unit.as_str() {
                    "s" => (1000.0, "s"),
                    "m" => (60_000.0, "m"),
                    _ => (1.0, "ms"),
                };
                Ok(DurationValue {
                    ms: n * scale,
                    unit,
                })
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    lua.globals()
        .set(
            "tide",
            build_tide_table(lua, tide).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    lua.globals()
        .set("_vars", lua.create_table().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    lua.globals()
        .set("_blocks", lua.create_table().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    lua.globals()
        .set("_handlers", lua.create_table().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    lua.globals()
        .set("_actions", lua.create_table().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;

    let c6 = collect.clone();
    let s6 = stack.clone();
    lua.globals()
        .set(
            "_on",
            lua.create_function(move |lua, (event, source): (String, String)| {
                if c6.get() {
                    return Ok(());
                }
                let f: Function = lua.load(&source).eval().map_err(mlua::Error::external)?;
                let handlers: mlua::Table = lua.globals().get("_handlers")?;
                let for_event: mlua::Table = match handlers.get::<Value>(event.as_str())? {
                    Value::Table(t) => t,
                    _ => {
                        let t = lua.create_table()?;
                        handlers.set(event.as_str(), &t)?;
                        t
                    }
                };
                for_event.set(for_event.raw_len() + 1, f)?;
                top(&s6).borrow_mut().push(Entry::Handler(event, source));
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    // Inside event handlers, `spawn(cmd)` and `action(string)` queue an
    // action for the compositor to run after the event dispatch finishes
    // (queued in `_actions`, drained by the compositor), so a handler
    // cannot re-enter the event machinery mid-dispatch.
    lua.globals()
        .set(
            "spawn",
            lua.create_function(|lua, cmd: String| {
                let actions: mlua::Table = lua.globals().get("_actions")?;
                actions.set(actions.raw_len() + 1, format!("spawn:{cmd}"))?;
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    lua.globals()
        .set(
            "action",
            lua.create_function(|lua, action: String| {
                let actions: mlua::Table = lua.globals().get("_actions")?;
                actions.set(actions.raw_len() + 1, action)?;
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    Ok(collect)
}

/// Builds the `tide` table from the caller's live-compositor facts.
pub(crate) fn build_tide_table(lua: &Lua, tide: &TideInfo) -> Result<mlua::Table, mlua::Error> {
    let table = lua.create_table()?;
    table.set("backend", tide.backend)?;
    let gpu = lua.create_table()?;
    gpu.set("vendor", tide.gpu_vendor)?;
    table.set("gpu", gpu)?;
    let outputs = lua.create_table()?;
    for (i, (name, width, height)) in tide.outputs.iter().enumerate() {
        let output = lua.create_table()?;
        output.set("name", name.as_str())?;
        output.set("width", *width)?;
        output.set("height", *height)?;
        outputs.set(i + 1, output)?;
    }
    table.set("outputs", outputs)?;
    table.set("workspace", tide.workspace)?;
    Ok(table)
}

/// A Lua value as JSON for `tidectl eval`: scalars, durations and colors
/// as their config strings, lists as arrays, plain tables as objects.
pub(crate) fn lua_value_to_json(value: Value) -> Result<serde_json::Value, String> {
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(b)),
        Value::Integer(i) => Ok(serde_json::Value::from(i)),
        Value::Number(n) => serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .ok_or_else(|| "number could not be represented as JSON".to_string()),
        Value::String(s) => Ok(serde_json::Value::String(s.to_string_lossy())),
        Value::UserData(ud) => {
            if let Ok(d) = ud.borrow::<DurationValue>() {
                return Ok(serde_json::Value::String(d.serialize()));
            }
            if let Ok(c) = ud.borrow::<ColorValue>() {
                return Ok(serde_json::Value::String(c.serialize()));
            }
            Err("unsupported userdata in eval result".to_string())
        }
        Value::Table(t) => {
            // A dense sequence becomes an array; otherwise an object of
            // scalar fields (nested tables recurse).
            let mut is_array = true;
            let n = t.raw_len();
            for i in 1..=n {
                if t.raw_get::<Value>(i).is_err() {
                    is_array = false;
                    break;
                }
            }
            if is_array && n > 0 {
                let mut out = Vec::new();
                for i in 1..=n {
                    let v: Value = t.raw_get(i).map_err(|e| e.to_string())?;
                    out.push(lua_value_to_json(v)?);
                }
                Ok(serde_json::Value::Array(out))
            } else {
                let mut out = serde_json::Map::new();
                for pair in t.pairs::<mlua::Value, mlua::Value>() {
                    let (k, v) = pair.map_err(|e| e.to_string())?;
                    let mlua::Value::String(key) = k else {
                        continue;
                    };
                    out.insert(key.to_string_lossy(), lua_value_to_json(v)?);
                }
                Ok(serde_json::Value::Object(out))
            }
        }
        other => Err(format!("unsupported value type in eval result: {}", other.type_name())),
    }
}

/// Compiles and evaluates a Wave file, returning the same [`Entry`] list
/// the line-based parser produces.
#[allow(dead_code)] // exercised by tests; W7's tidectl eval consumes it
pub(crate) fn evaluate(source: &str, path: &Path) -> Result<Vec<Entry>, String> {
    let lua_source = compile(source, path)?;
    // Sandboxed from creation: only math/string/table, no io/os/package.
    let lua = Lua::new_with(
        StdLib::MATH | StdLib::STRING | StdLib::TABLE,
        mlua::LuaOptions::default(),
    )
    .map_err(|e| format!("in file {}: failed to create Lua state: {e}", path.display()))?;

    let stack: EntryStack = Rc::new(RefCell::new(vec![Rc::new(RefCell::new(Vec::new()))]));
    let collect = Rc::new(Cell::new(false));
    let statics = Rc::new(RefCell::new(std::collections::HashMap::new()));
    let bodies = Rc::new(RefCell::new(Vec::new()));
    install_env(&lua, &stack, collect, statics, bodies, &TideInfo::default())?;

    let chunk = lua.load(&lua_source).set_name(path.display().to_string());
    chunk.exec().map_err(|e| format!("in file {}: {e}", path.display()))?;

    let entries = top(&stack).borrow().clone();
    Ok(entries)
}

/// The recursive include walk shared by both resolve rounds. `collect`
/// selects the round; entries are merged with the shared merge policy
/// (`waves::merge_into`), so include order, cycle detection, and the
/// "including file's own keys win" contract hold for every file.
fn resolve_walk(
    path: &Path,
    ancestors: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
    lua: &Lua,
    stack: &EntryStack,
    sym: &mut Symbols,
    collect: Rc<Cell<bool>>,
) -> Result<Vec<Entry>, String> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if ancestors.contains(&canonical) {
        return Err(format!("include cycle detected in file {}", path.display()));
    }
    ancestors.push(canonical);
    let result = resolve_uncycled(path, ancestors, warnings, lua, stack, sym, collect);
    ancestors.pop();
    result
}

fn resolve_uncycled(
    path: &Path,
    ancestors: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
    lua: &Lua,
    stack: &EntryStack,
    sym: &mut Symbols,
    collect: Rc<Cell<bool>>,
) -> Result<Vec<Entry>, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|err| format!("in file {}: {err}", path.display()))?;

    // Each file evaluates as its own chunk; a fresh top-level sink keeps
    // its entries separate from included files' entries.
    stack.borrow_mut().push(Rc::new(RefCell::new(Vec::new())));
    let lua_source = compile_with(&contents, path, sym)?;
    let chunk = lua.load(&lua_source).set_name(path.display().to_string());
    let run = || chunk.exec();
    let exec_result = if sym.lenient {
        // The collect round is best-effort: a file whose expressions
        // reference definitions from a file evaluated later may fail
        // here, and that is fine, round two is authoritative.
        match run() {
            Ok(()) => Ok(()),
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "Wave definition-collection pass failed for this file, continuing");
                Ok(())
            }
        }
    } else {
        run()
    };
    exec_result.map_err(|e| format!("in file {}: {e}", path.display()))?;
    let file_entries = {
        let sink = stack.borrow_mut().pop().unwrap();
        let entries = sink.borrow().clone();
        entries
    };

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut merged = Vec::new();
    let mut own_entries = Vec::with_capacity(file_entries.len());
    for entry in file_entries {
        match entry {
            Entry::Include(include) => {
                let include_path = super::waves::resolve_include_path(parent, &include);
                match resolve_walk(&include_path, ancestors, warnings, lua, stack, sym, collect.clone()) {
                    Ok(included) => super::waves::merge_into(&mut merged, included),
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
    super::waves::merge_into(&mut merged, own_entries);
    Ok(merged)
}

/// Reads `path` as Wave, resolving `include "..."` statements with the
/// shared merge policy, cycle detection, and include order.
///
/// Two rounds over one shared Lua state implement the shared-variable
/// contract: round one evaluates every reachable file in collect mode so
/// every `@name = value` definition (from any file, before or after its
/// use) becomes textually substitutable in every emitter; round two
/// evaluates with the full environment and the collected symbols, then
/// merges per-file entry lists in include order.
#[cfg(test)]
pub(crate) fn resolve(path: &Path) -> Result<(Vec<Entry>, Vec<String>), String> {
    let lua = Lua::new_with(
        StdLib::MATH | StdLib::STRING | StdLib::TABLE,
        mlua::LuaOptions::default(),
    )
    .map_err(|e| format!("in file {}: failed to create Lua state: {e}", path.display()))?;
    resolve_with_lua(&lua, &TideInfo::default(), path)
}

/// `resolve` on a caller-owned Lua state: the runtime path (Smallvil's
/// session Lua) so `@name` globals and section tables persist after the
/// load and stay queryable through `tidectl eval` and `on` handlers.
/// The environment is re-installed on each call (fresh `tide`, fresh
/// `_vars`/`_blocks` tracking, and stale user globals from a previous
/// config are cleared before the real round).
pub(crate) fn resolve_with_lua(
    lua: &Lua,
    tide: &TideInfo,
    path: &Path,
) -> Result<(Vec<Entry>, Vec<String>), String> {
    let stack: EntryStack = Rc::new(RefCell::new(vec![Rc::new(RefCell::new(Vec::new()))]));
    let statics_out: Rc<RefCell<std::collections::HashMap<String, String>>> =
        Rc::new(RefCell::new(std::collections::HashMap::new()));

    // Round 1: collect definitions, leniently. Every reachable file
    // evaluates in collect mode with a lenient emitter, so every
    // `@name = value` (from any file, before or after its use) becomes
    // textually substitutable in every round-two emitter.
    let bodies = Rc::new(RefCell::new(Vec::new()));
    let collect = Rc::new(Cell::new(true));
    install_env(lua, &stack, collect.clone(), statics_out.clone(), bodies, tide)?;
    let mut ancestors = Vec::new();
    let mut sym = Symbols {
        lenient: true,
        ..Default::default()
    };
    resolve_walk(path, &mut ancestors, &mut Vec::new(), lua, &stack, &mut sym, collect.clone())?;

    // Round 2: evaluate with the collected definitions visible to every
    // emitter. `compile_with` mutates the shared symbols (loop/`fn`
    // scopes are balanced per file; `@` re-definitions are idempotent).
    // Stale globals from a previous config load are dropped first so a
    // removed `@name` or `name { }` block cannot keep answering evals.
    collect.set(false);
    clear_user_globals(lua).map_err(|e| format!("in file {}: {e}", path.display()))?;
    sym.lenient = false;
    sym.statics = statics_out.borrow().clone();
    let mut warnings = Vec::new();
    let entries = resolve_walk(path, &mut ancestors, &mut warnings, lua, &stack, &mut sym, collect)?;

    Ok((entries, warnings))
}

/// Nils the Lua globals that user config (not the environment) created:
/// `@name` variables and section tables, tracked in `_vars`/`_blocks`.
fn clear_user_globals(lua: &Lua) -> Result<(), mlua::Error> {
    for table_name in ["_vars", "_blocks"] {
        let tracked: mlua::Table = lua.globals().get(table_name)?;
        for pair in tracked.pairs::<String, bool>() {
            let (name, _) = pair?;
            lua.globals().set(name.as_str(), mlua::Value::Nil)?;
        }
        tracked.clear()?;
    }
    Ok(())
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
        // statically known @variables fold their literal value into
        // expressions: `extra` is 4, so `8 * extra` compiles to `8 * 4`
        let lua = compile_str("@extra = 4\ngaps = 8 * extra\n");
        assert!(lua.contains("_leaf(\"gaps\", 8 * 4)"));
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
        assert!(lua.contains("_leaf(\"gradient\", {\"theme.primary\", \"theme.deep\"})"));
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
        let lua = compile_str("@mod = SUPER\nbind $mod+R { \"spawn:rofi -show drun\" }\n");
        assert!(lua.contains("bind(\"SUPER+R\", {\"spawn:rofi -show drun\"})"));
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
        // `on` without a block still errors
        let err = compile("on \"x\"\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("needs an event name and a block"));
        let err = compile("}\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("unexpected `}`"));
    }

    #[test]
    fn compile_eval_expression_handles_surface_and_identifiers() {
        // durations and colors are surface literals
        assert_eq!(compile_eval_expression("600ms * 2").unwrap(), "_dur(600, \"ms\") * 2");
        assert_eq!(compile_eval_expression("1.5s / 2").unwrap(), "_dur(1.5, \"s\") / 2");
        assert_eq!(compile_eval_expression("#8EDDFF").unwrap(), "_color(\"8EDDFF\")");
        // unknown names stay identifiers for the session Lua to resolve
        assert_eq!(compile_eval_expression("theme.primary").unwrap(), "theme.primary");
        assert_eq!(compile_eval_expression("mod").unwrap(), "mod");
        assert_eq!(compile_eval_expression("tide.backend == \"winit\"").unwrap(), "tide.backend == \"winit\"");
        // markers error with a hint
        let err = compile_eval_expression("$mod").unwrap_err();
        assert!(err.contains("use `mod`"), "{err}");
        let err = compile_eval_expression("@mod").unwrap_err();
        assert!(err.contains("use `mod`"), "{err}");
    }

    #[test]
    fn empty_one_line_blocks_are_allowed() {
        let lua = compile_str("vessels { }\noutput eDP-1 { }\n");
        assert!(lua.contains("_block(\"vessels\", \"\", function()\nend)"));
        assert!(lua.contains("_block(\"output\", \"eDP-1\", function()\nend)"));
        // a non-empty one-liner is still an error
        let err = compile("vessels { enabled = true }\n", Path::new("test.wave")).unwrap_err();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn on_handler_registers_and_queues_actions() {
        let lua = Lua::new_with(
            StdLib::MATH | StdLib::STRING | StdLib::TABLE,
            mlua::LuaOptions::default(),
        )
        .unwrap();
        let tide = TideInfo {
            backend: "winit",
            workspace: 3,
            ..Default::default()
        };
        let dir = std::env::temp_dir().join(format!(
            "tidewm-wave-on-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("config.wave");
        std::fs::write(
            &main,
            "on \"workspace-changed\" {\n    if tide.workspace == 3 then\n        spawn(\"kitty\")\n    end\n}\n",
        )
        .unwrap();

        let (entries, warnings) = resolve_with_lua(&lua, &tide, &main).expect("should resolve");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(entries.len(), 1);
        let Entry::Handler(event, source) = &entries[0] else {
            panic!("expected a handler entry, got {:?}", entries[0]);
        };
        assert_eq!(event, "workspace-changed");
        assert!(source.contains("if tide.workspace == 3 then"), "{source}");

        // The live function sits in the session Lua's _handlers table and
        // queues a spawn through _actions when it runs.
        let handlers: mlua::Table = lua.globals().get("_handlers").unwrap();
        let for_event: mlua::Table = handlers.get("workspace-changed").unwrap();
        let f: mlua::Function = for_event.get(1).unwrap();
        f.call::<()>(()).expect("handler should run");
        let actions: mlua::Table = lua.globals().get("_actions").unwrap();
        let queued: Vec<String> = actions
            .sequence_values()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(queued, vec!["spawn:kitty".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
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


    #[test]
    fn duration_math_and_serialization() {
        // 600ms * 2 -> 1200ms; 1.5s * 2 -> 3s; 90m / 2 -> 45m
        let entries = evaluate(
            "a = 600ms * 2\nb = 1.5s * 2\nc = 90m / 2\nd = 2 * 300ms\ne = 1s + 500ms\nf = 1.5s\n",
            Path::new("test.wave"),
        )
        .expect("duration math should evaluate");
        let values: Vec<String> = entries
            .iter()
            .map(|e| match e {
                Entry::Assign(_, v) => v.clone(),
                _ => panic!("unexpected entry"),
            })
            .collect();
        assert_eq!(
            values,
            vec![
                "1200ms".to_string(),
                "3s".to_string(),
                "45m".to_string(),
                "600ms".to_string(),
                // 1s + 500ms keeps the first operand's unit: 1.5s
                "1.5s".to_string(),
                "1.5s".to_string(),
            ]
        );
    }

    #[test]
    fn dbg_chunk() {
        let src = "theme {\n    primary = #8EDDFF\n    deep = primary.darken(0.35)\n}\n";
        eprintln!("=== CHUNK:\n{}", compile(src, std::path::Path::new("t.wave")).unwrap());
    }

    #[test]
    fn color_palette_with_darken_lighten_and_section_globals() {
        let entries = evaluate(
            "theme {\n    primary = #8EDDFF\n    deep = primary.darken(0.35)\n    highlight = primary.lighten(0.15)\n}\nborder {\n    gradient = [theme.primary, theme.deep]\n}\n",
            Path::new("test.wave"),
        )
        .expect("palette should evaluate");
        // deep = 8EDDFF * 0.65: r=142*0.65=92.3->92=0x5C,
        // g=221*0.65=143.65->144=0x90, b=255*0.65=165.75->166=0xA6
        assert_eq!(entries[0], Entry::Block("theme".into(), "".into(), vec![
            Entry::Assign("primary".into(), "8EDDFF".into()),
            Entry::Assign("deep".into(), "5C90A6".into()),
            // lighten(0.15): 142+(255-142)*0.15=159=0x9F, 221+34*.15=226=0xE2, 255=0xFF
            Entry::Assign("highlight".into(), "9FE2FF".into()),
        ]));
        // gradient = [theme.primary, theme.deep]; list items serialize
        // as bare values (colors as RRGGBB)
        assert_eq!(entries[1], Entry::Block("border".into(), "".into(), vec![
            Entry::Assign("gradient".into(), "[8EDDFF, 5C90A6]".into()),
        ]));
    }

    #[test]
    fn resolve_handles_includes_in_new_syntax() {
        let dir = std::env::temp_dir().join(format!(
            "tidewm-wave-resolve-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("keybinds.wave"),
            "@mod = SUPER\nbind $mod+Q { close-window }\n",
        )
        .unwrap();
        let main = dir.join("config.wave");
        std::fs::write(
            &main,
            "include \"keybinds.wave\"\nterminal = kitty\nbind $mod+Return { spawn:kitty }\n",
        )
        .unwrap();

        let (entries, warnings) = resolve(&main).expect("should resolve");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            entries,
            vec![
                Entry::VarDef("mod".into(), "SUPER".into()),
                Entry::Bind("SUPER+Q".into(), "close-window".into()),
                Entry::Assign("terminal".into(), "kitty".into()),
                Entry::Bind("SUPER+Return".into(), "spawn:kitty".into()),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}



