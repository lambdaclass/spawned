-------------------------- MODULE MCAdmissionLive --------------------------
EXTENDS Admission
\* Per-PR liveness instance: a shared pool of two credits, two waiting
\* requests (weights 1 and 2), one message each, one priority and one
\* ordinary edge. WaiterResolves fails here with LazyHandoff.
MCSenders == {"a", "b"}
MCWeight == [s \in MCSenders |-> IF s = "b" THEN 2 ELSE 1]
MCKind == [s \in MCSenders |-> "req"]
MCMode == [s \in MCSenders |-> "wait"]
MCSkip == [s \in MCSenders |-> s = "a"]
=============================================================================
