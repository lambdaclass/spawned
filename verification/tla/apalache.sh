#!/usr/bin/env bash
# Inductive-invariant checks of the TLA+ supervision and shutdown-coordinator
# models with Apalache.
# Run inside `nix develop` or via `nix run .#check-apalache` (both provide
# apalache-mc, a JDK and perl). Exits non-zero if any check gives an
# unexpected result.
#
#   apalache.sh                       prove every *Ind module at its default size
#   apalache.sh --size N R            Supervision with N children, MaxRestarts R
#   apalache.sh --targets K           Coordinator with K shutdown targets
#   apalache.sh --negative-controls   also require counterexamples from mutated specs
#   apalache.sh --model Supervision   one model only (Supervision, Cohorts, Nested,
#                                     Coordinator)
#
# For each model the checks are, for the constants in MC<Model>Ind.tla:
#   init     Init => IndInv                  --init=Init   --inv=IndInv  --length=0
#   step     IndInv /\ Next => IndInv'       --init=IndInv --inv=IndInv  --length=1
#   actions  IndInv /\ Next => action props  --init=IndInv --inv=ActionProps --length=1
#   probe-*  IndInv admits states far outside TLC's bounds (must be violated)
# The step check starts from every state satisfying IndInv, so it is not
# bounded by depth (and, for Supervision, not by MaxGen: generations are
# unbounded integers). It is still one instance size: the fixed constants.
# In Cohorts and NestedSupervision MaxGen/MaxRounds are action guards, i.e.
# part of the checked system.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
apalache="${APALACHE:-apalache-mc}"

