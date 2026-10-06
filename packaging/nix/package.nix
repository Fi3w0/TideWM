{
  lib,
  rustPlatform,
  pkg-config,
  libdrm,
  libGL,
  libinput,
  libxkbcommon,
  libgbm,
  pipewire,
  seatd,
  systemd,
  wayland,
  withScreencast ? true,
  withAccessibility ? false,
  # Embedded in `TideWM --version`; the flake passes its own revision.
  gitCommit ? null,
}:

let
  root = ../..;
  cargoToml = lib.importTOML (root + "/Cargo.toml");
in
rustPlatform.buildRustPackage {
  pname = "tidewm";
  inherit (cargoToml.package) version;

  # Only what the build reads, so `target/` and notes never enter the store.
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
      (root + "/build.rs")
      (root + "/src")
      (root + "/assets")
      (root + "/share")
      (root + "/LICENSE")
      (root + "/LICENSES")
      (root + "/LICENSING.md")
      (root + "/README.md")
      (root + "/CHANGELOG.md")
      (root + "/DOCUMENTATION.md")
      (root + "/WAVE.md")
    ];
  };

  cargoLock = {
    lockFile = root + "/Cargo.lock";
    # The pinned Smithay git dependency is fetched by its locked revision.
    allowBuiltinFetchGit = true;
  };

  nativeBuildInputs = [
    pkg-config
  ]
  # pipewire-sys generates bindings with bindgen (libclang).
  ++ lib.optional withScreencast rustPlatform.bindgenHook;

  buildInputs = [
    libdrm
    libGL
    libgbm
    libinput
    libxkbcommon
    seatd
    systemd # libudev
    wayland
  ]
  ++ lib.optional withScreencast pipewire;

  buildNoDefaultFeatures = true;
  buildFeatures =
    lib.optional withScreencast "screencast" ++ lib.optional withAccessibility "accessibility";

  env = lib.optionalAttrs (gitCommit != null) { TIDEWM_GIT_COMMIT = gitCommit; };

  # EGL and libwayland-client are loaded with dlopen by the nested (winit)
  # backend; link them explicitly so they resolve from the Nix store.
  RUSTFLAGS = map (arg: "-C link-arg=${arg}") [
    "-Wl,--push-state,--no-as-needed"
    "-lEGL"
    "-lwayland-client"
    "-Wl,--pop-state"
  ];

  # The test suite runs in CI and via the Arch PKGBUILD's check(); it has not
  # been audited for the Nix build sandbox, so it doesn't gate this build.
  doCheck = false;

  postInstall = ''
    install -Dm644 share/wayland-sessions/tidewm.desktop -t $out/share/wayland-sessions
    substituteInPlace $out/share/wayland-sessions/tidewm.desktop \
      --replace-fail "Exec=TideWM" "Exec=$out/bin/TideWM"
    install -Dm644 share/icons/TideWM-logo-faithful-4k.png $out/share/pixmaps/tidewm.png
    install -Dm644 LICENSE $out/share/licenses/tidewm/LICENSE
    install -Dm644 share/icons/LICENSE $out/share/licenses/tidewm/LOGO-NOTICE
    install -Dm644 LICENSES/CC-BY-NC-SA-4.0.txt $out/share/licenses/tidewm/CC-BY-NC-SA-4.0.txt
    install -Dm644 assets/fonts/OFL-LICENSE.txt $out/share/licenses/tidewm/OFL-LICENSE.txt
  ''
  + lib.optionalString withScreencast ''
    install -Dm644 share/xdg-desktop-portal/tidewm.portal -t $out/share/xdg-desktop-portal/portals
    install -Dm644 share/xdg-desktop-portal/tidewm-portals.conf -t $out/share/xdg-desktop-portal
  ''
  + ''
    install -Dm644 README.md CHANGELOG.md DOCUMENTATION.md WAVE.md LICENSING.md -t $out/share/doc/tidewm
  '';

  # Lets services.displayManager.sessionPackages find tidewm.desktop.
  passthru.providedSessions = [ "tidewm" ];

  meta = {
    description = "Water-themed Wayland tiling compositor built on Smithay";
    homepage = "https://github.com/Fi3w0/TideWM";
    license = with lib.licenses; [
      gpl3Plus
      cc-by-nc-sa-40
      ofl
    ];
    mainProgram = "TideWM";
    platforms = lib.platforms.linux;
  };
}
