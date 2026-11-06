# Agy review of completed partial results

Agy reviewed the completed within-source V6 medians, V3 clock controls and the
planned validation/overload/resource scope. Verdict: FOR INTERPRETATION, with
reporting guards. This was a review of supplied tables/methodology, not raw-data
reproduction or a code-safety claim. Raw transcript:
[archived transcript](../data/evidence/spawned-partial-results-agy.log).

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

## Completed results and pinned-profile review

Final supplied findings/protocol/profile documentation received **Approved — no
blockers** for interpretation. The review inspected actual tables and stated
limits; it did not run benchmarks, recompute raw CSVs or certify production
correctness. Its four-core example accidentally says Tokio8; the actual cell is
Tokio4. The report and raw metadata retain the correct worker count.

V9's measured source a8471f7 passed 21 RT unit tests, selected pinned parked and
spinning actor suites, workspace all-feature tests, release build and all-target/
all-feature Clippy. Post-measurement20260cd independently passed the same gates
and exact-head GitHub technical checks before PR188 merged. Agy approved the final
supplied code. It correctly identifies removal of repeated *optional* config
parsing; the existing worker-count environment lookup remains. The review's
statement that all dependency/helper caveats are in RT comments is broader than
the source: detailed limitations are recorded in the PR and profile guide.
Blocking helpers can inherit a shard's single-core affinity and outlive shutdown.
The intentional Linux taskset dependency remains explicit.

The collector validated all45 planned blocks and5,744 successful process rows,
with source/binary/controller metadata and required result conservation. All17
five-minute resource configurations completed. Renderer grouping preserves every
workload/experiment setting except artifact paths; smokes, profiles and source
versions remain separate. Visual QA caught a V9 axis bound set before later series,
which clipped high curves. Bounds now follow all series and a generic plotted-data
bound check rejects clipping; final graphs are regenerated and inspected.

Raw final-results and pinned-code/controller transcripts are retained under
[data/evidence](../data/evidence/), alongside the earlier scoped reviews. A
review approval is not independent raw-data reproduction or a universal safety
claim. The support policy, Tokio default and owned/addressed message model remain
unchanged.
