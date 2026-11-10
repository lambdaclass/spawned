---------------------------- MODULE MCCohortsInd ----------------------------
(* Apalache instance for CohortsInd (no .cfg: check.sh skips it). Strategy *)
(* and the Mode table are chosen by ConstInit, so one run covers OneForAll *)
(* and RestForOne with every restart-mode assignment. apalache.sh sets     *)
(* OVERRIDE_Fault for the negative controls.                               *)
EXTENDS CohortsInd

OVERRIDE_N == 5
OVERRIDE_MaxGen == 4
OVERRIDE_MaxRounds == 4
OVERRIDE_Fault == "None"

ConstInit ==
 /\ Strategy \in {"OneForAll", "RestForOne"}
 /\ Mode \in [Children -> {"Permanent", "Transient", "Temporary"}]
=============================================================================
