# Research notes

Maintained research behind the [roadmap](../../ROADMAP.md). Each note says what
a technique or system is, how it works, who uses it (sourced claims only), what
it means for Spawned with links to ranked items, and its sources. Claims we
could not verify are marked in place. Earlier research, no longer maintained,
is in the [archived `legacy/docs/research`](https://github.com/lambdaclass/spawned/tree/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/docs/research); these notes link to it
rather than repeat it.

| Note | Roadmap items |
|---|---|
| [Virtual actors](virtual-actors.md) | Ranks 12, 26, 44, 49-51 |
| [Erlang/OTP, Elixir and Gleam compared](otp-elixir-gleam.md) | The OTP, Elixir and Gleam gaps table in the roadmap (G1-G23), 0.9 designs, non-goals |
| [Systematic testing, simulation and runtime monitoring](systematic-testing.md) | Verification rule, testing to add, ranks 5c, 20-22a |
| [How the Ethereum ecosystem tests clients](ethereum-testing.md) | Ranks 5, 5c, 20-22 |
| [Request semantics references](request-semantics-references.md) | Ranks 11, 11a, 47, 47a; rank 11 and 11a decisions in the [deferred replies design](../design/deferred-replies.md) |
| [Other concurrency models](concurrency-models.md) | Ranks 8a, 11b, 15a (new); notes on 11a, 13, 15, 19, 33, 34, 39, 48; research pointers 52-54 |
| [Rust verification tools](verification-tools.md) | Phase 4 batch 1 (Kani), batch 2 (Apalache, Veil), testing to add (Kani and unbounded-invariant rows), rank 3 |
| [Observability](observability.md) | Ranks 9, 28, 29, 30 |
| [Actor test kits](actor-testing.md) | Rank 6 (remaining work), rank 20 (Concuerror), Porcupine row, simulation backend spike |
| [Rust actor landscape](rust-actor-landscape.md) | North star; notes on ranks 30, 44, 49 |
| [Supervision models](supervision-models.md) | Ranks 12a, 13, 27 |
| [Deadlock and stall detection](deadlock-and-stall-detection.md) | Ranks 7 (approach chosen), 8, 8a, 30; rank 6 idle check |
| [Test techniques](test-techniques.md) | Testing to add (cancellation safety, dhat, open-loop load, platform matrix, metamorphic rows); TESTING.md planned layers |
| [Determinism audit](determinism-audit.md) | Determinism backlog D1-D10 (priority tier 3), S2, ranks 6b and 22a |
| [Event sourcing](event-sourcing.md) | Ranks 21, 49, 50, 51; phase 8 |

**[Virtual actors](virtual-actors.md).** Orleans grains are always addressable
by key, activated on first message, removed when idle, kept to one live
instance per key and placed across a cluster, with state as an in-memory cache
over storage. Akka Cluster Sharding and Dapr actors provide the same contract;
Restate virtual objects add journaled, durable handler execution. For Spawned,
the lifecycle-owned keyed registry (rank 12) comes first because ethrex creates
peers by dial/accept; in-process virtual actors (rank 26) stay needs-driven,
distribution (rank 44) stays research, and Restate is the reference design for
durability (ranks 49-51).

**[Systematic testing](systematic-testing.md).** Surveys controlled scheduling
(Coyote, Shuttle, Loom), model checking (TLA+, P, Stateright), deterministic
simulation (FoundationDB, TigerBeetle VOPR, turmoil, madsim, Antithesis) and
history checking (Jepsen/Elle, Porcupine, PObserve), plus how MongoDB, S3
ShardStore, Dropbox, CockroachDB, etcd and others combine them. Proposes a
PObserve-style production arm (rank 5c): Rust monitors derived from the TLA+
invariants, run online in a live node and offline over recorded traces, tested
against the negative controls TLC rejects. Also environment simulation in
`spawned-test` (rank 22a) and a Porcupine-style history checker.

**[Ethereum testing](ethereum-testing.md).** Hive, Kurtosis devnets, Attacknet,
ethshadow, Antithesis, fuzzing and formal verification of the consensus
specification all treat a client as a black box or verify the protocol, not a
client's internal concurrency. Spawned can add deterministic actor-level tests
and production monitors inside ethrex and ethlambda; Attacknet and later
ethshadow can host the ethlambda canary's fault runs.

**[Request semantics](request-semantics-references.md).** Promise pipelining
(E, Cap'n Proto, Goblins), transactional turns and time-travel debugging
(Goblins) as inputs to the deferred-reply design (ranks 11, 11a);
choreographic programming (ChoRus, HasChor) as an alternative next to session
types (ranks 47, 47a); Pony reference capabilities briefly, for vocabulary.

**[Concurrency models](concurrency-models.md).** CPU compute pools (rayon,
Akka dispatchers, Orleans `Task.Run`, Ray tasks, BEAM dirty schedulers),
structured concurrency (Trio, Swift, Kotlin, Java `StructuredTaskScope`, Rust
crates), preemption and cooperative budgets (BEAM reductions, Go async
preemption, Tokio coop), and briefer notes on backpressured streams, STM,
thread-per-core I/O, incremental dataflow, CRDTs and sagas, CSP, effects and
join calculus. Proposes a managed compute pool (rank 11b) backed by ethrex's
hand-built rayon pools and its 0% CPU import deadlock, cancel scopes as the
shared design of ranks 13, 15 and 19 (rank 15a), and cooperative budgets with
per-turn work measurement next to the stall watchdog (rank 8a).

**[Verification tools](verification-tools.md).** Kani (bounded model checking
on CBMC, used by AWS on Firecracker and the Rust standard library verification
effort), Prusti, Verus, Creusot, Flux and Miri, and when each fits. Spawned uses
Kani for small arithmetic (backoff, jitter, the restart-budget window,
generations; the surviving mutants) and TLA+ for protocols, with Apalache
inductive checks and a Veil (Lean 4) spike to prove the protocol invariants for
any number of children; Miri waits for
unsafe code. *Landed in:* phase 4 batch 1 and the Kani row of testing to add;
rank 3.

**[Observability](observability.md).** Erlang observer and recon,
tokio-console, kameo's console, OpenTelemetry context propagation across
message boundaries, Akka Insights and Orleans meters, plus the Prometheus
`_max` exposition bug the Ethlambda canary found (now fixed on main). *Landed in:* ranks 9, 28,
29 (parser check for new families; `_max` already fixed) and 30 (`spawned-top` columns and safety rules,
optional tokio-console integration).

**[Actor testing](actor-testing.md).** Akka TestKit (TestProbe, BehaviorTestKit,
ManualTime), ExUnit with Mox, Common Test, PropEr and Concuerror, Orleans test
clusters, and the actix, ractor and kameo helpers, compared with
`spawned-test` phase A. *Landed in:* rank 6 remaining work (synchronous
behavior test kit, manual time, concrete `ActorRef<B>` mocks), rank 20
(Concuerror as prior art), the simulation backend spike.

**[Rust actor landscape](rust-actor-landscape.md).** actix, ractor, kameo,
xtra, coerce, lunatic and bastion compared on supervision, typed messaging,
request/reply, links and monitors, distribution, testing, verification and
maintenance, with where Spawned stands. *Landed in:* the north star; notes on
ranks 30, 44 and 49.

**[Supervision models](supervision-models.md).** OTP and Elixir strategies,
dynamic, partition and task supervisors, significant children, intensity and
escalation; Akka classic and typed supervision with backoff; Orleans' lack of
supervision; mapped to what Spawned implements. *Landed in:* ranks 12a
(`terminate_child`, budget defaults, non-goals), 13 and 27.

**[Deadlock and stall detection](deadlock-and-stall-detection.md).** Wait-for
graphs, Go's global-only deadlock check and leak profiles, Erlang's call
timeouts and `calling_self`, Java's `findDeadlockedThreads`, Akka's starvation
detector and Tokio's console lints and task hooks. *Landed in:* rank 7
(eager wait-for detection chosen), ranks 8, 8a and 30, and rank 6's idle check.

**[Test techniques](test-techniques.md).** Cancellation safety in async Rust,
dhat allocation-count tests, coordinated omission and open-loop load, a
platform matrix, and metamorphic testing beyond databases. *Landed in:* the
testing to add rows and the planned layers in [TESTING.md](../TESTING.md).

**[Event sourcing](event-sourcing.md).** Akka Persistence, Orleans
`JournaledGrain` and Restate's journal, and how they relate to durability and
crash tapes. *Landed in:* ranks 21, 49, 50 and 51; phase 8.

**[Determinism audit](determinism-audit.md).** What escapes the deterministic
simulator in Spawned, Ethrex and Ethlambda: direct Tokio spawns and timers,
OS threads and blocking offloads, wall-clock reads, unseeded randomness,
default-hasher maps and real I/O, counted per client subsystem, with
same-seed rerun results (no schedule nondeterminism found). The main lever is
madsim-style interposition of the clock and entropy. *Landed in:* the
determinism backlog D1-D10.
