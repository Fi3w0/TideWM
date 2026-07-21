# TideWM Documentation

Full reference for configuring and controlling TideWM: every config key, every action string, IPC, and the protocol matrix.

- [Command-line flags](#command-line-flags)
- [Config file](#config-file)
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

`$XDG_CONFIG_HOME/tidewm/config.wave`, or `~/.config/tidewm/config.wave` if `XDG_CONFIG_HOME` isn't set (or the path given to `--config`, see above). Written out with working defaults on first run. Almost every change hot-reloads on save — no restart needed — and a bad edit is reported (with a file/line pointing at the exact problem) and the previous config kept running, not silently discarded. The two documented exceptions: `xwayland { enabled }` (spawning/tearing down `xwayland-satellite` isn't done live) and `input { touchpad { ... } }` (re-reads on save, but only reaches a touchpad connected *after* the edit — see that section below). Everything else, including keyboard layout, applies immediately.

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
- `output <name> { }` and `rule { }` blocks accumulate — entries from every file all end up present.
- A later `include` overlays an earlier one.
- **The including file's own keys always win over anything it includes.** If `config.wave` includes `overrides.wave`, and both set `gaps`, `config.wave`'s own value wins — put an override directly in the file doing the including, not in a file you list last.
- A broken include (missing, unreadable, unparseable, or a cycle) is skipped with a warning; it doesn't fail the whole config.
- The config directory is watched recursively (`*.wave` files only, dotfiles/dotdirs skipped) — editing any included file hot-reloads exactly like editing the main file.

## Config reference

### Top-level keys

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `terminal` | string | `"kitty"` | Spawned by the default `Super+Return` bind. The shipped default is `$wave(kitty, alacritty, foot, xterm)` — see [`$wave(...)`](#waves-format) above. |
| `show_welcome_hint` | bool | `true` | Shows a persistent "fake window" card pointing you at `Super+Return` whenever the desktop is otherwise empty. Disappears the instant a real window maps; delete this key (or set it `false`) to stop it coming back. Checked on every reload, not just at startup. |
| `water_effects` | bool | `true` | Reserved for the water/aqua visual identity. The toggle exists now so config written today won't need migrating later; nothing reads it yet since `render/` isn't built. |
| `cursor_always_visible` | bool | `false` | Forces the udev backend's software cursor to stay visible even when a client asks to hide it (e.g. a terminal hiding its own pointer glyph after inactivity). Off by default — respecting a client's own hide request is correct behavior; this is an opt-in override. |
| `gaps` | integer | `8` | Pixel gap the tiling engine applies around and between tiles, both layout algorithms. |
| `default_layout` | `bsp` \| `master` | `bsp` | Starting tiling algorithm for a workspace with no runtime override (see `layout:bsp`/`layout:master` actions below). `bsp` is dwindle-style: split orientation follows each window's own aspect ratio. `master` is one master pane plus an evenly-split stack, always left/right. |
| `pseudo_tile_scale` | float, `0.05`–`1.0` | `0.7` | Fraction of its tile a pseudo-tiled window keeps, centered within it. Out-of-range values are clamped, not rejected. |
| `spawn_at_startup` | repeatable key | none | Commands launched once at startup — repeat the key once per command (`spawn_at_startup = waybar` on its own line, again for the next one), not one line holding a list. Args split on whitespace — no shell involved, so quoting/globs/pipes aren't supported; wrap in `sh -c "..."` yourself if you need those. |

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

udev backend only — winit's nested host input never reaches a real libinput device, so these sit unused there. Every key is opt-in: omit it and that setting is left at whatever your driver already defaults to. Applied once per touchpad-capable device (libinput's tap-finger-count check) when it's reported by libinput — startup enumeration and hotplug both fire this, but **a config edit here needs a restart to reach an already-connected touchpad** (unlike everything else in this table, this one isn't reloaded live).

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

