# Ranked library ideas

Assessment updated 2026-11-08 against v0.6.0. These rankings are engineering
judgment, not measured speedup predictions or release commitments. The current
[roadmap](../ROADMAP.md) makes execution groups plus supervision the next combined milestone. Rankings indicate value
and sequence of investigation, not a strictly serial design schedule. Runtime
ownership, helper completion, shutdown and control-event contracts must be
designed together before their public APIs are frozen.

## Build the foundation

| Rank | Idea | Assessment / dependency |
|---|---|---|
| 1 | ActorSystem, execution groups and placement | Phase 1a of the next combined milestone. Optional system ownership, parked pools, bounded workers, fallible construction and stop/join ownership; preserve `.start()`. Design recovery placement and helper lifetimes with phase 1b. Affinity and additional execution profiles graduate separately. |
| 2 | Automatic supervision/restart policies | Phase 1b. ChildSpec factories, OneForOne, Permanent/Transient/Temporary modes, restart intensity/window, backoff/jitter and ordered shutdown. Instance-bound ActorRef; explicit replacement lookup. No implicit state restoration or effect replay. OneForAll/RestForOne follow later. |
| 3 | True Tokio-free thread feature | Independently useful build capability after/alongside the combined milestone. Completion, Response, monitoring, timers and macro output need a feature plan and dependency-graph CI. |
| 4 | Scoped, lifecycle-owned registries | High-value smaller foundation for systems and supervision. Use registration generations/compare-and-remove so an old guard cannot remove its replacement. |
| 5 | Fallible startup / readiness | High value for reliable boot and supervision. Preserve old lifecycle implementations; define typed startup failure and when a reference becomes usable. The remote `result_on_start` branch exists but is not an approved implementation. |
| 6 | Coordinated shutdown and bounded draining | Ownership/shutdown design gate for 1/2. Start with explicit stop-and-join; full draining closes admission, handles queued work by policy, joins helpers and reports unfinished work. Keep current stop/discard as the default. |
| 7 | Deferred typed replies, forwarding and pipe-to-self | High-value capability after the foundation. Distinguish delegation, deferred reply ownership and completion messages; bound outstanding promises, propagate deadlines and define late-result/owner-exit behavior without general reentrancy. |
| 8 | Bounded mailboxes and admission | Design with 9/10, opt-in alongside unbounded transport. Define reject/wait semantics, cancellation/deadlines and cyclic-send deadlocks. |
| 9 | Pre-allocation byte credits | Design with 8/10: independently of optional metrics, bound allocated/in-flight bytes, not just queue slots. Credits can cover a whole pipeline; oversize requests must not wait forever. |
| 10 | Propagated budgets / stale-work skipping | Design with admission; cheap stale-work rejection need not wait for a complete credit system. Opt-in expiry before dispatch, inherited monotonic deadlines and distinct outcomes. Timeout is not rollback; current requests still run after caller timeout/drop. |
| 11 | Actor-owned helper completion scopes | Ownership design gate for 1/6, not a late add-on. Gate new helper registration, cancel, then explicitly join; helpers depending on their stopped owner can deadlock. |
| 12 | Separate fair control path | Part of the supervision/shutdown design in 2/6; defer a generic public priority API. Specify boundedness and ordering for startup, monitor and trapped-exit events. Current stop cancellation already bypasses the queue. Priority cannot preempt a running handler or starve user work. |

## Add capabilities against a client need

| Rank | Idea | Assessment / experiment |
|---|---|---|
| 13 | Causal tracing and ingress context | Valuable next observability step for a real actor graph. Preserve span context across awaits/fan-out and bound retention/sampling cost. |
| 14 | Bounded role/cohort metrics, oldest age, active monitor counts | Useful diagnosis beyond v0.6 globals. Bound cohort/role cardinality and define gauge lifetime; avoid actor-ID label growth. Exact queue age needs a transport contract independent of sampled timing. |
| 15 | Typed streams and demand-driven pipelines | Specify subscription ownership, slow consumers, failure, credit release and bounded completion-watermark windows. Ownership release differs from explicit successful acknowledgement. Start with one concrete producer/consumer application. |
| 16 | Per-handler blocking-worker leasing | Worth a prototype for mostly idle blocking actors. Preserve serial turns and runtime/I/O ownership; measure worker capacity and long-handler progress. Existing private executors are evidence, not a promoted replacement. |
| 17 | Batch handlers / generated batch dispatch | Promising for actual I/O/crypto batching. No guaranteed win. Preserve per-message replies, failure/drop semantics and ordering across heterogeneous message types. |
| 18 | Explicit coalescing / overload policies | Good for obsolete progress/state notifications. Use bounded keys; ordinary transactional requests cannot silently disappear. |
| 19 | Timer drift / missed-tick policies | Useful smaller project: fixed-delay/rate, Skip/Delay/Burst and cancellation. Preserve current timer behavior unless users opt in. Durable timers are separate. |
| 20 | Process groups, typed topics and routers | Useful building blocks after registry lifetimes. Membership cleanup, per-recipient backpressure and partial-failure semantics matter more than a broadcast convenience method. |
| 21 | State-machine behavior and bounded stashing | Useful for connection/protocol phases. Start with explicit bounded postponement; unrestricted selective receive has harder ordering and memory semantics. |
| 22 | Read-only runtime inspection and bounded event logs | Start with safe status/last-known snapshots, type names and redaction. A busy handler cannot be synchronously inspected through its own mailbox. |
| 23 | Generated closed-protocol dispatch optimization | Prototype after profiling framework cost. Enum dispatch needs explicit codegen/transport bridging while retaining flexible/manual Handler support. It is not automatically allocation-free; payload and reply allocations remain separate costs. |
| 24 | CPU service classes / weighted resource groups | Potentially better utilization than fixed partitions, but requires a workload and a clear fairness contract. Elapsed handler time includes awaits and is not CPU accounting. |

