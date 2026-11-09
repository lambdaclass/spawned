-------------------------- MODULE TraceCohorts --------------------------
EXTENDS Cohorts
CONSTANT Trace
VARIABLE i
TraceInit == Init /\ i = 0
TraceAction(e) ==
 CASE e.a = "CRequest" -> CRequest(e.c,e.r,e.x)
 [] e.a = "CSelect" -> CSelect(e.c,e.r,e.n)
 [] e.a = "CAssign" -> CAssign(e.c,e.r,e.x)
 [] e.a = "CStop" -> CStop(e.c,e.r)
 [] e.a = "CClean" -> CClean(e.c,e.r)
 [] e.a = "CResume" -> CResume(e.c,e.r)
 [] e.a = "CBegin" -> CBegin(e.c,e.r,e.n)
 [] e.a = "CReady" -> CReady(e.c,e.r,e.n)
 [] e.a = "CJoined" -> CJoined(e.c,e.n)
 [] e.a = "CRetire" -> CRetire(e.c,e.r)
 [] e.a = "CAck" -> CAck(e.c,e.r)
 [] e.a = "CEnd" -> CEnd(e.r,e.x)
 [] e.a = "CInitialReady" -> CInitialReady
 [] e.a = "CClose" -> CClose
 [] e.a = "CDone" -> CDone
 [] OTHER -> FALSE
TraceNext == /\ i < Len(Trace)
             /\ TraceAction(Trace[i+1]) /\ i' = i+1
TraceIncomplete == i < Len(Trace)
=============================================================================
