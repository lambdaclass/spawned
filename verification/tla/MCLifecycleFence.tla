------------------------- MODULE MCLifecycleFence -------------------------
EXTENDS Lifecycle
\* Fences: two late registrations and unlink, plus one actor-to-actor message
\* (the per-PR MCLifecycle has none); small and fast.
MCActors == {"a1", "a2"}
MCLinkParent == [a \in MCActors |-> IF a = "a2" THEN "a1" ELSE "none"]
=============================================================================
