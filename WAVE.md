# Wave

**Make waves with Lua.**

Wave is a config format that is data on the surface and Lua underneath. This file explains what that means, why it exists, and where it has rough edges. It is not a reference manual: the exhaustive key-by-key reference lives in `DOCUMENTATION.md`. Read this first, reach for that when you need a key.

Wave is the target format for TideWM's config rewrite (the roadmap lives in `AGENT.md`). The `config.wave` that TideWM parses today still uses the older line-based grammar; this document describes where the format is going.

## What Wave is

Every Wave file has two layers.

The surface is data. `name { }` nodes group settings, `key = value` leaves set them, and the file reads the way a Hyprland or sway config reads:

```wave
border {
    width = 2
}
```

The inside is Lua. Every value is an expression, and statements are first-class: conditionals, loops, functions, and a `script { }` block when you want the full language.

```wave
border {
    width    = 2
    gradient = [theme.primary, theme.deep]
}
```

A beginner's config never needs to know the second layer exists. A power user's config never needs to fight it.

## Why not just Lua

Hyprland moved its config to Lua in 0.55. The power is real, and so is the cost: a config that is all code reads as code, and newcomers who never asked to write a program now have to. The reaction to that switch shows both sides: long-time users love it, newcomers are the ones who suffer.

Wave takes the middle road. The surface stays declarative, so a config still reads like a config, and Lua is the engine under it, so the power is always one level down. You escalate to code when you need it, never because the format forces you to.

## Why not YAML, TOML, or KDL

Each one has a documented complaint against it, and those complaints are what matter in practice:

- **YAML** has three sins: meaningful indentation, implicit typing, and silent failures. All three are gone in Wave. Blocks use braces, values are explicit, and unknown keys are loud errors.
- **TOML** cannot nest comfortably. Configs that grow past a few sections turn into arrays-of-tables and dotted-key soup.
- **KDL** is picky about quoting, and its space-separated leaves make it hard to tell a setting from an argument without knowing the schema.

None of the three can compute. Wave can, because it is Lua underneath.

## How a Wave file works

### The three kinds of lines

1. A **node** opener: `name {` ... `}`. Groups settings. Nodes nest.
2. A **leaf**: `key = value`. One setting, one typed value.
3. A **statement**: `if`, `for`, `fn`, `include`, `on`, `script`.

That is the whole grammar. There is no "rest of the line is the value" rule, so nothing multi-piece can ever hide on one line.

### Values

Values are typed:

```wave
gaps          = 8                   # number
water_effects = true                # bool
terminal      = "kitty"             # quoted string
color         = #8EDDFF             # color literal
duration      = 600ms               # duration literal (ms, s, m)
spawn         = [waybar, "swaybg -i ~/wallpaper.png"]   # list
```

A bare token without spaces is a string, KDL-style: `SUPER+Return`, `spawn:kitty`, `water-drop`. Operators need spaces around them: `8 * scale` computes, `SUPER+Return` does not.

### Blocks and comments

A block is always multi-line. There are no one-liners: this is what keeps `key = value` unambiguous. `#` starts a comment, unless it starts a color literal. `--` and `--[[ ]]` also work, for people coming from Lua.

### Variables

Wave separates the two roles a variable can play, with one marker each:

- **`@name = value` defines** a variable. `@` appears only on definition lines, never anywhere else.
- **`$name` references** it inside a bare string. `$` appears only in strings, never on a definition line.
- **Expressions use the plain name**: `gaps = 8 * extra`. The `$` and `@` markers are both errors in expressions, with a message pointing at the identifier form.

```wave
@mod = SUPER
@terminal = wave(kitty, alacritty, foot)

bind $mod+Return { spawn:$terminal }
```

A statically known definition (`@mod = SUPER`) substitutes its literal text into strings. A runtime definition (`@terminal = wave(...)`) splices through a concatenation, which is why `$terminal` works anywhere after its line.

**Quoted strings are literal.** No substitution, no `$` processing: `"$HOME"` in a spawn command is verbatim text, and an env var can never collide with a config variable. An unquoted `$name` that is not defined is a loud compile error telling you to define it (`@name = value`) or quote it.

Variables live in one shared environment, so a variable defined in an included file is visible to the includer and back. Loop variables and `fn` parameters are referenced the same way in strings: `$i`, `$key`, `$app`.

```wave
for i = 1, 9 do
    bind $mod+Num$i { workspace:$i }
end

fn media(key, app) {
    bind $mod+$key { spawn:$app }
}
media(comma, spotify)
```

### Lua statements

