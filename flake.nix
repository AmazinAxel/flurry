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
    homeManagerModules.default = import ./hm-module.nix self;
  };
}
