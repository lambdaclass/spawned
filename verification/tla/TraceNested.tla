--------------------------- MODULE TraceNested ---------------------------
EXTENDS NestedSupervision, Sequences
CONSTANT Trace
VARIABLE i
TraceInit == Init /\ i = 0
TraceAction(e) ==
 CASE e.a = "TCreate" -> TCreate(e.node,e.arity)
 [] e.a = "TBegin" -> TBegin(e.node,e.slot,e.gen)
 [] e.a = "TAttach" -> TAttach(e.node,e.slot,e.gen,e.peer,e.arity)
 [] e.a = "TJoined" -> TJoined(e.node,e.slot,e.gen)
 [] e.a = "TClose" -> TClose(e.node)
 [] e.a = "TCancel" -> TCancel(e.node)
 [] e.a = "TPropagate" -> TPropagate(e.node,e.cancelled)
 [] e.a = "TDone" -> TDone(e.node)
 [] OTHER -> FALSE
TraceNext == /\ i < Len(Trace) /\ TraceAction(Trace[i+1]) /\ i' = i+1
TraceIncomplete == i < Len(Trace)
=============================================================================
