#!/usr/bin/env bash
set -euo pipefail
ROOT=/tmp/spawned-fede-research-25dGGmPA
source "$ROOT/env.sh"
cd "$ROOT/isolation-source"
git fetch "$ROOT/controller/runtime-isolation-source-final.bundle" HEAD > "$ROOT/isolation-v10/fetch-final.log" 2>&1
git checkout --detach a3cb5eb15d68f2e21a5d86eb19c84686e747547c >> "$ROOT/isolation-v10/fetch-final.log" 2>&1
export CARGO_TARGET_DIR="$ROOT/fairness-target"
cargo bench --locked -p spawned-concurrency --features backend-experiments --bench runtime_isolation --no-run --message-format=json > "$ROOT/isolation-v10/build.jsonl" 2> "$ROOT/isolation-v10/build-final.log"
RUSTC=/nix/store/xvp6nfxayb07si2jaggqwvx3iykw89g2-rustc-1.98.1/bin/rustc RUSTDOC=/nix/store/xvp6nfxayb07si2jaggqwvx3iykw89g2-rustc-1.98.1/bin/rustdoc CARGO_TARGET_DIR="$ROOT/fairness-clippy" cargo clippy --locked -p spawned-concurrency --features backend-experiments --bench runtime_isolation -- -D warnings > "$ROOT/isolation-v10/clippy.log" 2>&1
printf 'passed\n' > "$ROOT/isolation-v10/gates-state"
python3 "$ROOT/controller/isolation10.py" > "$ROOT/isolation-v10/run.log" 2>&1
