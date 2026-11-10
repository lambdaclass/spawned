----------------------------- MODULE MCNestedInd -----------------------------
(* Apalache instance for NestedSupervisionInd (no .cfg: check.sh skips    *)
(* it). apalache.sh sets OVERRIDE_Fault for the negative controls.         *)
EXTENDS NestedSupervisionInd

OVERRIDE_NodeCount == 4
OVERRIDE_Slots == 2
OVERRIDE_MaxGen == 3
OVERRIDE_Fault == "None"

ConstInit == TRUE
=============================================================================
