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
| `-s, --spawn <command>` | Spawn one specific command right after startup, instead of nothing. No shell parsing (same rule as `spawn_at_startup`). |
| `-v, --version` | Print the version and exit. |
| `-h, --help` | Print usage and exit. |

## Config file

`$XDG_CONFIG_HOME/tidewm/config.wave`, or `~/.config/tidewm/config.wave` if `XDG_CONFIG_HOME` isn't set (or the path given to `--config`, see above). Written out with working defaults on first run. Almost every change hot-reloads on save — no restart needed — and a bad edit is shown in a persistent compositor-owned panel that reserves space above tiled windows (with file/line detail) while the previous config keeps running. Fixing the file clears the panel; the existing short reload/debug toast remains separate. The main exception is `xwayland { enabled }` (spawning/tearing down `xwayland-satellite` isn't done live). Everything else, including keyboard layout and already-connected touchpads, applies immediately.

### Waves format

TideWM's own config format, not TOML. Three rules cover the whole grammar:

- **`key = value` is the rest of the line** after the first `=`, trimmed — so a spawn command's own flags and spaces never need quoting (`bind $mod+R = spawn:rofi -show drun` just works). Quote a value only if you need to keep meaningful leading/trailing whitespace, or a literal `#`.
- **A line ending in `{` opens a block**, always real multi-line — `output eDP-1 { position = 0x0 scale = 1.0 }` all on one line is not valid. No exceptions; this is what keeps "rest of line" from being ambiguous.
- **`#` starts a comment**, unless it's inside `"quotes"`.

`$name = value` defines a variable, substituted anywhere below (only names actually defined this way are substituted — a `$HOME` or `$PATH` inside a spawn command is left untouched). `$wave(a, b, c, ...)` is a built-in, not something you define: it resolves to the first candidate whose own first word is a real, executable file (checked directly, or via `$PATH`), falling back to the last candidate untried if none resolve — so a spawn still gets attempted and fails visibly instead of silently doing nothing. This is what makes the shipped default config's `terminal = $wave(kitty, alacritty, foot, xterm)` line actually portable across machines instead of hardcoding one binary name.

```
$mod = SUPER
terminal = $wave(kitty, alacritty, foot)
bind $mod+Return = spawn:$terminal
```

**Multi-file:** `include "path.wave"` as its own statement, repeatable (one per line), in any file (the main one, or one it includes). Each path resolves relative to the file that lists it; `~/` expands to your home directory. Rules:

- `input { }`, `input { touchpad { } }`, `env { }`, `switch_events { }`, and a given `submap <name> { }` merge field-by-field across files — the same key set from two files combines rather than one replacing the other.
- `output <name> { }`, `rule { }`, and `layer_rule { }` blocks accumulate — entries from every file all end up present.
- A later `include` overlays an earlier one.
- **The including file's own keys always win over anything it includes.** If `config.wave` includes `overrides.wave`, and both set `gaps`, `config.wave`'s own value wins — put an override directly in the file doing the including, not in a file you list last.
- A broken include (missing, unreadable, unparseable, or a cycle) is skipped with a warning; it doesn't fail the whole config.
- The config directory is watched recursively (`*.wave` files only, dotfiles/dotdirs skipped) — editing any included file hot-reloads exactly like editing the main file.

## Wallpaper behavior

TideWM always provides the bundled `assets/tide-aqua-4k.png` artwork, so a fresh session never needs a separate daemon. The source is decoded once at its native 3840×2160 resolution and scales to each output with centered `cover` cropping rather than distortion; it is never pre-downsampled, and it is hidden while the session is locked. This costs about 31.6 MiB of steady-state pixel backing in exchange for retaining full 4K detail. It is intentionally only a fallback: standard Wayland layer-shell background clients render above it, so tools such as `swaybg`, `swww`/`awww`, or another compatible wallpaper daemon can provide images, animations, transitions, and per-output management without a TideWM-specific API. Start one with `spawn_at_startup` if desired.

## Config reference

