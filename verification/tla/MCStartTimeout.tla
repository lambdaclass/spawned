---------------------------- MODULE MCStartTimeout ----------------------------
EXTENDS Start
\* Per-PR instance for the startup commitment's races: one child of every
\* waiting variant of a managed or unmanaged parent; the init timer, an
\* outside kill (also after started() resolved, before D) and an
\* administrative stop during started(), racing readiness, failure,
\* abandonment, the cleanup deadline and publication; group closure.
=============================================================================
