---------------------------- MODULE MCStartDeep ----------------------------
EXTENDS Start
\* Weekly safety instance (30M states, about 5 minutes on 4 workers): two
\* named children started with start_link or start_monitor by a managed
\* parent that may exit, abandonment, failure, retry: the per-PR MCStart and
\* MCStartNames combined.
=============================================================================
