------------------------- MODULE DynamicSupervisor -------------------------
(***************************************************************************)
(* Rank 12a design model (not implemented): a dynamic supervisor, OTP's   *)
(* simple_one_for_one / Elixir's DynamicSupervisor. Design contract:      *)
(* docs/design/dynamic-supervisor.md (revision 3).                        *)
(*                                                                         *)
(* Slots 1..N are start_child calls made at any moment. TermCalls are     *)
(* terminate_child calls, each aimed at one slot either by the actor id   *)
(* the caller saw (the slot's last published generation at call time) or *)
(* by the slot (terminate_slot); several calls may target one slot.       *)
(*                                                                         *)
(* As in OTP, the supervisor is one control loop. Requests and child     *)
(* exits arrive in one FIFO queue `q` (start requests, terminate calls,   *)
(* exit notices, backoff retries). The loop takes the head, runs it to   *)
(* completion (`op` holds a start, restart or terminate in progress),     *)
(* then the next. The exit of the child an operation is waiting for is   *)
(* consumed by that operation directly, as OTP's start_child and          *)
(* shutdown/2 consume it. Children run, exit and clean up concurrently.   *)
(* Shutdown and failure close admission at once (B, tree-wide closure).   *)
(*                                                                         *)
(* Backoff (opt-in) is a constant: without it the restart attempt runs in *)
(* the same loop turn as the exit decision (OTP restart/2); a failed      *)
(* attempt re-queues its exit at the tail (OTP try_again_restart).        *)
(*                                                                         *)
(* Two fairness sets: CallerFairness has no progress for user cleanup     *)
(* (CleanupDone, StuckDone, StopObserved, Ready); only deadlines fire. It *)
(* checks that callers are answered even if cleanup never ends.           *)
(* Fairness adds cleanup progress, for shutdown completion and recovery. *)
(*                                                                         *)
(* Instances are bounded: the per-PR ones use two slots; targeted        *)
(* three-slot instances check admission order, the cap and the shared     *)
(* budget with more children. These are bounded checks, not a cutoff.     *)
(*                                                                         *)
(* Event invariants read history variables that GhostUpdate computes      *)
(* from each step's state change, separately from the actions.            *)
(* Fault selects a negative control (DYNAMIC_CONTROLS, negative_checks).  *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    N,            \* start_child calls (slots 1..N)
    ModeSet,      \* restart types a slot may have; chosen per slot in Init
    MaxChildren, MaxRestarts, MaxExpire,
    SelfExits,    \* reasons a ready child may exit with on its own
    StartFails,   \* non-deadline outcomes of a failed construction
    CleanupFails, \* {FALSE} or {FALSE, TRUE}: cleanup may record a failure
    TermCalls,    \* terminate_child calls
    CallSlot,     \* CallSlot[c]: slot the call is about
    CallKind,     \* CallKind[c] \in {"id", "slot", "expect"}: by actor id, by
                  \* slot, or by slot fenced on the expected id@generation
    BackoffOn,    \* backoff configured (opt-in)
    FanoutSteps,  \* shutdown fan-out as separate steps (refinement)
    StartDeadlines, \* startup_timeout exists (TRUE in every configuration;
                    \* FALSE only for the NoAbort control, so that shutdown's
                    \* cancellation is the only thing that answers a start)
    Fault

Slots    == 1..N
Modes    == {"Permanent", "Transient", "Temporary"}
Reasons  == {"none", "timeout", "Normal", "Shutdown", "Panic", "Kill", "Killed"}
Abnormal == {"none", "timeout", "Panic", "Kill", "Killed"}
Faults   == {"None",
    "IgnoreMax", "UncountedRestart", "RestartTemporary", "RestartTerminated",
    "AcceptDuringShutdown", "RestartAfterClose", "NoEscalate", "EarlyDone",
    "Sequential", "NoAbort", "StuckBackoff", "ConcurrentStart", "StuckFailsAll",
    "PublishAfterClose", "OverlapGeneration", "PublishBeforeReady", "PrematureOk",
    "UnchargedRetry", "ChargeFirstStart", "TransientNormalRestart",
    "PermanentRetires", "RetryFirstStart", "EarlyFailedReply", "NoRefund",
    "ExpireReserved", "TermReplyEarly", "LateFailedOverwrite",
    \* revision 3
    "LaterOp", "MisclassifyStuck", "EarlyRemove", "OverReserve", "LostRestart",
    "TermLost", "NoDeadline", "SkipSignal", "SlotLookup", "QueuedTermOk",
    "PublishAfterExit", "StuckResultLost", "IgnoreExpect",
    \* round 3
    "EarlyFree", "IgnoreFreesEarly", "IgnoreAfterClose"}

ASSUME N \in Nat \ {0} /\ MaxChildren \in Nat /\ MaxRestarts \in Nat /\ MaxExpire \in Nat
ASSUME ModeSet \subseteq Modes /\ ModeSet # {}
ASSUME SelfExits \subseteq Reasons /\ StartFails \subseteq Reasons
ASSUME CleanupFails \subseteq BOOLEAN /\ FALSE \in CleanupFails
ASSUME \A c \in TermCalls : CallSlot[c] \in Slots /\ CallKind[c] \in {"id", "slot", "expect"}
ASSUME BackoffOn \in BOOLEAN /\ FanoutSteps \in BOOLEAN /\ StartDeadlines \in BOOLEAN
ASSUME Fault \in Faults

States   == {"idle", "queued", "starting", "running", "cleanup", "decide",
             "backoff", "restarting", "stuck", "finalizing", "removed", "rejected"}
\* "finalizing": the loop decided to remove the slot; slot finalization
\* (dropping retained arguments) runs and the slot stays counted until it
\* ends (revision 3, Codex round 3).
Active   == {"starting", "running", "cleanup", "decide", "backoff", "finalizing",
             "restarting", "stuck"}
Terminal == {"idle", "removed", "rejected"}
Idle     == [k |-> "idle", s |-> 0, c |-> 0]

VARIABLES
    open, failed, exhausted, sphase,
    windowed, expired, reserved, stamped,
    q,          \* FIFO of [k, s, c, t]: "start", "exit", "retry", "term"
    ticket,     \* next arrival ticket
    served,     \* ticket of the last operation the loop took from the head
    op,         \* operation in progress [k, s, c]
    mode, sl, tc,
    \* history (GhostUpdate only)
    lateCommit, latePub, lateGenClose, lateGenTerm, badRestart, badRetire,
    restarts, outOfOrder, freedEarly, queuedAtClose, pendAtStuck

sup   == <<open, failed, exhausted, sphase, windowed, expired, reserved, stamped,
           q, ticket, served, op>>
ghost == <<lateCommit, latePub, lateGenClose, lateGenTerm, badRestart, badRetire,
           restarts, outOfOrder, freedEarly, queuedAtClose, pendAtStuck>>
vars  == <<sup, sl, tc, ghost, mode>>

SlotInit == [cs |-> "idle", gen |-> 0, alive |-> {}, pub |-> 0, lastPub |-> 0,
             reason |-> "none", cfail |-> FALSE, readyGen |-> FALSE,
             everReady |-> FALSE, result |-> "none", cause |-> "none",
             term |-> FALSE, stopSig |-> FALSE, stuck |-> "none",
             retryQ |-> FALSE, ign |-> FALSE, fstuck |-> FALSE]
\* terminate call: st \in none, queued, pending, ok, stuck, notfound, stopped;
\* g = the actor (generation) the caller named.
CallInit == [st |-> "none", g |-> 0]

TypeOK ==
    /\ open \in BOOLEAN /\ failed \in BOOLEAN /\ exhausted \in BOOLEAN
    /\ sphase \in {"running", "signal", "waiting", "done"}
    /\ windowed \in Nat /\ expired \in 0..MaxExpire /\ reserved \subseteq Slots
    /\ stamped \in Nat /\ ticket \in Nat /\ served \in Nat
    /\ op.k \in {"idle", "start", "restart", "term", "final"}
    /\ \A s \in Slots : sl[s].cs \in States /\ sl[s].alive \subseteq Nat
    /\ \A c \in TermCalls :
         tc[c].st \in {"none", "queued", "pending", "ok", "stuck", "notfound", "stopped", "stale"}

Init ==
    /\ open = TRUE /\ failed = FALSE /\ exhausted = FALSE /\ sphase = "running"
    /\ windowed = 0 /\ expired = 0 /\ reserved = {} /\ stamped = 0
    /\ q = <<>> /\ ticket = 1 /\ served = 0 /\ op = Idle
    /\ mode \in [Slots -> ModeSet]
    /\ sl = [s \in Slots |-> SlotInit] /\ tc = [c \in TermCalls |-> CallInit]
    /\ lateCommit = [s \in Slots |-> FALSE] /\ latePub = [s \in Slots |-> FALSE]
    /\ lateGenClose = [s \in Slots |-> FALSE] /\ lateGenTerm = [s \in Slots |-> FALSE]
    /\ badRestart = [s \in Slots |-> FALSE] /\ badRetire = [s \in Slots |-> FALSE]
    /\ restarts = 0 /\ outOfOrder = FALSE /\ freedEarly = [s \in Slots |-> FALSE]
    /\ queuedAtClose = [c \in TermCalls |-> FALSE]
    /\ pendAtStuck = [c \in TermCalls |-> FALSE]

-----------------------------------------------------------------------------
(* Helpers *)

Counted ==
    IF Fault = "UncountedRestart"
    THEN {s \in Slots : sl[s].cs \in {"starting", "running"}}
    ELSE {s \in Slots : sl[s].cs \in Active}
Budget == windowed + Cardinality(reserved)

Enq(e) == q' = Append(q, [e EXCEPT !.t = ticket]) /\ ticket' = ticket + 1
Rm(i)  == [j \in 1..Len(q) - 1 |-> IF j < i THEN q[j] ELSE q[j + 1]]
IsHead(k, s, c) == Len(q) > 0 /\ q[1].k = k /\ q[1].s = s /\ q[1].c = c
\* Index of the exit notice of slot s, if queued.
ExitIdx(s) == {i \in 1..Len(q) : q[i].k = "exit" /\ q[i].s = s}
LoopIdle == op.k = "idle" \/ Fault = "ConcurrentStart"

DesignAllowsRestart(m, r, cf) ==
    CASE m = "Temporary" -> FALSE
      [] m = "Permanent" -> TRUE
      [] m = "Transient" -> r \in {"Panic", "Kill", "Killed"} \/ cf
\* An attempt that ignored is dropped, never restarted (OTP: ignore on a
\* restart removes the child).
Allowed(s) ==
    /\ sl[s].everReady /\ ~sl[s].ign
    /\ \/ DesignAllowsRestart(mode[s], sl[s].reason, sl[s].cfail)
       \/ ~sl[s].readyGen /\ mode[s] # "Temporary"

WouldRestart(s) ==
    /\ sl[s].everReady \/ Fault = "RetryFirstStart"
    /\ \/ mode[s] = "Permanent" /\ Fault # "PermanentRetires"
       \/ mode[s] = "Transient"
          /\ (sl[s].reason \in Abnormal \/ sl[s].cfail \/ Fault = "TransientNormalRestart")
       \/ mode[s] # "Temporary" /\ ~sl[s].readyGen
       \/ mode[s] = "Temporary" /\ Fault = "RestartTemporary"

Fail == failed' = TRUE /\ open' = FALSE

\* The pending terminate call that holds the loop for slot s, if any.
HeldCall(s) == IF op.k = "term" /\ op.s = s THEN {op.c} ELSE {}

-----------------------------------------------------------------------------
(* Callers *)

\* start_child: queued, or rejected at once (arguments returned) if closed.
Request(s) ==
    /\ sl[s].cs = "idle"
    /\ IF open
       THEN /\ sl' = [sl EXCEPT ![s].cs = "queued", ![s].result = "pending"]
            /\ Enq([k |-> "start", s |-> s, c |-> 0, t |-> 0])
       ELSE /\ sl' = [sl EXCEPT ![s].cs = "rejected", ![s].result = "stopped"]
            /\ UNCHANGED <<q, ticket>>
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, served, op, tc>>

\* The loop admits the start at the head (LaterOp: any queued start).
LoopAdmit(s) ==
    /\ sl[s].cs = "queued" /\ LoopIdle
    /\ open \/ Fault = "AcceptDuringShutdown"
    /\ \E i \in 1..Len(q) :
        /\ q[i].k = "start" /\ q[i].s = s
        /\ i = 1 \/ Fault = "LaterOp"
        /\ q' = Rm(i) /\ served' = q[i].t
    /\ IF Cardinality(Counted) >= MaxChildren /\ Fault # "IgnoreMax"
       THEN /\ sl' = [sl EXCEPT ![s].cs = "rejected", ![s].result = "max"]
            /\ UNCHANGED <<op, stamped, windowed>>
       ELSE /\ sl' = [sl EXCEPT ![s].cs = "starting", ![s].gen = 1, ![s].alive = {1},
                       ![s].result = IF Fault = "PrematureOk" THEN "ok" ELSE "pending"]
            /\ op' = IF Fault = "ConcurrentStart" THEN op ELSE [k |-> "start", s |-> s, c |-> 0]
            /\ IF Fault = "ChargeFirstStart"
               THEN stamped' = stamped + 1 /\ windowed' = windowed + 1
               ELSE UNCHANGED <<stamped, windowed>>
    /\ UNCHANGED <<open, failed, exhausted, sphase, expired, reserved, ticket, tc>>

\* Shutdown answers queued starts and terminate calls (Stopped, args back).
RejectQueued(s) ==
    /\ sl[s].cs = "queued" /\ ~open /\ Fault # "AcceptDuringShutdown"
    /\ \E i \in 1..Len(q) : q[i].k = "start" /\ q[i].s = s /\ q' = Rm(i)
    /\ sl' = [sl EXCEPT ![s].cs = "rejected", ![s].result = "stopped"]
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, ticket, served, op, tc>>

\* terminate_child(id) or terminate_slot(slot, expect). The caller may name
\* any generation it ever saw published, the current one or a superseded
\* one (a stale id, an outdated expect), so stale calls replay (agy round 3).
TermCall(c) ==
    /\ tc[c].st = "none" /\ sl[CallSlot[c]].everReady
    /\ IF open
       THEN /\ \E g \in (IF CallKind[c] = "slot" THEN {0} ELSE 1..sl[CallSlot[c]].lastPub) :
                 tc' = [tc EXCEPT ![c] = [st |-> "queued", g |-> g]]
            /\ Enq([k |-> "term", s |-> CallSlot[c], c |-> c, t |-> 0])
       ELSE /\ tc' = [tc EXCEPT ![c].st = "stopped"] /\ UNCHANGED <<q, ticket>>
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, served, op, sl>>

RejectQueuedTerm(c) ==
    /\ tc[c].st = "queued" /\ ~open
    /\ \E i \in 1..Len(q) : q[i].k = "term" /\ q[i].c = c /\ q' = Rm(i)
    /\ tc' = [tc EXCEPT ![c].st = IF Fault = "QueuedTermOk" THEN "ok" ELSE "stopped"]
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, ticket, served, op, sl>>

\* The loop looks the target up when it handles the call (Elixir).
LoopTerm(c) ==
    /\ tc[c].st = "queued" /\ op.k = "idle" /\ open /\ IsHead("term", CallSlot[c], c)
    /\ q' = Tail(q) /\ served' = q[1].t
    /\ LET s == CallSlot[c]
           named   == CallKind[c] = "slot" \/ sl[s].lastPub = tc[c].g
           live    == sl[s].cs \in Active \ {"stuck"}
           \* expect: the slot exists but its current published child moved on
           stale   == CallKind[c] = "expect" /\ sl[s].cs \in Active
                      /\ sl[s].lastPub # tc[c].g /\ Fault # "IgnoreExpect"
           found   == live /\ (named \/ Fault = "SlotLookup"
                              \/ (CallKind[c] = "expect" /\ Fault = "IgnoreExpect"))
       IN IF stale
          THEN /\ tc' = [tc EXCEPT ![c].st = "stale"] /\ UNCHANGED <<sl, op>>
          ELSE IF sl[s].cs = "stuck" /\ named
          THEN /\ tc' = [tc EXCEPT ![c].st = "stuck"] /\ UNCHANGED <<sl, op>>
          ELSE IF ~found
          THEN /\ tc' = [tc EXCEPT ![c].st = "notfound"] /\ UNCHANGED <<sl, op>>
          ELSE /\ tc' = [tc EXCEPT ![c].st = "pending"]
               /\ sl' = IF Fault = "TermLost" THEN sl
                        ELSE [sl EXCEPT ![s].term = TRUE, ![s].stopSig = TRUE]
               /\ op' = [k |-> "term", s |-> s, c |-> c]
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, ticket>>

\* The terminated child was removed: Ok, and the loop moves on.
TermResolve(c) ==
    /\ tc[c].st = "pending"
    /\ sl[CallSlot[c]].cs = "removed" \/ Fault = "TermReplyEarly"
    /\ tc' = [tc EXCEPT ![c].st = "ok"]
    /\ op' = IF op.k = "term" /\ op.c = c THEN Idle ELSE op
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, q, ticket, served, sl>>

Close ==
    /\ open /\ open' = FALSE
    /\ UNCHANGED <<failed, exhausted, sphase, windowed, expired, reserved, stamped,
                   q, ticket, served, op, sl, tc>>

WindowExpire ==
    /\ expired < MaxExpire
    /\ IF windowed > 0
       THEN windowed' = windowed - 1 /\ UNCHANGED reserved
       ELSE /\ Fault = "ExpireReserved" /\ reserved # {}
            /\ \E r \in reserved : reserved' = reserved \ {r}
            /\ UNCHANGED windowed
    /\ expired' = expired + 1
    /\ UNCHANGED <<open, failed, exhausted, sphase, stamped, q, ticket, served, op, sl, tc>>

-----------------------------------------------------------------------------
(* Children *)

Unchanged1 == UNCHANGED <<open, failed, exhausted, sphase, windowed, expired,
                          reserved, stamped, q, ticket, served, op, tc>>

\* Readiness commits while open; a first start's Ok is committed now.
Ready(s) ==
    /\ sl[s].cs \in {"starting", "restarting"}
    /\ \/ open
       \/ Fault = "AcceptDuringShutdown" /\ sl[s].cs = "starting"
       \/ Fault = "PublishAfterClose" /\ sl[s].cs = "restarting"
    /\ sl' = [sl EXCEPT ![s].cs = "running", ![s].pub = sl[s].gen,
                        ![s].lastPub = sl[s].gen,
                        ![s].readyGen = TRUE, ![s].everReady = TRUE,
                        ![s].result = IF sl[s].cs = "starting" THEN "ok" ELSE sl[s].result]
    /\ op' = IF op.s = s /\ op.k # "term" THEN Idle ELSE op
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, q, ticket, served, tc>>

\* The factory returned ChildStart::Ignore or started() returned
\* Init::ignore(): nothing will be supervised (OTP {ok, undefined}), but the
\* attempt's state and owned work are cleaned up like any exit, and the slot
\* is finalized, before the answer and before the slot frees. The outcome is
\* decided now (cause "ignored"), answered after cleanup or at its deadline.
Ignore(s) ==
    /\ sl[s].cs \in {"starting", "restarting"}
    \* an ignore decides the outcome: only while admission is open (agy round 3)
    /\ \/ open
       \/ Fault \in {"AcceptDuringShutdown", "IgnoreAfterClose"} /\ sl[s].cs = "starting"
    /\ IF Fault = "IgnoreFreesEarly"
       THEN sl' = [sl EXCEPT ![s].cs = "removed", ![s].alive = {}, ![s].pub = 0,
                     ![s].result = IF sl[s].cs = "starting" THEN "ignored" ELSE sl[s].result]
       ELSE sl' = [sl EXCEPT ![s].cs = "cleanup", ![s].reason = "Normal", ![s].pub = 0,
                     ![s].ign = TRUE,
                     ![s].cause = IF sl[s].cs = "starting" THEN "ignored" ELSE sl[s].cause]
    /\ op' = IF Fault = "IgnoreFreesEarly" /\ op.s = s THEN Idle ELSE op
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, q, ticket, served, tc>>

FailStart(s, r) ==
    /\ sl' = [sl EXCEPT ![s].cs = IF Fault = "EarlyRemove" /\ sl[s].cs = "starting"
                                  THEN "removed" ELSE "cleanup",
                        ![s].reason = r, ![s].pub = 0,
                        ![s].cause = IF sl[s].cs = "starting" THEN "failed" ELSE sl[s].cause,
                        ![s].result = IF Fault \in {"EarlyFailedReply", "EarlyRemove"}
                                         /\ sl[s].cs = "starting"
                                      THEN "failed" ELSE sl[s].result]

\* Factory error or panic, panic or stop in started().
StartFail(s, r) ==
    /\ sl[s].cs \in {"starting", "restarting"} /\ r \in StartFails
    /\ FailStart(s, r) /\ Unchanged1

\* startup_timeout fired (a deadline: fair in both fairness sets).
StartTimeout(s) ==
    /\ StartDeadlines /\ sl[s].cs \in {"starting", "restarting"}
    /\ FailStart(s, "timeout") /\ Unchanged1

AbortStart(s) ==
    /\ sl[s].cs \in {"starting", "restarting"} /\ ~open /\ Fault # "NoAbort"
    /\ sl' = [sl EXCEPT ![s].cs = "cleanup", ![s].reason = "Shutdown", ![s].pub = 0,
                        ![s].cause = IF sl[s].cs = "starting" THEN "aborted" ELSE sl[s].cause]
    /\ Unchanged1

SelfExit(s, r) ==
    /\ sl[s].cs = "running" /\ r \in SelfExits
    /\ sl' = [sl EXCEPT ![s].cs = "cleanup", ![s].reason = r,
                        ![s].pub = IF Fault = "PublishAfterExit" THEN sl[s].pub ELSE 0]
    /\ Unchanged1

