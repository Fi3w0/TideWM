#!/usr/bin/env bash
# Builds TideWM and installs it (binary, Wayland session entry, session
# icon) so a display manager (SDDM, GDM, greetd) launches the latest build.
# Re-run this instead of repeating README.md's "Building"/"Running" steps by
# hand after every change; log out and pick TideWM again afterward to use it.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

cargo build --release --locked

sudo install -Dm755 target/release/TideWM /usr/local/bin/TideWM
sudo install -Dm644 share/wayland-sessions/tidewm.desktop /usr/share/wayland-sessions/tidewm.desktop
sudo install -Dm644 share/icons/TideWM-logo-faithful-4k.png /usr/share/pixmaps/tidewm.png

echo "Installed. Log out and pick TideWM in your display manager to use this build."
