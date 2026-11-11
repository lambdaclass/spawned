---------------------------- MODULE MCStartLive ----------------------------
EXTENDS Start
\* Per-PR liveness instance: one child of every waiting variant of a
\* managed parent; parent exits (also held during started()) reach linked
\* children, and every waiting start is answered.
=============================================================================
