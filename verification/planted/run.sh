#!/usr/bin/env bash
# Re-run the planted-bug sensitivity matrix (docs/TESTING.md, "Planted-bug
# sensitivity").
#
#   verification/planted/run.sh [-l LAYERS] [-f FUZZ_SECONDS] [-o OUT] [PATCH...]
#
# For each patch in verification/planted/patches (or the ones given), apply it
# to this working tree, run every layer in LAYERS, record whether the layer
# caught the bug and how long it took, then revert the patch. The tree must be
# clean (the patches touch concurrency/src only).
#
# Layers (default TSCF):
#   T  cargo test -p spawned-concurrency --features failpoints (unit,
#      integration, proptest, failpoints; normal Tokio runtime)
#   S  simulated suites: scripts/run-sim-tests.py under --cfg spawned_sim
#   C  fuzz harness tests on stable: committed seeds, 64 generated inputs and
#      the regression inputs (cd fuzz && cargo test --release)
#   F  scenario fuzzer (cargo-fuzz, one process, from fuzz/seeds) for
#      FUZZ_SECONDS (default 300); time is to the first failing input
#   V  TLA+ trace validation of the dedicated trace tests' supervisor
#      lifecycles (needs nix; only a TLC rejection counts)
#
# Output: OUT/results.tsv (patch, layer, verdict, seconds, detail) and one log
# per patch and layer. Verdicts: caught, missed, build (the patch does not
# compile in that configuration: not a detection). The argument `baseline`
# runs the layers on the unpatched tree as a control.
set -uo pipefail

layers=TSCF
fuzz_secs=300
out=planted-out
while getopts "l:f:o:" opt; do
    case $opt in
        l) layers=$OPTARG ;;
        f) fuzz_secs=$OPTARG ;;
        o) out=$OPTARG ;;
        *) sed -n '2,26p' "$0"; exit 2 ;;
    esac
done
shift $((OPTIND - 1))

root=$(git rev-parse --show-toplevel)
here="$root/verification/planted"
nightly=${FUZZ_NIGHTLY:-nightly-2026-10-20}
sim_flags="--cfg spawned_sim --cfg tokio_unstable"
mkdir -p "$out"
out=$(cd "$out" && pwd)
results="$out/results.tsv"
[[ -f $results ]] || printf 'patch\tlayer\tverdict\tseconds\tdetail\n' > "$results"

