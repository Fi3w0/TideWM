# Distro packages

Each one installs the same files as the top-level `install.sh` (`TideWM`, `tidectl`, `wavefmt`, the Wayland session entry, the icon and the screencast portal files), but through the distro's own package manager. That way upgrades and removal work like any other package.

All of them build from git, so re-running the installer upgrades to the latest commit.

The compositor code is GPL-3.0-or-later, the installed icon is CC BY-SA 4.0
and the embedded font is OFL-1.1; packages install their notices alongside
the GPL text. Everything a package installs is free, so no license opt-in is
needed. See [LICENSING.md](../LICENSING.md).

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
        {
          programs.tidewm.enable = true;
        }
      ];
    };
  };
}
```

`programs.tidewm.enable` installs TideWM, adds it to the display manager's session list, and configures the portals (TideWM's ScreenCast backend plus GTK for everything else). It also turns on graphics, polkit and PipeWire if your config doesn't set them. Then run `sudo nixos-rebuild switch`.

Without the module:

```bash
nix run github:Fi3w0/TideWM
nix profile install github:Fi3w0/TideWM
nix develop
```

To change the build options, override the package: `tidewm.packages.x86_64-linux.default.override { withAccessibility = true; }`.

## After installing

Log out and pick **TideWM** in your display manager. The first launch writes `~/.config/tidewm/config.wave` and `keybinds.wave`. `Super+Return` opens a terminal, and the empty desktop shows your current binds and the config path.

## NVIDIA profile

The packages provide `50-tidewm.json`, a small NVIDIA OpenGL application profile
matching only the `TideWM` process. It sets `GLVidHeapReuseRatio = 0` to limit the
driver's pool of freed video memory. It adds no NVIDIA driver dependency; other
drivers ignore it. Ebb and TideWM work without it.

One native comparison on NVIDIA 595.104.02 found process GPU memory settled at
109 MiB without the profile and 49 MiB with it after the same screenshot
workload. RAM growth was unchanged. This supports the setting on that machine,
not a guarantee for every GPU, driver, workload or long session.

- **Arch/Gentoo:** packages install the active profile at
  `/etc/nvidia/nvidia-application-profiles-rc.d/50-tidewm.json`, plus a copy under
  `/usr/share/tidewm/nvidia/`. Arch tracks it as a backup file; Gentoo uses normal
  `/etc` configuration protection. Administrator edits are preserved on upgrades.
- **Source installer:** `install.sh` supplies a copy under
  `/usr/local/share/tidewm/nvidia/` and installs the `/etc` file only when there
  is no existing file or symlink at that path.
- **NixOS module:** installs the `/etc` profile declaratively. Set
  `programs.tidewm.nvidiaProfile.enable = false;` to opt out or manage your own
  profile. It does not enable or install NVIDIA drivers.
- **Nix package without the module:** supplies the data under
  `$out/share/tidewm/nvidia/`; installing the package alone does not activate it.
  A user-level profile works without root. For example, after building the
  package as `result`, run:

  ```bash
  install -Dm644 result/share/tidewm/nvidia/50-tidewm.json \
    "$HOME/.nv/nvidia-application-profiles-rc.d/50-tidewm.json"
  ```

The NVIDIA driver reads application profiles when it loads into a process, so
log out/back into TideWM after adding or changing the profile. No reboot is
needed. Existing sessions keep their startup settings.

To disable just the packaged TideWM rule on Arch/Gentoo/source installs, replace
the contents of its `/etc` file with this valid empty configuration (preserve
any custom settings elsewhere):

```json
{"rules": [], "profiles": []}
```

Do not just rename a file within `nvidia-application-profiles-rc.d`: NVIDIA reads
regular files there regardless of extension. An empty configuration also leaves
an administrator edit for normal package configuration protection. For a
user-level installation, remove its user-level file and ensure there is no
system-level TideWM rule. On NixOS, use the module option above. Relog after
rollback too. The source installer will preserve a customized empty file.

`tidectl doctor` inspects the selected compositor's mapped OpenGL libraries.
On proprietary NVIDIA OpenGL it checks the standard system/user `50-tidewm.json`
locations for the bundled data. It skips explicit application-profile disabling
or customized/unreadable files. A missing profile is an optional recommendation,
not a compositor failure. Profiles under other names, NVIDIA's extended JSON and
rule precedence are not evaluated; a PASS means the file is present, not proof
of the driver's effective setting. User profiles and driver-global options may
override the packaged file.
