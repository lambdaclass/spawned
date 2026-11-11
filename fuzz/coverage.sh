#!/usr/bin/env bash
# Source coverage of spawned-rt and spawned-concurrency reached by a corpus.
#
#   fuzz/coverage.sh CORPUS_DIR [REPORT_FILE]
#
# Replays every input with `cargo fuzz coverage` (-Cinstrument-coverage) and
# prints llvm-cov's per-file summary for rt/src and concurrency/src only.
set -euo pipefail

corpus=$(realpath "$1")
report=${2:-/dev/stdout}
nightly=${FUZZ_NIGHTLY:-nightly-2026-10-20}
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
export RUSTFLAGS="${RUSTFLAGS:-} --cfg spawned_sim --cfg tokio_unstable"
cargo +"$nightly" fuzz coverage scenario "$corpus" >/dev/null

sysroot=$(rustc +"$nightly" --print sysroot)
host=$(rustc +"$nightly" -vV | sed -n 's/^host: //p')
llvm_cov="$sysroot/lib/rustlib/$host/bin/llvm-cov"
target_dir=${CARGO_TARGET_DIR:-$here/target}
binary="$target_dir/$host/coverage/$host/release/scenario"
"$llvm_cov" report "$binary" \
    -instr-profile="$here/coverage/scenario/coverage.profdata" \
    -ignore-filename-regex='(\.cargo/registry|/rustc/|/fuzz/)' \
    > "$report"
