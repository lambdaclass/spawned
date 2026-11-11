------------------------- MODULE DynamicSupervisor -------------------------
(***************************************************************************)
(* Rank 12a design model (not implemented): a dynamic supervisor, OTP's   *)
(* simple_one_for_one / Elixir's DynamicSupervisor. Design contract:      *)
(* docs/design/dynamic-supervisor.md (revision 2, after review).          *)
(*                                                                         *)
(* Slots 1..N are potential start_child calls; each is made at most once, *)
(* at any time, and each started child can get one terminate_child call.  *)
(* As in OTP, one control loop (`op`) serialises the supervisor's         *)
(* operations: admitting a start and constructing its first generation,   *)
(* a restart decision and the restart attempt, and a terminate_child,     *)
(* each runs to completion before the next. Children run, exit and clean  *)
(* up concurrently. Shutdown (Close) and failure close admission at once  *)
(* and cancel the operation in progress (B: tree-wide admission closure). *)
(*                                                                         *)
(* Restart types are chosen per slot in Init (`mode`), so one run covers  *)
(* every combination (a simple_one_for_one template has one type; mixing  *)
(* them is the general case). Two slots suffice: every property relates   *)
(* at most a pair of children (one holds the cap or restarts, the other   *)
(* races it); three slots exceed 100M states with revision 2's detail.    *)
(*                                                                         *)
(* Abstractions:                                                           *)
(*  - `open` is the admission gate and the supervisor's cancel together.  *)
(*  - Time is abstract. The rolling window is `windowed` (stamped grants  *)
(*    still in the window), released by WindowExpire at most MaxExpire    *)
(*    times, which also bounds the generations. Backoff is the "backoff"  *)
(*    state (zero-length when backoff is disabled, the default).          *)
(*  - The shutdown_timeout -> kill escalation is the nondeterministic     *)
(*    reason of StopObserved (Shutdown, or Killed after the kill). A      *)
(*    child that misses its deadlines, before its death point (signalled  *)
(*    but still running) or after it (owned cleanup), is CleanupStuck.    *)
(*  - The shutdown fan-out is one step (the implementation sends every    *)
(*    signal without awaiting between sends; each child's intervals run  *)
(*    from its own signal, design E3).                                    *)
(*                                                                         *)
(* Invariants that name an event (a late commit, a late generation, a     *)
(* policy violation) read history variables that GhostUpdate computes     *)
(* from every step's state change, independently of the actions, so a     *)
(* mutated action cannot hide its own violation.                          *)
(*                                                                         *)
(* Fault selects a negative control; negative_checks.py requires each to  *)
(* violate its named property (see DYNAMIC_CONTROLS there).               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
    N,            \* number of start_child calls (slots 1..N)
    ModeSet,      \* restart types a slot may have; chosen per slot in Init
    MaxChildren,  \* max_children
    MaxRestarts,  \* restart intensity (grants per window)
    MaxExpire,    \* how often the rolling window may release a grant
    SelfExits,    \* reasons a ready child may exit with on its own
    StartFails,   \* outcomes of a failed construction or readiness
    CleanupFails, \* {FALSE} or {FALSE, TRUE}: cleanup may record a failure
    Fault

Slots    == 1..N
Modes    == {"Permanent", "Transient", "Temporary"}
\* "none": factory error or panic (no actor reason); "timeout": startup
\* deadline; otherwise the actor's exit reason (v0.8 reasons, design E5).
Reasons  == {"none", "timeout", "Normal", "Shutdown", "Panic", "Kill", "Killed"}
Abnormal == {"none", "timeout", "Panic", "Kill", "Killed"}
Faults   == {"None",
    \* revision 1 controls
    "IgnoreMax", "UncountedRestart", "RestartTemporary", "RestartTerminated",
    "AcceptDuringShutdown", "RestartAfterClose", "NoEscalate", "EarlyDone",
    "Sequential", "NoAbort", "StuckBackoff",
    \* revision 2 controls (review findings)
    "ConcurrentStart", "StuckFailsAll", "PublishAfterClose", "OverlapGeneration",
    "PublishBeforeReady", "PrematureOk", "UnchargedRetry", "ChargeFirstStart",
    "TransientNormalRestart", "PermanentRetires", "RetryFirstStart",
    "EarlyFailedReply", "NoRefund", "ExpireReserved", "TermReplyEarly",
    "LateFailedOverwrite"}

ASSUME N \in Nat \ {0} /\ MaxChildren \in Nat
ASSUME MaxRestarts \in Nat /\ MaxExpire \in Nat
ASSUME ModeSet \subseteq Modes /\ ModeSet # {}
ASSUME SelfExits \subseteq Reasons /\ StartFails \subseteq Reasons
ASSUME CleanupFails \subseteq BOOLEAN /\ FALSE \in CleanupFails
ASSUME Fault \in Faults

\* Slot states. "idle": start_child not called. "queued": the call waits
\* for the control loop. "starting": the first generation is constructed.
\* "restarting": a replacement is constructed. "cleanup": the generation is
\* exiting or its owned cleanup has not finished. "decide": cleanup done,
\* the loop has not handled the exit yet. "backoff": a restart is pending,
\* holding a budget reservation. "stuck": cleanup missed its deadline; the
\* slot stays counted until it really ends. "removed"/"rejected": terminal.
States   == {"idle", "queued", "starting", "running", "cleanup", "decide",
             "backoff", "restarting", "stuck", "removed", "rejected"}
Active   == {"starting", "running", "cleanup", "decide", "backoff",
             "restarting", "stuck"}
Terminal == {"idle", "removed", "rejected"}
\* start_child: "stopped" and "max" are pre-admission rejections (the
\* arguments are returned); "aborted" is shutdown during construction;
\* "failed" is FailedToStart.
Results  == {"none", "pending", "ok", "failed", "aborted", "max", "stopped"}
\* terminate_child: "queued" waits for the loop, "pending" for removal.
TermResults == {"none", "queued", "pending", "ok", "stuck", "notfound", "stopped"}

Idle == [k |-> "idle", s |-> 0]

VARIABLES
    open,       \* admission open: neither shut down nor failed
    failed,     \* the supervisor failed
    exhausted,  \* a restart was refused for lack of budget
    sphase,     \* "running", "signal" (Sequential only), "waiting", "done"
    windowed,   \* stamped grants still in the window
    expired,    \* window releases used (bounds the model)
    reserved,   \* slots holding a reservation (pending restarts)
    stamped,    \* grants ever stamped (charged restart attempts)
    op,         \* the control loop's current operation [k, s]
    mode,       \* restart type of each slot, fixed after Init
    sl,         \* per-slot record, see SlotInit
    \* History, written only by GhostUpdate.
    lateCommit, latePub, lateGenClose, lateGenTerm, badRestart, badRetire,
    restarts

sup   == <<open, failed, exhausted, sphase, windowed, expired, reserved, stamped, op>>
ghost == <<lateCommit, latePub, lateGenClose, lateGenTerm, badRestart, badRetire, restarts>>
vars  == <<sup, sl, ghost, mode>>

SlotInit == [cs |-> "idle", gen |-> 0, alive |-> {}, pub |-> 0,
             reason |-> "none", cfail |-> FALSE, readyGen |-> FALSE,
             everReady |-> FALSE, result |-> "none", cause |-> "none",
             term |-> FALSE, termRes |-> "none", stopSig |-> FALSE,
             stuck |-> "none"]

TypeOK ==
    /\ open \in BOOLEAN /\ failed \in BOOLEAN /\ exhausted \in BOOLEAN
    /\ sphase \in {"running", "signal", "waiting", "done"}
    /\ windowed \in Nat /\ expired \in 0..MaxExpire
    /\ reserved \subseteq Slots /\ stamped \in Nat
    /\ op.k \in {"idle", "start", "restart", "term"} /\ op.s \in 0..N
    /\ \A s \in Slots :
        /\ sl[s].cs \in States /\ sl[s].gen \in Nat /\ sl[s].alive \subseteq Nat
        /\ sl[s].pub \in Nat /\ sl[s].reason \in Reasons
        /\ sl[s].result \in Results /\ sl[s].termRes \in TermResults
        /\ sl[s].cause \in {"none", "failed", "aborted"}
        /\ sl[s].stuck \in {"none", "retire", "restart", "closing"}

Init ==
    /\ open = TRUE /\ failed = FALSE /\ exhausted = FALSE /\ sphase = "running"
    /\ windowed = 0 /\ expired = 0 /\ reserved = {} /\ stamped = 0 /\ op = Idle
    /\ sl = [s \in Slots |-> SlotInit]
    /\ mode \in [Slots -> ModeSet]
    /\ lateCommit = [s \in Slots |-> FALSE] /\ latePub = [s \in Slots |-> FALSE]
    /\ lateGenClose = [s \in Slots |-> FALSE] /\ lateGenTerm = [s \in Slots |-> FALSE]
    /\ badRestart = [s \in Slots |-> FALSE] /\ badRetire = [s \in Slots |-> FALSE]
    /\ restarts = 0

\* Children held: counted against max_children.
Counted ==
    IF Fault = "UncountedRestart"
    THEN {s \in Slots : sl[s].cs \in {"starting", "running"}}
    ELSE {s \in Slots : sl[s].cs \in Active}

LoopFree(s) == op.k = "idle" \/ op.s = s \/ Fault = "ConcurrentStart"
Release(s)  == op' = IF op.s = s THEN Idle ELSE op
Take(k, s)  == op' = IF Fault = "ConcurrentStart" /\ k # "term" THEN op ELSE [k |-> k, s |-> s]
Budget      == windowed + Cardinality(reserved)

\* The design's restart table, stated independently of the actions.
DesignAllowsRestart(m, r, cf) ==
    CASE m = "Temporary" -> FALSE
      [] m = "Permanent" -> TRUE
      [] m = "Transient" -> r \in {"Panic", "Kill", "Killed"} \/ cf
\* A restart of slot s is allowed now: a ready-then-exited generation under
\* the table, or a failed replacement (never ready) of a restartable child.
Allowed(s) ==
    /\ sl[s].everReady
    /\ \/ DesignAllowsRestart(mode[s], sl[s].reason, sl[s].cfail)
       \/ ~sl[s].readyGen /\ mode[s] # "Temporary"

\* What the supervisor decides (with the policy faults).
WouldRestart(s, cf) ==
    /\ sl[s].everReady \/ Fault = "RetryFirstStart"
    /\ \/ mode[s] = "Permanent" /\ Fault # "PermanentRetires"
       \/ mode[s] = "Transient"
          /\ (sl[s].reason \in Abnormal \/ cf \/ Fault = "TransientNormalRestart")
       \/ mode[s] # "Temporary" /\ ~sl[s].readyGen
       \/ mode[s] = "Temporary" /\ Fault = "RestartTemporary"

CloseGate == open' = FALSE
Fail      == failed' = TRUE /\ CloseGate

-----------------------------------------------------------------------------
(* Callers and environment *)

\* start_child(args): queued for the loop, or rejected at once if closed.
Request(s) ==
    /\ sl[s].cs = "idle"
    /\ sl' = IF ~open
             THEN [sl EXCEPT ![s].cs = "rejected", ![s].result = "stopped"]
             ELSE [sl EXCEPT ![s].cs = "queued", ![s].result = "pending"]
    /\ UNCHANGED sup

\* The loop admits a queued start: cap check, then the first generation.
LoopAdmit(s) ==
    /\ sl[s].cs = "queued" /\ LoopFree(s)
    /\ open \/ Fault = "AcceptDuringShutdown"
    /\ IF Cardinality(Counted) >= MaxChildren /\ Fault # "IgnoreMax"
       THEN /\ sl' = [sl EXCEPT ![s].cs = "rejected", ![s].result = "max"]
            /\ UNCHANGED <<op, stamped, windowed>>
       ELSE /\ sl' = [sl EXCEPT ![s].cs = "starting", ![s].gen = 1, ![s].alive = {1},
                                ![s].result = IF Fault = "PrematureOk" THEN "ok" ELSE "pending"]
            /\ Take("start", s)
            /\ IF Fault = "ChargeFirstStart"
               THEN stamped' = stamped + 1 /\ windowed' = windowed + 1
               ELSE UNCHANGED <<stamped, windowed>>
    /\ UNCHANGED <<open, failed, exhausted, sphase, expired, reserved>>

\* Shutdown rejects queued starts before admission (arguments returned).
RejectQueued(s) ==
    /\ sl[s].cs = "queued" /\ ~open
    /\ sl' = [sl EXCEPT ![s].cs = "rejected", ![s].result = "stopped"]
    /\ UNCHANGED sup

\* Supervisor::stop / shutdown, system shutdown, or the parent stopping it.
Close ==
    /\ open /\ CloseGate
    /\ UNCHANGED <<failed, exhausted, sphase, windowed, expired, reserved, stamped, op, sl>>

\* The rolling window forgets its oldest stamped grant; reservations stay.
WindowExpire ==
    /\ expired < MaxExpire
    /\ IF windowed > 0
       THEN windowed' = windowed - 1 /\ UNCHANGED reserved
       ELSE /\ Fault = "ExpireReserved" /\ reserved # {}
            /\ \E r \in reserved : reserved' = reserved \ {r}
            /\ UNCHANGED windowed
    /\ expired' = expired + 1
    /\ UNCHANGED <<open, failed, exhausted, sphase, stamped, op, sl>>

\* terminate_child(id) by a caller holding the child's last published id.
TerminateRequest(s) ==
    /\ sl[s].termRes = "none" /\ sl[s].everReady
    /\ sl' = [sl EXCEPT ![s].termRes = IF open THEN "queued" ELSE "stopped"]
    /\ UNCHANGED sup

\* The loop handles it: mark terminated, signal, and hold the loop until
\* the child is removed (OTP terminate_child blocks the supervisor).
LoopTerm(s) ==
    /\ sl[s].termRes = "queued" /\ op.k = "idle" /\ open
    /\ IF sl[s].cs = "removed"
       THEN sl' = [sl EXCEPT ![s].termRes = "notfound"] /\ UNCHANGED op
       ELSE IF sl[s].cs = "stuck"
       THEN sl' = [sl EXCEPT ![s].termRes = "stuck"] /\ UNCHANGED op
       ELSE /\ sl' = [sl EXCEPT ![s].term = TRUE, ![s].stopSig = TRUE,
                                ![s].termRes = "pending"]
            /\ op' = [k |-> "term", s |-> s]
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved, stamped>>

\* A terminate queued when shutdown began resolves with the removal.
TermQueuedClosed(s) ==
    /\ sl[s].termRes = "queued" /\ ~open
    /\ sl' = [sl EXCEPT ![s].termRes = "pending"]
    /\ UNCHANGED sup

TermResolve(s) ==
    /\ sl[s].termRes = "pending"
    /\ sl[s].cs \in {"removed", "stuck"} \/ Fault = "TermReplyEarly"
    /\ sl' = [sl EXCEPT ![s].termRes = IF sl[s].cs = "stuck" THEN "stuck" ELSE "ok"]
    /\ IF op = [k |-> "term", s |-> s] THEN op' = Idle ELSE UNCHANGED op
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved, stamped>>

-----------------------------------------------------------------------------
(* One child slot *)

\* started() returned while admission is open: readiness commits; for a
\* first start the caller's Ok is committed (historical from now on).
Ready(s) ==
    /\ sl[s].cs \in {"starting", "restarting"}
    /\ \/ open
       \/ Fault = "AcceptDuringShutdown" /\ sl[s].cs = "starting"
       \/ Fault = "PublishAfterClose" /\ sl[s].cs = "restarting"
    /\ sl' = [sl EXCEPT ![s].cs = "running", ![s].pub = sl[s].gen,
                        ![s].readyGen = TRUE, ![s].everReady = TRUE,
                        ![s].result = IF sl[s].cs = "starting" THEN "ok" ELSE sl[s].result]
    /\ Release(s)
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, reserved, stamped>>

\* Factory error or panic, panic or stop in started(), startup timeout.
StartFail(s, r) ==
    /\ sl[s].cs \in {"starting", "restarting"} /\ r \in StartFails
    /\ sl' = [sl EXCEPT ![s].cs = "cleanup", ![s].reason = r, ![s].pub = 0,
                        ![s].cause = IF sl[s].cs = "starting" THEN "failed" ELSE sl[s].cause,
                        ![s].result = IF Fault = "EarlyFailedReply" /\ sl[s].cs = "starting"
                                      THEN "failed" ELSE sl[s].result]
    /\ UNCHANGED sup

\* Admission closed during construction: the attempt is cancelled.
AbortStart(s) ==
    /\ sl[s].cs \in {"starting", "restarting"} /\ ~open /\ Fault # "NoAbort"
    /\ sl' = [sl EXCEPT ![s].cs = "cleanup", ![s].reason = "Shutdown", ![s].pub = 0,
                        ![s].cause = IF sl[s].cs = "starting" THEN "aborted" ELSE sl[s].cause]
    /\ UNCHANGED sup

SelfExit(s, r) ==
    /\ sl[s].cs = "running" /\ r \in SelfExits
    /\ sl' = [sl EXCEPT ![s].cs = "cleanup", ![s].reason = r, ![s].pub = 0]
    /\ UNCHANGED sup

\* The parent exit signal (terminate_child or shutdown) reached the child:
\* it ends Shutdown, or Killed after missing shutdown_timeout (design E3).
StopObserved(s, r) ==
    /\ sl[s].cs = "running" /\ sl[s].stopSig /\ r \in {"Shutdown", "Killed"}
    /\ sl' = [sl EXCEPT ![s].cs = "cleanup", ![s].reason = r, ![s].pub = 0]
    /\ UNCHANGED sup

\* Publication and owned cleanup finished; a destructor or helper failure
\* may be recorded (a Transient child then restarts).
CleanupDone(s, f) ==
    /\ sl[s].cs = "cleanup" /\ f \in CleanupFails
    /\ sl' = [sl EXCEPT ![s].cs = "decide", ![s].cfail = f,
                        ![s].alive = sl[s].alive \ {sl[s].gen}]
    /\ UNCHANGED sup

\* The child missed its deadlines: still running after its stop signal and
\* the kill, or its cleanup is stuck. The slot stays counted and alive.
\* It fails the supervisor only when the child would have been restarted
\* (V3: no replacement while the old generation lives) or while closing;
\* a child that was retiring anyway fails only itself.
CleanupStuck(s) ==
    /\ sl[s].stuck = "none"
    /\ \/ sl[s].cs = "cleanup"
       \/ sl[s].cs = "running" /\ sl[s].stopSig
    /\ LET kind == IF ~open THEN "closing"
                   ELSE IF ~sl[s].term /\ sl[s].cs = "cleanup" /\ WouldRestart(s, TRUE)
                        THEN "restart" ELSE "retire"
       IN /\ sl' = [sl EXCEPT ![s].cs = "stuck", ![s].stuck = kind, ![s].pub = 0,
                              ![s].result = IF sl[s].result = "pending"
                                            THEN sl[s].cause ELSE sl[s].result]
          /\ IF kind # "retire" \/ Fault = "StuckFailsAll"
             THEN Fail ELSE UNCHANGED <<open, failed>>
    /\ IF op.s = s /\ op.k # "term" THEN op' = Idle ELSE UNCHANGED op
    /\ UNCHANGED <<exhausted, sphase, windowed, expired, reserved, stamped>>

\* A stuck child finally ends (assumed for liveness only).
StuckDone(s) ==
    /\ sl[s].cs = "stuck"
    /\ sl' = [sl EXCEPT ![s].cs = "removed", ![s].alive = {}]
    /\ UNCHANGED sup

\* The loop handles the exit: first-start failure, removal, or a restart
\* that reserves budget first (failing at once if none is left).
Decide(s) ==
    /\ \/ sl[s].cs = "decide"
       \/ Fault = "OverlapGeneration" /\ sl[s].cs = "cleanup"
    /\ LoopFree(s)
    /\ IF ~sl[s].everReady /\ Fault # "RetryFirstStart"
       THEN /\ sl' = [sl EXCEPT ![s].cs = "removed",
                       ![s].result = IF Fault = "LateFailedOverwrite" /\ ~open
                                     THEN "aborted" ELSE sl[s].cause]
            /\ UNCHANGED <<open, failed, exhausted, reserved>>
       ELSE IF ~open \/ (sl[s].term /\ Fault # "RestartTerminated")
       THEN /\ sl' = [sl EXCEPT ![s].cs = "removed"]
            /\ UNCHANGED <<open, failed, exhausted, reserved>>
       ELSE IF WouldRestart(s, sl[s].cfail)
       THEN IF Budget < MaxRestarts
            THEN /\ sl' = [sl EXCEPT ![s].cs = "backoff"]
                 /\ reserved' = reserved \cup {s}
                 /\ UNCHANGED <<open, failed, exhausted>>
            ELSE /\ exhausted' = TRUE
                 /\ sl' = [sl EXCEPT ![s].cs = "removed"]
                 /\ IF Fault = "NoEscalate" THEN UNCHANGED <<open, failed>> ELSE Fail
                 /\ UNCHANGED reserved
       ELSE /\ sl' = [sl EXCEPT ![s].cs = "removed"]
            /\ UNCHANGED <<open, failed, exhausted, reserved>>
    /\ Release(s)
    /\ UNCHANGED <<sphase, windowed, expired, stamped>>

\* Backoff elapsed: stamp the reservation and construct the replacement.
BackoffDone(s) ==
    /\ sl[s].cs = "backoff" /\ LoopFree(s)
    /\ open \/ Fault = "RestartAfterClose"
    /\ ~sl[s].term \/ Fault = "RestartTerminated"
    /\ LET g == sl[s].gen + 1
           charge == ~(Fault = "UnchargedRetry" /\ ~sl[s].readyGen)
       IN /\ sl' = [sl EXCEPT ![s].cs = "restarting", ![s].gen = g,
                              ![s].alive = sl[s].alive \cup {g},
                              ![s].pub = IF Fault = "PublishBeforeReady" THEN g ELSE 0,
                              ![s].reason = "none", ![s].cfail = FALSE,
                              ![s].readyGen = FALSE]
          /\ reserved' = reserved \ {s}
          /\ IF charge THEN stamped' = stamped + 1 /\ windowed' = windowed + 1
             ELSE UNCHANGED <<stamped, windowed>>
    /\ Take("restart", s)
    /\ UNCHANGED <<open, failed, exhausted, sphase, expired>>

