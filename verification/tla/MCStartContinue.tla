---------------------------- MODULE MCStartContinue ----------------------------
EXTENDS Start
\* Per-PR instance for the continuation and trapping children: one child
\* of every variant with every explicit placement, two messages from others,
\* a child that may trap exits (a held parent exit is then queued behind the
\* continuation, E3). ContinueFirst and its control run here.
=============================================================================