### Top-level keys

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `terminal` | string | `"kitty"` | Spawned by the default `Super+Return` bind. The shipped default is `$wave(kitty, alacritty, foot, xterm)` — see [`$wave(...)`](#waves-format) above. |
| `pointer_modifier` | modifier or `+`-joined modifiers | `super` | Modifier physically held for compositor mouse actions: left-drag moves floating windows or drag-swaps tiles; right-drag resizes floating or tiled windows. Accepts `super`/`logo`/`mod4`, `alt`/`mod1`, `ctrl`/`control`, and `shift`. The shipped config sets it to `$mod`, so changing `$mod` can update keyboard and mouse behavior together. `mouse_modifier` and `drag_modifier` are aliases. |
| `show_welcome_hint` | bool | `true` | Shows a persistent "fake window" card pointing you at `Super+Return` whenever the desktop is otherwise empty. Disappears the instant a real window maps; delete this key (or set it `false`) to stop it coming back. Checked on every reload, not just at startup. |
| `water_effects` | bool | `true` | Master toggle for TideWM's water/aqua render identity. Disables water-glass, backdrop capture, impulse ripples, wave workspace transitions, automatic depth/buoyancy, interactive viscosity, connected-vessel resize, and floating sway when `false`. |
| `viscosity` | float, `0`–`4` | `1.0` | Interactive window move/resize damping. `0` follows the pointer immediately; higher values settle more slowly. Render-only: logical geometry and hit-testing stay at the pointer target. Disabled by `water_effects = false`. |
| `cursor_always_visible` | bool | `false` | Forces the udev backend's software cursor to stay visible even when a client asks to hide it (e.g. a terminal hiding its own pointer glyph after inactivity). Off by default — respecting a client's own hide request is correct behavior; this is an opt-in override. |
| `cursor_hide_after_ms` | integer | `0` | udev backend only: hides the software cursor after this many milliseconds of no real pointer motion (niri's `cursor.hide-after-inactive-ms`). `0` disables it. Independent of `cursor_always_visible` — that overrides a *client's* hide request, this is a compositor-driven idle timer, and the two can be combined. |
| `workspace_auto_back_and_forth` | bool | `false` | Re-selecting the already-active workspace jumps back to whichever one was active immediately before it, instead of no-opping (niri's own feature of the same name). |
| `workspace_name` | repeatable key | none | Names a workspace number for use in `workspace:<name>`/`move-to-workspace:<name>` (niri's `set-workspace-name`, Hyprland's `workspace name:foo`) — `workspace_name = 3 web`, repeat the key once per name. Purely an addressing convenience: the workspace's real identity is still its number. An unknown name at action time warns and no-ops rather than switching. |
| `gaps` | integer | `8` | Pixel gap the tiling engine applies around and between tiles, both layout algorithms. |
| `workspace_gaps` | repeatable key | none | Per-workspace gap override — `workspace_gaps = 3 0` (workspace 3, no gaps), repeat the key once per workspace. Accepts a `workspace_name` alias in place of the number. Beats both the output-level `gaps` override and the global `gaps`. |
| `default_layout` | `bsp` \| `master` | `bsp` | Starting tiling algorithm for a workspace with no runtime override (see `layout:bsp`/`layout:master` actions below). `bsp` is dwindle-style: split orientation follows each window's own aspect ratio. `master` is one master pane plus an evenly-split stack. |
| `master_orientation` | `left` \| `right` \| `top` \| `bottom` | `left` | Which side the master pane sits on under `default_layout = master`. `left`/`right` stack the other windows vertically in the remaining strip; `top`/`bottom` stack them horizontally instead. One global setting, not per-workspace. |
| `bsp_split_bias` | `auto` \| `horizontal` \| `vertical` | `auto` | Manual override for `default_layout = bsp`'s per-split axis choice. `auto` is the existing aspect-ratio-driven behavior, unchanged. `horizontal`/`vertical` force every split one way regardless of window/output shape (Hyprland dwindle's `force_split` idea). One global setting, not per-workspace. |
| `pseudo_tile_scale` | float, `0.05`–`1.0` | `0.7` | Fraction of its tile a pseudo-tiled window keeps, centered within it. Out-of-range values are clamped, not rejected. |
| `spawn_at_startup` | repeatable key | none | Commands launched once at startup — repeat the key once per command (`spawn_at_startup = waybar` on its own line, again for the next one), not one line holding a list. Args split on whitespace — no shell involved, so quoting/globs/pipes aren't supported; wrap in `sh -c "..."` yourself if you need those. |

### Workspace transitions

Workspace actions use a directional wave wipe while both `water_effects` and `workspace_transition.enabled` are true. The default `water` style is a full-screen transition: a blue body with moving caustic streaks, a curling foamy crest, and spray floods across the outgoing workspace; once water covers the whole output, it continues across to reveal the incoming workspace. The alternate `glow` style retains the slimmer colored sinusoidal boundary wipe. Cursor and compositor chrome remain live above either style.

TideWM captures the outgoing desktop after its submitted frame and keeps the incoming workspace live underneath the effect. State is bounded to one pending target and one transient ARGB8888 full-output texture per output (about 7.9MiB at 1080p or 31.6MiB at 4K), with newer switches replacing rather than queueing. Optional workspace motion captures the incoming desktop too, doubling that transient cost only while its slide runs; both textures are released when the transition ends. Both procedural styles are shader-only and allocate no other textures. Disabling either toggle makes workspace switching immediate.

The enable/duration/curve split follows niri’s useful per-animation configuration shape; the direction, speed, wavefront, and geometry controls are TideWM-specific. Values are snapshotted when a transition begins, so hot reload affects the next workspace switch.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Disables only workspace transitions; other water effects stay active. |
| `style` | `water` \| `glow` | `water` | `water` fills the output before revealing the new workspace. `glow` uses the original thin colored boundary. |
| `duration_ms` | integer, `50`–`5000` | `520` | Transition lifetime in milliseconds. `duration` is an alias. |
| `speed` | float, `0.1`–`10` | `1.0` | Speed multiplier applied to `duration_ms`: `2.0` is twice as fast and `0.5` is half speed. |
| `curve` | enum | `cubic-in-out` | Progress easing: `linear`, `cubic-out`, `cubic-in-out`, `quad-out`, or `exp-out`. `ease` is an alias. |
| `direction` | enum | `auto` | `auto` sweeps right-to-left for a higher-numbered workspace and left-to-right for a lower-numbered one. `left-to-right`/`ltr` and `right-to-left`/`rtl` force one direction. |
| `workspace_motion` | bool | `false` | Captures both desktops and slides the outgoing one out while the incoming one enters under the wave. Costs one additional transient full-output texture. `move_workspaces` is an alias. |
| `workspace_motion_delay_ms` | integer, `0`–`5000` | `150` | Delay after the water begins before both desktops start sliding. `100`, `200`, and `300` correspond to 0.1, 0.2, and 0.3 seconds. Values beyond 95% of the effective transition lifetime are clamped to that point. `motion_delay_ms` is an alias. |
| `wave_amplitude` | float, `0`–`500` | `34` | Maximum horizontal displacement of the moving boundary in physical pixels. `0` produces a straight wipe. `amplitude` is an alias. |
| `wave_frequency` | float, `0`–`20` | `3` | Sine cycles from the output’s top edge to its bottom edge. `0` removes vertical waviness. `frequency` is an alias. |
| `edge_width` | float, `0.5`–`250` | `18` | Half-width of the soft cross-fade boundary in physical pixels. Lower is sharper; higher is softer. |
| `color` | color | `8EDDFF` | Main water color, or the wavefront tint under `glow`. Accepts bare `RRGGBB`, quoted `"#RRGGBB"`, `rgb(RRGGBB)`, or `rgba(RRGGBB, AA)`. |
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
workspace_transition {
    enabled = true
    style = water
    duration_ms = 600
    speed = 1
    curve = cubic-out
    direction = auto
    workspace_motion = true
    workspace_motion_delay_ms = 150
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
visible oscillation), `riptide` (short and sharp), or `hypr-smooth`. The
`hypr-smooth` preset mirrors the maintainer's real Hyprland window settings:
open geometry uses the 300ms `overshot` curve, close geometry uses the 300ms
`easeInOut` curve, both slide through the nearest output edge, both fade on a
separate 400ms `easeInOut` clock, and layout motion uses 400ms `easeInOut`.
Explicit values in the same block always override the preset, even when
`preset` appears later. The top-level `enabled` disables all three transitions.
`slowdown` multiplies both geometry and opacity durations (`0.5` is twice as
fast, `2` twice as slow). Each `open`, `close`, and `movement` sub-block
supports:

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Disables only this transition. |
| `animate_size` | bool | movement `true`, lifecycle `false` | Interpolates the outer window size together with movement. It currently affects the `movement` block; `resize`, `size`, and `animate-size` are aliases. |
| `duration_ms` | integer, `1`–`10000` | open `190`, close `160`, movement `190` | Geometry lifetime before the visual state reaches its logical target. `duration` is an alias. |
| `curve` | easing | see example | `linear`, `quad-out`, `cubic-out`, `cubic-in-out`, `exp-out`, or CSS-compatible `cubic-bezier(x1,y1,x2,y2)`. `ease-out-quad`, `ease-out-cubic`, and `ease-out-expo` aliases are accepted. |
| `opacity_duration_ms` | integer, `1`–`10000` | follows `duration_ms` | Independent opacity lifetime. `fade_duration_ms` and `opacity_duration` are aliases. |
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

    open {
        duration_ms = 190
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
        duration_ms = 160
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
        duration_ms = 190
        curve = cubic-bezier(0.16,1,0.3,1)
        effect = tide
        wave_amplitude = 1.25
        wave_decay = 2.4
    }
}
```

### Interactive viscosity

`viscosity` controls TideWM's liquid drag and resize feel independently of the
fixed-duration `animations { movement { } }` transition. Pointer grabs update
the real window position, resize target, layout ratios, and hit-testing
immediately. The rendered window rectangle follows with refresh-rate-independent
exponential damping, including floating move/resize, tiled drag-to-swap, direct
split-border resize, and modifier-drag tiled resize. Repeated pointer events
retarget from the current on-screen rectangle, so motion stays continuous.

The state is bounded to one small record per moving window and stores no pointer
history, textures, or framebuffers. `0` disables it, `1.0` is the default,
and values up to `4.0` progressively slow settling. `water_effects = false`
bypasses it globally. A matching `rule { viscosity = ... }` overrides the
global value for one app.

### `connected_vessels { }`

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

### `depth { }`

Configures automatic attention depth and buoyancy. A mapped window starts at the surface. After `sink_after_ms` without focus or keyboard input it moves to tier 1, keeping its live content with reduced opacity and a cool-water wash. Each additional `tier_interval_ms` moves it one tier deeper, capped by `max_tier`; tier 2 and below use a cached box-and-title schematic instead of live client pixels. Focusing, clicking, or typing into the window returns it to tier 0 immediately. Urgent windows retain a bright bioluminescent border at every tier.

Depth state is bounded to one small record per mapped toplevel. Schematic buffers exist only for visible tier-2-or-deeper windows and are evicted when a window resurfaces, unmaps, or is destroyed. The inactivity scan reuses the backend’s bounded timer and is throttled to 10Hz. `water_effects = false` disables the model regardless of this block.

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

Configures Phase R2 frosted glass. A floating window selects it with `glass = frost` in a matching `rule { }`. The preferred path is client-provided background transparency (for example Kitty’s `background_opacity`): the client keeps text/foreground pixels opaque while TideWM blurs what its transparent background reveals. A TideWM `opacity` rule remains available, but it multiplies the entire surface, including text. For backward compatibility, an `opacity` below `1.0` with no explicit glass mode still selects water refraction. `glass = none` keeps ordinary compositor transparency without a captured-backdrop shader. `water_effects = false` bypasses both modes.

The frost shader uses a fixed-cost 25-tap Gaussian kernel over the existing window-sized backdrop capture, followed by adjustable strength, saturation, vibrancy, contrast, brightness, noise, and tint treatment. Changing these values does not allocate more buffers or increase the tap count. Capture runs immediately before the visible output bind so interactive moves sample the current window geometry in the same frame; one reusable ARGB8888 texture is kept per eligible floating window and resized only when the window dimensions change. Every key in this table also works inside a matching `rule { frost { } }` sub-block; omitted per-app keys inherit the global value.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Disables frost windows without changing water glass or their opacity rules. |
| `radius` | float, `0`–`64` | `12` | Maximum blur reach in physical pixels. `blur_radius` is an alias. Zero disables diffusion. |
| `strength` | float, `0`–`1` | `1.0` | Mix of sharp capture to fully diffused result. `blur_strength` and `frost` are aliases. |
| `opacity` | float, `0`–`1` | `1.0` | Opacity of the processed backdrop layer. Lower values mix the frost with the real, sharp desktop beneath it. `glass_opacity` and `background_opacity` are aliases. This does not change client text/content opacity. |
| `saturation` | float, `0`–`2` | `1.0` | `0` is grayscale, `1` preserves the captured color, values above `1` increase color. |
| `contrast` | float, `0`–`2` | `1.0` | Contrast modulation; inspired by Hyprland blur tuning. |
| `brightness` | float, `0`–`2` | `1.0` | Multiplier applied after saturation. |
| `noise` | float, `0`–`0.25` | `0.0` | Static grain that can hide banding in smooth blurred gradients. `grain` is an alias. |
| `noise_scale` | float, `0.25`–`16` | `1.0` | Physical size of the noise cells. `grain_scale` is an alias. |
| `vibrancy` | float, `0`–`1` | `0.0` | Extra saturation beyond the base `saturation`. |
| `vibrancy_darkness` | float, `0`–`1` | `0.0` | Biases extra vibrancy toward darker pixels. |
| `tint_color` | color | `8EDDFF` | Optional frost tint. `color` is an alias. It has no effect while `tint_alpha = 0`. |
| `tint_alpha` | float, `0`–`1` | `0.0` | Strength of the tint mix. The neutral default adds no color layer. |
| `corner_radius` | float, `0`–`256` | `0` | Rounded clipping radius in physical pixels. `rounding` is an alias. |
| `corner_softness` | float, `0.25`–`8` | `1.0` | Antialias/feather width at the rounded edge, in physical pixels. |

```wave
frost {
    enabled = true
    radius = 12
    strength = 1.0
    opacity = 1.0
    saturation = 1.0
    contrast = 1.0
    brightness = 1.0
    noise = 0.0
    noise_scale = 1.0
    vibrancy = 0.0
    vibrancy_darkness = 0.0
    tint_color = 8EDDFF
    tint_alpha = 0.0
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

Every key also works inside `rule { shadow { } }`; omitted fields inherit the global block and multiple matching shadow sub-blocks merge field by field. Colors accept `RRGGBB`, `RRGGBBAA`, quoted `"#RRGGBBAA"`, `rgb(...)`, `rgba(...)`, and legacy Hyprland `0xAARRGGBB`.

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
| `shapes` | space-separated list | `ring` | Compatibility mode: any combination of `ring`, `square`, `droplet`, and `cross`. Assigning this key automatically selects `preset = legacy`. `shape` and `form` are aliases. |
| `color` | color | `8EDDFF` | RGB tint. Use bare `RRGGBB`, quoted `"#RRGGBB"`, `rgb(RRGGBB)`, or `rgba(RRGGBB, AA)`. A bare leading `#` starts a Waves comment, so it must be quoted. Alpha in the color form is currently ignored; use `peak_alpha`. |
| `secondary_color` | color | `E8FCFF` | Gradient, membrane, and highlight tint. `accent_color` and `highlight_color` are aliases. |
| `peak_radius` | positive float | `220` | Maximum radius in logical pixels. `radius` is an alias. |
| `size_mode` | enum | `fixed` | `fixed` uses `peak_radius`; `window` uses half the diagonal; `width`, `height`, `min`, and `max` use half that window dimension. `radius_mode` and `scale_mode` are aliases. |
| `size_scale` | float | `1.0` | Multiplies window-derived sizes, clamped to `0.01`–`8.0`. `radius_scale` and `window_scale` are aliases. |
| `min_radius` | float | `24` | Lower clamp applied after adaptive sizing. |
| `max_radius` | float | `2048` | Upper clamp applied after adaptive sizing. |
| `thickness` | positive float | `8` | Outline half-width in logical pixels. |
| `duration_ms` | positive integer | `650` | Ripple lifetime in milliseconds. `duration` is an alias. |
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
    duration_ms = 650
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

### `env { }`

`KEY = VALUE` pairs, applied to TideWM's own process before the backend starts (so e.g. `XCURSOR_THEME` here actually changes the cursor theme TideWM itself loads, not just what child processes see) and folded into the systemd/D-Bus session-activation environment alongside `WAYLAND_DISPLAY`, so anything session-activated (a portal backend, a polkit agent) sees them too.

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

Per-connector overrides, **udev backend only** — winit's single simulated output has no real mode list or transform-as-monitor-orientation meaning. Purely opt-in: an output with no matching block auto-configures (preferred mode, auto-positioned to the right of whatever's already mapped, scale 1, no rotation). One block per connector; repeat the block (in the same or another included file) for a second monitor.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| header | string | — | Connector name, e.g. `eDP-1`, `DP-2`. Check your logs or the `outputs` IPC query for what TideWM detected. |
| `enabled` | bool | `true` | Set `false` to leave a connected output unused. |
| `mode` | string, optional | connector's preferred mode | `1920x1080` or `1920x1080@60`. Falls back to the connector's own preferred mode if unset or unmatched. |
| `position` | `WxH`, optional | auto-layout | e.g. `1920x0`. Falls back to auto-layout (rightmost edge of already-mapped outputs) if unset. |
| `scale` | float | `1.0` | |
| `transform` | string | `normal` | One of `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`, `flipped-180`, `flipped-270`. |
| `gaps` | integer, optional | global `gaps` | Per-output gap override for every workspace shown on this connector. A `workspace_gaps` entry beats it. |

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
| `app_id` | string, optional | Matches exactly. At least one of `app_id`/`title` is required — a rule with neither never matches anything. |
| `title` | string, optional | Matches case-insensitively, anywhere in the string. |
| `app_id_regex` | regular expression, optional | Rust regex matched against the full app ID string. Combines with other criteria in the same rule. |
| `title_regex` | regular expression, optional | Rust regex matched against the title. Use `(?i)` for case-insensitive matching. |
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
| `glass` | `water`, `frost`, or `none` | Captured-backdrop treatment for a floating window. Explicit `water`/`frost` works with client-provided alpha; when unset, a TideWM `opacity` below `1.0` implicitly selects `water`. `none` preserves plain transparency. `glass_mode` is an alias. |
| `viscosity` | float, `0`–`4` | Per-app interactive move/resize damping. Last matching rule wins; `0` disables it for the matched app. |
| `sway` | bool | Per-app opt-in/out for floating sway. Last matching rule wins; unset falls back to `sway.enabled`. |
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

Excludes a layer-shell surface (a bar, panel, or launcher, not an ordinary app window) from screenshots and screencasts by namespace, without hiding it from your own screen (niri's `layer-rule { block-out-from ... }`) — for something like a password-manager quick-access panel that shouldn't end up in a recording. One block per rule; repeat for more.

| Key | Type | Notes |
| --- | --- | --- |
| `namespace` | string, optional | Matches case-sensitively, anywhere in the surface's namespace string (the name the client itself sets — rofi's is `rofi`). Required — a rule with no `namespace` never matches anything. |
| `block_capture` | bool | Default `false`. When `true`, the matched surface's rect renders as solid black in `wlr-screencopy`/`ext-image-copy-capture` output instead of its real content. |

```
layer_rule {
    namespace = rofi
    block_capture = true
}
```

### `bind`

`bind <Modifier+Key> = <action-string>`. Modifiers: `Super`/`Logo`/`Mod4`, `Ctrl`/`Control`, `Alt`, `Shift`, combined with `+`. Key names are matched against the *unshifted* keysym, so it doesn't matter whether you write the letter upper or lowercase. See [Action strings](#action-strings) for every value a bind can take, and the generated `config.wave` for the shipped defaults (also summarized in README's Quick Start table). A later `bind` on the same combo overrides an earlier one.

