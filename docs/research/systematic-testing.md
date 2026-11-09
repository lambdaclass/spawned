# Systematic testing, simulation and runtime monitoring

Research note, 2026-11-09. Informs the [verification rule](../../ROADMAP.md#verification-rule),
the [`spawned-test` crate](../../ROADMAP.md#testing-crate-spawned-test), the
[testing to add](../../ROADMAP.md#testing-to-add) table and ranks 5c, 20-22 and
22a. The earlier [verification strategy](../../legacy/docs/research/verification-strategy.md)
chose TLA+ plus trace validation and split Shuttle (framework internals) from
actor-level exploration (users); this note surveys the wider field and adds a
production arm.

## What it is

Four families of tools find concurrency and distribution bugs that ordinary
tests miss:

1. **Controlled scheduling** runs real code but takes over the points where
   concurrency is decided, then searches orderings (Loom, Shuttle, Coyote).
2. **Model checking** explores a model of the design, or a model written in the
   implementation language (TLA+, P, Stateright).
3. **Deterministic simulation** runs a whole system, its network, disk and clock
   in one deterministic process or VM, injecting faults from a seed
   (FoundationDB, TigerBeetle VOPR, turmoil, madsim, Antithesis).
4. **History and log checking** observes a real run and checks the recorded
   history against a specification (Jepsen with Knossos/Elle, Porcupine,
   PObserve, and Spawned's own TLA+ trace validation).

## How it works

### Coyote (Microsoft, formerly P#)

Coyote "takes control of the concurrency so that it can manipulate every
possible scheduling" ([site](https://microsoft.github.io/coyote/)). It rewrites
.NET test binaries so tasks, locks and actor sends become scheduling points,
then explores interleavings and declared nondeterminism (failures, timeouts)
across many iterations. Strategies include random, delay bounding, Q-learning
and a priority-based strategy whose source cites the PCT algorithm
([`PrioritizationStrategy.cs`](https://github.com/microsoft/coyote/blob/main/Source/Core/Testing/Interleaving/PrioritizationStrategy.cs);
[Burckhardt et al., ASPLOS 2010](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/asplos277-pct.pdf));
the default is a tuned portfolio. Every bug comes with a reproducible trace.

Coyote also offers an actor and state-machine programming model: "Actors run
concurrently with respect to each other, but individually they handle their
input queue in a sequential way"
([actors](https://microsoft.github.io/coyote/concepts/actors/overview/)). For
actors the useful unit of exploration is the handler turn: messages to
different actors commute, so only deliveries racing at one actor need to
branch. That is the design rank 20 adopts.

History: Coyote "is the evolution of the P# project, and is often published
under the name of P#" ([publications](https://microsoft.github.io/coyote/overview/publications/)).
The FAST 2016 paper *Uncovering Bugs in Distributed Storage Systems during
Testing (not in Production!)* applied P# to Azure Storage vNext, Azure Live
Table Migration and Azure Service Fabric by modelling the environment
(network, failures, timers) around the real code
([USENIX](https://www.usenix.org/conference/fast16/technical-sessions/presentation/deligiannis)).
A secondary summary reports 8 serious bugs found
([the morning paper](https://blog.acolyer.org/2016/05/05/uncovering-bugs-in-distributed-storage-systems-during-testing-not-in-production/));
we did not confirm the count against the paper text.

### P and PObserve (AWS)

P models a system as communicating state machines ("or actors"), with
specification monitors; its checker "explores interleavings of messages and
failures" and returns a reproducible error trace
([What is P](https://p-org.github.io/P/whatisP/)). The
[P repository](https://github.com/p-org/P) states that teams across AWS (S3,
EBS, DynamoDB, MemoryDB, Aurora, EC2, IoT) use P to reason about their designs.

**PObserve** checks the *same* P specifications against logs of the real
service, "from integration testing or in production", "without requiring
additional instrumentation"
([PObserve](https://p-org.github.io/P/advanced/pobserve/pobserve/)). The user
writes a log parser that turns service log lines into timestamped P events; a
sequencer merges them into one globally ordered stream; a demultiplexer routes
each event to the spec monitors that observe it. The stream processing is
implemented with Java Streams.

### Stateright (Rust)

"A model checker for implementing distributed systems." The system is written
as Rust actors; the same actors can be model-checked (safety, reachability,
experimental liveness), explored in a local web UI, or spawned on a real
network ("transmitting JSON messages over UDP"). It includes a linearizability
tester that runs inside the model checker; examples include single-decree Paxos
and two-phase commit ([docs.rs](https://docs.rs/crate/stateright/latest), version
0.31.0, July 2025). Stateright owns its actor runtime, so it cannot check actors
written against Spawned's API without a port; it is prior art for rank 20, not a
dependency.

### Deterministic simulation

- **FoundationDB.** The whole cluster runs in one thread on Flow, which
  "supports both production execution and deterministic simulated execution";
  machines, disks and network are simulated and faults include connection
  loss, reboots and *swizzle-clogging* (stop connections of a random subset of
  nodes, restore them in random order)
  ([testing](https://apple.github.io/foundationdb/testing.html)).
- **TigerBeetle VOPR.** "A simulated environment where an entire cluster,
  running real code, is subjected to all kinds of network, storage and process
  faults", at about 1000x speed, running continuously on 1024 cores
  ([safety](https://docs.tigerbeetle.com/concepts/safety/)).
- **turmoil (tokio-rs).** Deterministic execution of many simulated hosts in
  one thread, a `tokio::net`-like API, partition/repair/hold/release on links,
  virtual time and an unstable filesystem ([docs.rs](https://docs.rs/turmoil/latest/turmoil/)).
- **madsim.** A tokio-like runtime with deterministic simulation; projects swap
  in patched crates (`madsim-tokio`) under a cfg flag; used by RisingWave
  ([GitHub](https://github.com/madsim-rs/madsim)).
- **Antithesis.** A deterministic hypervisor runs unmodified containers in one
  VM, injects faults (partitions, restarts, pauses) and can rewind to reproduce
  a bug ([deterministic simulation testing](https://antithesis.com/resources/deterministic_simulation_testing/)).
  Commercial; not reproducible outside the platform.

### History checkers

- **Jepsen / Knossos / Elle.** Jepsen drives a real cluster under faults and
  records client operations. Elle checks transactional histories by inferring
  an Adya-style dependency graph and finding cycles, roughly linear in history
  length ([Elle](https://github.com/jepsen-io/elle);
  [Kingsbury and Alvaro, PVLDB 14(3)](https://arxiv.org/abs/2003.10554)).
- **Porcupine.** A linearizability checker in Go, "generally 1,000x-10,000x
  faster" than Knossos on Jepsen data, with P-compositionality (check each key
  independently) and an HTML visualization of linearization points; etcd uses
  it in its robustness tests ([GitHub](https://github.com/anishathalye/porcupine)).

### Loom and Shuttle

- **Loom** "runs a test many times, permuting the possible concurrent
  executions of that test under the C11 memory model" ([GitHub](https://github.com/tokio-rs/loom)).
  Bounded-exhaustive; for small primitives.
- **Shuttle** (AWS Labs) controls thread scheduling with random and PCT
  schedulers; it "is not sound" but "scales to much larger test cases than
  Loom", and failing schedules replay deterministically
  ([GitHub](https://github.com/awslabs/shuttle)).

### Comparison

| Tool | Explores or checks | Runs on | Use in Spawned |
|---|---|---|---|
| Loom | Every interleaving and memory ordering, bounded | Code (small tests) | Exists: cancellation, ready gate; next reply slots, registry compare-and-remove |
| Shuttle | Random/PCT thread schedules | Code | Framework-internal multi-actor races (testing to add, after v0.7) |
| Coyote | Message delivery orders, failures, timers; replayable traces | Code (.NET) | Design of rank 20 `explore` |
| Stateright | Actor model states; linearizability | Rust model that also runs | Prior art for rank 20; not a dependency |
| TLA+ / TLC | All behaviours of a bounded design model | Model | Source of truth under the verification rule (exists) |
| P | Message/failure interleavings of a state-machine model | Model | Not adopted; TLA+ already chosen |
| TLA+ trace validation | Recorded run is a behaviour of the model | Logs from tests | Exists for supervision (`lifecycle-trace`) |
| PObserve | Spec monitors over service logs | Logs, tests or production | Pattern for rank 5c production monitors |
| FoundationDB, VOPR, turmoil, madsim | Seeded network, disk, clock faults over a whole system | Code in a simulator | Pattern for rank 22a environment simulation |
| Antithesis, ethshadow | Unmodified binaries under a deterministic VM/simulator | Binaries | Hosts for client-level runs ([Ethereum testing](ethereum-testing.md)) |
| Jepsen/Elle, Porcupine | Consistency of recorded client histories | Logs | Porcupine-style checker for `request_histories` (testing to add) |

## Who uses it

Only sourced claims:

- **Coyote / P#:** Azure Storage vNext, Azure Live Table Migration and Azure
  Service Fabric (FAST 2016); the Coyote site lists case studies from Azure
  Batch and Azure Blockchain Service.
- **P:** AWS used P to build "formal models of all the core distributed
  protocols involved in S3's strong consistency" (Werner Vogels, quoted on the
  [P case studies page](https://p-org.github.io/P/casestudies/)). Brooker and
  Desai's *Systems Correctness Practices at Amazon Web Services* (ACM Queue /
  CACM 2025) surveys P, PObserve and other methods across AWS. We could not load
  the article text (HTTP 403) and rely on the P site's quotations of it. We
  found no source saying PObserve was used for the S3 launch itself; PObserve
  postdates it.
- **Porcupine:** etcd, Amazon MemoryDB, MIT 6.5840 (project README).
- **madsim:** RisingWave (project README).
- **Shuttle:** maintained under awslabs; the README does not name internal users.
- **Antithesis:** see [Ethereum testing](ethereum-testing.md#antithesis).

## How databases and storage systems test

Production systems combine these families. Each row is sourced below; the last
column is our inference for Spawned.

| System | Technique | Result or lesson | Spawned analogue |
|---|---|---|---|
| MongoDB server and Realm Sync | Model-based trace checking of replication against TLA+; model-based test-case generation from TLC's state graph for Realm Sync's operational transformation ([PVLDB 2020](https://arxiv.org/abs/2006.00915)) | Trace checking was judged impractical: snapshotting a multithreaded server's state took most of the budget and the spec, written years after the code, did not match its non-atomic steps; the effort was stopped. Test generation produced 4,913 C++ tests for one configuration with 100% branch coverage of the algorithm. The 2025 follow-up: make spec and code conform from the start and log observable events ([MongoDB blog](https://mongodb.com/blog/post/engineering/conformance-checking-at-mongodb-testing-our-code-matches-our-tla-specs)) | Spawned's trace validation works because the recorder was designed with the models, one event per model action inside a linearizing section, rather than retrofitted. Keep that rule for the base lifecycle model and rank 5c |
| Amazon S3 ShardStore | Executable reference models as specifications, property-based testing against them, stateless model checking for concurrency, crash-consistency checks ([Bornholt et al., SOSP 2021](https://jamesbornholt.com/papers/shardstore-sosp21.pdf)) | "Prevented 16 issues from reaching production"; non-experts extended the checks. Secondary sources cite reference models at about 1% of the implementation's size; we did not confirm a code-overhead figure or which model checker was used (Shuttle is plausible but unconfirmed) | Reference models in `request_histories`; Shuttle for framework internals |
| Dropbox Nucleus (Rust) | All randomized tests deterministic from a logged seed; *CanopyCheck* tests the planner without I/O; *Trinity* runs the engine's Rust future under a custom executor with mocked filesystem, network and timers; "tens of millions of randomized test runs" nightly ([Dropbox, 2020](https://dropbox.tech/infrastructure/-testing-our-new-sync-engine)) | A Rust engine can own its executor in tests and make every run reproducible | The closest precedent for rank 20 `explore` and rank 22a |
| CockroachDB | Jepsen nightly: "each test+nemesis combination for 6 minutes" on a 5-node cluster ([2019](https://www.cockroachlabs.com/blog/jepsen-tests-lessons/)); Pebble metamorphic testing runs one random operation sequence under several configurations and compares outputs, nightly ([2023](https://www.cockroachlabs.com/blog/metamorphic-testing-the-database/)); Antithesis reproduced a rare "transaction unexpectedly committed" crash ([2024](https://www.cockroachlabs.com/blog/demonic-nondeterminism/)) | Different techniques find different bugs; a rare production bug needed deterministic replay | Metamorphic check: the same actor program on async, blocking and thread backends must produce the same outcomes (partly done by `delivery_simulation` across four backends) |
| etcd | Robustness tests record client operation histories and check them with Porcupine; failpoints via gofail; each known bug keeps a reproduction target such as `make test-robustness-issue14370` ([README](https://github.com/etcd-io/etcd/blob/main/tests/robustness/README.md)). Antithesis runs rediscovered all known bugs tested and found a watch bug in all stable releases ([etcd blog, 2025](https://etcd.io/blog/2025/autonomus_testing_with_antithesis/)) | Keep every found bug as a permanent named reproduction | Reintroduced PR #182/#185 races as permanent `explore` cases (rank 22 go/no-go) |
| RisingWave (madsim), S2 (turmoil + mad-turmoil) | Rust DST: madsim simulates network, disk and stubs for etcd, S3 and Kafka ([RisingWave](https://risingwave.com/blog/deterministic-simulation-a-new-era-of-distributed-system-testing/)); S2 runs on Tokio with turmoil, overrides clock and entropy, and a CI test reruns a seed and compares TRACE logs byte for byte; 17 notable issues found ([S2](https://s2.dev/blog/dst)) | DST is practical on Tokio without rewriting the system | Rank 22a builds on turmoil; S2's same-seed log comparison is a cheap determinism check for `explore` |
| Polar Signals (Rust) | Components written as single-threaded state machines (`receive`, `tick`) driven by a message bus that owns scheduling, time, randomness and fault injection ([2025](https://blog.polarsignals.com/blog/posts/2025/07/08/dst-rust)) | Found a data-loss and a duplication bug; retrofitting DST is costly, logic drifts into untested production drivers | Spawned actors are already message-driven state machines; the mailbox is the bus, so users get this without a rewrite |
| WarpStream (Go) | Antithesis over the entire SaaS: agents, control plane, Kafka clients, Postgres, an S3 stand-in; checks lost, reordered and duplicated records ([2024](https://www.warpstream.com/blog/deterministic-simulation-testing-for-our-entire-saas)) | Found a data race and a rare data-loss bug that existing tests missed | Antithesis as a host for whole-client runs ([Ethereum testing](ethereum-testing.md#antithesis)) |
| TigerBeetle | Protocol-aware DST: the simulator sees every replica's state and asserts, for each committed request, that checksums match across replicas; such global checks are "expensive" in production so they live in the simulator ([2026](https://tigerbeetle.com/blog/2026-08-20-protocol-aware-dst)) | Simulation can check invariants production cannot afford | `explore` checks cross-actor invariants (for example, one live generation per registry key) that rank 5c monitors, which are per entity, do not |
| Azure Cosmos DB | TLA+ specification of user-visible consistency behaviour ([Hackett, Rowe, Kuppe, arXiv 2210.13661](https://arxiv.org/abs/2210.13661)) | Found two issues in the public documentation, since fixed, and a fix for an outage in a dependent service | Model the *user-visible* contract too (request outcomes, `ActorStopped`), not only internals |

## What it means for Spawned

- **Rank 20 (`explore`)** stays the Coyote design: one handler turn is one
  atomic step, deliveries are controlled at the mailbox, timers and exits are
  scheduling points, search uses random plus PCT-style priorities, and every
  failure prints a replayable delivery list. Stateright shows the payoff of the
  same actors running both under a checker and for real; Spawned gets that
  without a separate runtime because the hooks sit in its own mailboxes.
- **Rank 22a (environment simulation)** extends phase 5: once delivery is
  controlled, add turmoil/madsim-style simulated network, disk and clock so a
  test can partition peers, fail a write or jump time from the same seed.
  Prefer adapting to turmoil (tokio-rs, `tokio::net`-shaped) over owning a
  network simulator. Handlers that use Tokio I/O directly remain outside
  control unless their I/O goes through the simulated layer.
- **Porcupine-style checker for `request_histories`** (testing to add): the
  existing checker is a bounded search (16 operations, 100,000 states).
  Partition histories per key (P-compositionality), use the search Porcupine
  builds on (Lowe's linearizability testing with memoized states), and emit a
  visual history on failure,
  so longer histories from proptest, soak and canary runs can be checked.
- **Rank 5c (production monitors)**: the section below.
- **Not adopted:** P as a second modelling language. TLA+ is in place with trace
  validation; PObserve's *architecture* transfers without the language.

## A PObserve for Spawned

### What exists

With the `lifecycle-trace` feature (off by default, compiled out otherwise),
`run_supervisor`, `run_child`, the cohort controllers and the tree owner record
one event per model action (`concurrency/src/lifecycle_trace.rs`). Tests write
the traces as JSON; `verification/tla/trace_check.py` replays each as a
behaviour of `TraceSupervision.tla`, `TraceCohorts.tla` or `TraceNested.tla` in
CI, and hand-edited negative controls must be rejected
([TLA+ README](../../verification/tla/README.md#trace-validation)).

This is design-to-code conformance for *test runs*. Three things keep it from
running in a live node: the recorder buffers every event in a process-wide
`Mutex<BTreeMap>` until a test takes it (unbounded); it linearizes events
through one global critical section; and TLC replay is offline, needs a JVM and
takes seconds per trace.

### Proposal

PObserve's split carries over: events from the implementation, an ordering
step, and monitors derived from the specification. Three parts:

**(a) Online monitors in Rust.** Hand-written Rust state machines, one per
invariant, compiled from the TLA+ properties and consuming the trace stream in
the live process:

| Monitor | Source property | State kept |
|---|---|---|
| `AtMostOneGeneration` | `Supervision.tla` | Live generation per child slot |
| `PublishedIsLive` | `Supervision.tla` | Published generation per slot |
| `BudgetBounded` | `Supervision.tla` | Restart timestamps within the window |
| `ReverseShutdown` | `Supervision.tla` action property | Shutdown cursor per supervisor |
| Every accepted request is answered or gets `ActorStopped` | Base lifecycle model ([next model](../../verification/tla/README.md#next-model)), not yet written | Outstanding request ids per actor |

Each monitor's state is per live supervisor or actor and is dropped when it
finishes, so memory is bounded by live entities, plus a fixed cap with an
explicit `monitor_overflow` report when exceeded. Violations go to `tracing`
(error level, with the last N events for that entity) and a metrics counter;
monitors never panic or stop the node. The recorder needs a production sink: a
bounded per-supervisor channel or ring, dropping with a counted gap rather than
blocking the runtime, and ordering per entity instead of the global section
(every listed property is per supervisor or per actor, so a global
linearization is not needed). The last row needs new request events
(accepted, replied, rejected), which the base lifecycle model must define
first.

**(b) Offline mode.** The same monitors run over recorded JSON traces: in CI
next to `trace_check.py` (fast, no JVM), and on the
[Ethlambda canary](../../ROADMAP.md#step-2-release-and-run-in-a-client) runs on
fede256, where the node writes traces to disk and the monitors check them after
the run.

**(c) TLA+ remains the source of truth.** Monitors are derived from the models,
never the other way round. Each monitor is tested by replaying the same
negative-control traces TLC rejects (`negative_checks.py` and the hand-edited
traces): every trace TLC rejects for a property the monitor covers must also be
flagged by the monitor, and every accepted trace must pass. A monitor that
disagrees with TLC is a bug in the monitor.

Limits: monitors check safety properties over what was recorded; they cannot
check liveness beyond timeouts, cannot see unrecorded paths (`Unmodelled`
events stay a reported gap), and a dropped-event gap makes the affected entity
unchecked rather than flagged.

### Prior art in Rust

We found no Rust port of P or PObserve (searches of crates.io-indexed results
and the p-org organization, 2026-11-09). Stateright is the closest Rust tool for
model-checking actors. General runtime-verification tools exist (for example
RTLola, whose monitor generation can emit Rust), but none targets actor
lifecycle traces; we did not evaluate them.

## Sources

- [Coyote](https://microsoft.github.io/coyote/), [actors](https://microsoft.github.io/coyote/concepts/actors/overview/), [publications](https://microsoft.github.io/coyote/overview/publications/), [PrioritizationStrategy.cs](https://github.com/microsoft/coyote/blob/main/Source/Core/Testing/Interleaving/PrioritizationStrategy.cs)
- [Deligiannis et al., FAST 2016](https://www.usenix.org/conference/fast16/technical-sessions/presentation/deligiannis); [summary (the morning paper)](https://blog.acolyer.org/2016/05/05/uncovering-bugs-in-distributed-storage-systems-during-testing-not-in-production/)
- [PCT: A Randomized Scheduler with Probabilistic Guarantees of Finding Bugs (ASPLOS 2010)](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/asplos277-pct.pdf)
- [What is P](https://p-org.github.io/P/whatisP/), [PObserve](https://p-org.github.io/P/advanced/pobserve/pobserve/), [P case studies](https://p-org.github.io/P/casestudies/), [p-org/P](https://github.com/p-org/P)
- [Brooker and Desai, Systems Correctness Practices at Amazon Web Services (CACM)](https://cacm.acm.org/practice/systems-correctness-practices-at-amazon-web-services/) (not loaded; see above)
- [Werner Vogels on S3 strong consistency](https://www.allthingsdistributed.com/2021/04/s3-strong-consistency.html)
- [Stateright](https://docs.rs/crate/stateright/latest)
- [FoundationDB simulation and testing](https://apple.github.io/foundationdb/testing.html)
- [TigerBeetle safety and VOPR](https://docs.tigerbeetle.com/concepts/safety/)
- [turmoil](https://docs.rs/turmoil/latest/turmoil/), [madsim](https://github.com/madsim-rs/madsim)
- [Antithesis: deterministic simulation testing](https://antithesis.com/resources/deterministic_simulation_testing/)
- [Elle](https://github.com/jepsen-io/elle), [Elle paper](https://arxiv.org/abs/2003.10554), [Porcupine](https://github.com/anishathalye/porcupine)
- [Loom](https://github.com/tokio-rs/loom), [Shuttle](https://github.com/awslabs/shuttle)
- [eXtreme Modelling in Practice (MongoDB, PVLDB 2020)](https://arxiv.org/abs/2006.00915), [Conformance checking at MongoDB (2025)](https://mongodb.com/blog/post/engineering/conformance-checking-at-mongodb-testing-our-code-matches-our-tla-specs)
- [Using Lightweight Formal Methods to Validate a Key-Value Storage Node in Amazon S3 (SOSP 2021)](https://jamesbornholt.com/papers/shardstore-sosp21.pdf)
- [Testing sync at Dropbox](https://dropbox.tech/infrastructure/-testing-our-new-sync-engine)
- CockroachDB: [Jepsen lessons](https://www.cockroachlabs.com/blog/jepsen-tests-lessons/), [metamorphic testing](https://www.cockroachlabs.com/blog/metamorphic-testing-the-database/), [Antithesis of a one-in-a-million bug](https://www.cockroachlabs.com/blog/demonic-nondeterminism/)
- etcd: [robustness tests](https://github.com/etcd-io/etcd/blob/main/tests/robustness/README.md), [autonomous testing with Antithesis](https://etcd.io/blog/2025/autonomus_testing_with_antithesis/)
- [RisingWave deterministic simulation](https://risingwave.com/blog/deterministic-simulation-a-new-era-of-distributed-system-testing/), [S2 DST](https://s2.dev/blog/dst), [Polar Signals DST in Rust](https://blog.polarsignals.com/blog/posts/2025/07/08/dst-rust), [WarpStream DST](https://www.warpstream.com/blog/deterministic-simulation-testing-for-our-entire-saas)
- [TigerBeetle protocol-aware DST](https://tigerbeetle.com/blog/2026-08-20-protocol-aware-dst)
- [Understanding Inconsistency in Azure Cosmos DB with TLA+](https://arxiv.org/abs/2210.13661)
