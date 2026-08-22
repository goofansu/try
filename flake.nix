{
  description = "try — find, create and jump into project directories";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      version = "0.1.0";
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      # Lets a consumer build try against their own nixpkgs instead of the one
      # pinned here: nixpkgs.overlays = [ try.overlays.default ];
      overlays.default = final: _prev: {
        try = final.callPackage ./package.nix { inherit version; };
      };

      packages = forAllSystems (pkgs: rec {
        try = pkgs.callPackage ./package.nix { inherit version; };
        default = try;
      });

      # Home Manager module: adds the package and, unless disabled, the shell
      # function that makes `try` actually change directory.
      homeModules.default =
        { pkgs, lib, ... }:
        {
          imports = [ ./modules/home-manager.nix ];
          programs.try.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.try;
        };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            git
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
