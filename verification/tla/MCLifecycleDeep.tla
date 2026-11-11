------------------------- MODULE MCLifecycleDeep -------------------------
EXTENDS Lifecycle
\* a1 is supervised by Sup and started a2 with start_linked; a3 is unrelated.
MCActors == {"a1", "a2", "a3"}
MCLinkParent == [a \in MCActors |-> IF a = "a2" THEN "a1" ELSE "none"]
=============================================================================
