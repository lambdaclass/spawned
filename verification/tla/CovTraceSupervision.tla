------------------------- MODULE CovTraceSupervision -------------------------
(* Trace side of trace coverage: TraceSupervision plus a history variable *)
(* cov collecting the SupervisionCoverage keys of every replayed step. The *)
(* accepted behavior TLC reports (TraceIncomplete violated) ends in a      *)
(* state whose cov is the trace's coverage.                               *)
(* A stutter (i unchanged) is an unlogged WindowExpire.                    *)
EXTENDS TraceSupervision, SupervisionCoverage

VARIABLE cov

CovTraceInit == TraceInit /\ cov = {}

CovTraceNext ==
    /\ TraceNext
    /\ cov' = cov \cup (IF i' = i THEN Keys("WindowExpire", 0)
                        ELSE Keys(Trace[i'].a, Trace[i'].c))

=============================================================================
