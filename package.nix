{ lib, rustPlatform, settings ? {}, sideview, pkg-config, makeWrapper, harfbuzz, fontconfig, libxkbcommon, dbus, pipewire, pam,
  iwd, cliphist, wl-clipboard, swappy, ffmpeg-headless }: let
  bins = [ iwd cliphist wl-clipboard swappy ffmpeg-headless ];
  rustString = builtins.toJSON; # urls are plain ASCII, valid as Rust string literals
  pages = lib.mapAttrsToList (k: v: "(${rustString k}, ${rustString v})") (settings.sideview or {});
in rustPlatform.buildRustPackage {
  pname = "flurry";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./build.rs ./icons ./app.rs ./client.rs ./config.rs ./settings.rs ./lib ./ui ./widgets ];
  };
  cargoLock.lockFile = ./Cargo.lock;
  doCheck = false; # tests run in the dev shell; skipping them saves a second LTO compile per rebuild

  nativeBuildInputs = [ pkg-config rustPlatform.bindgenHook makeWrapper ];
  buildInputs = [ harfbuzz fontconfig libxkbcommon dbus pipewire pam ];


  postPatch = ''
    cat > settings.rs <<'EOF'
    pub const SIDEVIEW: &[(&str, &str)] = &[${lib.concatStringsSep ", " pages}];
    EOF
  '';

  postInstall = ''
    ln -s ${lib.getExe sideview} $out/bin/flurry-sideview
    # only the shell needs the tools; the client stays an unwrapped binary (no bash per keybind)
    wrapProgram $out/bin/flurry-shell --prefix PATH : "${lib.makeBinPath bins}"
  '';

  meta.mainProgram = "flurry";
}
