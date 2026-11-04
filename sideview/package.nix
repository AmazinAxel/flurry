# flurry-sideview: Servo embedded in a layer surface. Builds against nixpkgs' servo toolchain and
# inputs (SpiderMonkey needs its python/clang/yasm setup), so it only rebuilds when its own
# sources or the pinned nixpkgs change.
{ lib, rustPlatform, runCommand, writeShellScript, servo, bubblewrap, libxkbcommon, wayland, libGL, vulkan-loader }:

let unwrapped = rustPlatform.buildRustPackage {
  pname = "flurry-sideview";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ];
  };
  # fetchCargoVendor (not cargoLock) so the mozjs workaround below finds $cargoDepsCopy
  cargoHash = "sha256-7pbkBokegpMNhX08lRUaPIxnGlVAMKMGGXERMRwONHQ=";

  nativeBuildInputs = servo.nativeBuildInputs;
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

  # dlopened libraries on the rpath, not LD_LIBRARY_PATH: that would leak into the pages' and
  # Claude Code's child processes
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath [ libxkbcommon wayland libGL vulkan-loader ]} $out/bin/flurry-sideview
  '';

  meta.mainProgram = "flurry-sideview";
};
# the flurry:claude page lives outside the Servo build, so editing it only rewrites the launcher
page = lib.fileset.toSource { root = ./claude; fileset = ./claude; };
# Web content runs in a sandbox: the Nix store, /etc, the GPU, the Wayland socket and the network,
# with its data dir and Claude Code's state (~/.claude, ~/.claude.json, its cwd) the only writable
# user files. No sway IPC, session bus or agents: links open through flurry (the "open" line).
launcher = writeShellScript "flurry-sideview" ''
  run=''${XDG_RUNTIME_DIR:?}
  home=''${HOME:?}
  case ''${WAYLAND_DISPLAY:=wayland-0} in
    /*) wl=$WAYLAND_DISPLAY ;;
    *) wl=$run/$WAYLAND_DISPLAY ;;
  esac
  case ''${XDG_DATA_HOME:-} in
    /*) data=$XDG_DATA_HOME/flurry-sideview ;;
    *) data=$home/.local/share/flurry-sideview ;;
  esac
  mkdir -p -m 700 "$data" "$data/cache" "$run/flurry-claude" "$home/.claude"
  # a bind needs its source: one created inside would be lost with the sandbox
  [ -e "$home/.claude.json" ] || (umask 077 && echo '{}' > "$home/.claude.json")
  exec ${lib.getExe bubblewrap} \
    --unshare-all --share-net --die-with-parent --new-session \
    --unsetenv SWAYSOCK --unsetenv I3SOCK --unsetenv DBUS_SESSION_BUS_ADDRESS \
    --unsetenv SSH_AUTH_SOCK --unsetenv GPG_AGENT_INFO --unsetenv DISPLAY --unsetenv XAUTHORITY \
    --ro-bind /nix/store /nix/store \
    --ro-bind-try /nix/var/nix/profiles /nix/var/nix/profiles \
    --ro-bind /etc /etc \
    --ro-bind /sys /sys \
    --proc /proc \
    --dev /dev \
    --dev-bind-try /dev/dri /dev/dri \
    --dev-bind-try /dev/nvidiactl /dev/nvidiactl \
    --dev-bind-try /dev/nvidia0 /dev/nvidia0 \
    --dev-bind-try /dev/nvidia-modeset /dev/nvidia-modeset \
    --dev-bind-try /dev/nvidia-uvm /dev/nvidia-uvm \
    --tmpfs /tmp \
    --ro-bind-try /run/current-system /run/current-system \
    --ro-bind-try /run/opengl-driver /run/opengl-driver \
    --ro-bind-try /run/systemd/resolve /run/systemd/resolve \
    --ro-bind-try /run/nscd /run/nscd \
    --perms 0700 --dir "$run" \
    --ro-bind "$wl" "$wl" \
    --ro-bind-try "$run/pulse/native" "$run/pulse/native" \
    --bind "$run/flurry-claude" "$run/flurry-claude" \
    --bind "$data" "$data" \
    --bind "$home/.claude" "$home/.claude" \
    --bind "$home/.claude.json" "$home/.claude.json" \
    --ro-bind-try "$home/.nix-profile" "$home/.nix-profile" \
    --ro-bind-try "$home/.local/state/nix/profiles" "$home/.local/state/nix/profiles" \
    --ro-bind-try "$home/.local/bin" "$home/.local/bin" \
    --ro-bind-try "$home/.local/share/claude" "$home/.local/share/claude" \
    --ro-bind-try "$home/.local/share/fonts" "$home/.local/share/fonts" \
    --ro-bind-try "$home/.fonts" "$home/.fonts" \
    --ro-bind-try "$home/.config/fontconfig" "$home/.config/fontconfig" \
    --setenv XDG_CACHE_HOME "$data/cache" \
    --setenv FLURRY_CLAUDE_PAGE ${page} \
    --chdir / \
    -- ${lib.getExe unwrapped} "$@"
'';
in runCommand "flurry-sideview" { meta.mainProgram = "flurry-sideview"; passthru = { inherit unwrapped; }; } ''
  mkdir -p $out/bin
  ln -s ${launcher} $out/bin/flurry-sideview
''
