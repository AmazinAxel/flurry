# flurry map

Rust Wayland shell (no GTK, no async runtime), consumed by borealis. Public repo: no private info, no hardcoded user paths (use `lib/xdg.rs`).

- `flake.nix`: `packages.default` (`package.nix`), `packages.sideview` (`sideview/package.nix`, Servo, built from the separately pinned `nixpkgs-servo` input), `homeManagerModules.default` (`hm-module.nix`, `programs.flurry.{enable,settings,package}`, systemd user service).
- Settings are Nix-only: `package.nix` writes `settings.rs` at build (validated at eval) and holds the only defaults (unset hm options are null); `config.rs` merges defaults. Never runtime config files. `programs.flurry.settings`: sideview, bluetoothNames, playlists ({name,color}), mpdCrossfade, syncCommand, fileManager, imageViewer, notificationFilters (argvs are lists), fonts (fallback font packages).
- System side (not in the module): `security.pam.services.flurry-auth = {}` is required for unlocking (optional `flurry-fprint` for fingerprint). Runtime tools not wrapped into PATH: gpu-screen-recorder, pass (and whatever the settings' argvs name).
- `app.rs`: shell entry (`flurry-shell`); `client.rs` is `flurry <cmd> [arg]`, the libc-only request client for sway keybinds (no args execs the shell); one datagram to `$XDG_RUNTIME_DIR/flurry.sock` (path from `lib/xdg.rs`, shared by both).
- `ui/`: the toolkit: `runtime.rs` (single poll loop, timers, idle, tiny executor), `wayland.rs` (layer/lock surfaces, shm, input), `node.rs` (GTK-like box model + painting), `text.rs` (HarfBuzz; no fontconfig at runtime: `package.nix` runs `fc-match` at build over Sora, decompressed from sideview's vendored woff2, plus `settings.fonts` into `settings.rs` `FONTS`; the dev shell passes the same table as `FLURRY_FONTS`), `icons.rs` (baked by `build.rs`), `entry.rs`, `window.rs`.
- `lib/`: services (dbus, logind, iwd, bluetooth, battery, audio=pipewire, notifd, apps, sway, mediaPlayer=mpd, pam, wallpaper) and shared widgets (searchDialog, inputControl, backgroundSection, deviceMenu); `style.rs` holds the resolved CSS values.
- `widgets/`: one file per window, same layout as the old TS.
- `sideview/`: separate crate `flurry-sideview` (Servo), spawned by `widgets/sideview.rs`, commands over stdin. `flurry:claude` pages are `sideview/claude/` (vendored marked + KaTeX + Sora variable font) talking to `claude -p` stream-json via `src/claude.rs`.
- Dev: `nix-shell` (or `nix develop`), `cargo test`, `cargo clippy`, `cargo build --release`.
- Visual check: `tests/capture/run.sh <flurry-client> <out-dir>` (the `flurry` binary; it starts the sibling `flurry-shell`) captures every window in an isolated headless sway (never the live session); compare with `tests/capture/sbs.sh`.
- Build: `nix build .#default`.
