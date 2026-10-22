# TideWM — Technical Report

The quick technical picture: what TideWM is built from, what works, what's been tested on real hardware, and how to build it. For the pitch, see [README.md](README.md). For every config key, action and IPC request, see [DOCUMENTATION.md](DOCUMENTATION.md).

## What it is

A Wayland compositor written in Rust on [Smithay](https://github.com/Smithay/smithay). Underneath is a complete tiling window manager: BSP, master/stack and cascade layouts, workspaces, multi-monitor, layer shell, IPC and XWayland. On top sits the water look (ripples, waves, glass, shadows, physics), and every piece of it is a config toggle.

Current version: **0.90.168**, beta. 1.0 waits for a wider real-hardware pass (Intel, hybrid laptops, more distros).

## Architecture

- **Two backends.** `winit` runs TideWM nested inside your current desktop (safe to try, the main dev loop). `udev`/DRM is the real standalone session you log into.
- **Three spatial engines**, switchable live with `tidectl engine` or a config reload, carrying every open window across:
  - **Classic:** numbered workspaces per monitor, with the Depth Deck for parking tiles.
  - **Ocean:** one zoomable 2D canvas with a camera per monitor.
  - **Scroll:** a scrolling strip of apps per monitor, niri-style.

  How they relate: [SPATIAL_MODEL.md](SPATIAL_MODEL.md). Scroll is covered [below](#scroll-engine).
- **XWayland** through [`xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite), so X11 apps arrive as ordinary Wayland windows.
- **One render pipeline.** Glass, shadows, rounding, borders and animations are drawn once per frame from a shared window-placement snapshot. The same path feeds both backends, screenshots, screen sharing and workspace transitions, so an effect never needs a second implementation.
- **Config:** [Wave](WAVE.md), data on the surface and Lua underneath. It's hot-reloaded, and a broken edit never half-applies.
- **Memory:** about 50 MB PSS for plain tiling and 60–70 MB with the full water stack on AMD. About 100 MB on NVIDIA, a third of which is the driver's own mappings. 2 GB is a hard ceiling for feature growth, not a target.

## Scroll engine

Scroll gives every monitor its own strip of apps that you slide along, niri-style. New apps join the strip instead of shrinking the ones already there, and an app never ends up half on one monitor and half on another. Groups, glass, shadows, animations, screenshots and screen sharing work exactly as in Classic and Ocean.

### Switching

```sh
tidectl engine scroll
tidectl engine classic   # or ocean, to go back
```

This switches the running session without touching your config. To make Scroll your default, set `engine = scroll` in `config.wave`. You can also bind it, for example `bind $mod+Ctrl+S { engine:scroll }`. A switch is refused while the screen is locked, while a launcher or other exclusive-keyboard layer is open, or during a drag.

### Configuration

Every key is optional, reloads live and is measured against the monitor's usable area. These are the defaults:

```wave
scroll {
    workspaces           = separate               # or sections
    orientation          = horizontal             # or vertical
    tile_proportion      = 0.5                    # 0.1 – 1.0 of the viewport
    center_focused       = never                  # always, on-overflow
    always_center_single = false
    preset_widths        = [0.333, 0.5, 0.667]
    gesture_distance     = 1200
    float_anchor         = screen                 # or strip
    float_rescue         = true
}
```

- **`tile_proportion`** is how much of the screen a new app takes:
  - `1.0` fills the screen;
  - `0.94` leaves a narrow peek of the neighbors;
  - `0.5` fits two apps side by side.

  Apps you resized by hand keep their own size.
- **`center_focused`:**
  - `always` keeps the focused column in the middle, even the first, last or only one;
  - `on-overflow` centers a column only when it isn't already fully on screen;
  - `never` moves the strip only as far as needed.

  `true` and `false` mean `always` and `never`.
- **`always_center_single`:** a workspace with just one column keeps it centered.
- **`preset_widths`:** the widths `switch-preset-width` steps through.
- **`orientation`:** horizontal or vertical strips. For a free 2D layout, use Ocean.
- **`workspaces`:** `separate` gives each workspace its own strip and scroll position. `sections` lays a monitor's workspaces end to end in one strip, and switching workspace jumps to its section. In both modes, scratchpads stay separate, and floating windows float above the strip or stay on it (see `float_anchor`).
- **`gesture_distance`:** how far a touchpad swipe travels per screen. The swipe uses your workspace-swipe finger count.
- **`float_anchor`:** `screen` keeps floating windows where they are on the monitor while the strip scrolls. `strip` leaves each one at its spot on the strip, so it scrolls away with the columns and comes back when you return. Pinned windows and scratchpads always stay on screen. `toggle-float-anchor` flips it until the next reload.
- **`float_rescue`:** with `float_anchor = strip`, a floating window the strip can no longer scroll to (because the apps around it closed) comes back onto the screen. Set `false` to leave it off-screen.
- **Spacing** comes from the global `gaps`.

A nearly full-width setup with neighbor peeks:

```wave
scroll {
    tile_proportion = 0.94
    center_focused  = always
}
gaps = 8
```

### Moving around

Scroll uses your existing binds. It adds none of its own.

A column holds one app or several stacked ones. The table is for horizontal strips; on vertical strips the two directions trade places.

| Action | In Scroll |
| --- | --- |
| `focus-left/right` | Focus the next column, on the app you last used there, and slide it into view |
| `focus-up/down` | Focus the app above or below in the same column |
| `swap-left/right` | Move the whole column along the strip |
| `swap-up/down` | Move the app up or down inside its column |
| `resize-left/right` | Make the column narrower or wider |
| `resize-up/down` | Make the app shorter or taller inside a shared column |
| `consume-or-expel-left/right` | Alone: join the column on that side. Sharing a column: leave it for a new column on that side |
| `consume-into-column` | Pull the next column's first app into this column |
| `expel-from-column` | Move the app out into its own column, right after |
| `switch-preset-width` | Step the column through `preset_widths` |
| `switch-preset-width-back` | The same, the other way |
| `set-column-width:<v>` | `50%`, `800` (pixels), or `+10%`/`-10%`/`+100` from the current width |
| `maximize-column` | Full width, and back to the old width |
| `expand-column` | Fill the space the other fully visible columns leave |
| `center-column` | Center the column once, even at the ends of the strip |
| `focus-column-first/last` | Jump to the first or last column |
| `focus-column-right-or-first` / `focus-column-left-or-last` | Next or previous column, wrapping at the ends |
| `focus-window-top/bottom` | Jump to the first or last app in the column |
| `move-column-first/last` | Move the column to the start or end |
| `toggle-column-tabbed` | Turn the column into tabs (one app shown, a tab bar on top), and back |
| `scroll-left/right/up/down` | Slide the strip half a screen |
| `toggle-float-anchor` | Floating windows follow the screen or stay on the strip, until the next reload |
| Mod + right-drag | Drag sideways to resize the column, up/down to resize the app in a shared column |
| Mod + left-drag | Drop near a tile's left or right edge for a new column there, near its top or bottom to join that column, or in the middle to swap. A highlight shows where it lands |
| Touchpad swipe | Slide the strip with your fingers |

New apps open as their own column right after the focused column, never in the middle of it. Closing the top app of a column keeps the rest of the column and its width.

Focusing with the mouse never moves the strip: hovering with `focus_follows_mouse`, clicking, touch and tablet pens all leave it alone. You can cross to another monitor or click a neighbor's peek without anything sliding away. Keyboard focus, `tidectl focus-window` / `focus-app` and notification clicks do bring the app into view. When the strip slides under a still mouse, the app now under the pointer gets the wheel and clicks right away.

Switching between Classic and Scroll keeps your layout trees intact. Switching to or from Ocean converts the layout, and Ocean's camera history and bookmarks don't carry over.

For tools: each `tidectl outputs` entry has a `scroll` object (`offset`, `orientation`, `workspaces`), each `windows` entry says which column and row it's in, and `tidectl subscribe scroll` streams the strip's columns whenever they move or change. The full key reference is in [DOCUMENTATION.md](DOCUMENTATION.md#scroll-spatial-engine).

## Feature status

| Area | Status |
| --- | --- |
| Tiling: BSP, master/stack, cascade, floating layout, snapping | Done |
| Workspaces, scratchpads, pinning, window groups (tabs), fullscreen, maximize | Done |
| Classic, Ocean and Scroll engines, live switching | Done |
| Multi-monitor, hotplug, mixed scale, dragging tiles between monitors | Done |
| Layer shell (bars, launchers, wallpapers, lock screens) | Done |
| XWayland | Done, via xwayland-satellite |
| Screenshots, per-window capture, clipboard, session lock | Done |
| Screen sharing (OBS, Discord) through TideWM's own portal | Done with `--features screencast` |
| IPC and `tidectl`: windows with geometry and Scroll position, outputs, layers, an event stream for windows, workspaces, focus, monitors, modes, engine and Scroll, plus `doctor`, `report`, `perf` | Done |
| Water stack: ripples, wave transitions, water/frost glass, shadows, gradient borders, viscosity, sway, buoyancy, caustics, currents | Done |
| Custom GLSL shaders (up to four chained passes) | Done |
| Floating-window ocean physics, Ocean compass and minimap | Done; tested nested only |
| Hardware cursors, explicit sync, direct scanout feedback, VRR | Done |
| Multi-GPU client offload | Done; not yet tested on a hybrid laptop |
| AUR package | Not yet |

## Hardware

| Hardware | Status |
| --- | --- |
| **AMD** | Main development hardware. Standalone session, the whole water stack, touchpad gestures and Ocean are verified here. |
| **NVIDIA** | Daily driver on an RTX 3060 (proprietary driver, open kernel modules) with two monitors at mixed scale (1.0 and 1.25). Glass, shaders, screen sharing, XWayland, games and all three engines work. Idle: about 0.5% of one CPU core and 69 MB of compositor VRAM. |
| **Intel** | Untested. Reports welcome. |
| **Hybrid AMD + NVIDIA laptops** | GPU selection and offload are implemented and unit-tested, but not yet run end to end on a laptop. |

**Distros:** Arch and Gentoo are fully supported and tested. On Gentoo, both systemd and OpenRC work. Fedora, Debian/Ubuntu, openSUSE and NixOS have build instructions or packages and should work, but they get less testing. TideWM needs no particular init system: libseat and libudev are enough.

## Building

**Rust:** stable, 1.86 or newer.

| Library | Notes |
| --- | --- |
| `pkg-config` | Build only |
| `wayland` | Client, server and EGL libraries |
| `libudev` | `systemd-libs` on systemd distros, `eudev` or `libudev-zero` elsewhere |
| `libinput`, `libxkbcommon`, `libdrm` | |
| `mesa` | `libEGL`, `libgbm` |
| `libseat` | `seatd`, or systemd-logind |
| `clang` / `libclang` | Build only, and only with `--features screencast` (`install.sh` always enables it) |

```bash
# Arch (add clang for screencast)
sudo pacman -S pkg-config wayland systemd-libs libinput libxkbcommon mesa libdrm seatd

# Gentoo, systemd or OpenRC (skip sys-fs/eudev if systemd is installed; add llvm-core/clang for screencast)
sudo emerge --ask dev-util/pkgconf dev-libs/wayland dev-libs/libinput \
    x11-libs/libxkbcommon media-libs/mesa x11-libs/libdrm sys-auth/seatd sys-fs/eudev

# Fedora (add clang for screencast)
sudo dnf install pkgconf-pkg-config wayland-devel systemd-devel libinput-devel \
    libxkbcommon-devel mesa-libEGL-devel mesa-libgbm-devel libdrm-devel libseat-devel

# Debian / Ubuntu (add libclang-dev for screencast)
sudo apt install pkg-config libwayland-dev libudev-dev libinput-dev \
    libxkbcommon-dev libegl1-mesa-dev libgbm-dev libdrm-dev libseat-dev

# openSUSE (add clang for screencast)
sudo zypper install pkgconf-pkg-config wayland-devel libudev-devel libinput-devel \
    libxkbcommon-devel Mesa-libEGL-devel Mesa-libgbm-devel libdrm-devel libseat-devel
```

If a package name is wrong on your distro, the library list above is what matters, and an [issue](https://github.com/Fi3w0/TideWM/issues/new/choose) helps everyone.

Optional: [`xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite) for X11 apps (`xwayland.enabled = false` turns it off).

```bash
git clone https://github.com/Fi3w0/TideWM.git
cd TideWM
cargo build --release --locked --features screencast
```

Or skip all of this and install a real package: [packaging/README.md](packaging/README.md).

### Running

```bash
cargo run --locked --features screencast
```

TideWM picks the backend itself: nested when `WAYLAND_DISPLAY` or `DISPLAY` is set, standalone otherwise.

To log in from a display manager (SDDM, GDM, greetd), run `./install.sh`. It installs the `TideWM` binary (capitalized: the session file expects exactly that name), the session entry and its icon. Run it again after pulling to update.

### Screen sharing

Discord, OBS and other screen-sharing apps go through `xdg-desktop-portal`. TideWM ships its own ScreenCast backend, so you don't need the GNOME portal stack:

```bash
cargo build --release --locked --features screencast
sudo cp share/xdg-desktop-portal/tidewm.portal /usr/share/xdg-desktop-portal/portals/
sudo cp share/xdg-desktop-portal/tidewm-portals.conf /usr/share/xdg-desktop-portal/
```

Everything except screen sharing (file chooser, settings, printing) goes to `xdg-desktop-portal-gtk`, so install it too. Log in through TideWM's own session entry so the portal sees `XDG_CURRENT_DESKTOP=tidewm`. Frames travel over shared memory; DMA-BUF export is off.

## Roadmap

- **Wider hardware coverage:** Intel GPUs and an end-to-end hybrid-laptop run.
- **Scroll:** feel-tuning of the new column features on real hardware.
- **Physics and Ocean extras on real hardware:** floating-window physics, the compass and the minimap are tested nested only.
- **Feel tuning:** viscosity, sway, depth timing and transition presets get refined against real use.
- **Lower VRAM:** measure large glass windows before considering a shared blur buffer per monitor.
- **Design-pending ideas:** tide contexts and workspace depth-moods, which need a design conversation first.
- **AUR package.**

## Docs map

- [README.md](README.md): the pitch and quick start
- [DOCUMENTATION.md](DOCUMENTATION.md): every config key, action and IPC request
- [WAVE.md](WAVE.md): the config format
- [SPATIAL_MODEL.md](SPATIAL_MODEL.md): Classic, Ocean and Scroll
- [Scroll engine](#scroll-engine): the Scroll guide, in this file
- [CHANGELOG.md](CHANGELOG.md): what changed, in short; [DEV_CHANGELOG.md](DEV_CHANGELOG.md) has the full detail
- [packaging/README.md](packaging/README.md): distro packages
- [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), [LICENSING.md](LICENSING.md)
