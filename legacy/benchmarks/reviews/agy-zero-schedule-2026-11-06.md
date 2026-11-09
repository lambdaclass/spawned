# Zero-sized schedule callback review

The paired control is based on direct inspection of pinned async-task4.7.1:
captured schedules require temporary Waker retention in scheduling paths. A
zero-sized Rust function item can instead recover the weak pool from metadata
while owning the Runnable ticket. The function reads metadata before transferring
the ticket and does not access it afterward. Inner future boxing/panic boundaries
and Actor/Handler/Backend signatures stay unchanged; the private selector defaults
0 and is immutable after initialization.

Inline Agy review returned APPROVE FOR TESTING with no blockers. It checked
function-item size, metadata access before ticket transfer, concurrent completion,
weak-pool/lease ownership and unchanged panic/disposal boundaries. Linux gates
remain pending at this checkpoint. Do not infer a speedup from fewer source
operations or treat reference-count samples as causal CPU attribution.

Raw review: /private/tmp/spawned-zero-schedule-agy.log. Agy referred to Pool::spawn_raw
in its explanation; the supplied function is named Pool::spawn. Its review is
source inspection, not a claim that tests or timing trials ran.

A second inline Agy review approved the crossed WAKE_PRECHECK control for testing,
with no blockers. It checked startup range/default/immutability, the no-sleeper
fast path, unchanged queue publication, both flag combinations in mixed-wake
units and the planned Root/Task/Deque precheck-on integration suites. Qualify its
“bounded1ms” phrasing: this is a requested timer fallback, not a hard wall-time
wake bound or a guarantee against blocked/misbehaving handlers. No speedup or
mixed-load wake-tail improvement was claimed. Raw review:
/private/tmp/spawned-wake-precheck-agy.log.
