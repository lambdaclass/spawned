---------------------------- MODULE MCLifecycleWideDeep ----------------------------
EXTENDS Lifecycle
\* The former per-PR MCLifecycle (43M states), weekly since the per-PR
\* MCLifecycle was made smaller. a1 is supervised by Sup and started a2
\* with start_linked.
MCActors == {"a1", "a2"}
MCLinkParent == [a \in MCActors |-> IF a = "a2" THEN "a1" ELSE "none"]
=============================================================================
