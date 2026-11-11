------------------------- MODULE DynamicSupervisor -------------------------
(***************************************************************************)
(* Rank 12a design model (not implemented): a dynamic supervisor, OTP's   *)
(* simple_one_for_one / Elixir's DynamicSupervisor. Design contract:      *)
(* docs/design/dynamic-supervisor.md.                                     *)
(*                                                                         *)
(* Slots 1..N are potential start_child calls; each is made at most once, *)
(* at any time, so starts race crashes, restarts, terminate_child,        *)
(* max_children and shutdown. A slot that is admitted owns one child:     *)
(* its first generation, then restarts with fresh generations (fresh      *)
(* ActorIds) until the child is removed. Restart types are per slot so    *)
(* one instance covers all three (a simple_one_for_one template has one   *)
(* type; mixing them is the heterogeneous case and loses nothing).         *)
(*                                                                         *)
(* Abstractions, as in Supervision.tla:                                   *)
(*  - The admission gate and the supervisor's cancel are one boolean      *)
(*    `open`; shutdown (Close) and failure (Fail) both close it.          *)
(*  - Time is abstract. The rolling restart window is a counter released  *)
(*    by WindowExpire, at most MaxExpire times, which also bounds the     *)
(*    number of generations (no state constraint is needed).              *)
(*  - The shutdown_timeout -> kill escalation is the nondeterministic     *)
(*    reason of StopObserved (Shutdown, or Killed after the kill). A      *)
(*    cleanup that misses its second interval is CleanupStuck: the        *)
(*    supervisor fails, the generation stays alive until it really ends.  *)
(*  - Backoff is the "backoff" state; its sleep is not modelled.          *)
(*  - The budget is charged when a restart attempt is admitted; the       *)
(*    implementation's reserve-then-stamp protocol is checked in          *)
(*    Supervision.tla and shared code.                                    *)
(*                                                                         *)
(* Fault selects a negative control; negative_checks.py requires each to  *)
(* violate its named invariant:                                           *)
(*   IgnoreMax            (MaxChildrenBound)    admission ignores the cap *)
(*   UncountedRestart     (MaxChildrenBound)    a crashed child frees its *)
(*                                              slot until it restarts    *)
(*   RestartTemporary     (TemporaryNeverRestarted)                       *)
(*   RestartTerminated    (TerminatedNeverRestarted) terminate_child      *)
(*                                              does not cancel restart   *)
(*   AcceptDuringShutdown (NoStartAfterShutdown) start_child admitted and *)
(*                                              committed after close     *)
(*   RestartAfterClose    (NoGenerationAfterClose) a pending restart      *)
(*                                              proceeds after close      *)
(*   NoEscalate           (ExhaustionCloses)    budget exhaustion drops   *)
(*                                              only that child           *)
(*   EarlyDone            (NoChildOutlivesSupervisor) done before every   *)
(*                                              child finished            *)
(*   Sequential           (ConcurrentShutdown)  children stopped one at a *)
(*                                              time, as a static         *)
(*                                              supervisor does           *)
(* and two liveness controls (named_temporal):                            *)
(*   NoAbort              (StartAnswered)       a start racing shutdown   *)
(*                                              is not cancelled          *)
(*   StuckBackoff         (ClosedLeadsToDone)   a pending restart is not  *)
(*                                              cancelled by shutdown     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
    N,            \* number of start_child calls (slots 1..N)
    Mode,         \* Mode[s] \in {"Permanent", "Transient", "Temporary"}
    MaxChildren,  \* max_children
    MaxRestarts,  \* restart intensity (max_restarts per window)
    MaxExpire,    \* how often the rolling window may release an entry
    SelfExits,    \* reasons a ready child may exit with on its own
    StartFails,   \* outcomes of a failed construction or readiness
    Fault

Slots  == 1..N
Modes  == {"Permanent", "Transient", "Temporary"}
\* "none": factory error or panic (no actor reason); "timeout": startup
\* deadline; otherwise the actor's exit reason (v0.8 reasons, design E5).
Reasons  == {"none", "timeout", "Normal", "Shutdown", "Panic", "Kill", "Killed"}
Abnormal == {"none", "timeout", "Panic", "Kill", "Killed"}

ASSUME N \in Nat \ {0} /\ MaxChildren \in Nat \ {0}
ASSUME MaxRestarts \in Nat /\ MaxExpire \in Nat
ASSUME \A s \in Slots : Mode[s] \in Modes
ASSUME SelfExits \subseteq Reasons /\ StartFails \subseteq Reasons
ASSUME Fault \in {"None", "IgnoreMax", "UncountedRestart", "RestartTemporary",
                  "RestartTerminated", "AcceptDuringShutdown", "RestartAfterClose",
                  "NoEscalate", "EarlyDone", "Sequential", "NoAbort",
                  "StuckBackoff"}

\* Slot states. "idle": start_child not called yet. "starting": the first
\* generation is being constructed (the caller waits). "restarting": a
\* replacement is being constructed. "cleanup": the generation is exiting
\* and its owned cleanup has not finished. "decide": cleanup finished,
\* restart decision pending. "backoff": a restart is pending.
\* "removed" and "rejected" are terminal.
States   == {"idle", "starting", "running", "cleanup", "decide", "backoff",
             "restarting", "removed", "rejected"}
Active   == {"starting", "running", "cleanup", "decide", "backoff", "restarting"}
Terminal == {"idle", "removed", "rejected"}
\* What start_child returned. "pending" while the caller waits.
Results  == {"none", "pending", "ok", "failed", "max", "shutdown"}

VARIABLES
    open,       \* admission open: neither shut down nor failed
    failed,     \* the supervisor failed (budget exhausted or cleanup stuck)
    sphase,     \* "running", "signal" (Sequential only), "waiting", "done"
    budget,     \* restart grants in the current window
    expired,    \* window releases used (bounds the model)
    cs,         \* slot state
    gen,        \* generations started in this slot
    alive,      \* live (not yet cleaned up) generations
    pub,        \* generation current() returns, 0 if none
    reason,     \* exit reason of the latest generation
    readyGen,   \* the latest generation became ready
    everReady,  \* the slot's first start succeeded
    result,     \* start_child's outcome
    term,       \* terminate_child was called for this child
    stopSig,    \* the supervisor's stop signal reached this slot
    \* Ghosts for the invariants.
    termGen,    \* gen when terminate_child was called
    closeGen,   \* gen of each slot when admission closed (meaningful once closed)
    lateOk,     \* a start_child returned ok after admission closed
    exhausted   \* a restart was refused for lack of budget

sup   == <<open, failed, sphase, budget, expired>>
slot  == <<cs, gen, alive, pub, reason, readyGen, everReady, result, term, stopSig>>
ghost == <<termGen, closeGen, lateOk, exhausted>>
vars  == <<sup, slot, ghost>>

TypeOK ==
    /\ open \in BOOLEAN /\ failed \in BOOLEAN
    /\ sphase \in {"running", "signal", "waiting", "done"}
    /\ budget \in 0..MaxRestarts /\ expired \in 0..MaxExpire
    /\ cs \in [Slots -> States]
    /\ gen \in [Slots -> Nat]
    /\ alive \in [Slots -> SUBSET Nat]
    /\ pub \in [Slots -> Nat]
    /\ reason \in [Slots -> Reasons]
    /\ readyGen \in [Slots -> BOOLEAN] /\ everReady \in [Slots -> BOOLEAN]
    /\ result \in [Slots -> Results]
    /\ term \in [Slots -> BOOLEAN] /\ stopSig \in [Slots -> BOOLEAN]
    /\ termGen \in [Slots -> Nat]
    /\ closeGen \in [Slots -> Nat]
    /\ lateOk \in [Slots -> BOOLEAN]
    /\ exhausted \in BOOLEAN

Init ==
    /\ open = TRUE /\ failed = FALSE /\ sphase = "running"
    /\ budget = 0 /\ expired = 0
    /\ cs = [s \in Slots |-> "idle"]
    /\ gen = [s \in Slots |-> 0]
    /\ alive = [s \in Slots |-> {}]
    /\ pub = [s \in Slots |-> 0]
    /\ reason = [s \in Slots |-> "none"]
    /\ readyGen = [s \in Slots |-> FALSE]
    /\ everReady = [s \in Slots |-> FALSE]
    /\ result = [s \in Slots |-> "none"]
    /\ term = [s \in Slots |-> FALSE]
    /\ stopSig = [s \in Slots |-> FALSE]
    /\ termGen = [s \in Slots |-> 0]
    /\ closeGen = [s \in Slots |-> 0]
    /\ lateOk = [s \in Slots |-> FALSE]
    /\ exhausted = FALSE

\* Slots that count against max_children: every admitted child that was not
\* removed, including one that is restarting (in backoff or constructing its
\* replacement) or still cleaning up. The control counts only live,
\* published children.
Counted ==
    IF Fault = "UncountedRestart"
    THEN {s \in Slots : cs[s] \in {"starting", "running"}}
    ELSE {s \in Slots : cs[s] \in Active}

\* Close admission (shutdown or failure) and record each slot's generation.
CloseGate ==
    /\ open' = FALSE
    /\ closeGen' = IF open THEN gen ELSE closeGen

Fail == failed' = TRUE /\ CloseGate

Set(f, s, v) == [f EXCEPT ![s] = v]

-----------------------------------------------------------------------------
(* Callers and environment *)

\* start_child(args). Admission is atomic with the count and the gate.
Request(s) ==
    /\ cs[s] = "idle"
    /\ IF ~open /\ Fault # "AcceptDuringShutdown"
       THEN /\ cs' = Set(cs, s, "rejected") /\ result' = Set(result, s, "shutdown")
            /\ UNCHANGED <<gen, alive, reason, readyGen>>
       ELSE IF Cardinality(Counted) >= MaxChildren /\ Fault # "IgnoreMax"
       THEN /\ cs' = Set(cs, s, "rejected") /\ result' = Set(result, s, "max")
            /\ UNCHANGED <<gen, alive, reason, readyGen>>
       ELSE /\ cs' = Set(cs, s, "starting") /\ result' = Set(result, s, "pending")
            /\ gen' = Set(gen, s, 1) /\ alive' = Set(alive, s, {1})
            /\ reason' = Set(reason, s, "none") /\ readyGen' = Set(readyGen, s, FALSE)
    /\ UNCHANGED <<sup, pub, everReady, term, stopSig, ghost>>

\* Supervisor::stop / shutdown, system shutdown, or the parent stopping it.
Close ==
    /\ open
    /\ CloseGate
    /\ UNCHANGED <<failed, sphase, budget, expired, slot, termGen, lateOk, exhausted>>

\* terminate_child(id) on a child whose start returned. It never restarts
\* afterwards, whatever its restart type. After close it is a no-op (the
\* supervisor already stops every child), so it is only modelled while open.
TerminateChild(s) ==
    /\ open /\ everReady[s] /\ ~term[s]
    /\ cs[s] \in {"running", "cleanup", "decide", "backoff", "restarting"}
    /\ term' = Set(term, s, TRUE)
    /\ stopSig' = Set(stopSig, s, TRUE)
    /\ termGen' = Set(termGen, s, gen[s])
    /\ UNCHANGED <<sup, cs, gen, alive, pub, reason, readyGen, everReady, result,
                   closeGen, lateOk, exhausted>>

\* The rolling window forgets its oldest grant.
WindowExpire ==
    /\ budget > 0 /\ expired < MaxExpire
    /\ budget' = budget - 1 /\ expired' = expired + 1
    /\ UNCHANGED <<open, failed, sphase, slot, ghost>>

-----------------------------------------------------------------------------
(* One child slot *)

\* started() returned while admission is open: readiness commits, and for
\* a first start start_child returns Ok(ref).
Ready(s) ==
    /\ cs[s] \in {"starting", "restarting"}
    /\ open \/ Fault = "AcceptDuringShutdown"
    /\ ~(term[s] /\ cs[s] = "restarting")
    /\ cs' = Set(cs, s, "running")
    /\ pub' = Set(pub, s, gen[s])
    /\ readyGen' = Set(readyGen, s, TRUE)
    /\ everReady' = Set(everReady, s, TRUE)
    /\ result' = IF cs[s] = "starting" THEN Set(result, s, "ok") ELSE result
    /\ lateOk' = IF cs[s] = "starting" /\ ~open THEN Set(lateOk, s, TRUE) ELSE lateOk
    /\ UNCHANGED <<sup, gen, alive, reason, term, stopSig, termGen, closeGen, exhausted>>

\* Factory error or panic, panic or stop in started(), startup timeout.
StartFail(s, r) ==
    /\ cs[s] \in {"starting", "restarting"}
    /\ r \in StartFails
    /\ cs' = Set(cs, s, "cleanup") /\ reason' = Set(reason, s, r)
    /\ UNCHANGED <<sup, gen, alive, pub, readyGen, everReady, result, term, stopSig, ghost>>

\* Admission closed (or the child was terminated) during construction: the
\* attempt is cancelled and cleaned up; it never publishes.
AbortStart(s) ==
    /\ cs[s] \in {"starting", "restarting"}
    /\ (~open /\ Fault # "NoAbort") \/ term[s]
    /\ cs' = Set(cs, s, "cleanup") /\ reason' = Set(reason, s, "Shutdown")
    /\ UNCHANGED <<sup, gen, alive, pub, readyGen, everReady, result, term, stopSig, ghost>>

\* A ready child exits on its own.
SelfExit(s, r) ==
    /\ cs[s] = "running" /\ r \in SelfExits
    /\ cs' = Set(cs, s, "cleanup") /\ reason' = Set(reason, s, r)
    /\ pub' = Set(pub, s, 0)
    /\ UNCHANGED <<sup, gen, alive, readyGen, everReady, result, term, stopSig, ghost>>

\* The supervisor's parent exit signal (Shutdown) reached the child, from
\* terminate_child or supervisor shutdown. It ends Shutdown, or Killed if
\* it missed shutdown_timeout and was killed (design E3).
StopObserved(s, r) ==
    /\ cs[s] = "running" /\ stopSig[s]
    /\ r \in {"Shutdown", "Killed"}
    /\ cs' = Set(cs, s, "cleanup") /\ reason' = Set(reason, s, r)
    /\ pub' = Set(pub, s, 0)
    /\ UNCHANGED <<sup, gen, alive, readyGen, everReady, result, term, stopSig, ghost>>

\* The generation reached publication and its owned cleanup joined.
CleanupDone(s) ==
    /\ cs[s] = "cleanup"
    /\ alive' = Set(alive, s, {})
    /\ cs' = Set(cs, s, "decide")
    /\ UNCHANGED <<sup, gen, pub, reason, readyGen, everReady, result, term, stopSig, ghost>>

\* Cleanup missed its deadline: the supervisor fails (and so stops every
\* other child); the generation stays alive until CleanupDone.
CleanupStuck(s) ==
    /\ cs[s] = "cleanup" /\ ~failed
    /\ Fail
    /\ UNCHANGED <<sphase, budget, expired, slot, termGen, lateOk, exhausted>>

\* OTP's restart table, plus: a replacement that fails before readiness is a
\* failed restart attempt and is retried unless Temporary (V1).
Restarts(s) ==
    \/ Mode[s] = "Permanent"
    \/ Mode[s] = "Transient" /\ reason[s] \in Abnormal
    \/ Mode[s] # "Temporary" /\ ~readyGen[s]
    \/ Mode[s] = "Temporary" /\ Fault = "RestartTemporary"

Decide(s) ==
    /\ cs[s] = "decide"
    /\ IF ~everReady[s]
       \* First start failed: start_child returns the error; never restarted
       \* and never charged to the budget (OTP start_child).
       THEN /\ cs' = Set(cs, s, "removed")
            /\ result' = Set(result, s, IF open THEN "failed" ELSE "shutdown")
       ELSE IF ~open \/ (term[s] /\ Fault # "RestartTerminated")
       THEN cs' = Set(cs, s, "removed") /\ UNCHANGED result
       ELSE IF Restarts(s)
       THEN cs' = Set(cs, s, "backoff") /\ UNCHANGED result
       ELSE cs' = Set(cs, s, "removed") /\ UNCHANGED result
    /\ UNCHANGED <<sup, gen, alive, pub, reason, readyGen, everReady, term, stopSig, ghost>>

\* Backoff elapsed (or was cancelled). A restart needs a budget grant;
\* without one the supervisor fails and escalates.
BackoffDone(s) ==
    /\ cs[s] = "backoff"
    /\ open \/ term[s] \/ Fault # "StuckBackoff"
    /\ IF (~open /\ Fault # "RestartAfterClose") \/ (term[s] /\ Fault # "RestartTerminated")
       THEN /\ cs' = Set(cs, s, "removed")
            /\ UNCHANGED <<open, failed, budget, gen, alive, reason, readyGen, closeGen, exhausted>>
       ELSE IF budget < MaxRestarts
       THEN /\ budget' = budget + 1
            /\ cs' = Set(cs, s, "restarting")
            /\ gen' = Set(gen, s, gen[s] + 1)
            /\ alive' = Set(alive, s, {gen[s] + 1})
            /\ reason' = Set(reason, s, "none")
            /\ readyGen' = Set(readyGen, s, FALSE)
            /\ UNCHANGED <<open, failed, closeGen, exhausted>>
       ELSE /\ exhausted' = TRUE
            /\ cs' = Set(cs, s, "removed")
            /\ IF Fault = "NoEscalate" THEN UNCHANGED <<open, failed, closeGen>> ELSE Fail
            /\ UNCHANGED <<budget, gen, alive, reason, readyGen>>
    /\ UNCHANGED <<sphase, expired, pub, everReady, result, term, stopSig, termGen, lateOk>>

-----------------------------------------------------------------------------
(* Supervisor shutdown: every child is signalled at once *)

SupSignal ==
    /\ sphase = "running" /\ ~open /\ Fault # "Sequential"
    /\ stopSig' = [s \in Slots |-> TRUE]
    /\ sphase' = "waiting"
    /\ UNCHANGED <<open, failed, budget, expired, cs, gen, alive, pub, reason,
                   readyGen, everReady, result, term, ghost>>

\* Control: one child at a time, the next only after the previous finished.
SupSignalOne ==
    /\ sphase \in {"running", "signal"} /\ ~open /\ Fault = "Sequential"
    /\ \A s \in Slots : stopSig[s] => cs[s] \in Terminal
    /\ LET todo == {s \in Slots : ~stopSig[s] /\ cs[s] \notin Terminal}
       IN IF todo = {}
          THEN sphase' = "waiting" /\ UNCHANGED stopSig
          ELSE \E s \in todo : stopSig' = Set(stopSig, s, TRUE) /\ sphase' = "signal"
    /\ UNCHANGED <<open, failed, budget, expired, cs, gen, alive, pub, reason,
                   readyGen, everReady, result, term, ghost>>

\* Done once every child finished (removed, rejected or never requested).
SupDone ==
    /\ sphase = "waiting"
    /\ (\A s \in Slots : cs[s] \in Terminal) \/ Fault = "EarlyDone"
    /\ sphase' = "done"
    /\ UNCHANGED <<open, failed, budget, expired, slot, ghost>>

-----------------------------------------------------------------------------

SlotStep(s) ==
    \/ Request(s) \/ TerminateChild(s) \/ Ready(s)
    \/ \E r \in StartFails : StartFail(s, r)
    \/ AbortStart(s)
    \/ \E r \in SelfExits : SelfExit(s, r)
    \/ \E r \in {"Shutdown", "Killed"} : StopObserved(s, r)
    \/ CleanupDone(s) \/ CleanupStuck(s) \/ Decide(s) \/ BackoffDone(s)

Next ==
    \/ Close \/ WindowExpire
    \/ SupSignal \/ SupSignalOne \/ SupDone
    \/ \E s \in Slots : SlotStep(s)

\* Progress: construction resolves, signalled children stop, cleanup ends
\* (CleanupStuck and crashes are not fair), the supervisor moves on.
Fairness ==
    /\ \A s \in Slots :
         /\ WF_vars(Ready(s)) /\ WF_vars(AbortStart(s))
         /\ WF_vars(\E r \in {"Shutdown", "Killed"} : StopObserved(s, r))
         /\ WF_vars(CleanupDone(s)) /\ WF_vars(Decide(s)) /\ WF_vars(BackoffDone(s))
    /\ WF_vars(SupSignal) /\ WF_vars(SupSignalOne) /\ WF_vars(SupDone)

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* Safety *)

\* A slot never has two live generations (V3).
AtMostOneGeneration == \A s \in Slots : Cardinality(alive[s]) <= 1

\* current() returns only the live, latest generation (V4).
PublishedIsLive ==
    \A s \in Slots : pub[s] # 0 => pub[s] = gen[s] /\ pub[s] \in alive[s]

\* No child outlives the supervisor: done means every child finished.
NoChildOutlivesSupervisor ==
    sphase = "done" => \A s \in Slots : alive[s] = {} /\ cs[s] \in Terminal

\* The live count, restarting children included, never exceeds the cap.
MaxChildrenBound ==
    Cardinality({s \in Slots : cs[s] \in Active}) <= MaxChildren

TemporaryNeverRestarted ==
    \A s \in Slots : Mode[s] = "Temporary" => gen[s] <= 1

TerminatedNeverRestarted ==
    \A s \in Slots : term[s] => gen[s] = termGen[s]

\* No start_child succeeds after shutdown begins.
NoStartAfterShutdown == \A s \in Slots : ~lateOk[s]

\* No generation (first start or restart) begins after admission closed.
NoGenerationAfterClose ==
    ~open => \A s \in Slots : gen[s] <= closeGen[s]

\* Budget exhaustion fails the supervisor, which closes it (and so stops
\* every child: ClosedLeadsToDone).
ExhaustionCloses == exhausted => failed /\ ~open

BudgetBounded == budget <= MaxRestarts

\* Shutdown is concurrent: once the supervisor started stopping, every
\* running child has its stop signal; none waits for a sibling.
ConcurrentShutdown ==
    sphase # "running" => \A s \in Slots : cs[s] = "running" => stopSig[s]

\* start_child returned Ok only for a child that became ready.
OkMeansReady == \A s \in Slots : result[s] = "ok" => everReady[s]

Safety ==
    /\ TypeOK /\ AtMostOneGeneration /\ PublishedIsLive
    /\ NoChildOutlivesSupervisor /\ MaxChildrenBound
    /\ TemporaryNeverRestarted /\ TerminatedNeverRestarted
    /\ NoStartAfterShutdown /\ NoGenerationAfterClose
    /\ ExhaustionCloses /\ BudgetBounded /\ ConcurrentShutdown /\ OkMeansReady

NeverReopens == [][open' => open]_vars

(* Liveness, assuming cleanup ends (CleanupStuck is not fair) *)

\* Shutdown or failure ends with every child stopped.
ClosedLeadsToDone == (~open) ~> (sphase = "done")

\* Every start_child call gets an answer.
StartAnswered == \A s \in Slots : (result[s] = "pending") ~> (result[s] # "pending")

\* terminate_child completes: the child is removed.
TerminateCompletes == \A s \in Slots : term[s] ~> (cs[s] = "removed")
=============================================================================
