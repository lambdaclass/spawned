# Virtual actors: Orleans, Akka Cluster Sharding, Dapr and Restate

Research note, 2026-11-09. Informs ranks 12, 26, 44 and 49-51 of the
[roadmap](../../ROADMAP.md#ranked-backlog). The earlier design sketch for
in-process per-key entities (state strategies, handoff guarantees) is in
[framework ideas](../../legacy/docs/research/framework-ideas.md#design-notes-per-key-entities-and-state);
this note does not repeat it.

## What it is

A *virtual actor* is addressed by a stable key rather than by a reference to a
running instance. The runtime, not the caller, decides when an instance exists:
it is created when a message for the key arrives, removed from memory when idle,
and recreated on the next message. Callers never create or destroy it. Microsoft
Orleans introduced the term; Orleans calls these actors *grains*.

## How it works

### Orleans

- **Always addressable.** "Actors are purely logical entities that *always*
  exist, virtually. An actor cannot be explicitly created nor destroyed, and
  its virtual existence is unaffected by the failure of a server that executes
  it" ([Orleans overview](https://learn.microsoft.com/en-us/dotnet/orleans/overview)).
  A grain has a user-defined key (GUID, integer, string or compound).
- **Activation on demand, deactivation when idle.** "The Orleans runtime
  automatically instantiates grains on demand. Grains not used for a while are
  automatically removed from memory to free up resources" (same page). Orleans
  9 adds memory-pressure activation shedding.
- **One live instance per key.** The runtime keeps a grain directory that maps a
  key to its single activation; the original paper describes it as a one-hop
  distributed hash table
  ([Bernstein et al., MSR-TR-2014-41](https://www.microsoft.com/en-us/research/publication/orleans-distributed-virtual-actors-for-programmability-and-scalability/)).
  Orleans 9 added a strongly consistent directory option. (Stateless worker
  grains are the explicit exception: they may activate on many servers.)
- **State as a cache over storage.** "While a grain runs, Orleans keeps the
  state in memory to serve read requests without accessing storage"; a write
  goes to the backing store only when the grain calls `WriteStateAsync`. What
  the grain did not write is lost with the activation.
- **Placement across a cluster.** Servers (*silos*) form a cluster; on
  activation the runtime picks a silo using a configurable policy (random,
  prefer-local, load-based, resource-optimized, or custom). Orleans 9 also
  rebalances activations.
- Around this core: durable *reminders* (timers that survive deactivation),
  grain interface versioning, call filters, streams and distributed
  transactions.

### Akka Cluster Sharding

Entities are addressed by an entity id; a hash maps the id to a *shard*, and a
coordinator allocates shards to nodes and rebalances them when nodes join.
"Each entity actor runs only at one place." Idle entities can be *passivated*;
a message that arrives while an entity is stopping without the `Passivate`
handshake is dropped, while with it the shard buffers messages until the stop
completes. *Remember entities* restarts entities after rebalance or crash, but
"the state of the entities themselves is not restored unless they have been
made persistent" (Akka Persistence). Single-writer-per-id is the reason to
combine sharding with persistence
([Akka docs](https://doc.akka.io/libraries/akka-core/current/typed/cluster-sharding.html)).

### Dapr actors

Dapr "includes a runtime that specifically implements the Virtual Actor
pattern": an actor is activated on its first request, the in-memory object is
garbage-collected when unused, state lives in a transactional state store, and
a placement service tells sidecars where each actor lives. Calls are
*turn-based*: one call per actor at a time. Timers are forgotten on
deactivation; reminders persist
([Dapr actors overview](https://docs.dapr.io/developing-applications/building-blocks/actors/actors-overview/)).

### Restate virtual objects

Restate is a durable-execution server ("a single binary written in Rust") with
SDKs in several languages. A *virtual object* is a keyed service with key/value
state "retained indefinitely and shared across requests"; "at most one handler
with write access can run at a time per object key", which "mimicks a queue per
object key"; *shared* handlers are read-only and may run concurrently
([Restate services](https://docs.restate.dev/concepts/services)).

The difference from Orleans is durability of *execution*, not just of state.
Restate records each step of a handler (calls, timers, side effects with their
results) in a journal; after a failure it "replays the journal, skipping
completed steps and resuming from exactly where it left off"
([durable execution](https://docs.restate.dev/concepts/durable_execution)).
State updates and journal entries commit together, so a handler that crashes
mid-way resumes rather than restarting from scratch or leaving half-written
state. (The atomic commit of state with the journal is our reading of the
design; the cited page states the replay behaviour, not the storage layout.)

### Comparison

| | Orleans | Akka Cluster Sharding | Dapr actors | Restate virtual objects |
|---|---|---|---|---|
| Address | Grain key | Entity id → shard | Actor type + id | Service + object key |
| Creation | On first call | On first message | On first call | On first invocation |
| Idle removal | Automatic | Passivation (opt-in/auto) | Automatic GC | Not applicable: state is fetched per invocation (our reading of the docs) |
| Single writer per key | Directory | Shard coordinator | Placement service | Per-key queue in the server |
| State | In-memory cache, explicit write | In-memory; persistence via Akka Persistence | State store | Durable K/V |
| In-flight work after crash | Lost unless written | Lost unless persisted | Lost unless saved | Resumed from journal |
| Durable timers | Reminders | Via persistence | Reminders | Durable timers in journal |

## Who uses it

- **Orleans.** Microsoft's docs state it is used "in Azure, Xbox, Skype, Halo,
  PlayFab, Gears of War, and many other internal services"
  ([overview](https://learn.microsoft.com/en-us/dotnet/orleans/overview)). The
  best-documented case is Halo 4, whose cloud services were built on Orleans
  ([Microsoft Research blog](https://www.microsoft.com/en-us/research/blog/orleans-simplifies-development-scalable-apps-cloud/);
  [Halo 4 talk](https://speakerdeck.com/caitiem20/building-the-halo-4-services-with-orleans-oredev-2014)).
  The Azure, Xbox, Skype and PlayFab claims come only from Microsoft's own
  documentation; we found no independent technical account of them.
- **Akka Cluster Sharding, Dapr actors, Restate.** We did not verify named
  production users for this note; none are claimed here.

## What it means for Spawned

- **Rank 12 first (keyed, lifecycle-owned registry).** Ethrex keeps one
  connection actor per peer, but peers are created by dial/accept, not by a
  first message, and a duplicate connection's cleanup once removed the live
  entry (#7435). That needs keyed registration with generations, not
  activation on demand ([client search](../../ROADMAP.md#client-search-results)).
  Orleans' "one live activation per key" is the same invariant the rank 12
  model must show: an old entry's cleanup never removes its replacement.
- **Rank 26 (in-process virtual actors) stays needs-driven.** If a client
  appears that addresses work by key and wants creation on first message,
  build the Orleans core in-process: one live instance per key, activation and
  deactivation hooks, idle timeout, ordered handoff (the next activation waits
  for the previous deactivation), and a defined outcome for messages caught
  mid-deactivation (forward or fail with `ActorStopped`, never drop silently;
  Akka's unacknowledged-passivation drop is the counter-example). It can sit on
  rank 12a's dynamic supervisor and rank 12's registry. Handoff needs a TLA+
  model under the [verification rule](../../ROADMAP.md#verification-rule).
- **Rank 44 (distribution).** Placement, a cluster directory and rebalancing are
  Orleans' distributed half. Keep key-based addressing such that a later
  distributed layer can resolve a key to a remote node, but do not build a
  directory before a multi-node user exists.
- **Ranks 49-51 (durability): Restate is the reference design.** Its journal
  answers the questions ranks 49-51 list: idempotency keys and outbox (49),
  durable timers (50) and deterministic replay of handler decisions (51). A
  Spawned version would journal at the handler boundary (inputs, outgoing
  requests and their replies, timer and clock reads), which is also what the
  crash tape (rank 21) records. The overlap suggests one recording format for
  testing and durability, but that is a hypothesis to check, not a plan.
  Restate's model keeps business code ordinary and moves durability into the
  runtime, consistent with Spawned's rule that handlers stay ordinary Rust.

## Sources

- [Orleans overview (Microsoft Learn)](https://learn.microsoft.com/en-us/dotnet/orleans/overview)
- [Orleans: Distributed Virtual Actors for Programmability and Scalability, MSR-TR-2014-41](https://www.microsoft.com/en-us/research/publication/orleans-distributed-virtual-actors-for-programmability-and-scalability/)
- [Orleans simplifies development of scalable apps for the cloud (Microsoft Research blog)](https://www.microsoft.com/en-us/research/blog/orleans-simplifies-development-scalable-apps-cloud/)
- [Building the Halo 4 services with Orleans (Caitie McCaffrey, Øredev 2014)](https://speakerdeck.com/caitiem20/building-the-halo-4-services-with-orleans-oredev-2014)
- [Akka Cluster Sharding](https://doc.akka.io/libraries/akka-core/current/typed/cluster-sharding.html)
- [Dapr actors overview](https://docs.dapr.io/developing-applications/building-blocks/actors/actors-overview/)
- [Restate services and virtual objects](https://docs.restate.dev/concepts/services)
- [Restate durable execution](https://docs.restate.dev/concepts/durable_execution)
