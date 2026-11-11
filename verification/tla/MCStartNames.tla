---------------------------- MODULE MCStartNames ----------------------------
EXTENDS Start
\* Per-PR instance for names and the commit: two children started with
\* start, racing for one name in a group with room for both and the parent;
\* lookups, refusals, release, retry.
=============================================================================
