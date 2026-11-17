----------------------------- MODULE MCDisposal -----------------------------
EXTENDS Disposal
\* The first caller's waker never returns.
MCBlocking == {WakeItem(1)}
=============================================================================
