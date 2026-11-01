self: { config, lib, pkgs, ... }: let
  cfg = config.programs.flurry;
  inherit (pkgs.stdenv.hostPlatform) system;
in {
  options.programs.flurry = {
    enable = lib.mkEnableOption "flurry shell";
    # baked into the binary at build time (package.nix writes settings.rs from these)
    settings = lib.mkOption {
      default = {};
      description = "Baked into the binary at build time.";
      type = lib.types.submodule {
        options = with lib.types; {
          sideview = lib.mkOption {
            type = attrsOf str;
            default = {};
            description = "Sideview pages: name ([A-Za-z0-9_-]+) -> url. \"assistant\" (flurry:claude) is built in.";
          };
          bluetoothNames = lib.mkOption {
            type = attrsOf str;
            default = {};
            description = "Bluetooth display names: advertised name -> shown name.";
          };
          playlists = lib.mkOption {
            type = listOf (submodule {
              options = {
                name = lib.mkOption { type = str; description = "Folder in the music dir."; };
                color = lib.mkOption { type = str; description = "red, yellow, purple, blue1..blue4 (anything else is blue2)."; };
              };
            });
            default = [];
            description = "mpd playlists in cycle order.";
          };
          mpdCrossfade = lib.mkOption {
            type = ints.unsigned;
            default = 0;
            description = "mpd crossfade seconds set on first play; 0 leaves mpd's setting alone.";
          };
          syncCommand = lib.mkOption {
            type = listOf str;
            default = [];
            description = "argv of the weekly \"Sync system\" reminder's action; empty disables the reminder.";
          };
          fileManager = lib.mkOption {
            type = listOf str;
            default = [ "xdg-open" ];
            description = "argv prefix opening a folder (the path is appended).";
          };
          imageViewer = lib.mkOption {
            type = listOf str;
            default = [ "xdg-open" ];
            description = "argv prefix opening an image (the path is appended).";
          };
          notificationFilters = lib.mkOption {
            type = listOf str;
            default = [];
            description = "Notifications whose body starts with one of these are dropped.";
          };
          fonts = lib.mkOption {
            type = listOf package;
            default = with pkgs; [ nerd-fonts.iosevka noto-fonts-color-emoji dejavu_fonts ];
            description = "Font packages fontconfig picks fallbacks from at build time, after the bundled Sora (Iosevka Nerd Font Mono is the monospace face).";
          };
        };
      };
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.default.override { inherit (cfg) settings; };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ]; # `flurry <cmd>` is also the request client

    systemd.user.services.flurry = {
      Unit = {
        PartOf = [ "graphical-session.target" ];
        After = [ "graphical-session-pre.target" ];
        X-Restart-Triggers = [ "${cfg.package}" ]; # restart the running shell when a switch changes it
      };
      Service = {
        ExecStart = "${cfg.package}/bin/flurry-shell";
        Restart = "on-failure";
        KillMode = "mixed";
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
