---------------------------- MODULE MCAdmission ----------------------------
EXTENDS Admission
\* Per-PR instance: a pool of two credits shared with another mailbox that
\* may hold one; three senders (a: waiting request of weight 1 whose
\* handler skips expired requests, b: waiting request of weight 2, c: send
\* that never waits, weight 1), two messages each; three edges, two of
\* them priority, valve 1. Every Admission negative control except
\* OversizeWaits fails here on its named invariant.
MCSenders == {"a", "b", "c"}
MCWeight == [s \in MCSenders |-> IF s = "b" THEN 2 ELSE 1]
MCKind == [s \in MCSenders |-> IF s = "c" THEN "send" ELSE "req"]
MCMode == [s \in MCSenders |-> IF s = "c" THEN "try" ELSE "wait"]
MCSkip == [s \in MCSenders |-> s = "a"]
=============================================================================
