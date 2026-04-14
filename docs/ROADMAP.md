# Spawned Roadmap

## Phase 1: Core Actor Framework — ✅ v0.4

- `Actor` trait with `started()` / `stopped()` lifecycle
- `ActorRef<A>` for communication (`request()` and `send()`)
- Dual execution modes (async tasks / sync threads)
- Timers (`send_after`, `send_interval`)
- Stream processing
- Signal handling via `send_message_on()`

## Phase 2: Type-Safe Multi-Message API — ✅ v0.5

Solved the two critical API issues (#144, #145) that blocked real-world usage:

- `Handler<M>` pattern — per-message type safety, no more `unreachable!()` arms
- `Recipient<M>` — type-erased handles, breaking circular dependencies between actors
- `#[protocol]` macro — generates message structs, blanket impls, and `XRef` type aliases from a trait definition
- `#[actor]` macro — derives `Actor` + `Handler<M>` boilerplate
- Named registry — global actor lookup by name

## Phase 3: Supervision Trees — in progress

The missing piece for production fault tolerance. Target: v1.0.0.

Following Erlang/OTP's proven design: supervisors link to children, trap exit signals, and apply restart policies. See `openspec/changes/supervision-trees/` for the full design and specs.

### 3a. Exit Reasons — ✅ [PR #163](https://github.com/lambdaclass/spawned/pull/163)

- `ExitReason` enum (`Normal`, `Shutdown`, `Panic(String)`, `Kill`) with `is_abnormal()`
- `ActorRef::wait_exit()` and `ActorRef::exit_reason()` to observe why an actor stopped
- Both tasks and threads modes

### 3b. Links and Trap Exit — next

- **Bidirectional links** ([#131](https://github.com/lambdaclass/spawned/issues/131)) — linked actors die together (fate-sharing); supervisors trap exits to receive `Exit` messages instead
- **Atomic `start_linked(ctx)`** — prevents race between spawn and link
- **`ctx.trap_exit(true)`** — converts exit signals into `Exit` messages via `Handler<Exit>`
- **Kill is untrappable** — `ExitReason::Kill` bypasses trap_exit

### 3c. Child Specs and Supervisor

- **Child specs** ([#132](https://github.com/lambdaclass/spawned/issues/132)) — factory pattern with restart type (`Permanent`, `Transient`, `Temporary`) and shutdown type (`BrutalKill`, `Timeout`, `Infinity`)
- **Supervisor actor** ([#133](https://github.com/lambdaclass/spawned/issues/133)) — `start_linked()` + `trap_exit` + `Handler<Exit>`, with strategies: OneForOne, OneForAll, RestForOne
- **Meltdown protection** — sliding window restart counter; supervisor self-terminates when exceeded
- **Dynamic supervisor** ([#134](https://github.com/lambdaclass/spawned/issues/134)) — add/remove children at runtime (stretch goal)
- **Error handling** ([#125](https://github.com/lambdaclass/spawned/issues/125)) — proper error propagation for channel send operations

## Phase 4: Documentation & Polish — pre-v1.0.0 release

- Comprehensive API docs
- Supervision and protocol guides
- Doc tests in crate READMEs ([#137](https://github.com/lambdaclass/spawned/issues/137))
- End-to-end examples (chat server, job queue, etc.)

## Future Considerations (post-v1.0)

| Feature | Notes |
|---------|-------|
| Monitors | Unidirectional actor observation (lighter than links) |
| Process groups (pg) | Erlang-style actor grouping |
| Priority message channels | Signal > Stop > Supervision > Message |
| State machines (`gen_statem`) | Protocol implementations |
| Backoff strategies | Built into supervision (Akka pattern) |
| Persistence / event sourcing | Akka Persistence pattern |
| Clustering / distribution | `ractor_cluster` equivalent |

## References

- PR #153: v0.5 implementation
- PR #154: Design research and framework comparison docs
- PR #163: Exit reason tracking (Phase 3a)