\* Shutdown or terminate_child cancels a pending restart and refunds it.
CancelBackoff(s) ==
    /\ sl[s].cs = "backoff"
    /\ \/ ~open /\ Fault # "StuckBackoff"
       \/ sl[s].term /\ Fault # "RestartTerminated"
    /\ sl' = [sl EXCEPT ![s].cs = "removed"]
    /\ reserved' = IF Fault = "NoRefund" THEN reserved ELSE reserved \ {s}
    /\ UNCHANGED <<open, failed, exhausted, sphase, windowed, expired, stamped, op>>

-----------------------------------------------------------------------------
(* Supervisor shutdown: every child is signalled without awaiting another *)

SupSignal ==
    /\ sphase = "running" /\ ~open /\ Fault # "Sequential"
    /\ sl' = [s \in Slots |-> [sl[s] EXCEPT !.stopSig = TRUE]]
    /\ sphase' = "waiting"
    /\ UNCHANGED <<open, failed, exhausted, windowed, expired, reserved, stamped, op>>

\* Control: one child at a time, the next only after the previous ended.
SupSignalOne ==
    /\ sphase \in {"running", "signal"} /\ ~open /\ Fault = "Sequential"
    /\ \A s \in Slots : sl[s].stopSig => sl[s].cs \in Terminal \cup {"stuck"}
    /\ LET todo == {s \in Slots : ~sl[s].stopSig /\ sl[s].cs \notin Terminal}
       IN IF todo = {}
          THEN sphase' = "waiting" /\ UNCHANGED sl
          ELSE \E s \in todo : sl' = [sl EXCEPT ![s].stopSig = TRUE] /\ sphase' = "signal"
    /\ UNCHANGED <<open, failed, exhausted, windowed, expired, reserved, stamped, op>>

