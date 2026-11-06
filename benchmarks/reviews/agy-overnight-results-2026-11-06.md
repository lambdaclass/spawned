# Agy review of completed partial results

Agy reviewed the completed within-source V6 medians, V3 clock controls and the
planned validation/overload/resource scope. Verdict: FOR INTERPRETATION, with
reporting guards. This was a review of supplied tables/methodology, not raw-data
reproduction or a code-safety claim. Raw transcript:
/private/tmp/spawned-partial-results-agy.log.

The review supports describing observed within-case median improvements for the
wake precheck, smaller schedule-representation changes, and keeping Tokio as the
primary baseline. Native N+R1 and idle-owner costs, oversubscribed Tokio5, source
boundaries, censored deadlines, invalid-generator labels and finite resource
windows must accompany comparisons.

Editorial corrections to broad wording in that review:

- The supplied table was one chain/cross/asynchronous-caller case. “Consistent”
  applies to those displayed configurations, not all graph shapes or workloads.
- Captured inclusive unpark_peer stacks are a hypothesis signal, not CPU time or
  proof that a rotator RMW caused a measured fraction of overhead.
- Comparing count-only with50µs turns changes clock checks and scheduling policy.
  Yield counts were not recorded, so its assertion about “frequent expiration”
  is unsupported. Do not attribute the result specifically to slice expiration.
- Root/Task/Deque differ in task and queue policy; do not state that batching
  alone caused their p99 differences. Median ratios without confidence analysis
  are descriptive; small gains are not proven significant or additive.
- Historical V1/V2 outcomes were retained after later defects were found, rather
  than declaring every timing observation invalid. They are not adoption proof.

Final reports still require actual overload/resource completion, min/max ranges,
visual inspection and a bounded final results review. No default promotion or
Tokio-removal recommendation follows from these partial CPU graphs.

Pipeline review also inspected the supplied collector/renderer/protocol/findings
source text without executing the analysis. It confirmed that grouping preserves
all workload/environment settings and that invalid-generator and censored-tail
labels remain explicit. Defensive guards were added for unique offered-rate
cells, complete/unique caller factorials, one default clock reference and unique
resource labels. These guard future matrix changes rather than fixing observed
cross-source pooling. Its proposed zero-value omission and string-rate concerns
do not occur in this CSV path: CSV values are strings, zero tuples are truthy,
and rate is converted to int before selection. Bar plots already reject duplicate
labels; inactive Root Z/I flags do not vary in the actual matrix.

The first pipeline attempt only announced a file read; the second reviewed source
supplied inline. Likewise, the optimized mixed-load controller's first attempts
were incomplete or denied a read tool; only the third, supplied-code review is
an approval for testing. It checked the full 11-spec Z/W factorial, I0-only
selection, source/flag/worker provenance, strict validity/full-drain behavior and
bounded resource windows. It requires sequential execution after earlier campaigns,
small smokes first, and honest partial-block handling at the deadline. None of
these reviews independently reproduces raw results or certifies production safety.

Final source2cec0a6 received focused Agy approvals for the queue correction,
callback hint fallback/concurrent conservation tests, and non-Send owner guard.
The reviews checked supplied code and did not execute tests. Actual final-source
fede gates then passed20 RT unit tests, both selected four-worker Deque actor
suites (joint Z/W0/1, inherited badly-behaved exclusion), workspace all-feature
tests, three release benchmark builds and all-target/all-feature Clippy. PR187
merged only after those gates and exact-head GitHub technical checks passed.

The five-minute ten-case V3 resource rerun completed at nice10 with continuous
pipe drainage. Raw expected assertions and terminal shutdown succeeded in every
case; resource plots still describe sampled whole-process behavior, not a
production leak/SLO guarantee. Final results interpretation receives its own
bounded review after the remaining source-matched workload blocks finish.
