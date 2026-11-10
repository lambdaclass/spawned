--------------------------- MODULE CovTraceNested ---------------------------
(* Trace side of trace coverage for NestedSupervision; see                *)
(* CovTraceSupervision.tla.                                               *)
EXTENDS TraceNested, NestedCoverage

VARIABLE cov

CovTraceInit == TraceInit /\ cov = {}

CovTraceNext ==
    /\ TraceNext
    /\ cov' = cov \cup Keys(Trace[i'].a, Trace[i'].node, Trace[i'].slot, Trace[i'].gen)

=============================================================================
