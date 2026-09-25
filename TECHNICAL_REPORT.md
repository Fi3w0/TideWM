# TideWM — Technical Report

A fast technical reference for TideWM: what it is, what's implemented, current hardware-verification status, and how to build it. For the pitch, see [README.md](README.md). For the full config/action/IPC reference, see [DOCUMENTATION.md](DOCUMENTATION.md). For the Classic/Ocean spatial-engine design, see [SPATIAL_MODEL.md](SPATIAL_MODEL.md). For release-by-release detail, see [CHANGELOG.md](CHANGELOG.md).

## What it is

A Wayland compositor written in Rust on [Smithay](https://github.com/Smithay/smithay). A full tiling-WM feature set (BSP/master-stack/cascade layouts, workspaces, multi-monitor, layer-shell, IPC, XWayland) with a water/aqua render identity layered on top as a fully toggleable effect stack, plus a second spatial engine ("Ocean") as an alternative to numbered workspaces.

Current release: **0.90.104**, second major pre-release. 1.0 is intentionally reserved until the effect stack and Ocean get a broader real-hardware pass (see CHANGELOG).

## Architecture

- **Two backends**: `winit` (nested inside an existing session, the primary dev/test loop) and `udev`/DRM (standalone TTY session, the real daily-driver path).
- **Two spatial engines**, chosen with `spatial_engine = classic|ocean` and switchable live on a config hot reload, migrating every window in place with no restart.
- **XWayland** via a spawned [`xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite) process rather than an embedded X11 window manager, so X11 clients arrive as ordinary `xdg_shell` surfaces.
- **Render pipeline**: one shared backdrop-capture pipeline feeds water-glass/frost glass, then shadow, then rounding/borders, then window-open/close/move animation. Each output render or desktop capture shares one frame-owned placement snapshot across those consumers; the same element walk feeds both backends, screenshots, screencasts, and workspace-transition captures, so effects don't need parallel implementations per output path.
- **RAM target**: effect-scaled with a 2GB absolute ceiling. Real measurements come in far below that: ~50MB PSS for a plain tiling setup (effects off), ~60-70MB PSS idle and ~63MB with nine glass windows with the full water stack on (real AMD, 0.90.59) -- roughly half of same-machine Hyprland.

## Feature status

| Area | Status |
| --- | --- |
| Tiling: BSP, master/stack, cascade (aspect-adapting rows, drag-resize, liquid pour/drain) | Done |
| Workspaces, scratchpads (classic + named), per-window pinning | Done |
| Multi-monitor, hotplug, independent tiling tree per output, mixed-DPI | Done |
| Hybrid/multi-GPU client render offload | Implemented with Smithay `GpuManager`, DMA-BUF feedback, and primary-node early import; real AMD+Nvidia laptop verification pending. Outputs remain confined to one selected GPU. |
| Window groups (tabbed), floating, fullscreen, maximize, pseudo-tiling | Done |
| Layer-shell (bars, launchers, lock screens) | Done |
| XWayland | Done, via xwayland-satellite |
| Screenshots, clipboard, session lock | Done |
| PipeWire screencasting | Done (`--features screencast`), verified end to end via real OBS and Discord on standalone hardware |
| IPC: request/response plus an event-stream subscribe mode | Done |
| `tidectl` CLI, including `doctor`/`report` for bug triage | Done |
| Water/decoration render stack: impulse ripples, wave workspace transitions, tiled/floating water-glass and per-app liquid frost glass, analytical shadows, animated gradient borders, rounded clipping, configurable window animations, move/resize viscosity, connected-vessel BSP resizing, opt-in floating sway, automatic depth/buoyancy, ambient caustics | Done; tiled/liquid frost is release-GLES nested-verified in Classic/Ocean, standalone AMD remains, and the earlier stack is standalone-AMD-verified |
| Custom shaders (`shaders { }`, `shader "<name>" { }`): opt-in user `.frag` effects over a window's backdrop, up to four chained stages with `save`/`get` and extra texture bindings, per-window and per-workspace assignment, bounded budgets | Done through Phase 7B; compile/draw covered by Mesa software-GLES tests, not yet run in a nested or native session |
| Classic Depth Deck (tiled-window park/swap recall) | Done, standalone-AMD-verified |
| Ocean spatial engine: reefs, per-output cameras, sink/dredge/surface depth, bookmarks, freeform window detach, smart tiling, live Classic↔Ocean migration | Done, standalone-AMD-verified |
| Ocean compass (off-screen urgent/deep glow cues) and whole-world overview minimap | Done, nested-verified only |
| Continuous swim navigation (trackpad swipe between workspaces) | Done, real-touchpad-verified |
| Floating-window ocean physics (`float_physics { tier = light|full }`): disturbance-driven bob/drift, `full` adds mass/collisions/a traveling wave field | Done, nested-verified only |
| Ocean currents: bounded render-only downstream drift for visible unfocused floating windows, with focus/drag pause | Done, release-GLES nested-verified on AMD |
| Weighted buoyancy: per-app render-only mass/sink for Classic and Ocean floaters, with Ocean flow attenuation | Done, release-GLES nested-verified on AMD under Ocean; Classic shares the validated render path |
| Nvidia support | Standalone DRM backend verified on a real RTX 3060 (proprietary 595.99.02, two outputs at mixed scale, 2026-09-25); nested verified earlier |
| AUR package | Not yet |

## Hardware verification

- **AMD**: primary development and test hardware. The standalone `udev`/DRM backend, the full water/decoration render stack, swim's real-touchpad gesture path, and Ocean's core navigation (reefs, cameras, freeform drag) are all verified here.
- **Latest standalone AMD health pass (2026-08-13)**: release 0.90.72 on Renoir completed full-output and cursor-overlay screencopy, reversible workspace transitions with ordered IPC events, invalid-action/batch/eval handling, and 100 one-shot plus 20 interrupted subscription connections without descriptor or PSS growth. Doctor remained all-pass with no TideWM panic/error or coredump. One-window PSS settled at 86.8 MiB after the reload/capture pass. The configured 24-fps caustics were the dominant continuous CPU/GPU cost; see `report.md` for the measured A/B and its whole-GPU caveat.
- **Latest standalone NVIDIA pass (2026-09-25)**: 0.90.106 (`13b104e`, `fiw/testing-fixes`) on the RTX 3060 above: doctor all-pass; full-output and cursor-overlay `grim`; unknown actions, an invalid mixed batch, eval syntax errors and a runaway eval loop (execution budget) all failed cleanly; oversized, garbage and truncated IPC requests left the compositor running; 100 one-shot queries plus 20 interrupted subscribers left PSS and descriptors unchanged. Idle ~1% of one core, 131 MiB TideWM VRAM; nine frost windows ~0-1% CPU and 139 MiB. PSS plateaus near 180 MiB after warm-up, of which ~60-95 MiB is `/dev/nvidia*` mappings; open/close cycles returned to the same value with a flat heap, so this is not a leak.
- **Nvidia**: standalone `udev`/DRM backend verified on a real RTX 3060 (proprietary 595.99.02 with the open kernel modules, Gentoo, kernel 7.2.7), logged in from the display manager as a real session. GLES renders on the dGPU with the Radeon iGPU excluded through `gpu { render = vendor:nvidia; exclude = [vendor:amd] }`; two outputs run together (2560×1440@144 at 1.25×, 1920×1080@180 at 1.0×). The overlay-plane workaround branch runs for every output on this driver and scanout is correct on both; no overlay-specific test was done. Frost glass, custom shaders, screencopy, xwayland-satellite, portal and drag and drop worked. Measured cost is in `report.md`. The nested backend was verified earlier on the same card.
- **Hybrid GPU**: selection/exclusion, render-node feedback, cross-device import, hot add/remove, and VT lifecycle compile and unit-test clean. The required end-to-end AMD+Nvidia PRIME/offload run is pending on the handoff laptop; do not call this hardware-verified until a non-primary game buffer displays correctly there at sustained frame rate.
- **Intel**: untested so far.
- **Still nested-only**: Ocean compass/overview, floating-window ocean physics (both tiers), `canvas_pan_button`, `modifier_pan_fingers`.

## Building

**Rust toolchain**: stable, 1.86+.

**Native dependencies:**

| Library | Notes |
| --- | --- |
| `pkg-config` | Build-time only |
| `wayland` | `libwayland-client`/`-server`/`-egl` |
| `libudev` | `systemd-libs` on systemd distros, `eudev`/`libudev-zero` elsewhere; doesn't require systemd |
| `libinput` | |
| `libxkbcommon` | |
| `mesa` | `libEGL`, `libgbm` |
| `libdrm` | |
| `libseat` | The `seatd` package on most distros; also works against systemd-logind |
| `clang`/`libclang` | Build-time only, and only if building with `--features screencast` -- `pipewire-sys`'s bindgen step needs it. `install.sh` always builds with this feature on, so it's needed for that path even if you don't care about screen-sharing. |

```bash
# Arch (add `clang` if building with --features screencast, e.g. via install.sh)
sudo pacman -S pkg-config wayland systemd-libs libinput libxkbcommon mesa libdrm seatd

# Fedora (add `clang` if building with --features screencast, e.g. via install.sh)
sudo dnf install pkgconf-pkg-config wayland-devel systemd-devel libinput-devel \
    libxkbcommon-devel mesa-libEGL-devel mesa-libgbm-devel libdrm-devel libseat-devel

# Debian / Ubuntu (add `libclang-dev` if building with --features screencast, e.g. via install.sh)
sudo apt install pkg-config libwayland-dev libudev-dev libinput-dev \
    libxkbcommon-dev libegl1-mesa-dev libgbm-dev libdrm-dev libseat-dev

# openSUSE (add `clang` if building with --features screencast, e.g. via install.sh)
sudo zypper install pkgconf-pkg-config wayland-devel libudev-devel libinput-devel \
    libxkbcommon-devel Mesa-libEGL-devel Mesa-libgbm-devel libdrm-devel libseat-devel

# Gentoo (systemd or OpenRC profile -- sys-fs/eudev covers libudev on a
# non-systemd profile; skip it if sys-apps/systemd is already merged. Add
# sys-devel/clang if building with --features screencast, e.g. via install.sh)
sudo emerge --ask dev-util/pkgconf dev-libs/wayland dev-libs/libinput \
    x11-libs/libxkbcommon media-libs/mesa x11-libs/libdrm sys-auth/seatd sys-fs/eudev
```

These package names are believed correct but only actually exercised on Arch (the maintainer's own distro) — if the build fails looking for one on your distro, that's exactly the kind of report worth [opening an issue](https://github.com/Fi3w0/TideWM/issues/new/choose) for. The library list above (not the package names) is what actually matters if you need to hunt one down yourself.

Optional: [`xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite) for X11 apps (`xwayland.enabled = false` to skip it).

```bash
git clone https://github.com/Fi3w0/TideWM.git
cd TideWM
cargo build --release --locked
```

### Running

```bash
cargo run --locked
```

Backend auto-selects: nested (`winit`) when `WAYLAND_DISPLAY`/`DISPLAY` is set, standalone TTY/DRM (`udev`) otherwise. Switch to a free VT with neither set to try it standalone.

To launch from a display manager (GDM, SDDM, greetd): put the `TideWM` binary (note the case; `[[bin]] name` in `Cargo.toml` builds it capitalized, and `tidewm.desktop`'s `Exec=` line expects exactly that) on `PATH`, copy `share/wayland-sessions/tidewm.desktop` into `/usr/share/wayland-sessions/`, and copy `share/icons/TideWM-logo-faithful-4k.png` to `/usr/share/pixmaps/tidewm.png` for the session picker's icon. `./install.sh` does all three, so re-running it after a change is the only step needed to pick up a new build at the next login.

### Screen sharing

Discord, OBS, and anything else that shares your screen go through `xdg-desktop-portal`, not TideWM directly. TideWM implements its own `org.freedesktop.impl.portal.ScreenCast` backend, so no `xdg-desktop-portal-gnome`/GTK4 chain is required. The PipeWire buffer path (SHM/MemFd) delivers real frames, verified end to end on a standalone TTY session through both real OBS and real Discord. DMA-BUF export stays disabled.

```bash
cargo build --release --locked --features screencast
sudo cp share/xdg-desktop-portal/tidewm.portal /usr/share/xdg-desktop-portal/portals/
sudo cp share/xdg-desktop-portal/tidewm-portals.conf /usr/share/xdg-desktop-portal/
```

`tidewm-portals.conf`'s `default=gtk` line routes every portal interface *other* than ScreenCast (file chooser, print, settings/appearance, etc.) to `xdg-desktop-portal-gtk`, a separate, much lighter package than the `xdg-desktop-portal-gnome` chain mentioned above (GTK3, not GTK4/libadwaita/nautilus). Install it too, or those other portal requests have no backend to answer them: `sudo pacman -S xdg-desktop-portal-gtk` on Arch, `xdg-desktop-portal-gtk` on Fedora/Debian/openSUSE.

`xdg-desktop-portal` itself must already be installed (most distros ship it by default). Log in through TideWM's own session entry so `xdg-desktop-portal` picks up `XDG_CURRENT_DESKTOP=tidewm` and reads the config above.

## Roadmap

- **Audit follow-through**: real-DRM cadence verification for P-12, a maintained Smithay-pin decision for DRM-master reacquisition failure, and fresh review of the medium-confidence items still open in `report.md`.
- **Lower TideWM-owned VRAM**: active capture cost is measurable through `tidectl perf`, adjustable with `backdrop_capture_scale`, and reclaimed after the last output stops presenting a glass surface; `builtin_wallpaper = false` also avoids the fallback texture. Measure representative large glass windows before considering a shared per-output blur framebuffer, whose different overlap/occlusion semantics need an explicit design decision. Client surface buffers remain outside compositor control.
- **Non-water motion presets**: smooth window move/resize and workspace motion that works with `water_effects = false`, exposed as Wave-selectable presets and tunable fields. Exact feel and defaults require maintainer approval before implementation.
- **Feel-tuning** across viscosity, sway, depth timings, cascade's drag feel, floating-window ocean physics, and the transition/ripple presets. All ship with working defaults; the actual feel still gets refined against real use.
- **Standalone hardware pass** for what's still nested-only: the Ocean compass/overview, and both floating-window ocean physics tiers.
- **AUR package**: build from source for now.
- **Design-pending identity features** (parking lot, needs a design conversation before pickup): tide contexts and workspace depth-moods. Currents, Cascade's pour/drain visual, and non-sorting weighted buoyancy are implemented with their approved contracts.

## Docs map

- [README.md](README.md) — the pitch and quick start
- [DOCUMENTATION.md](DOCUMENTATION.md) — full config-key, action-string, and IPC reference
- [SPATIAL_MODEL.md](SPATIAL_MODEL.md) — Classic vs. Ocean engine design
- [CHANGELOG.md](CHANGELOG.md) — release-by-release history
- [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md)
