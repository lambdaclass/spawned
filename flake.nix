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
        # Apalache (symbolic TLA+ checker, used for inductive invariants) is
        # not in nixpkgs: wrap the upstream release jar with a JDK 21.
        apalache = pkgs.stdenvNoCC.mkDerivation rec {
          pname = "apalache";
          version = "0.62.4";
          src = pkgs.fetchurl {
            url = "https://github.com/apalache-mc/apalache/releases/download/v${version}/apalache-${version}.tgz";
            hash = "sha256-t4G1O7oy+sFKwuArAzLh3C7cZMt7hQwGCNgULMeqLvQ=";
          };
          nativeBuildInputs = [ pkgs.makeWrapper ];
          installPhase = ''
            mkdir -p $out/share/apalache $out/bin
            cp -r bin lib $out/share/apalache/
            patchShebangs $out/share/apalache/bin
            makeWrapper $out/share/apalache/bin/apalache-mc $out/bin/apalache-mc \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.jdk21_headless ]}
          '';
        };
        apalacheTools = [ apalache pkgs.perl pkgs.coreutils ];
      in
      {
        packages.apalache = apalache;

        # `nix develop` provides tlc/tlasany and apalache-mc for verification/tla.
        # Rust itself is managed by rustup (see rust-toolchain / CI).
        devShells.default = pkgs.mkShell {
          packages = verifyTools ++ apalacheTools;
        };

        # `nix run .#check-apalache` checks the inductive invariants
        # (verification/tla/apalache.sh).
        apps.check-apalache = {
          type = "app";
          program = toString (pkgs.writeShellScript "check-apalache" ''
            export PATH=${pkgs.lib.makeBinPath apalacheTools}:$PATH
            exec ${pkgs.bash}/bin/bash ${self}/verification/tla/apalache.sh "$@"
          '');
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
