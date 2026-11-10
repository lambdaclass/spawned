--------------------------- MODULE DeferredReplies ---------------------------
(***************************************************************************)
(* Rank 11 design model, part 1: reply slots, deferred tokens, linked     *)
(* jobs and owner exit. Design contract: docs/design/deferred-replies.md. *)
(* Part 2 (job fencing, keyed replacement, emits) is DeferredJobs.tla.    *)
(*                                                                         *)
(* Each request owns a slot with one atomic open -> terminal transition.  *)
(* The deadline is fixed at send and is the only timeout; a reply at or   *)
(* after it expires the slot instead of delivering. Expiry is serviced    *)
(* independently of A's turns ("waiting" callers are polling; deferred    *)
(* tokens are covered by the deadline service).                          *)
(*                                                                         *)
(* Owner A. Queued requests are dispatched even if their slot is already  *)
(* terminal (OTP: the server still handles the call). In a turn the       *)
(* handler replies in place (no credit), defers (taking a credit, or      *)
(* Overloaded at BoundA, or nothing if the slot is already closed) into a *)
(* local token, then stores it, pipes it into a linked job, or drops it;  *)
(* drops in a turn commit when the turn ends: Dropped if it returns,      *)
(* Stopped if it panics. A linked job shares its token's credit while the *)
(* token is open and no other job of that request runs; otherwise it      *)
(* needs a fresh credit. A job's notification hands the token back to a   *)
(* turn (completion or job deadline; after a deadline the work keeps      *)
(* running and keeps its credit). A job may make a downstream request to  *)
(* B, which has its own deferral bound; when the request's last job ends, *)
(* that downstream request is abandoned.                                  *)
(*                                                                         *)
(* Exit: A enters "exiting" (stop between turns, or a panic in a turn).   *)
(* While exiting (stopped(), OTP terminate) stored tokens can still be    *)
(* answered, drops answer Stopped, nothing is dispatched and jobs are     *)
(* cancelled. Death closes the registry (every token A holds, wherever it *)
(* is) and discards the mailbox. If stopped() (or a destructor) is still  *)
(* running at the cleanup deadline, A becomes "stuck": nothing is closed   *)
(* early (ActorStopped means the actor is gone); stopped() may still       *)
(* reply; callers are bounded by their own deadlines; death may never come.*)
(*                                                                         *)
(* Fault selects a negative control; negative_checks.py requires each to  *)
(* violate the named invariant:                                           *)
(*   NoAutoReply     (NoOrphan)            drops leave the slot open       *)
(*   NoAtomicExpiry  (AtMostOneDischarge)  reply after expiry discharges   *)
(*   LateSuccess     (NoSuccessAfterDeadline)                              *)
(*   LoopDeadline    (TimelyExpiry)        expiry waits for A's turn       *)
(*   Lengthen        (DeadlineWithinCaller)                                *)
(*   Unbounded       (BoundedA)            defer ignores the bound         *)
(*   NoDestAdmission (BoundedB)            B defers past its bound         *)
(*   ExitStateOnly   (NoOrphan)            death skips job-held tokens     *)
(*   UnwindDropped   (PanicStopped)        unwind drops answer Dropped     *)
(*   ShareRunning    (BoundedA)            a second job reuses the credit  *)
(*   AttachClosed    (BoundedA)            a closed token funds a job      *)
(*   LeakDownstream  (DownstreamOwned)     B's request outlives its job    *)
(*   EarlyClose      (NoEarlyClose)        the cleanup deadline closes the  *)
(*                                         registry before death            *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets
CONSTANTS N, BoundA, BoundB, MaxTime, MaxWait, Fault
ASSUME Fault \in {"None", "NoAutoReply", "NoAtomicExpiry", "LateSuccess",
                  "LoopDeadline", "Lengthen", "Unbounded", "NoDestAdmission",
                  "ExitStateOnly", "UnwindDropped", "ShareRunning",
                  "AttachClosed", "LeakDownstream", "EarlyClose"}

Req == 1..N
\* qA queued; turnA PendingReply in a turn; localA ReplyTo in a turn;
\* dropA dropped in a turn (commits at turn end); stateA stored;
\* doneA captured by a linked job's done; gone no longer held.
Holders == {"none", "qA", "turnA", "localA", "dropA", "stateA", "doneA", "gone"}
Terminal == {"ok", "timeout", "overloaded", "stopped", "dropped", "abandoned"}

VARIABLES now, caller, sent, dl, slot, holder, discharges, okAt,
          jobs,     \* running linked jobs per request (0..2)
          down,     \* downstream request to B: none, qB, stateB, done, abandoned
          phase,    \* A: run, exiting, stuck (cleanup deadline passed), dead
          turnA,    \* request in A's turn, 0 if idle
          panicked, \* history: requests whose token was in a panicking turn
          earlyClosed \* history: tokens answered Stopped by a close before death

vars == <<now, caller, sent, dl, slot, holder, discharges, okAt, jobs, down,
          phase, turnA, panicked, earlyClosed>>

Init ==
    /\ now = 0 /\ caller = [r \in Req |-> "idle"]
    /\ sent = [r \in Req |-> 0] /\ dl = [r \in Req |-> 0]
    /\ slot = [r \in Req |-> "none"] /\ holder = [r \in Req |-> "none"]
    /\ discharges = [r \in Req |-> 0] /\ okAt = [r \in Req |-> 0]
    /\ jobs = [r \in Req |-> 0] /\ down = [r \in Req |-> "none"]
    /\ phase = "run" /\ turnA = 0 /\ panicked = {} /\ earlyClosed = {}

Open(r) == slot[r] = "open"
TokenHeld(r) == Open(r) /\ holder[r] \in {"localA", "dropA", "stateA", "doneA"}
\* Credits: a request's token and its first job share one credit; every
\* further running job, and every job whose token has closed, needs its own.
CreditOf(r) == IF jobs[r] > 0 THEN jobs[r] ELSE IF TokenHeld(r) THEN 1 ELSE 0
Credits == Cardinality({<<r, k>> \in Req \X {1, 2} : k <= CreditOf(r)})
HeldB == {r \in Req : down[r] = "stateB"}

View(k) == CASE k = "ok" -> "ok" [] k = "timeout" -> "timeout" [] OTHER -> "err"
DischargeSet(S, k) ==
    /\ slot' = [r \in Req |-> IF r \in S THEN k ELSE slot[r]]
    /\ discharges' = [r \in Req |-> IF r \in S THEN discharges[r] + 1 ELSE discharges[r]]
    /\ caller' = [r \in Req |-> IF r \in S /\ caller[r] = "waiting" THEN View(k) ELSE caller[r]]
    /\ okAt' = [r \in Req |-> IF r \in S /\ k = "ok" THEN now ELSE okAt[r]]
Discharge(r, k) == DischargeSet({r}, k)
Keep == UNCHANGED <<slot, discharges, caller, okAt>>
OpenOf(S) == {r \in S : Open(r)}

TryReply(r) ==
    IF Open(r)
    THEN IF now < dl[r] \/ Fault = "LateSuccess" THEN Discharge(r, "ok")
         ELSE Discharge(r, "timeout")
    ELSE IF Fault = "NoAtomicExpiry" /\ slot[r] = "timeout"
         THEN /\ discharges' = [discharges EXCEPT ![r] = @ + 1]
              /\ okAt' = [okAt EXCEPT ![r] = now]
              /\ UNCHANGED <<slot, caller>>
         ELSE Keep

\* A drop outside a turn: Dropped while running, Stopped while exiting.
DropNow(r) ==
    IF ~Open(r) \/ Fault = "NoAutoReply" THEN Keep
    ELSE Discharge(r, IF phase = "run" THEN "dropped" ELSE "stopped")

U(v) == UNCHANGED v

------------------------------------------------------------------------------
\* Callers

Send(r) ==
    /\ caller[r] = "idle" /\ phase # "dead"
    /\ \E d \in 1..MaxWait : now + d <= MaxTime
          /\ sent' = [sent EXCEPT ![r] = now + d] /\ dl' = [dl EXCEPT ![r] = now + d]
    /\ slot' = [slot EXCEPT ![r] = "open"] /\ holder' = [holder EXCEPT ![r] = "qA"]
    /\ caller' = [caller EXCEPT ![r] = "waiting"]
    /\ U(<<now, discharges, okAt, jobs, down, phase, turnA, panicked>>)

\* Dropping the receiver abandons the slot; the token's credit returns.
CallerGivesUp(r) ==
    /\ caller[r] = "waiting" /\ caller' = [caller EXCEPT ![r] = "gaveup"]
    /\ IF Open(r) THEN slot' = [slot EXCEPT ![r] = "abandoned"]
                       /\ discharges' = [discharges EXCEPT ![r] = @ + 1]
       ELSE U(<<slot, discharges>>)
    /\ U(<<now, sent, dl, holder, okAt, jobs, down, phase, turnA, panicked>>)

------------------------------------------------------------------------------
\* Owner A: turns

\* Dispatched even if the slot is already terminal (stale skipping is an
\* opt-in policy, rank 19).
DispatchA(r) ==
    /\ phase = "run" /\ turnA = 0 /\ holder[r] = "qA"
    /\ holder' = [holder EXCEPT ![r] = "turnA"] /\ turnA' = r
    /\ U(<<now, caller, sent, dl, slot, discharges, okAt, jobs, down, phase, panicked>>)

InTurn(r, h) == phase = "run" /\ turnA = r /\ holder[r] = h

ReplyNow(r) ==
    /\ InTurn(r, "turnA") /\ holder' = [holder EXCEPT ![r] = "gone"] /\ TryReply(r)
    /\ U(<<now, sent, dl, jobs, down, phase, turnA, panicked>>)

\* defer(): a credit for an open slot, Overloaded at the bound, and no
\* credit for a slot that is already closed.
Defer(r) ==
    /\ InTurn(r, "turnA")
    /\ IF ~Open(r) THEN holder' = [holder EXCEPT ![r] = "gone"] /\ Keep
       ELSE IF Credits >= BoundA /\ Fault # "Unbounded"
       THEN holder' = [holder EXCEPT ![r] = "gone"] /\ Discharge(r, "overloaded")
       ELSE holder' = [holder EXCEPT ![r] = "localA"] /\ Keep
    /\ U(<<now, sent, dl, jobs, down, phase, turnA, panicked>>)

Store(r) ==
    /\ InTurn(r, "localA") /\ holder' = [holder EXCEPT ![r] = "stateA"] /\ Keep
    /\ U(<<now, sent, dl, jobs, down, phase, turnA, panicked>>)

\* Dropping the deferred ReplyTo inside the turn (an unused PendingReply
\* is handled by EndTurn or Panic).
DropInTurn(r) ==
    /\ InTurn(r, "localA")
    /\ holder' = [holder EXCEPT ![r] = "dropA"] /\ Keep
    /\ U(<<now, sent, dl, jobs, down, phase, turnA, panicked>>)

\* pipe_reply: shares the token's credit only if the token is open and no
\* job of this request runs; a closed token is handed back (no job).
PipeReply(r) ==
    /\ InTurn(r, "localA")
    /\ \/ /\ Open(r) /\ jobs[r] < 2 /\ (jobs[r] = 0 \/ Fault = "ShareRunning")
          /\ jobs' = [jobs EXCEPT ![r] = @ + 1] /\ holder' = [holder EXCEPT ![r] = "doneA"]
       \/ /\ ~Open(r) /\ jobs[r] < 2 /\ Fault = "AttachClosed"
          /\ jobs' = [jobs EXCEPT ![r] = @ + 1] /\ holder' = [holder EXCEPT ![r] = "doneA"]
       \/ /\ (~Open(r) \/ jobs[r] > 0) /\ Fault \notin {"ShareRunning", "AttachClosed"}
          /\ IF Credits < BoundA /\ Open(r) /\ jobs[r] < 2
             THEN jobs' = [jobs EXCEPT ![r] = @ + 1] /\ holder' = [holder EXCEPT ![r] = "doneA"]
             ELSE U(<<jobs, holder>>)              \* PipeReplyError hands the token back
    /\ Keep /\ U(<<now, sent, dl, down, phase, turnA, panicked>>)

\* Turn returns: tokens dropped in the turn, or a PendingReply never used,
\* answer Dropped; a ReplyTo left in a local variable is dropped too.
EndTurn ==
    /\ phase = "run" /\ turnA # 0
    /\ LET r == turnA IN
       /\ holder[r] \in {"gone", "stateA", "doneA", "dropA", "turnA", "localA"}
       /\ IF holder[r] \in {"dropA", "turnA", "localA"}
          THEN holder' = [holder EXCEPT ![r] = "gone"]
               /\ IF Open(r) /\ Fault # "NoAutoReply" THEN Discharge(r, "dropped") ELSE Keep
          ELSE U(holder) /\ Keep
    /\ turnA' = 0
    /\ U(<<now, sent, dl, jobs, down, phase, panicked>>)

\* Panic in a turn: the turn's tokens are dropped while unwinding and
\* answer Stopped; exit begins.
Panic ==
    /\ phase = "run" /\ turnA # 0
    /\ LET r == turnA IN
       /\ panicked' = panicked \cup (IF holder[r] \in {"turnA", "localA", "dropA"} /\ Open(r)
                                     THEN {r} ELSE {})
       /\ IF holder[r] \in {"turnA", "localA", "dropA"}
          THEN holder' = [holder EXCEPT ![r] = "gone"]
               /\ IF Open(r) THEN Discharge(r, IF Fault = "UnwindDropped" THEN "dropped"
                                               ELSE "stopped")
                  ELSE Keep
          ELSE U(holder) /\ Keep
    /\ phase' = "exiting" /\ turnA' = 0
    /\ U(<<now, sent, dl, jobs, down>>)

\* A later turn, or stopped() while exiting, answers or drops a stored token.
LaterReply(r) ==
    /\ phase \in {"run", "exiting", "stuck"} /\ turnA = 0 /\ holder[r] = "stateA"
    /\ holder' = [holder EXCEPT ![r] = "gone"] /\ TryReply(r)
    /\ U(<<now, sent, dl, jobs, down, phase, turnA, panicked>>)

LaterDrop(r) ==
    /\ phase \in {"run", "exiting", "stuck"} /\ turnA = 0 /\ holder[r] = "stateA"
    /\ holder' = [holder EXCEPT ![r] = "gone"] /\ DropNow(r)
    /\ U(<<now, sent, dl, jobs, down, phase, turnA, panicked>>)

ExitBegin ==
    /\ phase = "run" /\ turnA = 0 /\ phase' = "exiting"
    /\ U(<<now, caller, sent, dl, slot, holder, discharges, okAt, jobs, down, turnA, panicked>>)

\* Death: the registry closes every token A holds; the mailbox is discarded.
\* Cleanup deadline while stopped() still runs: the instance is reported
\* stuck; nothing is closed early, since ActorStopped means "gone".
CleanupDeadline ==
    /\ phase = "exiting" /\ phase' = "stuck"
    /\ IF Fault = "EarlyClose"
       THEN LET early == OpenOf({r \in Req : holder[r] \in {"stateA", "doneA"}})
            IN DischargeSet(early, "stopped") /\ earlyClosed' = earlyClosed \cup early
       ELSE Keep /\ UNCHANGED earlyClosed
    /\ U(<<now, sent, dl, holder, jobs, down, turnA, panicked>>)

Death ==
    /\ phase \in {"exiting", "stuck"} /\ phase' = "dead"
    /\ DischargeSet(OpenOf({r \in Req : holder[r] \in
                     (IF Fault = "ExitStateOnly" THEN {"qA", "stateA"}
                      ELSE {"qA", "stateA", "doneA"})}), "stopped")
    /\ U(<<now, sent, dl, holder, jobs, down, turnA, panicked>>)

------------------------------------------------------------------------------
\* Linked jobs and the downstream actor B

\* When a request's last job ends, its downstream request is abandoned.
EndDown(r) ==
    IF jobs'[r] = 0 /\ down[r] \in {"qB", "stateB"} /\ Fault # "LeakDownstream"
    THEN down' = [down EXCEPT ![r] = "abandoned"] ELSE U(down)

\* A job's notification (its result, or its deadline) runs done in a turn,
\* which receives the token back. After a job deadline the work keeps
\* running (jobs unchanged); after completion it has ended.
JobNotify(r) ==
    /\ phase = "run" /\ turnA = 0 /\ holder[r] = "doneA" /\ jobs[r] > 0
    /\ holder' = [holder EXCEPT ![r] = "localA"] /\ turnA' = r
    /\ \/ jobs' = [jobs EXCEPT ![r] = @ - 1] /\ EndDown(r)
       \/ U(<<jobs, down>>)
    /\ Keep /\ U(<<now, sent, dl, phase, panicked>>)

\* A running job whose notification was already taken (or cancelled) ends.
JobEnds(r) ==
    /\ jobs[r] > 0 /\ (holder[r] # "doneA" \/ jobs[r] = 2)
    /\ jobs' = [jobs EXCEPT ![r] = @ - 1] /\ EndDown(r)
    /\ Keep /\ U(<<now, sent, dl, holder, phase, turnA, panicked>>)

\* Cancellation (opt-in coupling to the caller, or owner exit): done and
\* its token are dropped; the work runs on and keeps its credit.
JobCancel(r) ==
    /\ holder[r] = "doneA"
    /\ (~Open(r) \/ phase = "exiting")
    /\ holder' = [holder EXCEPT ![r] = "gone"] /\ DropNow(r)
    /\ U(<<now, sent, dl, jobs, down, phase, turnA, panicked>>)

JobCallB(r) ==
    /\ jobs[r] > 0 /\ down[r] = "none" /\ down' = [down EXCEPT ![r] = "qB"]
    /\ Keep /\ U(<<now, sent, dl, holder, jobs, phase, turnA, panicked>>)

BDispatch(r) ==
    /\ down[r] = "qB"
    /\ \/ down' = [down EXCEPT ![r] = "done"]
       \/ down' = [down EXCEPT ![r] = IF Cardinality(HeldB) >= BoundB /\ Fault # "NoDestAdmission"
                                     THEN "done" ELSE "stateB"]
    /\ Keep /\ U(<<now, sent, dl, holder, jobs, phase, turnA, panicked>>)

BReply(r) ==
    /\ down[r] = "stateB" /\ down' = [down EXCEPT ![r] = "done"]
    /\ Keep /\ U(<<now, sent, dl, holder, jobs, phase, turnA, panicked>>)

------------------------------------------------------------------------------
\* Deadlines

Blocked(r) == Fault = "LoopDeadline" /\ turnA # 0
              /\ holder[r] \in {"qA", "turnA", "localA", "dropA", "stateA", "doneA"}

Shorten(r) ==
    /\ Open(r) /\ holder[r] \in {"localA", "stateA", "doneA"}
    /\ \E d \in now..MaxTime : d # dl[r] /\ (d < dl[r] \/ Fault = "Lengthen")
                               /\ dl' = [dl EXCEPT ![r] = d]
    /\ U(<<now, caller, sent, slot, holder, discharges, okAt, jobs, down, phase, turnA, panicked>>)

Expire(r) ==
    /\ Open(r) /\ now >= dl[r] /\ ~Blocked(r) /\ Discharge(r, "timeout")
    /\ U(<<now, sent, dl, holder, jobs, down, phase, turnA, panicked>>)

Tick ==
    /\ now < MaxTime
    /\ \A r \in Req : (Open(r) /\ dl[r] <= now) => Blocked(r)
    /\ now' = now + 1
    /\ U(<<caller, sent, dl, slot, holder, discharges, okAt, jobs, down, phase, turnA, panicked>>)

------------------------------------------------------------------------------

NextOther ==
    \/ Tick \/ EndTurn \/ Panic \/ ExitBegin \/ Death
    \/ \E r \in Req :
        \/ Send(r) \/ CallerGivesUp(r) \/ DispatchA(r) \/ ReplyNow(r) \/ Defer(r)
        \/ Store(r) \/ DropInTurn(r) \/ PipeReply(r) \/ LaterReply(r) \/ LaterDrop(r)
        \/ JobNotify(r) \/ JobEnds(r) \/ JobCancel(r) \/ JobCallB(r)
        \/ BDispatch(r) \/ BReply(r) \/ Shorten(r) \/ Expire(r)

Next ==
    \/ CleanupDeadline
    \/ (UNCHANGED earlyClosed /\ NextOther)

Spec == Init /\ [][Next]_vars
\* Fairness only for the clock and the deadline service.
FairSpec == Spec /\ WF_vars(Tick /\ UNCHANGED earlyClosed)
                 /\ \A r \in Req : WF_vars(Expire(r) /\ UNCHANGED earlyClosed)

------------------------------------------------------------------------------
\* Properties

TypeOK ==
    /\ now \in 0..MaxTime /\ holder \in [Req -> Holders]
    /\ slot \in [Req -> {"none", "open"} \cup Terminal]
    /\ jobs \in [Req -> 0..2] /\ phase \in {"run", "exiting", "stuck", "dead"}

AtMostOneDischarge == \A r \in Req : discharges[r] <= 1
NoOrphan ==
    \A r \in Req : Open(r) =>
        /\ phase # "dead"
        /\ holder[r] \in {"qA", "turnA", "localA", "dropA", "stateA", "doneA"}
NoSuccessAfterDeadline == \A r \in Req : slot[r] = "ok" => okAt[r] < dl[r]
TimelyExpiry == \A r \in Req : Open(r) => now <= dl[r]
DeadlineWithinCaller == \A r \in Req : slot[r] # "none" => dl[r] <= sent[r]
BoundedA == Credits <= BoundA
BoundedB == Cardinality(HeldB) <= BoundB
PanicStopped == \A r \in panicked : slot[r] = "stopped"
\* The registry closes only at actual death: no token is answered Stopped
\* by a close while the instance (its stopped()) is still alive.
NoEarlyClose == earlyClosed = {}
DownstreamOwned == \A r \in Req : down[r] \in {"qB", "stateB"} => jobs[r] > 0
CallerConsistent ==
    \A r \in Req : /\ caller[r] = "ok" => (slot[r] = "ok" /\ okAt[r] < sent[r])
                   /\ caller[r] = "timeout" => slot[r] = "timeout"

Safety ==
    /\ TypeOK /\ AtMostOneDischarge /\ NoOrphan /\ NoSuccessAfterDeadline
    /\ TimelyExpiry /\ DeadlineWithinCaller /\ BoundedA /\ BoundedB
    /\ PanicStopped /\ NoEarlyClose /\ DownstreamOwned /\ CallerConsistent

EveryAcceptedDischarged == \A r \in Req : (slot[r] = "open") ~> (slot[r] \in Terminal)
CallerNeverHangs == \A r \in Req : (caller[r] = "waiting") ~> (caller[r] # "waiting")
=============================================================================
