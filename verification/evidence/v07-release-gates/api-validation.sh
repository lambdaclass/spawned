#!/usr/bin/env bash
set -euo pipefail
cd /tmp/spawned-v07-api-gate-repo
(
 source /tmp/spawned-fede-research-25dGGmPA/env.sh
 export CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=1 CARGO_TARGET_DIR=/tmp/spawned-pr-security-review/system-target
 export PATH=/nix/store/d8wrx2mvxd1c6m8fmljq2mcmpprbs410-cargo-hack-0.6.46/bin:$PATH
 python3 scripts/check-api.py features --offline > /tmp/spawned-v07-api-final-features.log 2>&1
)
export CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=1 CARGO_TARGET_DIR=/tmp/spawned-v07-api-target
nix shell --extra-experimental-features 'nix-command flakes' github:NixOS/nixpkgs/f4a03c008967841f2a0b3cb7c436fc6fc31671f6#cargo-semver-checks github:NixOS/nixpkgs/f4a03c008967841f2a0b3cb7c436fc6fc31671f6#rustc github:NixOS/nixpkgs/f4a03c008967841f2a0b3cb7c436fc6fc31671f6#cargo --command python3 scripts/check-api.py semver > /tmp/spawned-v07-api-final-semver.log 2>&1
