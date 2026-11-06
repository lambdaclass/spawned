# Scheduler research follow-ups, 6 November 2026

The overnight study targets concrete costs without changing Spawned's model.
Silver/flux's fixed workers and deliberate memory layout motivate experiments;
its Copy-only broadcast rings and overwrite-on-lapping semantics do not fit
ordinary addressed Rust messages and request/reply. Existing Silver research
and the initial profiling report remain the starting evidence.

## Scheduling locality and fewer hot-path operations

Tokio's scheduler design discusses local queues, message-passing locality,
limited stealing, fewer cross-thread synchronizations and less allocation and
reference counting. Its improvements are workload-specific, not a speedup we
can transfer by copying a queue. Our profiles motivate separate tests of split
queues, amortized runtime entry and an integrated task primitive. Keep each
change on a common-source controlled comparison, with correctness gates first.
[Tokio scheduler design](https://tokio.rs/blog/2019-10-scheduler).

CAF exposes separate hooks for external/internal work and rescheduling, and
supports customizable scheduling policy. This suggests measuring external
producer wakes separately from actor-to-actor continuations. Its documentation
also makes the non-preemption limit explicit: blocking actor work can consume
an entire worker. Wake-target selection and blocking isolation are distinct
experiments; neither is solved merely by adding more queue shards.
[CAF scheduler](https://actor-framework.readthedocs.io/en/0.18.4/Scheduler.html).

The Barghi/Karsten study proposes locality-aware actor scheduling on NUMA
machines. It supports investigating graph locality and work placement, but our
fede host reports one NUMA node. We cannot infer a NUMA speedup from these
four-core measurements or attribute placement differences to cache misses
without hardware evidence.
[Work-Stealing, Locality-Aware Actor Scheduling](https://cs.uwaterloo.ca/~mkarsten/papers/ipdps2018-preprint.pdf).

## Integrated task primitive

async-task4.7.1 supplies a unique Runnable ticket, metadata and wake-state
management. Running consumes a ticket; a wake arranges another scheduled ticket.
This gives a way to test whether replacing our custom Gate/future mutex/Arc job
reduces overhead while keeping the same queue/root/I/O design. It does not remove
all atomics or Tokio, and requires careful detach, panic and teardown behavior.
[Runnable API](https://docs.rs/async-task/4.7.1/async_task/struct.Runnable.html),
[Builder metadata](https://docs.rs/async-task/4.7.1/async_task/struct.Builder.html).

Source inspection adds a compatibility requirement: async-task aborts if its
stored future destructor unwinds. An outer catch around run cannot catch that
abort. Our experiment uses a safe boxed-future wrapper with separate poll/drop
unwind boundaries and a targeted regression before any timings. This retains
an inner allocation, so the experiment must not claim one allocation for all
actor state.
[async-task raw task implementation, pinned v4.7.1](https://github.com/smol-rs/async-task/blob/v4.7.1/src/raw.rs).

## Follow-up decisions

1. Complete source-matched root-driver graph, clock, offered/burst and lifecycle
   campaigns before choosing what to retain.
2. Gate and measure the Runnable primitive against custom jobs on a new common
   source. Keep the same message/graph/caller/worker controls.
3. Test more actors/producers and eight physical actor cores if time permits;
   do not pool those results with four-core resource budgets.
4. Measure idle CPU and wake/tail behavior before adding spin or fixed-core
   affinity options. Silver-style spinning consumes dedicated capacity and
   needs a workload where its latency benefit is demonstrated.
5. Treat I/O-engine work separately. Earlier Monoio io_uring/kqueue experiments
   already exist; a native actor scheduler still uses Tokio-compatible timers,
   helpers and TCP here. Replacing that I/O layer requires a compatibility design,
   owned-buffer/cancellation analysis and workload-specific measurements.

No result by itself justifies changing public Actor/Handler/protocol signatures,
message ownership, delivery semantics or the Tokio default.

## What the overnight prototypes tested

| Research idea | Experiment | Status / next evidence |
|---|---|---|
| Fewer shared ready-queue locks | Native-steal split mutex queues | Measured; reduced captured contended-mutex leaves did not make it the Tokio replacement |
| Amortize Tokio context entry | Native-root, one outer future per worker | Measured at several poll quanta; keeps Tokio I/O and cooperative-budget interactions |
| Reuse a task-state primitive | Native-runnable, async-task tickets | Correctness-gated and measured; destructor wrapper required by library abort behavior |
| Owner-local queues and batch injection | Native-deque, safe Crossbeam worker/stealer/injector | Measured within a common source; queue primitive and scheduling policy change together |
| Avoid captured scheduling closure overhead | Zero-sized metadata callback, Z1 | Measured in a factorial; small case-specific gains require workload context |
| Avoid idle-target selection when no peer looks asleep | Wake precheck, W1 | Larger graph gains in the displayed chain/cross case; mixed-load/resource follow-up fixed before those outcomes |
| Remove the inner boxed future | Generic pinned inline future, I1 | Four/eight-core graph pairs showed little throughput benefit; retain as research, including stale-Waker storage tradeoff |

Crossbeam documents separate owner queues, stealers and a shared external
injector, plus Retry outcomes and batch stealing. These APIs inspired V5; using
safe queue APIs does not prove a complete scheduler correct.
[Crossbeam deque API](https://docs.rs/crossbeam/latest/crossbeam/deque/).

The CAF documentation distinguishes external work from internal actor events,
reschedules actors after a message limit, and notes that workers cannot interrupt
running handlers. This supports treating external wake distribution, locality,
cooperative turns and blocking isolation as separate design questions.
[CAF scheduling policies](https://actor-framework.readthedocs.io/en/stable/core/Scheduler.html).

Tokio's scheduler article already addresses local queues, message-passing
locality, stealing, synchronization, allocations and reference counting. A custom
scheduler must earn its extra correctness/maintenance obligations against those
existing mechanisms. Our finite synthetic studies do not establish that removing
Tokio is beneficial.

## I/O engines remain a separate experiment

Monoio documents a thread-per-core model and an owned-buffer I/O abstraction.
Those are meaningful design choices, not an interchangeable actor run queue.
A future io_uring/epoll/kqueue backend needs a written compatibility plan for
timers, helpers, socket/buffer ownership, cancellation and user Tokio calls.
Keeping Actor/Handler signatures is necessary but does not by itself preserve
these runtime behaviors. Earlier engine-only experiments remain separate from
the current full actor graph measurements.
[Monoio model](https://docs.rs/monoio/latest/monoio/),
[owned-buffer reads](https://docs.rs/monoio/latest/monoio/io/trait.AsyncReadRent.html).

The native root driver multiplexes actors over a fixed worker population:
16,128 actors on eight actor workers is not16,128 OS threads. It still relies on
Tokio for channels, I/O, timer fallback and root-driver cooperative machinery.
Root/Runnable actor futures are not Tokio tasks; task identity and blocking
compatibility limits must remain explicit. A runtime rewrite is justified only
by a representative application need and end-to-end evidence.

## A bounded next scheduler question

Review suggested a sparse-work experiment: after some root polls, an empty
ready-queue scan could yield the outer driver once before constructing its idle
Notify/timer wait. This might flush deferred Tokio wakes sooner and avoid
temporary timer registration. It is an unmeasured hypothesis, not another
implemented backend or a correctness fix. Compare idle CPU, external wake tails,
timer/helper/TCP progress and graph throughput on one common source; do not infer
the benefit from stack presence or larger-quantum medians.

Prefer this type of narrow, falsifiable change and a client workload over more
backend names. Optional counters should distinguish injection, local continuation,
peer steal, no-progress poll and explicit yield without a single contended global
measurement counter becoming the workload.

## Retain, investigate and archive

Keep current public execution modes supported and Tokio as the default. Treat
process-owned Tokio runtimes, static shards and custom Root/Runnable/Deque
schedulers as candidates with explicit use cases while their selectors remain
private. Preserve Global/Local/Steal as research controls rather than exposing
every queue policy as an application-facing choice.

Leave generic inline futures in the research archive: their source and negative
throughput evidence remain reproducible. I/O engines and benchmark-only native
transports also remain experiments. Graduation requires a repeatable advantage
in a named application workload, acceptable resource/fairness tradeoffs and a
complete compatibility/maintenance contract. The final report recommends which
questions deserve follow-up; it does not silently promote experimental selectors.
