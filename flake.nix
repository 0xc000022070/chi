{
  description = "chi: self-hosted error tracking (bugs) and build tracking (builds)";

  inputs.nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";

  outputs = {nixpkgs, ...}: let
    systems = nixpkgs.lib.genAttrs ["aarch64-linux" "x86_64-linux"];
  in {
    packages = systems (system: {
      default = nixpkgs.legacyPackages.${system}.callPackage ./package.nix {};
    });
  };
}