StopObserved(s, r) ==
    /\ sl[s].cs = "running" /\ sl[s].stopSig /\ r \in {"Shutdown", "Killed"}
    /\ sl' = [sl EXCEPT ![s].cs = "cleanup", ![s].reason = r, ![s].pub = 0]
    /\ Unchanged1

\* Owned cleanup finished. The exit is queued for the loop (OTP 'EXIT'),
\* unless the operation in progress waits for this very child (a start or
\* a terminate), which consumes it directly: no queue entry, no ticket. A
\* failed restart attempt gives the loop back and re-queues
\* (try_again_restart).
CleanupDone(s, f) ==
    /\ sl[s].cs = "cleanup" /\ f \in CleanupFails
    /\ sl' = [sl EXCEPT ![s].cs = "decide", ![s].cfail = f,
                        ![s].alive = sl[s].alive \ {sl[s].gen}]
    /\ IF op.s = s /\ op.k \in {"start", "term"}
       THEN UNCHANGED <<q, ticket>>
       ELSE Enq([k |-> "exit", s |-> s, c |-> 0, t |-> 0])
    /\ op' = IF op.k = "restart" /\ op.s = s THEN Idle ELSE op
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, served, tc>>

\* A deadline was missed: signalled but still running after the kill and
\* the second interval, owned cleanup past its deadline, or slot
\* finalization past its deadline. The slot stays
\* counted. Results waiting on it are committed now and stay (historical).
\* Kind: "restart" only for a child the table would restart, judged from
\* the exit reason alone (a stall is not a recorded cleanup failure);
\* "closing" during shutdown; otherwise "retire" (fails only the child).
CleanupStuck(s) ==
    /\ Fault # "NoDeadline"
    \* generation deadlines once per slot life; the finalization deadline has
    \* its own flag, so a slot that was stuck before and then reached
    \* finalization can still miss (and report) that deadline
    /\ \/ sl[s].stuck = "none" /\ sl[s].cs = "cleanup"
       \/ sl[s].stuck = "none" /\ sl[s].cs = "running" /\ sl[s].stopSig
       \/ sl[s].cs = "finalizing" /\ ~sl[s].fstuck
    /\ LET restartable ==
               /\ sl[s].everReady /\ ~sl[s].term /\ sl[s].cs = "cleanup"
               /\ \/ DesignAllowsRestart(mode[s], sl[s].reason, FALSE)
                  \/ ~sl[s].readyGen /\ mode[s] # "Temporary"
           kind == IF ~open THEN "closing"
                   ELSE IF restartable \/ (Fault = "MisclassifyStuck" /\ ~sl[s].term)
                        THEN "restart" ELSE "retire"
       IN /\ sl' = [sl EXCEPT ![s].cs = "stuck", ![s].stuck = kind, ![s].pub = 0,
                              ![s].fstuck = sl[s].fstuck \/ sl[s].cs = "finalizing",
                              ![s].result = IF sl[s].result = "pending"
                                            THEN sl[s].cause ELSE sl[s].result]
          /\ IF kind # "retire" \/ Fault = "StuckFailsAll"
             THEN Fail ELSE UNCHANGED <<open, failed>>
    /\ tc' = [c \in TermCalls |->
                IF c \in HeldCall(s) /\ Fault # "StuckResultLost"
                THEN [tc[c] EXCEPT !.st = "stuck"] ELSE tc[c]]
    /\ op' = IF op.s = s /\ (op.k # "term" \/ Fault # "StuckResultLost") THEN Idle ELSE op
    /\ UNCHANGED <<exhausted, sphase, windowed, expired, reserved, stamped, q, ticket, served>>

