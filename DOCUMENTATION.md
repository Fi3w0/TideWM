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
| `show_welcome_hint` | bool | `true` | Shows a persistent "fake window" card pointing you at `Super+Return` whenever the desktop is otherwise empty. Disappears the instant a real window maps; delete this key (or set it `false`) to stop it coming back. Checked on every reload, not just at startup. |
| `water_effects` | bool | `true` | Reserved for the water/aqua visual identity. The toggle exists now so config written today won't need migrating later; nothing reads it yet since `render/` isn't built. |
| `cursor_always_visible` | bool | `false` | Forces the udev backend's software cursor to stay visible even when a client asks to hide it (e.g. a terminal hiding its own pointer glyph after inactivity). Off by default — respecting a client's own hide request is correct behavior; this is an opt-in override. |
| `cursor_hide_after_ms` | integer | `0` | udev backend only: hides the software cursor after this many milliseconds of no real pointer motion (niri's `cursor.hide-after-inactive-ms`). `0` disables it. Independent of `cursor_always_visible` — that overrides a *client's* hide request, this is a compositor-driven idle timer, and the two can be combined. |
| `workspace_auto_back_and_forth` | bool | `false` | Re-selecting the already-active workspace jumps back to whichever one was active immediately before it, instead of no-opping (niri's own feature of the same name). |
| `workspace_name` | repeatable key | none | Names a workspace number for use in `workspace:<name>`/`move-to-workspace:<name>` (niri's `set-workspace-name`, Hyprland's `workspace name:foo`) — `workspace_name = 3 web`, repeat the key once per name. Purely an addressing convenience: the workspace's real identity is still its number. An unknown name at action time warns and no-ops rather than switching. |
| `gaps` | integer | `8` | Pixel gap the tiling engine applies around and between tiles, both layout algorithms. |
| `default_layout` | `bsp` \| `master` | `bsp` | Starting tiling algorithm for a workspace with no runtime override (see `layout:bsp`/`layout:master` actions below). `bsp` is dwindle-style: split orientation follows each window's own aspect ratio. `master` is one master pane plus an evenly-split stack. |
| `master_orientation` | `left` \| `right` \| `top` \| `bottom` | `left` | Which side the master pane sits on under `default_layout = master`. `left`/`right` stack the other windows vertically in the remaining strip; `top`/`bottom` stack them horizontally instead. One global setting, not per-workspace. |
| `bsp_split_bias` | `auto` \| `horizontal` \| `vertical` | `auto` | Manual override for `default_layout = bsp`'s per-split axis choice. `auto` is the existing aspect-ratio-driven behavior, unchanged. `horizontal`/`vertical` force every split one way regardless of window/output shape (Hyprland dwindle's `force_split` idea). One global setting, not per-workspace. |
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
| `opacity` | float | Per-window alpha, clamped to `0.0`–`1.0`. Applies to the whole window surface tree. |
| `position` | `<x>x<y>`, optional | Exact floating placement. No-op unless the window ends up floating. |
| `size` | `<width>x<height>`, optional | Exact floating size. No-op unless the window ends up floating. |

Multiple rules can match the same window: `workspace`/`output`/`position`/`size` take the *last* match; boolean effects accumulate (any match sets one, never unsets it).

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
- `raise-window` / `lower-window` — floating windows only, no-op on a tiled one

**Focus and layout**
- `cycle-focus` — most-recently-used order, not z-order
- `focus-urgent` — jump to whichever window is currently marked urgent, if any
- `focus-left` / `focus-right` / `focus-up` / `focus-down`
- `swap-left` / `swap-right` / `swap-up` / `swap-down`
- `resize-left` / `resize-right` / `resize-up` / `resize-down` — shrink/grow the focused floating window by 24 logical pixels, or move its nearest BSP split
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
| `org.gnome.Mutter.ScreenCast` + PipeWire | Monitor/window video streams for `xdg-desktop-portal-gnome`-based setups | Behind the `screencast` Cargo feature — DBus session lifecycle works, but PipeWire streaming is broken (DMA-BUF allocation fails on real hardware, MemFd fallback is unreliable). Not usable for real screen sharing yet |
| `org.freedesktop.impl.portal.ScreenCast` (DBus, the real `xdg-desktop-portal` backend interface) | Discord/OBS-style screen sharing | Behind the `screencast` Cargo feature, self-contained (no `xdg-desktop-portal-gnome` needed), with compositor-owned monitor/window/virtual-source selection — but see the PipeWire row above, the actual stream doesn't work reliably yet. Virtual sources also mirror the selected desktop dimensions rather than creating a headless DRM connector |

Everything else on the original protocol/rice compatibility list is implemented. Screencasting is the one exception: the DBus/portal plumbing is there, but PipeWire buffer delivery is broken and needs real GBM-backed DMA-BUF allocation before it's usable.
