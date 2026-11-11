------------------------------ MODULE TraceJobs ------------------------------
(***************************************************************************)
(* Trace validation for DeferredJobs.tla.                                  *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, the job core records one event per  *)
(* model action for each owner instance (concurrency/src/reply/jobs.rs):   *)
(* [a |-> action, j |-> job (1..J in start order), x |-> parameter,        *)
(* keyed, dl, fenced]. Each event is recorded in the trace section         *)
(* together with its effect (the flag change, the enqueue, or the fence    *)
(* read at dispatch), so the log is a linearization.                       *)
(*                                                                         *)
(*   Start(j)        admission (keyed: replaces the key; dl: has a job     *)
(*                   deadline)                                             *)
(*   Emit(j)         an emit enqueued                                       *)
(*   Finish(j)       execution ended; x = "notified" if it enqueued the    *)
(*                   terminal message, "quiet" if the notification was     *)
(*                   already taken or suppressed                           *)
(*   JobDeadline(j)  the job deadline took the notification                 *)
(*   Cancel(j)       handle cancel (or the opt-in caller give-up)          *)
(*   Exit            the owner's exit cancelled the job set                *)
(*   Dispatch(j)     the mailbox head, of kind x, fenced or delivered      *)
(*                                                                         *)
(* One trace is one owner instance (MaxGen = 1) and at most one key. The   *)
(* model's permit bound is not checked (JobBound = J): rejected            *)
(* admissions are not model actions. Delayed emits (emit_at) are not       *)
(* recorded; the recorded scenarios use none (MCDeferredJobsHeld checks    *)
(* them). The trace is accepted iff some behavior consumes the whole log   *)
(* (TLC reports TraceIncomplete violated).                                 *)
(***************************************************************************)
EXTENDS DeferredJobs, TLC

CONSTANT Trace

VARIABLE i

traceVars == <<vars, i>>

TraceInit == Init /\ i = 0

Step(e) ==
    CASE e.a = "Start"       -> e.j \in Jobs /\ Start(e.j) /\ hasDl'[e.j] = e.dl
                                /\ (e.j \in Keyed) = e.keyed
      [] e.a = "Emit"        -> e.j \in Jobs /\ Emit(e.j)
      [] e.a = "Finish"      -> e.j \in Jobs /\ Finish(e.j)
                                /\ (termSent'[e.j] > termSent[e.j]) = (e.x = "notified")
      [] e.a = "JobDeadline" -> e.j \in Jobs /\ JobDeadline(e.j)
      [] e.a = "Cancel"      -> e.j \in Jobs /\ Cancel(e.j)
      [] e.a = "Exit"        -> Exit
      [] e.a = "Dispatch"    -> /\ mbox # <<>>
                                /\ Head(mbox).j = e.j /\ Head(mbox).kind = e.x
                                /\ Fenced(Head(mbox)) = e.fenced
                                /\ Dispatch
      [] OTHER               -> FALSE

TraceNext == i < Len(Trace) /\ Step(Trace[i + 1]) /\ i' = i + 1

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* Violated exactly when the whole log has been matched: TLC reporting this
\* invariant as violated means the trace is ACCEPTED.
TraceIncomplete == i < Len(Trace)
=============================================================================
