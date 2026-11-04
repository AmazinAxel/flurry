let
  # the flake's nixpkgs, so `nix-shell` and `nix develop` agree
  lock = (builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.nixpkgs.locked;
in
{ pkgs ? import (fetchTarball { url = "https://github.com/NixOS/nixpkgs/archive/${lock.rev}.tar.gz"; sha256 = lock.narHash; }) {} }: let
  flurry = pkgs.callPackage ./package.nix { sideview = pkgs.writeShellScriptBin "flurry-sideview" "exit 1"; };
in pkgs.mkShell {
  inputsFrom = [ flurry ];
  nativeBuildInputs = with pkgs; [ clippy rustfmt ];
  # the font table package.nix bakes into settings.rs; dev builds read it at compile time (ui/text.rs)
  shellHook = ''
    export FLURRY_FONTS="$(cat ${flurry.fonts}/fonts.tsv)"
  '';
}