SupDone ==
    /\ sphase = "waiting"
    /\ (\A s \in Slots : sl[s].cs \in Terminal) \/ Fault = "EarlyDone"
    /\ sphase' = "done"
    /\ UNCHANGED <<open, failed, exhausted, windowed, expired, reserved, stamped, op, sl>>

-----------------------------------------------------------------------------

Grew(s) == sl'[s].gen > sl[s].gen

\* History, computed from each step's effect, not from the action taken.
GhostUpdate ==
    /\ lateCommit' = [s \in Slots |-> lateCommit[s] \/
                        (sl'[s].result = "ok" /\ sl[s].result # "ok" /\ ~open)]
    /\ latePub' = [s \in Slots |-> latePub[s] \/
                        (sl'[s].pub # 0 /\ sl[s].pub = 0 /\ (~open \/ sl[s].term))]
    /\ lateGenClose' = [s \in Slots |-> lateGenClose[s] \/ (Grew(s) /\ ~open)]
    /\ lateGenTerm' = [s \in Slots |-> lateGenTerm[s] \/ (Grew(s) /\ sl[s].term)]
    /\ badRestart' = [s \in Slots |-> badRestart[s] \/
                        (Grew(s) /\ sl[s].gen > 0 /\ ~Allowed(s))]
    /\ badRetire' = [s \in Slots |-> badRetire[s] \/
                        (sl[s].cs = "decide" /\ sl'[s].cs = "removed" /\ open'
                         /\ ~sl[s].term /\ Allowed(s))]
    /\ restarts' = restarts + Cardinality({s \in Slots : Grew(s) /\ sl[s].gen > 0})
    /\ UNCHANGED mode

