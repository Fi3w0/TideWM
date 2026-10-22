# TideWM — Technical Report

The quick technical picture: what TideWM is built from, what works, what's been tested on real hardware, and how to build it. For the pitch, see [README.md](README.md). For every config key, action and IPC request, see [DOCUMENTATION.md](DOCUMENTATION.md).

## What it is

A Wayland compositor written in Rust on [Smithay](https://github.com/Smithay/smithay). Underneath is a complete tiling window manager: BSP, master/stack and cascade layouts, workspaces, multi-monitor, layer shell, IPC and XWayland. On top sits the water look (ripples, waves, glass, shadows, physics), and every piece of it is a config toggle.

Current version: **0.90.162**, beta. 1.0 waits for a wider real-hardware pass (Intel, hybrid laptops, more distros).

## Architecture

- **Two backends.** `winit` runs TideWM nested inside your current desktop (safe to try, the main dev loop). `udev`/DRM is the real standalone session you log into.
- **Three spatial engines**, switchable live with `tidectl engine` or a config reload, carrying every open window across:
  - **Classic:** numbered workspaces per monitor, with the Depth Deck for parking tiles.
  - **Ocean:** one zoomable 2D canvas with a camera per monitor.
  - **Scroll:** a scrolling strip of apps per monitor, niri-style.

  How they relate: [SPATIAL_MODEL.md](SPATIAL_MODEL.md).
- **XWayland** through [`xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite), so X11 apps arrive as ordinary Wayland windows.
- **One render pipeline.** Glass, shadows, rounding, borders and animations are drawn once per frame from a shared window-placement snapshot. The same path feeds both backends, screenshots, screen sharing and workspace transitions, so an effect never needs a second implementation.
- **Config:** [Wave](WAVE.md), data on the surface and Lua underneath. It's hot-reloaded, and a broken edit never half-applies.
- **Memory:** about 50 MB PSS for plain tiling and 60–70 MB with the full water stack on AMD. About 100 MB on NVIDIA, a third of which is the driver's own mappings. 2 GB is a hard ceiling for feature growth, not a target.

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
| IPC and `tidectl` (queries, event stream, `doctor`, `report`, `perf`) | Done |
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
- **Scroll:** the niri features still missing, such as stacked columns, consume/expel, pointer resizing of columns, and drag-to-insert previews.
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
- [SCROLL.md](SCROLL.md): the Scroll engine guide
- [CHANGELOG.md](CHANGELOG.md): what changed, in short; [DEV_CHANGELOG.md](DEV_CHANGELOG.md) has the full detail
- [packaging/README.md](packaging/README.md): distro packages
- [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), [LICENSING.md](LICENSING.md)
