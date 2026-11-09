# Research: ideas that would make Spawned much more useful

> Historical; superseded by the [roadmap](../../../ROADMAP.md). Implemented since: execution groups including `pinned_group`, OneForOne/OneForAll/RestForOne supervision with nested trees, and Tokio-free synchronous builds (all unreleased). Statuses and rank numbers below are as of writing.

Date: 2026-11-08. Status: reference material (idea catalog and design notes),
not a commitment or release plan. **Rankings and order live only in the single
backlog, [ranked backlog](../../../ROADMAP.md#ranked-backlog), and the [roadmap](../../../ROADMAP.md).** Written against
`0ef612dd21e8e8a401422eb06e1f31f5197cd4b2` (v0.6.0), before execution groups and
OneForOne supervision landed on main; see [Status since writing](#status-since-writing).
See also the [concurrency ideas research](concurrency-ideas.md) and the
companion [verification strategy](verification-strategy.md).

The roadmap and earlier research already cover metrics, representative
benchmarks, bounded mailboxes, batching, affinity, sharding, runtime ownership,
supervision, propagated deadlines, seeded delivery simulation, causal context,
helper scopes, credits and durable outboxes. This document collects the ideas
that are *not* yet covered, or that push a covered idea substantially further.
Their placement in the backlog is recorded in [ranked backlog](../../../ROADMAP.md#ranked-backlog).

Cost estimates are engineering judgment, not measurements. Before committing to
items 1–3 and 6 in the catalog, check Ethrex and Ethlambda for the patterns they
are meant to replace (see [Validation first](#validation-first)).

## Contents

- [Status since writing](#status-since-writing)
- [The idea catalog](#the-idea-catalog) (42 ideas, grouped; 31–42 from agy's review)
- [Positioning](#positioning)
- [Design notes: request semantics](#design-notes-request-semantics)
- [Design notes: per-key entities and state](#design-notes-per-key-entities-and-state)
- [Design notes: live introspection](#design-notes-live-introspection)
- [Design notes: record in production, replay in simulation](#design-notes-record-in-production-replay-in-simulation)
- [Notes on the per-message allocation cost](#notes-on-the-per-message-allocation-cost)
- [Validation first](#validation-first)
- [Sources](#sources)

## Status since writing

Main moved after this catalog was drafted (commit `12baea41` and the roadmap
update before it):

| Item here | Status on main |
|---|---|
| Execution groups (38) | Implemented: named parked async pools, `start_on(&group)`, owned helpers, ordered teardown. Dedicated/pinned groups and sized blocking pools still to add. |
| Supervision (11) | OneForOne implemented with ChildSpec factories, restart modes, rolling limits, backoff/jitter, ordered startup and reverse shutdown. OneForAll/RestForOne still to add. |
| Fallible `started()` (39) | Implemented for supervised children (fallible startup and readiness). |
| Instance- vs. slot-bound references (42) | Decided: instance-bound references with explicit replacement lookup. |
| Graceful shutdown (12) | Ordered shutdown implemented; no drain policy yet (idea priorities #6). |
| Signal priority lane (4) | Supervision lifecycle observation bypasses the user-message backlog; a generic priority API is deferred (idea priorities #12). |

The verdict on item 38 below ("don't make it a prerequisite") is superseded: the
team chose to build execution groups together with supervision, and did.

### Disagreements with the earlier backlog

The ranking differences this catalog originally recorded (test kit, deadlock
detection, watchdog, snapshots, virtual actors, pub-sub, verification) were
resolved by folding every item into [ranked backlog](../../../ROADMAP.md#ranked-backlog).

## The idea catalog

Items marked *(roadmap)* or *(research)* are already tracked elsewhere; they are
listed so the catalog is complete. Numbers are catalog identifiers, not ranks.

### A. Request and reply semantics

| # | Idea | What it gives users | Cost |
|---|---|---|---|
| 1 | **Deferred replies** (`Reply::Ready` / `Reply::Defer`) | A handler hands off its reply sender and returns, so one slow `await` no longer freezes the mailbox. Callers are unchanged. | Medium |
| 2 | **Stash / postpone** (`Reply::Stash`) | Set aside messages that arrive too early (future-slot blocks, pre-handshake traffic) and replay them in arrival order on a state change. Needs a size limit and drop policy. | Low–medium |
| 3 | **Request deadlock detection** | A request cycle (A waits on B waits on A) fails immediately with `WouldDeadlock` naming the cycle, instead of hanging until a timeout. Designed together with 1. | Low–medium |
| 4 | **Priority lane for system signals** | Exit and stop signals skip ahead of queued ordinary messages. Needed for supervision under load. | Medium |
| 5 | **Propagated deadlines** *(research)* | Skip work whose caller has already given up, along a chain of actors. | Medium |

### B. Building blocks (the OTP equivalents)

| # | Idea | What it gives users | Cost |
|---|---|---|---|
| 6 | **Per-key entities** (Orleans-style virtual actors, in-process) | Address actors by key; created on first message, stopped when idle, recreated on demand, with load/save hooks and a safe handoff between instances. | ~1 week |
| 7 | **Groups / pub-sub** (Erlang `pg`) | Named topics with automatic removal of dead members. Broadcasting to all peers becomes one line. | Low |
| 8 | **Pools / routers** | N workers behind one reference, routed round-robin, least-busy (mailbox depth is tracked by the metrics feature) or by key hash (preserves per-key order). | Low–medium |
| 9 | **Typed state-machine actor** (`gen_statem`) | Handlers grouped by state, per-state timeouts, postpone built in. Fits handshakes, sync phases and consensus rounds. | Medium |
| 10 | **Handler middleware** (Tower-style) | Tracing, rate limits, timeouts, retries, dedup and metrics without editing handlers. Deadlines and tracing can be implemented as middleware. | Medium |
| 11 | **Supervision** *(roadmap)* | Restart strategies, limits, backoff, escalation. | High |
| 12 | **Graceful shutdown in one call** | On SIGTERM: stop admission, drain with a deadline, stop the actor tree in dependency order. | Low–medium |
| 13 | **Ready-made integrations** | `Recipient<M>` as a stream sink, actors behind Axum/Tonic as Tower services, a standard connection-owning actor with reconnect and backoff. | Low each |

### C. Testing and correctness

| # | Idea | What it gives users | Cost |
|---|---|---|---|
| 14 | **Test kit** | `TestProbe` recipients ("this actor sent X within 100 ms"), stub actors with canned replies, controllable time for `send_after`. | Low |
| 15 | **Deterministic simulation backend** *(research, extended)* | Single thread, a seed picks the next runnable actor, virtual time jumps to the next timer, fault injection. Failures print as a Mermaid sequence diagram. | Medium–high |
| 16 | **Systematic exploration of message orderings** | Explore every ordering that matters (partial-order reduction over deliveries) instead of random seeds; offered to users as `sim::explore(...)`. | High (research) |
| 17 | **Formal lifecycle model** | TLA+ (or P) model of links, monitors, trapped exits and stop, with trace validation against the real code. See [verification strategy](verification-strategy.md). | Medium |
| 18 | **Record in production, replay in simulation** | Record ingress plus mailbox delivery order into a bounded black box; replay exactly. See [design notes](#design-notes-record-in-production-replay-in-simulation). | High |
| 19 | **Compile-time actor graph** | Macros emit which actor sends what to whom: diagrams for free and a static request-cycle check. | High (research) |

### D. Observability and debugging

| # | Idea | What it gives users | Cost |
|---|---|---|---|
| 20 | **Live introspection** (Erlang `observer` / `sys`) | List actors and the parent/child tree, mailbox depth, the running handler and its age, a `Debug` state snapshot; tokio-console integration. | Medium |
| 21 | **Stall watchdog** (upgrade `WarnOnBlocking`) | Separate "one poll blocked a worker" from "handler stuck across awaits", name the actor and message type, optional in release builds. | Low |
| 22 | **Flight recorder** | Last N handled message types and timestamps per actor, dumped on panic, stall or timeout. The first building block for 18. | Low |
| 23 | **Causal tracing context** *(research)* | Follow one ingress request through actor fan-out. | Low–medium |

### E. Performance (profile a client workload first)

| # | Idea | What it gives users | Cost |
|---|---|---|---|
| 24 | **Generated per-actor message enum** | Removes the boxed envelope and boxed handler future per message for protocol actors. Large messages are boxed inside the enum. | Medium |
| 25 | **Reusable reply slots** | Removes the per-request oneshot allocation. | Medium |
| 26 | **Bounded mailboxes, credits, batching, affinity, sharding** *(roadmap / research)* | Only when measurements call for them. | Varies |

### F. Reach

| # | Idea | What it gives users | Cost |
|---|---|---|---|
| 27 | **Tokio-free thread mode, then embedded** (Embassy backend) | Actors on microcontrollers; follows the roadmap's runtime-neutral thread work. | Medium–high |
| 28 | **Runtime for AI agents** | One strong example: supervision, timeouts, cancellation and tracing for long-running, failure-prone LLM agents. | Low (example) |
| 29 | **Distribution** | Remote actors. Per-key entities (6) are the natural path because callers already address by key. | Very high; wait for a user |
| 30 | **Durable outbox and idempotency** *(research)* | Effectively-once side effects across restarts, as a recipe first. | Low (recipe) |

### G. Additional ideas from an independent review (agy)

An independent review by agy, run against the same revision, proposed the
following ideas that are not covered above. Each verdict is ours, after
checking the claim against the code.

| # | Idea | What it gives users | Cost | Verdict |
|---|---|---|---|---|
| 31 | **`ActorRef::mock()`** | A real `ActorRef<B>` backed by closures for chosen message types, so actors keep concrete `ActorRef<B>` fields and tests need no generics or real dependencies. | Low | **Adopt as the core of the test kit (14).** Needs an `Envelope` hook that downcasts to `MessageEnvelope<M>` and yields `msg` and `tx`. Unmocked messages should fail loudly. |
| 32 | **Published state snapshots** (`Projected<S>`) | The actor publishes a read-only snapshot (an `ArcSwap` or `watch`-style slot); callers read the chain head or sync status without waiting in the mailbox behind a long import. | Low | **Adopt.** It replaces the `Arc<RwLock<_>>` escape hatch. Document that a snapshot can lag messages already sent, so it does not give read-your-writes. |
| 33 | **`ctx.spawn_ordered`** (parallel compute, then ordered commit) | Stateless CPU work (BLS, decoding, hashing) runs on a pool; results commit to `&mut self` in arrival order and complete the original caller's reply. | Medium | **Adopt, as a specialization of deferred replies (1).** State can change between submission and commit, so the commit must revalidate. Needs a bound on in-flight work. Ethlambda already does this by hand with `spawn_blocking`. |
| 34 | **Single-flight requests** | Identical in-flight requests share one execution and a cloned reply. | Medium | **Maybe.** Only sound for pure reads. Needs `Hash + Eq` arguments, a `Clone` reply and an index of the mailbox. Prove the thundering-herd problem exists in a client first. |
| 35 | **Latest-wins coalescing and batch draining** | Coalescing overwrites stale queued updates; batching drains up to N messages per turn. | Medium | **Batching was already tracked (catalog item 26); coalescing is new.** Coalescing must be opt-in per message type and never silently drop a request, because a caller is waiting for its reply; superseded requests get an explicit error. |
| 36 | **Queue-wait shedding (CoDel)** | Drop messages marked sheddable when the oldest message has waited longer than a target, instead of using fixed length limits. | Medium | **Good for gossip.** Enqueue timestamps exist only with the `metrics` feature today, so shedding needs its own cheap timestamp. Requires an explicit reject path and counters. |
| 37 | **Typed streams between actors** | Consumer-driven demand and bounded buffering between actors, cancelled when either side exits; replaces unbounded push from `spawn_listener`. | Medium | **Adopt after credits** (concurrency ideas #5). Shares the permit primitive. |
| 38 | **`ActorSystem` and execution groups** | Named, explicitly owned execution pools (shared async, bounded blocking, dedicated threads, pinned shards) with system-wide shutdown. | High | **Design together with supervision (11) and graceful shutdown (12), but don't make it a prerequisite.** The `SPAWNED_EXPERIMENT_*` variables are private experiment knobs, not a user problem. The cited 19.0 ms → 0.040 ms p99 gain comes from a synthetic runtime-isolation benchmark, and the clients already isolate critical actors with `Backend::Thread` and `Backend::Blocking`. |
| 39 | **Fallible `started()`** | `started()` returns `Result` and the actor exits with a reason, instead of panicking or half-starting. | Low | **Adopt.** The `origin/result_on_start` branch exists; review it alongside supervision's child-start semantics. |
| 40 | **Per-actor crash tape** | Record the last N serialized messages one actor handled and replay them into a fresh instance on panic. | Medium | **Adopt as the cheap first step toward record/replay (18), with one fix.** Replaying the last N messages into a *fresh* instance does not reproduce the state those messages were applied to. The tape needs a state snapshot at its start, or must cover the actor since `started()`. Replies from requests the actor made, clock reads and I/O are inputs too, and must be recorded. |
| 41 | **Out-of-process status table** (`spawned-top`) | A shared-memory table that an external CLI reads even when the process is wedged or deadlocked. | Medium–high | **Later.** Already discussed in the [Silver/flux research](silver-flux.md); in-process introspection (20) comes first. |
| 42 | **Instance-bound vs. slot-bound references** | Decide whether an `ActorRef` dies with its instance or points at a stable supervisor-owned slot, so restarts are transparent and in-flight replies fail with an explicit error. | Design decision | **Required input to supervision (11).** Per-key entities (6) already answer it for keyed actors. |

agy also listed items we already track or track elsewhere: actor-level ordering
exploration (16), formal lifecycle model (17), deadlock detection (3), helper
completion scopes and propagated budgets (concurrency ideas #4 and #1), causal
context (23), client workload benchmarks (roadmap milestone 2), per-handler
blocking worker leasing (`benchmarks/RESEARCH-NEXT.md`), Tokio-free thread mode
(27), and a sparse-work driver yield experiment (backend research only).

## Positioning

Rankings were moved to [ranked backlog](../../../ROADMAP.md#ranked-backlog). The positioning
argument remains: the cheap items (test kit with `ActorRef::mock()`, deferred
replies plus stash, deadlock detection, pub-sub, virtual actors) drive adoption,
while the distinctive long-term story is **Erlang semantics plus reproducible
concurrency bugs**: actor-level ordering exploration, then crash tapes, then
record/replay, with failing simulations printed as sequence diagrams.

## Design notes: request semantics

Items 1–3 interact and should be designed as one piece of work.

### Why deferred replies

In `concurrency/src/tasks/actor.rs` (`MessageEnvelope::handle`), the reply is
sent only after `actor.handle(msg, ctx).await` completes:

```rust
let result = actor.handle(self.msg, ctx).await;
if let Some(tx) = self.tx {
    let delivered = tx.send(result).is_ok();
```

While a handler awaits a downstream request, a disk read or `spawn_blocking`
work, the whole mailbox waits, including cheap queries. Moving work to a spawned
task frees the mailbox but loses the typed reply. This is Erlang's
`{noreply, State}` plus `gen_server:reply/2`.

Sketch:

```rust
enum Reply<T> {
    Ready(T),
    Defer,            // handler took ctx.reply_sender() and will answer later
    Stash,            // re-queue this envelope until ctx.unstash_all()
}
```

Decisions to make:

- **Ordering.** Replies may complete out of order relative to the mailbox.
  Callers do not notice; documentation must say so.
- **State.** Background work cannot touch `&mut self`. Results that mutate state
  return as messages; the API should make that the easy path.
- **Dropped senders.** If a deferred sender is dropped or the actor stops, the
  caller gets `ActorStopped`, never a hang. Supervision must define whether
  pending replies survive a restart.
- **Metrics.** Separate handler elapsed time from request-to-reply time.

### Stash

Handlers receive `msg: M` by value, while the reply sender stays in
`MessageEnvelope`. A `ctx.stash(msg)` API would lose the caller's reply, so
stashing must operate on the envelope, e.g. by returning `Reply::Stash`.
Requirements:

- **A size limit and drop policy.** An unbounded stash lets a peer flooding
  future-slot blocks grow memory without limit.
- Stashed requests receive `ActorStopped` when the actor stops.
- Unstashed envelopes keep their original order and enqueue timestamps; define
  whether queue-wait metrics include stash time.

### Deadlock detection

Carrying a chain of waiting actor IDs inside each request detects only cycles
that close along one call chain (A→B, then B's handler calls A). It misses B
already waiting on A while handling a different message, because that fact lives
at B. Use a shared wait-for table (actor → actor it is awaiting) checked when a
request is issued, and report the cycle:
`BlockchainServer(4) → PeerManager(12) → SyncActor(7) → BlockchainServer(4)`.

- `ActorRef::request` takes only `&self`, so the callee does not know the
  caller. Without changing signatures, set a thread-local `CURRENT_ACTOR` only
  while the actor's own handler future is being polled (and around the
  synchronous `handle` call in thread mode). A request created during that poll
  records the edge `caller → target`. Detached helper tasks are not inside that
  poll, so they create no edges and cause no false positives; their awaits do not
  block the actor's mailbox anyway. (Suggested by agy's review.)
- Store a small wait set per actor rather than a single target, so
  `join!(b.request(..), c.request(..))` is represented correctly.
- With deferred replies, "A awaits B" no longer means A's mailbox is blocked;
  only in-handler awaits create edges.
- Consider debug-only by default; measure the cost before enabling in release.

### Signal priority lane

For actors that do not trap exits, an abnormal linked exit already bypasses the
mailbox: it cancels the actor's token (`own_cancel_fn` in `tasks/actor.rs`).
Only actors with `trap_exit(true)`, typically supervisors, receive
`ExitEnvelope` through the ordinary FIFO mailbox (`own_send_exit_fn`), where it
waits behind queued messages. Monitor `Down` messages are also ordinary sends,
issued from a separate task once the target's exit is observed.

The real need is therefore a control lane for system commands (shutdown, kill,
inspect, suspend) and possibly for exits to supervisors. A priority lane must
not break per-sender causal ordering: Erlang guarantees that a message a process
sent before exiting is seen before its exit signal. Today's FIFO preserves that
for messages the exiting actor itself sent before exiting. Messages sent by its
helper tasks have no such ordering, and an exit that jumps the queue would break
even the first case. Options: use the priority lane only for commands that carry
no data ordering, or drain the exiting sender's already-queued messages before
delivering the exit. Model this in the
[lifecycle spec](verification-strategy.md) before implementing. Selective
receive or message priorities are a separate, opt-in extension.

## Design notes: per-key entities and state

### What they are

You address the actor by key, not by reference, and the framework owns its
lifecycle. Without it, every "one actor per peer" codebase writes:

```rust
struct PeerManager {
    peers: HashMap<PeerId, ActorRef<PeerActor>>,
}
// plus monitors to remove dead entries, an idle-eviction timer,
// handling sends to actors that died after lookup, and routing every
// caller through PeerManager.
```

Each of those is a bug source: leaked dead entries, duplicate creation under
concurrent callers (or a manager bottleneck to avoid it), stale references, and
memory proportional to every key ever seen.

With entities:

```rust
let peers = Entities::<PeerActor>::new(|id| PeerActor::new(id))
    .idle_timeout(Duration::from_secs(600));

peers.get(peer_id).send(msg)?; // from anywhere
```

Created on first message, exactly once; stopped when idle; recreated on demand;
addressable from anywhere by key. Fits peers, sync requests, slots/epochs,
mempool senders and RPC subscriptions.

### State

Memory is a cache; anything that must survive lives elsewhere.

| Strategy | How | Survives idle stop | Survives crash | Example |
|---|---|---|---|---|
| Ephemeral | Start empty | no | no | A sync request |
| Reload from source of truth | `activate` reads DB/chain | yes | yes | Chain-derived state |
| Save on stop | `deactivate` writes state | yes | no | Peer scores |
| Write through | Persist before replying | yes | yes | Anything acknowledged |
| Never stop | Disable idle stop or pin a key | n/a | no | Expensive in-memory state |

Rule: **if losing it would be a bug, persist it before replying or
acknowledging.** Save-on-stop is an optimization for losable state.

```rust
impl Entity for PeerActor {
    type Key = PeerId;
    async fn activate(key: PeerId, deps: &Deps) -> Self { /* load */ }
    async fn deactivate(&mut self, deps: &Deps) { /* save; not called on crash */ }
}
```

Framework guarantees:

1. **One live instance per key.**
2. **Ordered handoff.** A new instance for key X waits until the previous
   instance's `deactivate` has finished, so it never loads stale state.
3. **Crash discards state and reloads.** Half-updated memory is thrown away.
4. **Defined outcome for requests caught mid-stop**: forwarded to the new
   instance or failed, never silently lost.

Limits: stopped entities run no timers, so proactive behavior needs a pinned
entity; a crash can lose queued messages exactly as today.

### Prior use

Orleans has run in production at Microsoft since 2011 and is best known as the
basis of the Halo 4 cloud services. Azure shipped Service Fabric Reliable
Actors, based heavily on the Orleans API. EA BioWare's Orbit was inspired by it.
The same pattern exists as Akka Cluster Sharding, Dapr Actors and Proto.Actor
virtual actors (not verified for this document). Spawned would start in-process;
the distributed half is not needed for the benefits above.

## Design notes: live introspection

The handler future borrows `&mut A` exclusively for the whole handler, including
while it is suspended at an `.await`, which is exactly when you most want to
inspect a stuck actor. So a `Debug` snapshot of actor state (Erlang's
`sys:get_state`) can only be taken **between handler turns**, serviced by the run
loop through a control message. What can be read at any time is metadata kept
outside the actor: id, type name, parent, links and monitors, mailbox depth,
status (idle, polling, suspended at an await), the current message type, how
long the current turn has been running, and the wait-for edges from deadlock
detection. (Point raised by agy's review.)

## Design notes: record in production, replay in simulation

**Why:** in Ethereum clients the expensive part of a bug is reproducing it. Record
and replay turns a one-off mainnet race into a permanent test, builds a corpus of
real traffic for regression tests, and allows performance comparisons on real
traffic.

**Tractable because:** only ingress is recorded (network messages, timer firings,
later clock and randomness reads). Internal messages are regenerated by running
the same code, so only ingress types must be serializable.

**Races:** identical inputs do not imply identical execution. Also record the
delivery order, a cheap `(actor_id, seq)` per dequeue with no payload, and force
that order on replay. This is `rr`'s approach applied at actor granularity.

**Hard parts:**

1. **Every nondeterminism source must go through the framework.** Direct
   `tokio::net`, `SystemTime::now()`, `rand` or DB reads escape the recording.
   Needs `ctx.now()`, `ctx.rng()`, recorded listeners, and `spawn_blocking`
   results treated as inputs. This is a discipline change for client code and
   the largest risk.
2. **Starting state.** Replay from genesis is impossible; a recording needs a DB
   snapshot plus the log from that point.
3. **Volume.** Mainnet gossip is gigabytes per hour. Use a black-box ring buffer
   of the last N minutes, written out on panic, stall or failed invariant.
4. **Divergence detection.** A hash per handler call reports
   "diverged at actor 12, message 4,031" when code changed since recording.

**A cheaper first step: the per-actor crash tape (40).** One actor's state is
determined by its starting state and the inputs it handled. Those inputs include
replies to requests it made, clock reads and I/O, not just incoming messages. A
tape that records all of those, plus a state snapshot at the start of the tape
(or a tape covering the actor since `started()`), can replay one actor's crash in
a unit test without simulating the rest of the system. It does not reproduce
cross-actor races, which is what full record/replay is for.

**Minimum version:**

1. Deterministic simulation backend (15).
2. Opt-in `Recordable` trait for ingress; record at framework-controlled points:
   `spawn_listener`, `send_after` / `send_interval`, requests from outside actors.
3. Record and enforce delivery order; add divergence hashes.
4. **Go/no-go:** reintroduce a known race (e.g. the PR #182 shutdown race or a
   known Ethlambda race), record it in a stress run, and check it replays every
   time. Ethlambda is the better first target: smaller, consensus code, inputs
   mostly gossip and slot timers.

Prior art: Temporal workflow history replay (closest design), `rr`,
FoundationDB simulation, Antithesis. No actor framework found offers this as a
built-in feature; this was not an exhaustive survey.

## Notes on the per-message allocation cost

At this revision a request performs three heap allocations in
`concurrency/src/tasks/actor.rs`: the oneshot reply channel, the
`Box<dyn Envelope<A>>` in `enqueue`, and the `Box::pin` of the handler future in
`MessageEnvelope::handle` on every dequeue. A send performs the latter two.

- A macro-generated enum per protocol actor removes both boxes; the match yields
  a concrete future. `Recipient<M>` still needs type erasure, and the enum is as
  large as the largest message, so large variants should be boxed.
- Fusing envelope and future into one allocation requires unsafe
  self-referential code, because the future borrows `&mut A`. Not recommended.
- Profile a client workload before doing any of this, per the roadmap.

## Validation first

Before prioritizing items 1–3 and 6, search Ethrex and Ethlambda actor code for:

- hand-built pending queues (`VecDeque` of deferred messages) → stash;
- hand-written per-key actor maps (`HashMap<_, ActorRef<_>>`) → entities;
- requests awaited inside handlers → deferred replies and deadlock detection;
- manual broadcast lists of recipients → groups.

The counts and the incidents behind them should reorder this list more than
the estimates above.

## Sources

- [Orleans Simplifies Development of Scalable Apps for the Cloud (Microsoft Research)](https://www.microsoft.com/en-us/research/blog/orleans-simplifies-development-scalable-apps-cloud/)
- [Orleans project page (Microsoft Research)](https://www.microsoft.com/en-us/research/project/orleans/)
- [Project Orleans, the .NET framework used in Halo 4 (The New Stack)](https://thenewstack.io/project-orleans-the-net-framework-from-microsoft-research-used-in-halo-4/)
- [Building the Halo 4 services with Orleans (Speaker Deck)](https://speakerdeck.com/caitiem20/building-the-halo-4-services-with-orleans-oredev-2014)
- [Erlang `gen_server:reply/2`](https://www.erlang.org/doc/apps/stdlib/gen_server.html#reply/2)
- [Erlang `gen_statem` postpone](https://www.erlang.org/doc/apps/stdlib/gen_statem.html)
- [Erlang `pg` process groups](https://www.erlang.org/doc/apps/kernel/pg.html)
- [Ractor factories](https://docs.rs/ractor/latest/ractor/factory/index.html)
- [Temporal: deterministic workflow replay](https://docs.temporal.io/workflows#deterministic-constraints)
- [rr record and replay](https://rr-project.org/)