SlotStep(s) ==
    \/ Request(s) \/ LoopAdmit(s) \/ RejectQueued(s)
    \/ TerminateRequest(s) \/ LoopTerm(s) \/ TermQueuedClosed(s) \/ TermResolve(s)
    \/ Ready(s) \/ \E r \in StartFails : StartFail(s, r) \/ AbortStart(s)
    \/ \E r \in SelfExits : SelfExit(s, r)
    \/ \E r \in {"Shutdown", "Killed"} : StopObserved(s, r)
    \/ \E f \in CleanupFails : CleanupDone(s, f)
    \/ CleanupStuck(s) \/ StuckDone(s)
    \/ Decide(s) \/ BackoffDone(s) \/ CancelBackoff(s)

Act ==
    \/ Close \/ WindowExpire \/ SupSignal \/ SupSignalOne \/ SupDone
    \/ \E s \in Slots : SlotStep(s)

Next == Act /\ GhostUpdate

\* Progress: the loop, construction, stops, cleanup and stuck children
\* eventually move (crashes, start failures and getting stuck are not fair).
Fairness ==
    /\ \A s \in Slots :
         /\ WF_vars(LoopAdmit(s) /\ GhostUpdate) /\ WF_vars(RejectQueued(s) /\ GhostUpdate)
         /\ WF_vars(LoopTerm(s) /\ GhostUpdate) /\ WF_vars(TermQueuedClosed(s) /\ GhostUpdate)
         /\ WF_vars(TermResolve(s) /\ GhostUpdate)
         /\ WF_vars(Ready(s) /\ GhostUpdate) /\ WF_vars(AbortStart(s) /\ GhostUpdate)
         /\ WF_vars((\E r \in {"Shutdown", "Killed"} : StopObserved(s, r)) /\ GhostUpdate)
         /\ WF_vars((\E f \in CleanupFails : CleanupDone(s, f)) /\ GhostUpdate)
         /\ WF_vars(StuckDone(s) /\ GhostUpdate)
         /\ WF_vars(Decide(s) /\ GhostUpdate) /\ WF_vars(BackoffDone(s) /\ GhostUpdate)
         /\ WF_vars(CancelBackoff(s) /\ GhostUpdate)
    /\ WF_vars(SupSignal /\ GhostUpdate) /\ WF_vars(SupSignalOne /\ GhostUpdate)
    /\ WF_vars(SupDone /\ GhostUpdate)

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* Safety *)

