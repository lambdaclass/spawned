------------------------ MODULE MCDynamicSupervisor ------------------------
(* Model-checking instances for DynamicSupervisor (rank 12a design).     *)
EXTENDS DynamicSupervisor

\* Two terminate calls on slot 1: by the id the caller saw, and by slot
\* fenced on the expected id@generation (introspection's console).
MCCalls2     == {1, 2}
MCCallSlot2  == [c \in MCCalls2 |-> 1]
MCCallKind2  == [c \in MCCalls2 |-> IF c = 1 THEN "id" ELSE "expect"]
\* One terminate call by slot (unfenced).
MCCalls1     == {1}
MCCallSlot1  == [c \in MCCalls1 |-> 1]
MCCallKind1  == [c \in MCCalls1 |-> "slot"]
\* One terminate_slot call fenced with expect.
MCCalls1e    == {1}
MCCallSlot1e == [c \in MCCalls1e |-> 1]
MCCallKind1e == [c \in MCCalls1e |-> "expect"]
\* No terminate calls.
MCCalls0     == {}
MCCallSlot0  == [c \in MCCalls0 |-> 1]
MCCallKind0  == [c \in MCCalls0 |-> "id"]
=============================================================================
