# Zero-sized schedule callback review

The paired control is based on direct inspection of pinned async-task4.7.1:
captured schedules require temporary Waker retention in scheduling paths. A
zero-sized Rust function item can instead recover the weak pool from metadata
while owning the Runnable ticket. The function reads metadata before transferring
the ticket and does not access it afterward. Inner future boxing/panic boundaries
and Actor/Handler/Backend signatures stay unchanged; the private selector defaults
0 and is immutable after initialization.

Inline Agy review and Linux gates are pending at this checkpoint. Record actual
verdict/source/results before scoring; do not infer a speedup from fewer source
operations or treat reference-count samples as causal CPU attribution.
