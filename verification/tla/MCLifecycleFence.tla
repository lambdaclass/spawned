------------------------- MODULE MCLifecycleFence -------------------------
EXTENDS Lifecycle
\* Fences: two late registrations and unlink, nothing else (small and fast).
MCActors == {"a1", "a2"}
MCLinkParent == [a \in MCActors |-> IF a = "a2" THEN "a1" ELSE "none"]
=============================================================================
