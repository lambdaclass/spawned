---------------------------- MODULE MCStartTimeout ----------------------------
EXTENDS Start
\* Per-PR instance for the startup cell's races: one child of every waiting
\* variant of a managed or unmanaged parent; the init timer racing readiness,
\* failure, abandonment and publication; group closure.
=============================================================================
