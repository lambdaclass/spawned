---------------------------- MODULE MCStartNames ----------------------------
EXTENDS Start
\* Per-PR instance for names and the commit: two children started with
\* start into P's group (room for both and P), racing for one name;
\* lookups, refusals (also by group closure), release, retry.
=============================================================================
