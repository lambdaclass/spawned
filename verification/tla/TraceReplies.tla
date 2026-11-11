----------------------------- MODULE TraceReplies -----------------------------
(***************************************************************************)
(* Trace validation for DeferredReplies.tla (reply slots, without linked   *)
(* jobs and the downstream actor).                                         *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, the reply transport records per     *)
(* owner instance: Send (slot created), Dispatch (its turn starts),        *)
(* Discharge (the slot's one OPEN -> terminal transition, recorded in the  *)
(* trace section together with the CAS; a reply counts at WRITING), Exit   *)
(* (the registry enters exiting, after committing the turn's drops) and    *)
(* Death (the registry closes; the closes it and the mailbox disposal      *)
(* perform belong to this one model step and are not logged).             *)
(*                                                                         *)
(* Turn-internal steps leave no trace and are taken silently: Defer that   *)
(* takes a credit (or finds the slot closed), Store, DropInTurn, a turn    *)
(* end that answers nothing, a drop while exiting (the slot becomes exit   *)
(* pending and is answered by Death), and the clock. A Discharge is the model       *)
(* action that discharges exactly that request with that kind. The model's *)
(* turns are request turns: later replies run between them (from          *)
(* non-request turns, or stopped()).                                       *)
(*                                                                         *)
(* trace_check.py instantiates N (requests), BoundA (MAX_OUTSTANDING) and  *)
(* a clock generous enough for the trace; deadlines are chosen by TLC.     *)
(***************************************************************************)
EXTENDS DeferredReplies, Sequences, TLC

CONSTANT Trace

VARIABLE i

traceVars == <<vars, i>>

TraceInit == Init /\ i = 0

Only(r, k) == slot[r] = "open" /\ slot'[r] = k
              /\ \A q \in Req \ {r} : slot'[q] = slot[q]

Discharging(r) ==
    \/ ReplyNow(r) \/ LaterReply(r) \/ LaterDrop(r) \/ Expire(r)
    \/ CallerGivesUp(r) \/ Defer(r) \/ EndTurn \/ Panic

Step(e) ==
    CASE e.a = "Send"      -> e.r \in Req /\ Send(e.r)
      [] e.a = "Dispatch"  -> e.r \in Req /\ DispatchA(e.r)
      [] e.a = "Discharge" -> e.r \in Req /\ Discharging(e.r) /\ Only(e.r, e.x)
      [] e.a = "Exit"      -> \/ ExitBegin
                              \/ Panic /\ UNCHANGED slot
                              \/ phase = "exiting" /\ UNCHANGED vars
      [] e.a = "Death"     -> Death
      [] OTHER             -> FALSE

\* Steps that leave no trace: they answer no request.
Silent ==
    /\ \/ Tick \/ EndTurn
       \/ \E r \in Req : Defer(r) \/ Store(r) \/ DropInTurn(r) \/ LaterDrop(r)
    /\ UNCHANGED slot

TraceNext ==
    /\ i < Len(Trace)
    /\ \/ Step(Trace[i + 1]) /\ UNCHANGED earlyClosed /\ i' = i + 1
       \/ Silent /\ UNCHANGED earlyClosed /\ UNCHANGED i

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* Violated exactly when the whole log has been matched: TLC reporting this
\* invariant as violated means the trace is ACCEPTED.
TraceIncomplete == i < Len(Trace)
=============================================================================
