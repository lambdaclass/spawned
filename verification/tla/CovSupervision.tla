--------------------------- MODULE CovSupervision ---------------------------
(***************************************************************************)
(* Model side of trace coverage (trace_coverage.py): explores Supervision  *)
(* exactly as Spec does and prints every SupervisionCoverage key of every  *)
(* reachable step.                                                         *)
(*                                                                         *)
(* lbl names the action that produced a state. It is excluded from the     *)
(* fingerprint (VIEW CovView), so the explored state space is the model's  *)
(* own: trace_coverage.py checks the distinct-state count against the     *)
(* plain model. CovNext must list the same actions as Next. Emit is an     *)
(* action constraint that is always true; it prints each key the first     *)
(* time a TLC worker sees it (TLCGet/TLCSet are per worker).               *)
(***************************************************************************)
EXTENDS SupervisionCoverage

VARIABLE lbl

CovInit == Init /\ lbl = <<"Init", 0>> /\ TLCSet(1, {})

L(a, c) == lbl' = <<a, c>>

CovNext ==
    \/ Close /\ L("Close", 0)
    \/ WindowExpire /\ L("WindowExpire", 0)
    \/ ShutdownDeadline /\ L("ShutdownDeadline", 0)
    \/ SupLaunch /\ L("SupLaunch", 0)
    \/ SupStartDone /\ L("SupStartDone", 0)
    \/ SupCancelSeen /\ L("SupCancelSeen", 0)
    \/ SupStopTurn /\ L("SupStopTurn", 0)
    \/ SupChildDone /\ L("SupChildDone", 0)
    \/ SupChildTimeout /\ L("SupChildTimeout", 0)
    \/ SupJoin /\ L("SupJoin", 0)
    \/ \E c \in Children :
        \/ LoopTop(c) /\ L("LoopTop", c)
        \/ RetryReserve(c) /\ L("RetryReserve", c)
        \/ RetryRelease(c) /\ L("RetryRelease", c)
        \/ BackoffDone(c) /\ L("BackoffDone", c)
        \/ Attempt(c) /\ L("Attempt", c)
        \/ BecomeReady(c) /\ L("BecomeReady", c)
        \/ \E r \in StartupResults \cup {"Shutdown"} : StartupFailed(c, r) /\ L("StartupFailed", c)
        \/ \E r \in SelfExits : SelfExit(c, r) /\ L("SelfExit", c)
        \/ StopObserved(c) /\ L("StopObserved", c)
        \/ CleanupDone(c) /\ L("CleanupDone", c)
        \/ CleanupStuck(c) /\ L("CleanupStuck", c)
        \/ DeferExit(c) /\ L("DeferExit", c)
        \/ Undefer(c) /\ L("Undefer", c)
        \/ Decide(c) /\ L("Decide", c)
        \/ Finish(c) /\ L("Finish", c)

CovView == vars

Emit ==
    GenBound' =>
        \A k \in Keys(lbl'[1], lbl'[2]) :
            k \in TLCGet(1) \/ (PrintT(k) /\ TLCSet(1, TLCGet(1) \cup {k}))
=============================================================================
