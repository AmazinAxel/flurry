# flurry map

Rust Wayland shell (no GTK, no async runtime), consumed by borealis. Public repo: no private info, no hardcoded user paths (use `lib/xdg.rs`).

- `flake.nix`: `packages.default` (`package.nix`), `packages.sideview` (`sideview/package.nix`, Servo, built from the separately pinned `nixpkgs-servo` input), `homeManagerModules.default` (`hm-module.nix`, `programs.flurry.{enable,settings,package}`, systemd user service).
- Settings are Nix-only: `package.nix` writes `settings.rs` at build; `config.rs` merges defaults. Never runtime config files.
- `app.rs`: shell entry (`flurry-shell`); `client.rs` is `flurry <cmd> [arg]`, the libc-only request client for sway keybinds (no args execs the shell); request socket in `$XDG_RUNTIME_DIR/flurry.sock`.
- `ui/`: the toolkit: `runtime.rs` (single poll loop, timers, idle, tiny executor), `wayland.rs` (layer/lock surfaces, shm, input), `node.rs` (GTK-like box model + painting), `text.rs` (fontconfig + HarfBuzz), `icons.rs` (baked by `build.rs`), `entry.rs`, `window.rs`.
- `lib/`: services (dbus, logind, iwd, bluetooth, battery, audio=pipewire, notifd, apps, sway, mpd, pam, wallpaper) and shared widgets (searchDialog, inputControl, backgroundSection, deviceMenu); `style.rs` holds the resolved CSS values.
- `widgets/`: one file per window, same layout as the old TS.
- `sideview/`: separate crate `flurry-sideview` (Servo), spawned by `widgets/sideview.rs`, commands over stdin. `flurry:claude` pages are `sideview/claude/` (vendored marked + KaTeX + Sora variable font) talking to `claude -p` stream-json via `src/claude.rs`.
- Dev: `nix-shell` (or `nix develop`), `cargo test`, `cargo clippy`, `cargo build --release`.
- Visual check: `tests/capture/run.sh <flurry-binary> <out-dir>` captures every window in an isolated headless sway (never the live session); compare with `tests/capture/sbs.sh`.
- Build: `nix build .#default`.