```wave
if tide.gpu.vendor == "nvidia" then
    udev { disable_overlay_planes = true }
end

for i = 1, 9 do
    bind $mod+Num$i { workspace:$i }
end

fn media(key, app) {
    bind $mod+$key { spawn:$app }
}
media(comma, spotify)
```

`script { }` is raw Lua for the cases where the sugar does not go deep enough.

### One registry, two surfaces

Keybinds are the example that ties it together. The node form:

```wave
bind $mod+Q { close-window }
```

desugars to the same registration the function form uses:

```wave
script {
    bind("$mod+Q", "close-window")
}
```

Same function, same semantics, one registry. The node form is sugar, not a second mechanism, the same way TideWM's IPC socket is the keybind mechanism reachable over a socket, not a second dispatch.

## The rules that keep it clean

1. A line is a node opener, a leaf, or a statement. Nothing else exists.
2. Commas are separators only. A comma-soup line cannot be written.
3. Anything with parts is a node or a list. No stringly-typed payloads.
4. Every key is checked against the schema of its node. Unknown keys, wrong types, and typos are `file:line` errors in the config panel, and the old config keeps running.
5. `wavefmt` reformats any file to the canonical tree. Mess does not accumulate, because formatting is a command, not a habit.

## What Wave can do

- **Hardware conditionals.** One config for every machine, via the `tide` query table: `tide.backend`, `tide.gpu.vendor`, `tide.outputs`.
- **One palette.** Colors are values: `primary.darken(0.35)` and `primary.lighten(0.15)` derive a whole theme from one line.
- **Loops and macros.** Nine workspace binds in three lines. Your own bind groups as `fn`.
- **Reactive config.** `on "workspace-changed" { }` handlers, fed by the same event pipeline TideWM's IPC subscribe uses.
- **Live everything.** Hot reload with a diff: the new file evaluates, only the changed sections apply, and a broken edit shows the error while the old config keeps running. `tidectl eval "theme.primary"` evaluates an expression against the live session.

## Rough edges (honest notes)

- **`@` defines, `$` references.** One marker per role, and both are errors where they do not belong. `@mod` in an expression or `$mod` on a definition line tell you what to write instead. The error messages are the teaching tool.
- **Quotes are literal.** No `$` processing inside `"..."`, so `"$HOME"` is verbatim and env vars can never collide with config variables. A bare `$name` that is not defined is a compile error, never silent text.
- **Bare tokens are strings.** `spawn:rofi -show drun` needs quotes, because spaces end the token. The old grammar's "rest of the line" rule made that unnecessary; expressions made it impossible. Migration is one pass of quotes.
- **`#` is a comment unless it starts a color.** `color = #8EDDFF` works, `# hello` is a comment. This is the one place the lexer looks ahead.
- **Blocks are always multi-line.** The earlier grammar allowed one-line blocks; they were removed because they made `key = value` ambiguous.
- **Config Lua is sandboxed.** No file, network, or process access from expressions. Config evaluation is deterministic on purpose, because diffing the old and new configs is what makes hot reload safe. Anything that touches the machine goes through TideWM actions (`spawn:...`), never through Lua IO.
- **Durations need units.** `duration = 600` is an error. `duration = 600ms` is not. The unit is the type.
- **Loops use `$i`.** A loop variable references through the same `$` in strings. It looks like a config variable and is not one, which is exactly why it uses the same marker.
- **Aliases warn.** Renamed keys from the old format still parse, with a one-line deprecation warning in the panel. They go away after one release.

## The desugaring contract (for implementers)

This section is the exact surface-to-Lua mapping. The rule of thumb: the surface is sugar over a small registration API, and every surface construct maps to exactly one Lua construct. Implementations must follow this table, not invent a second translation.

### The registration API (provided by the host environment)

| Surface | Emitted Lua | Host behavior |
| --- | --- | --- |
| `key = value` (leaf) | `_leaf("key", value)` | Records an assignment entry; value serialized back to its textual form |
| `@name = value` (variable) | `_vardef("name", value)` | Records a variable entry AND sets the Lua global `name` |
| `name { }` (block) | `_block("name", "header", function() ... end)` | Runs the builder, captures produced entries into the block body |
| `bind X { a b }` | `bind("X", {"a", "b"})` | One binding entry per action |
| `bind X = action` (deprecated line form) | `bind("X", "action")` | Same registration, value is raw rest-of-line |
| `include "path"` | `include("path")` | Records an include entry, resolved later by the existing include machinery |
| `$wave(a, b)` in a string | `.. wave("a", "b")` spliced | First installed candidate, last-candidate fallback, unchanged from today |
| `wave(a, b)` in an expression | `wave("a", "b")` | Same function, now callable anywhere |
| `if` / `for` / `while` / `do` / `end` / `else` / `elseif` / `local` / `function` / `return` | passed through verbatim | Lua statements; surface lines inside their bodies are still transpiled |
| `fn name(args) { }` | `local function name(args) ... end` | Macro sugar; params are in scope for `$param` concatenation |
| `script { }` | body passed through raw | No transpilation, no comment stripping |
| `on "event" { }` | not yet | Landed in W7 |

