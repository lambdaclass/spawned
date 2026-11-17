----------------------------- MODULE TraceDisposal -----------------------------
(***************************************************************************)
(* Trace validation for Disposal.tla: the deadline service's hand-off of   *)
(* wake-ups and payloads to the disposal workers, the pool's bookkeeping,  *)
(* and the owner's hold.                                                   *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, while `record_disposal` is on, the  *)
(* code records (concurrency/src/lifecycle_trace.rs, DisposalEvent):       *)
(* on the service thread                                                   *)
(*   Expire   a slot's OPEN -> TIMED_OUT and credit return, in a visit     *)
(*            (registry.rs); x: "wake" if its wake-up was handed off       *)
(*            (Expire, with the caller's `waiting` matching x)             *)
(*   Defer    a payload (x: "drop", DropFailures::discard, in one section  *)
(*            with the hold check) or another item (x: "other": the last   *)
(*            reference to a job or registry, emitter wakers, a slot with  *)
(*            an unread reply) handed off (Release, Other)                 *)
(*   Submit   the visit's items queued and the start rule (x: "spawn" if   *)
(*            a worker start was decided) (Submit)                         *)
(* in the pool (disposal.rs `run`, each under the pool lock except Run,    *)
(* Settle and Done)                                                        *)
(*   Begin, Take (x as for Submit), Wait, Woke, Retire  (same names)       *)
(*   Run      the worker starts its item (a wake-up wakes its caller) (Run)*)
(*   Settle   a payload's hold check, x: "held" or "dropped" (Settle)      *)
(*   Done     the item returned (Done)                                    *)
(* and the owner: Exit (DropFailures::hold) and Death (release).          *)
(*                                                                         *)
(* trace_check.py turns the leading `Pool` event (the pool when recording *)
(* starts: its idle workers) into InitIdle, numbers requests, payloads,    *)
(* other items and workers, and sets Workers to the code's cap. The clock  *)
(* is silent. No item blocks in a recorded run (Blocking = {}).            *)
(***************************************************************************)
EXTENDS Disposal, TLC

CONSTANT Trace

VARIABLE i

traceVars == <<vars, i>>

TraceInit == Init /\ i = 0

Item(e) == [k |-> e.k, id |-> e.n]

Step(e) ==
    CASE e.a = "Expire" -> e.n \in Req /\ waiting[e.n] = (e.x = "wake") /\ Expire(e.n)
      [] e.a = "Defer"  -> \/ e.k = "drop" /\ e.n \in Pay /\ Release(e.n)
                           \/ e.k = "other" /\ e.n \in Oth /\ Other(e.n)
      [] e.a = "Submit" -> SubmitDecide(e.x = "spawn")
      [] e.a = "Begin"  -> e.w \in W /\ Begin(e.w)
      [] e.a = "Take"   -> e.w \in W /\ queue # <<>> /\ Head(queue) = <<Item(e)>>
                           /\ TakeDecide(e.w, e.x = "spawn")
      [] e.a = "Wait"   -> e.w \in W /\ Wait(e.w)
      [] e.a = "Woke"   -> e.w \in W /\ Woke(e.w)
      [] e.a = "Retire" -> e.w \in W /\ Retire(e.w)
      [] e.a = "Run"    -> e.w \in W /\ cur[e.w] # <<>> /\ Head(cur[e.w]) = Item(e) /\ Run(e.w)
      [] e.a = "Settle" -> e.w \in W /\ cur[e.w] # <<>> /\ Head(cur[e.w]) = Item(e)
                           /\ SettleTo(e.w, e.x)
      [] e.a = "Done"   -> e.w \in W /\ cur[e.w] # <<>> /\ Head(cur[e.w]) = Item(e) /\ Done(e.w)
      [] e.a = "Exit"   -> Exit
      [] e.a = "Death"  -> Death
      [] OTHER          -> FALSE

TraceNext ==
    /\ i < Len(Trace)
    /\ \/ Step(Trace[i + 1]) /\ i' = i + 1
       \/ Tick /\ UNCHANGED i

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* Violated exactly when the whole log has been matched: TLC reporting this
\* invariant as violated means the trace is ACCEPTED.
TraceIncomplete == i < Len(Trace)
=============================================================================
