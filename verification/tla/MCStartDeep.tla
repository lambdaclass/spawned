---------------------------- MODULE MCStartDeep ----------------------------
EXTENDS Start
\* Weekly safety instance: every explicit placement (a group, unmanaged,
\* the parent's via placement_of) for every start variant, spawn included.
\* Messages from others are checked in MCStartContinue (with them this
\* instance passed 74M distinct states unfinished).
=============================================================================
