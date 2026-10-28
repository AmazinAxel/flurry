{ pkgs ? import <nixpkgs> {} }: pkgs.mkShell {
  nativeBuildInputs = with pkgs; [ cargo rustc clippy rustfmt pkg-config rustPlatform.bindgenHook ];
  buildInputs = with pkgs; [ harfbuzz fontconfig libxkbcommon dbus pipewire pam ];
}