\* A stuck child (or finalization) finally ended; finalization follows.
StuckDone(s) ==
    /\ sl[s].cs = "stuck"
    /\ sl' = [sl EXCEPT ![s].cs = "finalizing", ![s].alive = {}]
    /\ Unchanged1

\* Slot finalization ended: retained arguments dropped, then capacity and
\* the max_children slot released. A start's answer is given now.
Finalize(s) ==
    /\ sl[s].cs = "finalizing"
    /\ sl' = [sl EXCEPT ![s].cs = "removed",
                 ![s].result = IF sl[s].result = "pending"
                               THEN IF Fault = "LateFailedOverwrite" /\ ~open
                                    THEN "aborted" ELSE sl[s].cause
                               ELSE sl[s].result]
    /\ op' = IF op = [k |-> "final", s |-> s, c |-> 0] THEN Idle ELSE op
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, q, ticket, served, tc>>

\* The loop handles an exit: at the head of the queue, or directly when the
\* operation in progress waits for this child (start or terminate).
Decide(s) ==
    /\ \/ sl[s].cs = "decide"
       \/ Fault = "OverlapGeneration" /\ sl[s].cs = "cleanup"
    /\ \/ \E i \in ExitIdx(s) :
           /\ \/ i = 1 /\ LoopIdle
              \/ op.s = s /\ op.k \in {"start", "term"}
           /\ q' = Rm(i)
           /\ served' = IF i = 1 /\ LoopIdle THEN q[i].t ELSE served
       \* consumed directly by the operation waiting for this child
       \/ /\ op.s = s /\ op.k \in {"start", "term"} /\ ExitIdx(s) = {}
          /\ sl[s].cs = "decide"
          /\ UNCHANGED <<q, served>>
       \/ Fault = "OverlapGeneration" /\ sl[s].cs = "cleanup" /\ LoopIdle
          /\ UNCHANGED <<q, served>>
    /\ LET ownStart == op.s = s /\ op.k = "start"
           \* removal: finalize; the loop waits for it unless a terminate holds it
           Remove == /\ sl' = [sl EXCEPT ![s].cs = IF Fault = "EarlyFree" THEN "removed"
                                                  ELSE "finalizing"]
                     /\ op' = IF op.k = "term" /\ op.s = s THEN op
                              ELSE IF Fault = "EarlyFree" THEN Idle
                              ELSE [k |-> "final", s |-> s, c |-> 0]
       IN IF ~sl[s].everReady /\ Fault # "RetryFirstStart"
       THEN \* first-start failure or ignore: answered after finalization
            /\ Remove
            /\ UNCHANGED <<open, failed, exhausted, reserved, stamped, windowed>>
       ELSE IF ~open \/ (sl[s].term /\ Fault # "RestartTerminated") \/ sl[s].ign
       THEN /\ Remove
            /\ UNCHANGED <<open, failed, exhausted, reserved, stamped, windowed>>
       ELSE IF WouldRestart(s)
       THEN IF Budget < MaxRestarts \/ Fault = "OverReserve"
            THEN IF BackoffOn \/ Fault = "LostRestart"
                 THEN \* reserve, wait off the loop
                      /\ sl' = [sl EXCEPT ![s].cs = "backoff"]
                      /\ reserved' = reserved \cup {s}
                      /\ op' = IF ownStart THEN Idle ELSE op
                      /\ UNCHANGED <<open, failed, exhausted, stamped, windowed>>
                 ELSE \* reserve and stamp now; the attempt runs in this turn (OTP)
                      /\ LET g == sl[s].gen + 1 IN
                         sl' = [sl EXCEPT ![s].cs = "restarting", ![s].gen = g,
                                ![s].alive = sl[s].alive \cup {g},
                                ![s].pub = IF Fault = "PublishBeforeReady" THEN g ELSE 0,
                                ![s].reason = "none", ![s].cfail = FALSE, ![s].readyGen = FALSE]
                      /\ IF Fault = "UnchargedRetry" /\ ~sl[s].readyGen
                         THEN UNCHANGED <<stamped, windowed>>
                         ELSE stamped' = stamped + 1 /\ windowed' = windowed + 1
                      /\ op' = [k |-> "restart", s |-> s, c |-> 0]
                      /\ UNCHANGED <<open, failed, exhausted, reserved>>
            ELSE /\ exhausted' = TRUE
                 /\ Remove
                 /\ IF Fault = "NoEscalate" THEN UNCHANGED <<open, failed>> ELSE Fail
                 /\ UNCHANGED <<reserved, stamped, windowed>>
       ELSE /\ Remove
            /\ UNCHANGED <<open, failed, exhausted, reserved, stamped, windowed>>
    /\ UNCHANGED <<sphase, expired, ticket, tc>>

