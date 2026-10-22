# Changelog

What changed in TideWM, in plain words, newest first. Releases are grouped by the stretch of work they came from. Every release in full technical detail lives in [DEV_CHANGELOG.md](DEV_CHANGELOG.md).

## 0.90.180 · 2026-10-22 — No half-visible floaters after a drop

- With `float_anchor = strip`, dropping a floating window across two monitors slides it fully onto the monitor it belongs to instead of leaving half of it cut off.

## 0.90.179 · 2026-10-22 — Dragging floaters between monitors

- With `float_anchor = strip`, a floating window you drag onto the other monitor stays visible there during the drag instead of vanishing until you let go.

## 0.90.178 · 2026-10-22 — Alt+Tab finds floating windows

- With `float_anchor = strip`, switching to a floating window that is off-screen (Alt+Tab, keyboard focus, notifications) scrolls the strip to it.

## 0.90.177 · 2026-10-22 — No more lost floating windows

- With `float_anchor = strip`, closing the apps around a floating window could leave it off-screen where the strip can no longer scroll. It now comes back onto the screen. `scroll { float_rescue = false }` keeps the old behavior.

## 0.90.176 · 2026-10-22 — Floating windows stay on their monitor

- With `float_anchor = strip`, a floating window could jump to the other monitor after the strip scrolled it toward that side. It now stays on its own monitor until you drag it to another one.

## 0.90.175 · 2026-10-22 — Less idle work

- Moving the mouse with focus-follows-mouse does less hit-testing per motion.
- Wave event handlers, float physics and currents skip their per-event or per-frame work when there is nothing for them to do.

## 0.90.174 · 2026-10-22 — Column top and bottom

- `focus-window-top` and `focus-window-bottom` jump to the first or last window of a stacked Scroll column.

## 0.90.173 · 2026-10-22 — Monitor under the mouse

- New `tidectl pointer-output` (IPC `pointer-output`): the monitor under the mouse, even with nothing focused, so launchers and shell popups can open where you are.

## 0.90.172 · 2026-10-22 — Wrap-around column focus

- `focus-column-right-or-first` and `focus-column-left-or-last` move focus along the strip and wrap at the ends.

## 0.90.171 · 2026-10-22 — Preset widths backwards

- `switch-preset-width-back` steps through `preset_widths` the other way.

## 0.90.170 · 2026-10-22 — Exact column widths

- `set-column-width:<v>` sets a Scroll column to `50%`, `800` pixels, or grows/shrinks it with `+10%`, `-10%`, `+100`.

## 0.90.169 · 2026-10-22 — Floating windows that stay on the strip

- New `scroll { float_anchor = strip }`: floating windows stay where you left them on the strip and scroll away with it instead of following the camera. The default `screen` keeps the old behavior.
- `toggle-float-anchor` switches between the two without editing config.

## 0.90.168 · 2026-10-22 — Scroll fixes

- Switching workspaces in Scroll plays your workspace animation again.
- `scroll-changed` events wait until a drag or swipe ends instead of firing on every mouse movement.

## 0.90.167 · 2026-10-22 — Tabbed columns and drag-to-insert

- `toggle-column-tabbed` turns a Scroll column into tabs and back.
- Dragging a tile in Scroll can drop it as a new column or into another column, not just swap, with a highlight showing where it lands.
- Tab bars line up with their window on scaled monitors (they drifted left at 125%).

## 0.90.166 · 2026-10-22 — More IPC

- `tidectl windows` reports each window's pid, position and size, and its Scroll column and row.
- New `tidectl focused-output` and `tidectl layers`.
- `tidectl subscribe` gains `output`, `mode`, `engine` and `scroll` events, so bars and scripts never need to poll for them.

## 0.90.165 · 2026-10-22 — Scroll mouse resize and column actions

- Mod + right-drag resizes a Scroll column, and an app's height in a shared column.
- niri's column actions: `switch-preset-width` (with `preset_widths`), `maximize-column`, `expand-column`, `center-column`, `focus-column-first/last` and `move-column-first/last`.
- `center_focused` can be `on-overflow`, and `always_center_single` keeps a lone column centered.

## 0.90.164 · 2026-10-22 — Stacked Scroll columns

- Scroll columns can hold several apps stacked on top of each other, niri-style. `consume-or-expel-left/right`, `consume-into-column` and `expel-from-column` build and split them.
- Left/right moves between columns and lands on the app you last used there; up/down moves inside a column. `swap-*` and `resize-*` work on columns along the strip and on apps inside a column across it.
- New apps open after the focused column instead of splitting it.

## 0.90.163 · 2026-10-22 — Config cleanup

- Old pre-Wave spellings are gone: `spatial_engine`/`wm_mode` (use `engine`), `pointer_modifier`/`mouse_modifier` (use `drag_modifier`), `submap:` (use `mode:`), `float_physics enabled` (use `tier`), `0x` colors and space-separated lists (use `[a, b]`). A config still using one gets an "unknown key" warning and keeps the default.
- `tidectl active-submap` and `tidectl submap` are now `active-mode` and `mode`.

## 0.90.157 – 0.90.162 · 2026-10-22 — Scroll

