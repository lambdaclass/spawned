# Ranked library ideas

Assessment updated 2026-11-08 against main after execution groups and OneForOne
supervision landed (`12baea41`). **This is the single ranked backlog.** The
[roadmap](../ROADMAP.md) states the current order in brief; this page holds the
detail. Other research pages ([framework ideas](framework-ideas.md),
[concurrency ideas](concurrency-ideas.md), [Commonware](commonware.md),
[Silver/flux](silver-flux.md), [verification strategy](verification-strategy.md))
are reference material: sources, design notes and experiments, without rankings
of their own.

Rankings are engineering judgment, not measured speedup predictions or release
commitments. They indicate value and sequence of investigation, not a strictly
serial design schedule. Runtime ownership, helper completion, shutdown and
control-event contracts must be designed together before their public APIs are
frozen. No item below changes released behavior by appearing here.

## Implemented (no longer ranked)

| Idea | Status on main |
|---|---|
| ActorSystem and execution groups | Named parked async pools with explicit worker budgets, `start_on(&group)`, owned helpers, readiness and reverse teardown. Dedicated/pinned groups and sized blocking-actor pools remain open (rank 24). |
| Automatic supervision, OneForOne | ChildSpec factories, Permanent/Transient/Temporary, rolling restart limits, capped backoff/jitter, ordered startup and reverse shutdown. OneForAll/RestForOne remain open (rank 23). |
| Fallible startup / readiness | Fallible startup and readiness for supervised children. The `result_on_start` branch is not the implementation; unsupervised `started()` still cannot fail. |
| Instance-bound references | Decided: references are instance-bound; replacements are found by explicit lookup. Stable logical proxies remain an optional later design. |
| Ordered shutdown | Implemented for groups and supervisors. No drain policy yet (rank 12). |
| Supervision lifecycle observation off the user backlog | Implemented for supervision. A generic public priority API is deferred (rank 13). |

## Step 0: settle disagreements with client evidence

| Rank | Idea | Assessment |
|---|---|---|
| 0 | Client-pattern search | One day. Search the pinned Ethrex/Ethlambda revisions from the [client audit](../../research/backend-experiments/2026-11-06-runtime-isolation/CLIENT-WORKLOADS.md) for hand-built pending queues, per-key `HashMap<_, ActorRef<_>>` maps, requests awaited inside handlers, and hand-managed broadcast lists. The counts decide where stashing, virtual actors, deadlock detection and pub-sub belong. |

## Step 1: verify what just landed

Do this before OneForAll/RestForOne and before a release that includes
supervision. Supervision shipped before the lifecycle model that was meant to
precede it; restart and shutdown races are the bug class behind PRs #180, #182
and #185. Plan and acceptance criteria: [verification strategy](verification-strategy.md).

| Rank | Idea | Assessment |
|---|---|---|
| 1 | Formal lifecycle model | TLA+ (or P/Stateright) model of exit reasons, monitors, links, trapped exits, stop and the implemented OneForOne contract. Reintroducing the #182/#185 bugs in the model must produce counterexamples. Also answers per-sender ordering between a dying actor's messages and its exit/`Down`. |
| 2 | Trace validation | Test-only lifecycle events from the Rust code, checked against the model on scenario, churn and supervision failure tests. Ties the spec to the implementation. |
| 3 | Miri and ThreadSanitizer in CI | Cheap. Required before any unsafe envelope or reply-slot optimization. |
| 4 | Mutation testing | `cargo-mutants` over `supervision.rs`, `system.rs` and lifecycle code; surviving mutants become missing tests. Periodic, not per-PR. |

## Step 2: release and run in a client

| Rank | Idea | Assessment |
|---|---|---|
| 5 | Release v0.7 and run it in Ethlambda | The roadmap's validation gate. A full-client replay or same-node canary outranks further synthetic research. Local EngineClient probes are a different evidence level. |

## Step 3: cheap developer-experience track (in parallel)

Small, independent of the scheduler and lifecycle design, and visible to users.

