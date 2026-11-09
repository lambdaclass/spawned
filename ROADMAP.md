# Spawned roadmap

Updated 2026-11-09. This is the single planning document: the phases below give
the implementation order in brief, and the [ranked backlog](#ranked-backlog)
holds the detail, tradeoffs and client evidence behind it. It is an order, not
a release schedule, and no speculative feature is promised by a date.
Maintained [research notes](docs/research/README.md) (virtual actors,
systematic testing, Ethereum client testing, request semantics, other
concurrency models, verification tools, observability, actor testing, the Rust
actor landscape, supervision models, deadlock and stall detection, test
techniques, event sourcing) give the prior art and sources behind several items.

Business handlers stay ordinary Rust. The framework owns dispatch and lifecycle.
Keep typed messages/replies, dynamic actors, addressed delivery, cooperative
handlers and the existing default behavior. New policies are explicit and opt-in.

## Where we are

- **v0.6.0 (released):** typed protocols, async/blocking/thread execution, actor
  identities, monitors, links and trapped exits, linked startup, shutdown fixes,
  optional framework metrics. No automatic supervision. Thread actors need no
  active Tokio executor but still compile Tokio.
- **v0.7.0:** `ActorSystem` with async/native/pinned execution groups, Tokio-free
  synchronous builds, and nested OneForOne/OneForAll/RestForOne supervision (see [Implemented](#implemented-no-longer-ranked)), a
  bounded dedicated-thread runtime, and the supervision verification of phase 2.
  See the [guide](docs/GUIDE.md#execution-groups-and-supervision) and the
  [design contract](docs/design/execution-groups-supervision.md).

The v0.7.1 fix branch is validating canary shutdown, destructor-panic and restart
budget fixes. Already-erased protocol timeout configuration remains a separate
API follow-up: configure the concrete handle before `to_x_ref()` today. Native
calls block before returning a Response, so a generic wrapper around an already
returned response cannot correctly apply that timeout. Preserve handwritten
protocol/forwarding compatibility when designing an explicit erased dispatch API.

## North star

Spawned aims to be the best-designed and best-tested actor framework. This list
is an index of the pillars, so no thread is lost; the detail lives in the linked
sections and [research notes](docs/research/README.md).

- **OTP-grade supervision and lifecycle:** [implemented](#implemented-no-longer-ranked) strategies, then ranks 12, 12a, 13, 14 ([supervision models](docs/research/supervision-models.md), [Rust actor landscape](docs/research/rust-actor-landscape.md)).
- **Request semantics:** deferred replies and pipe-to-self (11, 11a), compute pool (11b), cancel scopes (15a), admission and deadlines (17-19) ([request semantics](docs/research/request-semantics-references.md), [concurrency models](docs/research/concurrency-models.md)).
- **Fairness and placement:** execution groups (implemented), stall watchdog (8), cooperative budgets (8a), deadlock detection (7) ([deadlock and stall detection](docs/research/deadlock-and-stall-detection.md)).
- **Observability:** metrics (29), `spawned-top` and inspection (30), causal tracing (28), flight recorder (9, 43) ([observability](docs/research/observability.md)).
- **Verification:** TLA+ with trace replay under the [verification rule](#verification-rule), production monitors (5c), Kani, Loom/Shuttle, sanitizers, mutants ([verification tools](docs/research/verification-tools.md), [systematic testing](docs/research/systematic-testing.md)).
- **Deterministic testing:** [`spawned-test`](#testing-crate-spawned-test) (6), failpoints, the [simulation backend spike](#simulation-backend-spike), then `explore` (20), replay (21-22) and environment simulation (22a) ([actor testing](docs/research/actor-testing.md), [test techniques](docs/research/test-techniques.md)).
- **Client validation:** Ethlambda and Ethrex canaries (rank 5, [step 2](#step-2-release-and-run-in-a-client)), with umbrella PRs [lambdaclass/ethlambda#783](https://github.com/lambdaclass/ethlambda/pull/783) and [lambdaclass/ethrex#7518](https://github.com/lambdaclass/ethrex/pull/7518) ([Ethereum testing](docs/research/ethereum-testing.md)).
- **Durability (research):** ranks 49-51 ([event sourcing](docs/research/event-sourcing.md), [virtual actors](docs/research/virtual-actors.md)).

## Phases

1. **Execution groups plus supervision: implemented.** Includes fixed native actor
   pools, parked Linux pinned shards, nested supervisor specs, OneForAll and
   RestForOne. Tokio-free synchronous builds are also implemented. All are
   available in v0.7.0; examples and resource limits are in the guide.
2. **v0.7 verification: completed.** Retain the OneForOne checks alongside
   cohort selection/barrier/supersession and nested shutdown ownership models,
   replay real Rust traces and require negative controls to fail. These bounded
   projections complement failure/stress tests, review and ThreadSanitizer;
   they do not prove the wider base lifecycle or every Rust schedule.
   All crates retain `forbid(unsafe_code)`; Miri precedes any future unsafe proposal.
   See the [implementation evidence](legacy/docs/reviews/capabilities-extension.md)
   and [v0.7 review fixes and formal coverage](legacy/docs/reviews/v07-supervision.md).
   The client search is done; see [results](#client-search-results).
3. **Adopt v0.7 in Ethlambda** (rank 5), including the per-method
   request timeout (rank 5a), the migration guide for the clients' hand-wired
   shutdown and startup, and an example for every way of using the library
   (rank 5b, [examples to add](#examples-to-add)). Review fixes are implemented: `Debug` and
   custom-supertrait protocols keep compiling; the API gate covers defaults,
   metrics and the explicit-async migration, and audits the exact documented
   no-default removals. Conditional `request_timeout` attributes are explicitly
   rejected, the API script supports Python 3.9, stream examples use handshakes,
   and dated reports/evidence are archived in `legacy/` without host-specific
   driver scripts. Both clients pin spawned 0.5.0, so their
   upgrade crosses 0.6 as well; the migration guide covers 0.5 to 0.7 in one
   path. v0.7.0 is released; migrate Ethlambda (shutdown
   deadline via `ActorSystem`, supervision, per-method timeouts) with fault
   injection, then Ethrex (racy Ctrl-C shutdown, `ActorStopped` exits).
   Once the canary runs, check its lifecycle traces with production monitors
   derived from the TLA+ invariants (rank 5c, [PObserve-style](docs/research/systematic-testing.md#a-pobserve-for-spawned)).
   In parallel, the developer-experience track (ranks 6-10: test kit, whose
   phase A started the `spawned-test` crate, deadlock
   detection, stall watchdog with cooperative budgets and per-turn work
   measurement (rank 8a), flight recorder, published snapshots), motivated
   by the [client sweep](#client-sweep-production-pain).
4. **Database-grade testing, before new capabilities.** Test Spawned the way
   FoundationDB, TigerBeetle, Dropbox, etcd and AWS test their systems, starting
   now and running alongside the Ethlambda canary. It blocks phase 5 (request
   semantics), not the release or the canary. Schedule, batches and exit
   criterion: [database-grade testing program](#database-grade-testing-program).
5. **Request semantics:** deferred replies and pipe-to-self (ranks 11-16), with
   the dynamic supervisor (rank 12a) alongside the keyed registry, with
   promise pipelining and transactional turns as design inputs (rank 11a,
   [references](docs/research/request-semantics-references.md)); the managed
   compute pool (rank 11b) as the first user of rank 11's completion contract;
   cancel scopes (rank 15a, [concurrency models](docs/research/concurrency-models.md))
   as the one ownership design behind draining, helpers and deadlines; then
   admission, credits and deadlines as one design package (ranks 17-19). Each
   ships under the [verification rule](#verification-rule). Before starting,
   re-rank phases 5-7 with what the Ethlambda and Ethrex migrations taught,
   as rank 0 did with the client search.
6. **The testing flagship (ranks 20-22), in the [`spawned-test`](#testing-crate-spawned-test) crate:**
   ordering exploration at the actor level (Coyote-style controlled delivery), per-actor crash tapes, then record
   in production and replay in simulation. `explore` builds on the
   [simulation backend spike](#simulation-backend-spike) of phase 4 (a
   deterministic `spawned-rt::tasks` backend), adding delivery control at the
   mailbox boundary on top of its seeded executor. After phase 5, because exploration
   and replay need well-defined reply, registry and mailbox semantics. The
   go/no-go: a reintroduced known race reproduces every time. Then
   environment simulation (rank 22a): turmoil/madsim-style network, disk and
   clock faults from the same seed; multi-client runs belong on ethshadow or
   Attacknet ([Ethereum testing](docs/research/ethereum-testing.md)). Prior art:
   [systematic testing](docs/research/systematic-testing.md).
7. **Needs-driven (ranks 23-41):** scaling patterns (virtual actors, groups,
   streams), observability, ergonomics (middleware, `gen_statem`-style
   behavior, batch handlers), load policies and integrations. Nothing here is
   scheduled; each item needs a client or user first.
8. **Research or separate products (ranks 42-54):** distribution, durability
   (outbox, durable timers, Temporal-style execution, with Restate as the
   reference design; see [virtual actors](docs/research/virtual-actors.md#what-it-means-for-spawned);
   event sourcing as in Akka Persistence and Orleans `JournaledGrain`, see
   [event sourcing](docs/research/event-sourcing.md))
   and language-level ideas (session types and choreographies, ranks 47-47a).
   Research pointers 52-54 (thread-per-core I/O, incremental dataflow, CRDTs
   and sagas) record other concurrency models and why they wait
   ([concurrency models](docs/research/concurrency-models.md)).
   Likely separate crates, only if someone builds a multi-node or durable system
   on Spawned.

A full Ethrex/Ethlambda replay or same-node canary remains the application
rollout validation gate.
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

**Production arm (rank 5c).** Trace validation checks test runs. After the
Ethlambda canary, the same invariants (`AtMostOneGeneration`,
`PublishedIsLive`, `BudgetBounded`, `ReverseShutdown`, and, once the base
lifecycle model exists, every accepted request answered or `ActorStopped`) run
as Rust monitors over the lifecycle trace stream: online in a live node with
bounded memory, reporting through `tracing` and metrics, and offline over
recorded traces in CI and canary runs. TLA+ stays the source of truth: a
monitor ships only if it flags every negative-control trace TLC rejects for its
property and passes every accepted trace. Design and limits:
[A PObserve for Spawned](docs/research/systematic-testing.md#a-pobserve-for-spawned).

| Needs a model | What the model must show |
|---|---|
| Dynamic supervisor (rank 12a) | Children starting and stopping while others restart or the supervisor shuts down; `max_children` is never exceeded, counting restarting children; no start succeeds after shutdown begins. |
| Deferred replies and pipe-to-self (rank 11) | Every accepted request gets exactly one reply, including on owner exit or panic; a late result never reaches a restarted instance. |
| Managed compute pool (rank 11b) | Every admitted job yields exactly one completion or cancellation outcome, including on panic and owner exit; no completion reaches a restarted instance or a superseded generation; in-flight jobs never exceed the pool budget. |
| Cancel scopes (rank 15a, with ranks 13, 15 and 19) | No child is admitted after its scope closes; every child ends or is reported unfinished by the scope's deadline; a child deadline is never later than its parent's; shielded cleanup cannot pass the parent's hard deadline. One model serves ranks 13, 15 and 19. |
| Keyed registry (rank 12) | An old entry's cleanup never removes its replacement (ethrex #7435). |
| Draining shutdown (rank 13) | Queued work is handled or reported; shutdown finishes within its deadline. |
| Bounded mailboxes and credits (ranks 17-18) | No credit leaks; waiting senders cannot deadlock. |
| Base actor lifecycle | Links, monitors, `Down` exactly once, every request answered or `ActorStopped`; reintroducing the PR #182/#185 bugs must fail ([next model](verification/tla/README.md#next-model)). |
| Production monitors (rank 5c) | No new model: each monitor is checked against an existing model's accepted traces and negative controls. |

No model needed (tests, examples and review suffice): the per-method timeout
(5a), examples (5b), the migration guide, metrics, snapshots (10), the test kit
(6) and macro work. Deadlock detection (7), the stall watchdog (8) and
cooperative budgets (8a, which changes scheduling between turns, not lifecycle)
are optional; their risk is false alarms or unfairness, which tests catch better.

**Every fixed bug keeps a reproducing test.** A bug fix lands with a test that
fails on the code before the fix (as the PR #166/#168/#182/#185 regressions and
the supervision gate races do), and TESTING.md lists it. Where the bug is a
protocol race, its trace also becomes a negative control for the model.

### Testing to add

Existing layers are listed in [TESTING.md](docs/TESTING.md): unit and
integration tests, proptest scenarios, trybuild, Loom, ThreadSanitizer,
cargo-mutants and TLA+ with trace replay. Release gates and remaining gaps:

| When | Test | Why |
|---|---|---|
| v0.7: implemented | `cargo-semver-checks` and a `cargo-hack --each-feature` build matrix | Defaults/metrics and explicit-async migration checks, exact declared no-default breaks, plus each feature without dev-dependency unification. See [testing](docs/TESTING.md#api-and-feature-checks). |
| v0.7: implemented | Supervisor policies under proptest | Generated crash/exit/shutdown scripts for all strategies and modes, using zero backoff and an unexhausted budget; budget/backoff races have separate controlled tests. [Scope and replay](docs/TESTING.md#generated-supervisor-policy-scripts). |
| Client rollout: pending | Fault injection in the Ethlambda canary | Kill and restart supervised actors in a running client and check it recovers; the application rollout gate (rank 5) should exercise failures, not just the happy path. |
| After v0.7 | Shuttle for multi-actor framework races | Randomised schedules over whole supervisor/registry tests too large for Loom. Listed in TESTING.md, not started. Internal tool, separate from rank 20. |
| After v0.7 | Long-running soak tests | Memory, file descriptors, threads and tasks stay flat over hours of actor churn and restarts. The ethrex 1.5 GB/day mailbox leak is the motivating bug. Nightly, not per PR. |
| With ranks 11-13 | Loom on the new primitives | Reply slots, registry compare-and-remove and mailbox publication versus stop. |
| Ongoing | Coverage report (`cargo-llvm-cov`) | Find untested branches in supervision and system code; pairs with the surviving mutants. Report only, no threshold. |
| After v0.7 | Porcupine-style linearizability checker for `request_histories` | The current oracle is a bounded search (16 operations, 100,000 states). Partition histories per key (P-compositionality), memoize search states and render a failing history, so longer histories from proptest, soak and canary runs can be checked. See [Porcupine](docs/research/systematic-testing.md#history-checkers). PropEr's `parallel_commands` (sequential prefix, then two parallel lists checked for a valid serialization) is prior art for generating the histories. |
| With rank 5c | Offline lifecycle monitors over canary traces | The Ethlambda canary writes lifecycle traces; monitors check them after each run without a JVM. |
| After v0.7 | Test cases generated from the TLA+ models | The half of MongoDB's eXtreme Modelling study that worked (100% branch coverage): enumerate interesting model states (crash during backoff while shutdown starts, restart racing a deadline) and emit each as a concrete Rust test. Complements trace checking, which only judges runs we happened to write. |
| After v0.7 | Cancellation-safety tests | Drop every public future (`request`, `join`, `shutdown`, `restart_child`, group starts) at each of its await points in turn and check framework state stays consistent. Dropping halfway is the commonest async Rust bug; nothing tests it systematically. Method: poll each future N times under `tokio_test::task::spawn` with the clock paused, drop it, let the system settle, check invariants; the failpoint `Yield` action reaches awaits that would not otherwise suspend; document each public future's cancel safety in rustdoc ([test techniques](docs/research/test-techniques.md#cancellation-safety-in-async-rust)). |
| After v0.7 | Panic-safety tests | Inject a panic into each user callback in turn (`started`, `stopped`, handlers, factories, a message's `Drop`) and check registries, links, monitors and supervisors stay consistent. Today only handler panics are covered. |
| After v0.7 | Leak tests and LeakSanitizer | After shutdown every actor, task, thread and registry entry is gone and `ActorRef` strong counts reach zero (reference cycles between actors are the classic leak). LeakSanitizer job alongside TSAN. |
| v0.7 | Single-CPU runs | Run the suite with one worker thread and under `taskset` on one core; exposes hidden ordering and starvation assumptions cheaply. |
| After v0.7 | Kani proofs of small functions | Bounded proofs of no overflow/panic and stated properties for backoff math, the restart-budget window and generation counters. `Backoff::delay` mutants survived the mutation run. Harness properties for any input: no panic or overflow; delay at most `maximum` and `initial*2^failures`; jittered delay never above the unjittered bound; the window allows at most `max_restarts` and drops entries older than the window; generations never repeat. Each harness must fail on a known mutant before it is trusted ([verification tools](docs/research/verification-tools.md)). |
| After v0.7 | Cross-backend (metamorphic) testing | Run one workload on async, blocking, thread, native-pool and pinned backends; observable results (replies, exit reasons, ordering per sender) must match. Catches backend-specific bugs without a hand-written oracle. More relations: an unrelated idle actor or supervisor changes no outcome; splitting a group under the same total budget leaves replies unchanged; zero versus small backoff gives the same restart count while the budget lasts; results match with and without `metrics`/`lifecycle-trace` ([test techniques](docs/research/test-techniques.md)). |
| After v0.7 | Failpoints (`BUGGIFY`-style) | Named fault points in Spawned's own lifecycle code (fail or delay during restart, shutdown, registration), compiled out by default and switchable from tests and `spawned-test`. FoundationDB and etcd use them to reach rare paths on purpose. |
| After v0.7 | Clock-jump tests | Paused time, large forward/backward wall-clock jumps and slot-aligned ticks; needed before timer policies (rank 38) and for clients that align to wall-clock slots. |
| With ranks 8, 8a, 14 | Fairness and starvation tests | Under load no actor waits beyond a bound, and lifecycle/control messages keep moving behind a data backlog. On one CPU, a busy actor cannot delay another actor's message beyond a stated bound (rank 8a). |
| After v0.7 | Allocation budgets (dhat) | Fail when handling one message allocates more than a set count; keeps per-message cost from regressing silently. One integration-test file per budget using dhat's testing mode (`Profiler::builder().testing()`, `dhat::assert!`), release build, blocks per message after warm-up; the baseline rank 37 must beat ([test techniques](docs/research/test-techniques.md)). |
| v0.7 | Supply-chain checks | `cargo-deny`, `cargo-audit` (and later `cargo-vet`) in CI. |
| After v0.7 | Deterministic performance tests | Benchmarks whose results do not depend on hardware noise (simulated time or instruction counts), as TigerBeetle does, so performance changes are comparable across runs and machines. |
| Ongoing | Flaky-test tracking | Record reruns and quarantine flaky concurrency tests instead of retrying them green. |
| After v0.7 | Platform matrix | Linux and macOS, x86 and ARM, several core counts; matters most for pinned shards. Add `ubuntu-24.04-arm` and `macos-latest` jobs for the default and failpoint suites (free for public repositories); pinned shards and TSAN stay on Linux x86 and arm64 ([test techniques](docs/research/test-techniques.md)). |
| After v0.7 | Open-loop load generation | Benchmarks issue load on a schedule, not after each reply, so tail latency is not understated (coordinated omission). A timer-driven fixed-rate driver in `benchmarks/` recording latency from the scheduled send time into HdrHistogram (wrk2's correction); used by the rank 8a and rank 17 acceptance tests ([test techniques](docs/research/test-techniques.md)). |

Deterministic simulation and record/replay remain the testing flagship (ranks
20-22), followed by environment simulation (rank 22a). Prior art and how
databases combine these techniques: [systematic testing](docs/research/systematic-testing.md).

The [archived v0.7 gate review](legacy/docs/reviews/v07-release-gates.md) records
the original scans and their limits. The corrected [testing guide](docs/TESTING.md)
covers the expanded API profiles and protocol/supervision regressions.

### Database-grade testing program

Every technique the databases in [systematic testing](docs/research/systematic-testing.md#how-databases-and-storage-systems-test)
use has a Spawned equivalent. Most need no new features, so they run before
request semantics: the v0.7 review rounds kept finding real races, and phase 5
builds on this lifecycle code. Two techniques need groundwork first, so phase 5
adds the hooks for them.

| Batch | When | Techniques | Database precedent |
|---|---|---|---|
| 1 | Now (days) | Single-CPU CI runs; `cargo-deny`/`cargo-audit`; LeakSanitizer job plus leak tests (actors, tasks, threads, registry entries and `ActorRef` counts back to zero after shutdown); reproducing-test rule in the PR template; flaky-test tracking | Sanitizers, etcd's bug-per-test practice |
| 1 | Now | Kani proofs for backoff and jitter, the restart-budget window and generation counters (closes the surviving `Backoff::delay` and jitter mutants); TLA+ stays the tool for protocols ([verification tools](docs/research/verification-tools.md)) | AWS (Kani on Firecracker) |
| 1 | Now | Failpoints (`BUGGIFY`-style) in lifecycle code: restart, shutdown, registration, startup; compiled out by default, switchable from tests and `spawned-test` | FoundationDB, etcd (gofail) |
| 1 | Now | Cross-backend metamorphic tests; cancellation-safety tests (drop each public future at each await point); panic-safety tests (panic in each user callback) | CockroachDB/Pebble, ShardStore |
| 2 | Next (1-2 weeks) | Shuttle on supervisor and system tests; tests generated from the TLA+ models; Porcupine-style history checker; reference models for registry, links and monitors | ShardStore, MongoDB (test-case generation), etcd (Porcupine) |
| 2 | Next | [Simulation backend spike for `spawned-rt`](#simulation-backend-spike): a cfg-switched deterministic `spawned-rt::tasks` backend (one thread, virtual time, seeded choice of the next ready task) running unmodified async actors; a known supervision race must replay from its seed | FoundationDB (simulated runtime), madsim/RisingWave, S2 |
| 2 | Next | Nightly volume: a dedicated machine runs proptests, Shuttle, failpoint and cross-backend suites over fresh seeds every night; every failure prints its seed and becomes a reproducing test | Dropbox (tens of millions of runs nightly), TigerBeetle (simulators 24/7) |
| 3 | With the Ethlambda canary | Fault injection on fede256 with `spawned-test`; soak runs with resource budgets; offline lifecycle monitors over the canary's traces (rank 5c); Antithesis pilot if budgeted | AWS PObserve, TigerBeetle soak, etcd on Antithesis |
| 4 | During phase 5 | Simulation hooks designed in: time, randomness and delivery behind one controllable interface, for deferred replies, registries and mailboxes from the start | FoundationDB, Dropbox Nucleus (designed for simulation) |
| 5 | Phase 6 | `explore` (rank 20), crash tapes and replay (21-22), environment simulation (22a), protocol-aware invariants checked inside the simulator | FoundationDB, TigerBeetle VOPR, Dropbox Trinity |
| — | Later | Deterministic performance tests; allocation budgets; platform matrix; open-loop load | TigerBeetle |

**Exit criterion for phase 4:** batches 1 and 2 run in CI and nightly; the
Ethlambda canary has run on fede256 for an agreed period with faults injected;
every bug found keeps a reproducing test; no open finding from the nightly runs.
Phase 5 starts after that, with batch 4 as part of its design.

#### Simulation backend spike

A time-boxed spike in batch 2, and the first step toward `explore` (rank 20).
Goal: run unmodified async actors, supervisors and execution groups under a
deterministic `spawned-rt::tasks` backend selected by a cfg flag (as madsim's
`--cfg madsim` does), with one executor thread, virtual time that advances
only when every task is idle, and a seeded choice of which ready task to poll
next.

- **The seam is one module.** `spawned-concurrency` reaches Tokio only through
  `spawned_rt::tasks`: `rt/src/tasks/mod.rs:23-32` re-exports `mpsc`,
  `oneshot`, `sleep`, `timeout`, `watch`, `CancellationToken`, `spawn`,
  `spawn_blocking`, `block_in_place`, `Handle`, `JoinHandle`, `Runtime` and
  the stream wrappers (from `rt/src/tasks/tokio/mod.rs:4-12`), and
  `rt/src/tasks/mod.rs:37-50` builds the worker and control runtimes that
  execution groups use (`concurrency/src/system/group.rs:258-261`). Tokio is a
  dev-dependency of `spawned-concurrency` only (`concurrency/Cargo.toml:17-20`):
  measured 2026-11-09, its direct `tokio::` uses are 21 in 6 files, all test
  code (9 in 5 files outside `link_regression_tests.rs`, which has 12):
  `time::timeout`, `task::yield_now`, `sync::oneshot`, `sync::broadcast`,
  `join!`, `runtime::Builder` and `#[tokio::test]`. Those tests would move to `spawned_rt` to run
  under the simulator; library code needs no change.
- **Build or borrow.** Evaluate (a) madsim's Tokio shim (`madsim-tokio`, a
  drop-in package rename that switches under `--cfg madsim`), which also needs
  `tokio-util`'s `CancellationToken` and `tokio-stream` wrappers to work under
  it (not verified), against (b) a small custom executor behind the same
  re-exports: a run queue, a timer wheel on virtual time, and channels from
  `futures` or our own. (b) keeps the seam ours and gives `explore` direct
  access to the choice of next task; (a) is less code if its coverage
  suffices. Commonware's deterministic runtime, already cited in
  `rt/src/tasks/mod.rs:4-5`, is a third reference.
- **Acceptance.** A known supervision race replays deterministically from its
  seed: the failpoints PR #204 finding that a never-started child's terminal
  state is `Stopped` or `Failed` depending on whether the supervisor's
  `finish_unstarted` sweep or the controller's `finish(failed)` runs last.
  The same seed gives the same outcome and the same lifecycle trace every run;
  some seed gives each outcome. (A fallback target: the PR #166 link
  post-drain window from [TESTING.md](docs/TESTING.md#bugs-reproduced-by-these-regressions).)
- **Limits, stated and detected.** `Backend::Thread` and `Backend::Blocking`,
  native and pinned execution groups, `spawn_blocking`/`block_in_place`,
  owner threads (`std::thread`), direct Tokio I/O and the wall clock
  (`Instant::now`, `SystemTime`) stay outside control. The tester must detect
  and report each one it meets (for example, a `spawn_blocking` call or a
  real-thread wakeup under the sim cfg fails the run naming the call site),
  never silently lose determinism. The spike reports how much of the
  existing async suite runs unchanged.

Prior art: madsim (RisingWave), FoundationDB's simulated runtime and S2's DST
([systematic testing](docs/research/systematic-testing.md)); Concuerror for
Erlang ([actor testing](docs/research/actor-testing.md)). Rank 20 then adds
delivery control at the mailbox boundary on top of this executor.

The same program applies to the clients: ethrex's plan is
[lambdaclass/ethrex#7518](https://github.com/lambdaclass/ethrex/pull/7518);
Ethlambda's follows its upgrade branch.

### Testing crate (`spawned-test`)

User-facing testing tools live in one crate in this workspace,
[`testing/`](testing/) (`spawned-test`, unreleased), used as a
dev-dependency (as `tokio-test` is to Tokio), not in a separate repository and
not as a framework for any actor library. It needs hooks at Spawned's mailboxes,
dispatch, timers, exits and restarts, which must change together with the core;
a library-agnostic tool would have to rebuild those internals or drop to thread
scheduling, which Loom and Shuttle already cover. A separate crate keeps test
code out of production builds and can make breaking releases while the core API
stays stable.

| In the core (small, behind a feature or cfg) | In `spawned-test` |
|---|---|
| Hook points at delivery, timers, exits and restarts that a controlled scheduler can take over | Test kit and actor mocks (rank 6; phase A exists); later a synchronous behavior test kit and manual time ([actor testing](docs/research/actor-testing.md)) |
| Lifecycle trace recording (`lifecycle-trace`, exists) | Fault injection: kill or restart an actor, delay a message, fire a timeout (phase A exists) |
| Deadlock detection and the stall watchdog (ranks 7-8): they protect production | Ordering exploration, `explore` (rank 20) |
| | Crash tapes and production record/replay (ranks 21-22) |
| | Environment simulation: network, disk and clock faults (rank 22a) |
| Production lifecycle monitors (rank 5c): they run in live nodes | Offline monitor runner over recorded traces (rank 5c) |
| | Helpers to check recorded traces against the TLA+ models |

The TLA+ models and checkers stay in `verification/`; they verify Spawned's own
design, not user code.

Order: built now, not after v0.7. The crate exists with phase A of the test kit (rank 6): mocks,
probes, bounded harness waits and fault injection through public APIs only,
ready for the Ethlambda canary. Next, the concrete-`ActorRef<B>` mock hook and
controllable timer time if a client needs them. `explore` and replay (ranks
20-22) come later, in phase 6. Do not build the controlled scheduler before
phase 5 settles reply and mailbox semantics.

How the tools compare:

| Tool | Explores | Use in Spawned |
|---|---|---|
| Loom | Every thread interleaving, exhaustively | Small primitives (cancellation, ready gate) |
| Shuttle | Random thread schedules | Framework-internal races (after v0.7) |
| Coyote (Microsoft, formerly P#) / rank 20 | Orderings of message deliveries, with seeded failures and timers and replayable traces | User-level actor bugs in whole applications |
| TLA+ | A model of the design, not the code | Lifecycle protocols, under the [verification rule](#verification-rule) |
| Stateright | States of a Rust actor model that can also run on a real network | Prior art for rank 20; it owns its runtime, so not a dependency |
| turmoil / madsim | Seeded network, clock (and with turmoil, filesystem) faults in one thread | Pattern for rank 22a |
| PObserve (P) | Spec monitors over service logs, in tests or production | Pattern for rank 5c, with TLA+-derived Rust monitors |

Survey and sources: [systematic testing](docs/research/systematic-testing.md).

Coyote takes over scheduling at the points that matter, searches orderings with
strategies such as PCT, injects timeouts and failures, and records every
failing run for replay. Spawned already owns those points, so `explore` needs no
handler changes. Ranks 21-22 add what Coyote does not centre on: capturing
delivery order from a real client and replaying it under the same scheduler.

The [v0.7 gate review](legacy/docs/reviews/v07-release-gates.md) records the completed
API/feature scans, generated-policy campaign and compatibility correction.

## Ranked backlog

Rankings are engineering judgment, not measured speedup predictions or release
commitments. They indicate value and sequence of investigation, not a strictly
serial design schedule. Runtime ownership, helper completion, shutdown and
control-event contracts must be designed together before their public APIs are
frozen. No item below changes released behavior by appearing here.

Maintained [research notes](docs/research/README.md) cover
[virtual actors](docs/research/virtual-actors.md),
[systematic testing](docs/research/systematic-testing.md),
[Ethereum client testing](docs/research/ethereum-testing.md),
[request semantics references](docs/research/request-semantics-references.md),
[other concurrency models](docs/research/concurrency-models.md),
[verification tools](docs/research/verification-tools.md),
[observability](docs/research/observability.md),
[actor testing](docs/research/actor-testing.md),
[the Rust actor landscape](docs/research/rust-actor-landscape.md),
[supervision models](docs/research/supervision-models.md),
[deadlock and stall detection](docs/research/deadlock-and-stall-detection.md),
[test techniques](docs/research/test-techniques.md) and
[event sourcing](docs/research/event-sourcing.md).
Older background notes (sources, design notes and experiments, without rankings of
their own) are archived in [legacy](legacy/README.md):
[framework ideas](legacy/docs/research/framework-ideas.md),
[concurrency ideas](legacy/docs/research/concurrency-ideas.md),
[Commonware](legacy/docs/research/commonware.md),
[Silver/flux](legacy/docs/research/silver-flux.md),
[verification strategy](legacy/docs/research/verification-strategy.md) and the
earlier [detailed acceptance criteria](legacy/docs/design/roadmap-acceptance-2026-11-06.md).

### Implemented (no longer ranked)

| Idea | Released capability |
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
| Test doubles | Ethlambda stubs an entire protocol by hand three times in tests; ethrex tests spawn real actors with dummy contexts. | Test kit with `ActorRef::mock()` (rank 6); phase A in `spawned-test`. |
| Cyclic wiring after spawn | Actors that reference each other are wired with `InitP2P`/`InitBlockChain` messages, forcing `Option<Ref>` fields. | **New:** late binding or two-phase start for actor cycles (with rank 12's keyed registry). |
| Self-only messages in macros | Ten `#[allow(dead_code)] // invoked via send_after` on one protocol, plus ~18 manual `impl Handler` blocks. | **New, small:** private/self-only messages in `#[actor]`/`#[protocol]`. |
| Hand-built observability | Ethlambda threads `Instant` stamps through messages to measure mailbox wait; ethrex polls the peer table every second for counts and checks actor health with an ordinary request (inferred: it times out while a blocking actor works); ethlambda's metadata is a hand-built `Arc<RwLock>` snapshot. | v0.6 metrics already measure queue wait (adoption, rank 5); out-of-band health/inspection (rank 30); published snapshots (rank 10). |
| Stream lag dropped silently | `BroadcastStream` `Lagged` errors are discarded before `spawn_listener`; another site hand-writes a `select!` loop to log lag. | Lag-aware listeners in typed streams (rank 33). |
| Exit reasons outside actors | A swarm adapter cannot tell an actor panic from a stop; nothing restarts the swarm when it ends. 29 ignored sends in ethrex. | Monitors/supervision for non-actor tasks; documented guidance on ignored sends. |
| CPU work offloaded by hand | Ethrex builds four rayon pools (global, a 17-thread merkle pool that deadlocks if smaller, prewarm, BAL warming), about 30 parallel call sites and 50 `spawn_blocking` calls, mostly outside actors; per-connection L2 imports blocked Tokio workers waiting on the shared merkle pool and deadlocked at 0% CPU ([535647e45](https://github.com/lambdaclass/ethrex/commit/535647e45)). Ethlambda hand-owns aggregation on `spawn_blocking` (token, session id, join timeout) and gossip checks behind five semaphores with `catch_unwind`. Counts and file links: [concurrency models](docs/research/concurrency-models.md#who-uses-it). | **New, rank 11b:** managed compute pool with results as messages, bounded admission, owner-tied cancellation and panic-as-message. |
| Singleton worker built outside spawned | ethrex's L2 block importer uses `Arc<Mutex<BTreeMap>>`, `Notify` and a raw task because `submit` must return a dedup result synchronously; it has no shutdown path. | **New, small:** a synchronous `try_admit`/keyed coalescing mailbox, or document the pattern with an owned helper. |

### Step 1: verify what just landed

Preserve this verification before a release and expand its scope for the new
cohort/nested semantics. Restart and shutdown races are the bug class behind
PRs #180, #182 and #185. Plan and acceptance criteria: [verification strategy](legacy/docs/research/verification-strategy.md).

| Rank | Idea | Assessment |
|---|---|---|
| 1 | Formal lifecycle model | **Supervision done** ([#192](https://github.com/lambdaclass/spawned/pull/192), `verification/tla`). Remaining: the base lifecycle (links, monitors, trapped exits, stop). Original scope: TLA+ (or P/Stateright) model of exit reasons, monitors, links, trapped exits, stop and the implemented OneForOne contract. Reintroducing the #182/#185 bugs in the model must produce counterexamples. Also answers per-sender ordering between a dying actor's messages and its exit/`Down`. |
| 2 | Trace validation | **Supervision done** (#192, `lifecycle-trace` feature); extend with rank 1. Original scope: Test-only lifecycle events from the Rust code, checked against the model on scenario, churn and supervision failure tests. Ties the spec to the implementation. |
| 3 | Miri and ThreadSanitizer in CI | **Done differently** (#192): ThreadSanitizer runs in CI; all crates are `#![forbid(unsafe_code)]`, so Miri waits until unsafe code is proposed. Original note: Cheap. Required before any unsafe envelope or reply-slot optimization. **Research:** Kani covers safe arithmetic now (phase 4, batch 1); Miri, then Kani on the unsafe blocks, comes in only when unsafe code is proposed ([verification tools](docs/research/verification-tools.md)). |
| 4 | Mutation testing | **Done** (#192): weekly in CI; surviving mutants in supervision/system code were triaged into tests or documented. Original scope: `cargo-mutants` over `supervision.rs`, `system.rs` and lifecycle code; surviving mutants become missing tests. Periodic, not per-PR. |

### Step 2: release and run in a client

| Rank | Idea | Assessment |
|---|---|---|
| 5 | Adopt v0.7 in Ethlambda | **v0.7.0 includes per-method request timeouts (rank 5a below) and the migration guide for the clients' hand-wired shutdown and startup (see [client sweep](#client-sweep-production-pain)).** Client rollout validation is still pending. A full-client replay or same-node canary outranks further synthetic research. Local EngineClient probes are a different evidence level. |
| 5a | Per-method request timeout in `#[protocol]` | **Available in v0.7.0:** protocol/method Duration defaults and a configured concrete-handle override, retaining five seconds by default. Existing fakes remain compatible. See the [migration guide](docs/MIGRATION.md#protocol-request-timeouts). |
| 5b | An example for every way of using the library | Runnable examples with assertions for every user-facing feature, in both execution modes where it exists in both. The previous stream/recipient/timeout/native-lifecycle/registration/testing gaps are now covered below. Tracked in [examples to add](#examples-to-add); each new capability ships with its example. |
| 5c | PObserve-style production lifecycle monitors | **After the Ethlambda canary runs.** Extends TLA+ trace validation (rank 2) from test runs to live nodes. Rust state machines derived from the TLA+ invariants consume the lifecycle trace stream online with bounded memory (state per live supervisor or actor, a fixed cap with a counted overflow) and report violations via `tracing` and metrics without stopping the node; the same monitors run offline over recorded traces in CI and on fede256 canary runs. Needs a production trace sink (bounded, per-entity ordering, counted gaps) instead of the test recorder's global section and unbounded buffer, and request events defined by the base lifecycle model. TLA+ stays the source of truth; each monitor must agree with TLC on the negative controls. See [A PObserve for Spawned](docs/research/systematic-testing.md#a-pobserve-for-spawned). |

### Step 3: cheap developer-experience track (in parallel)

Small, independent of the scheduler and lifecycle design, and visible to users.

| Rank | Idea | Assessment |
|---|---|---|
| 6 | Test kit with `ActorRef::mock()` | **Phase A implemented** in [`spawned-test`](#testing-crate-spawned-test) (unreleased): closure-backed `Mock` actors that implement `Handler<M>` for every message, so `ActorRef<Mock>` satisfies any `#[protocol]` and replaces hand-stubbed protocol refs with no production change; recording `Probe<M>` recipients (`expect`, `expect_no_message`); bounded harness waits; fault injection (`faults::exit`, `restart_child`, mock delay/discard/crash). Unmocked messages fail loudly. **Remaining:** mocking a field typed as a concrete `ActorRef<B>` needs an `Envelope` hook that downcasts to the concrete message and yields message and reply sender (a core change, deferred); controllable time for timers; a generic idle check; core `ChildHandle::exit(reason)` would make injected exits exact (today `Panic` and `Kill` go through a linked helper, so a panic surfaces as `Panic("panic in handler: ...")`). **Client evidence:** hand-stubbed protocols in ethlambda tests (`RecordingChain` in `column_checks.rs`, also `envelope_checks.rs`, `verdict.rs`). **Research ([actor testing](docs/research/actor-testing.md)):** three gaps against Akka/Erlang kits. (1) A synchronous behavior test kit (Akka `BehaviorTestKit`): run one handler with a test `Context` and record its effects (sends, spawns, timers, stop) without a runtime; needs a core change. (2) Manual time (Akka `ManualTime`): document Tokio's `start_paused` for async actors; thread actors need the framework-owned clock, the same seam as the [simulation backend spike](#simulation-backend-spike) and `ctx.now()`, so build one clock, not two. (3) Concrete `ActorRef<B>` mocking stays deferred (actix's `Mocker` is the precedent); until then users hold protocol refs or `Recipient`s. Also: the idle check should report total deadlock Go-`checkdead` style (every actor idle, a request outstanding, no timer pending), and a replay-equivalence check for event-sourced actors ([event sourcing](docs/research/event-sourcing.md)). |
| 7 | Request deadlock detection | **Client evidence:** requests awaited inside handlers exist (ethrex peer table), no cycle found; keep as a cheap safety net.  `ActorError::WouldDeadlock` naming the cycle instead of a timeout. `ActorRef::request` takes only `&self`, so set a thread-local `CURRENT_ACTOR` only while the actor's own handler future is polled (and around the synchronous handler in thread mode); detached helpers create no edges. Keep a small wait set per actor for `join!`. Measure cost before enabling outside debug builds. **Approach chosen ([deadlock and stall detection](docs/research/deadlock-and-stall-detection.md)):** eager detection. Add a wait-for edge when a handler sends a request, walk the per-actor wait sets for a cycle, fail the request that would close it with `WouldDeadlock` naming the cycle, remove the edge on reply or timeout; a request to self fails at once (Erlang's `calling_self`). Waits on raw channels, locks or `spawn_blocking` create no edges, so it is a safety net, not a proof. Edges are exported to rank 30. |
| 8 | Stall watchdog | **Client evidence:** wedged and runtime-starving handlers in both clients (see sweep). Upgrade the debug-only `WarnOnBlocking` (whole-loop type name, 10 ms per poll, Async backend only): name the actor and message type, separate "one poll blocked a worker" from "handler stuck across awaits", optional in release builds. Long handler time alone does not establish deadlock. **Research ([deadlock and stall detection](docs/research/deadlock-and-stall-detection.md)):** three detectors: per-poll timing inside dispatch with actor id and message type (a poll blocking a worker), turn age (a handler stuck across awaits), and an Akka-style scheduling-delay probe per execution group (starvation). Tokio's `tokio_unstable` task hooks are optional behind a feature, not required. |
| 8a | Cooperative budgets and per-turn work measurement | **Client evidence:** the same incidents as rank 8 (ethrex d958dcba4 filled every runtime worker; ethlambda moved its chain actor to `Backend::Thread`). Spawned cannot preempt Rust code; do what the BEAM does for native code instead. (A) Fairness between messages: an opt-in dispatch policy that yields after N messages or T µs per poll (today's private `MessageTurn` experiment), framework operations that consume Tokio's coop budget, and per-actor work metrics (poll time per message, polls per turn, long polls) feeding ranks 8, 29 and 39. (B) Opt-in `ctx.checkpoint().await` and a budgeted iterator that yield only when the turn is over budget. (C) Synchronous code is detected (rank 8) and isolated (placement, rank 11b), never preempted. Rejected: auto-instrumentation by macro, signal-based preemption. Acceptance: single-CPU starvation bound; watchdog fires on a blocking handler. See [preemption and budgets](docs/research/concurrency-models.md#3-preemption-reduction-counting-and-cooperative-budgets). Per-turn work measurement uses the same instrumentation point as rank 8. |
| 9 | Per-actor flight recorder | Last N handled message types and timestamps per actor, dumped on panic, stall or timeout. Bounded and opt-in; a building block for rank 22. **Research ([observability](docs/research/observability.md)):** the minimal record per message is type, enqueue time, start time and outcome, bounded per actor and readable by `spawned-top` (rank 30). |
| 10 | Published state snapshots (`Projected<S>`) | **Client evidence:** ethrex actors await read requests to `PeerTableServer` (see results above); its read-only queries are the first use.  Single-writer, many-reader snapshot slot so readers do not wait behind a long handler; replaces the `Arc<RwLock<_>>` escape hatch. Document that a snapshot can lag messages already sent (no read-your-writes). |

### Step 4: request semantics

| Rank | Idea | Assessment |
|---|---|---|
| 11 | Deferred typed replies, forwarding and pipe-to-self | Highest-value capability after the foundation; confirmed by the client search: ethrex hand-builds deferred replies with timeouts (`outgoing_request`/`current_requests`) and ethlambda hand-builds bounded pipe-to-self with exactly-one completion. Include reply timeouts and exactly-one completion on panic. Distinguish delegation, deferred reply ownership and completion messages; bound outstanding promises, propagate deadlines and define late-result/owner-exit behavior without general reentrancy. `ctx.spawn_ordered` (parallel stateless compute, then commit in arrival order, revalidating state at commit) is a follow-on specialization. |
| 11a | Design inputs to rank 11: promise pipelining and transactional turns | Decide within the rank 11 design, not as a separate API: (1) whether a pipelined send ("on reply to R, send M to the target R names") is expressible without an extra handler turn, with M failing like R rather than being dropped (E/Cap'n Proto/Goblins pipelining; the main latency tool if rank 44 ever ships); (2) whether deferred replies and completions are sent at the end of a turn or immediately, and an opt-in Goblins-style transactional turn that holds a handler's sends until it returns and discards them on panic (state rollback only for snapshot-able state; external effects are never rolled back); (3) whether per-turn snapshots can serve ranks 21 and 30 too. The rank 11 TLA+ model must cover a reply sent by a turn that then panics. See [request semantics references](docs/research/request-semantics-references.md); STM's "effects only on commit" is the same idea restricted to one actor, while its `retry` is selective receive and stays out ([STM note](docs/research/concurrency-models.md#5-software-transactional-memory)). |
| 11b | Managed compute pool (CPU offload with pipe-to-self) | **Client evidence:** ethrex's four hand-built rayon pools, 50 `spawn_blocking` calls and the L2 import deadlock at 0% CPU (535647e45); synchronous cell serving that occupied every runtime worker (d958dcba4); ethlambda's hand-owned aggregation worker and semaphore-bounded, `catch_unwind`-guarded gossip checks ([evidence](docs/research/concurrency-models.md#who-uses-it)). One bounded CPU pool per `ActorSystem` or group, never on Tokio workers; `ctx.compute(job, map)` (name illustrative) delivers the result to the submitting actor as a message; per-actor and per-pool admission caps that reject at once or follow rank 17; cancellation tied to owner stop, restart and scope close (rank 15a) with generation fencing; a panic becomes an error message, exactly once; metrics for queue depth, wait, run time and rejections. Document that jobs must not wait on their own pool and gang jobs need a dedicated pool. Rayon behind a feature first; a fixed native pool as the alternative. Built on rank 11's completion contract; shares admission design with rank 34. Prior art: Akka dispatchers, Orleans `Task.Run`, Ray tasks, BEAM dirty schedulers ([concurrency models](docs/research/concurrency-models.md#1-cpu-parallel-compute-pools-integrated-with-actors)). |
| 12 | Scoped, lifecycle-owned registries | **Client evidence:** ethrex's per-peer connection map and its `ConnectionId` guard (#7435). Keyed registration that owns the actor's lifetime, with generations so an old cleanup cannot remove its replacement. Also scope the registry to an `ActorSystem` instead of the process-global static, so parallel tests and multi-node simulations do not share names. Prerequisite for groups/pub-sub. Orleans keeps the same one-live-instance-per-key invariant ([virtual actors](docs/research/virtual-actors.md#what-it-means-for-spawned)). |
| 12a | Dynamic supervisor (`DynamicSupervisor` / `simple_one_for_one`) | **Client evidence:** ethrex starts one connection actor per peer at runtime and cleans it up by hand (see [per-key actor maps](#client-search-results)); today's `Supervisor` only owns the fixed child list declared at build time. Proposed contract: children are started at runtime from one `ChildSpec` template with per-call arguments, sharing its restart mode (Elixir-style heterogeneous specs only if a user needs them). The restart budget is supervisor-wide, as in OTP: exhausting it fails the supervisor and escalates to its parent, so a flapping child cannot restart forever. `max_children` counts restarting children, so a new start cannot take a slot mid-restart, and rejects with a typed error rather than queueing. `start_child` after shutdown begins is rejected, never dropped or leaked. Explicitly stopping a child does not restart it. Shutdown stops all live children concurrently under one supervisor deadline (OTP does not order dynamic children; sequential stops would multiply per-child timeouts). Usable inside a nested tree. Independent of rank 12 (`start_child` returns a handle); when combined, a restarted child re-registers under a new generation so an old cleanup cannot remove it. Planned after v0.7, not part of it; extend the TLA+ model (start/stop racing restart and shutdown, cap including restarting children) before release, as for OneForAll/RestForOne. The other OTP variants stay out: `Task.Supervisor` folds into ranks 11/15, `PartitionSupervisor` is sharding, and significant children/`auto_shutdown` wait for a user. **Research ([supervision models](docs/research/supervision-models.md)):** counting restarting children against `max_children` goes beyond Elixir; add `terminate_child` (stop without restart; today an individually stopped Permanent child is restarted) and consider it on static supervisors too. Significant children/`auto_shutdown` would be a trigger for draining (rank 13). Document why Spawned's default budget (5 restarts in 30 s) is looser than OTP's (1 in 5 s): per-child backoff already spreads restarts. Non-goals: an Akka-style Resume directive (keep state after a failure) and buffering messages during backoff, because references are instance-bound. |
| 13 | Coordinated shutdown and bounded draining | Ordered stop exists; add explicit draining as a policy: close admission, handle queued work by policy, join helpers and report unfinished work. Standalone scopes currently use reverse admission order; explore declared dependencies with cycle/unknown-edge rejection and model-checked deadline propagation. Keep current stop/discard as the default. Design with rank 15a: draining is closing the actor's scope under its deadline. Keep "stuck cleanup fails the tree" instead of OTP's `brutal_kill`; add only reporting of unfinished work ([supervision models](docs/research/supervision-models.md)). |
| 14 | Separate fair control path | Part of supervision/shutdown design; defer a generic public priority API. Non-trapping actors already receive abnormal exits through cancellation; queued `ExitEnvelope` only matters for trapping actors. Specify boundedness and per-sender ordering for startup, monitor and trapped-exit events. Priority cannot preempt a running handler or starve user work. |
| 15 | Actor-owned helper completion scopes | **Client evidence:** ethlambda hand-owns its aggregation worker; add cancel-previous, generation fencing and join-with-deadline. Managed helpers require ActorSystem; explore standalone ownership without detached fallback or changing actor-join guarantees implicitly. Gate new helper registration, cancel, then explicitly join; helpers depending on their stopped owner can deadlock. |
| 15a | Design input to ranks 13, 15 and 19: cancel scopes with deadlines | Decide within those designs, not as a separate nursery API: one ownership tree (system, group, supervisor, actor, turn or request), each node a scope with close (stop admission), cooperative cancel with shielded cleanup, join with deadline that reports unfinished children, and deadline inheritance into outbound requests and compute jobs. Draining (13) closes the actor scope; helpers (15) are its children and cancel-previous closes a sub-scope; propagated budgets (19) are scope deadlines. Scopes own `'static` tasks and close explicitly, since `Drop` cannot await. One TLA+ model for all three. Prior art: Trio nurseries and cancel scopes, Swift task groups, Kotlin scopes, Java `StructuredTaskScope` (still preview in JDK 27), Tokio `TaskTracker` with child tokens ([structured concurrency](docs/research/concurrency-models.md#2-structured-concurrency-and-cancel-scopes)). |
| 16 | Bounded stashing / postponement | Demoted by the client search: the clients' parking structures are keyed by dependency and evicted by domain rules, which a generic in-order stash would not replace. After rank 11, as a reply-ownership variant. Stash operates on the envelope, not the message, so the reply sender is kept. Requires a size limit and drop policy (a peer flooding future-slot blocks must not grow memory) and `ActorStopped` for stashed requests on stop. Unrestricted selective receive is not planned. |

### Step 5: admission, credits and deadlines (one design package)

| Rank | Idea | Assessment |
|---|---|---|
| 17 | Bounded mailboxes and admission | **Production evidence:** ethrex's 1.5 GB/day leak from unbounded per-connection mailboxes. Opt-in alongside unbounded transport. Define reject/wait semantics, cancellation/deadlines and cyclic-send deadlocks. |
| 18 | Pre-allocation byte credits | Independently of optional metrics, bound allocated/in-flight bytes, not just queue slots. Credits can cover a whole pipeline; oversize requests must not wait forever. |
| 19 | Propagated budgets / stale-work skipping | Cheap stale-work rejection need not wait for a complete credit system. Opt-in expiry before dispatch, inherited monotonic deadlines and distinct outcomes. Timeout is not rollback; current requests still run after caller timeout/drop. Inherited deadlines are scope deadlines (rank 15a). |

### Step 6: the testing flagship

| Rank | Idea | Assessment |
|---|---|---|
| 20 | Actor-level ordering exploration (`spawned::sim::explore`) | **First step:** the [simulation backend spike](#simulation-backend-spike) (phase 4, batch 2), a deterministic `spawned-rt::tasks` executor with virtual time and seeded task choice; `explore` adds delivery control on top. Treat one handler turn as an atomic step and control delivery at the mailbox boundary; only deliveries racing at the same actor, timer or exit branch the schedule (actor-level partial-order reduction, as in P#/Coyote). Failures shrink to readable delivery lists and can print as sequence diagrams. Start here rather than full environment simulation. Shuttle stays an internal tool for framework races (see verification strategy). Handlers that use Tokio I/O, `spawn_blocking` or wall-clock time directly remain outside control. Prior art: Coyote/P# for the design; Stateright (Rust actors that are model-checked and also run on a real network) and Dropbox's Trinity (a Rust engine under a custom test executor) for the Rust side ([systematic testing](docs/research/systematic-testing.md)); Concuerror, a stateless model checker that systematically explores Erlang process interleavings, for actor-level systematic testing in an OTP language ([actor testing](docs/research/actor-testing.md)). Report "all actors idle with a request outstanding" as a deadlock, as Concuerror reports a stuck receive; Concuerror's single-node modelling of distributed Erlang is prior art for rank 22a. |
| 21 | Per-actor crash tape | Record one actor's inputs and replay its crash in a unit test. Inputs include replies to its own requests, clock reads and I/O, and replay needs a state snapshot at the tape start (or a tape since `started()`); the last N messages into a fresh instance do not reproduce state. Does not reproduce cross-actor races. Share the snapshot interface with event-sourced recovery; keep tape and event formats separate (a hypothesis, [event sourcing](docs/research/event-sourcing.md)). |
| 22 | Record in production, replay in simulation | Record ingress plus mailbox delivery order (`(actor_id, seq)` per dequeue) in a bounded black box, replay with enforced order, detect divergence with per-handler hashes. Needs ranks 20 and 21, `ctx.now()`/`ctx.rng()` discipline in client code and DB snapshots. Go/no-go: replay a reintroduced known race every time. Highest ceiling; not started before 20 and 21. |
| 22a | Environment simulation in `spawned-test` | Extends phase 6 after ranks 20-22: turmoil/madsim-style simulated network (partition, delay, hold), disk (failed or slow writes) and clock (jumps, skew) driven by the same seed as `explore`, so a test can fail a peer's connection or a write at a chosen delivery. Prefer adapting turmoil (tokio-rs, `tokio::net`-shaped) to owning a simulator; only I/O routed through the simulated layer is controlled. Multi-client network runs stay outside the crate, on ethshadow (deterministic, real binaries) or Attacknet (Chaos Mesh on Kurtosis devnets), see [Ethereum testing](docs/research/ethereum-testing.md). Same-seed reruns should produce identical traces (S2's check). |

### When step 0 or a user justifies them

| Rank | Idea | Assessment |
|---|---|---|
| — | OneForAll/RestForOne and nested supervisor specs | Available in v0.7.0 with bounded cohort/nested models, real trace replay and negative controls; wider runtime guarantees remain outside those projections. |
| — | Dedicated/pinned execution groups | Implemented: fixed native actor slots sharing one helper runtime, and parked Linux pinned shards. Client placement changes still need measurements. |
| — | Genuinely Tokio-free thread builds | Implemented with an isolated dependency graph and CI. Both clients still require Tokio; this is a separate native build capability. |
| 26 | Virtual (per-key) actors, in-process | **Client search:** ethrex keeps a per-peer actor map, but peers are created by dial/accept rather than a first message, so the keyed registry (rank 12) is the better fit first.  Address by key; created on first message, stopped when idle, recreated on demand, with activation/deactivation hooks, one live instance per key, ordered handoff and reload after crash. No storage or distribution layer. Promote if step 0 finds hand-written per-key actor maps; Orleans-style distributed placement stays at rank 44. Orleans, Akka sharding, Dapr and Restate compared in [virtual actors](docs/research/virtual-actors.md). |
| 27 | Process groups, typed topics and routers | After rank 12. Membership cleanup, per-recipient backpressure and partial-failure semantics matter more than a broadcast convenience method. Pools with round-robin, least-busy or key-hash routing (key hash preserves per-key order) belong here. Broadcast can share one payload behind an `Arc` instead of cloning it per member; each mailbox still needs its own queue entry, so this saves payload copies, not per-recipient allocation. Elixir's `PartitionSupervisor` is prior art for key-hash pools ([supervision models](docs/research/supervision-models.md)). |
| 28 | Causal tracing and ingress context | Preserve span context across awaits/fan-out and bound retention/sampling cost. **Research ([observability](docs/research/observability.md)):** follow the OpenTelemetry messaging conventions: capture the sender's context when a message is queued and carry it in the envelope; link the handler's span to it, and use parent-child only for `request`; sample at ingress. `tracing` spans do not cross a mailbox unless the envelope carries them. |
| 29 | Bounded role/cohort metrics, oldest age, active monitor counts | Bound cohort/role cardinality and define gauge lifetime; avoid actor-ID label growth. Exact queue age needs a transport contract independent of sampled timing. The Prometheus `_max` exposition bug found by the Ethlambda canary is fixed (maxima are separate gauge families, checked by `scripts/check-metrics-exposition.py` in CI). Next, add what Orleans exposes and Spawned lacks: a long-running-turn counter (fed by rank 8), started/stopped/failed counts per role and oldest-message age ([observability](docs/research/observability.md)). |
| 30 | Read-only runtime inspection | Safe status/last-known snapshots: id, type, parent/links, mailbox depth, idle/polling/suspended status, current message type, turn age, wait-for edges. Actor state `Debug` only between turns, because the handler future holds `&mut A` across awaits. A concrete first tool: `spawned-top`, an Erlang-observer-style live view that attaches to a running node and lists actors with mailbox depth, state, links and current message. **Research ([observability](docs/research/observability.md)):** take observer's columns and recon's safety rules: never copy a mailbox to inspect it, rank actors over a sliding window rather than totals, rate-limit any tracing, read state only between turns; show rank 7's wait-for edges as `jstack` prints lock cycles. kameo's console TUI is the nearest Rust design to compare against. Optional tokio-console integration (task names and a span per turn via `console-subscriber`, behind `tokio_unstable`) complements `spawned-top` but cannot see Thread or Blocking actors or mailboxes. |
| 31 | Handler middleware (Tower-style) | Tracing, rate limits, timeouts, retries and dedup without handler edits; deadlines and tracing could be middleware rather than core special cases. |
| 32 | State-machine behavior | `gen_statem`-style per-state handlers and timeouts, built on bounded stashing (rank 16). |
| 33 | Typed streams and demand-driven pipelines | **Client evidence:** unbounded and lag-dropping `spawn_listener` inputs in both clients; promote with credits. After credits. Specify subscription ownership, slow consumers, failure, credit release and bounded completion-watermark windows. Ownership release differs from explicit acknowledgement. Start with one concrete producer/consumer. Reactive Streams' `request(n)` demand is rank 18's credits; build on them rather than a second mechanism ([streams note](docs/research/concurrency-models.md#4-streams-with-backpressure)). |
| 34 | Per-handler blocking-worker leasing | Prototype for mostly idle blocking actors. Preserve serial turns and runtime/I/O ownership; measure worker capacity and long-handler progress. The I/O-blocking counterpart of the compute pool (rank 11b); share its admission design. |
| 35 | Batch handlers / generated batch dispatch | Promising for actual I/O/crypto batching; no guaranteed win. Preserve per-message replies, failure/drop semantics and ordering across heterogeneous message types. |
| 36 | Explicit coalescing / overload policies | For obsolete progress/state notifications, with bounded keys. Requests cannot silently disappear; superseded requests get an explicit error. Single-flight sharing of identical in-flight reads is a related opt-in, only for pure reads. |
| 37 | Generated closed-protocol dispatch | After profiling framework cost. Today a request allocates a reply channel, a boxed envelope and a boxed handler future. Enum dispatch needs codegen/transport bridging while retaining `Handler`/`Recipient` support; payload and reply allocations remain separate costs. Reusable reply slots only if profiles show they matter. |
| 38 | Timer drift / missed-tick policies | **Promoted by client evidence:** both clients hand-roll fixed-delay, aligned, skip and pause behavior; build after step 4. Fixed-delay/rate, Skip/Delay/Burst and cancellation. Preserve current timer behavior unless users opt in. |
| 39 | CPU service classes / weighted resource groups | Requires a workload and a clear fairness contract. Elapsed handler time includes awaits and is not CPU accounting. Would aggregate rank 8a's per-turn work metrics and could weight several compute pools (rank 11b). |
| 40 | Ready-made integrations | `Recipient<M>` as a stream sink, actors behind Axum/Tonic as Tower services, a connection-owning actor with reconnect/backoff. Build on demand. |
| 41 | AI-agent example | One example showing supervision, timeouts, cancellation and tracing for long-running agents. Positioning, not a library change. |

### Research or separate products

| Rank | Idea | Assessment |
|---|---|---|
| 42 | CoDel-inspired mailbox shedding | Isolated experiment for explicitly droppable traffic after admission/deadlines. Network CoDel constants are not actor defaults; sampled metrics timestamps are insufficient for its controller. Compare against simpler policies. |
| 43 | Out-of-process/shared-memory flight recorder | If real hangs defeat in-process inspection. A bounded versioned last-known snapshot is a smaller first step; a recorder also needs ordered events, sequence/gap reporting and loss accounting. |
| 44 | Distributed actors | Large optional layer needing wire/version/security/failure contracts. Virtual actors (rank 26) keep the addressing path open; Orleans' directory and placement are the reference ([virtual actors](docs/research/virtual-actors.md)), and promise pipelining (rank 11a) is the latency tool. Rust prior art: `ractor_cluster` and kameo's libp2p remote refs ([Rust actor landscape](docs/research/rust-actor-landscape.md)). |
| 45 | Compile-time actor graph | Macro-generated topology for diagrams and static request-cycle checks; hard with dynamic actors. |
| 46 | Sparse-work root-driver yield hypothesis | Private experiment when profiles show deferred-wake delay; not a default or backend. |
| 47 | Session-typed protocols | Opt-in experiment for a fixed two-party protocol. Does not prove deadlock freedom for arbitrary dynamic actors, blocking handlers or failing external systems. |
| 47a | Choreographic protocols | Next to rank 47: one global description of a fixed multi-party exchange, projected into each actor's typed messages and handlers (ChoRus in Rust, HasChor in Haskell), instead of session-typed channels. Same limits as rank 47. See [request semantics references](docs/research/request-semantics-references.md#choreographic-programming-chorus-haschor). |
| 48 | Fair join patterns | Until real coordination code needs them. Bound unmatched messages and define fairness, cancellation and interactions with ordinary handlers. Join calculus background and CSP `select` are in [concurrency models](docs/research/concurrency-models.md#9-briefly-csp-select-algebraic-effects-join-calculus). |
| 49 | Durable outbox/idempotency recipe | Application-layer example when recovery needs it. Commit intent/dedup with effects; do not persist arbitrary boxed Rust envelopes. **Research ([event sourcing](docs/research/event-sourcing.md)):** a recipe for an event-sourced actor: snapshot plus replay in `started()`, append with expected version, side effects after the append, idempotency key. Prior art: Akka Persistence (`EventSourcedBehavior`), Orleans `JournaledGrain`, coerce's persistence in Rust. |
| 50 | Durable timers/reminders | Separate persistence contract: identity, clock changes, missed executions and duplication across restart. Model reminders as events (timer set, timer fired) so replay re-arms unfired timers ([event sourcing](docs/research/event-sourcing.md)). |
| 51 | Temporal-style durable execution | A separate product-scale subsystem: persisted decisions, deterministic replay, external effects and upgrades. Supervision alone does not provide it. Event sourcing persists state changes; durable execution persists handler progress. Restate remains the reference ([event sourcing](docs/research/event-sourcing.md)). |
| 52 | Thread-per-core I/O backend (io_uring) | **Research pointer, frozen.** Seastar, Glommio, monoio and tokio-uring. Parked pinned shards already give per-core placement; the earlier Monoio io_uring/epoll experiments ([design findings](legacy/benchmarks/DESIGN-FINDINGS.md#what-we-learned-in-the-linux-vm)) did not justify replacing Tokio, and both clients use Tokio I/O throughout. Reopen only with a client I/O profile ([note](docs/research/concurrency-models.md#6-thread-per-core-and-io_uring)). |
| 53 | Incremental dataflow | **Research pointer.** Timely/differential dataflow (Materialize). A different programming model; read its progress tracking (frontiers) before designing rank 33's completion watermarks ([note](docs/research/concurrency-models.md#7-incremental-dataflow)). |
| 54 | CRDT-replicated state, sagas and process managers | **Research pointer**, distribution/durability tier. CRDTs only with multi-node replicas (rank 44); a crash-surviving process manager is durable execution (ranks 49-51). A saga example with compensation, supervision and timeouts could come first ([note](docs/research/concurrency-models.md#8-crdts-sagas-and-process-managers)). |

Reference design for ranks 49-51: Restate (a Rust server) journals each handler
step with its result and replays the journal after a failure, with keyed
single-writer virtual objects and durable timers. Compare it with Temporal
before designing any of the three; see [virtual actors](docs/research/virtual-actors.md#restate-virtual-objects).

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

The former rank 5b gaps now have asserted examples in v0.7.0:

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
| 5c | Production lifecycle monitors | Inject a stale-generation publication in a test build; the online monitor reports it through `tracing` and a counter, and the offline runner flags the recorded trace. |
| 6 | Test kit (phase A implemented) | [Test kit](examples/capabilities/src/bin/test_kit.rs): mock a protocol dependency, probe sends, inject a slow reply, a crash and a supervised kill. Still to deliver: an actor holding a concrete `ActorRef<B>` with `B` mocked. |
| 7 | Deadlock detection | Two actors requesting each other fail fast with the named cycle. |
| 8 | Stall watchdog | A handler blocking a worker versus one stuck across an `.await`, each reported with actor and message type. |
| 8a | Cooperative budgets | On one CPU, an actor flooding itself with messages and one running a long async loop with `ctx.checkpoint()` both leave a second actor's latency under a bound; per-actor work metrics show the busy actor. |
| 10 | Published snapshots | Readers keep answering from a snapshot while the owner runs a long handler. |
| 11 | Deferred replies / delegation / pipe-to-self | Route a slow request while accepting another message; bound pending replies and demonstrate owner exit and late completion. |
| 11a | Pipelining / transactional turns | A pipelined send that fails with its source request; a handler that panics after sending, with sends held until the turn ends. |
| 11b | Managed compute pool | Offload a CPU-heavy job while the actor keeps answering requests; the result arrives as a message; a full pool rejects at once; a panicking job becomes an error message; stopping the owner cancels queued jobs and drops a late result. |
| 12 | Lifecycle-owned registries (managed generation guards implemented) | Register a service, crash it and verify its entry is removed without deleting a replacement's registration. |
| 12a | Dynamic supervisor | Start one supervised actor per incoming connection; crash a restartable child and see only it restart; stop one explicitly and see it stay stopped; hit `max_children` and get a rejection; then shut down with every live child stopped within the deadline. |
| 13 | Coordinated shutdown | Close admission, drain dependencies within a deadline and report unfinished work. [Finite-batch recipe works today](examples/capabilities/src/bin/graceful_drain.rs). |
| 14 | Fair control path | Observe lifecycle control behind a data backlog while checking ordinary work still progresses; no handler preemption. |
| 15 | Actor-owned helpers (managed scope implemented) | Stop an actor with active helpers, reject new helper admission and await owned cleanup to a deadline. |
| 15a | Cancel scopes | A request scope with a deadline: its outbound request and compute job inherit the deadline, closing the scope rejects new children, and a shielded cleanup completes while unfinished children are reported. |
| 16, 32 | State machines / bounded stashing | Queue a bounded number of messages while connecting; un-stash on readiness and reject overflow. |
| 17 | Bounded mailboxes | A slow consumer reaches capacity; demonstrate explicit rejection and cancellable waiting without blocking actor progress. |
| 18 | Byte/work credits | Reserve before decoding a large payload; pass credit across stages and reclaim it after rejection/panic. [Payload-budget adapter works today](examples/capabilities/src/bin/byte_budget_pipeline.rs). |
| 5a, 19 | Inherited deadlines | Exhaust an ingress budget and skip stale queued work without granting downstream calls fresh time. [Current timeout behavior](examples/capabilities/src/bin/timeout_outcomes.rs). |
| — | OneForAll/RestForOne and nested supervisor specs | Available in v0.7.0 with bounded cohort/nested models, real trace replay and negative controls; wider runtime guarantees remain outside those projections. |
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
| 47a | Choreographies | Project a three-party handshake into handlers and show a mismatched step cannot be expressed. |
| 48 | Fair join patterns | Match votes by round with bounded unmatched storage, cancellation and competing ready matches. |
| 49 | Durable outbox / idempotency | Crash after committing intent, retry and deduplicate effects using application storage; no arbitrary-envelope persistence. |
| 50 | Durable reminders | Restart a process and demonstrate missed-tick and duplicate-delivery policy against persisted state. |
| 51 | Durable workflows | Replay persisted decisions after a crash and distinguish replayed decisions from external effects. |

Full deterministic environment simulation remains research. Existing controlled
handler-progress replays, failure windows and request-history checks are in
[`concurrency/tests`](concurrency/tests) and documented in
[testing](docs/TESTING.md); they do not implement the capabilities above.
