{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # pinned separately so Servo (a long build) only rebuilds when this is bumped on purpose
    nixpkgs-servo.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, nixpkgs-servo }: let
    systems = [ "x86_64-linux" "aarch64-linux" ];
    forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system} nixpkgs-servo.legacyPackages.${system});
  in {
    packages = forAll (pkgs: servoPkgs: rec {
      sideview = servoPkgs.callPackage ./sideview/package.nix { };
      default = pkgs.callPackage ./package.nix { inherit sideview; };
    });
    devShells = forAll (pkgs: _: { default = import ./shell.nix { inherit pkgs; }; });
    # `nix run .#dev` from the repo: build it, swap it in for the installed service until it exits
    apps = forAll (pkgs: _: {
      dev = {
        type = "app";
        program = nixpkgs.lib.getExe (pkgs.writeShellApplication {
          name = "flurry-dev";
          text = ''
            [ -f Cargo.toml ] || { echo "run from the flurry repo" >&2; exit 1; }
            nix develop -c cargo build --release
            systemctl --user stop flurry
            trap 'systemctl --user start flurry' EXIT
            target/release/flurry-shell
          '';
        });
      };
    });
    homeManagerModules.default = import ./hm-module.nix self;
  };
}