if [[ $# -eq 0 ]]; then
    set -- "$here"/patches/*.patch
fi
if ! git -C "$root" diff --quiet -- concurrency; then
    echo "concurrency/ has local changes; refusing to plant on top of them" >&2
    exit 2
fi

record() { printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" >> "$results"; }

# Run "$@" with output to $log; sets $status and $secs.
timed() {
    local start=$(date +%s.%N)
    "$@" > "$log" 2>&1
    status=$?
    secs=$(printf '%.0f' "$(echo "$(date +%s.%N) - $start" | bc)")
}

layer_T() {
    timed timeout 1800 cargo test -p spawned-concurrency --features failpoints --no-fail-fast
    if grep -qE '^error(\[E[0-9]+\])?:' "$log" && ! grep -q '^test result' "$log"; then
        verdict=build; detail=""; return
    fi
    local failed
    failed=$(grep -E '^    [A-Za-z0-9_:]+$' "$log" | sed 's/^ *//' | sort -u)
    if [[ $status -eq 124 ]]; then
        verdict=caught; detail="hang (killed after 1800 s)"
    elif [[ $status -ne 0 ]]; then
        verdict=caught
        detail="$(echo "$failed" | grep -c .) failing: $(echo "$failed" | head -4 | tr '\n' ' ')"
    else
        verdict=missed; detail=""
    fi
}

layer_S() {
    timed env RUSTFLAGS="$sim_flags" CARGO_TARGET_DIR="$root/target/sim" \
        python3 "$root/scripts/run-sim-tests.py" -p spawned-concurrency --jobs 4 --timeout 120
    if grep -qE '^error(\[E[0-9]+\])?:' "$log" && ! grep -qE '[0-9]+ passed' "$log"; then
        verdict=build; detail=""; return
    fi
    if [[ $status -ne 0 ]]; then
        verdict=caught
        detail="$(grep -E '^[0-9]+ passed' "$log" | tail -1): $(grep -E '^  [a-z]+: ' "$log" | head -4 | sed 's/^ *//' | tr '\n' ' ')"
    else
        verdict=missed; detail=""
    fi
}

layer_C() {
    timed timeout 1800 bash -c "cd '$root/fuzz' && RUSTFLAGS='$sim_flags' cargo test --release --no-fail-fast"
    if grep -qE '^error(\[E[0-9]+\])?:' "$log" && ! grep -q '^test result' "$log"; then
        verdict=build; detail=""; return
    fi
    if [[ $status -ne 0 ]]; then
        verdict=caught
        detail=$(grep -oE 'FAILED: [A-Za-z0-9 _-]+|^test [a-z_]+ \.\.\. FAILED' "$log" | sort | uniq -c | sort -rn | head -3 | tr -s ' ' | tr '\n' ';')
    else
        verdict=missed; detail=""
    fi
}

layer_F() {
    local corpus="$out/corpus.$name"
    rm -rf "$corpus" && mkdir -p "$corpus" && cp "$root"/fuzz/seeds/* "$corpus"/
    # Build first so the clock covers fuzzing only.
    log="$out/$name.F.build.log"
    (cd "$root/fuzz" && RUSTFLAGS="$sim_flags" cargo +"$nightly" fuzz build -s none scenario) > "$log" 2>&1 \
        || { verdict=build; secs=0; detail=""; return; }
    log="$out/$name.F.log"
    timed bash -c "cd '$root/fuzz' && RUSTFLAGS='$sim_flags' SPAWNED_FUZZ_TAPE_DIR='$out/tapes' \
        cargo +$nightly fuzz run -s none scenario '$corpus' -- -max_total_time=$fuzz_secs \
        -timeout=60 -rss_limit_mb=4096 -max_len=4096 -artifact_prefix='$out/$name.' -print_final_stats=1"
    if grep -q 'spawned-fuzz: FAILED' "$log"; then
        verdict=caught
        local execs
        execs=$(grep -oE '^#[0-9]+' "$log" | tail -1)
        detail="$(grep -m1 -oE 'FAILED: .*' "$log" | cut -c9-90) (execs ${execs:-?})"
    elif grep -qE 'deadly signal|ERROR: libFuzzer' "$log"; then
        verdict=caught
        detail="libFuzzer: $(grep -m1 -oE 'ERROR: libFuzzer: [a-z -]+' "$log")"
    else
        verdict=missed
        detail="$(grep -oE 'stat::number_of_executed_units: *[0-9]+' "$log" | grep -oE '[0-9]+$') execs"
    fi
    rm -rf "$corpus"
}

layer_V() {
    local traces="$out/$name.traces"
    rm -rf "$traces" && mkdir -p "$traces"
    log="$out/$name.V.record.log"
    LIFECYCLE_TRACE_DIR="$traces" cargo test -p spawned-concurrency \
        --features lifecycle-trace,failpoints --no-fail-fast --test supervision_trace \
        --test cohort_trace --test tree_trace --test nested_supervision --test coverage_trace \
        > "$log" 2>&1
    if grep -qE '^error(\[E[0-9]+\])?:' "$log" && ! grep -q '^test result' "$log"; then
        verdict=build; secs=0; detail=""; return
    fi
    log="$out/$name.V.log"
    timed nix develop "$root" -c bash "$root/verification/tla/check.sh" --traces-only "$traces"
    if [[ $status -ne 0 ]] && grep -q REJECTED "$log"; then
        verdict=caught
        detail="$(grep -c REJECTED "$log") of $(ls "$traces" | wc -l) traces rejected"
    elif [[ $status -ne 0 ]]; then
        verdict=error; detail="check.sh failed without a rejection"
    else
        verdict=missed; detail="$(ls "$traces" | wc -l) traces accepted"
    fi
}

for patch in "$@"; do
    name=$(basename "$patch" .patch)
    if [[ $patch == baseline ]]; then
        # Unpatched control run: every layer must report "missed".
        patch=/dev/null
    elif ! git -C "$root" apply "$patch"; then
        record "$name" - apply-failed 0 ""
        continue
    fi
    trap '[[ $patch == /dev/null ]] || git -C "$root" apply -R "$patch"; exit 130' INT TERM
    echo "== $name"
    for ((i = 0; i < ${#layers}; i++)); do
        l=${layers:$i:1}
        log="$out/$name.$l.log"
        verdict=""; secs=0; detail=""
        "layer_$l"
        record "$name" "$l" "$verdict" "$secs" "$detail"
        echo "   $l $verdict ${secs}s $detail"
    done
    [[ $patch == /dev/null ]] || git -C "$root" apply -R "$patch"
    trap - INT TERM
done
