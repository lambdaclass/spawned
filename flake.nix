{
  description = "Spawned verification tooling (TLA+ model checking)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        # TLC runs on the JVM; nixpkgs' tlaplus wraps it with its own JDK.
        verifyTools = [ pkgs.tlaplus pkgs.python3 ];
      in
      {
        # `nix develop` provides tlc/tlasany for verification/tla.
        # Rust itself is managed by rustup (see rust-toolchain / CI).
        devShells.default = pkgs.mkShell {
          packages = verifyTools;
        };

        # `nix run .#check-tla` model-checks every MC*.cfg and validates
        # recorded traces (see verification/tla/README.md).
        apps.check-tla = {
          type = "app";
          program = toString (pkgs.writeShellScript "check-tla" ''
            export PATH=${pkgs.lib.makeBinPath verifyTools}:$PATH
            exec ${pkgs.bash}/bin/bash ${self}/verification/tla/check.sh "$@"
          '');
        };
      });
}
