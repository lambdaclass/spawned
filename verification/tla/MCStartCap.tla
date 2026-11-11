---------------------------- MODULE MCStartCap ----------------------------
EXTENDS Start
\* Per-PR instance for group capacity: a managed parent and three linked
\* children in a group of three, so a start is refused while the slots are
\* held (until publication), a retry after a failure gets the slot, and two
\* children run in the group beside the parent. AdmitClosed and IgnoreCap
\* run here.
=============================================================================
