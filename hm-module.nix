self: { config, lib, pkgs, ... }: let
  cfg = config.programs.flurry;
  inherit (pkgs.stdenv.hostPlatform) system;
in {
  options.programs.flurry = {
    enable = lib.mkEnableOption "flurry shell";
    settings = lib.mkOption {
      type = lib.types.attrs;
      default = {};
      description = "Baked into the bundle at build time.";
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.default.override { inherit (cfg) settings; };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package self.inputs.ags.packages.${system}.default ]; # ags cli for requests

    systemd.user.services.flurry = {
      Unit = {
        PartOf = [ "graphical-session.target" ];
        After = [ "graphical-session-pre.target" ];
      };
      Service = {
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        KillMode = "mixed";
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