\* A slot never has two live generations (V3); alive accumulates and only
\* the generation whose cleanup finished is removed.
AtMostOneGeneration == \A s \in Slots : Cardinality(sl[s].alive) <= 1

\* current() returns only the live, latest, ready generation (V4).
PublishedIsLive ==
    \A s \in Slots : sl[s].pub # 0 =>
        sl[s].pub = sl[s].gen /\ sl[s].pub \in sl[s].alive /\ sl[s].readyGen

NoChildOutlivesSupervisor ==
    sphase = "done" => \A s \in Slots : sl[s].alive = {} /\ sl[s].cs \in Terminal

\* Children held, restarting and stuck ones included, never exceed the cap.
MaxChildrenBound == Cardinality({s \in Slots : sl[s].cs \in Active}) <= MaxChildren

\* The control loop constructs one generation at a time (OTP).
SerialConstruction ==
    Cardinality({s \in Slots : sl[s].cs \in {"starting", "restarting"}}) <= 1

TemporaryNeverRestarted == \A s \in Slots : mode[s] = "Temporary" => sl[s].gen <= 1

\* No generation begins after terminate_child, or after admission closed.
TerminatedNeverRestarted == \A s \in Slots : ~lateGenTerm[s]
NoGenerationAfterClose == \A s \in Slots : ~lateGenClose[s]