```
input {
    repeat_delay = 200
    repeat_rate = 25
    xkb_layout = us

    touchpad {
        natural_scroll = true
        tap_to_click = true
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
| `workspace` | integer, optional | Initial workspace, same numbering as `workspace:N` keybinds (including `0`, the scratchpad). |
| `output` | string, optional | Initial output by connector name. Falls back to normal placement if unset or unconnected. |
| `float` | bool | Default `false`. |
| `pseudo_tile` | bool | Default `false`. No-op unless the window ends up tiled; ignored if `float`/`pin` also apply. |
| `pin` | bool | Default `false`. Implies `float`. |
| `tile` | bool | Default `false`. Forces tiled even if the auto-float heuristic (a window with a parent, e.g. a dialog, or one whose min/max size are equal, e.g. a splash screen) would otherwise float it. No effect if `float`/`pin` also match. |
| `no_focus` | bool | Default `false`. Maps without stealing focus — whatever was focused before stays focused. |
| `position` | `<x>x<y>`, optional | Exact floating placement. No-op unless the window ends up floating. |
| `size` | `<width>x<height>`, optional | Exact floating size. No-op unless the window ends up floating. |

Multiple rules can match the same window: `workspace`/`output`/`position`/`size` take the *last* match, `float`/`pseudo_tile`/`pin`/`tile`/`no_focus` accumulate (any match sets it, never unsets it).

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
- `raise-window` / `lower-window` — floating windows only, no-op on a tiled one

**Focus and layout**
- `cycle-focus` — most-recently-used order, not z-order
- `focus-urgent` — jump to whichever window is currently marked urgent, if any
- `focus-left` / `focus-right` / `focus-up` / `focus-down`
- `swap-left` / `swap-right` / `swap-up` / `swap-down`
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

`$XDG_RUNTIME_DIR/tidewm-<pid>.sock`: one JSON request line in, one JSON response line out, per connection. Read queries return structured data; `{"request": "action", "action": "<any string above>"}` runs any action string. This is genuinely the same path a keybind press uses (`config::parse_action` → `Smallvil::run_action`), so anything a keybind can do is scriptable from a shell, and anything a later version adds to the action catalog is IPC-addressable for free.

Queries: `outputs`, `workspaces`, `windows`, `focused-window`, `active-submap`.

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
| `ext-session-lock-v1` | Screen lock (`swaylock`, `hyprlock`) | Done |
| `ext-foreign-toplevel-list-v1` | Read-only toplevel list | Done |
| `wlr-foreign-toplevel-management-v1` | Bidirectional toplevel control (waybar's `wlr/taskbar`, ags v1) | Done |
| `wlr-screencopy-unstable-v1` + `ext-image-copy-capture-v1` | Screenshots (`grim`) | Done — output-level only, no per-window capture |
| `wlr-data-control-unstable-v1` | Clipboard managers (`cliphist`, `wl-clip-persist`) | Done — regular selection only, no primary selection |
| `xdg-output-manager-v1` | Output geometry disclosure | Done |
| `idle-inhibit-unstable-v1` / `ext-idle-notify-v1` | Idle inhibition and notification | Done |
| `wp-pointer-constraints-v1` + `wp-relative-pointer-v1` | Pointer lock/confine, relative motion | Done — verified live (Minecraft camera-look) |
| `wp-pointer-gestures-v1` | Touchpad gesture events to clients | Done (protocol only — no built-in compositor gesture binds yet) |
| `zwp-keyboard-shortcuts-inhibit-v1` | Let a client (VM, remote desktop) capture all shortcuts | Done |
| `zwp-text-input-v3` + `zwp-input-method-v2` + `zwp-virtual-keyboard-v1` | IME support | Done — app-side activation verified live; see CHANGELOG for the exact verification bar per sub-protocol |
| `wp-cursor-shape-v1` | Named-cursor requests (Qt6/GTK4, QuickShell) | Done |
| `wlr-output-management-unstable-v1` | Runtime output reconfiguration (`wlr-randr`, `kanshi`, `wdisplays`) | Done — position/transform/scale apply live; disabling an output or changing resolution needs real hardware to verify a live modeset |
| `wlr-output-power-management-unstable-v1` | Display on/off (DPMS) | Protocol + render-loop logic done; real CRTC power toggle unverified on hardware |
| `zwlr-gamma-control-manager-v1` | Night-light tools (`wlsunset`, `gammastep`) | Protocol + DRM gamma ioctls done; real color-change unverified on hardware |

Everything on this project's original "rice ecosystem compatibility" list is implemented. Still open beyond this matrix: compositor-bound touchpad gestures, workspace-overview accessibility, and screencasting's PipeWire half (the D-Bus interface exists; `Session.Start` errors until it's built).
