<div align="center">

<img src="share/icons/TideWM-logo-faithful-4k.png" width="180" alt="TideWM logo">

# TideWM

**A Wayland desktop that feels like water.**

![License](https://img.shields.io/badge/license-GPL--3.0-blue?style=for-the-badge)
![Status](https://img.shields.io/badge/status-functional-brightgreen?style=for-the-badge)
[![Discord](https://img.shields.io/badge/Discord-Join-5865F2?style=for-the-badge&logo=discord&logoColor=white)](https://discord.gg/ZhkxA83cKk)

**[<kbd> <br> Try&nbsp;it <br> </kbd>](#try-it)**
**[<kbd> <br> Docs <br> </kbd>](DOCUMENTATION.md)**
**[<kbd> <br> Technical&nbsp;report <br> </kbd>](TECHNICAL_REPORT.md)**
**[<kbd> <br> Config <br> </kbd>](#configuration)**

</div>

Open a window and it ripples outward like a drop landing on a pond. Drag one and it sways. Switch workspaces and a wave sweeps the whole screen. It's not a screensaver sitting on top of your desktop, it's your desktop, and it runs fine on a laptop from 2015.

Underneath the water is a real tiling window manager: fast keyboard-driven layouts, multi-monitor, your older X11 apps still work, and a config file you edit and reload without restarting anything.

## See it

<video src="https://github.com/Fi3w0/TideWM/raw/master/share/media/demo-clasic.mp4" controls width="960">
  Your browser does not support embedded video. <a href="share/media/demo-clasic.mp4">Download the demo</a>.
</video>

## Why people are trying it

- **It actually looks like something.** Ripples on open, wave transitions between workspaces, glass and shadow on your windows, floating windows that drift like they're sitting on water. Every bit of it is a config toggle, and none of it costs you performance you'll notice.
- **Two ways to work.** Stick with classic numbered workspaces, or switch to Ocean: one endless zoomable canvas where every window lives in real 2D space instead of a workspace number. Switch between them live, no restart.
- **Runs on what you already own.** As little as ~600MB of RAM with the effects off, scaling up only as far as you actually turn things on.
- **Genuinely yours.** Every ripple, wave, border, and shadow is a value in a config file, hot-reloaded on save. Nothing about how it looks is hardcoded.

## Try it

TideWM builds from source, there's no package yet.

```bash
sudo pacman -S pkg-config wayland systemd-libs libinput libxkbcommon mesa libdrm seatd  # Arch
git clone https://github.com/Fi3w0/TideWM.git
cd TideWM
cargo build --release --locked
cargo run --locked   # opens nested inside your current session, safe to try
```

Other distros, the full dependency list, and setting it up as a real login session: see [TECHNICAL_REPORT.md](TECHNICAL_REPORT.md#building).

## A taste of the defaults

| Shortcut             | Action                     |
| --------------------- | -------------------------- |
| `Alt+Enter`           | Open a terminal            |
| `Alt+H/J/K/L`         | Focus a direction           |
| `Alt+V`               | Float a window              |
| `Alt+F`               | Fullscreen                  |
| `Super+1`..`Super+0`  | Switch workspace            |
| `Alt+O`               | Workspace overview          |

Everything here is rebindable, and this is a small slice of what's available. The full action/keybind/IPC catalog lives in [DOCUMENTATION.md](DOCUMENTATION.md).

## Configuration

`~/.config/tidewm/config.wave`, generated with working defaults the first time you run it. Save it and TideWM picks up the change immediately, no restart.

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

Full key-by-key reference: [DOCUMENTATION.md](DOCUMENTATION.md).

## About this project

I'm building TideWM solo. I use AI coding agents to move fast, then verify everything myself on real hardware before I trust it. If something breaks on your setup, that's genuinely useful, not an inconvenience, [open an issue](https://github.com/Fi3w0/TideWM/issues/new/choose) or drop into [Discord](https://discord.gg/ZhkxA83cKk).

For what's actually implemented, what's verified on real hardware, and what's still in progress: [TECHNICAL_REPORT.md](TECHNICAL_REPORT.md).

## Contributing

Not soliciting code contributions yet, but this is exactly the point where testing reports matter most, especially on other GPUs and distros. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

GPL-3.0, see [LICENSE](LICENSE). Copyright (C) 2026 Fi3w0.

&nbsp;

<div align="center">
<sub>Built on <a href="https://github.com/Smithay/smithay">Smithay</a>. Inspired by and borrows implementation patterns from <a href="https://github.com/YaLTeR/niri">niri</a>, <a href="https://github.com/malbiruk/driftwm">driftwm</a>, <a href="https://github.com/hyprwm/Hyprland">Hyprland</a>, and <a href="https://github.com/swaywm/sway">sway</a>.</sub>
</div>