### `submap <name> { }`

A temporary alternate keybind table (sway/Hyprland's "mode" idea), same `bind` statements as the top level (no modifier prefix needed if the submap's own binds are unmodified, like the shipped `nav` example). Entered via a `submap:<name>` action, which **fully replaces** the base binds — not layered on top of them — until an explicit `exit-submap` bind. Not tied to focus; stays active until you explicitly leave it. A config reload that drops or renames the active submap auto-exits back to the base binds.

```
submap nav {
    bind h = focus-left
    bind l = focus-right
    bind k = focus-up
    bind j = focus-down
    bind Escape = exit-submap
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
- `focus-left` / `focus-right` / `focus-up` / `focus-down`
- `swap-left` / `swap-right` / `swap-up` / `swap-down`
- `resize-left` / `resize-right` / `resize-up` / `resize-down` — shrink/grow the focused floating window by 24 logical pixels, or resize its nearest BSP split and connected parallel ancestors
- `layout:bsp` / `layout:master` — switch the current workspace's tiling algorithm
- `master-grow` / `master-shrink` — nudge the master/stack ratio (master layout only, no-op under BSP)

**Groups (window tabbing)**
- `group-left` / `group-right` / `group-up` / `group-down`
- `ungroup`
- `cycle-tab-next` / `cycle-tab-prev`

**Workspaces**
- `workspace:<N>` — switch to workspace `N`
- `move-to-workspace:<N>` — move the focused window to workspace `N`
- `swap-workspaces:<output-name>` — swap this output's and the named output's active-workspace content
- `workspace:<name>` / `move-to-workspace:<name>` — same two actions, addressed by a `workspace_name` alias instead of a number (see below) — a workspace's real identity is always its number, this is just another way to spell it

**Modes**
- `submap:<name>` — enter a `submap <name> { }` block
- `exit-submap`
- `toggle-overview` — schematic grid of every workspace on the current output (see README's Features list; not live thumbnails)

**Outputs**
- `toggle-dpms` — toggle every output's power together (all on, or all off)

**Process and session**
- `spawn:<command>` — args split on whitespace, no shell
- `quit`

## IPC and `tidectl`

`$XDG_RUNTIME_DIR/tidewm-<pid>.sock`: one JSON request line in, one JSON response line out, per connection. Read queries return structured data; `{"request": "action", "action": "<any string above>"}` runs any action string. `{"request":"batch","actions":["workspace:2","spawn:kitty"]}` validates the complete list first, then executes up to 128 actions in order, so an invalid later item cannot leave a half-run batch. This is genuinely the same path a keybind press uses (`config::parse_action` → `Smallvil::run_action`).

Queries: `outputs`, `workspaces`, `windows`, `focused-window`, `active-submap`.

**Subscribe (event stream).** The same socket also supports a long-lived mode for reactive widgets (a waybar module, an eww `deflisten`, a QuickShell socket reader) that shouldn't have to poll the queries above. Send `{"request": "subscribe", "events": ["window", "workspace", "focus", "urgent", "depth", "config"]}` as the first and only request on a fresh connection; omitting `events` (or sending an empty array) subscribes to all six channels. The server replies with one ack line (`{"ok": true, "data": {"subscription_id": <n>, "events": [...]}}` — the resolved channel list, so a typo'd filter is visible at handshake time instead of silently matching nothing), then keeps the connection open and writes one `{"event": "<kind>", "data": ...}` line per matching change until the client disconnects. The connection's lifetime is the subscription's lifetime.

Event kinds: `window-opened` / `window-closed` / `window-changed` (channel `window`), `workspace-changed` (`workspace`), `focus-changed` (`focus`), `urgent-changed` (`urgent`), `depth-changed` (`depth`), `config-reloaded` (`config`). Window-carrying events embed the same object shape the `windows` query returns; `focus-changed` is `null` when focus clears to nothing, and `window-closed` carries the closed window's last-known `window_id`/`app_id`/`title` directly (its tracking state is already gone by the time the event serializes). A subscriber that falls behind (roughly a quarter-meg of unwritten JSON) is disconnected outright rather than let its backlog grow unbounded.

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
--json                          # on any query, for scripting
```

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
| `xdg-toplevel-icon-v1` | Client-provided application/window icons | Done — names and buffers are retained in committed surface state for launchers and future TideWM UI consumers |
| `ext-session-lock-v1` | Screen lock (`swaylock`, `hyprlock`) | Done — a crashed lock client terminates the compositor session fail-closed so the login manager can recover; it never auto-unlocks |
| `ext-foreign-toplevel-list-v1` | Read-only toplevel list | Done |
| `wlr-foreign-toplevel-management-v1` | Bidirectional toplevel control (waybar's `wlr/taskbar`, ags v1) | Done |
| `wlr-screencopy-unstable-v1` + `ext-image-copy-capture-v1` | Screenshots (`grim`) | Done — output and per-window ext capture; SHM everywhere plus direct DMA-BUF rendering for full-output wlr capture on DRM sessions |
| `wlr-data-control-unstable-v1` | Clipboard managers (`cliphist`, `wl-clip-persist`) | Done, including primary selection |
| `wp-security-context-v1` | Sandboxed Wayland listener and policy identity | Done — sandboxed clients are denied session-lock, IME/virtual-keyboard, global clipboard control, capture, output-control, and foreign-toplevel globals |
| `xdg-output-manager-v1` | Output geometry disclosure | Done |
| `idle-inhibit-unstable-v1` / `ext-idle-notify-v1` | Idle inhibition and notification | Done |
| `wp-pointer-constraints-v1` + `wp-relative-pointer-v1` | Pointer lock/confine, relative motion | Done — verified live (Minecraft camera-look) |
| `wp-pointer-gestures-v1` | Touchpad gesture events to clients | Done — unbound streams reach clients; configured swipe/pinch streams are consumed atomically by compositor actions |
| `zwp-keyboard-shortcuts-inhibit-v1` | Let a client (VM, remote desktop) capture all shortcuts | Done |
| `zwp-text-input-v3` + `zwp-input-method-v2` + `zwp-virtual-keyboard-v1` | IME support | Done — app-side activation verified live; see CHANGELOG for the exact verification bar per sub-protocol |
| `wp-cursor-shape-v1` | Named-cursor requests (Qt6/GTK4, QuickShell) | Done |
| `wlr-output-management-unstable-v1` | Runtime output reconfiguration (`wlr-randr`, `kanshi`, `wdisplays`) | Done — position/transform/scale apply live; disabling an output or changing resolution needs real hardware to verify a live modeset |
| `wlr-output-power-management-unstable-v1` | Display on/off (DPMS) | Protocol + render-loop logic done; real CRTC power toggle unverified on hardware |
| `zwlr-gamma-control-manager-v1` | Night-light tools (`wlsunset`, `gammastep`) | Protocol + DRM gamma ioctls done; real color-change unverified on hardware |
| `org.freedesktop.a11y.KeyboardMonitor` (DBus, not a Wayland protocol) | Screen reader (Orca) grabbing/watching keys system-wide | Done, behind the `accessibility` Cargo feature (off by default, `cargo build --features accessibility`) — see CHANGELOG for the verification bar |
| `org.gnome.Mutter.ScreenCast` + PipeWire | Monitor/window video streams for `xdg-desktop-portal-gnome`-based setups | Behind the `screencast` Cargo feature — DBus session lifecycle works, and the PipeWire MemFd/SHM path now delivers real frames under the nested (winit) backend (verified with a direct PipeWire consumer: correctly-oriented, live-updating content, PSS flat over a sustained stream). Not yet verified on a standalone TTY (udev/DRM) session. DMA-BUF export stays disabled (fails on real hardware). Not yet run through a real portal-mediated client (OBS/Discord) |
| `org.freedesktop.impl.portal.ScreenCast` (DBus, the real `xdg-desktop-portal` backend interface) | Discord/OBS-style screen sharing | Behind the `screencast` Cargo feature, self-contained (no `xdg-desktop-portal-gnome` needed), with compositor-owned monitor/window/virtual-source selection, reusing the same now-working PipeWire path as the row above. Virtual sources still mirror the selected desktop dimensions rather than creating a headless DRM connector, and this exact entry point hasn't been exercised through a real `xdg-desktop-portal` process yet |

Everything else on the original protocol/rice compatibility list is implemented. Screencasting is the remaining loose end: the DBus/portal plumbing and PipeWire MemFd frame delivery both work now under the nested backend, verified directly, but the standalone udev/DRM backend and a real portal-mediated client (OBS/Discord) on real hardware haven't confirmed the full chain yet, and DMA-BUF export needs real GBM-backed allocation before it's usable.
