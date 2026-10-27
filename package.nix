{ lib, stdenv, ags, astal, settings ? {}, wrapGAppsHook4, gobject-introspection, gjs, webkitgtk_6_0, glib-networking, libgudev, iwd, cliphist, wl-clipboard, swappy, ffmpeg-headless }: let
  inherit (stdenv.hostPlatform) system;
  libs = (with astal.packages.${system}; [ io astal4 apps auth battery bluetooth mpris notifd wireplumber wl4 ])
    ++ [ webkitgtk_6_0 glib-networking libgudev ];
  bins = [ iwd cliphist wl-clipboard swappy ffmpeg-headless ];
in stdenv.mkDerivation {
  name = "flurry";
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./app.ts ./env.d.ts ./config.ts ./settings.ts ./style.css ./lib ./widgets ];
  };

  nativeBuildInputs = [ wrapGAppsHook4 gobject-introspection ags.packages.${system}.default ];
  buildInputs = libs ++ [ gjs ];

  postPatch = ''
    echo ${lib.escapeShellArg "export default ${builtins.toJSON settings};"} > settings.ts
  '';

  installPhase = ''
    mkdir -p $out/bin
    ags bundle app.ts $out/bin/flurry
  '';

  preFixup = ''
    gappsWrapperArgs+=(--prefix PATH : "${lib.makeBinPath bins}")
  '';

  meta.mainProgram = "flurry";
}
