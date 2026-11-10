-------------------------- MODULE MCCoordinatorInd --------------------------
(* Apalache instance for CoordinatorInd (no .cfg: check.sh skips it).      *)
(* apalache.sh --coordinator-size K generates the same module with another *)
(* TargetCount, and sets OVERRIDE_Fault for the negative controls.         *)
EXTENDS CoordinatorInd

OVERRIDE_TargetCount == 8
OVERRIDE_Fault == "None"

ConstInit == TRUE
=============================================================================
