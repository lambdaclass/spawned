---------------------------- MODULE MCStart ----------------------------
EXTENDS Start
\* Per-PR safety instance (design docs/design/unified-start.md, section 15):
\* one child started with start_link or start_monitor by a managed parent
\* that may trap and exit (D, then publication): every init outcome, the
\* disposition of every notice, abandonment, the cleanup deadline. Most
\* notice controls run here; names in MCStartNames, closure in MCStartCap and
\* MCStartTimeout, the timer in MCStartTimeout.
=============================================================================
