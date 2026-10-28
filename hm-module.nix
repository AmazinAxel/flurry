self: { config, lib, pkgs, ... }: let
  cfg = config.programs.flurry;
  inherit (pkgs.stdenv.hostPlatform) system;
in {
  options.programs.flurry = {
    enable = lib.mkEnableOption "flurry shell";
    settings = lib.mkOption {
      type = lib.types.attrs;
      default = {};
      description = "Baked into the binary at build time.";
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
