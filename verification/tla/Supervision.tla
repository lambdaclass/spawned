---------------------------- MODULE Supervision ----------------------------
(***************************************************************************)
(* Model of the implemented OneForOne supervisor in                        *)
(* concurrency/src/supervision.rs (run_supervisor / run_child).            *)
(*                                                                         *)
(* One supervisor with N ordered children. Each child controller is a      *)
(* small state machine (pc) that follows run_child's loop; the supervisor  *)
(* follows run_supervisor. Time is abstracted: backoff sleeps, startup and *)
(* cleanup deadlines and the rolling restart window are nondeterministic   *)
(* actions. Each action is named after the trace event the implementation *)
(* emits for it (see TraceSupervision.tla and verification/tla/README.md).          *)
(*                                                                         *)
(* Abstractions:                                                           *)
(*  - The admission gate and core.cancel are closed together by            *)
(*    Core::close, so they are one boolean `open`.                         *)
(*  - The rolling window is a counter released by WindowExpire.            *)
(*  - ExecutionGroup closure and placement failure are not modelled.       *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences

\* The `@type:` comments are Apalache type annotations (SupervisionInd.tla,
\* apalache.sh); TLC ignores them.
CONSTANTS
    \* @type: Int;
    N,            \* number of children, started in order 1..N
    \* @type: Int -> Str;
    Mode,         \* Mode[c] \in {"Permanent", "Transient", "Temporary"}
    \* @type: Int;
    MaxRestarts,  \* SupervisorConfig::max_restarts
    \* @type: Int;
    MaxGen        \* bound on generations per child (state constraint only)

Children == 1..N
Modes    == {"Permanent", "Transient", "Temporary"}
\* Exit reason recorded for a generation. "none" = no actor exit reason
\* (factory failure, or startup abandoned before an actor existed).
\* "timeout" = startup deadline exceeded.
\* A running actor may also exit with Shutdown while the supervisor is open
\* (ChildHandle::shutdown): the design restarts it if Permanent and keeps a
\* Transient child stopped. Found by trace validation.
SelfExits      == {"Normal", "Shutdown", "Panic", "Kill"}
StartupResults == {"none", "timeout", "Normal", "Panic", "Shutdown"}
Reasons        == {"none", "timeout", "Normal", "Shutdown", "Panic", "Kill"}

ASSUME N \in Nat \ {0} /\ MaxRestarts \in Nat /\ MaxGen \in Nat \ {0}
ASSUME \A c \in Children : Mode[c] \in Modes

\* ExitReason::is_abnormal plus RestartMode::restarts, and the startup
\* timeout special case in run_child.
Restarts(m, r) ==
    IF r = "timeout" THEN m # "Temporary"
    ELSE \/ m = "Permanent"
         \/ m = "Transient" /\ r \in {"none", "Panic", "Kill"}

\* Whether a controller retries after cleanup. A generation that never became
\* ready after the child's initial readiness is a failed restart attempt,
\* retried whatever its exit reason (as OTP retries a failed restart), unless
\* Temporary. Before initial readiness the restart table applies, so a
\* Transient child stopping Normal or Shutdown in its first startup fails it.
Retries(m, r, ready, init) ==
    \/ Restarts(m, r)
    \/ ~ready /\ init /\ m # "Temporary"

Pcs == {"idle", "top", "backoff", "attempt", "constructing", "running",
        "cleanup", "decide", "finish", "finished"}

VARIABLES
    \* @type: Bool;
    open,       \* admission gate / core.cancel not yet closed
    \* @type: Bool;
    failed,     \* core.failure is set
    \* @type: Str;
    sphase,     \* run_supervisor program counter
    \* @type: Int;
    sidx,       \* run_supervisor loop index
    \* @type: Bool;
    sready,     \* core.ready published
    \* @type: Bool;
    sdone,      \* core.done published
    \* @type: Set(Int);
    reserved,   \* pending backoffs holding retry capacity
    \* @type: Int;
    budget,     \* committed entries plus pending reservations
    \* @type: Bool;
    deadline,   \* Supervisor::shutdown deadline hit (stop_remaining used)
    \* @type: Set(Int);
    timedOut,   \* children whose cleanup deadline run_supervisor gave up on
    \* @type: Int -> Str;
    pc,
    \* @type: Int -> Str;
    st,
    \* @type: Int -> Int;
    gen,
    \* @type: Int -> Set(Int);
    alive,
    \* @type: Int -> Int;
    pub,
    \* @type: Int -> Bool;
    initial,
    \* @type: Int -> Str;
    exitR,
    \* @type: Int -> Bool;
    readyGen,
    \* @type: Int -> Bool;
    first,
    \* @type: Int -> Bool;
    stopTurn,
    \* @type: Int -> Bool;
    cdone

sup   == <<open, failed, sphase, sidx, sready, sdone, budget, reserved, deadline, timedOut>>
child == <<pc, st, gen, alive, pub, initial, exitR, readyGen, first, stopTurn, cdone>>
vars  == <<sup, child>>

TypeOK ==
    /\ open \in BOOLEAN /\ failed \in BOOLEAN
    /\ sphase \in {"starting", "await", "shutdown", "waitchild", "join", "done"}
    /\ sidx \in 0..N+1
    /\ sready \in BOOLEAN /\ sdone \in BOOLEAN /\ deadline \in BOOLEAN
    /\ budget \in 0..MaxRestarts
    /\ reserved \subseteq Children /\ Cardinality(reserved) <= budget
    /\ timedOut \subseteq Children
    /\ pc \in [Children -> Pcs]
    /\ st \in [Children -> {"Waiting", "Starting", "Running", "Backoff",
                             "Stopping", "Stopped", "Failed"}]
    /\ gen \in [Children -> Nat]
    /\ alive \in [Children -> SUBSET Nat]
    /\ pub \in [Children -> Nat]
    /\ exitR \in [Children -> Reasons]
    /\ initial \in [Children -> BOOLEAN] /\ readyGen \in [Children -> BOOLEAN]
    /\ first \in [Children -> BOOLEAN] /\ stopTurn \in [Children -> BOOLEAN]
    /\ cdone \in [Children -> BOOLEAN]

Init ==
    /\ open = TRUE /\ failed = FALSE
    /\ sphase = "starting" /\ sidx = 1
    /\ sready = FALSE /\ sdone = FALSE /\ budget = 0 /\ reserved = {}
    /\ deadline = FALSE /\ timedOut = {}
    /\ pc = [c \in Children |-> "idle"]
    /\ st = [c \in Children |-> "Waiting"]
    /\ gen = [c \in Children |-> 0]
    /\ alive = [c \in Children |-> {}]
    /\ pub = [c \in Children |-> 0]
    /\ initial = [c \in Children |-> FALSE]
    /\ exitR = [c \in Children |-> "none"]
    /\ readyGen = [c \in Children |-> FALSE]
    /\ first = [c \in Children |-> TRUE]
    /\ stopTurn = [c \in Children |-> FALSE]
    /\ cdone = [c \in Children |-> FALSE]

\* Core::fail: record the first failure and close.
Fail == failed' = TRUE /\ open' = FALSE

Goto(c, p) == pc' = [pc EXCEPT ![c] = p]

-----------------------------------------------------------------------------
(* External / environment actions *)

\* Supervisor::stop, ActorSystem shutdown, or StartupGuard drop.
Close ==
    /\ open
    /\ open' = FALSE
    /\ UNCHANGED <<failed, sphase, sidx, sready, sdone, budget, reserved, deadline, timedOut, child>>

\* Rolling window releases its oldest retry entry.
WindowExpire ==
    /\ budget > Cardinality(reserved)
    /\ budget' = budget - 1
    /\ UNCHANGED <<open, failed, sphase, sidx, sready, sdone, reserved, deadline, timedOut, child>>

\* Supervisor::shutdown deadline elapsed: stop_remaining stops every child
\* at once (documented incomplete-shutdown behaviour).
ShutdownDeadline ==
    /\ ~open /\ ~deadline /\ ~sdone
    /\ deadline' = TRUE
    /\ stopTurn' = [c \in Children |-> TRUE]
    /\ UNCHANGED <<open, failed, sphase, sidx, sready, sdone, budget, reserved, timedOut,
                   pc, st, gen, alive, pub, initial, exitR, readyGen, first, cdone>>

-----------------------------------------------------------------------------
(* Child controller: run_child *)

LoopTop(c) ==
    /\ pc[c] = "top"
    /\ IF ~open THEN Goto(c, "finish") /\ UNCHANGED <<st, first>>
       ELSE IF first[c] THEN Goto(c, "attempt") /\ first' = [first EXCEPT ![c] = FALSE]
                              /\ UNCHANGED st
       ELSE Goto(c, "backoff") /\ st' = [st EXCEPT ![c] = "Backoff"] /\ UNCHANGED first
    /\ UNCHANGED <<sup, gen, alive, pub, initial, exitR, readyGen, stopTurn, cdone>>

\* Reserve capacity before waiting. Cancellation/supersession refunds it;
\* only a committed grant enters the rolling time window.
RetryReserve(c) ==
    /\ pc[c] = "backoff" /\ open /\ c \notin reserved /\ budget < MaxRestarts
    /\ reserved' = reserved \cup {c} /\ budget' = budget + 1
    /\ UNCHANGED <<open, failed, sphase, sidx, sready, sdone, deadline, timedOut, child>>
RetryRelease(c) ==
    /\ c \in reserved /\ reserved' = reserved \ {c} /\ budget' = budget - 1
    /\ UNCHANGED <<open, failed, sphase, sidx, sready, sdone, deadline, timedOut, child>>
BackoffDone(c) ==
    /\ pc[c] = "backoff"
    /\ IF ~open THEN /\ c \notin reserved /\ Goto(c, "finish")
                       /\ UNCHANGED <<open, failed, budget, reserved>>
       ELSE IF c \in reserved
            THEN /\ reserved' = reserved \ {c} /\ Goto(c, "attempt")
                 /\ UNCHANGED <<open, failed, budget>>
            ELSE /\ budget = MaxRestarts /\ Fail /\ Goto(c, "finish")
                 /\ UNCHANGED <<budget, reserved>>
    /\ UNCHANGED <<sphase, sidx, sready, sdone, deadline, timedOut,
                   st, gen, alive, pub, initial, exitR, readyGen, first, stopTurn, cdone>>

\* Reserve a scope and publish a new generation under gate.if_open.
Attempt(c) ==
    /\ pc[c] = "attempt"
    /\ IF open
       THEN /\ gen' = [gen EXCEPT ![c] = gen[c] + 1]
            /\ alive' = [alive EXCEPT ![c] = alive[c] \cup {gen[c] + 1}]
            /\ st' = [st EXCEPT ![c] = "Starting"]
            /\ pub' = [pub EXCEPT ![c] = 0]
            /\ exitR' = [exitR EXCEPT ![c] = "none"]
            /\ readyGen' = [readyGen EXCEPT ![c] = FALSE]
            /\ Goto(c, "constructing")
       ELSE Goto(c, "finish") /\ UNCHANGED <<gen, alive, st, pub, exitR, readyGen>>
    /\ UNCHANGED <<sup, initial, first, stopTurn, cdone>>

\* Construction and actor.ready() succeed; publish under gate.if_open.
\* If the gate closed meanwhile the instance is not published and is
\* cleaned up (exit observed later as Shutdown).
BecomeReady(c) ==
    /\ pc[c] = "constructing"
    /\ IF open
       THEN /\ pub' = [pub EXCEPT ![c] = gen[c]]
            /\ st' = [st EXCEPT ![c] = "Running"]
            /\ initial' = [initial EXCEPT ![c] = TRUE]
            /\ readyGen' = [readyGen EXCEPT ![c] = TRUE]
            /\ Goto(c, "running")
            /\ UNCHANGED exitR
       ELSE /\ exitR' = [exitR EXCEPT ![c] = "Shutdown"]
            /\ Goto(c, "cleanup")
            /\ UNCHANGED <<pub, st, initial, readyGen>>
    /\ UNCHANGED <<sup, gen, alive, first, stopTurn, cdone>>

\* Factory error/panic, startup timeout, or the actor exiting during
\* started() (with any reason it stops with, Shutdown included while the
\* supervisor is open: found by trace validation); also admission closing
\* while constructing.
StartupFailed(c, r) ==
    /\ pc[c] = "constructing"
    /\ r \in StartupResults
    /\ exitR' = [exitR EXCEPT ![c] = r]
    /\ Goto(c, "cleanup")
    /\ UNCHANGED <<sup, st, gen, alive, pub, initial, readyGen, first, stopTurn, cdone>>

\* The ready actor exits on its own (normal stop, external shutdown, panic,
\* linked Kill).
\* current() stops returning it as soon as the scope token is cancelled.
SelfExit(c, r) ==
    /\ pc[c] = "running"
    /\ r \in SelfExits
    /\ exitR' = [exitR EXCEPT ![c] = r]
    /\ pub' = [pub EXCEPT ![c] = 0]
    /\ Goto(c, "cleanup")
    /\ UNCHANGED <<sup, st, gen, alive, initial, readyGen, first, stopTurn, cdone>>

\* Supervisor shutdown reached this child's stop turn: scope.stop(Shutdown).
StopObserved(c) ==
    /\ pc[c] = "running"
    /\ ~open /\ stopTurn[c]
    /\ exitR' = [exitR EXCEPT ![c] = "Shutdown"]
    /\ pub' = [pub EXCEPT ![c] = 0]
    /\ Goto(c, "cleanup")
    /\ UNCHANGED <<sup, st, gen, alive, initial, readyGen, first, stopTurn, cdone>>

\* Actor exit plus scope.joined(): every owned task of this generation ended.
CleanupDone(c) ==
    /\ pc[c] = "cleanup"
    /\ alive' = [alive EXCEPT ![c] = alive[c] \ {gen[c]}]
    /\ pub' = [pub EXCEPT ![c] = 0]
    /\ st' = [st EXCEPT ![c] = "Stopping"]
    /\ Goto(c, "decide")
    /\ UNCHANGED <<sup, gen, initial, exitR, readyGen, first, stopTurn, cdone>>

\* Cleanup exceeded shutdown_timeout: Core::fail, still wait for joined,
\* then leave the loop.
CleanupStuck(c) ==
    /\ pc[c] = "cleanup"
    /\ Fail
    /\ alive' = [alive EXCEPT ![c] = alive[c] \ {gen[c]}]
    /\ pub' = [pub EXCEPT ![c] = 0]
    /\ st' = [st EXCEPT ![c] = "Stopping"]
    /\ Goto(c, "finish")
    /\ UNCHANGED <<sphase, sidx, sready, sdone, budget, reserved, deadline, timedOut,
                   gen, initial, exitR, readyGen, first, stopTurn, cdone>>

Decide(c) ==
    /\ pc[c] = "decide"
    /\ IF ~open THEN Goto(c, "finish") /\ UNCHANGED <<open, failed>>
       ELSE IF Retries(Mode[c], exitR[c], readyGen[c], initial[c]) THEN Goto(c, "top") /\ UNCHANGED <<open, failed>>
       ELSE IF ~readyGen[c] THEN Fail /\ Goto(c, "finish")   \* non-restartable startup failure
       ELSE Goto(c, "finish") /\ UNCHANGED <<open, failed>>
    /\ UNCHANGED <<sphase, sidx, sready, sdone, budget, reserved, deadline, timedOut,
                   st, gen, alive, pub, initial, exitR, readyGen, first, stopTurn, cdone>>

\* view.finish(core.failure.is_some())
Finish(c) ==
    /\ pc[c] = "finish"
    /\ st' = [st EXCEPT ![c] = IF failed THEN "Failed" ELSE "Stopped"]
    /\ pub' = [pub EXCEPT ![c] = 0]
    /\ cdone' = [cdone EXCEPT ![c] = TRUE]
    /\ Goto(c, "finished")
    /\ UNCHANGED <<sup, gen, alive, initial, exitR, readyGen, first, stopTurn>>

-----------------------------------------------------------------------------
(* Supervisor: run_supervisor *)

\* Spawn the controller for child sidx once the previous child published
\* its first readiness.
SupLaunch ==
    /\ sphase = "starting" /\ open /\ sidx \in Children
    /\ IF sidx = 1 THEN TRUE ELSE initial[sidx - 1]
    /\ pc' = [pc EXCEPT ![sidx] = "top"]
    /\ sidx' = sidx + 1
    /\ UNCHANGED <<open, failed, sphase, sready, sdone, budget, reserved, deadline, timedOut,
                   st, gen, alive, pub, initial, exitR, readyGen, first, stopTurn, cdone>>

\* Initial loop ends: all children reached initial readiness, or admission
\* closed. finish_unstarted marks generation-0 children Stopped; readiness
\* is published only if the gate is still open.
SupStartDone ==
    /\ sphase = "starting"
    /\ \/ ~open
       \/ sidx = N + 1 /\ initial[N]
    /\ st' = [c \in Children |-> IF gen[c] = 0 THEN (IF failed THEN "Failed" ELSE "Stopped") ELSE st[c]]
    /\ cdone' = [c \in Children |-> IF gen[c] = 0 THEN TRUE ELSE cdone[c]]
    /\ sready' = open
    /\ sphase' = "await"
    /\ UNCHANGED <<open, failed, sidx, sdone, budget, reserved, deadline, timedOut,
                   pc, gen, alive, pub, initial, exitR, readyGen, first, stopTurn>>

SupCancelSeen ==
    /\ sphase = "await" /\ ~open
    /\ sphase' = "shutdown" /\ sidx' = N
    /\ UNCHANGED <<open, failed, sready, sdone, budget, reserved, deadline, timedOut, child>>

\* Reverse-order stop: child.stop() for sidx.
SupStopTurn ==
    /\ sphase = "shutdown" /\ sidx \in Children
    /\ stopTurn' = [stopTurn EXCEPT ![sidx] = TRUE]
    /\ sphase' = "waitchild"
    /\ UNCHANGED <<open, failed, sidx, sready, sdone, budget, reserved, deadline, timedOut,
                   pc, st, gen, alive, pub, initial, exitR, readyGen, first, cdone>>

SupChildDone ==
    /\ sphase = "waitchild" /\ cdone[sidx]
    /\ sidx' = sidx - 1
    /\ sphase' = IF sidx = 1 THEN "join" ELSE "shutdown"
    /\ UNCHANGED <<open, failed, sready, sdone, budget, reserved, deadline, timedOut, child>>

\* Child cleanup deadline exceeded: fail and move on to the next child.
SupChildTimeout ==
    /\ sphase = "waitchild" /\ ~cdone[sidx]
    /\ failed' = TRUE
    /\ timedOut' = timedOut \cup {sidx}
    /\ sidx' = sidx - 1
    /\ sphase' = IF sidx = 1 THEN "join" ELSE "shutdown"
    /\ UNCHANGED <<open, sready, sdone, budget, reserved, deadline, child>>

\* Join every controller task, then publish done.
SupJoin ==
    /\ sphase = "join"
    /\ \A c \in Children : pc[c] \in {"idle", "finished"}
    /\ sdone' = TRUE /\ sphase' = "done"
    /\ st' = [c \in Children |-> IF gen[c] = 0 /\ failed THEN "Failed" ELSE st[c]]
    /\ UNCHANGED <<open, failed, sidx, sready, budget, reserved, deadline, timedOut,
                   pc, gen, alive, pub, initial, exitR, readyGen, first, stopTurn, cdone>>

-----------------------------------------------------------------------------

ChildStep(c) ==
    \/ LoopTop(c) \/ RetryReserve(c) \/ RetryRelease(c) \/ BackoffDone(c) \/ Attempt(c) \/ BecomeReady(c)
    \/ \E r \in StartupResults : StartupFailed(c, r)
    \/ \E r \in SelfExits : SelfExit(c, r)
    \/ StopObserved(c) \/ CleanupDone(c) \/ CleanupStuck(c)
    \/ Decide(c) \/ Finish(c)

SupStep ==
    \/ SupLaunch \/ SupStartDone \/ SupCancelSeen \/ SupStopTurn
    \/ SupChildDone \/ SupChildTimeout \/ SupJoin

Next ==
    \/ Close \/ WindowExpire \/ ShutdownDeadline
    \/ SupStep
    \/ \E c \in Children : ChildStep(c)

\* Fairness: controllers and the supervisor make progress; cleanup
\* completes (CleanupDone is fair, CleanupStuck and timeouts are not).
Fairness ==
    /\ \A c \in Children :
         /\ WF_vars(RetryReserve(c)) /\ WF_vars(RetryRelease(c))
         /\ WF_vars(LoopTop(c)) /\ WF_vars(BackoffDone(c)) /\ WF_vars(Attempt(c))
         /\ WF_vars(StopObserved(c)) /\ WF_vars(CleanupDone(c))
         /\ WF_vars(Decide(c)) /\ WF_vars(Finish(c))
         /\ WF_vars(\E r \in {"Shutdown"} : StartupFailed(c, r))
    /\ WF_vars(SupLaunch) /\ WF_vars(SupStartDone) /\ WF_vars(SupCancelSeen)
    /\ WF_vars(SupStopTurn) /\ WF_vars(SupChildDone) /\ WF_vars(SupJoin)

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* Safety properties *)

\* Never two generations of one child slot alive at once (design: "Keep at
\* most one live generation per child slot").
AtMostOneGeneration == \A c \in Children : Cardinality(alive[c]) <= 1

\* SupervisedChild::current() only returns the live, latest generation.
PublishedIsLive ==
    \A c \in Children : pub[c] # 0 => pub[c] = gen[c] /\ pub[c] \in alive[c]

BudgetBounded == budget <= MaxRestarts

TemporaryStartsOnce ==
    \A c \in Children : Mode[c] = "Temporary" => gen[c] <= 1

\* "never report a partly ready tree as ready"
ReadyMeansAllStarted == sready => \A c \in Children : initial[c]

\* Supervisor done implies no generation is still alive and every
\* controller has finished.
DoneMeansQuiescent ==
    sdone => \A c \in Children : alive[c] = {} /\ pc[c] \in {"idle", "finished"}

\* A closed supervisor never reopens.
Safety ==
    /\ TypeOK /\ AtMostOneGeneration /\ PublishedIsLive /\ BudgetBounded
    /\ TemporaryStartsOnce /\ ReadyMeansAllStarted /\ DoneMeansQuiescent

(* Action properties *)

\* "No child starts after closure" and no publication after closure.
NoStartAfterClose ==
    [][\A c \in Children : (gen'[c] > gen[c] \/ (pub'[c] # 0 /\ pub[c] = 0)) => open]_vars

\* The design's restart table (docs/design/execution-groups-supervision.md),
\* stated independently of the Restarts operator the actions use:
\* Permanent restarts after any exit, Transient only after an abnormal exit
\* or failed startup, Temporary never.
DesignAllowsRestart(m, r) ==
    CASE m = "Temporary" -> FALSE
      [] m = "Permanent" -> TRUE
      [] m = "Transient" -> r \in {"none", "timeout", "Panic", "Kill"}

\* A new generation after the first only follows an exit for which the
\* design table allows a restart, or a failed restart attempt (a generation
\* that never became ready after the child's initial readiness) of a child
\* that is not Temporary.
RestartFollowsPolicy ==
    [][\A c \in Children :
        (gen'[c] > gen[c] /\ gen[c] > 0) =>
            \/ DesignAllowsRestart(Mode[c], exitR[c])
            \/ ~readyGen[c] /\ initial[c] /\ Mode[c] # "Temporary"]_vars

\* Ready children are stopped in reverse declaration order: a stop turn is
\* only given once every later child finished or was abandoned, unless the
\* caller's shutdown deadline forced stop_remaining.
ReverseShutdown ==
    [][\A c \in Children :
        (stopTurn'[c] /\ ~stopTurn[c] /\ ~deadline') =>
            \A d \in Children : d > c => cdone[d] \/ d \in timedOut]_vars

NeverReopens == [][open' => open]_vars

(* Liveness: once closed, the supervisor eventually finishes. *)
ClosedLeadsToDone == (~open) ~> sdone

\* Bound generations for finite model checking.
GenBound == \A c \in Children : gen[c] <= MaxGen
=============================================================================
