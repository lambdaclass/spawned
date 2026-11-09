# Spawned roadmap

Updated 2026-11-09. This is the single planning document: the phases below give
the implementation order in brief, and the [ranked backlog](#ranked-backlog)
holds the detail, tradeoffs and client evidence behind it. It is an order, not
a release schedule, and no speculative feature is promised by a date.

Business handlers stay ordinary Rust. The framework owns dispatch and lifecycle.
Keep typed messages/replies, dynamic actors, addressed delivery, cooperative
handlers and the existing default behavior. New policies are explicit and opt-in.

## Where we are

- **v0.6.0 (released):** typed protocols, async/blocking/thread execution, actor
  identities, monitors, links and trapped exits, linked startup, shutdown fixes,
  optional framework metrics. No automatic supervision. Thread actors need no
  active Tokio executor but still compile Tokio.
- **Main (unreleased):** `ActorSystem` with async/native/pinned execution groups, Tokio-free
  synchronous builds, and nested OneForOne/OneForAll/RestForOne supervision (see [Implemented](#implemented-no-longer-ranked)), a
  bounded dedicated-thread runtime, and the supervision verification of phase 2.
  See the [guide](docs/GUIDE.md#execution-groups-and-supervision) and the
  [design contract](docs/design/execution-groups-supervision.md).

## Phases

1. **Execution groups plus supervision: implemented.** Includes fixed native actor
   pools, parked Linux pinned shards, nested supervisor specs, OneForAll and
   RestForOne. Tokio-free synchronous builds are also implemented. All are
   unreleased; examples and resource limits are in the guide.
2. **Verify what landed before v0.7:** retain #192's OneForOne checks and add
   cohort selection/barrier/supersession and nested shutdown ownership models,
   replay real Rust traces and require negative controls to fail. These bounded
   projections complement failure/stress tests, review and ThreadSanitizer;
   they do not prove the wider base lifecycle or every Rust schedule.
   All crates retain `forbid(unsafe_code)`; Miri precedes any future unsafe proposal.
   See the [implementation evidence](legacy/docs/reviews/capabilities-extension.md)
   and [v0.7 review fixes and formal coverage](docs/reviews/v07-supervision.md).
   The client search is done; see [results](#client-search-results).
3. **Release v0.7 and run it in Ethlambda** (rank 5), including the per-method
   request timeout (rank 5a), a migration guide for the clients' hand-wired
   shutdown and startup, and an example for every way of using the library
   (rank 5b, [examples to add](#examples-to-add)). In parallel, the
   developer-experience track (ranks 6-10: test kit, deadlock detection, stall
   watchdog, flight recorder, published snapshots), motivated by the
   [client sweep](#client-sweep-production-pain).
4. **Then, guided by application needs:** request semantics, starting with
   deferred replies and pipe-to-self (ranks 11-16), with the dynamic
   supervisor (rank 12a) alongside the keyed registry; admission, credits and
   deadlines as one design package (ranks 17-19); the testing flagship (ranks
   20-22); then ranks 23-41 when a client or user justifies them.

A full Ethrex/Ethlambda replay or same-node canary remains the validation gate.
Local EngineClient probes and synthetic isolation/pipeline studies are different
evidence levels; do not transfer their ratios to a production client. Backend
research stays frozen until a client profile supplies a concrete reason (see the
[parallel validation track](#parallel-validation-track)).

**Tokio-free builds versus the clients.** The implemented native build lets
thread-only applications avoid compiling Tokio; it does not help Ethrex or
Ethlambda, which depend on Tokio throughout. The client-relevant item is a
fixed native actor pool, which shares one helper runtime rather than creating
one per actor. Additional placement changes still need application measurements. ActorSystem,
execution groups and supervision remain async-only for v0.7. Extending those
capabilities to the Tokio-free build is a later roadmap item, outside this release.

### Non-goals

Performance work may add opt-in profiles, but none of these may become the
default (from the [Silver/flux study](legacy/docs/research/silver-flux.md#what-must-not-become-spawneds-default)):

- static topology replacing dynamic actor creation;
- Copy-only payload requirements replacing ordinary Rust message types;
- broadcast replacing addressed delivery;
- silent loss on mailbox overwrite, especially for requests;
- application handlers owning poll loops, reply routing or runtime
  infrastructure to make a backend faster;
- mandatory dedicated cores or spinning for ordinary actors.

### Verification rule

Any feature that touches lifecycle, replies or ownership ships with a TLA+
model, trace replay of real Rust runs and negative checks that must fail, as
OneForOne, OneForAll/RestForOne and nested supervision did. Release is blocked
until they pass. Everything also keeps ordinary tests, an example, clippy and
ThreadSanitizer.

| Needs a model | What the model must show |
|---|---|
| Dynamic supervisor (rank 12a) | Children starting and stopping while others restart or the supervisor shuts down; `max_children` is never exceeded, counting restarting children; no start succeeds after shutdown begins. |
| Deferred replies and pipe-to-self (rank 11) | Every accepted request gets exactly one reply, including on owner exit or panic; a late result never reaches a restarted instance. |
| Keyed registry (rank 12) | An old entry's cleanup never removes its replacement (ethrex #7435). |
| Draining shutdown (rank 13) | Queued work is handled or reported; shutdown finishes within its deadline. |
| Bounded mailboxes and credits (ranks 17-18) | No credit leaks; waiting senders cannot deadlock. |
| Base actor lifecycle | Links, monitors, `Down` exactly once, every request answered or `ActorStopped`; reintroducing the PR #182/#185 bugs must fail ([next model](verification/tla/README.md#next-model)). |

No model needed (tests, examples and review suffice): the per-method timeout
(5a), examples (5b), the migration guide, metrics, snapshots (10), the test kit
(6) and macro work. Deadlock detection (7) and the stall watchdog (8) are
optional; their risk is false alarms, which tests catch better.

### Testing to add

Existing layers are listed in [TESTING.md](docs/TESTING.md): unit and
integration tests, proptest scenarios, trybuild, Loom, ThreadSanitizer,
cargo-mutants and TLA+ with trace replay. Release gates and remaining gaps:

| When | Test | Why |
|---|---|---|
| v0.7: implemented | `cargo-semver-checks` and a `cargo-hack --each-feature` build matrix | Default-API compatibility against v0.6.0 plus each feature without dev-dependency unification. See [testing](docs/TESTING.md#api-and-feature-checks). |
| v0.7: implemented | Supervisor policies under proptest | Generated crash/exit/shutdown scripts checked against an independent generation/retirement oracle for all strategies and modes; [scope and replay](docs/TESTING.md#generated-supervisor-policy-scripts). |
| v0.7 | Fault injection in the Ethlambda canary | Kill and restart supervised actors in a running client and check it recovers; the release gate (rank 5) should exercise failures, not just the happy path. |
| After v0.7 | Shuttle for multi-actor framework races | Randomised schedules over whole supervisor/registry tests too large for Loom. Listed in TESTING.md, not started. Internal tool, separate from rank 20. |
| After v0.7 | Long-running soak tests | Memory, file descriptors, threads and tasks stay flat over hours of actor churn and restarts. The ethrex 1.5 GB/day mailbox leak is the motivating bug. Nightly, not per PR. |
| With ranks 11-13 | Loom on the new primitives | Reply slots, registry compare-and-remove and mailbox publication versus stop. |
| Ongoing | Coverage report (`cargo-llvm-cov`) | Find untested branches in supervision and system code; pairs with the surviving mutants. Report only, no threshold. |

Deterministic simulation and record/replay remain the testing flagship (ranks
20-22).

The [v0.7 gate review](docs/reviews/v07-release-gates.md) records the completed
API/feature scans, generated-policy campaign and compatibility correction.

## Ranked backlog

Rankings are engineering judgment, not measured speedup predictions or release
commitments. They indicate value and sequence of investigation, not a strictly
serial design schedule. Runtime ownership, helper completion, shutdown and
control-event contracts must be designed together before their public APIs are
frozen. No item below changes released behavior by appearing here.

Background notes (sources, design notes and experiments, without rankings of
their own) are archived in [legacy](legacy/README.md):
[framework ideas](legacy/docs/research/framework-ideas.md),
[concurrency ideas](legacy/docs/research/concurrency-ideas.md),
[Commonware](legacy/docs/research/commonware.md),
[Silver/flux](legacy/docs/research/silver-flux.md),
[verification strategy](legacy/docs/research/verification-strategy.md) and the
earlier [detailed acceptance criteria](legacy/docs/design/roadmap-acceptance-2026-11-06.md).

### Implemented (no longer ranked)

| Idea | Status on main |
|---|---|
| ActorSystem and execution groups | Named parked async pools with explicit worker budgets, `start_on(&group)`, owned helpers, readiness and reverse teardown. Fixed native actor pools and parked Linux pinned shards are implemented; budgets exclude helper/owner/control/coordinator threads. |
| Automatic supervision trees | ChildSpec factories, Permanent/Transient/Temporary, rolling restart limits, capped backoff/jitter, ordered startup and reverse shutdown. Nested specs and OneForAll/RestForOne are implemented, with actual cleanup barriers and bounded formal models/real trace checks for all three strategies and nested ownership. |
| Tokio-free synchronous builds | Disable async defaults on both crates; isolated fixture and graph CI cover protocols, lifecycle and metrics. ActorSystem/supervision remain async-only. |
| Fallible startup / readiness | Fallible startup and readiness for supervised children. The `result_on_start` branch is not the implementation; unsupervised `started()` still cannot fail. |
| Instance-bound references | Decided: references are instance-bound; replacements are found by explicit lookup. Stable logical proxies remain an optional later design. |
| Ordered shutdown | Implemented for groups and supervisors. No drain policy yet (rank 13). |
| Supervision lifecycle observation off the user backlog | Implemented for supervision. A generic public priority API is deferred (rank 14). |

### Step 0: settle disagreements with client evidence

| Rank | Idea | Assessment |
|---|---|---|
| 0 | Client-pattern search | **Done 2026-11-09**; see [Client search results](#client-search-results). Deferred replies and pipe-to-self are confirmed as the next capability; published snapshots and lifecycle-owned keyed registries move up; generic stashing moves down; Orleans-style activation stays needs-driven. |

#### Client search results

Searched upstream main of ethrex `1dfaa09ec` and ethlambda `c3f5f566` (2026-11-09)
for the patterns the backlog disagreements depended on. Source reading, not a
performance result.

| Pattern | Ethrex | Ethlambda | Consequence |
|---|---|---|---|
| Hand-built deferred replies | **Strong.** `outgoing_request` carries a caller's `oneshot::Sender` inside a one-way message; the connection actor keeps [`current_requests`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/networking/p2p/rlpx/connection/server.rs#L421) and a `request_timeout(id)` message, completing replies when the peer answers. 22 references across peer handling, snap sync and tooling. | **Pipe-to-self.** [Gossip validation](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/net/p2p/src/beacon/verdict.rs#L640) runs on `spawn_blocking` behind semaphores (overload is reported, not queued), `catch_unwind` guarantees exactly one verdict, and a stopped actor is logged. Aggregation streams worker results back as messages. | Rank 11 confirmed. The design must cover what both clients hand-build: reply timeouts, bounded outstanding work, exactly-one completion including on panic, and owner-exit outcomes. |
| Per-key actor maps | **Yes.** [`PeerTableServer`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/networking/p2p/peer_table.rs#L520) keeps `IndexMap<H256, PeerData>` holding each peer's connection actor, plus a hand-rolled `ConnectionId` guard added after a real bug (#7435: a duplicate connection's cleanup removed the live entry). | None found. | Lifecycle-owned keyed registries with generation guards move up (rank 12). Peers are created by dial/accept, not by a first message, so Orleans-style activation (rank 26) stays needs-driven. |
| Requests awaited in handlers | **Yes.** Connection, discovery and broadcaster actors await requests to the single `PeerTableServer`, whose protocol has 22 request methods, mostly reads by name (`get_*`, `peer_count`, `target_*`). No request cycle found. | Few. | Published state snapshots (rank 10) have a concrete first user; deadlock detection (rank 7) stays a cheap safety net. |
| Hand-built pending queues | Minor (`pending_l1_messages`). | **Yes, but domain-specific.** [`pending_blocks`, `blocks_awaiting_columns`, `sidecars_awaiting_parent`](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/blockchain/src/lib.rs#L464), keyed by parent hash, bounded, evicted at finality. | A generic in-order stash would not replace these; stashing moves down (rank 16). |
| Hand-managed broadcast lists | The broadcaster requests all connected peers on each broadcast and loops over them. | None found. | Mild evidence for groups/pub-sub (rank 27); snapshots also cover it. |


#### Client sweep: production pain

A broader read of the same revisions for workarounds, incidents and comments
(2026-11-09). Each row cites code or a commit message; inferences are marked.

| Signal | Evidence | What it argues for |
|---|---|---|
| Hardwired 5 s request timeout | `#[protocol]` always uses `DEFAULT_REQUEST_TIMEOUT` (spawned `macros/src/lib.rs`). A timed-out `get_best_peer` under mailbox pressure exited the ethrex node ([63167661a](https://github.com/lambdaclass/ethrex/commit/63167661a)); ethrex now treats `RequestTimeout` as recoverable. | **New, rank 5a:** per-protocol/per-method timeout and a per-call override in the macro. Small; belongs in v0.7. Then propagated deadlines (rank 19). |
| Unbounded mailboxes leaked memory | ~1.5 GB/day heap growth traced to per-connection actors' unbounded mailboxes and `spawn_listener` ([d0b9fecb9](https://github.com/lambdaclass/ethrex/commit/d0b9fecb9)). Workarounds: a bounded channel plus raw writer task, a 150-connection semaphore whose permit lives in actor state. Inbound `spawn_listener` streams remain unbounded in both clients. | Bounded admission/credits (ranks 17-18) and bounded, demand-driven listeners (rank 33) have production evidence, not just design motivation. |
| Hand-wired, racy shutdown | Three ethrex actors each stop on their own Ctrl-C listener; the main path cancels a token, requests the peer table (saving an empty list if it already stopped), then sleeps 1 s. `ActorStopped` triggers `exit(2)`, which caused 6 of 25 failed CI runs ([188ac1331](https://github.com/lambdaclass/ethrex/commit/188ac1331)). Ethlambda stops actors by hand with no deadline; shutdown took 14.3 s against Docker's 10 s and users were told to raise the stop timeout ([1d7902da](https://github.com/lambdaclass/ethlambda/commit/1d7902da)). The ethrex L2 sequencer starts actors one by one and only logs startup failures. | Adopting v0.7 `ActorSystem` shutdown with a deadline and OneForOne supervision addresses this directly; a migration guide for these exact patterns belongs in the release (rank 5). Draining (rank 13) next. |
| Handlers that stall the runtime or cannot be stopped | ethrex comments that a connection actor "cannot be stopped while a handler is wedged in `.await`"; synchronous cell serving occupied every runtime worker ([d958dcba4](https://github.com/lambdaclass/ethrex/commit/d958dcba4)); per-connection L2 imports deadlocked at 0% CPU and froze RPC and logging ([535647e45](https://github.com/lambdaclass/ethrex/commit/535647e45)). Ethlambda runs its chain actor on `Backend::Thread` because its handlers starve a shared runtime. | Stall watchdog (rank 8) and handler-duration metrics; deferred replies (rank 11) so slow work leaves the mailbox. |
| Polling for a free peer | Sync code calls `get_best_peer` then sleeps 10 ms in a loop at five sites; a permit's `Drop` returns capacity with a fire-and-forget send whose failure is ignored. | A request answered when capacity frees up is a deferred reply (rank 11); a lease pool with waiters fits admission (rank 17). |
| Hand-rolled timer policies | ~14 ethrex actors reschedule with `send_after` so the next tick waits for the current one; a committer uses its timer's cancellation token as a pause flag; ethlambda aligns ticks to wall-clock slots and measures lateness by hand; the swarm loop uses `MissedTickBehavior::Skip`. | Timer drift policies (rank 38) promoted: fixed-delay, wall-clock aligned (`send_at`), skip/catch-up, pause/resume, tick coalescing. Low cost. |
| Helper tasks owned by hand or not at all | Ethlambda's aggregation session holds a token, `JoinHandle` and session id to drop stale results, joined with a 2 s timeout before a new session and on stop. Ethrex `tokio::spawn`s unowned work per transaction batch, header chunk and subscription. | Owned helpers (`ctx.spawn_helper`, on main) plus cancel-previous, generation fencing for stale results and join-with-deadline on stop (rank 15). |
| Test doubles | Ethlambda stubs an entire protocol by hand three times in tests; ethrex tests spawn real actors with dummy contexts. | Test kit with `ActorRef::mock()` (rank 6). |
| Cyclic wiring after spawn | Actors that reference each other are wired with `InitP2P`/`InitBlockChain` messages, forcing `Option<Ref>` fields. | **New:** late binding or two-phase start for actor cycles (with rank 12's keyed registry). |
| Self-only messages in macros | Ten `#[allow(dead_code)] // invoked via send_after` on one protocol, plus ~18 manual `impl Handler` blocks. | **New, small:** private/self-only messages in `#[actor]`/`#[protocol]`. |
| Hand-built observability | Ethlambda threads `Instant` stamps through messages to measure mailbox wait; ethrex polls the peer table every second for counts and checks actor health with an ordinary request (inferred: it times out while a blocking actor works); ethlambda's metadata is a hand-built `Arc<RwLock>` snapshot. | v0.6 metrics already measure queue wait (adoption, rank 5); out-of-band health/inspection (rank 30); published snapshots (rank 10). |
| Stream lag dropped silently | `BroadcastStream` `Lagged` errors are discarded before `spawn_listener`; another site hand-writes a `select!` loop to log lag. | Lag-aware listeners in typed streams (rank 33). |
| Exit reasons outside actors | A swarm adapter cannot tell an actor panic from a stop; nothing restarts the swarm when it ends. 29 ignored sends in ethrex. | Monitors/supervision for non-actor tasks; documented guidance on ignored sends. |
| Singleton worker built outside spawned | ethrex's L2 block importer uses `Arc<Mutex<BTreeMap>>`, `Notify` and a raw task because `submit` must return a dedup result synchronously; it has no shutdown path. | **New, small:** a synchronous `try_admit`/keyed coalescing mailbox, or document the pattern with an owned helper. |

### Step 1: verify what just landed

Preserve this verification before a release and expand its scope for the new
cohort/nested semantics. Restart and shutdown races are the bug class behind
PRs #180, #182 and #185. Plan and acceptance criteria: [verification strategy](legacy/docs/research/verification-strategy.md).

| Rank | Idea | Assessment |
|---|---|---|
| 1 | Formal lifecycle model | **Supervision done** ([#192](https://github.com/lambdaclass/spawned/pull/192), `verification/tla`). Remaining: the base lifecycle (links, monitors, trapped exits, stop). Original scope: TLA+ (or P/Stateright) model of exit reasons, monitors, links, trapped exits, stop and the implemented OneForOne contract. Reintroducing the #182/#185 bugs in the model must produce counterexamples. Also answers per-sender ordering between a dying actor's messages and its exit/`Down`. |
| 2 | Trace validation | **Supervision done** (#192, `lifecycle-trace` feature); extend with rank 1. Original scope: Test-only lifecycle events from the Rust code, checked against the model on scenario, churn and supervision failure tests. Ties the spec to the implementation. |
| 3 | Miri and ThreadSanitizer in CI | **Done differently** (#192): ThreadSanitizer runs in CI; all crates are `#![forbid(unsafe_code)]`, so Miri waits until unsafe code is proposed. Original note: Cheap. Required before any unsafe envelope or reply-slot optimization. |
| 4 | Mutation testing | **Done** (#192): weekly in CI; surviving mutants in supervision/system code were triaged into tests or documented. Original scope: `cargo-mutants` over `supervision.rs`, `system.rs` and lifecycle code; surviving mutants become missing tests. Periodic, not per-PR. |

### Step 2: release and run in a client

| Rank | Idea | Assessment |
|---|---|---|
| 5 | Release v0.7 and run it in Ethlambda | **Include the per-method request timeout (rank 5a below) and a migration guide for the clients' hand-wired shutdown and startup (see [client sweep](#client-sweep-production-pain)).** The roadmap's validation gate. A full-client replay or same-node canary outranks further synthetic research. Local EngineClient probes are a different evidence level. |
| 5a | Per-method request timeout in `#[protocol]` | **Implemented, unreleased:** protocol/method Duration defaults and a configured concrete-handle override, retaining five seconds by default. Existing fakes remain compatible. See the [migration guide](docs/MIGRATION.md#protocol-request-timeouts). |
| 5b | An example for every way of using the library | Runnable examples with assertions for every user-facing feature, in both execution modes where it exists in both. The previous stream/recipient/timeout/native-lifecycle/registration/testing gaps are now covered below. Tracked in [examples to add](#examples-to-add); each new capability ships with its example. |

### Step 3: cheap developer-experience track (in parallel)

Small, independent of the scheduler and lifecycle design, and visible to users.

| Rank | Idea | Assessment |
|---|---|---|
| 6 | Test kit with `ActorRef::mock()` | **Client evidence:** hand-stubbed protocols in ethlambda tests. Probe recipients ("sent X within 100 ms"), closure-backed mock actors so code keeps concrete `ActorRef<B>` fields, controllable time for timers. Needs an `Envelope` hook that downcasts to the concrete message and yields message and reply sender. Unmocked messages fail loudly. |
| 7 | Request deadlock detection | **Client evidence:** requests awaited inside handlers exist (ethrex peer table), no cycle found; keep as a cheap safety net.  `ActorError::WouldDeadlock` naming the cycle instead of a timeout. `ActorRef::request` takes only `&self`, so set a thread-local `CURRENT_ACTOR` only while the actor's own handler future is polled (and around the synchronous handler in thread mode); detached helpers create no edges. Keep a small wait set per actor for `join!`. Measure cost before enabling outside debug builds. |
| 8 | Stall watchdog | **Client evidence:** wedged and runtime-starving handlers in both clients (see sweep). Upgrade the debug-only `WarnOnBlocking` (whole-loop type name, 10 ms per poll, Async backend only): name the actor and message type, separate "one poll blocked a worker" from "handler stuck across awaits", optional in release builds. Long handler time alone does not establish deadlock. |
| 9 | Per-actor flight recorder | Last N handled message types and timestamps per actor, dumped on panic, stall or timeout. Bounded and opt-in; a building block for rank 22. |
| 10 | Published state snapshots (`Projected<S>`) | **Client evidence:** ethrex actors await read requests to `PeerTableServer` (see results above); its read-only queries are the first use.  Single-writer, many-reader snapshot slot so readers do not wait behind a long handler; replaces the `Arc<RwLock<_>>` escape hatch. Document that a snapshot can lag messages already sent (no read-your-writes). |

### Step 4: request semantics

| Rank | Idea | Assessment |
|---|---|---|
| 11 | Deferred typed replies, forwarding and pipe-to-self | Highest-value capability after the foundation; confirmed by the client search: ethrex hand-builds deferred replies with timeouts (`outgoing_request`/`current_requests`) and ethlambda hand-builds bounded pipe-to-self with exactly-one completion. Include reply timeouts and exactly-one completion on panic. Distinguish delegation, deferred reply ownership and completion messages; bound outstanding promises, propagate deadlines and define late-result/owner-exit behavior without general reentrancy. `ctx.spawn_ordered` (parallel stateless compute, then commit in arrival order, revalidating state at commit) is a follow-on specialization. |
| 12 | Scoped, lifecycle-owned registries | **Client evidence:** ethrex's per-peer connection map and its `ConnectionId` guard (#7435). Keyed registration that owns the actor's lifetime, with generations so an old cleanup cannot remove its replacement. Also scope the registry to an `ActorSystem` instead of the process-global static, so parallel tests and multi-node simulations do not share names. Prerequisite for groups/pub-sub. |
| 12a | Dynamic supervisor (`DynamicSupervisor` / `simple_one_for_one`) | **Client evidence:** ethrex starts one connection actor per peer at runtime and cleans it up by hand (see [per-key actor maps](#client-search-results)); today's `Supervisor` only owns the fixed child list declared at build time. Proposed contract: children are started at runtime from one `ChildSpec` template with per-call arguments, sharing its restart mode (Elixir-style heterogeneous specs only if a user needs them). The restart budget is supervisor-wide, as in OTP: exhausting it fails the supervisor and escalates to its parent, so a flapping child cannot restart forever. `max_children` counts restarting children, so a new start cannot take a slot mid-restart, and rejects with a typed error rather than queueing. `start_child` after shutdown begins is rejected, never dropped or leaked. Explicitly stopping a child does not restart it. Shutdown stops all live children concurrently under one supervisor deadline (OTP does not order dynamic children; sequential stops would multiply per-child timeouts). Usable inside a nested tree. Independent of rank 12 (`start_child` returns a handle); when combined, a restarted child re-registers under a new generation so an old cleanup cannot remove it. Planned after v0.7, not part of it; extend the TLA+ model (start/stop racing restart and shutdown, cap including restarting children) before release, as for OneForAll/RestForOne. The other OTP variants stay out: `Task.Supervisor` folds into ranks 11/15, `PartitionSupervisor` is sharding, and significant children/`auto_shutdown` wait for a user. |
| 13 | Coordinated shutdown and bounded draining | Ordered stop exists; add explicit draining as a policy: close admission, handle queued work by policy, join helpers and report unfinished work. Keep current stop/discard as the default. |
| 14 | Separate fair control path | Part of supervision/shutdown design; defer a generic public priority API. Non-trapping actors already receive abnormal exits through cancellation; queued `ExitEnvelope` only matters for trapping actors. Specify boundedness and per-sender ordering for startup, monitor and trapped-exit events. Priority cannot preempt a running handler or starve user work. |
| 15 | Actor-owned helper completion scopes | **Client evidence:** ethlambda hand-owns its aggregation worker; add cancel-previous, generation fencing and join-with-deadline. Ownership design gate for shutdown, partly addressed by group-owned helpers. Gate new helper registration, cancel, then explicitly join; helpers depending on their stopped owner can deadlock. |
| 16 | Bounded stashing / postponement | Demoted by the client search: the clients' parking structures are keyed by dependency and evicted by domain rules, which a generic in-order stash would not replace. After rank 11, as a reply-ownership variant. Stash operates on the envelope, not the message, so the reply sender is kept. Requires a size limit and drop policy (a peer flooding future-slot blocks must not grow memory) and `ActorStopped` for stashed requests on stop. Unrestricted selective receive is not planned. |

### Step 5: admission, credits and deadlines (one design package)

| Rank | Idea | Assessment |
|---|---|---|
| 17 | Bounded mailboxes and admission | **Production evidence:** ethrex's 1.5 GB/day leak from unbounded per-connection mailboxes. Opt-in alongside unbounded transport. Define reject/wait semantics, cancellation/deadlines and cyclic-send deadlocks. |
| 18 | Pre-allocation byte credits | Independently of optional metrics, bound allocated/in-flight bytes, not just queue slots. Credits can cover a whole pipeline; oversize requests must not wait forever. |
| 19 | Propagated budgets / stale-work skipping | Cheap stale-work rejection need not wait for a complete credit system. Opt-in expiry before dispatch, inherited monotonic deadlines and distinct outcomes. Timeout is not rollback; current requests still run after caller timeout/drop. |

### Step 6: the testing flagship

| Rank | Idea | Assessment |
|---|---|---|
| 20 | Actor-level ordering exploration (`spawned::sim::explore`) | Treat one handler turn as an atomic step and control delivery at the mailbox boundary; only deliveries racing at the same actor, timer or exit branch the schedule (actor-level partial-order reduction, as in P#/Coyote). Failures shrink to readable delivery lists and can print as sequence diagrams. Start here rather than full environment simulation. Shuttle stays an internal tool for framework races (see verification strategy). Handlers that use Tokio I/O, `spawn_blocking` or wall-clock time directly remain outside control. |
| 21 | Per-actor crash tape | Record one actor's inputs and replay its crash in a unit test. Inputs include replies to its own requests, clock reads and I/O, and replay needs a state snapshot at the tape start (or a tape since `started()`); the last N messages into a fresh instance do not reproduce state. Does not reproduce cross-actor races. |
| 22 | Record in production, replay in simulation | Record ingress plus mailbox delivery order (`(actor_id, seq)` per dequeue) in a bounded black box, replay with enforced order, detect divergence with per-handler hashes. Needs ranks 20 and 21, `ctx.now()`/`ctx.rng()` discipline in client code and DB snapshots. Go/no-go: replay a reintroduced known race every time. Highest ceiling; not started before 20 and 21. |

### When step 0 or a user justifies them

| Rank | Idea | Assessment |
|---|---|---|
| — | OneForAll/RestForOne and nested supervisor specs | Implemented for v0.7; expand the formal model before claiming these strategies formally verified. |
| — | Dedicated/pinned execution groups | Implemented: fixed native actor slots sharing one helper runtime, and parked Linux pinned shards. Client placement changes still need measurements. |
| — | Genuinely Tokio-free thread builds | Implemented with an isolated dependency graph and CI. Both clients still require Tokio; this is a separate native build capability. |
| 26 | Virtual (per-key) actors, in-process | **Client search:** ethrex keeps a per-peer actor map, but peers are created by dial/accept rather than a first message, so the keyed registry (rank 12) is the better fit first.  Address by key; created on first message, stopped when idle, recreated on demand, with activation/deactivation hooks, one live instance per key, ordered handoff and reload after crash. No storage or distribution layer. Promote if step 0 finds hand-written per-key actor maps; Orleans-style distributed placement stays at rank 44. |
| 27 | Process groups, typed topics and routers | After rank 12. Membership cleanup, per-recipient backpressure and partial-failure semantics matter more than a broadcast convenience method. Pools with round-robin, least-busy or key-hash routing (key hash preserves per-key order) belong here. Broadcast can share one payload behind an `Arc` instead of cloning it per member; each mailbox still needs its own queue entry, so this saves payload copies, not per-recipient allocation. |
| 28 | Causal tracing and ingress context | Preserve span context across awaits/fan-out and bound retention/sampling cost. |
| 29 | Bounded role/cohort metrics, oldest age, active monitor counts | Bound cohort/role cardinality and define gauge lifetime; avoid actor-ID label growth. Exact queue age needs a transport contract independent of sampled timing. |
| 30 | Read-only runtime inspection | Safe status/last-known snapshots: id, type, parent/links, mailbox depth, idle/polling/suspended status, current message type, turn age, wait-for edges. Actor state `Debug` only between turns, because the handler future holds `&mut A` across awaits. |
| 31 | Handler middleware (Tower-style) | Tracing, rate limits, timeouts, retries and dedup without handler edits; deadlines and tracing could be middleware rather than core special cases. |
| 32 | State-machine behavior | `gen_statem`-style per-state handlers and timeouts, built on bounded stashing (rank 16). |
| 33 | Typed streams and demand-driven pipelines | **Client evidence:** unbounded and lag-dropping `spawn_listener` inputs in both clients; promote with credits. After credits. Specify subscription ownership, slow consumers, failure, credit release and bounded completion-watermark windows. Ownership release differs from explicit acknowledgement. Start with one concrete producer/consumer. |
| 34 | Per-handler blocking-worker leasing | Prototype for mostly idle blocking actors. Preserve serial turns and runtime/I/O ownership; measure worker capacity and long-handler progress. |
| 35 | Batch handlers / generated batch dispatch | Promising for actual I/O/crypto batching; no guaranteed win. Preserve per-message replies, failure/drop semantics and ordering across heterogeneous message types. |
| 36 | Explicit coalescing / overload policies | For obsolete progress/state notifications, with bounded keys. Requests cannot silently disappear; superseded requests get an explicit error. Single-flight sharing of identical in-flight reads is a related opt-in, only for pure reads. |
| 37 | Generated closed-protocol dispatch | After profiling framework cost. Today a request allocates a reply channel, a boxed envelope and a boxed handler future. Enum dispatch needs codegen/transport bridging while retaining `Handler`/`Recipient` support; payload and reply allocations remain separate costs. Reusable reply slots only if profiles show they matter. |
| 38 | Timer drift / missed-tick policies | **Promoted by client evidence:** both clients hand-roll fixed-delay, aligned, skip and pause behavior; build after step 4. Fixed-delay/rate, Skip/Delay/Burst and cancellation. Preserve current timer behavior unless users opt in. |
| 39 | CPU service classes / weighted resource groups | Requires a workload and a clear fairness contract. Elapsed handler time includes awaits and is not CPU accounting. |
| 40 | Ready-made integrations | `Recipient<M>` as a stream sink, actors behind Axum/Tonic as Tower services, a connection-owning actor with reconnect/backoff. Build on demand. |
| 41 | AI-agent example | One example showing supervision, timeouts, cancellation and tracing for long-running agents. Positioning, not a library change. |

### Research or separate products

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

### Parallel validation track

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
contracts in conformance fixtures. See [testing](docs/TESTING.md).

Backend research (schedulers, queues, affinity, I/O engines) stays frozen until a
client profile supplies a concrete reason.

### Corrections to the proposed descriptions

- "Stage, then commit" handlers (agy review): mutate `self` only after the last
  fallible `.await`. Spawned does not cancel a running handler, and a panic
  discards the instance under supervision, so this is a documentation guideline
  for actors whose state outlives a failed handler, not a framework feature.
- An independent ranking (agy, 2026-11-09) listed 27 ideas as missing; it compared
  the pre-consolidation documents. All but three were already ranked here; the
  three (shared-payload broadcast, planned-restart state handoff, stage-then-commit
  handlers) are folded into ranks 27 and 23 and the note above.

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

## Examples to add

Every user-facing feature should have a small runnable example with assertions,
in both execution modes where the feature exists in both, showing its useful
behavior and its important failure/overload limit. Prefer a program with
assertions over a snippet that only compiles, and do not present speculative API
names as working code. The [examples index](examples/README.md) lists what
exists today. Experimental backend drivers and their evidence stay under
`benchmarks/` and [legacy](legacy/README.md).

### Release example coverage

The former rank 5b gaps now have asserted examples on unreleased main:

| Feature | Runnable example |
|---|---|
| Stream listeners and `Recipient<M>` | [Streams and recipients](examples/capabilities/src/bin/streams_and_recipients.rs), both modes, including a stopped recipient and cooperative iterator cancellation. |
| Request timeout configuration/outcomes | [Protocol timeouts](examples/capabilities/src/bin/protocol_timeouts.rs), both modes; [timeout outcomes](examples/capabilities/src/bin/timeout_outcomes.rs) for effects before replies. |
| Native monitors, links, trapped exits and linked startup | [Native lifecycle](examples/capabilities/src/bin/native_lifecycle.rs); [thread-only graph](examples/thread_only/README.md). |
| Name registration and protocol fakes | [Registrations and fakes](examples/capabilities/src/bin/registrations_and_fakes.rs), managed replacement and explicit manual cleanup. |
| Migrating client shutdown | [Coordinated shutdown](examples/capabilities/src/bin/coordinated_shutdown.rs), with producer join and snapshot before dependency closure. |

### Capability examples for planned work

Each ranked capability ships with its example. Links point at partial recipes
that already run today.

| Rank | Capability | Example to deliver with it |
|---|---|---|
| 6 | Test kit | Unit-test an actor that holds a concrete `ActorRef<B>` by mocking `B`. |
| 7 | Deadlock detection | Two actors requesting each other fail fast with the named cycle. |
| 8 | Stall watchdog | A handler blocking a worker versus one stuck across an `.await`, each reported with actor and message type. |
| 10 | Published snapshots | Readers keep answering from a snapshot while the owner runs a long handler. |
| 11 | Deferred replies / delegation / pipe-to-self | Route a slow request while accepting another message; bound pending replies and demonstrate owner exit and late completion. |
| 12 | Lifecycle-owned registries (managed generation guards implemented) | Register a service, crash it and verify its entry is removed without deleting a replacement's registration. |
| 12a | Dynamic supervisor | Start one supervised actor per incoming connection; crash a restartable child and see only it restart; stop one explicitly and see it stay stopped; hit `max_children` and get a rejection; then shut down with every live child stopped within the deadline. |
| 13 | Coordinated shutdown | Close admission, drain dependencies within a deadline and report unfinished work. [Finite-batch recipe works today](examples/capabilities/src/bin/graceful_drain.rs). |
| 14 | Fair control path | Observe lifecycle control behind a data backlog while checking ordinary work still progresses; no handler preemption. |
| 15 | Actor-owned helpers (managed scope implemented) | Stop an actor with active helpers, reject new helper admission and await owned cleanup to a deadline. |
| 16, 32 | State machines / bounded stashing | Queue a bounded number of messages while connecting; un-stash on readiness and reject overflow. |
| 17 | Bounded mailboxes | A slow consumer reaches capacity; demonstrate explicit rejection and cancellable waiting without blocking actor progress. |
| 18 | Byte/work credits | Reserve before decoding a large payload; pass credit across stages and reclaim it after rejection/panic. [Payload-budget adapter works today](examples/capabilities/src/bin/byte_budget_pipeline.rs). |
| 5a, 19 | Inherited deadlines | Exhaust an ingress budget and skip stale queued work without granting downstream calls fresh time. [Current timeout behavior](examples/capabilities/src/bin/timeout_outcomes.rs). |
| — | OneForAll/RestForOne and nested supervisor specs | Implemented for v0.7; expand the formal model before claiming these strategies formally verified. |
| — | Dedicated/pinned execution groups | Implemented: fixed native actor slots sharing one helper runtime, and parked Linux pinned shards. Client placement changes still need measurements. |
| — | Genuinely Tokio-free thread builds | Implemented with an isolated dependency graph and CI. Both clients still require Tokio; this is a separate native build capability. |
| 26 | Virtual actors | Activate/deactivate a logical identity and demonstrate replacement, persistence and concurrent-activation policy. |
| 27 | Groups / topics / routers | Join workers, route requests, remove crashed members and report partial broadcast failures. |
| 28 | Causal tracing | Follow one ingress through fan-out/fan-in, preserving origin across awaits and runtime boundaries. |
| 29 | Bounded cohort metrics | Separate busy importers from idle peers with a bounded label set and correct gauge cleanup. [Aggregate metrics work today](examples/capabilities/src/bin/framework_metrics.rs). |
| 30 | Read-only inspection | Inspect last-known status and a bounded redacted history while a handler is busy. |
| 33 | Typed streams / completion watermarks | Pull a bounded stream, stall an early item, then show released ownership differs from explicit successful acknowledgement. |
| 34 | Blocking worker leasing | Thousands of mostly idle actors share a bounded blocking pool while each actor stays serial. |
| 35 | Batching | Batch database writes up to a count/time budget; preserve documented ordering and individual reply/failure accounting. |
| 36 | Coalescing | Replace obsolete head notifications using bounded keys; demonstrate that transactional requests are never silently coalesced. |
| 37 | Generated protocol dispatch | Keep the same caller behavior across generated and manual handlers; measure allocations separately from useful work. |
| 38 | Timer policies | Pause a consumer, then compare Skip/Delay/Burst and cancellation against a controlled clock. |
| 39 | CPU service classes | Saturate a bulk class and show a critical class's measured service share under a fixed CPU budget. |
| 42 | Delay-based shedding | Compare simple admission and CoDel-inspired shedding on explicitly droppable messages; report losses and queue delay. |
| 43 | External flight recorder | Read a hung fixture's bounded snapshot/events from a separate process; show sequence gaps and redaction. |
| 44 | Distribution | Authenticate peers, reject incompatible wire versions and demonstrate a partition's ambiguous request outcome. |
| 46 | Sparse-work driver yield | Replay sparse bursts; compare wake latency and idle CPU, including a missed-wakeup check. |
| 47 | Session types | Demonstrate a valid two-party conversation and a compile-fail invalid sequence, with the proof's limits stated. |
| 48 | Fair join patterns | Match votes by round with bounded unmatched storage, cancellation and competing ready matches. |
| 49 | Durable outbox / idempotency | Crash after committing intent, retry and deduplicate effects using application storage; no arbitrary-envelope persistence. |
| 50 | Durable reminders | Restart a process and demonstrate missed-tick and duplicate-delivery policy against persisted state. |
| 51 | Durable workflows | Replay persisted decisions after a crash and distinguish replayed decisions from external effects. |

Full deterministic environment simulation remains research. Existing controlled
handler-progress replays, failure windows and request-history checks are in
[`concurrency/tests`](concurrency/tests) and documented in
[testing](docs/TESTING.md); they do not implement the capabilities above.
