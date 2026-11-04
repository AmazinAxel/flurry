self: { config, lib, pkgs, ... }: let
  cfg = config.programs.flurry;
  inherit (pkgs.stdenv.hostPlatform) system;
in {
  options.programs.flurry = {
    enable = lib.mkEnableOption ''
      flurry shell. System side: `security.pam.services.flurry-auth = {}` for unlocking, and for
      combined output (Shift+Enter in the output selector) a PipeWire null sink named
      `combined-bt-sink` (e.g. via `services.pipewire.extraConfig`); without it a single sink is
      picked'';
    # baked into the binary at build time (package.nix writes settings.rs from these); unset (null)
    # options take package.nix's defaults, the only copy of them
    settings = lib.mkOption {
      default = {};
      description = "Baked into the binary at build time; unset options take package.nix's defaults.";
      type = lib.types.submodule {
        options = with lib.types; {
          sideview = lib.mkOption {
            type = nullOr (attrsOf str);
            default = null;
            description = "Sideview pages: name ([A-Za-z0-9_-]+) -> url. \"assistant\" (flurry:claude) is built in.";
          };
          bluetoothNames = lib.mkOption {
            type = nullOr (attrsOf str);
            default = null;
            description = "Bluetooth display names: advertised name -> shown name.";
          };
          soundNames = lib.mkOption {
            type = nullOr (attrsOf str);
            default = null;
            description = "Sound output display names: advertised name (the sink's description) -> shown name.";
          };
          playlists = lib.mkOption {
            type = nullOr (listOf (submodule {
              options = {
                name = lib.mkOption { type = str; description = "Folder in the music dir."; };
                color = lib.mkOption { type = str; description = "red, yellow, purple, blue1..blue4 (anything else is blue2)."; };
              };
            }));
            default = null;
            description = "mpd playlists in cycle order.";
          };
          mpdCrossfade = lib.mkOption {
            type = nullOr ints.unsigned;
            default = null;
            description = "mpd crossfade seconds set on first play; 0 leaves mpd's setting alone.";
          };
          syncCommand = lib.mkOption {
            type = nullOr (listOf str);
            default = null;
            description = "argv of the weekly \"Sync system\" reminder's action; empty disables the reminder.";
          };
          fileManager = lib.mkOption {
            type = nullOr (listOf str);
            default = null;
            description = "argv prefix opening a folder (the path is appended).";
          };
          imageViewer = lib.mkOption {
            type = nullOr (listOf str);
            default = null;
            description = "argv prefix opening an image (the path is appended).";
          };
          focusBlocked = lib.mkOption {
            type = nullOr (listOf str);
            default = null;
            description = "Launcher app names (case-insensitive substrings) that start only after a focus break and run for at most 5 minutes.";
          };
          wallpaperDir = lib.mkOption {
            type = nullOr str;
            default = null;
            description = "Wallpaper directory; a relative path is under $HOME.";
          };
          passSyncCommand = lib.mkOption {
            type = nullOr (listOf str);
            default = null;
            description = "argv run after saving a pass entry, when argv[0] is on PATH; empty runs nothing.";
          };
          nightLight = lib.mkOption {
            type = nullOr (either (enum [ false ]) (submodule {
              options = {
                dark = lib.mkOption { type = ints.between 1000 25000; description = "Kelvin with the dark scheme."; };
                light = lib.mkOption { type = ints.between 1000 25000; description = "Kelvin with the light scheme."; };
              };
            }));
            default = null;
            description = "wl-gammarelay temperatures following the color scheme; false leaves the temperature alone.";
          };
          notificationFilters = lib.mkOption {
            type = nullOr (listOf str);
            default = null;
            description = "Notifications whose body starts with one of these are dropped.";
          };
          fonts = lib.mkOption {
            type = nullOr (listOf package);
            default = null;
            description = "Font packages fontconfig picks fallbacks from at build time, after the bundled Sora (Iosevka Nerd Font Mono is the monospace face).";
          };
        };
      };
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.default.override { settings = lib.filterAttrs (_: v: v != null) cfg.settings; };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ]; # `flurry <cmd>` is also the request client

    systemd.user.services.flurry = {
      Unit = {
        PartOf = [ "graphical-session.target" ];
        After = [ "graphical-session-pre.target" ];
        X-Restart-Triggers = [ "${cfg.package}" ]; # restart the running shell when a switch changes it
        StartLimitIntervalSec = 0; # the lock screen lives here: never give up restarting
      };
      Service = {
        ExecStart = "${cfg.package}/bin/flurry-shell";
        Restart = "always";
        RestartSec = 1;
        RestartPreventExitStatus = 0; # another shell already holds the lock
        KillMode = "mixed";
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
