# Native queue, caller and clock controls

Source revisions and raw campaigns remain separate from the first native study.
The Actor/Handler/Backend APIs and Tokio default stay unchanged. These are private
feature-gated research paths, not supported replacement backends.

## Per-worker queue hypothesis

The first native local policy stores all queues under one mutex. A new
`native-steal` policy gives each worker its own Mutex<VecDeque<Job>>, with bounded
local LIFO and oldest stealing. It retains the tested CAS wake gate and single
future mutex. Workers use std thread park/unpark tokens, register before a final
queue check, and request a 1 ms idle steal interval. That interval is not a hard
OS scheduling deadline. No queue locks are held together. This removes one shared
queue mutex; any performance effect must be measured, not inferred from design.

`fede_controls.py --stage queues` compares all policies on the same new source,
fixed blocking callers, full observation counter, default 50 µs/16-message turns.
Native N=3+R1 and N=4+R1 expose equal-total-runtime-worker and equal-actor-worker
controls against Tokio4; all share the same four physical actor CPUs. No source
versions or actor populations are pooled with earlier results.

## Caller/instrumentation factorial

`--stage callers` crosses two graph caller implementations with observation
counter on/off for each policy. Blocking callers use 256 native producer threads;
async callers use four current-thread runtime threads driving the same 256 logical
producers. IDs, target selection, payload/work, eight-node graph, one outstanding
root per producer, latency recording and final validation are common. Both read
intended and enqueue timestamps. Drivers stay on the same four producer cores.
Counter-off suppresses only the shared observed-root atomic; latency recording,
boxed request/reply transport, synthetic handler work and clock reads remain.
This is not an instrumentation-free or equal-total-thread comparison.

These options are graph-only; offered traffic and its validity threshold stay
unchanged. All groups include caller mode, counter mode, topology/layout, actor
count, workers and source. A driver effect does not by itself explain a scheduler.

## Count-only and time-limited message turns

`SPAWNED_EXPERIMENT_SLICE_US` is private and defaults to 50. Zero disables the
clock in explicit message turns and yields only at the configured message count;
25/50/200 µs retain time checks between completed handlers. Neither preempts a
running handler. Tokio's cooperative budget remains independent. A regression
checks count-only turns never start a clock, yield at the count, and remain
clock-free after yield/parking; the existing stop-during-yield regression remains.
`--stage clock` fixes the async caller/counter-on and compares slices, with
ordinary Tokio as a common control. Interpret mailbox occupancy and graph shape;
empty mailboxes do not establish that a message-turn limit was reached.

## Bounded-memory lifecycle soak

`lifecycle_soak` keeps 1,024 residents idle while repeatedly creating 128 actors,
checking 16 sequential stateful requests each, awaiting helper/timer work,
periodically injecting a handler panic, then stopping/joining and checking exact
monitor IDs/reasons. Monitor maps remove each ID. Each cycle's actor references
and bounded futures are released. Counts validate all starts, stops, drops,
requests and Down notifications. One unmonitored seed initializes the executors;
its counters are included explicitly. A single sampler streams one row/second to
disk with RSS, descriptors, threads and cumulative progress. No per-message sample
history grows with duration. Every process must finish with normal executor
shutdown, or is retained as a failed correctness/resource test.

Default duration is ten minutes; bound processes with an external watchdog and
an overnight stop time. Initialize actor/owned runtime workers in actor CPUs2–5
before moving the caller to producer CPUs and sampler to collector CPUs. The
initial resident/helper warm-up remains in the time series; compare post-warm-up
trends and completed cycles, not raw peak RSS as isolated actor memory. Allocator
high-water behavior and monitor/runtime storage remain part of the process.
This closed-loop soak is correctness/resource evidence, not an open-loop overload
SLO, production leak proof or throughput ranking.

Agy approved the new queue/clock/caller paths and the soak for testing. That does
not claim compilation, server tests, Loom or performance have already passed.
