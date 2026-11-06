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
