# Copyright 2026 Fiw
# Distributed under the terms of the GNU General Public License v2

EAPI=8

# Cargo.lock currently includes mlua 0.12, whose MSRV is Rust 1.88.
RUST_MIN_VER="1.88.0"

inherit cargo git-r3 xdg

DESCRIPTION="Water-themed Wayland tiling compositor built on Smithay"
HOMEPAGE="https://github.com/Fi3w0/TideWM"
EGIT_REPO_URI="https://github.com/Fi3w0/TideWM.git"
# Follow the upstream default branch.
EGIT_BRANCH="master"

LICENSE="GPL-3+"
# Dependent crate licenses
LICENSE+=" Apache-2.0 BSD CC0-1.0 ISC MIT Unicode-3.0 ZLIB"
SLOT="0"
IUSE="+screencast accessibility"

DEPEND="
	dev-libs/expat
	dev-libs/libinput:=
	media-libs/mesa
	dev-libs/wayland
	sys-auth/seatd:=
	virtual/libudev:=
	x11-libs/libdrm
	x11-libs/libxkbcommon
	screencast? ( media-video/pipewire:= )
"
RDEPEND="${DEPEND}
	screencast? (
		sys-apps/xdg-desktop-portal
		sys-apps/xdg-desktop-portal-gtk
	)
"
# pipewire-sys generates its bindings with bindgen (libclang)
BDEPEND="virtual/pkgconfig
	screencast? ( llvm-core/clang )"

QA_FLAGS_IGNORED=".*"

# Developer override: build from a local checkout instead of GitHub. Set
# TIDEWM_SRC=/path/to/TideWM in /etc/portage/env/gui-wm/tidewm (the
# packaging/gentoo/install.sh --local flag writes that file for you).
src_unpack() {
	if [[ -n ${TIDEWM_SRC} ]]; then
		einfo "Building from local checkout ${TIDEWM_SRC}"
		mkdir -p "${S}" || die
		tar -C "${TIDEWM_SRC}" --exclude=./target --exclude=./.git -cf - . |
			tar -C "${S}" -xf - || die
	else
		git-r3_src_unpack
	fi
	cargo_live_src_unpack
}

src_configure() {
	local myfeatures=(
		$(usev screencast)
		$(usev accessibility)
	)
	# Vendored Git crates are already fetched during src_unpack.
	cargo_src_configure --frozen --bin TideWM --bin tidectl --bin wavefmt
}

src_install() {
	dobin "$(cargo_target_dir)"/TideWM "$(cargo_target_dir)"/tidectl "$(cargo_target_dir)"/wavefmt

	insinto /usr/share/wayland-sessions
	doins share/wayland-sessions/tidewm.desktop
	newicon share/icons/TideWM-logo-faithful-4k.png tidewm.png

	if use screencast; then
		insinto /usr/share/xdg-desktop-portal/portals
		doins share/xdg-desktop-portal/tidewm.portal
		insinto /usr/share/xdg-desktop-portal
		doins share/xdg-desktop-portal/tidewm-portals.conf
	fi

	dodoc README.md CHANGELOG.md DOCUMENTATION.md WAVE.md
}

pkg_postinst() {
	xdg_pkg_postinst
	elog "Pick TideWM in your display manager, or run TideWM from a TTY."
	elog "The first launch writes ~/.config/tidewm/config.wave (and keybinds.wave)."
	elog "Default terminal bind: Super+Return. Users of GL clients need access"
	elog "to /dev/dri/renderD* (the video group or the uaccess udev tag)."
}
