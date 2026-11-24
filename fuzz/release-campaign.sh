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
#      under the simulator is reported, not emitted. A replay that hangs
#      (REPRO_TIMEOUT, 900 s) or dies without an oracle verdict is
#      UNRESOLVED and fails the gate.
#   3. For each distinct failure, the PER_KIND smallest inputs are minimized
#      (`cargo fuzz tmin`, bounded by MIN_RUNS runs and MIN_TIMEOUT seconds);
#      a minimized input that fails differently is dropped for its original.
#      The smallest one becomes fuzz/regressions/campaign_<oracle>_<hash>.input
#      with its shrunk decision tape, and fuzz/tests/campaign_<...>.rs, which
#      replays that tape strictly (permissively once a fix changes the task
#      structure) and sweeps 32 more seeds. The test fails until the bug or
#      the oracle is fixed; commit it with the fix.
#   4. A copy of the corpus is minimized (`cargo fuzz cmin`) and replaces
#      CORPUS only if that succeeded and kept inputs; -T leaves CORPUS alone.
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
    # libFuzzer exits non-zero when it wrote artifacts: triage them anyway.
    fuzz_status=0
    FUZZ_CORPUS="$corpus" "$here/campaign.sh" "$seconds" "$jobs" "$out" || fuzz_status=$?
    {
        echo "commit   $(git -C "$here" rev-parse HEAD)$([[ -z $(git -C "$here" status --porcelain --untracked-files=no) ]] || echo ' (dirty)')"
        echo "host     $(hostname) ($(nproc 2>/dev/null || echo '?') cpus)"
        echo "started  $started"
        echo "finished $(date -u +%FT%TZ)"
        echo "fuzzing  ${seconds}s, $jobs workers, nightly $nightly (campaign.sh exit $fuzz_status)"
        last=$(tail -1 "$out/coverage.csv")
        echo "last sample (elapsed_s,execs,cov,ft,corp,exec_per_s,crashes): $last"
        grep -E '^stat::' "$out/logs/fuzz.log" | tail -8 || true
    } > "$summary"
fi

cd "$here"
cargo +"$nightly" build --release --locked -q --example repro
repro="$here/target/release/examples/repro"

repro_timeout=${REPRO_TIMEOUT:-900}

