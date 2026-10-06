# Distro packages

Each one installs the same files as the top-level `install.sh` (`TideWM`, `tidectl`, `wavefmt`, the Wayland session entry, the icon and the screencast portal files), but through the distro's own package manager. That way upgrades and removal work like any other package.

All of them build from git, so re-running the installer upgrades to the latest commit.

| Distro | Files | One command |
| --- | --- | --- |
| Gentoo | `gentoo/` (overlay with a live ebuild) | `packaging/gentoo/install.sh` |
| Arch and derivatives | `arch/PKGBUILD` (`tidewm-git`) | `packaging/arch/install.sh` |
| NixOS | `/flake.nix`, `nix/` | see below |
| Anything else | | top-level `./install.sh` |

## Gentoo

```bash
packaging/gentoo/install.sh           # upstream master
packaging/gentoo/install.sh --local   # this checkout, uncommitted changes included
packaging/gentoo/install.sh --ask     # extra arguments go to emerge
```

The script copies the overlay to `/var/db/repos/tidewm`, registers it in `/etc/portage/repos.conf/tidewm.conf`, unmasks the live ebuild (`gui-wm/tidewm::tidewm **`), and emerges it. USE flags: `screencast` (default on) and `accessibility`.

`--local` writes `TIDEWM_SRC=<checkout>` to `/etc/portage/env/gui-wm/tidewm`. A file there that the script didn't write is left untouched.

Manual route: point a `repos.conf` entry at `packaging/gentoo`, then `emerge gui-wm/tidewm::tidewm`.

## Arch

```bash
packaging/arch/install.sh             # upstream master
packaging/arch/install.sh --local     # this checkout's committed HEAD
packaging/arch/install.sh --nocheck   # extra arguments go to makepkg
```

This builds `tidewm-git` in a temporary directory with `makepkg -si`, so `pacman -R tidewm-git` removes it cleanly. Builds include screen sharing. Optional runtime packages (portals, `xwayland-satellite`, a terminal) are listed as `optdepends`. The same PKGBUILD is the starting point for an AUR package.

## NixOS

Add the flake to your system flake and enable the module:

```nix
{
  inputs.tidewm.url = "github:Fi3w0/TideWM";

  outputs = { nixpkgs, tidewm, ... }: {
    nixosConfigurations.<host> = nixpkgs.lib.nixosSystem {
      modules = [
        tidewm.nixosModules.default
        { programs.tidewm.enable = true; }
      ];
    };
  };
}
```

`programs.tidewm.enable` installs TideWM, adds it to the display manager's session list, and configures the portals (TideWM's ScreenCast backend plus GTK for everything else). It also turns on graphics, polkit and PipeWire if your config doesn't set them. Then run `sudo nixos-rebuild switch`.

Without the module:

```bash
nix run github:Fi3w0/TideWM            # try it nested inside your current session
nix profile install github:Fi3w0/TideWM
nix develop                            # dev shell with every build dependency
```

To change the build options, override the package: `tidewm.packages.x86_64-linux.default.override { withAccessibility = true; }`.

## After installing

Log out and pick **TideWM** in your display manager. The first launch writes `~/.config/tidewm/config.wave` and `keybinds.wave`. `Super+Return` opens a terminal, and the empty desktop shows your current binds and the config path.
