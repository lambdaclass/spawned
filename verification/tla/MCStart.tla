---------------------------- MODULE MCStart ----------------------------
EXTENDS Start
\* Per-PR safety instance of the unified start model (design
\* docs/design/unified-start.md): two children racing for one name, every
\* start variant, inherited or explicit unmanaged placement, a managed or
\* unmanaged parent that may exit, a closing group with room for two actors,
\* startup timeouts. Every Start safety control except ContinueQueued
\* (MCStartContinue) fails here on its named invariant. MCStartDeep (weekly)
\* adds every explicit placement, spawn, and messages from others.
=============================================================================
