#!/usr/bin/env bash
# Build and install TideWM on Arch (and derivatives) as a pacman package,
# so it upgrades and uninstalls cleanly (pacman -R tidewm-git).
#
#   packaging/arch/install.sh           build the upstream master branch
#   packaging/arch/install.sh --local   build this checkout's committed HEAD
#
# Extra arguments go to makepkg (e.g. --nocheck to skip the test suite).
set -euo pipefail

pkg_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$pkg_dir/../.." && pwd)

if [[ ${1:-} == --local ]]; then
    shift
    # makepkg clones from git, so only committed changes are built.
    export TIDEWM_REPO="file://$repo_root"
    if [[ -n $(git -C "$repo_root" status --porcelain) ]]; then
        echo "Note: uncommitted changes in $repo_root are not included." >&2
    fi
    echo "Building committed HEAD of $repo_root"
fi

if ! command -v makepkg >/dev/null; then
    echo "makepkg not found: this installer is for Arch-based systems." >&2
    exit 1
fi

# Build in a scratch copy so the checkout never collects src/ or pkg/ trees.
build_dir=$(mktemp -d)
trap 'rm -rf "$build_dir"' EXIT
cp "$pkg_dir/PKGBUILD" "$build_dir/"
cd "$build_dir"
makepkg --syncdeps --install --cleanbuild "$@"

echo "Installed. Log out and pick TideWM in your display manager to use it."
