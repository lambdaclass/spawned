---------------------------- MODULE MCStartScope ----------------------------
EXTENDS Start
\* Per-PR instance for S8: a managed parent and one linked child, in its
\* group, in another group of its system (both owned) or unmanaged (not
\* owned), which may trap or unlink; P's scope may close; the polite
\* Shutdown at P's publication, the scope deadline, the generation end.
\* Safety and OwnedScopeFinishes.
=============================================================================