\* The backoff timer fires: the retry joins the queue.
BackoffFire(s) ==
    /\ sl[s].cs = "backoff" /\ ~sl[s].retryQ /\ open /\ Fault # "LostRestart"
    /\ sl' = [sl EXCEPT ![s].retryQ = TRUE]
    /\ Enq([k |-> "retry", s |-> s, c |-> 0, t |-> 0])
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved,
                   stamped, served, op, tc>>

\* The loop takes the retry: stamp the reservation, construct.
BackoffDone(s) ==
    /\ sl[s].cs = "backoff" /\ sl[s].retryQ /\ LoopIdle
    /\ IsHead("retry", s, 0)
    /\ open \/ Fault = "RestartAfterClose"
    /\ ~sl[s].term \/ Fault = "RestartTerminated"
    /\ q' = Tail(q) /\ served' = q[1].t
    /\ LET g == sl[s].gen + 1 IN
       sl' = [sl EXCEPT ![s].cs = "restarting", ![s].gen = g,
              ![s].alive = sl[s].alive \cup {g}, ![s].retryQ = FALSE,
              ![s].pub = IF Fault = "PublishBeforeReady" THEN g ELSE 0,
              ![s].reason = "none", ![s].cfail = FALSE, ![s].readyGen = FALSE]
    /\ reserved' = reserved \ {s}
    /\ IF Fault = "UnchargedRetry" /\ ~sl[s].readyGen
       THEN UNCHANGED <<stamped, windowed>>
       ELSE stamped' = stamped + 1 /\ windowed' = windowed + 1
    /\ op' = IF Fault = "ConcurrentStart" THEN op ELSE [k |-> "restart", s |-> s, c |-> 0]
    /\ UNCHANGED <<open, failed, exhausted, sphase, expired, ticket, tc>>

