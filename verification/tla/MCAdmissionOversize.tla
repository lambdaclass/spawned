------------------------ MODULE MCAdmissionOversize ------------------------
EXTENDS Admission
\* Per-PR instance for weights (rank 18): a private pool of three credits;
\* a waiting send of weight 4 (larger than the pool: TooLarge at once), a
\* waiting request of weight 3 (the whole pool) and a waiting request of
\* weight 1. The OversizeWaits control fails here.
MCSenders == {"a", "b", "big"}
MCWeight == [s \in MCSenders |-> CASE s = "a" -> 1 [] s = "b" -> 3 [] OTHER -> 4]
MCKind == [s \in MCSenders |-> IF s = "big" THEN "send" ELSE "req"]
MCMode == [s \in MCSenders |-> "wait"]
MCSkip == [s \in MCSenders |-> FALSE]
=============================================================================
