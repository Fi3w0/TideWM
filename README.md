<div align="center">

<img src="share/icons/TideWM-logo-faithful-4k.png" width="180" alt="TideWM logo">

# TideWM

A water-styled Wayland compositor, built in Rust on Smithay.

![License](https://img.shields.io/badge/license-GPL--3.0-blue?style=for-the-badge)
![Language](https://img.shields.io/badge/rust-%23000000.svg?style=for-the-badge&logo=rust&logoColor=white)
![Status](https://img.shields.io/badge/status-functional-brightgreen?style=for-the-badge)
![MSRV](https://img.shields.io/badge/rustc-1.86+-000?style=for-the-badge&logo=rust&logoColor=white)
[![Discord](https://img.shields.io/badge/Discord-Join-5865F2?style=for-the-badge&logo=discord&logoColor=white)](https://discord.gg/ZhkxA83cKk)

**[<kbd> <br> Docs <br> </kbd>](DOCUMENTATION.md)**
**[<kbd> <br> Quick&nbsp;Start <br> </kbd>](#quick-start)**
**[<kbd> <br> Building <br> </kbd>](#building)**
**[<kbd> <br> Config <br> </kbd>](#configuration)**
**[<kbd> <br> Contributing <br> </kbd>](CONTRIBUTING.md)**

</div>

> [!NOTE]
> TideWM already works as a real Wayland compositor: tiling, multi-monitor, workspaces, IPC, and most of the protocol surface a daily driver needs are all in. What's missing is the actual point of the project, the water/aqua render effects, and they haven't been started yet. Screen sharing (PipeWire/xdg-desktop-portal) now delivers real frames under the nested backend, but hasn't been tested yet on a standalone TTY session or through a real `xdg-desktop-portal`-mediated client like OBS or Discord; treat it as promising, not daily-driver-proven. This is exactly the stage where testing on hardware I don't own is most useful, jump into [Discord](https://discord.gg/ZhkxA83cKk) if something breaks.

Full modern tiling on the fundamentals: BSP and master/stack, workspaces, groups, multi-monitor, layer-shell, IPC, with a water identity layered on top once the render work starts. Built for low-end hardware first, 1.5GB is the target ceiling in normal use, 3GB is the line where it gets actively optimized.

## About

TideWM is a solo project. I use AI coding agents (OpenCode, Codex, Claude Code) to test and implement ideas quickly, then verify the result myself on real hardware before trusting it. Claude Sonnet 5.0 (xhigh) has done most of the work; GPT-5.2-Sol (xhigh) is the second most used model, with Kimi 3.0 and GLM 5.2 picking up smaller or parallel tasks. Every protocol/backend claim in the docs is marked with its actual verification status rather than "should work".

## Features

- Dynamic BSP tiling (dwindle-style) plus master/stack, switchable per workspace
- Workspaces per output, scratchpad, per-window pinning
- Window groups: tab several windows into one tile, first-party tab-strip UI
- Floating, fullscreen, maximize, pseudo-tiling
- Multi-monitor with hotplug, independent tiling tree per output, mixed-DPI
- Layer-shell: bars, launchers, lock screens
- XWayland, via [`xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite)
- Screenshots, clipboard, session lock
- PipeWire screencasting behind a feature flag, delivers real frames now, not yet proven through a real portal-mediated client (OBS/Discord)
- A low-memory built-in Tide wallpaper; layer-shell wallpaper tools replace it normally
- Hot-reloadable config in Waves, TideWM's own format, split across files, `env`/`$variables`/`$wave(...)`
- Per-app window rules, including regex matching, initial fullscreen/maximize, and capture privacy
- Submaps: temporary keybind layers (sway/Hyprland's "mode")
- Workspace overview (`Super+O`)
- Keyboard layout and touchpad (libinput) config
- JSON IPC socket, plus a `tidectl` CLI over it
- Server-side decorations enforced
- First-boot hint on an empty desktop, gone once you open a window or edit config
- Persistent, workspace-reserving config-error panel plus the existing reload/debug toasts

Full config reference, every action string, and the protocol matrix: [DOCUMENTATION.md](DOCUMENTATION.md).

## Status

- **Multi-monitor**: yes, core to the design from the start. Mixed-DPI via `wp-fractional-scale-v1`.
- **XWayland**: yes, via xwayland-satellite. X11 apps tile like any other window.
- **Nested (dev/testing)**: yes, `cargo run` opens TideWM as a window inside any existing session.
- **Real hardware**: verified on AMD, backend, tiling, pointer-constraints (tested against real Minecraft), interactive drag/resize.
- **Nvidia**: overlay-plane workaround is written, not yet run on real Nvidia hardware.
- **DPMS / gamma**: protocol-complete and verified on AMD hardware; lid/tablet switches still need broader hardware coverage.
- **Touchpad config**: built (tap, natural-scroll, accel, click-method, ...), not yet verified on real hardware.
- **Touchpad gestures**: compositor workspace swipes are built; broader real-device coverage is pending.
- **Screencasting**: built behind `--features screencast` with a real `xdg-desktop-portal` backend. The SHM/MemFd path delivers real frames now under the nested (winit) backend, verified with a direct PipeWire consumer (`glxgears` content, correctly oriented, live-updating, PSS flat over a sustained stream). Not yet verified on a standalone TTY (udev/DRM) session, and DMA-BUF still fails on real hardware and stays disabled. Not yet tested end to end through a real portal-mediated client (OBS/Discord) -- promising, not proven for daily use.
- **AUR package**: not yet, build from source below.

## Roadmap

Foundation before visuals has been the plan from the start. The WM itself, tiling, multi-monitor, workspaces, layer-shell, IPC, XWayland, is done, and it already runs as a daily compositor on AMD hardware. What's left:

- **Water/aqua render effects.** Ripples, wave-based workspace transitions, liquid window drag. This is the reason TideWM exists and it hasn't been started; render work was deliberately held off until the WM foundation was solid.
- **PipeWire screen sharing.** Frame delivery works now; not yet proven through a real portal-mediated client. Not something to depend on for Discord/OBS yet, but closer than it was.
- **Nvidia hardware.** The overlay-plane workaround exists in code but has never run on a real Nvidia GPU.
- **AUR package.** Not yet, build from source for now.

## Quick Start

`Super` is the default modifier. Default terminal is `kitty`; override in config.

| Shortcut                    | Action                               |
| ---------------------------- | ------------------------------------ |
| `Super+Enter`               | Spawn terminal                       |
| `Super+Q`                   | Close window                         |
| `Super+Shift+Q`             | Quit TideWM                          |
| `Super+1`..`Super+9,0`      | Switch workspace (1..10)             |
| `Super+Shift+<N>`           | Move window to workspace N           |
| `Super+H/J/K/L`             | Focus left/down/up/right             |
| `Super+Shift+H/J/K/L`       | Swap with neighbor in direction      |
| `Super+V`                   | Toggle floating                      |
| `Super+F`                   | Toggle fullscreen                    |
| `Super+Shift+P`             | Toggle pseudo-tile                   |
| `Super+P`                   | Toggle pin (floats above workspaces) |
| `Super+Tab`                 | Cycle focus                          |
| `Super+Minus`               | Toggle scratchpad                    |
| `Super+Ctrl+H/J/K/L`        | Group window with neighbor           |
| `Super+[/]`                 | Cycle tab in a group                 |
| `Super+W` / `Super+Shift+W` | Layout: BSP / master-stack           |
| `Super+O`                   | Toggle workspace overview            |

Mouse:

| Input                        | Action                                |
| ---------------------------- | -------------------------------------- |
| `Super` + left-drag          | Move floating window                  |
| `Super` + right-drag         | Resize floating window                |
| Left-drag a floating window's own edge | Resize it, no modifier needed |
| `Super` + left-drag (tiled)  | Pick up and drop into a new tile slot |
| `Super` + right-drag (tiled) | Resize the tile from an edge          |
| Click on a split gap         | Drag to adjust the split ratio        |

Full set, plus every action string and IPC/`tidectl` command, in [DOCUMENTATION.md](DOCUMENTATION.md). Rebind anything with `bind` in `config.wave`.

## Building

**Rust toolchain:** stable, 1.86+.

**Native dependencies:**

| Library        | Notes                                                                                         |
| -------------- | --------------------------------------------------------------------------------------------- |
| `pkg-config`   | Build-time only                                                                               |
| `wayland`      | `libwayland-client`/`-server`/`-egl`                                                          |
| `libudev`      | `systemd-libs` on systemd distros, `eudev`/`libudev-zero` elsewhere. Doesn't require systemd. |
| `libinput`     |                                                                                               |
| `libxkbcommon` |                                                                                               |
| `mesa`         | `libEGL`, `libgbm`                                                                            |
| `libdrm`       |                                                                                               |
| `libseat`      | The `seatd` package on most distros; also works against systemd-logind.                       |

```bash
sudo pacman -S pkg-config wayland systemd-libs libinput libxkbcommon mesa libdrm seatd  # Arch
```

Optional: [`xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite) for X11 apps (`xwayland.enabled = false` to skip).

```bash
git clone https://github.com/Fi3w0/TideWM.git
cd TideWM
cargo build --release --locked
```

### Running

```bash
cargo run --locked
```

Backend auto-selects: nested (winit) when `WAYLAND_DISPLAY`/`DISPLAY` is set, standalone TTY/DRM (udev) otherwise. Switch to a free VT with neither set to try it standalone.

To launch from a display manager (GDM, SDDM, greetd): put the `TideWM` binary (note the case, `[[bin]] name` in `Cargo.toml` builds it capitalized, and `tidewm.desktop`'s own `Exec=` line expects exactly that) on `PATH`, copy `share/wayland-sessions/tidewm.desktop` into `/usr/share/wayland-sessions/`, and copy `share/icons/TideWM-logo-faithful-4k.png` to `/usr/share/pixmaps/tidewm.png` for the session picker's icon.

### Screen sharing (works, not yet proven through a real portal client)

Discord, OBS, and anything else that shares your screen go through `xdg-desktop-portal`, not TideWM directly. TideWM implements its own `org.freedesktop.impl.portal.ScreenCast` backend so no `xdg-desktop-portal-gnome`/GTK4 chain is needed. The PipeWire buffer path (SHM/MemFd) delivers real frames under the nested backend; it hasn't yet been run on a standalone TTY session or end to end through a real `xdg-desktop-portal` process with OBS or Discord, so treat it as testing-ready rather than daily-use-proven. DMA-BUF export stays disabled either way.

Build with `cargo build --release --locked --features screencast`, then install the portal files so `xdg-desktop-portal` knows to route screen-share requests to TideWM:

```bash
sudo cp share/xdg-desktop-portal/tidewm.portal /usr/share/xdg-desktop-portal/portals/
sudo cp share/xdg-desktop-portal/tidewm-portals.conf /usr/share/xdg-desktop-portal/
```

`xdg-desktop-portal` itself must already be installed (most distros ship it by default). Log in through TideWM's own session entry so `xdg-desktop-portal` picks up `XDG_CURRENT_DESKTOP=tidewm` and reads the config above. See [DOCUMENTATION.md](DOCUMENTATION.md)'s protocol matrix for the current verification status, and the [Roadmap](#roadmap) above for where this stands.

## Configuration

`~/.config/tidewm/config.wave`, generated with working defaults on first run. Hot-reload on save. Full key-by-key reference: [DOCUMENTATION.md](DOCUMENTATION.md).

Config is [Waves](DOCUMENTATION.md#waves-format), TideWM's own format: `key = value` is the rest of the line after the first `=` (no quoting needed for a spawn command's own flags), a line ending in `{` opens a real multi-line block, `#` comments. Splits across files (Hyprland's `source` idea, adapted):

```
# config.wave
include "monitors.wave"
include "keybinds.wave"

$mod = SUPER
terminal = $wave(kitty, alacritty, foot)
```

```
# keybinds.wave
bind $mod+Return = spawn:$terminal
```

## Contributing

Not soliciting code contributions yet, but this is exactly the point where testing reports matter most, especially on other GPUs and distros. Open an issue, or drop into [Discord](https://discord.gg/ZhkxA83cKk). See [CONTRIBUTING.md](CONTRIBUTING.md) for more.

## License

GPL-3.0, see [LICENSE](LICENSE). Copyright (C) 2026 Fi3w0.

&nbsp;

<div align="center">
<sub>Built on <a href="https://github.com/Smithay/smithay">Smithay</a>. Inspired by and borrows implementation patterns from <a href="https://github.com/YaLTeR/niri">niri</a>, <a href="https://github.com/malbiruk/driftwm">driftwm</a>, <a href="https://github.com/hyprwm/Hyprland">Hyprland</a>, and <a href="https://github.com/swaywm/sway">sway</a>.</sub>
</div>
