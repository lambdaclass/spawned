----------------------------- MODULE CovCohorts -----------------------------
(* Model side of trace coverage for Cohorts; see CovSupervision.tla.      *)
EXTENDS CohortsCoverage

VARIABLE lbl

CovInit == Init /\ lbl = <<"Init", 0, 0, "">> /\ TLCSet(1, {})

L(a, c, r, x) == lbl' = <<a, c, r, x>>

CovNext ==
 \/ \E c \in Children, r \in Epochs, x \in {"true","false"} :
      \/ CRequest(c,r,x) /\ L("CRequest", c, r, x)
      \/ CAssign(c,r,x) /\ L("CAssign", c, r, x)
 \/ \E c \in Children, r \in Epochs, n \in Children : CSelect(c,r,n) /\ L("CSelect", c, r, "")
 \/ \E c \in Children, r \in Epochs :
      \/ CStop(c,r) /\ L("CStop", c, r, "")
      \/ CClean(c,r) /\ L("CClean", c, r, "")
      \/ CResume(c,r) /\ L("CResume", c, r, "")
      \/ CAck(c,r) /\ L("CAck", c, r, "")
      \/ CRetire(c,r) /\ L("CRetire", c, r, "")
 \/ \E c \in Children, r \in Epochs, n \in 1..MaxGen :
      \/ CBegin(c,r,n) /\ L("CBegin", c, r, "")
      \/ CReady(c,r,n) /\ L("CReady", c, r, "")
 \/ \E c \in Children, n \in 1..MaxGen : CJoined(c,n) /\ L("CJoined", c, 0, "")
 \/ \E r \in Epochs, x \in {"complete","superseded"} : CEnd(r,x) /\ L("CEnd", 0, r, x)
 \/ CInitialReady /\ L("CInitialReady", 0, 0, "")
 \/ CClose /\ L("CClose", 0, 0, "")
 \/ CDone /\ L("CDone", 0, 0, "")

CovView == vars

Emit ==
    \A k \in Keys(lbl'[1], lbl'[2], lbl'[3], lbl'[4]) :
        k \in TLCGet(1) \/ (PrintT(k) /\ TLCSet(1, TLCGet(1) \cup {k}))
=============================================================================
