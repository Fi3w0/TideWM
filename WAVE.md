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

`$name = value` defines a variable. It substitutes textually inside strings, the Hyprland way:

```wave
$mod = SUPER
bind $mod+Return { spawn:kitty }
```

And it is a real Lua global in expressions:

```wave
gaps = 8 + $gap_extra
```

Variables live in one shared environment, so a variable defined in an included file is visible to the includer and back.

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

- **`$name` does two jobs.** It substitutes textually in strings and acts as a global in expressions. Usually that is the point. Occasionally it surprises: `$mod` inside a quoted string is text, not a value you can add to.
- **Bare tokens are strings.** `spawn:rofi -show drun` needs quotes, because spaces end the token. The old grammar's "rest of the line" rule made that unnecessary; expressions made it impossible. Migration is one pass of quotes.
- **`#` is a comment unless it starts a color.** `color = #8EDDFF` works, `# hello` is a comment. This is the one place the lexer looks ahead.
- **Blocks are always multi-line.** The earlier grammar allowed one-line blocks; they were removed because they made `key = value` ambiguous.
- **Config Lua is sandboxed.** No file, network, or process access from expressions. Config evaluation is deterministic on purpose, because diffing the old and new configs is what makes hot reload safe. Anything that touches the machine goes through TideWM actions (`spawn:...`), never through Lua IO.
- **Durations need units.** `duration = 600` is an error. `duration = 600ms` is not. The unit is the type.
- **Loops use `$i`.** A loop variable substitutes textually, the way `$mod` does. It looks like a config variable and is not one.
- **Aliases warn.** Renamed keys from the old format still parse, with a one-line deprecation warning in the panel. They go away after one release.

## Wave outside TideWM

Wave is TideWM's format today, but nothing about the grammar is TideWM-shaped. It knows nodes, leaves, statements, and typed values; it does not know workspaces. A config schema is just the set of node and key names an application accepts, so the parser is meant to lift into its own crate and embed anywhere YAML or JSON is used today, with two things those formats do not give you: computation in the file, and typed values your application defines, like colors and durations. `wavefmt` and the error conventions come with it.

That is the point of the slogan. The surface is a configuration file. Underneath, it is Lua. Make waves with Lua.
