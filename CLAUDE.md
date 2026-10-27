# flurry map

Astal/gnim GTK4 TSX shell, consumed by borealis. Public repo: no private info, no hardcoded user paths (use GLib dirs).

- `flake.nix`: `packages.default` (`package.nix`, `ags bundle`), `homeManagerModules.default` (`hm-module.nix`, `programs.flurry.{enable,settings,package}`, systemd user service, ags cli for `ags request`).
- Settings are Nix-only: `package.nix` writes `settings.ts` at build; `config.ts` merges defaults. Never runtime config files.
- `app.ts`: entry; imports all CSS + widgets, request handler (sway keybinds call `ags request ...`).
- `widgets/`: status bar (`status/`), `sideview.tsx`, launcher, clipboard, notifications, osd, lockscreen, powermenu, pass, record, focus, emojiPicker, workspaces.
- `lib/`: services/helpers (audio, bluetooth, iwd, brightness, sway, mediaPlayer, wallpaper, idle, power, logind, perMonitor, searchDialog).
- Typecheck: `nix shell nixpkgs#typescript_5 -c tsc --noEmit -p . | rg -v nix/store` (brightness GUdev errors pre-existing).
- Build: `nix build .#default`.