\* No generation is published after terminate_child or close.
NoPublishAfterStop == \A s \in Slots : ~latePub[s]

\* A start_child success is committed only while admission is open.
NoStartAfterShutdown == \A s \in Slots : ~lateCommit[s]

\* Restarts follow the table; a restartable exit is never retired while open.
RestartFollowsPolicy == \A s \in Slots : ~badRestart[s] /\ ~badRetire[s]

\* A failed first start is never retried.
FirstStartNeverRetried == \A s \in Slots : sl[s].gen > 1 => sl[s].everReady

\* FailedToStart / aborted is returned only after cleanup, or at the stuck
\* deadline (the slot then stays counted).
FailedAfterCleanup ==
    \A s \in Slots : sl[s].result \in {"failed", "aborted"} =>
        sl[s].cs \in {"removed", "stuck"}

\* The error names what happened to the attempt, not the gate afterwards.
FailureCauseKept ==
    \A s \in Slots : sl[s].result \in {"failed", "aborted"} => sl[s].result = sl[s].cause

\* Every restart attempt is charged exactly once; first starts never.
ChargedRestarts == restarts = stamped

\* Reservations are exactly the pending restarts.
ReservationsMatch == reserved = {s \in Slots : sl[s].cs = "backoff"}

BudgetBounded == Budget <= MaxRestarts

