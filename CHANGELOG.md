# Changelog

All notable changes to TideWM are documented here. Format loosely follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added
- Screencast portal now lists monitor, window, and virtual sources with a compositor-owned picker instead of grabbing the first monitor.
- Touchpad swipe/pinch gestures can trigger any compositor action. Verified live on real hardware (external USB Apple Magic Trackpad and a ThinkPad's built-in touchpad): all four swipe directions and `pinch_in` confirmed, `pinch_out` not yet confirmed. See AGENT.md's Phase M gesture section for the full account.
- Per-window `opacity` window rule.
- `xdg-toplevel-icon-v1` support.
- Named scratchpads: `toggle-scratchpad:<name>` / `move-to-scratchpad:<name>` action variants, any number of them, on top of the existing single scratchpad (which stays the bare `toggle-scratchpad`). Each name is just another reserved workspace under the hood -- same hide/show machinery, no new data structure. The IPC `workspaces` query tags scratchpad entries with a `scratchpad` name field so bars can label or hide them.
- Per-workspace and per-output gap overrides: repeatable `workspace_gaps = <N|name> <pixels>` lines (names resolve through `workspace_name` aliases) and a `gaps` key inside `output` blocks. Workspace beats output beats the global `gaps`.
- Window swallowing: a tiled window matching a `swallow = true` window rule is hidden when a window its process spawned maps, and that child takes over its exact tile; closing the child puts it back in the same slot. PID ancestry is read from `/proc`, so it works for any terminal without shell integration. Verified live in a nested session with grim screenshots: exact-slot replacement and restore, uninvolved tiles untouched.

### Fixed
- Fullscreen windows no longer render beneath layer-shell bars/launchers, in both live frames and screenshots.
- A crashed session-lock client now fails closed (the compositor exits) instead of leaving the session in an unclear state.
- PipeWire screencasting now actually produces frames. The producer stream never called `pw_stream_trigger_process()`, which a `StreamFlags::DRIVER` stream needs to start each graph cycle, so `process()` simply never fired. Verified under the nested winit backend against a real PipeWire daemon with a direct consumer: correct, live-updating frames over the MemFd/SHM path, PSS flat over a sustained stream. See AGENT.md's Screencasting section for the full root-cause writeup.
- Real OBS screencasting over the udev/DRM backend, on real hardware, fixed and verified live: OBS's log showed `pipewire: ... error: no more input formats` (a PipeWire negotiation error), but the actual bug was one level up, in `xdg-desktop-portal.service` itself. A systemd `--user` manager that outlives a session switch (SDDM "switch session", any relogin that doesn't tear the user manager down) keeps whatever `xdg-desktop-portal.service` instance was already running from the previous login, with the previous desktop's `XDG_CURRENT_DESKTOP` baked into its own already-running process environment -- `dbus-update-activation-environment`/`systemctl --user import-environment` (already correct, see the "Session environment" section of AGENT.md) only affect *future* activations, not a process already running. Confirmed live on this machine: the frontend had `XDG_CURRENT_DESKTOP=Hyprland` in `/proc/<pid>/environ` from an earlier Hyprland login on the same systemd user manager, silently routing every screencast request (OBS included) to `xdg-desktop-portal-hyprland` instead of this compositor's own backend -- with no error anywhere on TideWM's side to see, since the request never reached it. `main.rs` now detects this (checks the running frontend's actual `/proc` environment, not just the activation environment) and restarts only the frontend, once, at startup, when it's stale. Discord's own screen-share also confirmed working end to end on the same real hardware session.

