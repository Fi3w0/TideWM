#!/usr/bin/env bash
# Install TideWM on Gentoo through Portage, using the overlay next to this
# script. Re-run it to update: emerge rebuilds the live ebuild from git.
#
#   packaging/gentoo/install.sh           build the upstream master branch
#   packaging/gentoo/install.sh --local   build this checkout instead
#
# Extra arguments after the flags go straight to emerge (e.g. --ask).
set -euo pipefail

overlay_src=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$overlay_src/../.." && pwd)
overlay_dst=/var/db/repos/tidewm
pkg=gui-wm/tidewm

local_build=0
if [[ ${1:-} == --local ]]; then
    local_build=1
    shift
fi

if ! command -v emerge >/dev/null; then
    echo "emerge not found: this installer is for Gentoo." >&2
    exit 1
fi

# Copy the overlay out of the checkout so Portage never reads from a home
# directory it may not have access to.
sudo install -d "$overlay_dst"
sudo rm -rf "${overlay_dst:?}"/{gui-wm,metadata,profiles}
sudo cp -r "$overlay_src"/{gui-wm,metadata,profiles} "$overlay_dst"/

sudo install -d /etc/portage/repos.conf
sudo tee /etc/portage/repos.conf/tidewm.conf >/dev/null <<EOF
[tidewm]
location = $overlay_dst
auto-sync = no
EOF

# Live ebuilds carry no KEYWORDS, so they must be unmasked explicitly.
# package.accept_keywords may be a directory or a single file.
keywords=/etc/portage/package.accept_keywords
if [[ -f $keywords ]]; then
    grep -qxF "$pkg::tidewm **" "$keywords" ||
        echo "$pkg::tidewm **" | sudo tee -a "$keywords" >/dev/null
else
    sudo install -d "$keywords"
    echo "$pkg::tidewm **" | sudo tee "$keywords/tidewm" >/dev/null
fi

# Portage sources /etc/portage/env/<category>/<package> for that package;
# the ebuild reads TIDEWM_SRC from it to build a local checkout. Only a file
# carrying our marker is replaced or removed; a hand-written one is kept.
marker="# Managed by TideWM packaging/gentoo/install.sh"
env_file=/etc/portage/env/$pkg
ours=1
if [[ -e $env_file ]] && ! grep -qxF "$marker" "$env_file"; then
    ours=0
    echo "Keeping existing $env_file (not written by this script)."
fi
if ((ours)); then
    sudo install -d /etc/portage/env/gui-wm
    if ((local_build)); then
        printf '%s\nTIDEWM_SRC=%s\n' "$marker" "$repo_root" | sudo tee "$env_file" >/dev/null
        echo "Building from local checkout $repo_root"
    else
        sudo rm -f "$env_file"
    fi
elif ((local_build)); then
    echo "--local ignored: set TIDEWM_SRC in $env_file yourself." >&2
fi

sudo emerge --verbose "$@" "$pkg::tidewm"

echo "Installed. Log out and pick TideWM in your display manager to use it."
