{
  description = "TideWM: a water-themed Wayland tiling compositor";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      gitCommit = self.shortRev or self.dirtyShortRev or null;
    in
    {
      packages = forAllSystems (pkgs: {
        tidewm = pkgs.callPackage ./packaging/nix/package.nix { inherit gitCommit; };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.tidewm;
      });

      overlays.default = final: _prev: {
        tidewm = final.callPackage ./packaging/nix/package.nix { inherit gitCommit; };
      };

      nixosModules.default =
        { pkgs, lib, ... }:
        {
          imports = [ ./packaging/nix/module.nix ];
          programs.tidewm.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.tidewm;
        };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.tidewm ];
          packages = [
            pkgs.cargo
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
