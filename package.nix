{ lib, rustPlatform, writeText, runCommand, settings ? {}, sideview, pkg-config, makeBinaryWrapper, harfbuzz, fontconfig, woff2, libxkbcommon, dbus, pipewire, pam,
  nerd-fonts, noto-fonts-color-emoji, dejavu_fonts,
  swappy, ffmpeg-headless }: let
  bins = [ swappy ffmpeg-headless ];
  # the one place defaults live: hm-module.nix passes only the options that are set (settings.rs is
  # the dev build's copy)
  s = {
    sideview = {};
    bluetoothNames = {};
    soundNames = {};
    playlists = [];
    mpdCrossfade = 0;
    syncCommand = [];
    fileManager = [ "xdg-open" ];
    imageViewer = [ "xdg-open" ];
    terminal = { command = [ "xdg-terminal-exec" ]; floating = []; tiledWith = []; };
    focusBlocked = [ "discord" ];
    wallpaperDir = "Wallpapers";
    passSyncCommand = [ "pass-sync" ];
    nightLight = { dark = 3500; light = 6500; };
    theme = false;
    notificationFilters = [];
    fonts = [ nerd-fonts.iosevka noto-fonts-color-emoji dejavu_fonts ];
  } // settings;
  # fontconfig runs here, not in the shell: it sorts each family's fallback chain over Sora (vendored
  # for sideview, OFL) and settings.fonts once, and the shell opens the files directly. Only the
  # chosen files are copied out, so the font packages stay build-time only. The families are
  # lib/style.rs's, at the fontconfig weights ui/text.rs asks for (CSS 400, 500, 700).
  fonts = runCommand "flurry-fonts" { nativeBuildInputs = [ woff2 fontconfig.bin ]; } ''
    mkdir -p sora $out/share/fonts
    cp ${./sideview/claude/fonts/Sora.woff2} sora/Sora.woff2
    woff2_decompress sora/Sora.woff2
    rm sora/Sora.woff2
    cat > fonts.conf <<EOF
    <?xml version="1.0"?>
    <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
    <fontconfig>
      <dir>$PWD/sora</dir>
      ${lib.concatMapStrings (p: "<dir>${p}</dir>") s.fonts}
      <include>${fontconfig.out}/etc/fonts/conf.d</include>
      <cachedir>$PWD/cache</cachedir>
    </fontconfig>
    EOF
    export FONTCONFIG_FILE=$PWD/fonts.conf
    # embolden defaults to False: read would collapse an empty field's adjacent tabs
    for family in "Iosevka Nerd Font Mono" Sora; do
      for weight in 80 100 200; do
        fc-match -s --format "$family\t$weight\t%{file}\t%{index}\t%{embolden:-False}\t%{charset}\n" "$family:weight=$weight"
      done
    done > sorted.tsv
    # A fallback stays only for characters no earlier face of its chain has (private use areas
    # aside: icons are baked, not drawn from fonts), and is swapped for an already kept file that
    # has them all (Iosevka Nerd Font in Sora's chain: its Mono twin, not 14 MB more each weight).
    awk -F '\t' -v OFS='\t' '
      $1 "\t" $2 != chain { chain = $1 "\t" $2; delete have; first = 1 }
      {
        delete mine; delete add; na = 0
        n = split($6, ranges, " ")
        for (i = 1; i <= n; i++) {
          k = split(ranges[i], ab, "-"); a = strtonum("0x" ab[1]); b = k > 1 ? strtonum("0x" ab[2]) : a
          for (c = a; c <= b; c++) {
            if ((c >= 57344 && c <= 63743) || c >= 983040) continue
            mine[c] = 1
            if (!(c in have)) add[++na] = c
          }
        }
        if (!first && na == 0) next
        file = $3; index_ = $4; bold = $5
        # only files kept at this weight: their embolden is right for it
        if (!first) {
          for (j = 1; j <= nk && file == $3; j++) {
            f = kept[j]
            if (f == $3 || !((f, $2) in kbold)) continue
            all = 1
            for (i = 1; i <= na && all; i++) if (!((f, add[i]) in cs)) all = 0
            if (all) { file = f; index_ = kidx[f]; bold = kbold[f, $2] }
          }
        }
        first = 0
        if (file == $3) {
          if (!(file in kidx)) {
            kept[++nk] = file; kidx[file] = index_
            for (c in mine) cs[file, c] = 1
          }
          kbold[file, $2] = bold
          for (c in mine) have[c] = 1
        } else {
          for (key in cs) { split(key, p, SUBSEP); if (p[1] == file) have[p[2]] = 1 }
        }
        print $1, $2, file, index_, bold
      }' sorted.tsv > kept.tsv
    declare -A copy
    n=0
    while IFS=$'\t' read -r family weight file index bold; do
      if [ -z "''${copy[$file]:-}" ]; then
        n=$((n + 1))
        copy[$file]=$out/share/fonts/$n-$(basename "$file")
        cp "$file" "''${copy[$file]}"
      fi
      printf '%s\t%s\t%s\t%s\t%s\n' "$family" "$weight" "''${copy[$file]}" "$index" "$bold"
    done < kept.tsv > $out/fonts.tsv
    # store paths can't hold quotes or backslashes, but file names below them could
    awk -F '\t' 'BEGIN { print "pub const FONTS: &[(&str, i32, &str, u32, bool)] = &[" }
      $3 ~ /["\\]/ { exit 1 }
      { printf "    (\"%s\", %d, \"%s\", %d, %s),\n", $1, $2, $3, $4, $5 == "True" ? "true" : "false" }
      END { print "];" }' $out/fonts.tsv > $out/fonts.rs
  '';
  # JSON string escapes are valid Rust ones; control characters are refused rather than escaped
  str = v:
    if builtins.isString v && builtins.match "[^[:cntrl:]]*" v != null then builtins.toJSON v
    else throw "flurry settings: expected a string without control characters, got ${builtins.toJSON v}";
  page = k:
    if builtins.match "[A-Za-z0-9_-]+" k != null then str k
    else throw "flurry settings: sideview page name ${builtins.toJSON k} must match [A-Za-z0-9_-]+";
  list = f: l: "&[${lib.concatMapStringsSep ", " f l}]";
  pairs = key: attrs: list (k: "(${key k}, ${str attrs.${k}})") (lib.attrNames attrs);
  crossfade = assert lib.assertMsg (builtins.isInt s.mpdCrossfade && s.mpdCrossfade >= 0) "flurry settings: mpdCrossfade must be a non-negative int"; s.mpdCrossfade;
  kelvin = k: assert lib.assertMsg (builtins.isInt k && k >= 1000 && k <= 25000) "flurry settings: nightLight temperatures must be ints in 1000..25000"; toString k;
  nightLight = if s.nightLight == false then "None" else "Some((${kelvin s.nightLight.dark}, ${kelvin s.nightLight.light}))";
  theme = if s.theme == false then "None" else "Some((${str s.theme.dark}, ${str s.theme.light}, ${str s.theme.dir}, ${list str (s.theme.signalUnits or [])}))";
  settingsRs = writeText "settings.rs" ''
    // Generated by package.nix from programs.flurry.settings.
    pub const SIDEVIEW: &[(&str, &str)] = ${pairs page s.sideview};
    pub const BLUETOOTH_NAMES: &[(&str, &str)] = ${pairs str s.bluetoothNames};
    pub const SOUND_NAMES: &[(&str, &str)] = ${pairs str s.soundNames};
    pub const PLAYLISTS: &[(&str, &str)] = ${list (p: "(${str p.name}, ${str p.color})") s.playlists};
    pub const MPD_CROSSFADE: u32 = ${toString crossfade};
    pub const SYNC_COMMAND: &[&str] = ${list str s.syncCommand};
    pub const FILE_MANAGER: &[&str] = ${list str s.fileManager};
    pub const IMAGE_VIEWER: &[&str] = ${list str s.imageViewer};
    pub const TERMINAL: &[&str] = ${list str s.terminal.command};
    pub const TERMINAL_FLOATING: &[&str] = ${list str s.terminal.floating};
    pub const TERMINAL_TILED_WITH: &[&str] = ${list str s.terminal.tiledWith};
    pub const FOCUS_BLOCKED: &[&str] = ${list str s.focusBlocked};
    pub const WALLPAPER_DIR: &str = ${str s.wallpaperDir};
    pub const PASS_SYNC_COMMAND: &[&str] = ${list str s.passSyncCommand};
    pub const NIGHT_LIGHT: Option<(u32, u32)> = ${nightLight};
    pub const THEME: Option<(&str, &str, &str, &[&str])> = ${theme};
    pub const NOTIFICATION_FILTERS: &[&str] = ${list str s.notificationFilters};
  '';
in rustPlatform.buildRustPackage {
  pname = "flurry";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./build.rs ./icons ./app.rs ./settings.rs ./lib ./ui ./widgets ./sideview/src/claude.rs ];
  };
  cargoLock.lockFile = ./Cargo.lock;
  doCheck = false; # tests run in the dev shell; skipping them saves a second LTO compile per rebuild

  nativeBuildInputs = [ pkg-config rustPlatform.bindgenHook makeBinaryWrapper ];
  buildInputs = [ harfbuzz libxkbcommon dbus pipewire pam ];

  postPatch = ''
    cat ${settingsRs} ${fonts}/fonts.rs > settings.rs
  '';

  postInstall = ''
    ln -s ${lib.getExe sideview} $out/bin/flurry-sideview
    # a binary wrapper: no bash starts with the shell
    wrapProgram $out/bin/flurry-shell --prefix PATH : "${lib.makeBinPath bins}"
  '';

  passthru = { inherit fonts; }; # the dev shell's FLURRY_FONTS, and `nix build .#default.fonts`
  meta.mainProgram = "flurry-shell";
}