- Keys no longer get stuck "held" in a nested session when the host compositor takes keyboard focus mid-chord (e.g. the host's own Super+L lock shortcut): the release the host swallowed left xkb's modifier state -- and the `wl_keyboard.modifiers` every client is told -- carrying a phantom Super, so plain drags acted as Super-drags and kitty decoded typing as CSI-u escape sequences. On host focus loss TideWM now synthesizes releases for everything still marked pressed, the same all-released semantics `wl_keyboard.leave` implies. Found by the Nvidia nested test pass, which also identified the root cause.
- Resizing the nested window (or applying a wlr-output-management transform/scale change) now actually retiles: the layer map's cached usable-area zone was only ever recomputed on layer-surface events, so `retile()` faithfully laid tiles out into the *old* output size while the wallpaper filled the new one. Reproduced live with a host-side resize, fixed by arranging the layer map at both mode-change sites.
- The nested backend now queries the host monitor's real refresh rate (was hardcoded 60Hz for both the advertised mode and the render-loop cadence -- the loop stays a bounded timer, just at the real rate), forwards the host's real scale factor including fractional (was always 1, which mis-sized anything DPI-aware on a scaled host monitor), and titles its window "TideWM" instead of the "Smithay" default.
- Screenshots taken inside a nested (winit) TideWM session are no longer vertically flipped. The output-capture render inherited the output's advertised transform, and the winit backend advertises `Flipped180` purely to cancel its EGL surface's y-orientation at present time -- baking that into the offscreen capture texture inverted the readback. Output captures now render with an explicit `Normal` transform, same as the window-capture path always did, and the capture privacy black-out rects follow the same transform so they stay aligned. Verified with grim in a nested session, full-output and region captures both. The udev backend (a normal, unrotated output) was never affected. Nested screencast frames shared the same pixels, so their orientation changes identically; real OBS/Discord screencasting was verified on udev and is untouched.

### Known issues
- DMA-BUF export is still disabled and still fails on real hardware, unrelated to the fixes above; MemFd/SHM is the supported transport.
- Portal virtual sources mirror the desktop instead of creating a real headless output.
- winit 0.30.13 (upstream, nested backend only) can panic with `failed to get pointer data` when the host changes seat capabilities under it (seen with a host session lock); crashes the nested dev process, not fixable from TideWM's side.
- A lone tiled window not filling the output, seen once in the Nvidia nested pass on a 125% host monitor, could not be reproduced at scale 1 and is plausibly the scale bug fixed above; needs a retest on a fractional-scale host.

## [0.58.0] - 2026-07-22

### Added
- A real `xdg-desktop-portal` ScreenCast backend (`org.freedesktop.impl.portal.ScreenCast`), self-contained, no `xdg-desktop-portal-gnome`/GTK4 chain needed. v1 is monitor-only, one stream, no source picker yet.
- Ships `share/xdg-desktop-portal/tidewm.portal` and `tidewm-portals.conf` for install.

## [0.57.0] - 2026-07-22

### Added
- Persistent on-screen panel for config parse errors, replacing the old timed toast for that case.
- Built-in 4K Tide wallpaper fallback, replaced by any layer-shell wallpaper tool once one is running.
- `wp-security-context-v1`: sandboxed clients can't see session-lock, IME, clipboard, capture, or output-management globals.
- Per-window capture/screencast source selection, and a full-output DMA-BUF screenshot fast path on DRM sessions.
- Daily-use additions: `resize-*` actions, IPC batch requests, regex window matching, initial fullscreen/maximize rules, per-window `block_capture`.

### Fixed
- Nested sessions no longer leak the host compositor's desktop identity into spawned children.
- Interactive move/resize now needs a real held Super key, not just a latched modifier.

## [0.56.0] - 2026-07-22

### Added
- Output screencasting over PipeWire (SHM-backed).
- AccessKit/AT-SPI tree for TideWM's own UI: workspaces, overview, toasts.
- Compositor workspace swipes on the touchpad.
- Primary-selection protocol support.

### Fixed
- Floating-window output-disconnect migration, popup null-buffer lifecycle, touch-tap focus, capture cursor parity.
- Process lifecycle hardening: children are reaped via SIGCHLD, IPC/capture/DBus connections get bounds and idle timeouts.

## [0.55.0] - 2026-07-22

### Added
- `org.freedesktop.a11y.KeyboardMonitor` (behind the `accessibility` feature): lets a screen reader like Orca grab or watch keys system-wide, ported from niri's implementation.

## [0.54.0] - 2026-07-21

### Added
- Window-rule and layout tier: `no_focus`, `position`, `size` rules; `master_orientation`; `workspace_auto_back_and_forth`; `toggle-dpms` action; `cursor_hide_after_ms`; `bsp_split_bias`; named workspaces; per-namespace layer-shell capture exclusion.
- `raise-window`/`lower-window`, most-recently-used `cycle-focus`, urgent-window tracking, auto-float for dialogs, no-modifier floating edge-resize, touchscreen input, disconnected-output window migration.

## [0.52.1] - 2026-07-21

### Fixed
- Touchpad config now hot-reloads for an already-connected device, not just a newly plugged one.

## [0.52.0] - 2026-07-21

### Added
- Replaced TOML config with Waves, TideWM's own line-based format (`config.wave`), closer to Hyprland's syntax.
- `$wave(a, b, c)`: resolves to the first installed candidate, used to make `terminal` portable across machines.
- `cursor_always_visible` config key.

## [0.51.3] - 2026-07-21

### Fixed
- `wl_output` global leak on monitor disconnect.
- A fence-wait error that could hang the compositor forever on a bad GPU fence.
- A startup panic when no outputs are mapped yet.

## [0.51.2] - 2026-07-21

### Tests
- First real multi-monitor hardware test: hotplug connect works; disconnect left a stale `wl_output` global (fixed in 0.51.3).
- `ext-session-lock-v1` verified live against `hyprlock`, including a real unlock.

## [0.51.1] - 2026-07-21

### Fixed
- The welcome hint's "delete to dismiss" setting doing nothing.

### Tests
- Real-hardware pass on AMD: DPMS, gamma/night-light, workspace switching, pseudo-tiling, and pin all confirmed working. Flat ~50MB PSS over 30 minutes.

## [0.51.0] - 2026-07-21

### Added
- First-run welcome hint, replacing the old auto-spawned terminal.
- Real CLI flags: `-c`/`--config`, `-v`/`--version`, `-h`/`--help`; `-s`/`--spawn` for a one-off launch command.
- Default terminal changed to `kitty`.

## [0.50.0] - 2026-07-21

### Added
- Keyboard layout config (`xkb_layout` and friends).
- Touchpad config: tap-to-click, natural scroll, accel, click/scroll method.

### Fixed
- A startup crash on an invalid keymap.

## [0.49.0] - 2026-07-20

### Added
- Workspace overview (`Super+O`): a schematic grid of every workspace, boxes rather than live thumbnails.

## [0.48.0] - 2026-07-20

### Added
- Second tiling algorithm, master/stack, alongside the existing adaptive BSP. `default_layout` config key, runtime switch, and ratio keybinds.

## [0.47.0] - 2026-07-20

### Added
- `[env]` block and `$variable` substitution in config, Hyprland's `$mainMod` idea.

## [0.46.0] - 2026-07-20

### Added
- Multi-file config via `include`.

## [0.45.0] - 2026-07-20

### Added
- Submaps: temporary keybind layers (sway/Hyprland's "mode"), plus a default vim-motion nav submap on `Super+N`.

## [0.44.0] - 2026-07-20

### Added
- `zwlr-gamma-control-manager-v1` for night-light tools (wlsunset, gammastep).

## [0.43.0] - 2026-07-20

### Added
- `wlr-output-power-management-unstable-v1` (DPMS), with a real DRM CRTC power hook on the udev backend.

## [0.42.0] - 2026-07-20

### Added
- Session environment export (`WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`) pushed to systemd/D-Bus at startup.

## [0.41.0] - 2026-07-20

### Added
- `tidectl`, a small CLI over the IPC socket.

## [0.40.0] - 2026-07-20

### Added
- `wlr-output-management-unstable-v1`: live position/transform/scale changes for kanshi, wlr-randr, wdisplays.

## [0.39.0] - 2026-07-20

### Added
- `wp-cursor-shape-v1`, plus a real per-shape xcursor lookup instead of always showing the default arrow.

## [0.38.0] - 2026-07-20

### Fixed
- A startup crash when both `XDG_CONFIG_HOME` and `HOME` are unset.

### Audited
- Distro-portability pass: no hardcoded paths, no unguarded GPU-vendor assumptions.

## [0.37.0] - 2026-07-20

### Added
- `wlr-foreign-toplevel-management-v1`, for waybar's `wlr/taskbar` and ags v1.

## [0.36.0] - 2026-07-20

### Added
- Screencast DBus interface scaffolding (`org.gnome.Mutter.ScreenCast`). PipeWire streaming itself wasn't implemented yet at this point.

## [0.35.0] - 2026-07-20

### Added
- IME support: `zwp-text-input-v3`, `zwp-input-method-v2`, `zwp-virtual-keyboard-v1`.

## [0.34.0] - 2026-07-20

### Changed
- README rewritten to match the structure of other well-known compositor READMEs.

## [0.33.0] - 2026-07-20

### Changed
- Repo hygiene: stopped tracking IDE files, expanded `.gitignore`.

## [0.32.0] - 2026-07-20

### Added
- `zwp-keyboard-shortcuts-inhibit-v1` (VM/remote-desktop key capture) and `zwp-pointer-gestures-v1`.

## [0.31.0] - 2026-07-20

### Added
- `ext-foreign-toplevel-list-v1`: read-only window list for taskbars and switchers.

## [0.30.0] - 2026-07-20

### Added
- Declared MSRV (1.86). Added a `.desktop` session entry for display managers.

### Fixed
- New windows opening on the wrong monitor when spawned via a keybind.

## [0.29.0] - 2026-07-20

### Added
- `ext-session-lock-v1`: real screen-lock support (swaylock, hyprlock).
- `zxdg-decoration-manager-v1` plus KDE's decoration protocol, both enforced server-side.
- Per-app window rules (`[[window_rule]]`): workspace/output/float/pseudo_tile/pin applied on map.
- `xdg-activation-v1`, `wp-single-pixel-buffer-v1`, `wp-presentation-time`, `wp-fractional-scale-v1`.
- `wp-pointer-constraints-v1` + `wp-relative-pointer-v1` for FPS-style mouse look, verified live against real Minecraft.

## [0.28.0] - 2026-07-19

### Removed
- The built-in hotkey-overlay cheat sheet. A first-party UI like that belongs outside a window manager, not inside one.

## [0.27.0] - 2026-07-19

### Added
- `wlr-data-control-unstable-v1` for clipboard managers (cliphist, wl-clip-persist).

## [0.26.0] - 2026-07-19

### Added
- Screenshots: `wlr-screencopy-unstable-v1` and `ext-image-copy-capture-v1`, output and region capture.
- Lid-switch and tablet-mode events can now trigger config actions.
- Window groups/tabs: merge windows into one tile, tab-strip UI, cycle/ungroup.
- Hotkey-overlay cheat sheet (removed again in 0.28.0).

## [0.25.0] - 2026-07-19

### Added
- Real xcursor-theme cursor on the udev backend, replacing the placeholder dot.

## [0.24.0] - 2026-07-19

### Fixed
- Explicit restoration state for fullscreen/maximize/floating/tiling/pinning across every transition: workspace swap, output change, interactive grabs.
- Unbounded memory growth from empty workspace trees on arbitrary workspace IDs.

## [0.23.0] - 2026-07-19

### Fixed
- Explicit XDG popup grab lifecycle: correct pointer/keyboard handoff, dismiss-on-outside-click, deadlock-safe teardown.

## [0.22.0] - 2026-07-19

### Fixed
- Centralized keyboard focus and XDG activation authority, replacing several places that used to set it independently.

## [0.21.0] - 2026-07-19

### Fixed
- Explicit XDG toplevel and layer-shell buffer lifecycle: nothing tiles, renders, or focuses before a real buffer maps.
- Pinned Smithay to niri's known-good revision for a required layer-shell lifecycle fix.

## [0.20.1] - 2026-07-19

### Measured
- First real-hardware idle footprint: ~60MB PSS, ~1% CPU, 0% GPU at idle on AMD. Same machine's Hyprland at idle: ~137MB PSS, ~2.8% CPU, ~16% GPU.

## [0.20.0] - 2026-07-19

### Added
- Closing the focused window now refocuses whatever the pointer is already over.

## [0.19.0] - 2026-07-19

### Added
- Tiled-window resize via `Super`+right-drag.
- Re-enabled tiled-window drag-to-swap after the deadlock fix in 0.15.1/0.16.0, pending a hardware retest.

## [0.18.0] - 2026-07-18

### Added
- Idle-inhibit and idle-notify (`zwp-idle-inhibit-manager-v1`, `ext-idle-notify-v1`), verified live against `hypridle`.

## [0.17.0] - 2026-07-18

### Fixed
- Render-timing hardening on the udev backend: GPU fence waits, empty-frame retry, DMA-BUF readiness blocking.
- Fullscreen state not surviving a floating/tiled transition or a cross-output workspace swap.

## [0.16.0] - 2026-07-18

### Fixed
- Root-caused and fixed the 0.15.1 hardware freeze: a self-deadlock in the tiled drag-to-swap grab. Kept disabled pending a real retest.
- Hardened all four interactive pointer grabs against a client being destroyed mid-drag.
- A crash reachable by a client with two mapped surfaces racing a grab.

## [0.15.1] - 2026-07-18

### Fixed
- Disabled tiled-window drag-to-swap after it froze the entire machine on its first real-hardware test. Pseudo-tiling, shipped the same version, was unaffected and stayed on.

## [0.15.0] - 2026-07-18

### Added
- Interactive tiled-window drag-to-swap (`Super`+left-drag) and pseudo-tiling (`Super+Shift+P`). See 0.15.1: the drag feature was disabled immediately after a hardware freeze.

## [0.14.0] - 2026-07-18

### Added
- Scratchpad, pin (`toggle-pin`), and cross-monitor workspace swap.

## [0.13.0] - 2026-07-18

### Added
- Minimal IPC/control socket, the first version of what `tidectl` now runs on.

## [0.12.0] - 2026-07-18

### Added
- `spawn_at_startup` config list and per-output config (`[[output]]`: resolution, position, scale, transform).

## [0.11.0] - 2026-07-18

### Added
- udev/DRM backend verified on real hardware for the first time (AMD): modeset, input, and VT switching all working.
- Focus-follows-mouse.

## [0.10.0] - 2026-07-17

### Added
- Fullscreen and maximize.
- Workspaces: one independent tiling tree per output, `Super+1..9,0` to switch.

## [0.9.0] - 2026-07-17

### Added
- XWayland support via `xwayland-satellite`.

## [0.8.0] - 2026-07-17

### Added
- Multi-monitor tiling: one tiling tree per output. Runtime output hotplug on the udev backend.

## [0.7.0] - 2026-07-17

### Added
- wlr-layer-shell support: bars, launchers, lock screens.

## [0.6.0] - 2026-07-17

### Added
- Directional focus/swap (`Super+hjkl`), split-ratio drag-resize.

## [0.5.0] - 2026-07-17

### Added
- udev/DRM backend: standalone TTY session, no host compositor required. The first real backend beyond the winit dev scaffold.

## [0.4.5] - 2026-07-17

### Added
- Release profile tuning cut the binary from 10.9MB to 6.6MB.

### Fixed
- A bad memory measurement (debug build plus raw RSS instead of release plus PSS) that made TideWM look far heavier than it actually is.

## [0.4.4] - 2026-07-17

### Fixed
- Floating windows falling behind the tiled layer on every retile.

## [0.4.3] - 2026-07-17

### Added
- Compositor-level `Super`+drag to move/resize floating windows.
- `Super+Tab` focus cycling.

## [0.4.2] - 2026-07-17

### Fixed
- Idle CPU pinned near 99% from an unthrottled redraw loop. Dropped to ~2%.

## [0.4.1] - 2026-07-17

### Added
- Floating-window toggle (`Super+V`). Windows now auto-focus on map.

## [0.4.0] - 2026-07-17

### Added
- Dynamic dwindle-style tiling layout engine.

## [0.3.1] - 2026-07-17

### Fixed
- The redraw loop compositing every frame unconditionally, even fully idle.

## [0.3.0] - 2026-07-17

### Added
- Config hot-reload and the first on-screen toast notification.

## [0.2.0] - 2026-07-17

### Added
- TOML config system and compositor-level keybinds.

## [0.1.0] - 2026-07-17

Initial scaffold. No water yet, this is the plumbing.

### Added
- Winit backend, xdg-shell support, basic move/resize grabs, adapted from Smithay's `smallvil` example.
