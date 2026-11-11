---------------------------- MODULE MCLifecycle ----------------------------
EXTENDS Lifecycle
\* a1 is supervised by Sup and started a2 with start_linked.
\* Per-PR safety instance (7.5M states): every behaviour class but fewer
\* reasons: self-exits Normal (Panic still comes from handler panics and
\* PanicWhileExiting), signals Normal and Kill (Shutdown still comes from
\* administrative stops and the supervisor), no actor-to-actor message (in
\* MCLifecycleFence). Every Lifecycle negative control fails here on its
\* named invariant. MCLifecycleWideDeep (weekly) is the former 43M-state
\* instance, MCLifecycleDeep (weekly) every reason for every operation.
MCActors == {"a1", "a2"}
MCLinkParent == [a \in MCActors |-> IF a = "a2" THEN "a1" ELSE "none"]
=============================================================================
