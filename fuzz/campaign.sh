#!/usr/bin/env bash
# Run a scenario-fuzzing campaign and record coverage growth.
#
#   fuzz/campaign.sh SECONDS JOBS OUT_DIR
#
# Builds the `scenario` target with the pinned nightly (FUZZ_NIGHTLY,
# default below), fuzzes for SECONDS in libFuzzer's fork mode with JOBS
# worker processes sharing OUT_DIR/corpus (seeded from fuzz/seeds; crashes
# do not stop the campaign), and samples the latest `cov:`/`ft:`/`exec/s`
# line once a minute into OUT_DIR/coverage.csv.
# Crashes land in OUT_DIR/artifacts, their decision tapes in OUT_DIR/tapes.
# Never scheduled: run it by hand (or from the workflow_dispatch job).
set -euo pipefail

seconds=${1:-3600}
jobs=${2:-4}
out=$(realpath -m "${3:-fuzz-campaign}")
nightly=${FUZZ_NIGHTLY:-nightly-2026-10-20}
here=$(cd "$(dirname "$0")" && pwd)

mkdir -p "$out"/corpus "$out"/artifacts "$out"/tapes "$out"/logs
cp -n "$here"/seeds/* "$out"/corpus/ 2>/dev/null || true
cd "$here"
export RUSTFLAGS="${RUSTFLAGS:-} --cfg spawned_sim --cfg tokio_unstable"
export SPAWNED_FUZZ_TAPE_DIR="$out/tapes"
cargo +"$nightly" fuzz build -s none scenario

start=$(date +%s)
cargo +"$nightly" fuzz run -s none scenario "$out/corpus" -- \
    -max_total_time="$seconds" -fork="$jobs" \
    -ignore_crashes=1 -ignore_timeouts=1 -ignore_ooms=1 \
    -timeout=60 -rss_limit_mb=4096 -max_len=4096 \
    -artifact_prefix="$out/artifacts/" -print_final_stats=1 \
    > "$out/logs/fuzz.log" 2>&1 &
fuzzer=$!

echo "elapsed_s,execs,cov,ft,corp,exec_per_s,crashes" > "$out/coverage.csv"
while kill -0 "$fuzzer" 2>/dev/null; do
    sleep 60
    now=$(( $(date +%s) - start ))
    grep -E '^#[0-9]+: cov: ' "$out/logs/fuzz.log" | tail -1 | sed -E \
        "s/^#([0-9]+): cov: ([0-9]+) ft: ([0-9]+) corp: ([0-9]+) exec\/s: ([0-9]+) oom\/timeout\/crash: [0-9]+\/[0-9]+\/([0-9]+).*/$now,\1,\2,\3,\4,\5,\6/" \
        >> "$out/coverage.csv" || true
done
wait "$fuzzer" || true
echo "campaign done: $(ls "$out"/artifacts | wc -l) artifacts, corpus $(ls "$out"/corpus | wc -l)"
