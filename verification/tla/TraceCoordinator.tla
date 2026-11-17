--------------------------- MODULE TraceCoordinator ---------------------------
(***************************************************************************)
(* Trace validation for Coordinator.tla.                                   *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, ActorSystem::begin_shutdown         *)
(* (concurrency/src/system.rs) records one event per model action under    *)
(* the system's trace id (lifecycle_trace::Coordinator): [a |-> action,    *)
(* c |-> target (stop order, 1 = last admitted), x |-> parameter, n |->    *)
(* target count]. Only the coordinator task records, so the log is its     *)
(* program order.                                                          *)
(*                                                                         *)
(*   Begin         admission closed and the roots snapshotted; n = the     *)
(*                 number of targets (TargetCount)                         *)
(*   StopNext(c)   the ordered loop sent target c the parent's Shutdown    *)
(*                 (a supervisor: its stop)                                *)
(*   Finish(c)     the coordinator observed target c's join (actor, its    *)
(*                 helpers and, for a supervisor, its tree)                *)
(*   Advance       the ordered loop moved past a joined target             *)
(*   Ordered       the ordered loop joined every target within the grace   *)
(*   GraceExpired  a join in the ordered loop timed out                    *)
(*   Emergency     every target signalled: after the grace expired, the    *)
(*                 kill pass (x = "kill": the remaining roots get the      *)
(*                 kill); after a panic, the fallback's first complete     *)
(*                 stop pass (x = "shutdown")                              *)
(*   Crash         the coordinator panicked; x = "reported" if the failure *)
(*                 is visible in the shutdown report                       *)
(*   Join          every target and group joined; runtime ownership        *)
(*                 released                                                *)
(*                                                                         *)
(* Refinement notes. After the grace expired the code still walks the     *)
(* remaining targets in stop order (Shutdown with a zero cleanup timeout;  *)
(* a supervisor's stop and immediate stop_remaining), then kills every     *)
(* scope: the model's Emergency is that whole pass, so the walk records    *)
(* nothing. A Finish is recorded when the coordinator observes the join,   *)
(* which may be after the target actually finished (the model's Finish is  *)
(* enabled from its signal on, so observing it later is a behavior too).   *)
(* Execution groups are outside the model: their done is folded into Join. *)
(* The trace is accepted iff some behavior consumes the whole log (TLC     *)
(* reports TraceIncomplete violated).                                      *)
(***************************************************************************)
EXTENDS Coordinator, Sequences

CONSTANT Trace

VARIABLE i

traceVars == <<vars, i>>

TraceInit == Init /\ i = 0

Step(e) ==
    CASE e.a = "Begin"        -> e.n = TargetCount /\ Begin
      [] e.a = "StopNext"     -> index = e.c /\ StopNext
      [] e.a = "Finish"       -> e.c \in Targets /\ Finish(e.c)
      [] e.a = "Advance"      -> Advance
      [] e.a = "Ordered"      -> Ordered
      [] e.a = "GraceExpired" -> GraceExpired
      [] e.a = "Emergency"    -> Emergency /\ (e.x = "kill") = (expired /\ ~crashed)
      [] e.a = "Crash"        -> Crash /\ reported' = (e.x = "reported")
      [] e.a = "Join"         -> Join
      [] OTHER                -> FALSE

TraceNext == i < Len(Trace) /\ Step(Trace[i + 1]) /\ i' = i + 1

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* Violated exactly when the whole log has been matched: TLC reporting this
\* invariant as violated means the trace is ACCEPTED.
TraceIncomplete == i < Len(Trace)
=============================================================================
