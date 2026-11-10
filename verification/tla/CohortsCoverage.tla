-------------------------- MODULE CohortsCoverage --------------------------
(* Coverage projection of Cohorts.tla (trace_coverage.py); see            *)
(* SupervisionCoverage.tla for the scheme. Per child: strategy, mode,      *)
(* round phase (gate, initial readiness committed), position relative to  *)
(* the failing child that selected the round, and how far the child got   *)
(* in the round (affected, stopped, clean, resumed, acknowledged) plus    *)
(* pending/terminal/live/published flags. Keys(a, c, r, x) takes the      *)
(* trace event's action, child, round and parameter.                      *)
EXTENDS Cohorts

ChildActions == {"CRequest", "CSelect", "CAssign", "CStop", "CClean", "CResume",
                 "CBegin", "CReady", "CJoined", "CRetire", "CAck"}

Ph == s.phase \o (IF s.open THEN "/open" ELSE "/closed")
              \o (IF s.committed THEN "" ELSE "/initial")

Pos(c) == IF s.epoch = 0 THEN "none"
          ELSE IF c < s.origin THEN "before"
          ELSE IF c = s.origin THEN "origin" ELSE "after"

Stage(c) == IF c \in s.ack THEN "ack"
            ELSE IF c \in s.resumed THEN "resumed"
            ELSE IF c \in s.clean THEN "clean"
            ELSE IF c \in s.stopped THEN "stopped"
            ELSE IF c \in s.affected THEN "affected" ELSE "-"

Flags(c) == (IF c \in s.pending THEN "P" ELSE "")
            \o (IF c \in s.terminal THEN "X" ELSE "")
            \o (IF s.live[c] # {} THEN "L" ELSE "")
            \o (IF s.published[c] # 0 THEN "U" ELSE "")

Act(a, c, r, x) ==
    CASE a \in {"CRequest", "CAssign", "CEnd"} -> a \o "/" \o x
      [] a \in {"CClean", "CAck"} -> a \o (IF r < s.epoch THEN "/stale" ELSE "/current")
      [] a \in {"CBegin", "CReady"} -> a \o (IF r = 0 THEN "/initial" ELSE "/round")
      [] a = "CRetire" -> a \o (IF s.open THEN "/open" ELSE "/closed")
      [] OTHER -> a

StateKeys ==
    { "S|" \o Strategy \o "|" \o Mode[c] \o "|" \o Ph \o "|" \o Pos(c) \o "|"
           \o Stage(c) \o "|" \o Flags(c) : c \in Children }

StepKeys(a, c, r, x) ==
    IF a \in ChildActions
    THEN { "A|" \o Strategy \o "|" \o Act(a, c, r, x),
           "T|" \o Act(a, c, r, x) \o "|" \o Strategy \o "|" \o Mode[c] \o "|" \o Ph
                \o "|" \o Pos(c) \o "|" \o Stage(c) }
    ELSE { "A|" \o Strategy \o "|" \o Act(a, 0, r, x) } \cup
         { "T|" \o Act(a, 0, r, x) \o "|" \o Strategy \o "|" \o Mode[d] \o "@" \o Stage(d)
                \o "|" \o Ph : d \in Children }

Keys(a, c, r, x) == StepKeys(a, c, r, x) \cup StateKeys \cup StateKeys'
=============================================================================
