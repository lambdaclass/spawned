-------------------------- MODULE NestedCoverage --------------------------
(* Coverage projection of NestedSupervision.tla (trace_coverage.py); see  *)
(* SupervisionCoverage.tla for the scheme. Per node: root or child, with  *)
(* or without attached subtrees, its stage (created, started, closed,     *)
(* cancelled, done) with propagated/live flags, and its parent's stage.   *)
(* Deliver is a fairness device, not a recorded event: its keys are kept  *)
(* apart by trace_coverage.py ("A|Deliver", "T|Deliver|...").             *)
EXTENDS NestedSupervision, Sequences

Stage(n) == IF n \in s.done THEN "done"
            ELSE IF n \in s.cancelled THEN "cancelled"
            ELSE IF n \in s.closed THEN "closed"
            ELSE IF n \in s.started THEN "started" ELSE "new"

Live(n) == \E c \in Children : s.live[n][c] # {}

Kind(n) == (IF s.parent[n] = 0 THEN "root" ELSE "child")
           \o (IF Descendants(n) # {} THEN "+sub" ELSE "")

NodeView(n) ==
    Kind(n) \o "|" \o Stage(n)
            \o (IF n \in s.propagated THEN "+prop" ELSE "")
            \o (IF Live(n) THEN "+live" ELSE "")
            \o "|" \o (IF s.parent[n] = 0 THEN "-" ELSE "parent:" \o Stage(s.parent[n]))

Act(a, n, c, g) ==
    CASE a = "TBegin" -> a \o (IF g = 1 THEN "/first" ELSE "/restart")
      [] a = "TJoined" -> a \o (IF Owned(n, c, g) # {} THEN "/owner" ELSE "/leaf")
      [] a = "TCreate" -> a \o (IF s.parent[n] = 0 THEN "/root" ELSE "/child")
      [] OTHER -> a

StateKeys == { "S|" \o NodeView(n) : n \in s.nodes }

StepKeys(a, n, c, g) ==
    { "A|" \o Act(a, n, c, g), "T|" \o Act(a, n, c, g) \o "|" \o NodeView(n) }

Keys(a, n, c, g) == StepKeys(a, n, c, g) \cup StateKeys \cup StateKeys'
=============================================================================
