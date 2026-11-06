# Runnable task primitive experiment

Profiles of split native queues still contain atomic/reference-count operations.
Test a second implementation of the actor task primitive using optional
async-task4.7.1, keeping the same per-worker mutex/deque queues, root driver,
shared Tokio I/O runtime, cooperative message turn and public actor API. This is
private native-runnable, not removal of Tokio or a supported runtime replacement.

The ready queue owns a unique Runnable ticket. Its integrated state machine owns
poll/wake exclusion, so this path does not need our custom Gate, Arc<Job> clone
per poll, a separately allocated Waker, or the mutex protecting each future.
Metadata keeps a weak pool pointer and preferred-worker atomic. Before each run,
worker TLS routes internal wakes locally; external wakes prefer the last worker.
Runnable.run consumes the ticket; wake schedules a new ticket. Detach Task before
scheduling so dropping a task handle cannot cancel the actor. Framework completion,
monitor, stop and helper/timer behavior stay inside the original actor future.

Source inspection found an important compatibility detail missed by the first
Agy review: async-task aborts when its stored future destructor unwinds. An outer
catch_unwind around Runnable.run cannot recover an abort. A safe wrapper therefore
keeps the actor future boxed and catches poll and disposal panics separately,
matching the custom native-job boundaries. This retains the inner allocation;
do not claim all actor state lives in one allocation. A targeted regression covers
completion-then-destructor-panic, checks subsequent jobs run and requires joined
shutdown. A direct wrapper regression checks separate poll/disposal unwinds. Rust double panic within an
already-unwinding user destructor remains normal Rust behavior.

Extend existing executor tests to Runnable: duplicate wakes/no concurrent polls,
real TCP/timers/detached helpers after zero actors, blocked-worker reactor progress,
worker self-shutdown rejection, ordinary job panic, retained stale wakers,
cooperative hot-MPSC progress, retired blocking-helper IDs and concurrent idle
shutdown. These tests and selected full actor suites must run on fede in a
non-scoring analysis gap before any measurement. Record actual results separately.

All policies in the new common source use the same Work enum ready-queue element,
including the existing custom jobs. This changes layout/code generation even for
controls; comparisons must be within the new source, with source/binary hashes.
A difference from V3 is not attributable solely to the Runnable primitive.

The private ROOT_QUANTUM selector defaults to8 polls as in V3, validates1..256
at startup, and is immutable after first successful initialization. Root and
Runnable compare1/8/32/128 polls before yielding the outer driver. This is a
scheduling tradeoff, not a running-handler preemption bound. Single-worker hot
MPSC progress is checked at all four values. Every timing records the selector
in experiment_json, and unlike quantum values are never pooled.

The first combined poll+destructor-panic integration probe aborted on the existing
custom Root control before reaching Runnable: the async await state machine
dropped the panicking future during its poll unwind, creating a normal Rust
double panic. Preserve that failed log as a test-design limitation, not a new
Runnable regression. The final executor probe tests a single destructor panic;
ordinary poll panic is covered separately, and the direct wrapper test checks
its own two unwind boundaries. No backend promises recovery from a double panic.
