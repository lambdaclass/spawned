#!/usr/bin/env bash
# Validate the supervisor lifecycles of a corpus sample against the TLA+ models.
#
#   fuzz/tla-sample.sh CORPUS_DIR [MAX_TRACES] [OUT_DIR]
#
# Replays every input with the `lifecycle-trace` feature, writes one trace per
# finished flat supervisor tree (OneForOne: Supervision.tla; OneForAll and
# RestForOne: Cohorts.tla), keeps at most MAX_TRACES (default 200) and runs
# verification/tla/trace_check.py on them (needs TLC, e.g. `nix develop`).
set -euo pipefail
corpus=$(realpath "$1")
max=${2:-200}
out=$(realpath -m "${3:-/tmp/spawned-fuzz-tla}")
here=$(cd "$(dirname "$0")" && pwd)
export RUSTFLAGS="${RUSTFLAGS:-} --cfg spawned_sim --cfg tokio_unstable"
cd "$here"
cargo build --release -q --example repro --features lifecycle-trace
repro="${CARGO_TARGET_DIR:-$here/target}/release/examples/repro"
rm -rf "$out" && mkdir -p "$out"
for f in "$corpus"/*; do
    SPAWNED_FUZZ_TLA_DIR="$out" "$repro" "$f" > /dev/null 2>&1 || true
done
ls "$out"/*.json 2>/dev/null | tail -n +"$((max + 1))" | xargs -r rm -f
echo "$(ls "$out" | wc -l) traces"
python3 "$here/../verification/tla/trace_check.py" --jobs "${JOBS:-2}" "$out"
