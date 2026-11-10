-------------------------- MODULE MCSupervisionInd --------------------------
(* Apalache instance for SupervisionInd (no .cfg: check.sh skips it).      *)
(* N and MaxRestarts are fixed; Mode is any table of restart modes, so one *)
(* run covers every Permanent/Transient/Temporary assignment. apalache.sh  *)
(* --size N R generates the same module with other constants. MaxGen only *)
(* bounds TLC's state constraint and is unused here.                       *)
EXTENDS SupervisionInd

\* Apalache replaces constant C by OVERRIDE_C (ranges like 1..N must be
\* constant); ConstInit then picks Mode nondeterministically.
OVERRIDE_N == 3
OVERRIDE_MaxRestarts == 3
OVERRIDE_MaxGen == 1

ConstInit == Mode \in [Children -> Modes]
=============================================================================
