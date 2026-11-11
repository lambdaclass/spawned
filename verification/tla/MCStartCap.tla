---------------------------- MODULE MCStartCap ----------------------------
EXTENDS Start
\* Per-PR instance for capacity: a managed parent and three children
\* spawned into its group of three: slots (P's included) are held until
\* release, a start is refused while they are held, and a retry gets a
\* slot after a release. (P's own slot is checked in MCStartScope.)
=============================================================================
