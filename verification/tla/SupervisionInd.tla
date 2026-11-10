--------------------------- MODULE SupervisionInd ---------------------------
(***************************************************************************)
(* Inductive invariant for Supervision.tla, checked with Apalache          *)
(* (verification/tla/apalache.sh).                                         *)
(*                                                                         *)
(* IndInv is TypeInd /\ the model's safety invariants /\ helper lemmas.   *)
(* apalache.sh checks, for fixed N and MaxRestarts and every Mode table:   *)
(*                                                                         *)
(*   Init => IndInv                 (--init=Init   --inv=IndInv --length=0) *)
(*   IndInv /\ Next => IndInv'      (--init=IndInv --inv=IndInv --length=1) *)
(*   IndInv /\ Next => A            (--init=IndInv --inv=A      --length=1) *)
(*                                                                         *)
(* where A ranges over the bodies of the action properties. The step check *)
(* starts from EVERY state satisfying IndInv, reachable or not, so it has  *)
(* no depth bound and needs no MaxGen state constraint: generation numbers *)
(* are unbounded SMT integers. It is still a check of one instance size.   *)
(***************************************************************************)
EXTENDS Supervision

Sts == {"Waiting", "Starting", "Running", "Backoff", "Stopping", "Stopped", "Failed"}
SPhases == {"starting", "await", "shutdown", "waitchild", "join", "done"}
Live == {"constructing", "running", "cleanup"}   \* pcs owning a generation

\* Shape of a state, written so Apalache can also use it as an initial
\* predicate. gen ranges over all naturals; alive and pub are determined by
\* H_alive and H_pub (stated as equalities so they also assign them).
TypeInd ==
    /\ open \in BOOLEAN /\ failed \in BOOLEAN
    /\ sphase \in SPhases
    /\ sidx \in 0..N+1
    /\ sready \in BOOLEAN /\ sdone \in BOOLEAN /\ deadline \in BOOLEAN
    /\ budget \in 0..MaxRestarts
    /\ reserved \in SUBSET Children
    /\ timedOut \in SUBSET Children
    /\ pc \in [Children -> Pcs]
    /\ st \in [Children -> Sts]
    /\ gen \in [Children -> Nat]
    /\ exitR \in [Children -> Reasons]
    /\ initial \in [Children -> BOOLEAN] /\ readyGen \in [Children -> BOOLEAN]
    /\ first \in [Children -> BOOLEAN] /\ stopTurn \in [Children -> BOOLEAN]
    /\ cdone \in [Children -> BOOLEAN]

-----------------------------------------------------------------------------
(* Helper lemmas. Each is true of every reachable state; the comment says  *)
(* why, in terms of the actions that can change it.                        *)

\* A child owns a live generation exactly while its controller is between
\* Attempt (adds gen+1) and CleanupDone/CleanupStuck (remove gen), and the
\* live generation is the latest one. Attempt runs only from "attempt",
\* where alive is empty. Implies AtMostOneGeneration.
H_alive == alive = [c \in Children |-> IF pc[c] \in Live THEN {gen[c]} ELSE {}]

\* Generation 0 means "never attempted": a controller holding a generation
\* has gen >= 1.
H_gen == \A c \in Children : pc[c] \in Live => gen[c] >= 1

\* Only BecomeReady publishes (pub := gen, pc := running); every action
\* leaving "running" (SelfExit, StopObserved) and Attempt clear pub.
\* Implies PublishedIsLive.
H_pub == pub = [c \in Children |-> IF pc[c] = "running" THEN gen[c] ELSE 0]

\* `first` is only cleared by LoopTop on its way to "attempt" and is TRUE
\* initially, so a controller with first set has never attempted.
H_first == \A c \in Children :
    first[c] => pc[c] \in {"idle", "top", "finish", "finished"} /\ gen[c] = 0

\* Until SupLaunch reaches c, its controller is idle (SupLaunch overwrites
\* pc[sidx] unguarded, so this is what makes that write safe); once the
\* initial loop is over nothing launches. An idle controller never ran.
H_launch ==
    /\ sphase = "starting" => /\ sidx \in 1..N+1
                              /\ \A c \in Children : (pc[c] = "idle") <=> c >= sidx
    /\ \A c \in Children : pc[c] = "idle" => first[c] /\ gen[c] = 0

\* The ordered start loop launches c+1 only after initial[c]; initial is
\* never cleared. Together with SupStartDone's guard this gives
\* ReadyMeansAllStarted.
H_initial ==
    /\ sphase = "starting" => \A c \in Children : c < sidx - 1 => initial[c]
    /\ sphase = "starting" => ~sready

\* A restartable exit is what sends a controller back to "top" (Decide),
\* and exitR is only rewritten between Attempt and cleanup. So a controller
\* waiting to retry holds an exit reason its restart mode allows. A
\* Temporary child never restarts, so it can only be pre-first-attempt.
H_retry == \A c \in Children :
    /\ pc[c] \in {"top", "backoff", "attempt"} /\ gen[c] > 0 => Restarts(Mode[c], exitR[c])
    /\ pc[c] = "decide" => gen[c] > 0

\* Reverse-order shutdown: while stopping, every child after sidx has
\* finished or was abandoned on timeout (cdone and timedOut only grow).
H_shutdown ==
    /\ sphase \in {"shutdown", "waitchild"} => sidx \in Children
    /\ sphase = "join" => sidx = 0
    /\ sphase \in {"shutdown", "waitchild", "join", "done"} =>
         \A d \in Children : d > sidx => cdone[d] \/ d \in timedOut

\* done is published only by SupJoin, after every controller finished;
\* finished and idle are terminal once the start loop is over.
H_done ==
    /\ sdone <=> sphase = "done"
    /\ sphase = "done" => \A c \in Children : pc[c] \in {"idle", "finished"}

\* Pending reservations never exceed the capacity they are counted in.
H_budget == Cardinality(reserved) <= budget

-----------------------------------------------------------------------------
(* The model's state invariants (Supervision.tla) without TypeOK, whose    *)
(* `Nat` sets Apalache cannot enumerate; TypeInd replaces it.              *)

SafetyInd ==
    /\ AtMostOneGeneration /\ PublishedIsLive /\ BudgetBounded
    /\ TemporaryStartsOnce /\ ReadyMeansAllStarted /\ DoneMeansQuiescent

Helpers ==
    /\ H_gen /\ H_first /\ H_launch /\ H_initial
    /\ H_retry /\ H_shutdown /\ H_done /\ H_budget

IndInv == TypeInd /\ H_alive /\ H_pub /\ Helpers /\ SafetyInd

-----------------------------------------------------------------------------
(* Bodies of the model's action properties ([][A]_vars), checked as       *)
(* Apalache action invariants over one step from IndInv.                   *)

NoStartAfterCloseStep ==
    \A c \in Children : (gen'[c] > gen[c] \/ (pub'[c] # 0 /\ pub[c] = 0)) => open
RestartFollowsPolicyStep ==
    \A c \in Children :
        (gen'[c] > gen[c] /\ gen[c] > 0) => DesignAllowsRestart(Mode[c], exitR[c])
ReverseShutdownStep ==
    \A c \in Children :
        (stopTurn'[c] /\ ~stopTurn[c] /\ ~deadline') =>
            \A d \in Children : d > c => cdone[d] \/ d \in timedOut
NeverReopensStep == open' => open

ActionProps ==
    /\ NoStartAfterCloseStep /\ RestartFollowsPolicyStep
    /\ ReverseShutdownStep /\ NeverReopensStep

-----------------------------------------------------------------------------
(* Non-vacuity probes: apalache.sh requires each of these to be VIOLATED, *)
(* i.e. IndInv admits (and steps from) states far beyond any MaxGen bound *)
(* TLC uses, with every child live, mid-shutdown and holding retries.     *)

NoDeepRunningState ==
    ~(/\ \A c \in Children : pc[c] = "running" /\ gen[c] > 1000
      /\ sready)
NoMidShutdownState ==
    ~(sphase = "waitchild" /\ \E c \in Children : pc[c] = "cleanup" /\ gen[c] > 1000)
NoRestartStep ==
    ~(\E c \in Children : gen'[c] = gen[c] + 1 /\ gen[c] > 1000)
=============================================================================