negative=0
models="Supervision Cohorts Nested Coordinator"
size=""
targets=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --negative-controls) negative=1 ;;
    --size) size="$2 $3"; shift 2 ;;
    --targets) targets="$2"; shift ;;
    --model) models="$2"; shift ;;
    *) echo "usage: $0 [--size N R] [--targets K] [--model NAME] [--negative-controls]" >&2; exit 2 ;;
  esac
  shift
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp "$here"/*.tla "$work"/
chmod u+w "$work"/*.tla  # sources may be read-only (nix store)
cd "$work"

status=0

# check LABEL EXPECT DIR MODULE INIT INV LENGTH
# EXPECT is "holds" (Apalache reports NoError) or "violated" (a counterexample).
check() {
  local label="$1" expect="$2" dir="$3" module="$4" init="$5" inv="$6" length="$7"
  local log="$work/${label//[^A-Za-z0-9-]/_}.log" got start=$SECONDS
  (cd "$dir" && "$apalache" check --out-dir="$work/out" --cinit=ConstInit \
      --init="$init" --next=Next --inv="$inv" --length="$length" --no-deadlock \
      "$module.tla") >"$log" 2>&1 || true
  if grep -q "The outcome is: NoError" "$log"; then got=holds
  elif grep -q "The outcome is: Error" "$log"; then got=violated
  else got=failed
  fi
  printf '%-48s %-9s (%ss)\n' "$label" "$got" "$((SECONDS - start))"
  if [[ "$got" != "$expect" ]]; then
    echo "  expected $expect; log follows" >&2
    tail -40 "$log" >&2
    status=1
  fi
}

prove() {  # prove DIR MODULE PREFIX ACTIONS(yes|no) PROBES...
  local dir="$1" module="$2" prefix="$3" actions="$4"; shift 4
  check "$prefix init"    holds "$dir" "$module" Init   IndInv      0
  check "$prefix step"    holds "$dir" "$module" IndInv IndInv      1
  if [[ "$actions" == yes ]]; then
    check "$prefix actions" holds "$dir" "$module" IndInv ActionProps 1
  fi
  local probe
  for probe in "$@"; do
    check "$prefix probe $probe" violated "$dir" "$module" IndInv "$probe" 1
  done
}

# mutate MODULE NAME FILE PERL_SUBST INV [REACH]: copy the specs, apply one
# guard removal with perl, require the substitution to change FILE, and
# require a counterexample to induction (INV is IndInv or ActionProps). With
# REACH, also require a counterexample from Init within REACH steps, so the
# mutation breaks a reachable behaviour, not only an unreachable IndInv state.
mutate() {
  local module="$1" name="$2" file="$3" subst="$4" inv="$5" reach="${6:-}"
  local dir="$work/mut-$module-$name"  # Cohorts and Nested share fault names
  mkdir -p "$dir"
  cp "$work"/*.tla "$dir"/
  perl -0pi -e "$subst" "$dir/$file"
  if cmp -s "$work/$file" "$dir/$file"; then
    echo "mutation $name did not apply to $file" >&2
    status=1
    return
  fi
  check "negative $module/$name" violated "$dir" "$module" IndInv "$inv" 1
  if [[ -n "$reach" ]]; then
    check "negative $module/$name reachable" violated "$dir" "$module" Init "$inv" "$reach"
  fi
}

for model in $models; do
  case "$model" in
    Supervision)
      module=MCSupervisionInd
      if [[ -n "$size" ]]; then
        read -r n r <<<"$size"
        module=MCSupervisionIndSized
        sed -e "s/MODULE MCSupervisionInd /MODULE MCSupervisionIndSized /" \
            -e "s/^OVERRIDE_N == .*/OVERRIDE_N == $n/" \
            -e "s/^OVERRIDE_MaxRestarts == .*/OVERRIDE_MaxRestarts == $r/" \
            MCSupervisionInd.tla >"$module.tla"
      fi
      echo "== Apalache $module ($(grep -E '^OVERRIDE_(N|MaxRestarts) ' "$module.tla" | tr '\n' ' '))"
      prove "$work" "$module" Supervision yes NoDeepRunningState NoMidShutdownState NoRestartStep
      if [[ "$negative" == 1 ]]; then
        # Each removes one guard of Supervision.tla; the comment names the
        # property whose induction must break.
        # BudgetBounded: reserve retry capacity without checking the limit.
        mutate "$module" budget Supervision.tla \
          's/(RetryReserve\(c\) ==.*?) \/\\ budget < MaxRestarts/$1/s' IndInv
        # ReadyMeansAllStarted: launch the next child before the previous is ready.
        mutate "$module" ordered-start Supervision.tla \
          's/IF sidx = 1 THEN TRUE ELSE initial\[sidx - 1\]/TRUE/' IndInv
        # DoneMeansQuiescent: join without waiting for every controller.
        mutate "$module" join Supervision.tla \
          's/(SupJoin ==.*?)\/\\ \\A c \\in Children : pc\[c\] \\in \{"idle", "finished"\}/$1/s' IndInv
        # TemporaryStartsOnce: restart regardless of mode and exit reason.
        mutate "$module" restart-policy Supervision.tla \
          's/ELSE IF Restarts\(Mode\[c\], exitR\[c\]\) THEN/ELSE IF TRUE THEN/' IndInv
        # AtMostOneGeneration: a self-exit skips cleanup and goes straight to decide.
        mutate "$module" skip-cleanup Supervision.tla \
          's/(SelfExit\(c, r\) ==.*?)Goto\(c, "cleanup"\)/$1Goto(c, "decide")/s' IndInv
        # PublishedIsLive: an observed stop leaves the instance published.
        mutate "$module" stale-publication Supervision.tla \
          's/(StopObserved\(c\) ==.*?)pub\x27 = \[pub EXCEPT !\[c\] = 0\]/$1UNCHANGED pub/s' IndInv
        # NoStartAfterClose (negative_checks.py "Closed"): attempt ignores the gate.
        mutate "$module" closed Supervision.tla \
          's/(Attempt\(c\) ==\s*\/\\ pc\[c\] = "attempt"\s*\/\\ IF) open/$1 TRUE/s' ActionProps
        # ReverseShutdown: move to the next stop turn before the current child
        # finished. The single step from IndInv still satisfies ReverseShutdown;
        # what breaks is H_shutdown, the lemma that carries it to the next turn.
        mutate "$module" reverse-shutdown Supervision.tla \
          's/\/\\ sphase = "waitchild" \/\\ cdone\[sidx\]/\/\\ sphase = "waitchild"/' IndInv
      fi
      ;;
    Cohorts|Nested)
      module="MC${model}Ind"
      if [[ "$model" == Cohorts ]]; then
        probes=NoMidRoundState faults="Selection Cleanup Retirement Closed"
      else
        probes=NoFinishedSubtree faults="Join Closed"
      fi
      echo "== Apalache $module ($(grep -E '^OVERRIDE_' "$module.tla" | grep -v Fault | tr '\n' ' '))"
      # Their safety properties are state invariants over history flags
      # (badBarrier, badJoin, ...), so there is no separate action check.
      prove "$work" "$module" "$model" no $probes
      if [[ "$negative" == 1 ]]; then
        # The model's own Fault mutations (as in negative_checks.py), each
        # removing one guard: induction must fail for every one.
        for fault in $faults; do
          mutate "$module" "$fault" "$module.tla" \
            "s/OVERRIDE_Fault == \"None\"/OVERRIDE_Fault == \"$fault\"/" IndInv
        done
      fi
      ;;
    Coordinator)
      module=MCCoordinatorInd
      if [[ -n "$targets" ]]; then
        module=MCCoordinatorIndSized
        sed -e "s/MODULE MCCoordinatorInd /MODULE MCCoordinatorIndSized /" \
            -e "s/^OVERRIDE_TargetCount == .*/OVERRIDE_TargetCount == $targets/" \
            MCCoordinatorInd.tla >"$module.tla"
      fi
      echo "== Apalache $module ($(grep -E '^OVERRIDE_TargetCount ' "$module.tla"))"
      prove "$work" "$module" Coordinator yes NoLateWaitingState NoPartialEmergencyJoin
      if [[ "$negative" == 1 ]]; then
        # The model's Fault mutations (negative_checks.py runs the same ones
        # under TLC). ReleaseEarly: Join without every target finished;
        # ForgetFailure: a crash not recorded; ForgetSignal: emergency join
        # without signalling the remaining targets (JoinAfterAllSignalled);
        # RollbackReady: the #209 bug, a ready root stopped by its startup
        # rollback outside the coordinator's order (OrderedShutdown).
        for fault in ReleaseEarly ForgetFailure ForgetSignal RollbackReady; do
          mutate "$module" "$fault" "$module.tla" \
            "s/OVERRIDE_Fault == \"None\"/OVERRIDE_Fault == \"$fault\"/" IndInv 4
        done
        # OrderedShutdown: advance to the next target before the current
        # one finished.
        mutate "$module" advance-early Coordinator.tla \
          's/(Advance == \/\\ phase = "Waiting") \/\\ index \\in finished/$1/' IndInv 4
        # OrderedSignalStep: stop targets in the reverse of the stop order
        # (earliest-admitted root first).
        mutate "$module" reverse-order Coordinator.tla \
          's/signalled\x27 = signalled \\cup \{index\}/signalled\x27 = signalled \\cup {TargetCount + 1 - index}/' ActionProps 4
        # Admission closed outside Running (Safety): begin shutdown, or
        # crash, without closing admission.
        mutate "$module" begin-keeps-open Coordinator.tla \
          's/(Begin ==.*?)open\x27 = FALSE/$1open\x27 = open/s' IndInv 4
        mutate "$module" crash-keeps-open Coordinator.tla \
          's/(Crash ==.*?)open\x27 = FALSE/$1open\x27 = open/s' IndInv 4
      fi
      ;;
    *) echo "unknown model $model" >&2; exit 2 ;;
  esac
done
exit "$status"
