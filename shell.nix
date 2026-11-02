{ pkgs ? import <nixpkgs> {} }: pkgs.mkShell {
  nativeBuildInputs = with pkgs; [ cargo rustc clippy rustfmt pkg-config rustPlatform.bindgenHook ];
  buildInputs = with pkgs; [ harfbuzz libxkbcommon dbus pipewire pam ];
  # the font table package.nix bakes into settings.rs; dev builds read it at compile time (ui/text.rs)
  shellHook = ''
    export FLURRY_FONTS="$(cat ${(pkgs.callPackage ./package.nix { sideview = null; }).fonts}/fonts.tsv)"
  '';
}
