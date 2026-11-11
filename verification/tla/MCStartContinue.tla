---------------------------- MODULE MCStartContinue ----------------------------
EXTENDS Start
\* Per-PR instance for the continuation (Init::continue_with): one child of
\* every variant, two messages from others racing started(), the parent's
\* exit and a startup timeout. ContinueFirst and its control run here.
=============================================================================