\* Shutdown or terminate cancels a pending restart and refunds it.
CancelBackoff(s) ==
    /\ sl[s].cs = "backoff"
    /\ \/ ~open /\ Fault # "StuckBackoff"
       \/ sl[s].term /\ Fault # "RestartTerminated"
    /\ sl' = [sl EXCEPT ![s].cs = "finalizing", ![s].retryQ = FALSE]
    /\ q' = SelectSeq(q, LAMBDA e : ~(e.k = "retry" /\ e.s = s))
    /\ reserved' = IF Fault = "NoRefund" THEN reserved ELSE reserved \ {s}
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, stamped,
                   ticket, served, op, tc>>

-----------------------------------------------------------------------------
(* Shutdown *)

\* Atomic fan-out: every admitted child signalled in one step.
SupSignal ==
    /\ ~FanoutSteps /\ sphase = "running" /\ ~open
    /\ sl' = [s \in Slots |->
                IF Fault = "SkipSignal" /\ s = 1 THEN sl[s]
                ELSE [sl[s] EXCEPT !.stopSig = TRUE]]
    /\ sphase' = "waiting"
    /\ UNCHANGED <<open, failed, exhausted, windowed, expired, reserved, stamped,
                   q, ticket, served, op, tc>>

\* Refinement: one signal per step, interleaved with the children.
\* Sequential waits for each signalled child to finish first.
SupSignalStep ==
    /\ FanoutSteps /\ sphase \in {"running", "signal"} /\ ~open
    /\ Fault = "Sequential" => \A s \in Slots : sl[s].stopSig => sl[s].cs \notin Active
    /\ LET todo == {s \in Slots : ~sl[s].stopSig /\ sl[s].cs \in Active}
       IN IF todo = {}
          THEN sphase' = "waiting" /\ UNCHANGED sl
          ELSE \E s \in todo :
                 /\ sl' = [sl EXCEPT ![s].stopSig = TRUE]
                 \* the last signal ends the fan-out (no extra unlogged step)
                 /\ sphase' = IF todo = {s} THEN "waiting" ELSE "signal"
    /\ UNCHANGED <<open, failed, exhausted, windowed, expired, reserved, stamped,
                   q, ticket, served, op, tc>>

