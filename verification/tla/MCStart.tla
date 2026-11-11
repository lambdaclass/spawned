---------------------------- MODULE MCStart ----------------------------
EXTENDS Start
\* Per-PR safety instance (design docs/design/unified-start.md, section 15):
\* two children racing for one name in a group with room for both and the
\* parent, every waiting variant with its default placement, a managed or
\* unmanaged parent that may exit, group closure, init timeouts, abandoned
\* waits, failure and retry. Most safety controls run here.
=============================================================================
