---------------------------- MODULE MCStartScope ----------------------------
EXTENDS Start
\* Per-PR instance for S8 (linked children owned by the parent's scope):
\* a managed parent and one linked child, in its group (owned) or unmanaged
\* (not owned), which may trap or unlink; the scope deadline and the generation end. Safety and
\* OwnedScopeFinishes.
=============================================================================
