# Event sourcing for actors: Akka Persistence, Orleans JournaledGrain and Restate

Research note, 2026-11-09. Informs ranks 49-51 (durability) and ranks 21-22
(crash tapes and replay) of the [roadmap](../../ROADMAP.md#ranked-backlog).
Restate's journaled execution and the virtual-actor addressing it sits on are
described in [virtual actors](virtual-actors.md#restate-virtual-objects); this
note does not repeat them.

## What it is

*Event sourcing* stores the changes to an actor's state as an append-only
sequence of events instead of storing the state itself. The current state is a
fold of the events over an initial state; after a crash or a move to another
node the actor is rebuilt by replaying its events, usually starting from a
snapshot. Two rules make this work: only one writer appends to a given log, and
applying an event is deterministic and free of side effects.

It is distinct from *durable execution* (Restate, Temporal), which journals the
steps of a handler (calls, timers, side effects and their results) so the
handler itself resumes after a crash. Event sourcing persists *what happened to
the state*; durable execution persists *how far the code got*.

## How it works

### Akka Persistence (typed `EventSourcedBehavior`)

- **Commands, events, state.** A command handler takes the current state and
  a command and returns an `Effect`: `persist` (one or more events, stored
  atomically) or `none`. An event handler computes the new state from the
  current state and one event and "must only update state, never perform side
  effects". Side effects attach with `thenRun`, which runs only after the
  events are stored.
- **Recovery.** "A stateful actor is recovered by replaying the stored events
  to the actor, allowing it to rebuild its state." The actor "will always
  receive a `RecoveryCompleted` signal, even if there are no events in the
  journal", which is where end-of-recovery side effects go.
- **Single writer.** "Akka Persistence is based on the single-writer
  principle": one live instance per `PersistenceId`, which Cluster Sharding
  usually enforces.
- **Storage.** Events go to a pluggable journal; a snapshot store is optional
  ([Akka Persistence](https://doc.akka.io/libraries/akka-core/current/typed/persistence.html)).
- **Snapshots.** Saved "every N events or when a given predicate of the state
  is fulfilled" (`snapshotWhen`, `RetentionCriteria.snapshotEvery`, with older
  snapshots deleted by retention). "During recovery, the persistent actor is
  using the latest saved snapshot to initialize the state," then replays the
  later events ([snapshotting](https://doc.akka.io/libraries/akka-core/current/typed/persistence-snapshot.html)).
- **Read side.** Akka Projections processes "a stream of events or records from
  a source to a projected model or external system", storing an offset so a
  restarted projection resumes where it stopped
  ([Akka Projections](https://doc.akka.io/libraries/akka-projection/current/overview.html)).

### Orleans `JournaledGrain`

- A grain derives from `JournaledGrain<TGrainState, TEventBase>`. `State` is
  read-only; `Version` "always equals the total number of confirmed events",
  and the state "is the result of applying all confirmed events to the initial
  state".
- `RaiseEvent` "initiates a write to storage but doesn't wait for the write to
  complete"; `await ConfirmEvents()` waits for persistence. `RaiseEvents`
  writes a batch atomically; two separate `RaiseEvent` calls risk the grain
  failing after only the first is written.
- Transitions are `Apply` overloads on the state or a `TransitionState`
  override; they must be deterministic and side-effect free, and an exception
  in one is logged as a warning rather than failing the grain
  ([JournaledGrain basics](https://learn.microsoft.com/en-us/dotnet/orleans/grains/event-sourcing/journaledgrain-basics)).
- **Log-consistency providers** decide what is stored
  ([providers](https://learn.microsoft.com/en-us/dotnet/orleans/grains/event-sourcing/log-consistency-providers)):
  - *StateStorage* stores state snapshots plus a version, not events (so
    `RetrieveConfirmedEvents` is unsupported);
  - *LogStorage* stores the whole event list as one object and is "not suitable
    for production use unless the event sequences are guaranteed to remain
    fairly short"; its purpose is samples and testing;
  - *CustomStorage* calls the application's `ReadStateFromStorage` and
    `ApplyUpdatesToStorage(updates, expectedVersion)`, which must return
    `false` on a version mismatch (an e-tag check). A retried write after an
    exception may duplicate events, and the application must make that safe.
- Event sourcing is also Orleans' route to geo-replicated grain instances
  ([overview](https://learn.microsoft.com/en-us/dotnet/orleans/grains/event-sourcing/)).

### Restate

Restate journals handler steps and replays the journal to resume a handler,
with keyed state committed alongside (see
[virtual actors](virtual-actors.md#restate-virtual-objects)). In this
note's terms it is durable execution with per-key state, not event sourcing:
the application does not define events or a fold.

### Comparison

| | Akka Persistence | Orleans JournaledGrain | Restate |
|---|---|---|---|
| What is persisted | Domain events (plus optional snapshots) | Events or state, by provider | Handler journal plus K/V state |
| Rebuild | Snapshot, then replay events | Replay events or load state | Replay journal, skip completed steps |
| Side effects | After persist (`thenRun`) | Application's responsibility | Journaled; results replayed |
| Single writer | One instance per `PersistenceId` (sharding) | One activation (directory) | Per-key queue |
| Duplicate on retry | Journal-dependent (not checked here) | Possible with CustomStorage; app must handle | Journal dedup (our reading) |
| Read models | Akka Projections | Notifications | Not covered here |

## Who uses it

We did not verify named production users of Akka Persistence, Orleans event
sourcing or Restate for this note; none are claimed.

## What it means for Spawned

- **Spawned stays storage-agnostic; durability is an application recipe first
  (rank 49).** The roadmap rules out persisting arbitrary boxed envelopes. Event
  sourcing fits that rule: the application defines a serializable event type
  and a pure `apply(&mut State, &Event)`, and persists events, not messages.
  The rank 49 example can be an event-sourced actor: `started()` loads a
  snapshot and replays events through `apply`; a handler validates the command,
  appends events with an expected version (Orleans' e-tag check), then applies
  them and replies; side effects run only after the append succeeds (Akka's
  `thenRun`). An idempotency key on the command makes the retry after a crash
  between append and reply safe.
- **Supervision is the recovery trigger.** A supervised restart (`ChildSpec`
  factory) runs `started()` again, which is Akka's recovery: rebuild from
  storage, then accept messages. The single-writer rule is what instance-bound
  references and generations already give within one process; rank 12's keyed
  registry extends it to one live actor per key, which is the precondition
  Akka gets from Cluster Sharding.
- **Durable timers (rank 50)** are events too: scheduling a reminder appends a
  `TimerSet` event, firing appends `TimerFired`, and replay re-arms unfired
  timers. Missed and duplicate firings are then visible in the log.
- **Durable execution (rank 51) is a different product.** Event sourcing
  makes *state* survive; Restate and Temporal make *in-flight handler steps*
  survive. Rank 51 stays a separate subsystem; Restate remains its reference.
- **Crash tapes and replay (ranks 21-22).** An event-sourced actor's log is a
  ready-made tape of its *state changes*, but not of its *inputs*: a crash tape
  records messages, replies, clock reads and delivery order, which is what a
  replay needs to reproduce a bug; an event log records only the decisions that
  were persisted. The overlap is the snapshot-plus-suffix shape: rank 21 needs a
  state snapshot at the tape start, the same mechanism Akka uses to bound
  recovery. Share the snapshot interface if both are built; keep the formats
  separate (a hypothesis, not a plan).
- **Testing.** Event sourcing gives a cheap invariant for `spawned-test`:
  rebuilding from the log must equal the live state (replay equivalence), a
  metamorphic relation in the sense of [test techniques](test-techniques.md).

## Sources

- [Akka Persistence (event sourcing)](https://doc.akka.io/libraries/akka-core/current/typed/persistence.html)
- [Akka Persistence snapshotting](https://doc.akka.io/libraries/akka-core/current/typed/persistence-snapshot.html)
- [Akka Projections overview](https://doc.akka.io/libraries/akka-projection/current/overview.html)
- [Orleans event sourcing overview](https://learn.microsoft.com/en-us/dotnet/orleans/grains/event-sourcing/)
- [Orleans JournaledGrain basics](https://learn.microsoft.com/en-us/dotnet/orleans/grains/event-sourcing/journaledgrain-basics)
- [Orleans log-consistency providers](https://learn.microsoft.com/en-us/dotnet/orleans/grains/event-sourcing/log-consistency-providers)
- Restate: see [virtual actors](virtual-actors.md#sources)
