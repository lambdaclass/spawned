---------------------------- MODULE MCStartLive ----------------------------
EXTENDS Start
\* Per-PR liveness instance: a parent exit, also one held during the
\* child's started(), reaches every linked child.
=============================================================================
