---------------------------- MODULE MCLifecycle ----------------------------
EXTENDS Lifecycle
\* a2 was started with start_linked by a1; a3 is supervised by Sup.
MCActors == {"a1", "a2", "a3"}
MCLinkParent == [a \in MCActors |-> IF a = "a2" THEN "a1" ELSE "none"]
=============================================================================
