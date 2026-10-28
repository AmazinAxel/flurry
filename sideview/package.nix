# flurry-sideview: Servo embedded in a layer surface. Builds against nixpkgs' servo toolchain and
# inputs (SpiderMonkey needs its python/clang/yasm setup), so it only rebuilds when its own
# sources or the pinned nixpkgs change.
{ lib, rustPlatform, servo, makeWrapper, libxkbcommon, wayland, libGL, vulkan-loader }:

rustPlatform.buildRustPackage {
  pname = "flurry-sideview";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ];
  };
  # fetchCargoVendor (not cargoLock) so the mozjs workaround below finds $cargoDepsCopy
  cargoHash = "sha256-LTwjPZjFCxQHQHVatUHouxkobxnOcVDWkJsi8kc8b40=";

  nativeBuildInputs = servo.nativeBuildInputs ++ [ makeWrapper ];
  buildInputs = servo.buildInputs ++ [ libxkbcommon ];
  # from nixpkgs servo: mozjs-sys warning flags, writable HOME, no AS override
  env.NIX_CFLAGS_COMPILE = "-Wno-error=format-security";
  preConfigure = ''
    export HOME=$TMPDIR
    unset AS
  '';

  # same mozjs/cbindgen `cargo metadata` workaround as nixpkgs' servo
  # after cargoSetupHook has written .cargo/config.toml
  preBuild = ''
    vendor=''${cargoDepsCopy:-$(echo $NIX_BUILD_TOP/*-vendor)}
    mkdir -p $vendor/.cargo
    cp $NIX_BUILD_TOP/.cargo/config.toml $vendor/.cargo/config.toml
    for mozjs_dir in $vendor/*/mozjs_*/ $vendor/mozjs_*/; do
      [ -d "$mozjs_dir" ] || continue
      cp Cargo.lock $mozjs_dir
    done
  '';

  postFixup = ''
    wrapProgram $out/bin/flurry-sideview \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ libxkbcommon wayland libGL vulkan-loader ]}
  '';

  meta.mainProgram = "flurry-sideview";
}
