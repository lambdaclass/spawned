# Amortized actor driver experiment

Profile-guided hypothesis: per-worker ready queues removed most captured
contended-mutex leaves, but native-steal still trails Tokio and spends samples in
atomic/reference-count operations and runtime Handle cloning. Test whether
amortizing runtime entry helps; stack presence alone is not a causal measurement.

`native-root` drives custom actor jobs inside one current-thread Tokio root future
per native worker. A persistent shared-I/O Handle scope belongs only to that
nonmigrating root future. The worker-local runtime creates no per-actor Tokio
tasks and has no I/O driver; helpers/TCP/timers bind the same shared I/O runtime
as native-steal. Eight job polls share the outer cooperative budget, then the
root yields to reset the budget and flush deferred custom-job wakes. Notify
registration and a second queue check precede idle waiting; the shared timer
drives a requested 1 ms fallback. It is not a hard latency or preemption bound.

Native N+R OS worker counts stay the same. There are N fixed current-thread driver
objects, plus the shared I/O runtime and its idle owner thread. This remains a
Tokio-dependent implementation of custom actor scheduling, not a Tokio-free
runtime. Runtime task IDs are absent; current-thread driver restrictions apply,
including unsupported block_in_place. Raw blocking handlers consume an actor
worker; use the appropriate backend or helper. Ordinary Actor/Handler/protocol
signatures, message types, addressed unbounded delivery and lifecycle remain.

Root-runtime construction uses a startup handshake before accepting actors;
partial startup failure closes already-created workers. Pools remain alive after
zero actor leases and require explicit terminal shutdown after actors/helpers
finish. Existing seven unit tests cover Root alongside the four previous modes,
including cooperative hot receive, duplicate self wakes, helper/timer/TCP,
blocked-worker/shared-reactor independence, panic/retained wakers and helper-thread
retirement. Full native-root actor suites and Clippy passed on fede before timing.
The readiness gate is unchanged from the four-model publication-corrected version.
Agy approved the supplied code for testing; its block_in_place statement about
other native variants was not independently verified and is not a general claim.

`fede_root.py` tests 1,920 actors, 256 logical producers, eight handlers/root,
1,048,576 root requests, both graph shapes/maps, both caller implementations and
five fresh repetitions. Resource controls include native N3+R1 versus Tokio4,
native N4+R1 versus Tokio4 and Tokio5, all on four physical actor cores. Tokio5 is
an oversubscription control, not five cores. Only static shards enforce same/cross
placement. Thirty-two small smoke processes precede 320 long graph processes;
14 profile cases are separate. Source, binaries, schedule, CPU masks and controller
hashes are recorded. Never pool these with the earlier source versions, and keep
Tokio default regardless of a single attractive graph result.
