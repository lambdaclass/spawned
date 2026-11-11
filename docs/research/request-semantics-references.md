# Request semantics: references from object-capability and choreographic systems

Research note, 2026-11-09. Design inputs for rank 11 (deferred replies,
forwarding, pipe-to-self) through rank 11a, and for rank 47 (session types)
through rank 47a, in the [roadmap](../../ROADMAP.md#step-4-request-semantics).
The existing rank 11 design notes (why deferred replies, delegation versus
promises, CAF and Pekko references) are in
[framework ideas](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/docs/research/framework-ideas.md#design-notes-request-semantics);
this note adds three ideas from other traditions.

## What it is

- **Promise pipelining** (E, Cap'n Proto, Spritely Goblins): send a message to
  the *result* of a request before that result exists.
- **Transactional turns** (Goblins): a handler's state changes and outgoing
  messages take effect only if the handler completes; on an error they are
  discarded.
- **Time-travel debugging** (Goblins): because each turn is a transaction over
  a snapshot-able heap, a debugger can step back through earlier states.
- **Choreographic programming** (ChoRus, HasChor): write one global program
  for all participants and project it into per-participant code.
- **Reference capabilities** (Pony): type qualifiers that make data races
  impossible across actors.

## How it works

### Promise pipelining (E, Cap'n Proto, Goblins)

Cap'n Proto describes it as "time travel": in `bar(foo())`, the client sends
the call to `foo` and then the call to `bar` on its not-yet-returned result
together, so a chain costs one round trip instead of two. "The results of an
RPC call are returned to the client instantly, before the server even receives
the initial request!" The limit: a pipelined result can only be used in new
requests to the same server. The protocol is "based heavily on CapTP, the
distributed capability protocol used by the E programming language"
([Cap'n Proto RPC](https://capnproto.org/rpc.html)). Goblins offers the same:
"Make me a car, and as soon as that car is ready, I want to drive it!"
([Goblins: promise pipelining](https://files.spritely.institute/docs/guile-goblins/latest/Promise-pipelining.html)).

### Transactional turns and time travel (Goblins)

Goblins groups objects in *vats* (event loops); messages to a vat "are queued
and then handled one turn at a time". Within a turn, `$` calls are synchronous
and `<-` sends are asynchronous; the turn's synchronous effects form one
transaction. "If an unhandled error occurs", "any state changes resulting from
synchronous operations within that transaction will simply not be committed",
and asynchronous messages "are only sent if the turn completes successfully"
([transactions](https://files.spritely.institute/docs/guile-goblins/latest/Transactions-make-errors-survivable.html);
[vat model](https://files.spritely.institute/docs/guile-goblins/latest/The-_0060_0060vat-model_0027_0027-of-computation.html)).
Goblins "uses transactional heaps to manage actor state, enabling us to take
snapshots of a vat before each event is processed"; its debugger walks back
through those snapshots. Logging is opt-in per vat because of its cost
([distributed debugger, April 2023](https://spritely.institute/news/introducing-a-distributed-debugger-for-goblins-with-time-travel.html)).

### Choreographic programming (ChoRus, HasChor)

A *choreography* describes the whole interaction ("Alice sends X to Bob, Bob
computes Y, Bob sends Y to Carol") as one program; *endpoint projection*
generates each participant's code. Because sends and receives are generated in
matching pairs, the projected programs cannot disagree about who sends what
next; in the choreography literature this is the source of deadlock freedom by
construction (the ChoRus pages we read do not state that guarantee for
ChoRus).

- **ChoRus** is a Rust library (`chorus_lib`, 0.5.0) using
  "End-point Projection as Dependency Injection", macros for locations, located
  values and pluggable transports (built-in `Local` and `HTTP`)
  ([ChoRus](https://lsd-ucsc.github.io/ChoRus/);
  [arXiv 2311.11472](https://arxiv.org/abs/2311.11472)).
- **HasChor** is a Haskell library expressing choreographies as monadic
  computations, under 150 lines of core code
  ([Shen, Kashiwa and Kuper, ICFP 2023](https://arxiv.org/abs/2303.00924)).

Session types, by contrast, *type-check* each participant's hand-written code
against a protocol type. Choreographies *generate* the participants' code from
one description.

### Pony reference capabilities

Pony marks every reference with a capability: `iso` (sole reference, may be
sent), `val` (immutable, shareable), `ref` (mutable, actor-local), `box`
(read-only view), `trn` (writable, convertible to `val`) and `tag` (identity
only). "By sharing only immutable data and exchanging only isolated data we can
have safe concurrent programs without locks"
([Pony tutorial](https://tutorial.ponylang.io/reference-capabilities/reference-capabilities.html)).
Rust already provides the same split through ownership, `Send` and `Sync`:
moving a value into a message is `iso`, an `Arc<T>` with `T: Sync` and no
interior mutability is close to `val`. Nothing to adopt; useful vocabulary when
documenting what a message may carry.

## Who uses it

- **Cap'n Proto**: we did not verify named users for this note; none are
  claimed here.
- **Goblins** is developed by the Spritely Institute; we found no production
  users to cite.
- **ChoRus, HasChor** are research libraries.
- **Pony** has a small user base; no users are claimed here.

## What it means for Spawned

**Rank 11a: design inputs to rank 11.**

1. **Pipelining as forwarding.** Rank 11 must already distinguish
   delegation (transfer the reply obligation) from a retained promise. A
   pipelined call is a third case: "send M to whatever the reply to R names".
   In-process there is no round trip to save, but the shape matters for actors
   that request a handle and immediately use it (ethrex asks the peer table for
   a peer, then sends to that peer's connection actor). Decide whether the
   rank 11 API can express "on reply, forward to the reply's target" without a
   handler turn in between, and what happens when R fails (the pipelined
   message must fail with the same error, never be dropped). For rank 44
   (distribution), pipelining is the main latency tool; keep the option open
   rather than building it now.
2. **Transactional turns as an opt-in policy.** Today a panicking handler
   discards the instance under supervision, and messages the handler already
   sent stay sent. Goblins instead holds outgoing sends until the turn commits.
   For Spawned that would mean buffering `send` calls inside a handler and
   flushing them on success, discarding them on panic; state rollback would
   need snapshot-able actor state (`Clone` or a persistent structure) and
   cannot cover effects outside the actor (I/O, helpers, `spawn_blocking`).
   The [stage-then-commit guideline](../../ROADMAP.md#corrections-to-the-proposed-descriptions)
   is the documentation-only version of this. Proposal: specify in the rank 11
   design whether deferred replies and pipe-to-self completions are sent
   *at the end of the turn* (Goblins-like) or *immediately*; the TLA+ model
   for rank 11 ("every accepted request gets exactly one reply, including on
   owner exit or panic") must then cover a reply sent by a turn that later
   panics.
3. **Time travel needs the same snapshots.** Per-turn state snapshots would
   also serve the crash tape (rank 21, which needs a state snapshot at the tape
   start) and read-only inspection (rank 30, state only between turns). If
   rank 11a's transactional option requires `Clone` state, the crash tape can
   reuse it.

**Rank 47a: choreographies next to session types.** For a fixed multi-party
exchange (a handshake between a connection actor, the peer table and the
sync actor), a choreography macro that generates each actor's handlers may
fit Spawned better than session-typed channels, because it targets the
existing typed messages and `Handler<M>` impls instead of a new channel type.
Same limits as rank 47: it covers a fixed protocol among known roles, not
dynamic actors, failures or blocking handlers. Research only; ChoRus is the
Rust reference to read first.

## Sources

- [Cap'n Proto RPC protocol](https://capnproto.org/rpc.html)
- [E: promise pipelining](http://www.erights.org/elib/distrib/pipeline.html) (not loaded during checking; cited via Cap'n Proto)
- Spritely Goblins: [promise pipelining](https://files.spritely.institute/docs/guile-goblins/latest/Promise-pipelining.html), [transactions make errors survivable](https://files.spritely.institute/docs/guile-goblins/latest/Transactions-make-errors-survivable.html), [vat model](https://files.spritely.institute/docs/guile-goblins/latest/The-_0060_0060vat-model_0027_0027-of-computation.html), [distributed debugger with time travel](https://spritely.institute/news/introducing-a-distributed-debugger-for-goblins-with-time-travel.html)
- [ChoRus](https://lsd-ucsc.github.io/ChoRus/), [Portable, Efficient, and Practical Library-Level Choreographic Programming](https://arxiv.org/abs/2311.11472)
- [HasChor: Functional Choreographic Programming for All (ICFP 2023)](https://arxiv.org/abs/2303.00924)
- [Pony reference capabilities](https://tutorial.ponylang.io/reference-capabilities/reference-capabilities.html)
