---------------------------- MODULE MCStartScope ----------------------------
EXTENDS Start
\* Per-PR instance for S8 (linked children owned by the parent's scope):
\* a managed parent, two linked children in its group or unmanaged, which
\* may trap or unlink; the scope deadline and the generation end. Safety and
\* OwnedScopeFinishes.
=============================================================================
