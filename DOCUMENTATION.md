# TideWM Documentation

Full reference for configuring and controlling TideWM: every config key, every action string, IPC, and the protocol matrix.

- [Command-line flags](#command-line-flags)
- [Config file](#config-file)
- [Wallpaper behavior](#wallpaper-behavior)
- [Config reference](#config-reference)
- [Action strings](#action-strings)
- [IPC and `tidectl`](#ipc-and-tidectl)
- [Protocol matrix](#protocol-matrix)

## Command-line flags

| Flag | Notes |
| --- | --- |
| `-c, --config <path>` | Load this file instead of `$XDG_CONFIG_HOME/tidewm/config.wave`. Applies to the hot-reload watcher too, not just the initial load. |
| `-s, --spawn <command>` | Spawn one specific command right after startup, instead of nothing. No shell parsing (same rule as the `spawn` config key). |
| `-v, --version` | Print the version and exit. |
| `-h, --help` | Print usage and exit. |

## Config file

`$XDG_CONFIG_HOME/tidewm/config.wave`, or `~/.config/tidewm/config.wave` if `XDG_CONFIG_HOME` isn't set (or the path given to `--config`, see above). Written out with working defaults on first run. Almost every change hot-reloads on save — no restart needed — and a bad edit is shown in a persistent compositor-owned panel that reserves space above tiled windows (with file/line detail) while the previous config keeps running. Fixing the file clears the panel; the existing short reload/debug toast remains separate. `spatial_engine` is hot-reloadable too: a change migrates every live window between the Classic and Ocean models in place instead of requiring a restart (workspace stacks become reefs laid out on the lateral line, and back; see the Ocean section below for the mapping). Startup-owned exceptions are `xwayland { enabled }` and Ocean reef/bookmark declarations; changing one shows a restart-required warning. Ocean's `camera_step` remains hot-reloadable. Keyboard layout and already-connected touchpads also apply immediately.

### Wave format

TideWM's own config format. The surface is declarative data, and underneath it is Lua: every value is an expression, and statements (`if`, `for`, `fn`, `script`, `on`) are first-class. Read `WAVE.md` for the design rationale and the exact desugaring contract; this section is the working summary.

A line is one of three things:

- **`key = value`**, a setting. The value is a typed literal (number, `true`/`false`, a color like `#8EDDFF`, a duration like `600ms`/`1.5s`/`90m`, a bare string like `spawn:kitty` or `SUPER+Return`, a `[list, of, things]`), or an expression: `gaps = 8 * scale`, `deep = primary.darken(0.35)`.
- **`name { ... }`**, a block, always real multi-line (an empty one-liner, `vessels { }`, is allowed and means "all defaults"). `output eDP-1 { ... }` and `rule { ... }` are blocks; a block's keyword becomes a Lua table you can reference (`theme.primary`), and inside a block, sibling fields resolve: `deep = primary.darken(0.35)` reads the `primary` leaf above it.
- **A statement**: `@name = value` defines a variable, `include "path.wave"` includes, `bind`, `if`, `for`, `fn`, `script`, `on`.

**`@` defines, `$` references.** `@mod = SUPER` defines a variable (the only place `@` appears); `$mod` in a bare string references it (the only place `$` appears); expressions use the plain name (`pointer_modifier = mod`). Quoted strings are literal: `"$HOME"` in a spawn command is verbatim text. An undefined `$name` is a compile error with the fix in the message. `$wave(a, b, c, ...)` in a bare string is the portable-candidate builtin: it resolves to the first candidate that is a real executable (directly or via `$PATH`), falling back to the last candidate untried — `terminal = wave(kitty, alacritty, foot)` is the expression form of the same thing.

```wave
@mod = SUPER
terminal = wave(kitty, alacritty, foot)

bind $mod+Return { spawn:kitty }
bind $mod+Q      { close-window }
bind $mod+D      { "spawn:rofi -show drun" }
```

Binds are node form: `bind <combo> { <action> }`, one action per line (or comma-separated on one line). `terminal` is a top-level key, not a `@name` variable — reference it as `$terminal` only after defining `@terminal = ...`.

**Typed values.** Durations and colors are real values with math: `600ms * 2` is `1200ms`, `1.5s * 2` is `3s`, `2 * 300ms` is `600ms`; `primary.darken(0.35)`, `primary.lighten(0.15)`, and `alpha(a)` derive palette colors. Every duration key accepts a unit (`cursor_hide_after = 2s`) or a bare millisecond number.

**Reactive config.** `on "event" { ... }` registers a handler body that runs when the event fires, with the live `tide` table (`tide.backend`, `tide.gpu.vendor`, `tide.outputs`, `tide.workspace`) refreshed first:

```wave
on "workspace-changed" {
    if tide.workspace == 3 then
        spawn("kitty --class notes")
    end
}
```

`spawn(cmd)` and `action(string)` inside a handler queue an action that runs after the dispatch. Events: `window-opened`, `window-closed`, `window-changed`, `workspace-changed`, `focus-changed`, `urgent-changed`, `depth-changed`, `config-reloaded`. The same `tide` table powers hardware conditionals at load time: `if tide.backend == "udev" and tide.gpu.vendor == "nvidia" then ... end`.

**Live queries.** `tidectl eval <expression>` evaluates on the running session's Lua — config variables, section tables, and the live `tide` table are all answerable (`tidectl eval "theme.primary"`, `tidectl eval "tide.workspace"`).

The line-based grammar is gone; Wave is the only grammar. Old configs were migrated (the mechanical changes: `$mod = SUPER` becomes `@mod = SUPER`, `bind X = Y` becomes `bind X { Y }`, `$wave(...)` becomes `wave(...)` in values, `spawn_at_startup` lines become one `spawn = [...]` list) and the old spellings no longer parse: a legacy key is an unknown-key warning.

**Multi-file:** `include "path.wave"` as its own statement, repeatable (one per line), in any file (the main one, or one it includes). Each path resolves relative to the file that lists it; `~/` expands to your home directory. Rules:

- `input { }`, `input { touchpad { } }`, `env { }`, `switch_events { }`, and a given `submap <name> { }` merge field-by-field across files — the same key set from two files combines rather than one replacing the other.
- `output <name> { }`, `rule { }`, and `layer_rule { }` blocks accumulate — entries from every file all end up present.
- A later `include` overlays an earlier one.
- **The including file's own keys always win over anything it includes.** If `config.wave` includes `overrides.wave`, and both set `gaps`, `config.wave`'s own value wins — put an override directly in the file doing the including, not in a file you list last.
- A broken include (missing, unreadable, unparseable, or a cycle) is skipped with a warning; it doesn't fail the whole config.
- The config directory is watched recursively (`*.wave` files only, dotfiles/dotdirs skipped) — editing any included file hot-reloads exactly like editing the main file.

## Wallpaper behavior

TideWM always provides the bundled `assets/tide-aqua-4k.png` artwork, so a fresh session never needs a separate daemon. The source is decoded once at its native 3840×2160 resolution and scales to each output with centered `cover` cropping rather than distortion; it is never pre-downsampled, and it is hidden while the session is locked. This costs about 31.6 MiB of steady-state pixel backing in exchange for retaining full 4K detail. It is intentionally only a fallback: standard Wayland layer-shell background clients render above it, so tools such as `swaybg`, `swww`/`awww`, or another compatible wallpaper daemon can provide images, animations, transitions, and per-output management without a TideWM-specific API. Start one with `spawn = [swaybg ...]` if desired.

## Config reference

### Top-level keys

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `terminal` | string | `"kitty"` | Spawned by the shipped `$mod+Return` bind (`@mod = SUPER` in the generated file). The terminal fallback is `wave(kitty, alacritty, foot, xterm)` — see the Wave format section above. |
| `engine` | `classic` \| `ocean` | `classic` | Selects one of TideWM's two WM ownership models. Classic keeps numbered workspaces. Ocean has no workspaces: outputs are cameras into one continuous 2D world. Hot-reloadable: a change migrates every live window in place. Classic→Ocean turns each output's populated workspace trees into reefs on the lateral line at `X = (N-1) * (output width + 128)` with the camera at the previously-active workspace; depth-deck windows are recalled to their tiles, floating windows translate to world coordinates around their workspace's reef, and pinned windows become Ocean screen pins. Ocean→Classic turns reefs sorted left-to-right into workspaces `1..N` on the output whose camera is nearest, selects the active workspace from each camera, clamps floating windows into the visible area, and restores pins. Tab groups and fullscreen/maximized entries carry across both directions; Ocean bookmarks and camera history are dropped (no Classic counterpart). |
| `drag_modifier` | modifier or `+`-joined modifiers | `super` | Modifier physically held for compositor mouse actions: left-drag moves floating windows or drag-swaps tiles; right-drag resizes floating or tiled windows. Accepts `super`/`logo`/`mod4`, `alt`/`mod1`, `ctrl`/`control`, and `shift`. The shipped config sets it to `mod`. |
| `welcome_hint` | bool | `true` | Shows a persistent empty-desktop card reminding you to use your configured terminal bind. Disappears when a real window maps; delete this key (or set it `false`) to stop it returning. |
| `reload_toast` | bool | `true` | Shows the short compositor card after a successful hot reload. `false` hides that confirmation only; parse errors and configuration warnings remain visible so a bad config cannot silently lock itself in. |
| `water_effects` | bool | `true` | Master toggle for TideWM's water/aqua render identity. Disables water-glass, backdrop capture, impulse ripples, wave workspace transitions, Cascade pour/drain, automatic depth/buoyancy, interactive viscosity, connected-vessel resize, and floating sway when `false`. |
| `builtin_wallpaper` | bool | `true` | Whether the embedded 4K aqua fallback wallpaper is decoded and drawn. A layer-shell wallpaper (swaybg/swww/hyprpaper) renders above it regardless, so set this `false` to skip the decode and its GPU texture entirely and reclaim the CPU and VRAM it would otherwise cost — useful on low-RAM machines or when an external wallpaper daemon is always present. Live-reloadable: a running session stops drawing it the moment this becomes `false`. |
| `viscosity` | float, `0`–`4` | `1.0` | Interactive window move/resize damping. `0` follows the pointer immediately; higher values settle more slowly. Render-only: logical geometry and hit-testing stay at the pointer target. Disabled by `water_effects = false`. |
| `backdrop_capture_scale` | int, `1`–`4` | `1` | Linear downscale for the per-window backdrop capture that feeds frost glass, water glass, and layer-shell blur. `1` captures at native resolution (unchanged look). `2`/`4` allocate a texture with 1/4 or 1/16 the area — real VRAM/GPU savings with several glass windows open at once — at the cost of a visibly softer captured image once magnified back up to the window's size. Changes how the water identity looks, so it defaults to the unchanged behavior rather than a pre-picked value. Captures are also released automatically after their last output stops presenting the surface, including hidden Classic workspaces and off-camera Ocean windows. See `report.md`'s P-13/P-14 entries for the measurements behind this knob. |
| `cursor_always_visible` | bool | `false` | Forces the udev backend's software cursor to stay visible even when a client asks to hide it (e.g. a terminal hiding its own pointer glyph after inactivity). Off by default — respecting a client's own hide request is correct behavior; this is an opt-in override. |
| `cursor_hide_after` | duration | `0` | udev backend only: hides the software cursor after this long without real pointer motion (niri's `cursor.hide-after-inactive-ms`), e.g. `2s` or `2000ms`; `0` disables it. Independent of `cursor_always_visible` — that overrides a *client's* hide request, this is a compositor-driven idle timer, and the two can be combined. |
| `auto_back_and_forth` | bool | `false` | Re-selecting the already-active workspace jumps back to whichever one was active immediately before it, instead of no-opping (niri's own feature of the same name). |
| `workspace_name` | repeatable key | none | Names a workspace number for use in `workspace:<name>`/`move-to-workspace:<name>` (niri's `set-workspace-name`, Hyprland's `workspace name:foo`) — `workspace_name = 3 web`, repeat the key once per name. Purely an addressing convenience: the workspace's real identity is still its number. An unknown name at action time warns and no-ops rather than switching. |
| `gaps` | integer | `8` | Pixel gap the tiling engine applies around and between tiles, both layout algorithms. |
| `workspace_gaps` | repeatable key | none | Per-workspace gap override — `workspace_gaps = 3 0` (workspace 3, no gaps), repeat the key once per workspace. Accepts a `workspace_name` alias in place of the number. Beats both the output-level `gaps` override and the global `gaps`. |
| `workspace_count` | integer, `0`–`64` | `0` (off) | When set, workspaces `1..=N` are advertised as existing on every output even while empty — by the IPC `workspaces` query and the `ext-workspace-v1` protocol (see below), so a bar can show a persistent strip instead of only the workspaces that currently have windows. `0` restores the original behavior: only a workspace with a window, or the one currently active, is reported. Workspaces themselves stay lazily created either way — this only affects what gets advertised to external tooling, not `Layouts`' own on-demand tree creation, and a manual switch past `N` is still reported truthfully. |
| `layout` | `bsp` \| `master` \| `cascade` | `bsp` | Starting tiling algorithm for a workspace with no runtime override (see `layout:bsp`/`layout:master`/`layout:cascade` actions below). `bsp` is dwindle-style: split orientation follows each window's own aspect ratio. `master` is one master pane plus an evenly-split stack. `cascade` wraps windows into rows left to right, top to bottom, choosing the row count so the grid's shape best matches the output's own aspect ratio -- TideWM's own "fills the basin" mode. Row height and cell width are manually draggable the same way as BSP (an empty-gap border drag, or a modifier+body drag on the window), except a drag only ever redistributes the two immediate neighbors either side of it, not BSP's wider connected-vessel chain; opening or closing a window keeps a row's manual sizing as long as that row's own window count didn't change. |
| `master_side` | `left` \| `right` \| `top` \| `bottom` | `left` | Which side the master pane sits on under `default_layout = master`. `left`/`right` stack the other windows vertically in the remaining strip; `top`/`bottom` stack them horizontally instead. One global setting, not per-workspace. |
| `split_bias` | `auto` \| `horizontal` \| `vertical` | `auto` | Manual override for `default_layout = bsp`'s per-split axis choice. `auto` is the existing aspect-ratio-driven behavior, unchanged. `horizontal`/`vertical` force every split one way regardless of window/output shape (Hyprland dwindle's `force_split` idea). One global setting, not per-workspace. |
| `pseudo_tile_scale` | float, `0.05`–`1.0` | `0.7` | Fraction of its tile a pseudo-tiled window keeps, centered within it. Out-of-range values are clamped, not rejected. |
| `adaptive_sync` (aliases `vrr`, `variable_refresh_rate`) | `off` \| `on` \| `on-demand` | `off` | Global adaptive-sync (VRR) preference; a `[[output]]`'s own `adaptive_sync` beats it. **Config surface only right now** — the udev backend queries and logs each connector's real hardware VRR capability but does not yet actually enable it. See AGENT.md's config-gap-audit section for the full reasoning and what's deferred. |
| `spawn` | list | none | Commands launched once at startup, as a real list: `spawn = [waybar, "swaybg -i ~/wallpaper.png -m fill"]`. Args split on whitespace — no shell involved, so quoting/globs/pipes aren't supported; wrap in `sh -c "..."` yourself if you need those. |

### Workspace transitions

Workspace actions use a directional wave wipe while both `water_effects` and `workspace_transition.enabled` are true. The default `water` style is a full-screen transition: a blue body with moving caustic streaks, a curling foamy crest, and spray floods across the outgoing workspace; once water covers the whole output, it continues across to reveal the incoming workspace. The alternate `glow` style retains the slimmer colored sinusoidal boundary wipe. Cursor and compositor chrome remain live above either style.

TideWM captures the outgoing desktop after its submitted frame and keeps the incoming workspace live underneath the effect. State is bounded to one pending target and one transient ARGB8888 full-output texture per output (about 7.9MiB at 1080p or 31.6MiB at 4K), with newer switches replacing rather than queueing. Optional workspace motion captures the incoming desktop too, doubling that transient cost only while its slide runs; both textures are released when the transition ends. Both procedural styles are shader-only and allocate no other textures. Disabling either toggle makes workspace switching immediate.

The enable/duration/curve split follows niri’s useful per-animation configuration shape; the direction, speed, wavefront, and geometry controls are TideWM-specific. Values are snapshotted when a transition begins, so hot reload affects the next workspace switch.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Disables only workspace transitions; other water effects stay active. |
| `style` | `water` \| `glow` | `water` | `water` fills the output before revealing the new workspace. `glow` uses the original thin colored boundary. |
| `duration` | duration | `520` | Transition lifetime: `600ms`, `1.5s`, `0.6s`. |
| `speed` | float, `0.1`–`10` | `1.0` | Speed multiplier applied to `duration`: `2.0` is twice as fast and `0.5` is half speed. |
| `curve` | enum | `cubic-in-out` | Progress easing: `linear`, `cubic-out`, `cubic-in-out`, `quad-out`, or `exp-out`. `ease` is an alias. |
| `direction` | enum | `auto` | `auto` sweeps right-to-left for a higher-numbered workspace and left-to-right for a lower-numbered one. `left-to-right`/`ltr` and `right-to-left`/`rtl` force one direction. |
| `workspace_motion` | bool | `false` | Captures both desktops and slides the outgoing one out while the incoming one enters under the wave. Costs one additional transient full-output texture. `move_workspaces` is an alias. |
| `workspace_motion_delay` | duration | `150` | Delay after the water begins before both desktops start sliding, e.g. `150ms`. Values beyond 95% of the effective transition lifetime are clamped to that point. |
| `wave_amplitude` | float, `0`–`500` | `34` | Maximum horizontal displacement of the moving boundary in physical pixels. `0` produces a straight wipe. `amplitude` is an alias. |
| `wave_frequency` | float, `0`–`20` | `3` | Sine cycles from the output’s top edge to its bottom edge. `0` removes vertical waviness. `frequency` is an alias. |
| `edge_width` | float, `0.5`–`250` | `18` | Half-width of the soft cross-fade boundary in physical pixels. Lower is sharper; higher is softer. |
| `color` | color | `8EDDFF` | Main water color, or the wavefront tint under `glow`. Accepts bare `RRGGBB` or quoted `"#RRGGBB"`. |
| `wave_size` | float, `0`–`250` | `10` | Curl/lobe size under `water`, or colored core half-width under `glow`, in physical pixels. `size` is an alias. |
| `wave_alpha` | float, `0`–`1` | `0.9` | Colored core opacity under `glow`. `alpha` is an alias. |
| `glow_size` | float, `0`–`500` | `46` | `glow` style only: reach beyond the colored core in physical pixels. |
| `glow_alpha` | float, `0`–`1` | `0.25` | `glow` style only: outer glow opacity. |
| `water_depth` | float, `1`–`2000` | `260` | `water` style only: depth/shading scale and off-screen travel margin in physical pixels. `depth` is an alias. |
| `water_alpha` | float, `0`–`1` | `0.88` | `water` style only: opacity of the body while it fills the output. Lower values reveal more of the workspace beneath the water. |
| `foam_color` | color | `E8FCFF` | `water` style only: crest, spray, and caustic highlight color. Uses the same color formats as `color`. |
| `foam_size` | float, `0`–`250` | `18` | `water` style only: foamy crest width in physical pixels. |
| `foam_alpha` | float, `0`–`1` | `0.95` | `water` style only: crest and spray opacity. |
| `spray_amount` | float, `0`–`1` | `0.7` | `water` style only: density/opacity of droplets ahead of the entering crest. `spray` is an alias. |
| `turbulence` | float, `0`–`2` | `0.7` | `water` style only: strength of secondary crest harmonics and moving caustic streaks. |

```wave
transition {
    enabled = true
    style = water
    duration = 600ms
    speed = 1
    curve = cubic-out
    direction = auto
    workspace_motion = true
    workspace_motion_delay = 150ms
    wave_amplitude = 52
    wave_frequency = 2.5
    edge_width = 14
    color = 159DFF
    wave_size = 28
    wave_alpha = 0.9
    glow_size = 54
    glow_alpha = 0.3
    water_depth = 260
    water_alpha = 0.88
    foam_color = F2FDFF
    foam_size = 22
    foam_alpha = 1
    spray_amount = 0.8
    turbulence = 0.9
}
```

To retain the earlier boundary-only animation, set `style = glow`; the shared timing, direction, color, and geometry fields continue to apply, while the `wave_alpha`, `glow_size`, and `glow_alpha` fields control that style’s appearance.

### `cascade { }`

Adds a liquid lifecycle to the specific tiled window opening or closing in a
Classic workspace whose active layout is `cascade`. Other cells keep their
ordinary geometry reflow; floating windows, BSP/master workspaces, and Ocean
placements are unaffected. The effect samples each existing client texture
directly, including subsurfaces, using whole-window coordinates. Closing uses
the same bounded retained texture handles as `animations.close`, so it adds no
framebuffer or copied window texture.

The `wave` preset is the default balanced crest. `trickle` is narrower and
calmer; `splash` uses a larger turbulent front and sparse droplets; `none`
disables that lifecycle leg. The crest/noise functions are shared with the
full-output water transition, while the window mask remains a much cheaper
single surface pass. `water_effects = false` bypasses both legs.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `pour` | `wave` \| `trickle` \| `splash` \| `none` | `wave` | Reveal applied to a newly opened Cascade tile. |
| `drain` | `wave` \| `trickle` \| `splash` \| `none` | `wave` | Recede applied to the bounded close snapshot. |
| `replace_motion` | bool | `false` | `false` layers liquid over ordinary open/close geometry and opacity. `true` makes liquid the lifecycle motion; neighboring reflow animations are unchanged. |
| `pour_duration` | duration, `50ms`–`5s` | `240ms` | Pour lifetime. |
| `drain_duration` | duration, `50ms`–`5s` | `210ms` | Drain lifetime and snapshot retention time. |
| `curve` | enum | `cubic-out` | Progress easing: `linear`, `cubic-out`, `cubic-in-out`, `quad-out`, or `exp-out`; `ease` is an alias. |

```wave
cascade {
    pour = wave
    drain = wave
    replace_motion = false
    pour_duration = 240ms
    drain_duration = 210ms
    curve = cubic-out
}
```

### `animations { }`

Controls window opening, closing, and layout movement. TideWM applies focus,
input, and final layout geometry immediately; animation is only a visual
offset/opacity settling over that real state. Retargeting movement mid-flight
starts from the current on-screen rectangle, avoiding a snap to either the old
or new position or size. With `movement.animate_size = true` (the default),
client surfaces, subsurfaces, popups, rounded clipping, borders, shadows,
glass, and depth geometry resize together. This is a direct render-element
transform and allocates no intermediate framebuffer. Closing uses the last
already-imported surface textures recorded by the normal frame walk, so direct
xdg-role destruction and null-buffer unmap both remain drawable after Smithay
releases the live surface. Cloning these GPU handles does not allocate another
framebuffer or copy the window.

`preset` selects a complete baseline: `tide` (the calm default), `wave` (more
visible oscillation), `riptide` (short and sharp), `hypr-smooth`, or one of
the non-water motion packs: `silk`, `snappy`, `gentle`, `cinematic`, and
`minimal`. The five non-water packs enable smooth workspace and interactive
move/resize motion even when `water_effects = false`; all values remain
individually editable in Wave. Existing presets and the default keep those two
new controls disabled for compatibility. The
`hypr-smooth` preset mirrors the maintainer's real Hyprland window settings:
open geometry uses the 300ms `overshot` curve, close geometry uses the 300ms
`easeInOut` curve, both slide through the nearest output edge, both fade on a
separate 400ms `easeInOut` clock, and layout motion uses 400ms `easeInOut`.
Explicit values in the same block always override the preset, even when
`preset` appears later. The top-level `enabled` disables all animation groups.
`slowdown` multiplies both geometry and opacity durations (`0.5` is twice as
fast, `2` twice as slow). `max_closing_snapshots` (default `64`) caps detached
windows retained for close animation, while `close_snapshot_output_budget`
(default `2.0`) caps their estimated physical pixels to that multiple of the
live outputs' aggregate mode area. These are complementary: the count protects
against floods of tiny windows, and the output-relative area budget scales with
the actual nested, HiDPI, or multi-monitor session without assuming a screen
resolution. Setting either to `0` disables detached close snapshots. Each
`open`, `close`, and `movement` sub-blocks support:

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Disables only this transition. |
| `animate_size` | bool | movement `true`, lifecycle `false` | Interpolates the outer window size together with movement. It currently affects the `movement` block; `resize`, `size`, and `animate-size` are aliases. |
| `duration` | duration | open `190ms`, close `160ms`, movement `190ms` | Geometry lifetime before the visual state reaches its logical target. |
| `curve` | easing | see example | `linear`, `quad-out`, `cubic-out`, `cubic-in-out`, `exp-out`, or CSS-compatible `cubic-bezier(x1,y1,x2,y2)`. `ease-out-quad`, `ease-out-cubic`, and `ease-out-expo` aliases are accepted. |
| `opacity_duration` | duration | follows `duration` | Independent opacity lifetime. |
| `opacity_curve` | easing | follows `curve` | Independent opacity easing. Accepts the same values as `curve`; `fade_curve` and `opacity_ease` are aliases. |
| `origin` | direction | `offset` | `offset` uses the configured travel. `nearest-edge` mirrors an unforced Hyprland `slide`; `top`, `right`, `bottom`, and `left` force an output edge. `slide_from`, `slide-from`, and `direction` are aliases. |
| `offset` | `<x>x<y>` | open `0x24`, close `0x18` | Opening begins at this logical-pixel offset and settles to zero. Closing travels from zero to this offset. Used when `origin = offset`; movement derives its offset from old/new geometry. `travel` is an alias. |
| `from_opacity` | float, `0`–`1` | open `0.28`, close/movement `1` | Opacity at transition start. |
| `to_opacity` | float, `0`–`1` | open/movement `1`, close `0` | Opacity at transition end. |
| `effect` | `glide`, `tide`, or `wave` | `tide` | `glide` follows only the eased path. `tide` adds one broad perpendicular swell. `wave` adds a decaying oscillation. `motion` is an alias. |
| `wave_amplitude` | float, `0`–`512` | open `4`, close `2.5`, movement `1.25` | Maximum perpendicular displacement in logical pixels. `amplitude` is an alias. |
| `wave_cycles` | float, `0`–`8` | `0.5` | Oscillations across the lifetime for `wave`; `tide` always uses one broad swell. `cycles` and `frequency` are aliases. |
| `wave_decay` | float, `0`–`8` | varies | How quickly the extra water trajectory settles near the target. `decay` is an alias. |

```wave
animations {
    preset = tide
    enabled = true
    slowdown = 1.0
    max_closing_snapshots = 64
    close_snapshot_output_budget = 2.0

    open {
        duration = 190ms
        curve = cubic-bezier(0.16,1,0.3,1)
        origin = offset
        offset = 0x24
        from_opacity = 0.28
        to_opacity = 1
        effect = tide
        wave_amplitude = 4
        wave_decay = 2.2
    }

    close {
        duration = 160ms
        curve = cubic-out
        origin = offset
        offset = 0x18
        from_opacity = 1
        to_opacity = 0
        effect = tide
        wave_amplitude = 2.5
        wave_decay = 2
    }

    movement {
        animate_size = true
        duration = 190ms
        curve = cubic-bezier(0.16,1,0.3,1)
        effect = tide
        wave_amplitude = 1.25
        wave_decay = 2.4
    }

    workspace {
        enabled = false
        style = slide-fade
        duration = 220ms
        curve = cubic-bezier(0.16,1,0.3,1)
        travel = 0.2
    }

    interactive {
        enabled = false
        half_life = 28ms
    }
}
```

For a completely non-water setup with smooth motion, the short form is:

```wave
water_effects = false
animations { preset = silk }
```

`workspace { }` is a non-water transition drawn from one temporary outgoing
workspace snapshot over the live incoming workspace. `style` accepts `slide`,
`slide-fade`, or `fade`; `duration` and `curve` use the same forms as window
motion. `travel` is a `0`–`1` fraction of the output's live physical width,
not a fixed pixel distance. At most one snapshot is retained per output, it is
dropped when the transition ends or the output disappears, and the existing
water workspace transition takes precedence when both are enabled.

`interactive { }` smooths pointer-driven move and resize without enabling the
water identity. `half_life` is the refresh-independent exponential settling
time (`0ms` makes the path immediate). It keeps one small record per actively
moving window and allocates no texture or framebuffer.

### Interactive viscosity

The legacy top-level `viscosity` controls TideWM's liquid drag and resize feel
independently of the fixed-duration `animations { movement { } }` transition.
Pointer grabs update the real window position, resize target, layout ratios,
and hit-testing immediately. The rendered window rectangle follows with
refresh-rate-independent exponential damping, including floating move/resize,
tiled drag-to-swap, direct split-border resize, and modifier-drag tiled resize.
Repeated pointer events retarget from the current on-screen rectangle, so
motion stays continuous.

The state is bounded to one small record per moving window and stores no pointer
history, textures, or framebuffers. `0` disables it, `1.0` is the default,
and values up to `4.0` progressively slow settling. `water_effects = false`
bypasses this legacy route. When `animations.interactive.enabled = true`, its
direct `half_life` takes precedence and works regardless of `water_effects`.
A matching `rule { viscosity = ... }` overrides only the legacy liquid value
for one app.

### `vessels { }` (legacy: `connected_vessels { }`)

Connected-vessel resize spreads BSP pressure beyond the nearest split. The
split selected by a direct border drag, modifier-right-drag, or keyboard resize
receives the full displacement. Parallel ancestor splits receive progressively
less pressure according to `falloff ^ tree-distance`, so nearby tiles move more
than distant ones. Perpendicular ancestors are untouched.

Window-body and keyboard resize track which child contains the target window,
so positive horizontal/vertical motion grows it consistently even when it is
the second child of a split. A direct border drag retains literal pointer
direction. Ratios and spans are captured once at gesture start, and topology or
split-axis changes invalidate the handles instead of retargeting another node.
The implementation stores only a short handle vector for an active grab and
allocates no render resources.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Enables spatial BSP resize propagation. `false` restores the original one-split path without disabling viscosity or other water effects. |
| `falloff` | float, `0`–`1` | `0.5` | Pressure retained per ancestor tree level. `0` is equivalent to one-split resize. `damping` is an alias. |
| `max_splits` | integer, `1`–`8` | `4` | Maximum handles per resized axis, including the primary split. `depth` is an alias. |

```wave
connected_vessels {
    enabled = true
    falloff = 0.5
    max_splits = 4
}
```

`water_effects = false` is the master bypass and also restores one-split BSP
resize. Master/stack layout has no BSP split chain, so this block has no effect
there.

### `sway { }`

Optional lateral sway for floating windows while they are dragged, like they
are sitting in water. Each horizontal drag step kicks a damped oscillation
that offsets only what is drawn; the window's real position, focus, and
hit-testing always follow the pointer immediately. Once the drag stops, the
offset swings side to side and decays back to rest on its own, then stops
consuming frames entirely.

The effect is explicitly opt-in: `enabled` defaults to `false`, and a matching
`rule { sway = true }` opts a single app in (or out) without changing the
global value. `water_effects = false` bypasses it regardless of this block.
State is one small closed-form record per swaying window — no motion history,
textures, or framebuffers.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch for the mechanic. A per-app `rule { sway = ... }` overrides it. |
| `response` | float, `0`–`1` | `0.08` | Fraction of each horizontal drag step converted into sway displacement. `0` freezes the effect. `gain` is an alias. |
| `max_offset` | float, `0`–`128` | `24.0` | Hard cap on lateral displacement in logical pixels. `amplitude` is an alias. |
| `frequency` | float, `0.1`–`10` | `1.6` | Oscillations per second. |
| `damping` | float, `0.1`–`20` | `3.0` | Exponential decay rate; higher settles back sooner. |

```wave
sway {
    enabled = true
    response = 0.08
    max_offset = 24
    frequency = 1.6
    damping = 3.0
}
```

### `currents { }`

Ocean-only ambient current for floating windows. An unfocused floater follows
a slow, bounded downstream eddy in render space; its authoritative Ocean
rectangle, focus target, pointer hit testing, and IPC geometry never move.
Focusing or directly dragging the window pauses its phase and eases the visual
offset back to zero. Unfocusing fades it gently back into the same flow instead
of jumping or catching up the time spent paused.

This effect is explicitly opt-in because an eligible visible window keeps the
frame pump active. Tiled, fullscreen, and screen-pinned windows are excluded,
as are all Classic workspaces. State is one small phase record per visible
eligible floater, with no texture, framebuffer, or motion history.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Enables currents in Ocean. `water_effects = false` remains the master bypass. |
| `direction` | cardinal or degrees | `right` (`0`) | `right`/`east`, `down`/`south`, `left`/`west`, `up`/`north`, or any numeric angle. Screen coordinates use clockwise-positive degrees. |
| `strength` | float, `0`–`64` | `10.0` | Maximum render-offset envelope in logical pixels. `0` disables animation work. |
| `period` | duration, `1s`–`120s` | `14s` | Time for one smooth downstream eddy. |

```wave
currents {
    enabled = true
    direction = right
    strength = 10
    period = 14s
}
```

### `buoyancy { }`

Weighted buoyancy gives floating windows an apparent mass without moving their
real rectangles. An unfocused floater sinks by its configured weight; focus or
direct dragging eases it back to zero so the drawn window meets its input
geometry while it is being used. Classic and Ocean both get the sink. In Ocean,
weight also reduces the render-only contribution from currents and floating
physics, so a heavy window follows the same flow less than a light one.

The effect is opt-in, floating-only, and excludes fullscreen and pinned
windows. Tiled placement remains owned by the layout. At rest it consumes no
frames: only the short focus/drag transition animates. `water_effects = false`
is the master bypass.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Enables weighted buoyancy in both Classic and Ocean. |
| `default_weight` | float, `0`–`1` | `0.35` | Weight inherited by floaters without a matching `rule { weight }`. `0` opts a window out. |
| `max_sink` | float, `0`–`64` | `18.0` | Downward render offset at weight `1`, in logical pixels. |
| `settle` | duration, `0ms`–`5s` | `240ms` | Approximate focus/drag transition duration. `0ms` changes immediately. |
| `flow_reduction` | float, `0`–`1` | `0.65` | Ocean-only reduction of currents/physics at weight `1`; `0` preserves full response. |

```wave
buoyancy {
    enabled = true
    default_weight = 0.35
    max_sink = 18
    settle = 240ms
    flow_reduction = 0.65
}

rule {
    app_id = pavucontrol
    weight = 0.85
}
```

### `swim { }`

Continuous lateral navigation between workspaces: a horizontal trackpad swipe
pans the viewport continuously instead of the ordinary one-shot discrete
switch. Workspace identity is unchanged -- still the ordinary `u32` number --
only the visual camera offset while dragging is continuous. A drag past the
half-spot mark advances the real workspace live; releasing springs the
residual offset back to zero over `snap_duration_ms`. Pressing against either
end of the workspace axis (workspace 1, or `u32` overflow) resists rather than
wrapping into nothing.

Driven by the same `wp-pointer-gestures` compositor-consumed swipe path
`[input.touchpad] workspace_swipe_fingers`/`workspace_swipe_distance` already
use, so those two settings still apply: `workspace_swipe_distance` is one full
spot-width of swipe travel, and `workspace_swipe_fingers` picks which
finger-count swipe drives it. Adjacent tiled, floating, fullscreen, maximized,
pseudo-tiled, and grouped-window content slides into view during the pan.
Those windows are rendered directly from their retained workspace ownership;
they are not mapped into the active `Space`, so focus, hit testing, IPC
visibility, and the discrete workspace authority do not change before the
half-spot crossing. Only strips intersecting the viewport are assembled, and
none are assembled at rest. Gesture events are real-libinput-touchpad-only
(the udev backend), never emitted under the nested winit backend used for
day-to-day development, so the drag-driven path requires a real touchpad pass.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch. `false` falls back to the ordinary discrete workspace switch (and its wave transition, if enabled). |
| `neighbors` | integer, `1`–`4` | `1` | Maximum adjacent workspace distance available to the render-only preview on each side. Only viewport-intersecting strips are assembled; `1` covers ordinary pans. `window` is an alias. |
| `response` | float, `0.1`–`4` | `1.0` | Swipe-to-offset gain. `1.0` maps one `workspace_swipe_distance` of travel to one spot-width of camera motion. `gain` is an alias. |
| `snap_duration_ms` | integer, `0`–`2000` | `220` | Spring-to-rest animation length after the fingers lift. `snap_ms` is an alias. |

```wave
swim {
    enabled = false
    neighbors = 1
    response = 1.0
    snap_duration_ms = 220
}
```

`water_effects = false` is the master bypass, same as `sway`/`viscosity`.

### `ocean { }`

Configures the workspace-free Ocean engine selected with
`spatial_engine = ocean`. Windows have stable world rectangles on both X and
Y. Each output stores an independent continuous camera into that same world;
moving a camera never moves or resizes a window. Reefs are named local BSP
tiling zones, not pages, and bookmarks are named camera return points. The
optional camera-anchored guide field moves and scales with the world, so empty
travel remains legible instead of looking like windows sliding over a fixed
wallpaper.

Rendering derives one placement snapshot from each output's current camera and
shares it across every consumer in that render pass, including glass capture,
tab strips, and final composition. Reefs outside that camera are rejected
before their BSP trees are walked. Whole-world features such as the minimap
still inspect every reef, and no placement snapshot persists across frames.

With no `reef` declaration TideWM creates `main` at `0x0`. Its dimensions come
from the real logical output viewport—there is no 1080p resolution constant.
An explicitly declared reef may omit either dimension to inherit and expand
to the largest real viewport that uses Ocean, or set a positive dimension to
make that world zone intentionally fixed/larger. Numbered `workspace:N`
actions become compatibility jumps to reef/bookmark `N`; they do not create
workspaces. Reef and configured-bookmark declarations are startup-owned;
runtime saved bookmarks last for the session. The remaining Ocean camera,
zoom, guide, and depth toggles/tuning hot-reload.

```wave
spatial_engine = ocean
ocean {
    freeform_windows = true
    canvas_pan_button = left
    canvas_pan_requires_modifier = false
    camera_step = 480
    camera_animation_ms = 260
    camera_sway = 18
    canvas_guides = true
    canvas_grid_size = 240
    canvas_grid_alpha = 0.10
    canvas_marker = true
    canvas_marker_fade_ms = 4200
    zoom_enabled = true
    modifier_zoom = true
    min_zoom = 0.25
    max_zoom = 2.0
    zoom_step = 1.2
    depth_enabled = true
    reef main {
        x = 0
        y = 0
        # width/height omitted: use real output geometry
    }
    reef code {
        x = 4000
        y = 0
        width = 3440
        height = 1440
    }
    bookmark home {
        x = 0
        y = 0
    }
}
```

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `freeform_windows` | boolean | `true` | In Ocean, beginning the configured move/resize gesture on a reef tile detaches it at the same world rectangle and continues as a free zoom-aware drag. `toggle-floating` can tile it into a reef again. `false` retains tile swap/split resize behavior. |
| `smart_tiling` | boolean | `true` | Keeps modifier-left drags of tiled Ocean windows in the reef for tile-to-tile swaps, and reattaches a floating window released close to an existing tile. The dragged window lifts out and follows the pointer, and the tile it would swap into on release gets an active-border magnet highlight. |
| `smart_tiling_snap_distance` | integer, `0`–`512` | `64` | Screen-pixel distance at which a floating Ocean window attaches to a nearby tiled window on release. `0` requires overlap. |
| `smart_tiling_preserve_size` | boolean | `true` | Keeps the floating window's current size after smart reattachment. The window remains a member of the reef tiling tree while its custom size is rendered around its tile slot. |
| `canvas_pan_button` | `left` \| `middle` \| `right` \| `none` | `left` | Button that directly drags genuinely empty Ocean canvas. `none` disables mouse camera grabbing and reserves no button. Layer surfaces and windows keep their clicks. |
| `canvas_pan_requires_modifier` | boolean | `false` | When true, empty-canvas dragging also requires the currently configured `pointer_modifier`; false gives direct Drift-style canvas movement. |
| `camera_step` | integer, `32`–`8192` | `480` | Logical pixels moved by an `ocean-pan-*` keyboard action. Hot-reloadable. |
| `camera_animation_ms` | integer, `0`–`5000` | `260` | Smooth pan/zoom/bookmark travel duration. `0` makes camera actions immediate. |
| `camera_sway` | float, `0`–`256` | `18` | Small perpendicular arc, in screen pixels, during keyboard camera travel. `0` keeps a straight path. |
| `canvas_guides` | boolean | `true` | Draw the world-anchored adaptive reference grid behind windows. Independent from water effects and depth. |
| `canvas_grid_size` | integer, `32`–`8192` | `240` | Logical world units between guide lines; density adapts as the camera zooms out. |
| `canvas_grid_alpha` | float, `0`–`1` | `0.10` | Guide visibility. `0` is also a complete visual bypass. |
| `canvas_marker` | boolean | `true` | Show a small point at the viewport center only after the Ocean camera moves. Independent from the grid. |
| `canvas_marker_fade_ms` | integer, `0`–`30000` | `4200` | Inactivity fade duration for the center point. `0` disables it. |
| `zoom_enabled` | boolean | `true` | Enables all Ocean zoom actions and the modifier-wheel gesture. When disabled the live camera is held at `1.0`. |
| `modifier_zoom` | boolean | `true` | With zoom enabled, physically holding `pointer_modifier` while scrolling zooms around the pointer. |
| `min_zoom` / `max_zoom` | float, `0.05`–`8` | `0.25` / `2.0` | Camera scale limits. |
| `zoom_step` | float, `1.01`–`3` | `1.2` | Multiplicative keyboard/wheel zoom step. |
| `depth_enabled` | boolean | `true` | Enables Ocean sink/dredge/surface and Depth Up/Down. Does not affect the 2D camera or zoom. |
| `reef <name> { x, y, width?, height? }` | nested block | implicit `main` | Local tiling zone in world coordinates. Omitted dimensions follow actual output geometry. |
| `bookmark <name> { x, y }` | nested block | `home = 0x0` | Named camera top-left position. Reefs also synthesize numeric bookmarks in declaration order for `workspace:N` compatibility. |

### `compass { }`

Bioluminescent edge-glow compass for the Ocean engine (spatial roadmap S5).
When a window sits outside the output camera's viewport, a soft glow appears
at the viewport edge in that window's direction, keeping off-screen windows
discoverable without navigating to them:

- **Urgent** windows glow bright cyan in any direction (left, right, up,
  down, or diagonally toward a corner).
- **Deep** windows (sunk via `sink-window`, or sitting in a lower reef)
  glow cool blue in any direction, same as urgent.

Nearer windows glow brighter; the cue fades linearly to nothing at
`max_distance`. The cues are ambient and render-only: they do not respond
to clicks, and camera travel stays on the existing pan/zoom/bookmark/depth
actions. No element is produced at all when nothing is off-screen, so an
idle desktop ticks zero frames. One analytical shader, no texture or
framebuffer, and a 16-cue cap (urgent first, then nearest).

Ocean-only; `water_effects = false` disables the compass regardless of this
block. Has no effect under the Classic engine.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Master switch for the compass under Ocean. |
| `urgent_color` | color | `76F1FF` | Glow color for off-screen urgent windows. `urgent` is an alias. |
| `deep_color` | color | `2D7096` | Glow color for windows below the viewport. `deep` is an alias. |
| `max_distance` | float, `> 0` | `3000` | World-logical pixels beyond the viewport edge at which a cue fades to zero. |
| `size` | float, `8`–`1024` | `96` | Glow rect side, logical pixels. |
| `alpha` | float, `0`–`1` | `0.85` | Peak glow alpha at zero distance. `peak_alpha` is an alias. |
| `shape` | string | `circle` | Cue shape: `circle`/`glow`/`blob`, `arrow`/`triangle`, `chevron`/`wedge`, `ring`, `diamond`. Arrows and chevrons point toward the window. |

```wave
compass {
    enabled = true
    urgent_color = 76F1FF
    deep_color = 2D7096
    max_distance = 3000
    size = 96
    alpha = 0.85
    shape = circle
}
```

### `minimap { }`

Whole-world overview minimap for the Ocean engine (spatial roadmap S5's
other half, alongside the compass). Hold the configured `key` to peek: a
schematic map of every window in the shared world, plus every connected
output's current camera viewport (the triggering output's own viewport
drawn with the active accent color, every other output's viewport plainer),
scaled to fit the screen. Click a window or region while still holding to
travel that output's camera there and dismiss the peek; release without
clicking just dismisses it, same place you started.

Built once per peek rather than every frame. Screen-pinned windows aren't
drawn (a pin is glued to one output's screen space, not a world location,
so it has nothing to show on a world map). An urgent window highlights in
the same accent color as the triggering output's own "you are here"
viewport glow.

Ocean-only. Unlike the compass, **not** gated by `water_effects` -- the
minimap reads as navigation utility rather than a visual effect, so it
stays available with water off.

`preset` picks a visual baseline; `background_color`/`window_color`/
`accent_color` still override individual colors on top of whichever preset
is active, the same shape `compass`'s `shape` + `urgent_color`/`deep_color`
already uses.

- `plain` -- the original flat schematic: sharp corners, no glow, same
  dark-panel/labeled-box language as `toggle-overview`'s Classic grid.
- `bioluminescent` (default) -- deep-water gradient backdrop, rounded
  glassy window boxes, and a cyan/teal glow rim, matching the compass's
  own bioluminescent palette.
- `glass` -- frosted, low-contrast rounded panels with a neutral drop
  shadow instead of a colored glow, for a subtler look.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Master switch for the minimap under Ocean. |
| `key` | chord string | `Super+Space` | Hold this chord to peek. A single modifier+key combo (no multi-key helper chords). |
| `preset` | string | `bioluminescent` | Visual baseline: `plain`, `bioluminescent`, or `glass`. |
| `background_color` | color, optional | preset default | Overrides the backdrop. `background` is an alias. |
| `window_color` | color, optional | preset default | Overrides window box fill. `window` is an alias. |
| `accent_color` | color, optional | preset default | Overrides the viewport-beacon/urgent-highlight/glow color, and the window border. `accent` is an alias. |

```wave
minimap {
    enabled = true
    key = "Super+Space"
    preset = bioluminescent
}
```

### `classic_depth { }`

Enables the Classic spatial engine's per-workspace Depth Deck. This is
structural parking, separate from the automatic visual cooling in `depth { }`,
and defaults off. `depth-down`/`depth-up` are the main workspace-like path:
they rotate the focused tile forward/backward through that workspace's deck
without opening a modal UI. The first Down on an empty deck parks the focused
window and reveals the next surface tile. `sink-window` performs that park
explicitly. It removes the focused ordinary tiled window from
the active layout while keeping its client alive and owned by the same real
workspace. `dive` opens a title-card deck for that workspace. Selecting a card
replaces the focused surface tile exactly and parks the displaced window in the
same slot; with no compatible focused tile, it restores the selected window as
an ordinary tile.

Grouped, fullscreen, maximized, pinned, pseudo-tiled, and floating windows are
left unchanged in this first version. Disabling the block on hot reload restores
every parked window to its owning workspace tree, so opting out cannot strand a
client. This switch is independent of both `water_effects` and `depth.enabled`:
plain deck, visual-only depth, both, and neither are all supported.

Direct switches use a distinct analytical pressure wave: Down travels from the
top of the output to the bottom; Up reverses it. Unlike the workspace water
wipe, this is a narrow undulating crest with wake bands and bubbles, does not
capture either workspace/window into a texture, and adds no retained framebuffer.
Set `animation = false` (or duration `0`) for an immediate switch.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch. When false, deck actions are removed from keyboard matching and remain inert over IPC. |
| `animation` | bool | `true` | Enables the vertical pressure-wave switch cue. |
| `animation_duration_ms` | duration | `420` | Sweep duration; `0` is immediate. |
| `wave_color` | color | `3EC4E0` | Pressure crest/wake tint. `color` is an alias. |
| `wave_alpha` | float, `0`–`1` | `0.72` | Transition strength. `alpha` is an alias. |

```wave
classic_depth {
    enabled = true
    animation = true
    animation_duration_ms = 420
    wave_color = 3EC4E0
    wave_alpha = 0.72
}

# Added automatically while enabled unless those combos are already used:
# $mod+D       = depth-down
# $mod+Shift+D = depth-up
# $mod+Ctrl+D  = dive
```

### `depth { }`

Configures automatic attention depth and buoyancy. A mapped window starts at the surface. After `sink_after_ms` without focus or keyboard input it moves to tier 1, keeping its live content with reduced opacity and a cool-water wash. Each additional `tier_interval_ms` moves it one tier deeper, capped by `max_tier`; tier 2 and below use a cached box-and-title schematic instead of live client pixels. Focusing, clicking, or typing into the window returns it to tier 0 immediately. Urgent windows retain a bright bioluminescent border at every tier.

Depth state is bounded to one small record per mapped toplevel. Schematic buffers exist only for visible tier-2-or-deeper windows and are evicted when a window resurfaces, unmaps, or is destroyed. The inactivity scan reuses the backend’s bounded timer and is throttled to 10Hz. `water_effects = false` disables the model regardless of this block.

A matching `rule { depth = false }` pins that window buoyant: it stays at tier 0 forever, regardless of inactivity, useful for a widget or player you always want live. `rule { depth = true }` affirms the normal automatic behavior (mainly useful to override an earlier matching rule's `false`). Last matching rule wins, same as `sway`/`viscosity`/`glass`.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Disables only automatic depth/buoyancy; other water effects remain active. |
| `sink_after_ms` | integer, `0`–`86400000` | `30000` | Inactivity before entering tier 1. `delay_ms` is an alias. |
| `tier_interval_ms` | integer, `1`–`86400000` | `30000` | Additional inactivity per deeper tier. `interval_ms` is an alias. |
| `max_tier` | integer, `1`–`8` | `2` | Deepest tier a window may reach. `tiers` is an alias. |
| `tier_one_alpha` | float, `0`–`1` | `0.78` | Live client-content opacity at tier 1. `live_alpha` is an alias. |
| `cool_color` | color | `2D7096` | Tier-1 water wash. Uses the transition/ripple color formats. |
| `cool_alpha` | float, `0`–`1` | `0.24` | Tier-1 wash opacity. |
| `schematic_color` | color | `102330` | Background color for tier-2-and-deeper title cards. |
| `schematic_alpha` | float, `0`–`1` | `0.9` | Title-card background opacity. |
| `border_color` | color | `52A6C6` | Normal title-card border. |
| `urgent_color` | color | `76F1FF` | Bioluminescent urgent border color at any tier. |
| `urgent_alpha` | float, `0`–`1` | `0.95` | Bioluminescent border opacity. |

```wave
depth {
    enabled = true
    sink_after_ms = 30000
    tier_interval_ms = 30000
    max_tier = 2
    tier_one_alpha = 0.78
    cool_color = 2D7096
    cool_alpha = 0.24
    schematic_color = 102330
    schematic_alpha = 0.9
    border_color = 52A6C6
    urgent_color = 76F1FF
    urgent_alpha = 0.95
}
```

### `frost { }`

Configures Phase R2 frosted glass. A tiled or floating window selects it with `glass = frost` in a matching `rule { }`. The preferred path is client-provided background transparency (for example Kitty’s `background_opacity`): the client keeps text/foreground pixels opaque while TideWM blurs what its transparent background reveals. A TideWM `opacity` rule remains available, but it multiplies the entire surface, including text. For backward compatibility, an `opacity` below `1.0` with no explicit glass mode still selects water refraction. `glass = none` keeps ordinary compositor transparency without a captured-backdrop shader. `water_effects = false` bypasses both modes.

The frost shader uses a fixed-cost 25-tap Gaussian kernel over the existing window-sized backdrop capture, followed by adjustable strength, saturation, vibrancy, contrast, brightness, noise, and tint treatment. Its `liquid` control adds edge-local inward refraction, a soft top-left highlight, and restrained opposite-edge shade so the material reads as a pane with optical thickness rather than a flat blur. This treatment is inspired by Apple's Liquid Glass material and Hyprland's practical contrast/vibrancy/noise blur controls, but remains a static, bounded shader: changing these values does not allocate more buffers, increase the tap count, or keep an idle output repainting.

Capture runs immediately before the visible output bind so interactive moves sample the current window geometry in the same frame; one reusable ARGB8888 texture is kept per eligible window and resized only when the visible dimensions change. Tiled glass samples the shared behind-scene inside its tile while configured gaps remain untouched. Classic and Ocean use the same placement-aware path, including Ocean camera transforms. Every key in this table also works inside a matching `rule { frost { } }` sub-block; omitted per-app keys inherit the global value.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Disables frost windows without changing water glass or their opacity rules. |
| `liquid` | float, `0`–`1` | `0.6` | Optical edge thickness: local refraction plus directional highlight/shade. `0` disables the liquid rim while retaining ordinary frost. `liquid_strength` is an alias. |
| `radius` | float, `0`–`64` | `16` | Maximum blur reach in physical pixels. `blur_radius` is an alias. Zero disables diffusion. |
| `strength` | float, `0`–`1` | `1.0` | Mix of sharp capture to fully diffused result. `blur_strength` and `frost` are aliases. |
| `opacity` | float, `0`–`1` | `1.0` | Opacity of the processed backdrop layer. Lower values mix the frost with the real, sharp desktop beneath it. `glass_opacity` and `background_opacity` are aliases. This does not change client text/content opacity. |
| `saturation` | float, `0`–`2` | `1.0` | `0` is grayscale, `1` preserves the captured color, values above `1` increase color. |
| `contrast` | float, `0`–`2` | `0.92` | Contrast modulation; the slightly softened default follows the practical Hyprland-style material treatment. |
| `brightness` | float, `0`–`2` | `1.04` | Multiplier applied after saturation. |
| `noise` | float, `0`–`0.25` | `0.008` | Static grain that can hide banding in smooth blurred gradients. `grain` is an alias. |
| `noise_scale` | float, `0.25`–`16` | `1.0` | Physical size of the noise cells. `grain_scale` is an alias. |
| `vibrancy` | float, `0`–`1` | `0.16` | Extra saturation beyond the base `saturation`. |
| `vibrancy_darkness` | float, `0`–`1` | `0.35` | Biases extra vibrancy toward darker pixels. |
| `tint_color` | color | `8EDDFF` | Optional frost tint. `color` is an alias. It has no effect while `tint_alpha = 0`. |
| `tint_alpha` | float, `0`–`1` | `0.04` | Strength of the tint mix. The default adds a very light aqua cast; set `0` for neutral glass. |
| `corner_radius` | float, `0`–`256` | `0` | Rounded clipping radius in physical pixels. `rounding` is an alias. |
| `corner_softness` | float, `0.25`–`8` | `1.0` | Antialias/feather width at the rounded edge, in physical pixels. |

```wave
frost {
    enabled = true
    liquid = 0.6
    radius = 16
    strength = 1.0
    opacity = 1.0
    saturation = 1.0
    contrast = 0.92
    brightness = 1.04
    noise = 0.008
    noise_scale = 1.0
    vibrancy = 0.16
    vibrancy_darkness = 0.35
    tint_color = 8EDDFF
    tint_alpha = 0.04
    corner_radius = 0
    corner_softness = 1.0
}

rule {
    app_id = kitty
    float = true
    glass = frost
    frost {
        radius = 20
        opacity = 0.9
        noise = 0.015
        tint_alpha = 0.0
        corner_radius = 12
    }
}
```

For the example above, configure Kitty with `background_opacity 0.72` (or launch it with `kitty -o background_opacity=0.72`). Add TideWM’s own `opacity = 0.72` only when intentionally making the complete client surface—including its text—translucent.

### `shadow { }`

Configures Phase R2 compositor-drawn drop shadows. TideWM combines niri’s CSS-like `softness`, signed `spread`, `offset`, and draw-behind behavior with Hyprland’s `render_power`, `sharp`, and `scale` controls. The shader is an analytical rounded-rectangle distance field: it allocates no texture or framebuffer and adds no cache whose size can grow with time. Shadows are inserted immediately behind their own window in real front-to-back z-order, including screenshot, screencast, backdrop, and workspace-transition capture paths.

Shadows are ordinary decoration and therefore independent of `water_effects`. The default `draw_behind_window = false` is especially important for transparent/frosted apps: TideWM cuts the actual window body out of the shadow, so the shadow cannot become a dark or colored filter over the client. Set it to `true` only when a client’s unknown rounded CSD shape would otherwise leave artifacts.

Every key also works inside `rule { shadow { } }`; omitted fields inherit the global block and multiple matching shadow sub-blocks merge field by field. Colors accept `RRGGBB`, `RRGGBBAA`, quoted `"#RRGGBBAA"`, and legacy `0xAARRGGBB`.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Master switch for this scope. A rule can re-enable or disable it per app. |
| `softness` | float, `0`–`256` | `28` | Soft falloff reach in logical pixels. `range` and `size` are aliases. Zero gives a hard edge. |
| `spread` | float, `-128`–`256` | `2` | CSS-style expansion before falloff; negative values contract the shadow. |
| `offset` | pair | `0x8` | Logical-pixel X/Y offset. Accepts `<x>x<y>`, `x y`, or `{x, y}`. `offset_x`/`offset_y` can override one axis. |
| `scale` | float, `0`–`1` | `1.0` | Scales the base shadow rectangle around the window center. |
| `render_power` | float, `1`–`8` | `2` | Higher values make the falloff disappear faster. `falloff_power` is an alias. |
| `sharp` | bool | `false` | Bypasses the soft falloff and draws a hard shadow shape. |
| `draw_behind_window` | bool | `false` | If false, cuts the window body out to protect transparent content from tinting. `draw_behind` is an alias; historical Hyprland `ignore_window` is accepted with inverse meaning. |
| `color` | RGBA color | dark blue-black, alpha `0.48` | Focused shadow color. `active_color` is an alias. |
| `inactive_color` | RGBA color | dark blue-black, alpha `0.30` | Unfocused shadow. `color_inactive` is an alias. |
| `urgent_color` | RGBA color | aqua, alpha `0.72` | TideWM extension: bioluminescent attention shadow. `color_urgent` is an alias. |
| `opacity` | float, `0`–`1` | `1.0` | Extra focused alpha multiplier, applied after color alpha. `active_opacity` is an alias. |
| `inactive_opacity` | float, `0`–`1` | `1.0` | Extra unfocused alpha multiplier. |
| `urgent_opacity` | float, `0`–`1` | `1.0` | Extra urgent alpha multiplier. |
| `corner_radius` | float, `0`–`256` | `0` | Rounded shadow geometry in logical pixels. `rounding` is an alias. |
| `floating_only` | bool | `false` | When true, tiled windows do not get compositor shadows. |
| `fullscreen` | bool | `false` | Enables fullscreen shadows. Usually wasted beyond the output edge, so off by default. |

```wave
shadow {
    enabled = true
    softness = 28
    spread = 2
    offset = 0x8
    scale = 1.0
    render_power = 2
    sharp = false
    draw_behind_window = false
    color = 040E137A
    inactive_color = 03080C4D
    urgent_color = 2EC7FFB8
    opacity = 1.0
    inactive_opacity = 1.0
    urgent_opacity = 1.0
    corner_radius = 0
    floating_only = false
    fullscreen = false
}

rule {
    app_id = kitty
    shadow {
        softness = 36
        spread = 4
        offset_y = 12
        color = 04131A80
        corner_radius = 12
    }
}
```

### `rounding { }`

Controls compositor-owned window geometry. TideWM clips the main toplevel surface and its subsurfaces while leaving real xdg-popups independent, matching niri’s `geometry-corner-radius` plus `clip-to-geometry` behavior. The same resolved radius drives borders, water/frost glass, and shadows so transparent corners cannot reveal a square effect layer underneath. Values are logical pixels and scale with the output.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Enables rounded geometry. |
| `radius` | one or four floats | `12` | One radius for all corners, or CSS order: top-left, top-right, bottom-right, bottom-left. `radii` and `geometry_corner_radius` are aliases. |
| `top_left`, `top_right`, `bottom_right`, `bottom_left` | float | inherited | Sparse per-corner overrides. |
| `power` | float, `1`–`10` | `2` | Superellipse exponent. `2` is circular; higher values produce squarer Hyprland-style corners. `rounding_power` is an alias. |
| `antialias` | float, `0`–`8` | `1` | Edge feather width in physical pixels. |
| `clip` | bool | `true` | Clips actual client content. `clip_to_geometry` is an alias. |
| `floating_only` | bool | `false` | Leaves tiled windows square when enabled. |
| `fullscreen` | bool | `false` | Allows rounding while fullscreen. |

```wave
rounding {
    enabled = true
    radius = 18 18 12 12
    power = 2.35
    antialias = 1
    clip = true
}
```

`corners { }` is accepted as a block alias. Inside a rule, `rounding = 20`, `rounding = off`, and `clip_to_geometry = true` are convenient shorthands.

### `border { }`

Draws a fixed-cost rounded border immediately above its window. Equal start/end colors produce a solid border; different colors produce a linear gradient. Active, inactive, and urgent states have independent gradients and opacity. Rotation/pulse animation is opt-in because an animated border intentionally keeps the output redrawing.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Master switch for this scope. |
| `width` | float, `0`–`64` | `2` | Logical-pixel thickness. `size` and `border_size` are aliases. |
| `placement` | `outside`, `center`, `inside` | `outside` | Chooses how much border overlaps client geometry. |
| `active_from`, `active_to` | RGBA colors | aqua gradient | Focused gradient. `color`/`active_color` alias the start color. |
| `inactive_from`, `inactive_to` | RGBA colors | dark aqua gradient | Unfocused gradient. |
| `urgent_from`, `urgent_to` | RGBA colors | bright blue gradient | Bioluminescent attention gradient. |
| `angle` | degrees | `135` | Static gradient angle. `gradient_angle` is an alias. |
| `opacity`, `inactive_opacity`, `urgent_opacity` | float, `0`–`1` | `1` | State alpha multipliers applied after color alpha. |
| `animate` | bool | `false` | Continuously rotates and optionally pulses the gradient. |
| `animate_focused` | bool | `true` | Allows motion while focused. `animate_active` and `animate_on_focus` are aliases. |
| `animate_inactive` | bool | `true` | Allows motion while unfocused. Disable this for focus-only animation while retaining a static inactive border. |
| `animate_urgent` | bool | `true` | Allows motion in the urgent state. |
| `inactive_enabled` | bool | `true` | Shows the inactive border. `focus_only = true` disables it while keeping focused/urgent borders. |
| `animation_speed` | degrees/second | `28` | Signed rotation speed. |
| `pulse_amount` | float, `0`–`1` | `0` | Brightness/alpha modulation depth. |
| `pulse_speed` | cycles/second | `1` | Pulse frequency. |
| `radius_offset` | float | `0` | Expands or contracts border rounding relative to window rounding. |
| `antialias` | float, `0`–`8` | `1` | Physical-pixel edge feather. |
| `floating_only` | bool | `false` | Omits borders from tiled windows. |
| `fullscreen` | bool | `false` | Allows fullscreen borders. |

```wave
border {
    width = 3
    placement = outside
    active_from = 22BEEFFF
    active_to = 61FFD6FF
    inactive_from = 123746B8
    inactive_to = 071821B8
    urgent_from = 7BEFFFFF
    urgent_to = 586FFFFF
    angle = 135
    animate = true
    animate_focused = true
    animate_inactive = true
    animate_urgent = true
    inactive_enabled = true
    animation_speed = 45
    pulse_amount = 0.12
    pulse_speed = 1.2
}
```

Every field can be overridden inside `rule { border { } }`; matching blocks merge field by field. `border = on|off|none` is the rule shorthand.

### `popup { }`

Themes TideWM's own popup chrome: the config warning panel and the toast. Everything auto-follows the theme by default, so first-party UI matches window decoration without a second palette to keep in sync. Border thickness tracks `border.width` clamped into the 1-4px band a small pill can carry; border color tracks the same accent gradient window borders use; radius tracks the average `[rounding]` radius. Set a field only to pin that one piece away from the theme; the rest keep following it.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `border_width` | float, `0`–`8` | `border.width` (clamped `1`–`4`) | Popup border stroke thickness in logical pixels. `border-width` is an alias. |
| `border_color` | RGBA color | accent gradient | Flat popup border color. When set, the gradient position is ignored and every sampled point uses this color. `border-color` is an alias. |
| `radius` | float, `0`–`64` | average `[rounding]` radius | Popup corner radius in logical pixels. |

```wave
popup {
    border_width = 3
    border_color = FF0000
    radius = 20
}
```

### `ripple { }`

Configures the Phase R1 impulse ripple shared by window-map, focus-change, and urgent-attention events. A newly mapped window that automatically receives focus emits only its map ripple; the lifecycle focus step is coalesced so `map_preset` and `focus_preset` do not overlap. Later pointer, keyboard, or command-driven window-to-window focus changes emit the focus ripple normally. `water_effects = false` disables every ripple regardless of this block. The active ripple list is capped at 16 so rapid mapping cannot grow render state without bound.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Enables ripples in this scope. |
| `preset` | enum or name | `water-drop` | Polished analytical appearance: `water-drop`, `jelly`, `bubble`, `splash`, `tide`, `legacy`, or the name of a `ripple_preset <name> { }` block. `jiggle` and `giggle` alias `jelly`; `style` aliases this key. Every polished preset remains one fixed-cost shader element. |
| `map_preset` | preset | inherited | Appearance used only when a window opens. `map_style` is an alias. |
| `focus_preset` | preset | inherited | Appearance used only when focus changes. `focus_style` is an alias. |
| `urgent_preset` | preset | inherited | Appearance used only for an urgent-attention impulse. `urgent_style` is an alias. |
| `focus_on_map` | bool | `false` | When `true`, automatic focus during map also emits `focus_preset`, deliberately stacking it over the map effect. `stack_focus_on_map` is an alias. Later focus handoffs are unaffected. |
| `urgent_repeat` | bool | `true` | When `true`, the urgent ripple re-fires every `urgent_repeat_interval_ms` until the window is focused or its hint clears, instead of firing once. `urgent_pulse` is an alias. |
| `urgent_repeat_interval_ms` | integer | `1500` | Milliseconds between urgent-repeat pulses, clamped to `100`–`60000`. `urgent_interval_ms` and `urgent_interval` are aliases. |
| `shapes` | space-separated list | `ring` | Compatibility mode: any combination of `ring`, `square`, `droplet`, and `cross`. Assigning this key automatically selects `preset = legacy`. `shape` and `form` are aliases. |
| `color` | color | `8EDDFF` | RGB tint. Use bare `RRGGBB` or quoted `"#RRGGBB"`. A bare leading `#` starts a Wave comment, so it must be quoted. Alpha in the color form is currently ignored; use `peak_alpha`. |
| `secondary_color` | color | `E8FCFF` | Gradient, membrane, and highlight tint. `accent_color` and `highlight_color` are aliases. |
| `peak_radius` | positive float | `220` | Maximum radius in logical pixels. `radius` is an alias. |
| `size_mode` | enum | `fixed` | `fixed` uses `peak_radius`; `window` uses half the diagonal; `width`, `height`, `min`, and `max` use half that window dimension. `radius_mode` and `scale_mode` are aliases. |
| `size_scale` | float | `1.0` | Multiplies window-derived sizes, clamped to `0.01`–`8.0`. `radius_scale` and `window_scale` are aliases. |
| `min_radius` | float | `24` | Lower clamp applied after adaptive sizing. |
| `max_radius` | float | `2048` | Upper clamp applied after adaptive sizing. |
| `thickness` | positive float | `8` | Outline half-width in logical pixels. |
| `duration` | duration | `650` | Ripple lifetime. |
| `peak_alpha` | float | `0.88` | Peak opacity, clamped to `0.0`–`1.0`. `alpha` is an alias. |
| `glow` | float | `0.55` | Soft halo strength, clamped to `0.0`–`2.0`. `glow_strength` is an alias. |
| `wobble` | float | `0.7` | Organic displacement strength, clamped to `0.0`–`2.0`. `jiggle` and `distortion` are aliases. |
| `detail` | float | `0.8` | Strength of inner rings, highlights, lobes, and spray, clamped to `0.0`–`2.0`. `complexity` is an alias. |
| `ease` | enum | `cubic-out` | `linear`, `cubic-out`, `cubic-in-out`, `quad-out`, or `exp-out`. |
| `anchor` | enum | `center` | `center`, `cursor`, `top`, `bottom`, `left`, `right`, `nearest-edge`, or any corner. `nearest-edge` projects the current pointer onto the closest side. |
| `edge_position` | float | `0.5` | Position along a fixed side anchor: `0.0` is its left/top end, `1.0` its right/bottom end. |
| `edge_offset` | float | `0` | Signed distance normal to a side: positive moves outside the window, negative moves inside. |
| `offset` | `<dx>x<dy>` | `0x0` | Logical-pixel offset added to the selected anchor. |
| `layer` | enum | `above-windows` | `above-all`, `above-windows`, `below-windows`, or `below-all`. |
| `triggers` | space-separated list | `map` | Any combination of `map`, `focus`, and `urgent`. |

```
ripple_preset edge-jelly {
    preset = jelly
    size_mode = min
    size_scale = 0.8
    min_radius = 80
    max_radius = 420
    anchor = nearest-edge
    edge_offset = 8
    color = 89B4FA
    secondary_color = CBA6F7
    glow = 0.7
    wobble = 1.2
    detail = 0.9
}

ripple {
    preset = water-drop
    map_preset = splash
    focus_preset = edge-jelly
    urgent_preset = bubble
    focus_on_map = false
    color = 89B4FA
    secondary_color = CBA6F7
    duration = 650ms
    glow = 0.65
    wobble = 0.9
    triggers = map focus urgent
}
```

Named presets are reusable sparse bundles. They can select a built-in or
another named preset, and include any ripple field. Cycles and unknown names
warn and safely fall back. Selection order is system defaults → named preset
→ global overrides → per-app named preset → per-app overrides, so a rule can
reuse a bundle and still change one field locally.

### `glass { }` (legacy: `water_glass { }`)

Controls how the water-glass refraction distortion moves over time. The glass
layer itself is selected per window by the `glass` rule (or the legacy
`opacity < 1` trigger); this block only controls how the refraction animates
once selected. `water_effects = false` bypasses the whole effect.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `animation` | enum | `reactive` | `static` is the original fixed distortion (no time uniform, never ticks frames on its own). `reactive` energizes the distortion when the window moves, the backdrop behind it changes, or a ripple passes underneath, then settles back to still over `settle_ms`. `ambient` drifts constantly, ticking frames while glass is visible by design. `mode` is an alias. |
| `speed` | float | `1.0` | Phase drift multiplier, clamped to `0`–`8`. |
| `amplitude` | float | `1.0` | Distortion strength multiplier on the shader's built-in UV offset, clamped to `0`–`4`. `strength` is an alias. |
| `settle_ms` | integer | `1200` | Reactive-mode settle time after the last disturbance, clamped to `100`–`10000`. `settle` is an alias. |

```
water_glass {
    animation = reactive
    speed = 1.0
    amplitude = 1.0
    settle_ms = 1200
}
```

### `caustics { }`

Ambient caustic light patterns over the wallpaper, below windows. One
analytical full-output shader element per output: no texture, no framebuffer,
no element at all when disabled or locked. Works under both Classic and Ocean
engines. `water_effects = false` is the master bypass.

The motion model preserves the idle-zero-frames rule by default: `fps = 0`
animates only on frames that are already being rendered for some other reason
(damage, an active animation elsewhere), so an idle desktop shows static
caustics that read as part of the wallpaper. A non-zero `fps` opts into
constant drift at roughly that rate.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Enables the overlay. |
| `intensity` | float | `0.35` | Peak light alpha, clamped to `0`–`1`. `strength` and `alpha` are aliases. |
| `color` | color | `8CDDFF` | RGB tint of the light ridges. Uses the same color formats as `ripple.color`. |
| `scale` | float | `1.0` | Pattern size multiplier; higher packs more cells per output, clamped to `0.1`–`8`. |
| `speed` | float | `1.0` | Phase drift speed multiplier, clamped to `0`–`8`. |
| `fps` | integer | `0` | `0` piggybacks on damage-driven frames. `1`–`60` opts into constant motion at roughly that rate. |

```
caustics {
    enabled = true
    intensity = 0.35
    color = 8CDDFF
    scale = 1.0
    speed = 1.0
    fps = 0
}
```

### `env { }`

`KEY = VALUE` pairs, applied to TideWM's own process before the backend starts (so e.g. `XCURSOR_THEME` here actually changes the cursor theme TideWM itself loads, not just what child processes see) and exported on standalone sessions to the systemd/D-Bus activation environment alongside `WAYLAND_DISPLAY`. That external export is an ordered, best-effort background task: direct children receive TideWM's process environment immediately, while a missing or wedged session helper may delay session-activated services seeing the update without delaying the compositor, input, or startup commands. Invalid Unix environment names/values are ignored with a config warning; values are never repeated in that diagnostic because they may contain secrets.

```
env {
    XCURSOR_THEME = Adwaita
    XCURSOR_SIZE = 24
    QT_QPA_PLATFORMTHEME = gtk3
    GDK_BACKEND = wayland
}
```

### `input { }`

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `repeat_delay` | integer (ms) | `200` | Delay before a held key starts repeating. |
| `repeat_rate` | integer (Hz) | `25` | Repeat rate once it starts. |
| `focus_follows_mouse` | bool | `true` | Moving the pointer over a window focuses it. |
| `unfocus_on_empty` | bool | `false` | With hover focus enabled, moving onto empty desktop/Ocean canvas clears keyboard focus instead of retaining the last window. |
| `xkb_layout` | string | unset | xkbcommon layout(s), comma-separated for a switchable multi-layout setup (e.g. `us,de`). Unset falls back to the `XKB_DEFAULT_*` env vars. Hot-reloaded; an invalid value is logged and ignored, keeping the previous keymap. |
| `xkb_variant` | string | unset | xkbcommon variant(s), one per layout. |
| `xkb_options` | string | unset | xkbcommon options, e.g. `grp:alt_shift_toggle` to cycle multiple layouts. |
| `xkb_model` | string | unset | xkbcommon keyboard model. |
| `xkb_rules` | string | unset | xkbcommon rules file. |

### `input { touchpad { } }`

udev backend only — winit's nested host input never reaches a real libinput device, so these sit unused there. Every key is opt-in: omit it and that setting is left at whatever your driver already defaults to. Applied per touchpad-capable device (libinput's tap-finger-count check) at startup/hotplug and re-applied to already-connected touchpads on config reload.

| Key | Type | Notes |
| --- | --- | --- |
| `tap_to_click` | bool | Tap the touchpad to click. |
| `tap_and_drag` | bool | Tap-then-drag to move a dragged item without holding a button. |
| `drag_lock` | bool | Keep dragging after a brief tap-and-drag release. |
| `disable_while_typing` | bool | Ignore the touchpad while the keyboard is in use. |
| `natural_scroll` | bool | Reverse scroll direction (touch-surface convention). |
| `left_handed` | bool | Swap left/right buttons. |
| `middle_emulation` | bool | Emulate a middle-click from simultaneous left+right press. |
| `click_method` | string | `clickfinger` or `button-areas`. |
| `scroll_method` | string | `two-finger`, `edge`, `on-button-down`, or `none`. |
| `accel_speed` | float | `-1.0` (slowest) to `1.0` (fastest). |
| `accel_profile` | string | `adaptive` or `flat`. |
| `workspace_swipe_fingers` / `workspace_swipe_distance` | integer / float | Compatibility shortcut for adjacent-workspace horizontal swipes. |
| `gesture_swipe_fingers` | integer | Finger count for the four `swipe_*` action bindings below. |
| `swipe_left`, `swipe_right`, `swipe_up`, `swipe_down` | action | Runs any ordinary TideWM action after a dominant-axis swipe crosses `workspace_swipe_distance` (default 200). |
| `gesture_pinch_fingers` | integer | Finger count for `pinch_in` / `pinch_out`. |
| `pinch_in`, `pinch_out` | action | Runs after the completed pinch reaches scale 0.8 / 1.2 respectively. |
| `modifier_pan_fingers` | integer, unset by default | A swipe with this many fingers, held together with `pointer_modifier`, moves/pans the same way `pointer_modifier`+left-drag does -- the touch itself is the "grab," no button needed. Over a window it picks the window up (tiled or floating, same decision the mouse path makes, including Ocean smart-tiling swap and reattachment); over empty Ocean canvas it pans the camera. |

```
input {
    repeat_delay = 200
    repeat_rate = 25
    xkb_layout = us

    touchpad {
        natural_scroll = true
        tap_to_click = true
        gesture_swipe_fingers = 3
        swipe_left = workspace:2
        pinch_in = toggle-overview
    }
}
```

### `xwayland { }`

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Whether `xwayland-satellite` is spawned at startup. Takes effect on next launch — not hot-reloaded. |
| `path` | string | `xwayland-satellite` | Path to the `xwayland-satellite` binary. |

### `output <name> { }`

Per-connector overrides, **udev backend only** — winit's single simulated output has no real mode list or transform-as-monitor-orientation meaning. Purely opt-in: an output with no matching block auto-configures (preferred mode, auto-positioned to the right of whatever's already mapped, scale 1, no rotation). One block per connector; repeat the block (in the same or another included file) for a second monitor. Position validation uses the selected output's live logical size after mode, transform, and scale: TideWM does not assume a resolution, refresh rate, or maximum monitor count. A configured position whose rectangle or complete desktop span cannot be represented warns and uses automatic placement instead.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| header | string | — | Connector name, e.g. `eDP-1`, `DP-2`. Check your logs or the `outputs` IPC query for what TideWM detected. |
| `enabled` | bool | `true` | Set `false` to leave a connected output unused. |
| `mode` | string, optional | connector's preferred mode | `1920x1080` or `1920x1080@60`. Falls back to the connector's own preferred mode if unset or unmatched. |
| `position` | `WxH`, optional | auto-layout | e.g. `1920x0`. Falls back to the live layout's right edge if unset or outside the logical coordinate domain; if no adjacent position fits at that edge, the output safely overlaps the live minimum corner. |
| `scale` | float | `1.0` | |
| `transform` | string | `normal` | One of `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`, `flipped-180`, `flipped-270`. |
| `gaps` | integer, optional | global `gaps` | Per-output gap override for every workspace shown on this connector. A `workspace_gaps` entry beats it. |
| `adaptive_sync` (aliases `vrr`, `variable_refresh_rate`) | `off` \| `on` \| `on-demand`, optional | global `adaptive_sync` | Per-output override. Same config-only scope as the global key — see that row above. |

```
output eDP-1 {
    enabled = true
    mode = 1920x1080@60
    position = 0x0
    scale = 1.0
    transform = normal
}
```

### `switch_events { }`

Laptop lid / tablet-mode switch bindings, **udev backend only** (winit has no host-independent access to libinput's switch capability). Each entry takes any [action string](#action-strings) — in practice almost always `spawn:...`, since the things you'd react with (suspend, lock, brightness, an onboard keyboard) live outside the compositor.

| Key | Type | Default |
| --- | --- | --- |
| `lid_close` | action string, optional | unset |
| `lid_open` | action string, optional | unset |
| `tablet_mode_on` | action string, optional | unset |
| `tablet_mode_off` | action string, optional | unset |

systemd-logind's own `HandleLidSwitch=` policy (`/etc/systemd/logind.conf`) already triggers suspend on lid close independently of this — `lid_close` here is for whatever *extra* you want on top of that, not a replacement. No logind? Nothing suspends on lid-close on its own; put `spawn:systemctl suspend` or your init's equivalent here.

```
switch_events {
    lid_close = spawn:systemctl suspend
    lid_open = spawn:brightnessctl s 50%
}
```

### `rule { }`

Per-app placement applied the moment a window first maps, before it's ever tiled/rendered at a default spot (i3/sway's `for_window`, Hyprland's `windowrule`). One block per rule; repeat for more.

| Key | Type | Notes |
| --- | --- | --- |
| `app_id` | string, optional | Matches exactly. At least one of `app_id`/`title`/`app_id_regex`/`title_regex`/`pid`/`xwayland` is required — a rule with none of these never matches anything. |
| `title` | string, optional | Matches case-insensitively, anywhere in the string. |
| `app_id_regex` | regular expression, optional | Rust regex matched against the full app ID string. Combines with other criteria in the same rule. |
| `title_regex` | regular expression, optional | Rust regex matched against the title. Use `(?i)` for case-insensitive matching. |
| `pid` | integer, optional | Exact match against the window's real client PID (sway's `[pid=...]`). Never matches a window whose PID couldn't be read (a dead client). |
| `xwayland` | bool, optional | Tri-state: unset matches either kind of window; `true`/`false` requires the window to be (or not be) an X11 client running through `xwayland-satellite` (Hyprland's `xwayland:1`/`xwayland:0`). `is_xwayland` is an alias. |
| `workspace` | integer, optional | Initial workspace, same numbering as `workspace:N` keybinds (including `0`, the scratchpad). |
| `output` | string, optional | Initial output by connector name. Falls back to normal placement if unset or unconnected. |
| `float` | bool | Default `false`. |
| `pseudo_tile` | bool | Default `false`. No-op unless the window ends up tiled; ignored if `float`/`pin` also apply. |
| `pin` | bool | Default `false`. Implies `float`. |
| `tile` | bool | Default `false`. Forces tiled even if the auto-float heuristic (a window with a parent, e.g. a dialog, or one whose min/max size are equal, e.g. a splash screen) would otherwise float it. No effect if `float`/`pin` also match. |
| `no_focus` | bool | Default `false`. Maps without stealing focus — whatever was focused before stays focused. |
| `maximize` | bool | Default `false`. Opens maximized and implies floating placement. |
| `fullscreen` | bool | Default `false`. Opens fullscreen on its selected output. |
| `block_capture` | bool | Default `false`. A per-window capture/screencast source renders black instead of exposing the window. |
| `swallow` | bool | Default `false`. Marks matching windows as swallowers (Hyprland's `misc:enable_swallow`): when a tiled match spawns a process that opens its own window, the match is hidden and the new window takes over its exact tile; closing that window puts it back in the same slot. Detection is PID ancestry via `/proc`, so any terminal works without shell integration. Tiled matches only — a floating terminal keeps both windows visible. |
| `opacity` | float | Base per-window alpha multiplier, clamped to `0.0`–`1.0`. Applies to the whole window surface tree. |
| `active_opacity` | float | Extra multiplier while the window is focused. `focused_opacity` is an alias. |
| `inactive_opacity` | float | Extra multiplier while the window is unfocused. `unfocused_opacity` is an alias. |
| `fullscreen_opacity` | float | Extra multiplier while fullscreen; takes priority over active/inactive state. |
| `glass` | `water`, `frost`, or `none` | Captured-backdrop treatment for tiled and floating windows. Explicit `water`/`frost` works with client-provided alpha; when unset, a TideWM `opacity` below `1.0` implicitly selects `water`. `none` preserves plain transparency. Gaps stay unblurred. `glass_mode` is an alias. |
| `viscosity` | float, `0`–`4` | Per-app interactive move/resize damping. Last matching rule wins; `0` disables it for the matched app. |
| `sway` | bool | Per-app opt-in/out for floating sway. Last matching rule wins; unset falls back to `sway.enabled`. |
| `weight` | float, `0`–`1` | Per-app apparent weight for `buoyancy { }`. Last matching rule wins; unset falls back to `buoyancy.default_weight`; `0` opts the matched floater out. |
| `depth` | bool | Per-app buoyancy override for the automatic depth/attention system. `false` pins the window at tier 0 forever (never dims/sinks); `true` affirms the normal automatic behavior. Last matching rule wins; unset falls back to `depth.enabled`. |
| `frost { }` | sub-block | Per-app overrides for every global frost field. Unspecified fields inherit the global block; multiple matching rules merge field by field. |
| `shadow` | bool / `on`, `off`, `none` | Shorthand to enable or disable compositor shadows for matching windows. |
| `shadow { }` | sub-block | Per-app overrides for every global shadow field. Unspecified fields inherit the global block; multiple matching rules merge field by field. |
| `rounding` | radius / `on`, `off`, `none` | Shorthand for per-app rounding radius or enablement. `corners` is an alias. |
| `clip_to_geometry` | bool | Per-app shorthand for `rounding.clip`. |
| `rounding { }` | sub-block | Per-app radius, power, clipping, antialias and scope overrides. Matching rules merge field by field. |
| `border` | bool / `on`, `off`, `none` | Shorthand to enable or disable compositor borders. |
| `border { }` | sub-block | Per-app border geometry, state-gradient, animation and scope overrides. Matching rules merge field by field. |
| `position` | `<x>x<y>`, optional | Exact floating placement. No-op unless the window ends up floating. |
| `size` | `<width>x<height>`, optional | Exact floating size. No-op unless the window ends up floating. |
| `ripple { }` | sub-block | Per-app overrides for any global ripple field; unspecified fields inherit the global block. `ripple = none` suppresses ripples for matching windows. |

The effective surface opacity is `opacity × state opacity`, clamped to `0`–`1`; for example, `opacity = 0.9` plus `inactive_opacity = 0.8` renders at `0.72`. This compositor opacity affects text and foreground pixels too. For colorless frost with opaque text, keep these at `1.0`, set per-app `frost.tint_alpha = 0.0`, and use the app's own background transparency when available.

Multiple rules can match the same window: scalar fields take the *last* match; `frost { }`, `shadow { }`, `rounding { }`, `border { }`, and `ripple { }` sub-blocks merge field by field; boolean effects accumulate (any match sets one, never unsets it).

```
rule {
    app_id = pavucontrol
    float = true
}

rule {
    title = Picture-in-Picture
    float = true
    pin = true
}

rule {
    app_id = Slack
    workspace = 3
}

rule {
    # Float every X11 client by default -- most are legacy dialogs/tools
    # that don't tile well.
    xwayland = true
    float = true
}

rule {
    app_id = kitty
    swallow = true
    active_opacity = 1.0
    inactive_opacity = 0.92
    fullscreen_opacity = 1.0
    glass = frost
    frost {
        radius = 18
        strength = 1.0
        opacity = 1.0
        tint_alpha = 0.0
        noise = 0.015
    }
}
```

### `layer_rule { }`

Matches a layer-shell surface (a bar, panel, or launcher, not an ordinary app window) by namespace. One block per rule; repeat for more. Hyprland `layerrule` parity items, from the 2026-08-06 config audit:

| Key | Type | Notes |
| --- | --- | --- |
| `namespace` | string, optional | Matches case-sensitively, anywhere in the surface's namespace string (the name the client itself sets — rofi's is `rofi`). Required — a rule with no `namespace` never matches anything. |
| `block_capture` | bool | Default `false`. When `true`, the matched surface's rect renders as solid black in `wlr-screencopy`/`ext-image-copy-capture` output instead of its real content, without hiding it from your own screen (niri's `layer-rule { block-out-from ... }`) — for something like a password-manager quick-access panel that shouldn't end up in a recording. |
| `z_order` | integer, optional | Reorders the matched namespace within its own layer-shell stratum (Background/Bottom/Top/Overlay) instead of the client's natural mapping-order stacking — Hyprland's `order`. Higher sorts more to the front. Last matching rule that actually sets it wins. |
| `dim_around` | bool | Default `false`. While a matching surface on the Overlay or Top stratum is mapped, dims everything rendered behind it — every non-fullscreen window, lower layers, and the wallpaper — Hyprland's `dimaround`. |
| `dim_amount` | float 0.0-1.0, optional | The dim overlay's alpha. Default `0.35` when `dim_around` is on and this is unset. |
| `above_lock_screen` | bool | Default `false`. Keeps the matched surface rendered on top of the session-lock surface instead of being blanked with everything else — Hyprland's `abovelock`. Render-only: input still never reaches anything but the lock surface itself while locked, so this cannot be used to bypass the lock. Screenshots/screencasts taken while locked still honor `block_capture` for an `above_lock_screen` surface. |
| `blur` | bool | Default `false`. Frost-glasses the matched surface's own backdrop — Hyprland's `layerrule = blur`. Requires the global `frost { enabled = true }` (checked separately from this per-namespace opt-in); always frost, there's no water-refraction choice for a bar. No `ignore_alpha` knob yet: the frost rect covers the surface's full negotiated geometry, not just its opaque pixels, so a layer client whose logical size is much bigger than what it actually draws (an invisible full-output click-catcher) would blur the whole area behind it, same as Hyprland without `ignorealpha`. Doesn't affect an ordinary bar/launcher sized to its visible content. |

```
layer_rule {
    namespace = rofi
    block_capture = true
}

layer_rule {
    namespace = waybar
    z_order = 10
}

layer_rule {
    namespace = launcher
    dim_around = true
    dim_amount = 0.5
}

layer_rule {
    namespace = volume-osd
    above_lock_screen = true
}

layer_rule {
    namespace = waybar
    blur = true
}
```

### `workspace_rule { }`

Per-workspace-number overrides — Hyprland's `workspace_rule` block. Matches by raw workspace number on any output, the same convention `workspace_gaps` already uses (no `workspace_name` alias support yet). One block per rule; a later block naming the same number overrides an earlier one field by field, last-match-wins. `default_name` isn't a field here — `workspace_name = <N> <name>` (top-level key) already names a workspace unconditionally, so it isn't duplicated.

| Key | Type | Notes |
| --- | --- | --- |
| `workspace` | integer, required | The workspace number this rule applies to. A rule with no `workspace` never matches anything. |
| `layout` | `bsp` \| `master` \| `cascade` | Default tiling algorithm for this workspace number, on any output. Loses to an explicit runtime `layout:<algo>` action already applied to that specific (output, workspace) pair — that's the user actively choosing something right now, not the workspace's resting default. |
| `border` | bool, or a `border { }` sub-block | Same shorthand/sub-block shape as `rule { border }` above. Acts as the base every matching `[[window_rule]]`'s own `border` builds on for a window living on this workspace, underneath the global `border { }` block. |
| `rounding` | bool, radii, or a `rounding { }` sub-block | Same shape as `rule { rounding }`, same base-layer role as `border` above. |
| `shadow` | bool, or a `shadow { }` sub-block | Same shape as `rule { shadow }`, same base-layer role as `border` above. |
| `on_created_empty` | string, optional | Command run the first time this workspace is switched into while it has zero windows. TideWM's numbered workspaces always exist as addressable slots — there's no real create/destroy lifecycle the way Hyprland has — so this fires once per (output, workspace) pair for the process lifetime, the closest honest analog available, rather than repeating on every later empty visit. |

```
workspace_rule {
    workspace = 8
    layout = master
    on_created_empty = "discord"
    border {
        active_color = #8EDDFF
    }
}
```

### `bind`

`bind <chord> { <action> }` — one action per line inside the block, or comma-separated on one line. XKB modifiers (`Super`/`Logo`/`Mod4`,
`Ctrl`/`Control`, `Alt`, `Shift`) can be combined freely. Ordinary keys can
also be held as user-defined helpers: `bind P+H { focus-left }` suppresses P
while held and runs the action when H is pressed; `P+R+H` and combinations
such as `P+Ctrl+H` work too. P has no reserved meaning—it is a normal key
unless a bind uses it. A completely bare action such as
`bind F { toggle-fullscreen }` is valid and intentionally captures F from
clients. Key names match the unshifted keysym, case-insensitively.

Variables are reusable chord pieces. The shipped default uses one,
`@mod = SUPER`, for everything; nothing stops splitting binds across
independent layers of your own instead, e.g. `@mod = ALT`, `@helper =
SUPER`, `@move = CTRL`.
Parsed Wave bindings are authoritative: no built-in table or feature-specific
bindings are invisibly merged underneath them. The one mechanism outside the
normal table is the recovery chord `Ctrl+Alt+Escape`; it temporarily activates
a known-safe fallback table without rewriting the file. The fallback clears
on the next successful config reload or TideWM restart.

See [Action strings](#action-strings) for every value a bind can take. A later
`bind` on the same chord overrides an earlier one. The old
`bind <chord> = <action>` line form still parses as a deprecated alias.

### `mode <name> { }` (legacy: `submap <name> { }`)

A temporary alternate keybind table (sway/Hyprland's "mode" idea), same `bind` statements as the top level (no modifier prefix needed if the submap's own binds are unmodified, like the shipped `nav` example). Entered via a `submap:<name>` action, which **fully replaces** the base binds — not layered on top of them — until an explicit `exit-mode` bind. Not tied to focus; stays active until you explicitly leave it. A config reload that drops or renames the active submap auto-exits back to the base binds.

```
submap nav {
    bind h { focus-left }
    bind l { focus-right }
    bind k { focus-up }
    bind j { focus-down }
    bind Escape { exit-mode }
}
```

Query which submap (if any) is active via `tidectl active-submap` or the IPC `active-submap` request.

## Action strings

The same set of strings works after `bind ... =` at the top level or inside a `submap { }`, in `switch_events { }`, and as `tidectl`'s action argument / the IPC `action` request — one dispatch mechanism, not four.

**Windows**
- `close-window`
- `toggle-floating`
- `toggle-fullscreen`
- `toggle-pin`
- `toggle-pseudo-tile`
- `toggle-scratchpad`
- `move-to-scratchpad`
- `toggle-scratchpad:<name>` / `move-to-scratchpad:<name>` — named scratchpads (Hyprland's named special workspaces), any number of them, independent of the unnamed one. A name is created on first use; no declaration needed. The IPC `workspaces` query reports these entries with a `scratchpad` field holding the name (`""` for the unnamed scratchpad) so bars can label or hide them.
- `raise-window` / `lower-window` — floating windows only, no-op on a tiled one

**Focus and layout**
- `cycle-focus` — most-recently-used order, not z-order
- `focus-urgent` — jump to whichever window is currently marked urgent, if any
- `focus-left` / `focus-right` / `focus-up` / `focus-down` — Classic selects by live screen geometry. Ocean compares the current camera projection, keeps the camera still for a visible neighbor, and glides it to an off-camera neighbor using `ocean.camera_animation_ms`/`camera_sway`; screen pins use their viewport position.
- `swap-left` / `swap-right` / `swap-up` / `swap-down`
- `resize-left` / `resize-right` / `resize-up` / `resize-down` — shrink/grow the focused floating window by 24 logical pixels, or resize its nearest BSP split and connected parallel ancestors
- `layout:bsp` / `layout:master` / `layout:cascade` — switch the current workspace's tiling algorithm
- `master-grow` / `master-shrink` — nudge the master/stack ratio (master layout only, no-op under BSP)

**Groups (window tabbing)**
- `group-left` / `group-right` / `group-up` / `group-down`
- `ungroup`
- `cycle-tab-next` / `cycle-tab-prev`

**Workspaces**
- `workspace:<N>` — Classic switches to workspace `N`; Ocean jumps to numeric bookmark/reef `N` without creating a workspace
- `move-to-workspace:<N>` — move the focused window to workspace `N`
- `swap-workspaces:<output-name>` — swap this output's and the named output's active-workspace content
- `workspace:<name>` / `move-to-workspace:<name>` — same two actions, addressed by a `workspace_name` alias instead of a number (see below) — a workspace's real identity is always its number, this is just another way to spell it

**Ocean camera** (removed from keyboard matching while Classic is selected)
- `ocean-pan-left` / `ocean-pan-right` / `ocean-pan-up` / `ocean-pan-down` — glide only the current output camera by `ocean.camera_step` screen pixels
- `ocean-zoom-in` / `ocean-zoom-out` / `ocean-zoom-reset` — scale the continuous world around the output center
- `ocean-center-focused` — center the camera on the focused window without moving that window in world space
- `ocean-bookmark:<name>` — glide the current output camera to a configured or runtime bookmark
- `ocean-save-bookmark:<name>` — store the current camera position for this session without rewriting config
- `depth-down` / `depth-up` — travel to the next/previous meaningful world Y: a reef origin or explicitly floating/sunk window, never a local tile row
- `sink-window` — make the focused window a world floater and place it immediately below the current viewport
- `ocean-dredge-window` — pull the nearest floating window below the viewport into its center
- `ocean-surface-window` — place the focused window at world Y=0 and center the camera on it

The generated example uses `$move = CTRL` for camera arrows and keyboard zoom,
while pointer-anchored wheel zoom uses `pointer_modifier`. Depth examples use
the main `$mod` layer. These are ordinary explicit `bind` lines: TideWM never
synthesizes Ocean or Depth bindings behind the config, and deleting or
rewriting a line removes or changes it completely.

**Modes**
- `mode:<name>` — enter a `mode <name> { }` block
- `exit-mode` (legacy: `exit-submap`)
- `toggle-overview` — schematic grid of every workspace on the current output (see README's Features list; not live thumbnails)

**Classic Depth Deck** (all no-op while `classic_depth.enabled = false`)
- `depth-down` / `depth-up` — rotate the focused tile through its workspace's deck directly; Down on an empty deck parks the focused window first
- `sink-window` — park the focused ordinary tiled window in this workspace's deck
- `dive` — toggle the current workspace's deck overlay
- `depth-next` / `depth-prev` — move the selected deck card, wrapping
- `depth-select` — exact-slot swap recall, or ordinary tiled restore with no compatible focus target
- `depth-cancel` — close the deck without changing windows

**Outputs**
- `toggle-dpms` — toggle every output's power together (all on, or all off)

**Process and session**
- `spawn:<command>` — args split on whitespace, no shell
- `quit`

## IPC and `tidectl`

`$XDG_RUNTIME_DIR/tidewm-<pid>.sock`: one JSON request line in, one JSON response line out, per connection. Read queries return structured data; `{"request": "action", "action": "<any string above>"}` runs any action string. `{"request":"batch","actions":["workspace:2","spawn:kitty"]}` validates the complete list first, then executes up to 128 actions in order, so an invalid later item cannot leave a half-run batch. This is genuinely the same path a keybind press uses (`config::parse_action` → `Smallvil::run_action`).

Queries: `outputs`, `workspaces`, `windows`, `focused-window`, `active-submap`, `diagnostics`. `{"request": "eval", "expression": "<wave expression>"}` evaluates on the live session Lua (config variables, section tables, and the refreshed `tide` table) and returns the value as JSON.
The `windows` query describes every protocol-mapped toplevel, including clients on inactive Classic workspaces, parked group tabs, and Depth Deck entries; visibility in the current output scene is not treated as mapping state.
In Ocean, `outputs` reports `active_workspace: null`, the current two-axis
`camera_origin`, and `camera_zoom`; `workspaces` returns an empty list because bookmarks
are navigation targets rather than real workspaces. Ocean window entries use
`workspace: null` and `output: null` (the same world can be visible through
multiple outputs), plus `entry_output` as a non-owning input/focus hint.

`diagnostics` is the compositor-side half of `tidectl doctor`/`report`: version, git commit, build profile and date, backend (`winit`/`udev`), uptime, spatial engine, `water_effects`, config path and current parse warnings, XWayland enablement, session-lock state, layer-surface count, and keybind/submap counts. It exists so a bug report can state exactly which build ran without guessing from the host.

**Subscribe (event stream).** The same socket also supports a long-lived mode for reactive widgets (a waybar module, an eww `deflisten`, a QuickShell socket reader) that shouldn't have to poll the queries above. Send `{"request": "subscribe", "events": ["window", "workspace", "focus", "urgent", "depth", "config"]}` as the first and only request on a fresh connection; omitting `events` (or sending an empty array) subscribes to all six channels. The server replies with one ack line (`{"ok": true, "data": {"subscription_id": <n>, "events": [...]}}` — the resolved channel list, so a typo'd filter is visible at handshake time instead of silently matching nothing), then keeps the connection open and writes one `{"event": "<kind>", "data": ...}` line per matching change until the client disconnects. The connection's lifetime is the subscription's lifetime.

Event kinds: `window-opened` / `window-closed` / `window-changed` (channel `window`), `workspace-changed` (`workspace`), `focus-changed` (`focus`), `urgent-changed` (`urgent`), `depth-changed` (`depth`), `config-reloaded` (`config`). Window-carrying events embed the same object shape the `windows` query returns; XDG title/app-id requests emit `window-changed` immediately and do not require a later surface commit. `focus-changed` is `null` when focus clears to nothing, and `window-closed` carries the closed window's last-known `window_id`/`app_id`/`title` directly (its tracking state is already gone by the time the event serializes). A subscriber that falls behind (roughly a quarter-meg of unwritten JSON) is disconnected outright rather than let its backlog grow unbounded.

`tidectl` (built alongside the compositor: `cargo build --bin tidectl`) is a small CLI over this socket, auto-discovering the running instance:

```bash
tidectl outputs
tidectl workspaces
tidectl windows
tidectl focused-window
tidectl workspace 3            # shorthand for action workspace:3
tidectl move-to-workspace 2
tidectl spawn kitty
tidectl submap nav              # shorthand for action submap:nav
tidectl action toggle-floating  # explicit passthrough, works for any action string
tidectl batch workspace:2 spawn:kitty
tidectl active-submap
tidectl eval "theme.primary"               # evaluate on the live session Lua
tidectl eval "tide.workspace"              # the tide table is refreshed first
tidectl subscribe focus workspace window   # long-lived event stream, one JSON line per event
--json                          # on any query, for scripting
```

`tidectl subscribe [event...]` is the one long-lived CLI command: it opens the subscribe mode above and prints each `{"event": "<kind>", "data": ...}` line verbatim until TideWM exits (socket EOF ends the process cleanly, so a supervisor can restart it). With no event names every channel is subscribed. A bar or panel runs it as a persistent process and parses stdout — instant, event-driven updates with no polling; the QuickShell Tide rice uses exactly this instead of its old fixed-rate `tidectl` polls.

`tidectl perf [--window <secs>] [--json]` takes two IPC `perf` snapshots spaced by the window (default 3s) and prints a compact PSS/RSS/thread-count/render-state summary. CPU% comes from the delta of the compositor's own `getrusage` microsecond counters between the two snapshots, so it's always about the right process and needs no `CLK_TCK` constant. It also reports the ARGB pixel payload of TideWM-owned backdrop, built-in-wallpaper, caustics, and active workspace-transition textures, derived from their live allocations. That estimate excludes client buffers and driver metadata, while GPU-busy% still requires a vendor-specific tool.

**Diagnostics.** Two host-side commands run entirely outside the socket, so they work even when TideWM won't start:

- `tidectl doctor` runs a quick health check battery (compositor reachability, build profile, config warnings, PipeWire, xdg-desktop-portal, GPU, XWayland, journal errors, core dumps, compositor memory) and prints one `PASS`/`WARN`/`FAIL`/`SKIP` line per check. Exit code 0 = nothing wrong, 1 = warnings, 2 = failures. `tidectl doctor --json` emits the same checks as machine-readable JSON.
- `tidectl report [--output <path>]` writes a plain-text diagnostic file (default `tidewm-report.txt`) for attaching to a GitHub issue: build provenance, system, GPU, the embedded doctor quick check, live compositor state (outputs/workspaces/windows), services, and recent errors. The report stays compact (about 2-3 pages) when everything is healthy and only expands the log-heavy sections when problems were detected. A privacy note at the top reminds you the file includes window titles — review before attaching.

Full flag/command list: `tidectl --help`.

## Protocol matrix

| Protocol | Purpose | Status |
| --- | --- | --- |
| `xdg-shell` | Core window protocol | Done |
| `wlr-layer-shell-unstable-v1` | Bars, launchers, lock screens | Done — verified live with `rofi`, `hyprlock` |
| `xdg-decoration` + `org_kde_kwin_server_decoration` | Server-side decoration negotiation | Done — both resolve to server-side |
| `xdg-activation-v1` | Focus-stealing prevention / requested activation | Done |
| `wp-fractional-scale-v1` | Non-integer output scaling | Done |
| `wp-viewporter` | Surface scaling/cropping (needed by `xwayland-satellite`) | Done |
| `wp-presentation-time` | Frame timing feedback to clients | Done |
| `wp-single-pixel-buffer-v1` | Solid-color buffers without a real allocation | Done |
| `wp-tearing-control-v1` | Client hint that a surface's content may tear (games, drawing tablets) | Protocol done (global, per-surface double-buffered hint, verified live including the `tearing_control_exists` error path) — no DRM-level effect yet, no config surface. Honoring it means an async/immediate KMS page flip, and the pinned Smithay revision's `AtomicDrmSurface`/`DrmCompositor` hardcode their own atomic-commit flags with no caller override point, confirmed by reading the source. See AGENT.md for the full reasoning |
| `xdg-toplevel-icon-v1` | Client-provided application/window icons | Done — names and buffers are retained in committed surface state for launchers and future TideWM UI consumers |
| `ext-session-lock-v1` | Screen lock (`swaylock`, `hyprlock`) | Done — a crashed lock client terminates the compositor session fail-closed so the login manager can recover; it never auto-unlocks |
| `ext-foreign-toplevel-list-v1` | Read-only toplevel list | Done |
| `wlr-foreign-toplevel-management-v1` | Bidirectional toplevel control (waybar's `wlr/taskbar`, ags v1) | Done |
| `ext-workspace-v1` | Workspace listing and activation for bars (no compositor-specific TideWM waybar module exists, so this is the protocol-native alternative to polling `tidectl`) | Done for the scope this first pass covers — one group per output, `activate` request/capability only (`deactivate`/`assign`/`remove`/`create_workspace` are capability-gated off), no `output_enter`/`output_leave` (harmless for a single monitor, a multi-output client can't yet tell groups apart) and no per-workspace `urgent` bit. Verified live in a nested session with a throwaway `wayland-client` binary: bound the global, received all 12 workspaces (`workspace_count = 12`) with correct `state`/`capabilities`, and a real `activate()` request sent from the client switched the compositor's active workspace, confirmed both by a follow-up `state` event and `tidectl workspaces` |
| `wlr-screencopy-unstable-v1` + `ext-image-copy-capture-v1` | Screenshots (`grim`) | Done — output and per-window ext capture; SHM everywhere plus direct DMA-BUF rendering for full-output wlr capture on DRM sessions |
| `wlr-data-control-unstable-v1` | Clipboard managers (`cliphist`, `wl-clip-persist`) | Done, including primary selection |
| `wp-security-context-v1` | Sandboxed Wayland listener and policy identity | Done — sandboxed clients are denied session-lock, IME/virtual-keyboard, global clipboard control, capture, output-control, and foreign-toplevel/workspace globals |
| `xdg-output-manager-v1` | Output geometry disclosure | Done |
| `idle-inhibit-unstable-v1` / `ext-idle-notify-v1` | Idle inhibition and notification | Done |
| `wp-pointer-constraints-v1` + `wp-relative-pointer-v1` | Pointer lock/confine, relative motion | Done — verified live (Minecraft camera-look) |
| `wp-pointer-gestures-v1` | Touchpad gesture events to clients | Done — unbound streams reach clients; configured swipe/pinch streams are consumed atomically by compositor actions |
| `zwp-keyboard-shortcuts-inhibit-v1` | Let a client (VM, remote desktop) capture all shortcuts | Done |
| `zwp-text-input-v3` + `zwp-input-method-v2` + `zwp-virtual-keyboard-v1` | IME support | Done — app-side activation verified live; see CHANGELOG for the exact verification bar per sub-protocol |
| `wp-cursor-shape-v1` | Named-cursor requests (Qt6/GTK4, QuickShell) | Done |
| `zwp-tablet-v2` | Drawing tablet/pen support (tools, pressure, tilt, proximity) | Done via Smithay's convenience module — global, per-device hotplug advertisement, and full axis/proximity/tip/button forwarding live in `tide_core/input.rs`. Verified live nested that the global advertises and a real client's `get_tablet_seat` request completes cleanly with no crash; real tablet hardware to exercise actual `DeviceAdded`/axis/pressure/tilt events has not been available to test |
| `wlr-output-management-unstable-v1` | Runtime output reconfiguration (`wlr-randr`, `kanshi`, `wdisplays`) | Done — position/transform/scale apply live; complete layouts outside Smithay's logical coordinate domain fail atomically; disabling an output or changing resolution needs real hardware to verify a live modeset |
| `wlr-output-power-management-unstable-v1` | Display on/off (DPMS) | Protocol + render-loop logic done; real CRTC power toggle unverified on hardware |
| `zwlr-gamma-control-manager-v1` | Night-light tools (`wlsunset`, `gammastep`) | Protocol + DRM gamma ioctls done; real color-change unverified on hardware |
| `org.freedesktop.a11y.KeyboardMonitor` (DBus, not a Wayland protocol) | Screen reader (Orca) grabbing/watching keys system-wide | Done, behind the `accessibility` Cargo feature (off by default, `cargo build --features accessibility`) — see CHANGELOG for the verification bar |
| `org.gnome.Mutter.ScreenCast` + PipeWire | Monitor/window video streams for `xdg-desktop-portal-gnome`-based setups | Behind the `screencast` Cargo feature — DBus session lifecycle works, and the PipeWire MemFd/SHM path now delivers real frames under the nested (winit) backend (verified with a direct PipeWire consumer: correctly-oriented, live-updating content, PSS flat over a sustained stream). Not yet verified on a standalone TTY (udev/DRM) session. DMA-BUF export stays disabled (fails on real hardware). Not yet run through a real portal-mediated client (OBS/Discord) |
| `org.freedesktop.impl.portal.ScreenCast` (DBus, the real `xdg-desktop-portal` backend interface) | Discord/OBS-style screen sharing | Behind the `screencast` Cargo feature, self-contained (no `xdg-desktop-portal-gnome` needed), with compositor-owned monitor/window/virtual-source selection, reusing the same now-working PipeWire path as the row above. Virtual sources still mirror the selected desktop dimensions rather than creating a headless DRM connector, and this exact entry point hasn't been exercised through a real `xdg-desktop-portal` process yet |

Everything else on the original protocol/rice compatibility list is implemented. Screencasting is the remaining loose end: the DBus/portal plumbing and PipeWire MemFd frame delivery both work now under the nested backend, verified directly, but the standalone udev/DRM backend and a real portal-mediated client (OBS/Discord) on real hardware haven't confirmed the full chain yet, and DMA-BUF export needs real GBM-backed allocation before it's usable.
