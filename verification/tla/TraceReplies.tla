----------------------------- MODULE TraceReplies -----------------------------
(***************************************************************************)
(* Trace validation for DeferredReplies.tla: one owner instance A's reply  *)
(* slots, its pipe_reply jobs and their downstream requests to an actor B. *)
(*                                                                         *)
(* With the `lifecycle-trace` feature, the reply transport records per     *)
(* owner instance (concurrency/src/lifecycle_trace.rs, ReplyEvent; `r` is  *)
(* the request's slot, numbered by trace_check.py at each Send):           *)
(*   Send       slot created                                  (Send)       *)
(*   Dispatch   its request turn starts                       (DispatchA)  *)
(*   Discharge  the slot's one OPEN -> terminal transition, in the trace   *)
(*              section with its CAS; a reply's is OPEN -> REPLIED (x: ok) *)
(*              and reads no clock, so "timeout" is always an expiry       *)
(*              (Expire), never a reply (OTP: the caller's timeout)       *)
(*   Shorten    shorten_deadline lowered the deadline, under the slot lock *)
(*              (Shorten; TLC picks the new deadline)                      *)
(*   PipeReply  pipe_reply* launched a job ("linked") or the slot refused  *)
(*              it ("refused": closed, or no credit for a second job)      *)
(*   JobNotify  the job's done or deadline message is dispatched to A, not *)
(*              fenced (JobNotify; x: done ends the job, deadline keeps it)*)
(*   JobEnd     the job's execution ended without taking its notification *)
(*              (after a deadline notification, or cancelled) (JobEnds)    *)
(*   JobCancel  the job was cancelled while it still held the token: its  *)
(*              caller gave up (cancel_on_caller_gone) or A's exit began   *)
(*              (JobCancel)                                                *)
(*   CallB      the job's work created a request slot at another actor B   *)
(*              (JobCallB; a thread-local link set while the work runs)    *)
(*   BDispatch  B deferred that request ("stateB") or answered it in its   *)
(*              turn ("done": a reply, Overloaded at B's bound, a drop)    *)
(*   BReply     B answered the deferred request later (BReply)            *)
(*   Exit       the registry enters exiting (exit begins, before stopped())*)
(*   Stuck      a supervisor's cleanup deadline passed before A's death    *)
(*              point (CleanupDeadline); one after it (a slow destructor   *)
(*              before publication) is not recorded: the registry is sealed*)
(*   Death      the death point D: the reply gate is fenced, the registry  *)
(*              sealed (Death). The answers death then gives and their     *)
(*              wake-ups (Answer) are not logged and are taken silently.   *)
(*                                                                         *)
(* Turn-internal steps leave no trace and are taken silently: Defer that   *)
(* takes a credit (or finds the slot closed), Store, DropInTurn, a turn    *)
(* end that answers nothing, a drop while exiting (exit pending, answered  *)
(* by Death), and the clock. Sent messages (not requests) leave no trace:  *)
(* the model's turns are request turns and JobNotify turns, and replies    *)
(* from other turns or stopped() are LaterReply. B's caller-side closes    *)
(* (its slot abandoned when the job's future is dropped, or expired) are   *)
(* not B's steps: the model folds them into the job's end (EndDown).       *)
(*                                                                         *)
(* Not mapped (an event the model cannot take rejects the trace):          *)
(*  - pipe_reply from a non-request turn (a stored token), or a token      *)
(*    shortened inside a job: the model attaches and shortens only tokens  *)
(*    of the current request turn or stored ones;                          *)
(*  - a linked job cancelled through its JobHandle while its caller still  *)
(*    waits: the model cancels only on caller give-up or owner exit;       *)
(*  - a second downstream request from one request's jobs.                 *)
(*                                                                         *)
(* trace_check.py instantiates N (requests), BoundA (A's MAX_OUTSTANDING), *)
(* BoundB (B's, default 1) and a clock generous enough for the trace;      *)
(* deadlines are chosen by TLC.                                            *)
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
      [] e.a = "Shorten"   -> e.r \in Req /\ Shorten(e.r)
      [] e.a = "PipeReply" -> e.r \in Req /\ PipeReply(e.r)
                              /\ holder'[e.r] = IF e.x = "linked" THEN "doneA" ELSE "localA"
      [] e.a = "JobNotify" -> e.r \in Req /\ JobNotify(e.r)
                              /\ jobs'[e.r] = IF e.x = "done" THEN jobs[e.r] - 1 ELSE jobs[e.r]
      [] e.a = "JobEnd"    -> e.r \in Req /\ JobEnds(e.r)
      [] e.a = "JobCancel" -> e.r \in Req /\ JobCancel(e.r)
      [] e.a = "CallB"     -> e.r \in Req /\ JobCallB(e.r)
      [] e.a = "BDispatch" -> e.r \in Req /\ BDispatch(e.r) /\ down'[e.r] = e.x
      [] e.a = "BReply"    -> e.r \in Req /\ BReply(e.r)
      [] e.a = "Stuck"     -> CleanupDeadline
      [] e.a = "Death"     -> Death
      [] OTHER             -> FALSE

\* Steps that leave no trace: they answer no request.
Silent ==
    /\ \/ Tick \/ EndTurn
       \/ \E r \in Req : Defer(r) \/ Store(r) \/ DropInTurn(r) \/ LaterDrop(r)
    /\ UNCHANGED slot

\* The steps of death after D, which leave no trace.
DeathSilent == \/ \E r \in Req : Answer(r)
               \/ Discharged

TraceNext ==
    /\ i < Len(Trace)
    /\ \/ Step(Trace[i + 1]) /\ UNCHANGED <<earlyClosed, lateOk>> /\ i' = i + 1
       \/ (Silent \/ DeathSilent) /\ UNCHANGED <<earlyClosed, lateOk>> /\ UNCHANGED i

TraceSpec == TraceInit /\ [][TraceNext]_traceVars

\* Violated exactly when the whole log has been matched: TLC reporting this
\* invariant as violated means the trace is ACCEPTED.
TraceIncomplete == i < Len(Trace)
=============================================================================
