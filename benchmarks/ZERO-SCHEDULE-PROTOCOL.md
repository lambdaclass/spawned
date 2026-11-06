# Zero-sized schedule callback control

Pinned async-task4.7.1 inspects the schedule closure's size. With captured state,
its scheduling path retains a temporary Waker to keep that state alive. The
current callback captures Weak<Pool>, even though the same weak pool pointer is
already in Runnable metadata. Test a zero-sized Rust function item that reads
metadata while owning the ticket, upgrades the pool and then transfers the
ticket to the queue. No metadata is accessed after ownership transfer.

This is a private ZERO_SCHEDULE0/1 control, default0, immutable after pool startup.
It does not change actor methods, message ownership, unbounded mailbox semantics,
worker counts, quanta or the Tokio default. Runnable and Deque both support the
control on one new common source. It remains Tokio-dependent. A function item is
zero-sized; coercing it to a function pointer would invalidate that mechanism.

A targeted unit checks callback size and both modes on both policies, with
external one-shot producers, timer wakes, duplicate internal self wakes, exact
outcomes and joined teardown. Run the selected full actor suites in zero mode,
including real caller-runtime independence and terminal/drop-panic integration
cases, plus all existing gates before scoring. Paired graph cases keep quantum8,
1920actors/256logical producers, async callers and five fresh process repetitions;
Tokio and custom Root are controls. No causal speedup is claimed before results.
This small follow-up is subject to the overnight deadline and may remain partial.

[async-task pinned implementation](https://github.com/smol-rs/async-task/blob/v4.7.1/src/raw.rs)

A second paired control crosses WAKE_PRECHECK0/1: skip the shared rotating-target
fetch_add when a read-only scan sees no sleeping peer. V3 profiles had unpark_peer
in226/1523 captured Root worker stacks and313/2007 Steal stacks for chain-crossN4.
Those are inclusive user-space samples, not causal CPU percentages. The precheck
is a wake hint; register/recheck and requested1ms fallback remain. New sleepers
can race the snapshot, so mixed-load wake tails still need follow-up. Both flags
default0; no hard wake latency or preemption bound is introduced.

The final graph matrix has240processes: Tokio4/5,Root withprecheck0/1, and
Runnable/Deque withbothflagcombinations, four graphcases andfive repetitions.
Sixteen profiles are separate. The new unit exercises bothflags inbothpolicies;
selected Root/Runnable/Deque suites run withprecheck1 before scoring.
