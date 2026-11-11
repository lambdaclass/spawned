------------------------ MODULE MCDynamicSupervisor ------------------------
(* Model-checking instances for DynamicSupervisor (rank 12a design).     *)
EXTENDS DynamicSupervisor

\* One start_child slot per restart type (fast, every PR).
MCMode3 == [s \in 1..3 |-> CASE s = 1 -> "Permanent"
                             [] s = 2 -> "Transient"
                             [] OTHER -> "Temporary"]
=============================================================================