## Research or separate products

| Rank | Idea | Assessment |
|---|---|---|
| 25 | CoDel-inspired mailbox shedding | Isolated experiment for explicitly droppable traffic after admission/deadlines. Network CoDel constants are not actor defaults; sampled metrics timestamps are insufficient for its controller. Compare against simpler policies. |
| 26 | Out-of-process/shared-memory flight recorder | Useful if real hangs defeat ordinary inspection. A bounded versioned last-known snapshot is a smaller first step; a flight recorder also needs ordered events, sequence/gap reporting and loss accounting. Shared memory/CLI adds platform work. Long handler time alone does not establish deadlock. |
| 27 | Sparse-work root-driver yield hypothesis | Cheap private experiment when profiles show deferred-wake delay. Measure latency, idle CPU and wake correctness; not a new default or backend. |
| 28 | Session-typed protocols | Interesting opt-in experiment for a fixed two-party protocol. Does not prove deadlock freedom for arbitrary dynamic actors, blocking handlers or failing external systems. |
| 29 | Fair join patterns | Lower priority until real coordination code needs them. Bound unmatched messages and define fairness, cancellation and interactions with ordinary handlers. |
| 30 | Durable outbox/idempotency recipe | Useful application-layer example when recovery needs it. Commit intent/dedup with effects; do not persist arbitrary boxed Rust envelopes. |
| 31 | Durable timers/reminders | Separate persistence contract: identity, clock changes, missed executions and duplication across restart. |
| 32 | Distributed actors | Large optional layer needing wire/version/security/failure contracts; not the next step for the current local framework. |
| 33 | Virtual actors | Different activation, storage and identity model. Defer until a client needs it. |
| 34 | Temporal-style durable execution | A separate product-scale subsystem: persisted decisions, deterministic replay, external effects and upgrades. Supervision alone does not provide it. |

## Parallel validation track

Start client profiling in parallel with the first milestone; run one instrumented
Ethrex/Ethlambda workload before promoting performance policies. Local Ethlambda
EngineClient/actor probes have run; a full-client replay/canary is still needed.
A real block/gossip replay
should establish where time/bytes wait and which regression bounds matter.
Synthetic ratios do not transfer automatically.

Failure testing and simulation are enabling work, not substitutes for the
capabilities above. Keep generated cases/Loom, add semantic failure coverage,
independent short request histories and controlled handler-progress replays.
Those replays do not yet control arbitrary Tokio/OS scheduling, timer firing or
all link/monitor interleavings. Full Commonware-style deterministic environment
simulation remains research. Freeze only deliberately stable lifecycle contracts
in conformance fixtures. See [testing](../TESTING.md).

## Corrections to the proposed descriptions

- CoDel measures persistent queue delay in networking; its 5 ms / 100 ms settings
  are not justified actor defaults. Metrics timing can be sampled or absent and
  begins before transport publication. [RFC 8289](https://www.rfc-editor.org/rfc/rfc8289.html).
- Batch processing can amortize useful work, but "huge wins" require measurement;
  draining all messages of one type can reorder a mixed protocol.
- Deferred replies are a concrete capability, not automatic reentrancy.
  Delegation transfers reply responsibility; a promise retains it. Pipe-to-self
  brings results back through actor dispatch rather than mutating actor state
  from a helper. [CAF promises/delegation](https://actor-framework.readthedocs.io/en/stable/core/MessagePassing.html),
  [Pekko pipe-to-self](https://pekko.apache.org/docs/pekko/current/typed/interaction-patterns.html#send-future-result-to-self).
- Priority queues change queued-event ordering, not the ability to interrupt
  arbitrary Rust. Ractor's Kill behavior is different from Spawned's cooperative
  linked Kill. [Ractor semantics](https://github.com/slawlor/ractor/blob/main/docs/runtime-semantics.md).
- Draining and phased shutdown should be explicit policies rather than silently
  changing current stop behavior. Expired phase deadlines can leave unfinished
  work; coordinated shutdown does not imply successful drain.
  [Pekko coordinated shutdown](https://pekko.apache.org/docs/pekko/current/coordinated-shutdown.html).
- Completion watermarks track released ownership; successful acknowledgement
  requires a distinct explicit contract. Bound outstanding sequence windows.
  [Commonware tracked channel](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/utils/src/channel/tracked.rs),
  [acknowledgement](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/utils/src/acknowledgement.rs).
- Restarted instances need new identities. Stable logical proxies/rebinding are
  optional designs, not prerequisites or automatic exactly-once RPC.
- The pinned and runtime-isolation ratios are fixture-specific, with different
  tradeoffs/achieved loads. They justify API research, not general speedup claims.

The [Commonware review](commonware.md) and [earlier comparison](concurrency-ideas.md)
provide sources and smaller experiments. No item above changes v0.6.0 behavior by
appearing in this backlog.