# Failure of one input under the simulator: the oracle line, PASS, or
# `UNRESOLVED: ...` when `repro` hung or died without a verdict (a hang or a
# crash outside the oracles is a failure too: it fails the gate).
outcome() {
    local log status=0
    log=$(SPAWNED_SIM_SHRINK=0 timeout "$repro_timeout" "$repro" "$1" 2>&1 </dev/null) || status=$?
    if grep -q '^FAIL: ' <<<"$log"; then
        grep -m1 '^FAIL: ' <<<"$log" | sed 's/^FAIL: //'
    elif [[ $status -eq 0 ]] && grep -q '^PASS' <<<"$log"; then
        echo PASS
    elif [[ $status -eq 124 ]]; then
        echo "UNRESOLVED: repro did not finish in ${repro_timeout}s"
    else
        echo "UNRESOLVED: repro exited $status: $(grep -v '^[[:space:]]*$' <<<"$log" | tail -1 | tr '\t' ' ' | cut -c1-200)"
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
    printf '%s\t%s\t%s\n' "$(basename "$f")" "$(stat -c %s "$f" 2>/dev/null || stat -f %z "$f")" "$(outcome "$f" | tr '\t' ' ')" >> "$triage"
done

# Rows of the triage with this failure (ENVIRON: awk -v would expand
# backslashes in the oracle message).
rows() { K="$1" awk -F'\t' 'NR > 1 && $3 == ENVIRON["K"] {print $1 "\t" $2}' "$triage"; }

emitted=()
unresolved=()
unreproduced=0
while IFS= read -r failure; do
    [[ -z $failure ]] && continue
    if [[ $failure == PASS ]]; then
        unreproduced=$(rows PASS | wc -l)
        continue
    fi
    if [[ $failure == UNRESOLVED:* ]]; then
        unresolved+=("$(rows "$failure" | wc -l | tr -d ' ') artifact(s): $failure")
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
            -exact_artifact_path="$dst" > "$out/min/$name.log" 2>&1 </dev/null || true
        if [[ ! -s $dst || "$(outcome "$dst")" != "$failure" ]]; then
            cp "$src" "$dst"
        fi
        size=$(stat -c %s "$dst" 2>/dev/null || stat -f %z "$dst")
        if [[ -z $best || $size -lt $best_size ]]; then
            best=$dst
            best_size=$size
        fi
    done < <(rows "$failure" | sort -t$'\t' -k2,2n | head -n "$per_kind")
    hash=$(sha1sum "$best" 2>/dev/null | cut -c1-8 || shasum "$best" | cut -c1-8)
    name="campaign_$(slug "$failure")_$hash"
    if "$repro" "$best" --emit "$name" > "$out/min/$name.repro.log" 2>&1 </dev/null \
        && [[ -f $here/tests/$name.rs ]]; then
        emitted+=("fuzz/tests/$name.rs ($best_size bytes): $failure")
    else
        emitted+=("NOT EMITTED, see $out/min/$name.repro.log ($best_size bytes, $best): $failure")
    fi
done < <(awk -F'\t' 'NR > 1 {print $3}' "$triage" | sort -u)

# Keep a small corpus: libFuzzer's minimal subset with the same coverage.
# Minimized into a fresh directory; the persistent corpus is replaced only
# when that succeeded. A re-triage (-T) leaves it alone.
if [[ $triage_only -eq 1 ]]; then
    corpus_line="corpus   unchanged (re-triage)"
elif [[ $corpus == "$out/corpus" ]]; then
    corpus_line="corpus   NOT saved: -c is the run's own corpus directory"
elif [[ -d $out/corpus ]] && [[ -n "$(ls -A "$out/corpus")" ]]; then
    before=$(find "$out/corpus" -maxdepth 1 -type f | wc -l | tr -d ' ')
    rm -rf "$out/corpus-min"
    cp -R "$out/corpus" "$out/corpus-min"
    if cargo +"$nightly" fuzz cmin -s none scenario "$out/corpus-min" -- \
            -timeout=60 -rss_limit_mb=4096 \
            > "$out/logs/cmin.log" 2>&1 </dev/null \
        && after=$(find "$out/corpus-min" -maxdepth 1 -type f | wc -l | tr -d ' ') \
        && [[ $after -gt 0 ]]; then
        rm -rf "$corpus.new"
        cp -R "$out/corpus-min" "$corpus.new"
        rm -rf "$corpus.old"
        [[ ! -e $corpus ]] || mv "$corpus" "$corpus.old"
        mv "$corpus.new" "$corpus"
        rm -rf "$corpus.old"
        corpus_line="corpus   $before inputs minimized to $after, saved to $corpus ($(du -sh "$corpus" | cut -f1))"
    else
        corpus_line="corpus   minimization FAILED (see $out/logs/cmin.log); $corpus left unchanged"
    fi
else
    corpus_line="corpus   none"
fi

{
    [[ $triage_only -eq 0 ]] || echo "--- re-triage $(date -u +%FT%TZ) at $(git -C "$here" rev-parse HEAD)"
    echo "artifacts $(($(wc -l < "$triage") - 1)) ($unreproduced pass on this tree: fixed, or a wall-clock timeout the simulator does not reproduce)"
    echo "distinct failures ${#emitted[@]}"
    for e in "${emitted[@]}"; do
        echo "  $e"
    done
    echo "unresolved ${#unresolved[@]} (repro hung or crashed without an oracle verdict)"
    for e in "${unresolved[@]}"; do
        echo "  $e"
    done
    echo "$corpus_line"
} >> "$summary"
cat "$summary"
[[ ${#emitted[@]} -eq 0 && ${#unresolved[@]} -eq 0 ]]
