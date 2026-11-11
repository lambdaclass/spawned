------------------------- MODULE MCIntrospection -------------------------
EXTENDS Introspection
\* Per-PR instance: three messages, one status request, the watchdog and
\* both kinds of stop. Every Introspection negative control fails here on
\* its named invariant (negative_checks.py, INTROSPECTION_CONTROLS).
=============================================================================