| Rank | Idea | Assessment |
|---|---|---|
| 6 | Test kit with `ActorRef::mock()` | Probe recipients ("sent X within 100 ms"), closure-backed mock actors so code keeps concrete `ActorRef<B>` fields, controllable time for timers. Needs an `Envelope` hook that downcasts to the concrete message and yields message and reply sender. Unmocked messages fail loudly. |
| 7 | Request deadlock detection | `ActorError::WouldDeadlock` naming the cycle instead of a timeout. `ActorRef::request` takes only `&self`, so set a thread-local `CURRENT_ACTOR` only while the actor's own handler future is polled (and around the synchronous handler in thread mode); detached helpers create no edges. Keep a small wait set per actor for `join!`. Measure cost before enabling outside debug builds. |
| 8 | Stall watchdog | Upgrade the debug-only `WarnOnBlocking` (whole-loop type name, 10 ms per poll, Async backend only): name the actor and message type, separate "one poll blocked a worker" from "handler stuck across awaits", optional in release builds. Long handler time alone does not establish deadlock. |
| 9 | Per-actor flight recorder | Last N handled message types and timestamps per actor, dumped on panic, stall or timeout. Bounded and opt-in; a building block for rank 22. |
| 10 | Published state snapshots (`Projected<S>`) | Single-writer, many-reader snapshot slot so readers do not wait behind a long handler; replaces the `Arc<RwLock<_>>` escape hatch. Document that a snapshot can lag messages already sent (no read-your-writes). |

## Step 4: request semantics

| Rank | Idea | Assessment |
|---|---|---|
| 11 | Deferred typed replies, forwarding and pipe-to-self | Highest-value capability after the foundation; the client audit shows Ethlambda hand-builds the pattern with `spawn_blocking` and result messages. Distinguish delegation, deferred reply ownership and completion messages; bound outstanding promises, propagate deadlines and define late-result/owner-exit behavior without general reentrancy. `ctx.spawn_ordered` (parallel stateless compute, then commit in arrival order, revalidating state at commit) is a follow-on specialization. |
| 12 | Coordinated shutdown and bounded draining | Ordered stop exists; add explicit draining as a policy: close admission, handle queued work by policy, join helpers and report unfinished work. Keep current stop/discard as the default. |
| 13 | Separate fair control path | Part of supervision/shutdown design; defer a generic public priority API. Non-trapping actors already receive abnormal exits through cancellation; queued `ExitEnvelope` only matters for trapping actors. Specify boundedness and per-sender ordering for startup, monitor and trapped-exit events. Priority cannot preempt a running handler or starve user work. |
| 14 | Bounded stashing / postponement | After rank 11, as a reply-ownership variant. Stash operates on the envelope, not the message, so the reply sender is kept. Requires a size limit and drop policy (a peer flooding future-slot blocks must not grow memory) and `ActorStopped` for stashed requests on stop. Unrestricted selective receive is not planned. |
| 15 | Actor-owned helper completion scopes | Ownership design gate for shutdown, partly addressed by group-owned helpers. Gate new helper registration, cancel, then explicitly join; helpers depending on their stopped owner can deadlock. |
| 16 | Scoped, lifecycle-owned registries | Use registration generations/compare-and-remove so an old guard cannot remove its replacement. Prerequisite for groups/pub-sub. |

## Step 5: admission, credits and deadlines (one design package)

| Rank | Idea | Assessment |
|---|---|---|
| 17 | Bounded mailboxes and admission | Opt-in alongside unbounded transport. Define reject/wait semantics, cancellation/deadlines and cyclic-send deadlocks. |
| 18 | Pre-allocation byte credits | Independently of optional metrics, bound allocated/in-flight bytes, not just queue slots. Credits can cover a whole pipeline; oversize requests must not wait forever. |
| 19 | Propagated budgets / stale-work skipping | Cheap stale-work rejection need not wait for a complete credit system. Opt-in expiry before dispatch, inherited monotonic deadlines and distinct outcomes. Timeout is not rollback; current requests still run after caller timeout/drop. |

## Step 6: the testing flagship