ExhaustionCloses == exhausted => failed /\ ~open

\* A child stuck while retiring fails only itself.
StuckIsolated ==
    failed => exhausted \/ \E s \in Slots : sl[s].stuck \in {"restart", "closing"}

ConcurrentShutdown ==
    sphase # "running" => \A s \in Slots : sl[s].cs = "running" => sl[s].stopSig

OkMeansReady == \A s \in Slots : sl[s].result = "ok" => sl[s].everReady

\* terminate_child returns Ok only once the child was removed.
TermOkMeansRemoved ==
    \A s \in Slots : sl[s].termRes = "ok" => sl[s].cs = "removed" /\ sl[s].alive = {}

Safety ==
    /\ TypeOK /\ AtMostOneGeneration /\ PublishedIsLive /\ NoChildOutlivesSupervisor
    /\ MaxChildrenBound /\ SerialConstruction /\ TemporaryNeverRestarted
    /\ TerminatedNeverRestarted /\ NoGenerationAfterClose /\ NoPublishAfterStop
    /\ NoStartAfterShutdown /\ RestartFollowsPolicy /\ FirstStartNeverRetried
    /\ FailedAfterCleanup /\ FailureCauseKept /\ ChargedRestarts /\ ReservationsMatch
    /\ BudgetBounded /\ ExhaustionCloses /\ StuckIsolated /\ ConcurrentShutdown
    /\ OkMeansReady /\ TermOkMeansRemoved

NeverReopens == [][open' => open]_vars

(* Liveness (assumes stuck children eventually end) *)
ClosedLeadsToDone == (~open) ~> (sphase = "done")
StartAnswered == \A s \in Slots :
    (sl[s].result = "pending") ~> (sl[s].result # "pending")
TerminateAnswered == \A s \in Slots :
    (sl[s].termRes \in {"queued", "pending"}) ~> (sl[s].termRes \notin {"queued", "pending"})
\* A restartable exit is restarted, or superseded by terminate, shutdown,
\* exhaustion or a stuck cleanup.
RecoveryProgress == \A s \in Slots :
    (sl[s].cs \in {"decide", "backoff", "restarting"} /\ sl[s].everReady)
        ~> (sl[s].cs \in {"running", "removed", "stuck"})
=============================================================================
