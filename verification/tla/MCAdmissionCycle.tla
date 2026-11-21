------------------------- MODULE MCAdmissionCycle -------------------------
EXTENDS AdmissionCycle
\* Per-PR instance: mailboxes of one slot, two outside messages each,
\* handlers that wait under a deadline. NoDeadline breaks TurnsEnd.
=============================================================================