The environment also exposes `math`, `string`, `table`, and a `tide` query table (empty in W1, populated in W7).

### Statement dispatch

A line, after comment stripping, is dispatched on its first word:

1. `}` alone closes a block. A line ending in `{` (trimmed) opens one: first word is the keyword, the rest is the header (one quoted string or one bare word).
2. A reserved first word: `bind`, `include`, `fn`, `script`, `on`, `if`, `elseif`, `else`, `for`, `while`, `do`, `end`, `local`, `function`, `return`.
3. `@name = value`: a variable definition. `@` is the definition marker and appears nowhere else.
4. `key = value`: key is a bare identifier, `=` follows, the value is parsed as a value (see below). `bind X = ...` is the deprecated bind line form.
5. Anything else: an expression statement, which must be a call (`name(...)`). A bare word that is not a call is an error, so a typo reads as an error instead of a silent no-op.

Blocks are always multi-line, with one exception: `bind X { a, b }` may be written on one line, actions split on commas. This is the only one-liner in the grammar.

### Values

A value is one of two things, decided by a single rule: if the trimmed text contains whitespace or `(` or `[`, it is parsed as a Lua expression; otherwise it is a single token.

Single token: a number, `true`/`false`, a color (`#RRGGBB` or `#RRGGBBAA`, serialized back as `RRGGBB`), a duration (`500ms`, `1.5s`, `90m`), or anything else is a bare string (`SUPER+Return`, `spawn:kitty`, `water-drop`).

Expression: tokenized on whitespace and the operator/separator characters `( ) [ ] { } , + - * / % .. == ~= < > <= >= =`. Dots are part of word tokens, so `theme.primary` is one token. Tokens are then classified:

- numbers and quoted strings: as-is
- `true` / `false` / `nil`: as-is
- Lua keywords (`and`, `or`, `not`, `then`, ...): as-is
- a word followed by `(` or a known identifier (a defined `@name` variable, `fn` name, loop variable, `fn` parameter): as-is
- a word containing a dot, whose base is a known identifier: as-is (Lua member access)
- a color token: `_color("RRGGBB")`
- a duration token: `_dur(600, "ms")`
- `$wave(`: `wave(` with the arguments rewritten
- anything else: a quoted string. This is what makes `wave(kitty, alacritty)` and `media(comma, spotify)` work without quotes.

The markers have one role each, enforced with errors: `@` only defines (`@name = value` on its own line), `$` only references in bare strings, and both are compile errors in expressions with a message pointing at the identifier form. A `$name` that is not defined (not a `@name` variable, loop variable, or `fn` parameter) is a compile error, never silent text. A fully quoted string is literal: no `$` processing at all, so `"$HOME"` in a spawn command is verbatim.

### Strings inside binds

A bind action line is a bare token, a quoted string, or a full expression that evaluates to a string. `$wave(...)` splice works in all three.

### Comments

`#` starts a comment unless it starts a color token (six or eight hex digits followed by end of line, whitespace, `,`, `]`, `)`, or `}`). `--` starts a comment. `--[[ ]]` is a block comment removed in a pre-pass. Quoted strings protect all three.

### Deprecated forms

The old `bind X = rest-of-line` form parses and registers, with the action taken verbatim. It is removed one release after the rewrite lands (W8).

### Deferred

`on "event"` handlers (W7), section globals so `theme.primary` reads as an expression (W4), list values serialized through entries (W4). The grammar accepts lists (`[a, b]`) and serializes them as `["a", "b"]`; config-level list semantics land with the W4 rename work.

## Wave outside TideWM

Wave is TideWM's format today, but nothing about the grammar is TideWM-shaped. It knows nodes, leaves, statements, and typed values; it does not know workspaces. A config schema is just the set of node and key names an application accepts, so the parser is meant to lift into its own crate and embed anywhere YAML or JSON is used today, with two things those formats do not give you: computation in the file, and typed values your application defines, like colors and durations. `wavefmt` and the error conventions come with it.

That is the point of the slogan. The surface is a configuration file. Underneath, it is Lua. Make waves with Lua.
