#!/usr/bin/env bash
# Release fuzz campaign (docs/TESTING.md, "Release fuzz campaign"): fuzz for
# hours, minimize every failure and turn each into a regression test, and
# keep a small corpus for the next campaign.
#
#   fuzz/release-campaign.sh [-t SECONDS] [-j JOBS] [-o OUT] [-c CORPUS] [-k PER_KIND]
#   fuzz/release-campaign.sh -T -o OUT      # triage an earlier run only
#
#   -t  fuzzing time in seconds (default 14400, four hours)
#   -j  libFuzzer fork-mode workers (default 8)
#   -o  output directory (default ~/spawned-work/fuzz-release-<date>)
#   -c  persistent corpus directory: read at the start, replaced by the
#       campaign's minimized corpus at the end (default ~/spawned-work/fuzz-corpus)
#   -k  failing inputs minimized per distinct failure (default 3); one
#       regression is emitted per distinct failure
#   -T  skip fuzzing: triage OUT/artifacts again (after a harness fix, say)
#
# Steps:
#   1. campaign.sh: fork mode with JOBS workers from fuzz/seeds plus CORPUS;
#      crashes, timeouts and OOMs are kept in OUT/artifacts, coverage in
#      OUT/coverage.csv.
#   2. Triage: every artifact is replayed with `repro` (no libFuzzer) and
#      grouped by the failure it reproduces (the oracle's name).
#      OUT/triage.tsv lists them; a timeout or OOM that does not reproduce
#      under the simulator is reported, not emitted.
#   3. For each distinct failure, the PER_KIND smallest inputs are minimized
#      (`cargo fuzz tmin`, bounded by MIN_RUNS runs and MIN_TIMEOUT seconds);
#      a minimized input that fails differently is dropped for its original.
#      The smallest one becomes fuzz/regressions/campaign_<oracle>_<hash>.input
#      with its shrunk decision tape, and fuzz/tests/campaign_<...>.rs, which
#      replays that tape strictly (permissively once a fix changes the task
#      structure) and sweeps 32 more seeds. The test fails until the bug or
#      the oracle is fixed; commit it with the fix.
#   4. The corpus is minimized (`cargo fuzz cmin`) and replaces CORPUS.
#   5. OUT/summary.txt: commit, host, time, executions, coverage, failures
#      and emitted regressions. Link it in the release PR.
#
# Runs by hand only (no cron, timer or scheduled CI). Use a quiet host: on
# fede, under ~/spawned-work, with CARGO_INCREMENTAL=0; delete fuzz/target
# afterwards (about 3 GB).
set -euo pipefail

seconds=14400
jobs=8
out=""
corpus=${FUZZ_CORPUS:-$HOME/spawned-work/fuzz-corpus}
per_kind=3
triage_only=0
while getopts "t:j:o:c:k:T" opt; do
    case $opt in
        t) seconds=$OPTARG ;;
        j) jobs=$OPTARG ;;
        o) out=$OPTARG ;;
        c) corpus=$OPTARG ;;
        k) per_kind=$OPTARG ;;
        T) triage_only=1 ;;
        *) sed -n '2,40p' "$0"; exit 2 ;;
    esac
done
out=$(realpath -m "${out:-$HOME/spawned-work/fuzz-release-$(date +%Y%m%d-%H%M)}")
corpus=$(realpath -m "$corpus")
nightly=${FUZZ_NIGHTLY:-nightly-2026-10-20}
min_runs=${MIN_RUNS:-3000}
min_timeout=${MIN_TIMEOUT:-900}
here=$(cd "$(dirname "$0")" && pwd)
export CARGO_INCREMENTAL=0
export RUSTFLAGS="${RUSTFLAGS:-} --cfg spawned_sim --cfg tokio_unstable"
mkdir -p "$out" "$out/min"
summary="$out/summary.txt"

if [[ $triage_only -eq 0 ]]; then
    started=$(date -u +%FT%TZ)
    FUZZ_CORPUS="$corpus" "$here/campaign.sh" "$seconds" "$jobs" "$out"
    {
        echo "commit   $(git -C "$here" rev-parse HEAD)$(git -C "$here" diff --quiet || echo ' (dirty)')"
        echo "host     $(hostname) ($(nproc 2>/dev/null || echo '?') cpus)"
        echo "started  $started"
        echo "finished $(date -u +%FT%TZ)"
        echo "fuzzing  ${seconds}s, $jobs workers, nightly $nightly"
        last=$(tail -1 "$out/coverage.csv")
        echo "last sample (elapsed_s,execs,cov,ft,corp,exec_per_s,crashes): $last"
        grep -E '^stat::' "$out/logs/fuzz.log" | tail -8 || true
    } > "$summary"
