-------------------------- MODULE MCDisposalCapped --------------------------
EXTENDS Disposal
\* The first caller's waker never returns, and there is one worker.
MCBlocking == {WakeItem(1)}
=============================================================================
