------------------------ MODULE NestedSupervisionInd ------------------------
(* Inductive invariant for NestedSupervision.tla, checked with Apalache    *)
(* (apalache.sh) for the constants in MCNestedInd.tla.                     *)
EXTENDS NestedSupervision

\* Shape of a state, usable as an Apalache initial predicate (TypeOK leaves
\* slot, owner and the nested sets untyped).
TypeInd ==
 s \in [nodes: SUBSET Nodes, started: SUBSET Nodes, closed: SUBSET Nodes,
        cancelled: SUBSET Nodes, done: SUBSET Nodes, propagated: SUBSET Nodes,
        arity: [Nodes -> 0..Slots], parent: [Nodes -> 0..NodeCount],
        slot: [Nodes -> 0..Slots], owner: [Nodes -> 0..MaxGen],
        gen: [Nodes -> [Children -> 0..MaxGen]],
        live: [Nodes -> [Children -> SUBSET (1..MaxGen)]],
        badJoin: BOOLEAN, badStart: BOOLEAN]

\* No helper lemmas were needed: Safety is inductive on its own.
\* DoneMeansDescendantsJoined carries itself: a done node is Empty, so
\* TAttach (which needs a live generation of the parent) can never give it
\* a new, unfinished descendant, and TBegin is disabled for done nodes.
IndInv == TypeInd /\ Safety

\* Probe (apalache.sh requires it violated): IndInv admits finished
\* subtrees, so DoneMeansDescendantsJoined is exercised by the step check.
NoFinishedSubtree == ~(\E n \in s.done : Descendants(n) # {})
=============================================================================