- **New Scroll engine.** Each monitor gets its own scrolling strip of apps, niri-style, next to Classic and Ocean. Switch live with `tidectl engine scroll|classic|ocean`, no relog. See the [Scroll guide](TECHNICAL_REPORT.md#scroll-engine).
- App width, neighbor peeks, centering and gaps are all config values (`tile_proportion`, `center_focused`, `gaps`), and `resize-*` actions change the focused app's width live.
- Fullscreen apps move with their column instead of covering the neighbors.
- Moving the mouse focuses what's under it without scrolling the strip, so crossing monitors or clicking a peek never slides things away.
- Clicks land exactly under the cursor in GTK apps like Firefox (their shadow margin is accounted for).
- `tidectl focus-window` / `focus-app` reveal a window wherever it is, and notification tools can find the window that sent a notification (`tidectl resolve-window --pid`).

## 0.90.149 – 0.90.156 · 2026-10-21 — Sessions and portals

- Quick relogs no longer stall for ~40 seconds: portals stop with the session and start in the right order, on any init system.
- Session environment reaches portals and autostarted apps before they need it.
- Live per-window capture for tools that use `hyprland-toplevel-export-v1` (live window previews in switchers and docks).
- Keyboard layouts readable and switchable over IPC.
- Switch workspaces on a named monitor from IPC or binds.
- Apps that keep re-asking for their decoration mode are no longer sent a fresh configure every time.

## 0.90.144 – 0.90.148 · 2026-10-13 — Native performance

- Hardware cursor planes, with automatic software fallback (`hardware_cursor = false` to force it).
- Explicit sync (`linux-drm-syncobj-v1`) when the kernel and driver support it.
- Better DMA-BUF feedback, so fullscreen apps and games can be scanned out directly.
- Drag tiled windows between monitors in Classic and Ocean.
- `tidectl perf --json` shows per-monitor render timings and scanout decisions.

## 0.90.118 – 0.90.143 · 2026-10-06 to 2026-10-07 — NVIDIA pass, packages and popups

- Real-hardware NVIDIA fixes: floating windows open at their natural size, keyboard resizes no longer get lost, captures and glass use the monitor's real scale, new windows no longer flash on the wrong monitor.
- **Distro packages:** Gentoo overlay, Arch `PKGBUILD`, NixOS flake and module.
- Coming back from a TTY no longer leaves a black or frozen screen.
- `env { }` and the cursor theme reload live.
- TideWM's own popups got style options: `outline`, `underline`, a Hyprland-like slide, adjustable duration and size.
- Video wallpapers pause while fully covered, and their frames no longer reset tiled drags.
- Ocean can keep each monitor's windows separate (`shared_canvas` to turn the shared world back on).
- Clients no longer crash when a monitor disconnects.
- The welcome card shows your real binds and config path.

## 0.90.1 – 0.90.117 · 2026-08-07 to 2026-10-06 — Wave, shaders and a full audit

- **Wave**, the new config format: data on the surface, Lua underneath, hot-reloaded and transactional (a broken edit never half-applies). See [WAVE.md](WAVE.md).
- **Custom shaders:** your own GLSL effects on a window's backdrop, up to four chained passes, per window or workspace.
- Liquid frost with real optical depth, glass on tiled windows, ambient caustics, animated water refraction.
- Ocean currents, weighted buoyancy and two tiers of floating-window physics, all opt-in.
- Cascade pours new windows in and drains closing ones out.
- Five non-water motion presets (`animations.preset`) for people who want smooth but not wet.
- Classic edge/corner snapping and a `floating` layout.
- Ocean freeform canvas: detach any window and place it anywhere, with a compass for off-screen windows and a whole-world minimap.
- Live engine switching: changing the engine in config migrates every open window in place.
- Many more window/layer/workspace rules: tags, `persistent_size`, `scroll_factor`, `on_workspace`, `pin_output`, layer blur and more.
- Multi-GPU client offload, adaptive sync (VRR) and per-device input settings.
- Drawing tablets, tearing control and `ext-workspace-v1` (workspace widgets in bars).
- `tidectl doctor`, `tidectl report` and `tidectl perf` for quick bug triage.
- Drag and drop between apps works.
- A full repository audit closed every critical and high finding: lock-screen leaks, compositor freezes from bad clients, crashes on hotplug, unbounded memory growth and more.

## 0.90.0 · 2026-08-01 — The water desktop

- **Ocean**, a zoomable 2D canvas with per-monitor cameras, as an alternative to numbered workspaces.
- The full water look: ripples on open, wave workspace transitions, water and frost glass, shadows, rounded corners, animated gradient borders, viscous drags, floating sway, depth and buoyancy.
- Classic Depth Deck: park a tile out of the way and swap it back in.
- Cascade, a third tiling layout.
- Swim between workspaces with a touchpad swipe.
- An IPC event stream (`tidectl subscribe`).

## 0.60.0 · 2026-07-24 — Screen sharing

- Screen sharing to OBS and Discord through TideWM's own portal, with a source picker for monitors and windows.
- Touchpad gestures can run any action.
- Per-window opacity, named scratchpads, per-workspace gaps and window swallowing.

## 0.29.0 – 0.58.0 · 2026-07-20 to 2026-07-22 — Daily-driver protocols

- Screen lock, idle, clipboard managers, screenshots, taskbars, output management (kanshi, wlr-randr), night light, DPMS, IME and screen-reader support.
- `tidectl`, multi-file config, keybind modes, master/stack layout and the workspace overview (`Super+O`).
- Window rules, keyboard and touchpad settings, first-run welcome hint.

## 0.1.0 – 0.28.0 · 2026-07-17 to 2026-07-19 — Foundations

- From a nested window to a real standalone session: tiling, floating, fullscreen, workspaces, multi-monitor and hotplug, layer shell, XWayland, config hot reload and keybinds.
- First real-hardware runs on AMD at about 60 MB of memory and 1% CPU at idle.