SupDone ==
    /\ sphase = "waiting"
    /\ (\A s \in Slots : sl[s].cs \in Terminal) \/ Fault = "EarlyDone"
    /\ sphase' = "done"
    /\ UNCHANGED <<open, failed, exhausted, windowed, expired, reserved, stamped,
                   q, ticket, served, op, sl, tc>>

-----------------------------------------------------------------------------

Grew(s) == sl'[s].gen > sl[s].gen
\* A restart was decided (reservation, attempt or exhaustion) for slot s.
Decided(s) ==
    /\ sl[s].cs = "decide"
    /\ \/ sl'[s].cs \in {"backoff", "restarting"}
       \/ exhausted' /\ ~exhausted /\ sl'[s].cs \in {"finalizing", "removed"}

GhostUpdate ==
    \* a start outcome committed after close: Ok, or the ignore decision
    /\ lateCommit' = [s \in Slots |-> lateCommit[s] \/
                        (((sl'[s].result = "ok" /\ sl[s].result # "ok")
                          \/ (sl'[s].cause = "ignored" /\ sl[s].cause # "ignored")) /\ ~open)]
    /\ latePub' = [s \in Slots |-> latePub[s] \/
                        (sl'[s].pub # sl[s].pub /\ sl'[s].pub # 0 /\ (~open \/ sl[s].term))]
    /\ lateGenClose' = [s \in Slots |-> lateGenClose[s] \/ (Grew(s) /\ ~open)]
    /\ lateGenTerm' = [s \in Slots |-> lateGenTerm[s] \/ (Grew(s) /\ sl[s].term)]
    /\ badRestart' = [s \in Slots |-> badRestart[s] \/
                        (((Grew(s) /\ sl[s].gen > 0) \/ Decided(s)) /\ ~Allowed(s))]
    /\ badRetire' = [s \in Slots |-> badRetire[s] \/
                        (sl[s].cs = "decide" /\ sl'[s].cs \in {"finalizing", "removed"} /\ open'
                         /\ ~sl[s].term /\ Allowed(s))]
    /\ restarts' = restarts + Cardinality({s \in Slots : Grew(s) /\ sl[s].gen > 0})
    \* the loop selected an operation: it must be the head at selection
    /\ outOfOrder' = (outOfOrder \/ ((served' # served) /\ ((Len(q) = 0) \/ (served' # q[1].t))))
    \* terminate calls still queued at the moment admission closed
    /\ queuedAtClose' = [c \in TermCalls |-> queuedAtClose[c] \/
                           (open /\ ~open' /\ tc[c].st = "queued")]
    \* terminate calls in progress at the moment their child got stuck
    /\ pendAtStuck' = [c \in TermCalls |-> pendAtStuck[c] \/
                         (tc[c].st = "pending" /\ sl[CallSlot[c]].stuck = "none"
                          /\ sl'[CallSlot[c]].stuck # "none")]
    /\ freedEarly' = [s \in Slots |-> freedEarly[s] \/
                        (sl'[s].cs = "removed" /\ sl[s].cs \notin {"finalizing", "removed"})]
    /\ UNCHANGED mode

SlotStep(s) ==
    \/ Request(s) \/ LoopAdmit(s) \/ RejectQueued(s)
    \/ Ready(s) \/ Ignore(s) \/ \E r \in StartFails : StartFail(s, r)
    \/ StartTimeout(s) \/ AbortStart(s)
    \/ \E r \in SelfExits : SelfExit(s, r)
    \/ \E r \in {"Shutdown", "Killed"} : StopObserved(s, r)
    \/ \E f \in CleanupFails : CleanupDone(s, f)
    \/ CleanupStuck(s) \/ StuckDone(s) \/ Finalize(s)
    \/ Decide(s) \/ BackoffFire(s) \/ BackoffDone(s) \/ CancelBackoff(s)

CallStep(c) == TermCall(c) \/ RejectQueuedTerm(c) \/ LoopTerm(c) \/ TermResolve(c)

Act ==
    \/ Close \/ WindowExpire \/ SupSignal \/ SupSignalStep \/ SupDone
    \/ \E s \in Slots : SlotStep(s)
    \/ \E c \in TermCalls : CallStep(c)

Next == Act /\ GhostUpdate

\* Loop work and deadlines only: no user cleanup is assumed to finish.
CallerFairness ==
    /\ \A s \in Slots :
         /\ WF_vars((LoopAdmit(s)) /\ GhostUpdate) /\ WF_vars((RejectQueued(s)) /\ GhostUpdate) /\ WF_vars((StartTimeout(s)) /\ GhostUpdate)
         /\ WF_vars((AbortStart(s)) /\ GhostUpdate) /\ WF_vars((CleanupStuck(s)) /\ GhostUpdate) /\ WF_vars((Decide(s)) /\ GhostUpdate)
         /\ WF_vars((BackoffFire(s)) /\ GhostUpdate) /\ WF_vars((BackoffDone(s)) /\ GhostUpdate) /\ WF_vars((CancelBackoff(s)) /\ GhostUpdate)
    /\ \A c \in TermCalls : WF_vars((RejectQueuedTerm(c)) /\ GhostUpdate) /\ WF_vars((LoopTerm(c)) /\ GhostUpdate) /\ WF_vars((TermResolve(c)) /\ GhostUpdate)
    /\ WF_vars((SupSignal) /\ GhostUpdate) /\ WF_vars((SupSignalStep) /\ GhostUpdate) /\ WF_vars((SupDone) /\ GhostUpdate)

\* Plus progress of children and cleanup.
Fairness ==
    /\ CallerFairness
    /\ \A s \in Slots :
         /\ WF_vars((Ready(s)) /\ GhostUpdate)
         /\ WF_vars((\E r \in {"Shutdown", "Killed"} : StopObserved(s, r)) /\ GhostUpdate)
         /\ WF_vars((\E f \in CleanupFails : CleanupDone(s, f)) /\ GhostUpdate)
         /\ WF_vars((StuckDone(s)) /\ GhostUpdate)
         /\ WF_vars((Finalize(s)) /\ GhostUpdate)

Spec       == Init /\ [][Next]_vars /\ Fairness
CallerSpec == Init /\ [][Next]_vars /\ CallerFairness

-----------------------------------------------------------------------------
(* Safety *)

AtMostOneGeneration == \A s \in Slots : Cardinality(sl[s].alive) <= 1

PublishedIsLive ==
    \A s \in Slots : sl[s].pub # 0 =>
        /\ sl[s].cs = "running" /\ sl[s].pub = sl[s].gen
        /\ sl[s].pub \in sl[s].alive /\ sl[s].readyGen

NoChildOutlivesSupervisor ==
    sphase = "done" => \A s \in Slots : sl[s].alive = {} /\ sl[s].cs \in Terminal

MaxChildrenBound == Cardinality({s \in Slots : sl[s].cs \in Active}) <= MaxChildren

SerialConstruction ==
    Cardinality({s \in Slots : sl[s].cs \in {"starting", "restarting"}}) <= 1

\* The loop takes queued operations in arrival order.
FifoOrder == ~outOfOrder

TemporaryNeverRestarted == \A s \in Slots : mode[s] = "Temporary" => sl[s].gen <= 1
TerminatedNeverRestarted == \A s \in Slots : ~lateGenTerm[s]
NoGenerationAfterClose == \A s \in Slots : ~lateGenClose[s]
NoPublishAfterStop == \A s \in Slots : ~latePub[s]
NoStartAfterShutdown == \A s \in Slots : ~lateCommit[s]

\* Restart decisions (reservation, attempt, exhaustion) only for exits the
\* table allows; no restartable exit is retired while open.
RestartFollowsPolicy == \A s \in Slots : ~badRestart[s] /\ ~badRetire[s]

FirstStartNeverRetried == \A s \in Slots : sl[s].gen > 1 => sl[s].everReady

\* FailedToStart/Aborted only once cleanup really finished, or with the
\* cleanup deadline failure recorded.
FailedAfterCleanup ==
    \A s \in Slots : sl[s].result \in {"failed", "aborted"} =>
        \/ sl[s].cs = "removed" /\ sl[s].alive = {}
        \/ sl[s].stuck # "none"

FailureCauseKept ==
    \A s \in Slots : sl[s].result \in {"failed", "aborted"} => sl[s].result = sl[s].cause

ChargedRestarts == restarts = stamped
ReservationsMatch == reserved = {s \in Slots : sl[s].cs = "backoff"}
BudgetBounded == Budget <= MaxRestarts
ExhaustionCloses == exhausted => failed /\ ~open

\* The supervisor fails only by exhaustion, a stuck child the table would
\* restart, or while closing; and a "restart" classification is checked
\* against the table, not trusted.
StuckIsolated ==
    /\ failed => exhausted \/ \E s \in Slots : sl[s].stuck \in {"restart", "closing"}
    /\ \A s \in Slots : sl[s].stuck = "restart" =>
           /\ ~sl[s].term /\ sl[s].everReady
           /\ \/ DesignAllowsRestart(mode[s], sl[s].reason, FALSE)
              \/ ~sl[s].readyGen /\ mode[s] # "Temporary"

\* A slot is freed (removed) only after slot finalization ran: the cap and
\* capacity cover retained arguments and an ignored attempt's cleanup.
RemovedAfterFinalize == \A s \in Slots : ~freedEarly[s]

\* A terminate call still queued when shutdown began is answered Stopped
\* (and stays so), never Ok or NotFound.
QueuedTermStopped ==
    \A c \in TermCalls : queuedAtClose[c] => tc[c].st \in {"queued", "stopped"}

ConcurrentShutdown ==
    sphase \in {"waiting", "done"} => \A s \in Slots : sl[s].cs = "running" => sl[s].stopSig

OkMeansReady == \A s \in Slots : sl[s].result = "ok" => sl[s].everReady

\* terminate results: Ok only after removal; a call whose child got stuck
\* while it waited is Stuck (and stays so); an id call or a fenced
\* (expect) slot call never reaches a generation other than the one named.
TermResultsSound ==
    \A c \in TermCalls :
        LET s == CallSlot[c] IN
        /\ tc[c].st = "ok" => sl[s].cs = "removed" /\ sl[s].alive = {}
        \* a call in progress when its child got stuck reports CleanupStuck
        /\ pendAtStuck[c] => tc[c].st = "stuck"
        /\ tc[c].st = "pending" => op = [k |-> "term", s |-> s, c |-> c]
        /\ tc[c].st = "pending" /\ CallKind[c] \in {"id", "expect"} => sl[s].lastPub = tc[c].g

Safety ==
    /\ TypeOK /\ AtMostOneGeneration /\ PublishedIsLive /\ NoChildOutlivesSupervisor
    /\ MaxChildrenBound /\ SerialConstruction /\ FifoOrder /\ TemporaryNeverRestarted
    /\ TerminatedNeverRestarted /\ NoGenerationAfterClose /\ NoPublishAfterStop
    /\ NoStartAfterShutdown /\ RestartFollowsPolicy /\ FirstStartNeverRetried
    /\ FailedAfterCleanup /\ FailureCauseKept /\ ChargedRestarts /\ ReservationsMatch
    /\ BudgetBounded /\ ExhaustionCloses /\ StuckIsolated /\ ConcurrentShutdown
    /\ OkMeansReady /\ TermResultsSound /\ RemovedAfterFinalize /\ QueuedTermStopped

NeverReopens == [][open' => open]_vars
\* A committed terminate result never changes.
TermResultStable ==
    [][\A c \in TermCalls :
        tc[c].st \in {"ok", "stuck", "notfound", "stopped", "stale"} => tc'[c].st = tc[c].st]_vars

(* Liveness *)
\* Under CallerSpec (cleanup may never end):
StartAnswered == \A s \in Slots :
    (sl[s].result = "pending") ~> (sl[s].result # "pending")
TerminateAnswered == \A c \in TermCalls :
    (tc[c].st \in {"queued", "pending"}) ~> (tc[c].st \notin {"queued", "pending"})
\* Every admitted child is signalled even if siblings never finish.
FanoutCompletes == (~open) ~> (sphase \in {"waiting", "done"})
\* Once shutdown began, every pending start is answered (by its abort and
\* cleanup deadline), even without startup deadlines (MCDynamicSupervisorAbort).
ShutdownAnswersStarts == \A s \in Slots :
    (~open /\ sl[s].result = "pending") ~> (sl[s].result # "pending")
\* Under Spec (cleanup ends):
ClosedLeadsToDone == (~open) ~> (sphase = "done")
RecoveryProgress == \A s \in Slots :
    (sl[s].cs \in {"decide", "backoff", "restarting"} /\ sl[s].everReady)
        ~> (sl[s].cs \in {"running", "finalizing", "removed", "stuck"})
=============================================================================
