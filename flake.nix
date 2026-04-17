# see flake schema: https://nixos.wiki/wiki/flakes
{
  description = "lazydc development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        src = builtins.path {
          path = ./.;
          name = "lazydc-src";
        };
        lazydc = pkgs.rustPlatform.buildRustPackage {
          pname = "lazydc";
          version = "0.1.0";
          inherit src;
          cargoLock = {
            lockFileContents = builtins.readFile ./Cargo.lock;
          };
          postPatch = ''
            cp ${pkgs.writeText "Cargo.lock" (builtins.readFile ./Cargo.lock)} Cargo.lock
          '';
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          meta = with pkgs.lib; {
            description = "TUI and CLI utility for managing docker compose homelab projects";
            mainProgram = "lazydc";
            license = licenses.mit;
            platforms = platforms.linux ++ platforms.darwin;
          };
        };
      in
      {
        formatter = pkgs.nixfmt;
        packages = {
          default = lazydc;
          inherit lazydc;
        };
        apps = {
          default = {
            type = "app";
            program = "${lazydc}/bin/lazydc";
          };
          lazydc = {
            type = "app";
            program = "${lazydc}/bin/lazydc";
          };
        };
        checks = {
          inherit lazydc;
        };
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            pkg-config
          ];
        };
      }
    );
}
