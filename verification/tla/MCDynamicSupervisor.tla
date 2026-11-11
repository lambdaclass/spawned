------------------------ MODULE MCDynamicSupervisor ------------------------
(* Model-checking instances for DynamicSupervisor (rank 12a design).     *)
EXTENDS DynamicSupervisor

\* Two terminate calls on slot 1: one by the id the caller saw, one by slot.
MCCalls2     == {1, 2}
MCCallSlot2  == [c \in MCCalls2 |-> 1]
MCCallKind2  == [c \in MCCalls2 |-> IF c = 1 THEN "id" ELSE "slot"]
\* One terminate call by id.
MCCalls1     == {1}
MCCallSlot1  == [c \in MCCalls1 |-> 1]
MCCallKind1  == [c \in MCCalls1 |-> "id"]
\* No terminate calls.
MCCalls0     == {}
MCCallSlot0  == [c \in MCCalls0 |-> 1]
MCCallKind0  == [c \in MCCalls0 |-> "id"]
=============================================================================
