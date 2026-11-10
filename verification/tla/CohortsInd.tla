----------------------------- MODULE CohortsInd -----------------------------
(* Inductive invariant for Cohorts.tla, checked with Apalache              *)
(* (apalache.sh): Init => IndInv and IndInv /\ Next => IndInv', from every *)
(* state satisfying IndInv for the constants in MCCohortsInd.tla. The     *)
(* model's own bounds (MaxGen, MaxRounds) are guards of its actions, so    *)
(* they are part of the checked system, not a search depth.               *)
EXTENDS Cohorts

Phases == {"Idle", "Assign", "Stopping", "Starting", "Done"}

\* Shape of a state, usable as an Apalache initial predicate. origin and
\* start are not typed by TypeOK; CSelect only ever stores children there.
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
        badBarrier: BOOLEAN, badClosed: BOOLEAN, badTerminal: BOOLEAN]

-----------------------------------------------------------------------------
(* Helper lemmas, each with the reason it holds in every reachable state. *)

\* CBegin requires live = {} and adds gen' = gen + 1; CJoined only removes.
\* So the only live generation is the latest one (gives AtMostOneGeneration).
H_live == \A c \in Children : s.live[c] \subseteq {s.gen[c]}

\* The whole-cohort barrier: CResume needs clean = affected, and after a
\* resume only CClean changes clean, adding a stopped (hence affected)
\* child. CSelect clears resumed together with affected/clean/stopped, and
\* CResume is the only action adding to resumed, from Stopping/Starting.
H_barrier ==
 /\ s.stopped \subseteq s.affected
 /\ s.clean \subseteq s.stopped
 /\ s.resumed # {} => s.clean = s.affected
 /\ s.phase \in {"Assign", "Stopping"} => s.resumed = {}

\* CDone requires a closed supervisor, and nothing reopens it.
H_done == s.phase = "Done" => ~s.open

\* A round in progress started from CSelect, which stores a child as start
\* and origin and a positive epoch.
H_round ==
 /\ s.phase = "Assign" => s.start \in Children
 /\ s.epoch > 0 => s.origin \in Children

IndInv == TypeInd /\ H_live /\ H_barrier /\ H_done /\ H_round /\ Safety

\* Probe (apalache.sh requires it violated): IndInv admits a mid-round state
\* with live generations, i.e. the step check is not over an empty set.
NoMidRoundState ==
 ~(s.phase = "Starting" /\ s.epoch = MaxRounds /\ \A c \in Children : s.live[c] # {})
=============================================================================
