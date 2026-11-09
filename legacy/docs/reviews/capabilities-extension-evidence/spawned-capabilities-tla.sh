#!/usr/bin/env bash
set -euo pipefail
cd /tmp/spawned-capabilities-20261108
export JAVA_TOOL_OPTIONS='-Xmx768m -XX:ActiveProcessorCount=4'
# Resolve the exact nixpkgs revision from the repository's flake.lock.
nix shell --extra-experimental-features 'nix-command flakes' github:NixOS/nixpkgs/f4a03c008967841f2a0b3cb7c436fc6fc31671f6#tlaplus --command python3 verification/tla/trace_check.py --jobs 1 target/lifecycle-traces
