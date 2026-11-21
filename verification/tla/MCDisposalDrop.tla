--------------------------- MODULE MCDisposalDrop ---------------------------
EXTENDS Disposal
\* The first payload's destructor never returns.
MCBlocking == {DropItem(1)}
=============================================================================