fi

cd "$here"
cargo +"$nightly" build --release --locked -q --example repro
repro="$here/target/release/examples/repro"

# Failure of one input under the simulator: the oracle line, or PASS.
outcome() {
    local log
    log=$(SPAWNED_SIM_SHRINK=0 timeout 900 "$repro" "$1" 2>&1) || true
    if grep -q '^PASS' <<<"$log"; then
        echo PASS
    elif grep -q '^FAIL: ' <<<"$log"; then
        grep -m1 '^FAIL: ' <<<"$log" | sed 's/^FAIL: //'
    else
        echo "no verdict (timeout or crash of repro)"
    fi
}

# A short identifier for a failure: the contract code before the first ':'
# of the oracle name (V3, X1, R1, liveness, ...), or `other`.
slug() {
    local code
    code=$(sed -nE 's/.*oracle: ([^:]+):.*/\1/p' <<<"$1" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9\n' '_' | sed 's/_*$//;s/__*/_/g')
    echo "${code:-other}"
}

triage="$out/triage.tsv"
printf 'artifact\tbytes\tfailure\n' > "$triage"
shopt -s nullglob
for f in "$out"/artifacts/*; do
    printf '%s\t%s\t%s\n' "$(basename "$f")" "$(stat -c %s "$f" 2>/dev/null || stat -f %z "$f")" "$(outcome "$f")" >> "$triage"
done

emitted=()
unreproduced=0
while IFS= read -r failure; do
    [[ -z $failure ]] && continue
    if [[ $failure == PASS || $failure == no\ verdict* ]]; then
        unreproduced=$(( unreproduced + $(awk -F'\t' -v k="$failure" 'NR > 1 && $3 == k' "$triage" | wc -l) ))
        continue
    fi
    best=""
    best_size=0
    # The PER_KIND smallest inputs with this failure.
    while IFS=$'\t' read -r name _; do
        src="$out/artifacts/$name"
        dst="$out/min/$name"
        timeout "$min_timeout" cargo +"$nightly" fuzz tmin -s none scenario "$src" -- \
            -runs="$min_runs" -timeout=60 -rss_limit_mb=4096 \
            -exact_artifact_path="$dst" > "$out/min/$name.log" 2>&1 || true
        if [[ ! -s $dst || "$(outcome "$dst")" != "$failure" ]]; then
            cp "$src" "$dst"
        fi
        size=$(stat -c %s "$dst" 2>/dev/null || stat -f %z "$dst")
        if [[ -z $best || $size -lt $best_size ]]; then
            best=$dst
            best_size=$size
        fi
    done < <(awk -F'\t' -v k="$failure" 'NR > 1 && $3 == k {print $1 "\t" $2}' "$triage" | sort -t$'\t' -k2,2n | head -n "$per_kind")
    hash=$(sha1sum "$best" 2>/dev/null | cut -c1-8 || shasum "$best" | cut -c1-8)
    name="campaign_$(slug "$failure")_$hash"
    "$repro" "$best" --emit "$name" > "$out/min/$name.repro.log" 2>&1 || true
    emitted+=("fuzz/tests/$name.rs ($best_size bytes): $failure")
done < <(awk -F'\t' 'NR > 1 {print $3}' "$triage" | sort -u)

# Keep a small corpus: libFuzzer's minimal subset with the same coverage.
if [[ -d $out/corpus ]] && [[ -n "$(ls -A "$out/corpus")" ]]; then
    before=$(ls "$out/corpus" | wc -l)
    cargo +"$nightly" fuzz cmin -s none scenario "$out/corpus" -- \
        -timeout=60 -rss_limit_mb=4096 > "$out/logs/cmin.log" 2>&1 || true
    after=$(ls "$out/corpus" | wc -l)
    mkdir -p "$corpus"
    rm -f "$corpus"/*
    cp "$out/corpus"/* "$corpus"/
    corpus_line="corpus   $before inputs minimized to $after, saved to $corpus ($(du -sh "$corpus" | cut -f1))"
else
    corpus_line="corpus   none"
fi

{
    echo "artifacts $(($(wc -l < "$triage") - 1)) ($unreproduced do not reproduce under the simulator)"
    echo "distinct failures ${#emitted[@]}"
    for e in "${emitted[@]}"; do
        echo "  $e"
    done
    echo "$corpus_line"
} >> "$summary"
cat "$summary"
[[ ${#emitted[@]} -eq 0 ]]
