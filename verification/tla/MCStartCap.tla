---------------------------- MODULE MCStartCap ----------------------------
EXTENDS Start
\* Per-PR instance for group capacity: a managed parent and three children
\* spawned into its group of three (capacity does not depend on the start
\* variant, so no edges or waiters): two children run beside the parent, a
\* third is refused while the slots are held (until publication), and gets
\* a slot after a failure is published. AdmitClosed and IgnoreCap run here.
=============================================================================
