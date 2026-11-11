---------------------------- MODULE MCStartDeep ----------------------------
EXTENDS Start
\* Weekly safety instance: MCStart plus the init timer and an unmanaged
\* parent, so the timer races two named children's readiness, failure,
\* abandonment and publication. (Every variant with every option at two
\* children is beyond TLC; MCStartContinue, MCStartTimeout and MCStartScope
\* cover those dimensions one child at a time.)
=============================================================================
