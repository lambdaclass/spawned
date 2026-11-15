------------------------ MODULE SupervisionCoverage ------------------------
(***************************************************************************)
(* Coverage projection of Supervision.tla (trace_coverage.py).             *)
(*                                                                         *)
(* Concrete states depend on N, MaxRestarts and MaxGen, which differ       *)
(* between the model-checking configurations and the recorded traces, so  *)
(* coverage is measured on a projection that does not:                     *)
(*                                                                         *)
(*  - an action key per action and outcome (A|Decide/restart);             *)
(*  - a state key per child: its restart mode, controller pc, the          *)
(*    supervisor phase (run_supervisor pc, gate open or closed, shutdown   *)
(*    deadline hit, failure recorded) and the retry budget bucket (0, mid, *)
(*    max), plus whether the child holds a retry reservation;              *)
(*  - a transition key: the action key in the phase and budget bucket of  *)
(*    its pre-state, with the acting child's mode, or, for supervisor and *)
(*    environment actions, once per child with that child's mode and pc   *)
(*    (so "Close while a child is in backoff" is one key).                 *)
(*                                                                         *)
(* Keys(a, c) is a step predicate's value: it reads the pre-state and the  *)
(* primed post-state. a is the action name, c the child parameter of the   *)
(* trace event (0 for none). Outcomes come from the state change, not from *)
(* the logged parameter, so model and trace keys agree even for events     *)
(* logged as "unknown".                                                    *)
(***************************************************************************)
EXTENDS Supervision, TLC

ChildActions == {"LoopTop", "RetryReserve", "RetryRelease", "BackoffDone",
                 "Attempt", "BecomeReady", "StartupFailed", "SelfExit",
                 "StopObserved", "CleanupDone", "CleanupStuck", "DeferExit", "Undefer",
                 "Decide", "Finish"}

Bucket(b) == IF b = 0 THEN "0" ELSE IF b >= MaxRestarts THEN "max" ELSE "mid"

\* Keys are tuples of strings, rendered by trace_coverage.py as
\* kind|action/outcome|field|... (empty fields dropped).
Ctx == << sphase, IF open THEN "open" ELSE "closed",
          IF deadline THEN "deadline" ELSE "", IF failed THEN "failed" ELSE "",
          Bucket(budget) >>

Outcome(a, c) ==
    CASE a = "LoopTop"      -> pc'[c]
      [] a = "BackoffDone"  -> IF ~open THEN "closed"
                               ELSE IF pc'[c] = "attempt" THEN "retry" ELSE "exhausted"
      [] a = "Attempt"      -> IF open THEN "open" ELSE "closed"
      [] a = "BecomeReady"  -> IF open THEN "published" ELSE "closed"
      [] a = "StartupFailed" -> exitR'[c]
      [] a = "SelfExit"     -> exitR'[c]
      [] a = "Decide"       -> IF ~open THEN "closed"
                               ELSE IF pc'[c] = "top" THEN "restart"
                               ELSE IF open' THEN "stop" ELSE "fail"
      [] a = "SupStartDone" -> IF sready' THEN "ready" ELSE "notready"
      [] OTHER              -> ""

StateKeys ==
    { <<"S", Mode[c], "", pc[c]>> \o Ctx \o <<IF c \in reserved THEN "reserved" ELSE "">>
      : c \in Children }

StepKeys(a, c) ==
    IF a \in ChildActions
    THEN LET o == Outcome(a, c) IN
         { <<"A", a, o>>, <<"T", a, o, Mode[c]>> \o Ctx }
    ELSE LET o == Outcome(a, 0) IN
         { <<"A", a, o>> } \cup { <<"T", a, o, Mode[d], pc[d]>> \o Ctx : d \in Children }

\* Every key of one step: the step itself and both states' projections.
Keys(a, c) == StepKeys(a, c) \cup StateKeys \cup StateKeys'

\* Coverage-only instance with a Temporary child (MCSupervision has none).
CovModeTemporary == <<"Permanent", "Temporary">>
=============================================================================