| Rank | Idea | Assessment |
|---|---|---|
| 20 | Actor-level ordering exploration (`spawned::sim::explore`) | Treat one handler turn as an atomic step and control delivery at the mailbox boundary; only deliveries racing at the same actor, timer or exit branch the schedule (actor-level partial-order reduction, as in P#/Coyote). Failures shrink to readable delivery lists and can print as sequence diagrams. Start here rather than full environment simulation. Shuttle stays an internal tool for framework races (see verification strategy). Handlers that use Tokio I/O, `spawn_blocking` or wall-clock time directly remain outside control. |
| 21 | Per-actor crash tape | Record one actor's inputs and replay its crash in a unit test. Inputs include replies to its own requests, clock reads and I/O, and replay needs a state snapshot at the tape start (or a tape since `started()`); the last N messages into a fresh instance do not reproduce state. Does not reproduce cross-actor races. |
| 22 | Record in production, replay in simulation | Record ingress plus mailbox delivery order (`(actor_id, seq)` per dequeue) in a bounded black box, replay with enforced order, detect divergence with per-handler hashes. Needs ranks 20 and 21, `ctx.now()`/`ctx.rng()` discipline in client code and DB snapshots. Go/no-go: replay a reintroduced known race every time. Highest ceiling; not started before 20 and 21. |

## When step 0 or a user justifies them

| Rank | Idea | Assessment |
|---|---|---|
| 23 | OneForAll and RestForOne, nested supervisor specs | After the OneForOne lifecycle is verified (step 1) and proven in an application. Specify in the model before implementing. |
| 24 | Dedicated/pinned groups and a lighter dedicated-thread backend | **The Tokio-related item that matters to the clients.** Tasks-mode `Backend::Thread` creates a Tokio runtime per actor (capped to one helper worker in `bb1e2dd6`); Ethlambda's `BlockChainServer` uses it. A lighter dedicated-thread backend or group reduces that cost without removing Tokio. |
| 25 | Genuinely Tokio-free thread builds | A build capability: thread-only applications that do not compile Tokio. It does **not** help Ethrex or Ethlambda, which depend on Tokio throughout. Address Response, completion, monitoring, timers and macro output with dependency-graph CI. Opens an embedded/Embassy path later. Do it when a user needs it. |
| 26 | Virtual (per-key) actors, in-process | Address by key; created on first message, stopped when idle, recreated on demand, with activation/deactivation hooks, one live instance per key, ordered handoff and reload after crash. No storage or distribution layer. Promote if step 0 finds hand-written per-key actor maps; Orleans-style distributed placement stays at rank 44. |
| 27 | Process groups, typed topics and routers | After rank 16. Membership cleanup, per-recipient backpressure and partial-failure semantics matter more than a broadcast convenience method. Pools with round-robin, least-busy or key-hash routing (key hash preserves per-key order) belong here. |
| 28 | Causal tracing and ingress context | Preserve span context across awaits/fan-out and bound retention/sampling cost. |
| 29 | Bounded role/cohort metrics, oldest age, active monitor counts | Bound cohort/role cardinality and define gauge lifetime; avoid actor-ID label growth. Exact queue age needs a transport contract independent of sampled timing. |
| 30 | Read-only runtime inspection | Safe status/last-known snapshots: id, type, parent/links, mailbox depth, idle/polling/suspended status, current message type, turn age, wait-for edges. Actor state `Debug` only between turns, because the handler future holds `&mut A` across awaits. |
| 31 | Handler middleware (Tower-style) | Tracing, rate limits, timeouts, retries and dedup without handler edits; deadlines and tracing could be middleware rather than core special cases. |
| 32 | State-machine behavior | `gen_statem`-style per-state handlers and timeouts, built on bounded stashing (rank 14). |
| 33 | Typed streams and demand-driven pipelines | After credits. Specify subscription ownership, slow consumers, failure, credit release and bounded completion-watermark windows. Ownership release differs from explicit acknowledgement. Start with one concrete producer/consumer. |
| 34 | Per-handler blocking-worker leasing | Prototype for mostly idle blocking actors. Preserve serial turns and runtime/I/O ownership; measure worker capacity and long-handler progress. |
| 35 | Batch handlers / generated batch dispatch | Promising for actual I/O/crypto batching; no guaranteed win. Preserve per-message replies, failure/drop semantics and ordering across heterogeneous message types. |
| 36 | Explicit coalescing / overload policies | For obsolete progress/state notifications, with bounded keys. Requests cannot silently disappear; superseded requests get an explicit error. Single-flight sharing of identical in-flight reads is a related opt-in, only for pure reads. |
| 37 | Generated closed-protocol dispatch | After profiling framework cost. Today a request allocates a reply channel, a boxed envelope and a boxed handler future. Enum dispatch needs codegen/transport bridging while retaining `Handler`/`Recipient` support; payload and reply allocations remain separate costs. Reusable reply slots only if profiles show they matter. |
| 38 | Timer drift / missed-tick policies | Fixed-delay/rate, Skip/Delay/Burst and cancellation. Preserve current timer behavior unless users opt in. |
| 39 | CPU service classes / weighted resource groups | Requires a workload and a clear fairness contract. Elapsed handler time includes awaits and is not CPU accounting. |
| 40 | Ready-made integrations | `Recipient<M>` as a stream sink, actors behind Axum/Tonic as Tower services, a connection-owning actor with reconnect/backoff. Build on demand. |
| 41 | AI-agent example | One example showing supervision, timeouts, cancellation and tracing for long-running agents. Positioning, not a library change. |

## Research or separate products

| Rank | Idea | Assessment |
|---|---|---|
| 42 | CoDel-inspired mailbox shedding | Isolated experiment for explicitly droppable traffic after admission/deadlines. Network CoDel constants are not actor defaults; sampled metrics timestamps are insufficient for its controller. Compare against simpler policies. |
| 43 | Out-of-process/shared-memory flight recorder | If real hangs defeat in-process inspection. A bounded versioned last-known snapshot is a smaller first step; a recorder also needs ordered events, sequence/gap reporting and loss accounting. |
| 44 | Distributed actors | Large optional layer needing wire/version/security/failure contracts. Virtual actors (rank 26) keep the addressing path open. |
| 45 | Compile-time actor graph | Macro-generated topology for diagrams and static request-cycle checks; hard with dynamic actors. |
| 46 | Sparse-work root-driver yield hypothesis | Private experiment when profiles show deferred-wake delay; not a default or backend. |
| 47 | Session-typed protocols | Opt-in experiment for a fixed two-party protocol. Does not prove deadlock freedom for arbitrary dynamic actors, blocking handlers or failing external systems. |
| 48 | Fair join patterns | Until real coordination code needs them. Bound unmatched messages and define fairness, cancellation and interactions with ordinary handlers. |
| 49 | Durable outbox/idempotency recipe | Application-layer example when recovery needs it. Commit intent/dedup with effects; do not persist arbitrary boxed Rust envelopes. |
| 50 | Durable timers/reminders | Separate persistence contract: identity, clock changes, missed executions and duplication across restart. |
| 51 | Temporal-style durable execution | A separate product-scale subsystem: persisted decisions, deterministic replay, external effects and upgrades. Supervision alone does not provide it. |

## Parallel validation track

Start client profiling in parallel with the steps above; run one instrumented
Ethrex/Ethlambda workload before promoting performance policies. Local Ethlambda
EngineClient/actor probes have run; a full-client replay/canary is still needed.
A real block/gossip replay should establish where time/bytes wait and which
regression bounds matter. Synthetic ratios do not transfer automatically.

Failure testing and simulation are enabling work, not substitutes for the
capabilities above. Keep generated cases/Loom, add semantic failure coverage,
independent short request histories and controlled handler-progress replays.
Those replays do not yet control arbitrary Tokio/OS scheduling, timer firing or
all link/monitor interleavings. Freeze only deliberately stable lifecycle
contracts in conformance fixtures. See [testing](../TESTING.md).

Backend research (schedulers, queues, affinity, I/O engines) stays frozen until a
client profile supplies a concrete reason.

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
- The pinned and runtime-isolation ratios (for example 19.0 ms → 0.040 ms p99)
  are fixture-specific, with different tradeoffs/achieved loads. They justify API
  research, not general speedup claims.
- "Instant" deadlock detection, "100% reproducible" simulation and "10x" claims
  in review proposals are unmeasured. Simulation reproduces only what the
  framework controls.
- A crash tape of the last N messages replayed into a fresh instance does not
  reproduce the state those messages were applied to.
- A message an actor sends before exiting is enqueued before its monitor `Down`
  (FIFO publication order); the open ordering question concerns messages sent
  by its helper tasks and any future priority lane.
