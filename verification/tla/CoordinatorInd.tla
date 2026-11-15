--------------------------- MODULE CoordinatorInd ---------------------------
(* Inductive invariant for Coordinator.tla, checked with Apalache          *)
(* (apalache.sh) for the constants in MCCoordinatorInd.tla:                *)
(*                                                                         *)
(*   Init => IndInv                 (--init=Init   --inv=IndInv --length=0) *)
(*   IndInv /\ Next => IndInv'      (--init=IndInv --inv=IndInv --length=1) *)
(*   IndInv /\ Next => A            (--init=IndInv --inv=A      --length=1) *)
(*                                                                         *)
(* The step check starts from every state satisfying IndInv, reachable or *)
(* not, so it has no depth bound; it is still one value of TargetCount.    *)
EXTENDS Coordinator

Phases == {"Running", "Ordering", "Waiting", "Emergency", "Joining", "Done"}

\* Shape of a state, usable as an Apalache initial predicate.
TypeInd ==
    /\ phase \in Phases
    /\ open \in BOOLEAN
    /\ signalled \in SUBSET Targets
    /\ finished \in SUBSET Targets
    /\ index \in 1..TargetCount + 1
    /\ crashed \in BOOLEAN /\ reported \in BOOLEAN /\ released \in BOOLEAN
    /\ expired \in BOOLEAN /\ killed \in SUBSET Targets

-----------------------------------------------------------------------------
(* Helper lemma (the only one needed: without it Apalache returns a CTI   *)
(* that starts the ordered loop from a Running state with index = 9).      *)
(* Safety, OrderedShutdown and JoinAfterAllSignalled carry each other      *)
(* otherwise: JoinAfterAllSignalled follows across Ordered from            *)
(* OrderedShutdown with index > TargetCount, and across Emergency because  *)
(* it signals every target.                                                *)

\* The cursor starts at the first target in stop order: index is only
\* changed by Advance, which needs phase = "Waiting", and nothing returns
\* to Running. Begin then opens the ordered loop at target 1.
H_cursor == phase = "Running" => index = 1

\* The grace expires only in the ordered loop, which it ends: GraceExpired
\* moves to Emergency, from which the loop is never re-entered.
H_expired == expired => phase \in {"Emergency", "Joining", "Done"}

-----------------------------------------------------------------------------
(* The model's state invariants (Coordinator.tla). Safety contains its own *)
(* typing conjuncts; OrderedShutdown and JoinAfterAllSignalled are the     *)
(* ordered-shutdown properties.                                            *)

SafetyInd == Safety /\ OrderedShutdown /\ JoinAfterAllSignalled
             /\ KillOnlyAtGrace /\ GraceKillsRemaining

Helpers == H_cursor /\ H_expired

IndInv == TypeInd /\ Helpers /\ SafetyInd

-----------------------------------------------------------------------------
(* Action properties, checked over one step from IndInv.                   *)

\* Ordered shutdown, stated per step: outside the emergency fallback, a
\* target is newly signalled only when every earlier target in stop order
\* (every later-admitted root) has finished.
OrderedSignalStep ==
    phase # "Emergency" =>
        \A i \in Targets : (i \in signalled' /\ i \notin signalled) =>
            \A j \in Targets : j < i => j \in finished
\* Admission never reopens once closed.
NeverReopensStep == open' => open
\* A recorded coordinator failure is never dropped.
ReportKeptStep == reported => reported'
\* Finished targets stay finished and runtime ownership is released once.
TerminalStep == finished \subseteq finished' /\ (released => released')

ActionProps ==
    /\ OrderedSignalStep /\ NeverReopensStep /\ ReportKeptStep /\ TerminalStep

-----------------------------------------------------------------------------
(* Non-vacuity probes: apalache.sh requires each to be VIOLATED, i.e.      *)
(* IndInv admits (and steps from) states well past TLC's TargetCount = 3.  *)

\* An ordered loop waiting on the last target with all earlier ones done.
NoLateWaitingState == ~(phase = "Waiting" /\ index = TargetCount /\ TargetCount > 3)
\* A crash in the middle of the ordered loop, then an emergency join with
\* only part of the targets finished.
NoPartialEmergencyJoin ==
    ~(phase = "Joining" /\ crashed /\ finished # {} /\ finished # Targets
      /\ phase' = "Joining" /\ finished' # finished)
=============================================================================
