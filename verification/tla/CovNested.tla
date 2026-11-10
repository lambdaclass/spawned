----------------------------- MODULE CovNested -----------------------------
(* Model side of trace coverage for NestedSupervision; see                *)
(* CovSupervision.tla.                                                    *)
EXTENDS NestedCoverage

VARIABLE lbl

CovInit == Init /\ lbl = <<"Init", 1, 0, 0>> /\ TLCSet(1, {})

L(a, n, c, g) == lbl' = <<a, n, c, g>>

CovNext ==
 \/ \E n \in Nodes, a \in 0..Slots : TCreate(n,a) /\ L("TCreate", n, 0, 0)
 \/ \E n \in Nodes, c \in Children, g \in 1..MaxGen :
      \/ TBegin(n,c,g) /\ L("TBegin", n, c, g)
      \/ TJoined(n,c,g) /\ L("TJoined", n, c, g)
 \/ \E n,q \in Nodes, c \in Children, g \in 1..MaxGen, a \in 0..Slots :
      TAttach(n,c,g,q,a) /\ L("TAttach", n, c, g)
 \/ \E n \in Nodes :
      \/ TClose(n) /\ L("TClose", n, 0, 0)
      \/ TCancel(n) /\ L("TCancel", n, 0, 0)
      \/ TPropagate(n,TRUE) /\ L("TPropagate", n, 0, 0)
      \/ TDone(n) /\ L("TDone", n, 0, 0)
      \/ Deliver(n) /\ L("Deliver", n, 0, 0)

CovView == vars

Emit ==
    \A k \in Keys(lbl'[1], lbl'[2], lbl'[3], lbl'[4]) :
        k \in TLCGet(1) \/ (PrintT(k) /\ TLCSet(1, TLCGet(1) \cup {k}))
=============================================================================
