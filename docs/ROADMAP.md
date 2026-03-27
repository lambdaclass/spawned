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

## Phase 3: Supervision Trees — next

The missing piece for production fault tolerance. Target: v1.0.0.

- **Links and monitors** ([#131](https://github.com/lambdaclass/spawned/issues/131)) — bidirectional failure propagation and unidirectional observation
- **Child specs** ([#132](https://github.com/lambdaclass/spawned/issues/132)) — factory pattern for configuring supervised children
- **Supervisor actor** ([#133](https://github.com/lambdaclass/spawned/issues/133)) — manages child actor lifecycles with restart strategies
  - OneForOne — restart only the failed child
  - OneForAll — restart all children when one fails
  - RestForOne — restart the failed child and all children started after it
- **Dynamic supervisor** ([#134](https://github.com/lambdaclass/spawned/issues/134)) — add/remove children at runtime
- **Supervision trees** — nested supervisors forming a hierarchy
- **Meltdown protection** — rate-limit restarts to prevent infinite restart loops
- **Error handling** ([#125](https://github.com/lambdaclass/spawned/issues/125)) — proper error propagation for channel send operations

## Phase 4: Documentation & Polish — pre-v1.0.0 release

- Comprehensive API docs
- Supervision and protocol guides
- Doc tests in crate READMEs ([#137](https://github.com/lambdaclass/spawned/issues/137))
- End-to-end examples (chat server, job queue, etc.)

## Future Considerations (post-v1.0)

| Feature | Notes |
|---------|-------|
| Process groups (pg) | Erlang-style actor grouping |
| Priority message channels | Signal > Stop > Supervision > Message |
| State machines (`gen_statem`) | Protocol implementations |
| Backoff strategies | Built into supervision (Akka pattern) |
| Persistence / event sourcing | Akka Persistence pattern |
| Clustering / distribution | `ractor_cluster` equivalent |

## References

- PR #153: v0.5 implementation
- PR #154: Design research and framework comparison docs
