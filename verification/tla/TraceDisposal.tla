----------------------------- MODULE TraceDisposal -----------------------------
(***************************************************************************)
(* Trace validation for Disposal.tla: the deadline service's hand-off of   *)
(* wake-ups and payloads to the disposal workers, and the owner's hold.    *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, while `record_disposal` is on, the  *)
(* code records (concurrency/src/lifecycle_trace.rs, DisposalEvent; `id`   *)
(* is fresh per Expire or Release and numbered by trace_check.py):         *)
(*   Expire   a slot's OPEN -> TIMED_OUT and credit return on the service  *)
(*            thread, in a collecting visit (registry.rs, Expirable for    *)
(*            Slot); x: "wake" if a wake-up was handed off, else "none"    *)
(*            (Expire, with the caller's `waiting` matching x)             *)
(*   Release  a payload deferred from the service thread, in one section   *)
(*            with the hold check (DropFailures::discard) (Release)        *)
(*   Submit   the visit's work handed to the pool (deadline.rs) (Submit;   *)
(*            a visit whose items are all untracked, such as the last      *)
(*            reference to an entry, submits an empty batch here)          *)
(*   Wake     a worker runs a handed-off wake-up (Run of a wake item)      *)
(*   Drop     a worker settles a payload in one section with the hold     *)
(*            check (DropFailures::settle); x: "held" or "dropped" (Run of *)
(*            a drop item)                                                 *)
(*   Exit     the owner's hold begins (DropFailures::hold) (Exit)          *)
(*   Death    the hold is released at the death point (Death)             *)
(*                                                                         *)
(* Silent: the clock, and the workers' Start, Take and Retire. No item     *)
(* blocks in a recorded run (Blocking = {}).                               *)
(***************************************************************************)
EXTENDS Disposal, TLC

CONSTANT Trace

VARIABLE i

traceVars == <<vars, i>>

TraceInit == Init /\ i = 0

\* Some worker runs `item` next.
WorkerRuns(item) == \E w \in W : cur[w] # <<>> /\ Head(cur[w]) = item /\ Run(w)

Step(e) ==
    CASE e.a = "Expire"  -> e.id \in Req /\ waiting[e.id] = (e.x = "wake") /\ Expire(e.id)
      [] e.a = "Release" -> e.id \in Pay /\ Release(e.id)
      [] e.a = "Submit"  -> \/ Submit
                            \/ batch = <<>> /\ UNCHANGED vars
      [] e.a = "Wake"    -> e.id \in Req /\ WorkerRuns(WakeItem(e.id))
      [] e.a = "Drop"    -> e.id \in Pay /\ WorkerRuns(DropItem(e.id)) /\ payload'[e.id] = e.x
      [] e.a = "Exit"    -> Exit
      [] e.a = "Death"   -> Death
      [] OTHER           -> FALSE

Silent == \/ Tick
          \/ \E w \in W : Start(w) \/ Take(w) \/ Retire(w)

TraceNext ==
    /\ i < Len(Trace)
    /\ \/ Step(Trace[i + 1]) /\ i' = i + 1
       \/ Silent /\ UNCHANGED i

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* Violated exactly when the whole log has been matched: TLC reporting this
\* invariant as violated means the trace is ACCEPTED.
TraceIncomplete == i < Len(Trace)
=============================================================================
