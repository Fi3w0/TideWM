# NixOS module: `programs.tidewm.enable = true;` installs TideWM, offers it
# as a display-manager session and wires its screencast portal.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.tidewm;
in
{
  options.programs.tidewm = {
    enable = lib.mkEnableOption "the TideWM Wayland compositor";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The TideWM package to use. The flake sets this to its own build.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    services.displayManager.sessionPackages = [ cfg.package ];

    # TideWM implements the ScreenCast portal itself and routes the rest to
    # the GTK backend (share/xdg-desktop-portal/tidewm-portals.conf).
    xdg.portal = {
      enable = lib.mkDefault true;
      extraPortals = [
        cfg.package
        pkgs.xdg-desktop-portal-gtk
      ];
      configPackages = [ cfg.package ];
    };

    hardware.graphics.enable = lib.mkDefault true;
    security.polkit.enable = lib.mkDefault true;
    services.pipewire.enable = lib.mkDefault true;
  };
}
