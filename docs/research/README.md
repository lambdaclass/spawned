# Research notes

Maintained research behind the [roadmap](../../ROADMAP.md). Each note says what
a technique or system is, how it works, who uses it (sourced claims only), what
it means for Spawned with links to ranked items, and its sources. Claims we
could not verify are marked in place. Earlier research, no longer maintained,
is archived in [legacy](../../legacy/docs/research/); these notes link to it
rather than repeat it.

| Note | Roadmap items |
|---|---|
| [Virtual actors](virtual-actors.md) | Ranks 12, 26, 44, 49-51 |
| [Systematic testing, simulation and runtime monitoring](systematic-testing.md) | Verification rule, testing to add, ranks 5c, 20-22a |
| [How the Ethereum ecosystem tests clients](ethereum-testing.md) | Ranks 5, 5c, 20-22 |
| [Request semantics references](request-semantics-references.md) | Ranks 11, 11a, 47, 47a |
| [Other concurrency models](concurrency-models.md) | Ranks 8a, 11b, 15a (new); notes on 11a, 13, 15, 19, 33, 34, 39, 48; research pointers 52-54 |

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
