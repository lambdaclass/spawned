----------------------------- MODULE CohortsInd -----------------------------
(* Inductive invariant for Cohorts.tla, checked with Apalache              *)
(* (apalache.sh): Init => IndInv and IndInv /\ Next => IndInv', from every *)
(* state satisfying IndInv for the constants in MCCohortsInd.tla. The     *)
(* model's own bounds (MaxGen, MaxRounds) are guards of its actions, so    *)
(* they are part of the checked system, not a search depth.               *)
EXTENDS Cohorts

Phases == {"Idle", "Assign", "Stopping", "Starting", "Done"}

\* Shape of a state, usable as an Apalache initial predicate (TypeOK does
\* not type origin, start or scanned).
TypeInd ==
 s \in [open: BOOLEAN, initial: SUBSET Children, committed: BOOLEAN,
        phase: Phases, epoch: Epochs, origin: 0..N, start: 0..N,
        pending: SUBSET Children, covered: [Children -> Epochs],
        assigned: [Children -> Epochs], scanned: SUBSET Children,
        affected: SUBSET Children, stopped: SUBSET Children,
        clean: SUBSET Children, resumed: SUBSET Children,
        ack: SUBSET Children, terminal: SUBSET Children,
        gen: [Children -> 0..MaxGen], live: [Children -> SUBSET (1..MaxGen)],
        published: [Children -> 0..MaxGen],
        begun: SUBSET Children,
        badBarrier: BOOLEAN, badClosed: BOOLEAN, badTerminal: BOOLEAN,
        badRetry: BOOLEAN]

-----------------------------------------------------------------------------
(* Helper lemmas, each with the reason it holds in every reachable state. *)
(* Only two were needed: AtMostOneGeneration, PublishedIsLive,            *)
(* StrategySelection, NoStartAfterClose, NoRetiredParticipant and          *)
(* TemporaryStartsOnce are inductive by themselves (each is re-checked by *)
(* the guard of the one action that could break it).                      *)

\* The whole-cohort barrier: CResume needs clean = affected, and after a
\* resume only CClean changes clean, adding a stopped (hence affected)
\* child. CSelect clears resumed together with affected/clean/stopped, and
\* CResume is the only action adding to resumed, from Stopping/Starting.
H_barrier ==
 /\ s.stopped \subseteq s.affected
 /\ s.clean \subseteq s.stopped
 /\ s.resumed # {} => s.clean = s.affected
 /\ s.phase \in {"Assign", "Stopping"} => s.resumed = {}

\* CDone requires a closed supervisor, and nothing reopens it; without this
\* a "Done" state could be open and CBegin would make DoneMeansQuiescent
\* false.
H_done == s.phase = "Done" => ~s.open

\* S1: recovery rounds exist only after the tree committed ready, which
\* needs every child's first start. CRequest needs committed; CSelect needs
\* a pending request; CAssign assigns the selected round. So a round-r > 0
\* CBegin never runs for a child without its first start (FirstStartOnce).
H_first ==
 /\ s.committed => s.initial = Children
 /\ s.pending # {} => s.committed
 /\ s.epoch > 0 => s.committed
 /\ \A c \in Children : s.assigned[c] > 0 => s.committed

IndInv == TypeInd /\ H_barrier /\ H_done /\ H_first /\ Safety

\* Probe (apalache.sh requires it violated): IndInv admits a mid-round state
\* with live generations, i.e. the step check is not over an empty set.
NoMidRoundState ==
 ~(s.phase = "Starting" /\ s.epoch = MaxRounds /\ \A c \in Children : s.live[c] # {})
=============================================================================
