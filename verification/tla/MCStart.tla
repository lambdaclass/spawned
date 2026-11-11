---------------------------- MODULE MCStart ----------------------------
EXTENDS Start
\* Per-PR safety instance (design docs/design/unified-start.md, section 15):
\* two children racing for one name in a group with room for both and the
\* parent, start_link and start_monitor with their default placement, a
\* managed parent that may exit, group closure, abandoned
\* waits, failure and retry (the init timer is in MCStartTimeout). Most safety controls run here.
=============================================================================
