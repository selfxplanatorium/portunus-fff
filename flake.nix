{
  description = "Application launcher and power-user search for Wayland (portunus fork with fff file search)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    bun2nix.url = "github:nix-community/bun2nix?ref=2.1.2";
    bun2nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  # Only applied automatically for trusted users - everyone else needs the
  # substituter in their own nix.conf (see the Nix section in README.md) or
  # `--accept-flake-config`.
  nixConfig = {
    extra-substituters = [
      "https://portunus.cachix.org"
      "https://nix-community.cachix.org"
    ];
    extra-trusted-public-keys = [
      "portunus.cachix.org-1:byhkNv2iSgx4QQmrwgmtzYHFY+ztYe8+3vcAStcDemI="
      "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs="
    ];
  };

  outputs = { nixpkgs, bun2nix, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      pkgsFor = eachSystem (system: import nixpkgs {
        inherit system;
        overlays = [ bun2nix.overlays.default ];
      });
    in
    {
      packages = eachSystem (system: rec {
        unwrapped = pkgsFor.${system}.callPackage ./packaging/nix/unwrapped.nix { };
        default = pkgsFor.${system}.callPackage ./packaging/nix/package.nix { inherit unwrapped; };
      });

      devShells = eachSystem (system: {
        default = pkgsFor.${system}.mkShell {
          packages = with pkgsFor.${system}; [
            bun
            cargo
            rustc
            pkg-config
            rustPlatform.bindgenHook # leptess' *-sys crates need libclang for bindgen
            # qualified, bun2nix resolves to the flake input
            pkgsFor.${system}.bun2nix # regenerate bun.nix when bun.lock changes

            gtk3
            libsoup_3
            webkitgtk_4_1
            gtk-layer-shell
            tesseract
            leptonica
            glib-networking

            poppler-utils
            wl-clipboard
            cliphist
            dict
            wtype
          ];

          shellHook = ''
            # Match packaging/nix/package.nix (--set-default): keep WebKitGTK off
            # the explicit-sync path, otherwise the dev binary dies on NVIDIA with
            # "Missing acquire timeline" as soon as the window first paints.
            export __NV_DISABLE_EXPLICIT_SYNC="''${__NV_DISABLE_EXPLICIT_SYNC:-1}"
          '';
        };
      });
    };
}
