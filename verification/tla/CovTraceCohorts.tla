--------------------------- MODULE CovTraceCohorts ---------------------------
(* Trace side of trace coverage for Cohorts; see CovTraceSupervision.tla. *)
EXTENDS TraceCohorts, CohortsCoverage

VARIABLE cov

CovTraceInit == TraceInit /\ cov = {}

CovTraceNext ==
    /\ TraceNext
    /\ cov' = cov \cup Keys(Trace[i'].a, Trace[i'].c, Trace[i'].r, Trace[i'].x)

=============================================================================
