# Proposed capability examples

This tracks example coverage and future work; rows with runnable links have an initial implementation on main (unreleased). The current implementation
order is [the roadmap](../../docs/ROADMAP.md); the [ranked research backlog](../../docs/research/idea-priorities.md)
explains value, dependencies and sources. Ranks are planning priorities, not
independent sequential implementation steps. The rank column uses the backlog's
current numbering.

Every implemented capability should ship a small executable scenario that shows
both its useful behavior and the important failure/overload limit. Prefer a
program with assertions over a snippet that only compiles. Keep experimental
backend drivers and their performance evidence under `benchmarks/` and
`research/`; link them from examples when relevant.


## Existing features without a dedicated example

Every way of using the library should have a runnable example with assertions,
in both execution modes where the feature exists in both. These features ship
today without one (2026-11-09):

| Feature | Example to add |
|---|---|
| Stream listeners (`spawn_listener`) | Feed an actor from a stream, stop it mid-stream, and show the listener ending with the actor; note that the input is unbounded today. |
| `Recipient<M>` type erasure | Several actor types behind one `Recipient<M>` list (a simple broadcast), including a member that has stopped. |
| Request timeouts | `request_with_timeout`, the caller-side timeout versus work that keeps running, and an actor busy past the default. Pair with rank 5a once per-method timeouts exist. |
| Monitors and links in thread mode | The tasks-mode `exit_reason` example has no thread-mode counterpart; cover monitor, link, `trap_exit` and `start_linked` in both modes. |
| Name registration | `register_name` released with its generation versus the global `registry::register`, showing a replacement re-registering. |
| Testing actors | How to unit-test an actor today (a fake actor implementing the same protocol); replace with the test kit (rank 6) when it lands. |

## Capability examples for planned work

New examples the client evidence asks for, beyond the table below:

| Backlog rank | Capability | Example to deliver with it |
|---|---|---|
| 5a | Per-method request timeout | A protocol with a slow method configured above the default and a per-call override; a timed-out call that is handled, not fatal. |
| 6 | Test kit | Unit-test an actor that holds a concrete `ActorRef<B>` by mocking `B`. |
| 7 | Deadlock detection | Two actors requesting each other fail fast with the named cycle. |
| 8 | Stall watchdog | A handler blocking a worker versus one stuck across an `.await`, each reported with actor and message type. |
| 10 | Published snapshots | Readers keep answering from a snapshot while the owner runs a long handler. |

## Previously proposed capability examples

| Backlog rank | Proposed capability | Example to deliver with it |
|---|---|---|
| Implemented | Runtime ownership / execution groups | Critical router and background workers in named groups; partial startup failure; explicit stop/join. [Owned groups example](../capabilities/src/bin/execution_groups.rs); dedicated/pinned group profiles remain planned. |
| Implemented; 23 | Supervision, paired with execution groups | OneForOne with child factories, restart modes and budget/backoff; restart in the same group, show an old reference failing and resolve the replacement explicitly. [Recovery example](../capabilities/src/bin/supervision_recovery.rs) is implemented; integration tests also stop during backoff. OneForAll/RestForOne remain planned. |
| 25 | Tokio-free threads | Run protocol requests, timer, monitor and shutdown without Tokio in the dependency graph. Existing native-thread examples still compile Tokio. |
| 12 | Lifecycle-owned registries (managed generation guards implemented) | Register a service, crash it and verify its entry is removed without deleting a replacement's registration. |
| Implemented | Fallible startup / readiness (factory Results and managed readiness implemented) | Fail initialization and report the error before admitting work; contrast successful readiness. |
| 13 | Coordinated shutdown | Close admission, drain dependencies within a deadline and report unfinished work. [Finite-batch recipe works today](../capabilities/src/bin/graceful_drain.rs). |
| 11 | Deferred replies / delegation / pipe-to-self | Route a slow request while accepting another message; bound pending replies and demonstrate owner exit and late completion. |
| 17 | Bounded mailboxes | A slow consumer reaches capacity; demonstrate explicit rejection and cancellable waiting without blocking actor progress. |
| 18 | Byte/work credits | Reserve before decoding a large payload; pass credit across stages and reclaim it after rejection/panic. [Payload-budget adapter works today](../capabilities/src/bin/byte_budget_pipeline.rs). |
| 5a, 19 | Inherited deadlines | Exhaust an ingress budget and skip stale queued work without granting downstream calls fresh time. [Current timeout behavior is demonstrated here](../capabilities/src/bin/timeout_outcomes.rs). |
| 15 | Actor-owned helpers (managed scope implemented) | Stop an actor with active helpers, reject new helper admission and await owned cleanup to a deadline. |
| 14 | Fair control path | Observe lifecycle control behind a data backlog while checking ordinary work still progresses; no handler preemption. |
| 28 | Causal tracing | Follow one ingress through fan-out/fan-in, preserving origin across awaits and runtime boundaries. |
| 29 | Bounded cohort metrics | Separate busy importers from idle peers with a bounded label set and correct gauge cleanup. [Aggregate metrics work today](../capabilities/src/bin/framework_metrics.rs). |
| 33 | Typed streams / completion watermarks | Pull a bounded stream, stall an early item, then show released ownership differs from explicit successful acknowledgement. |
| 34 | Blocking worker leasing | Thousands of mostly idle actors share a bounded blocking pool while each actor stays serial. |
| 35 | Batching | Batch database writes up to a count/time budget; preserve documented ordering and individual reply/failure accounting. |
| 36 | Coalescing | Replace obsolete head notifications using bounded keys; demonstrate that transactional requests are never silently coalesced. |
| 38 | Timer policies | Pause a consumer, then compare Skip/Delay/Burst and cancellation against a controlled clock. |
| 27 | Groups / topics / routers | Join workers, route requests, remove crashed members and report partial broadcast failures. |
| 16, 32 | State machines / bounded stashing | Queue a bounded number of messages while connecting; un-stash on readiness and reject overflow. |
| 30 | Read-only inspection | Inspect last-known status and a bounded redacted history while a handler is busy. |
| 37 | Generated protocol dispatch | Keep the same caller behavior across generated and manual handlers; measure allocations separately from useful work. |
| 39 | CPU service classes | Saturate a bulk class and show a critical class's measured service share under a fixed CPU budget. |
| 42 | Delay-based shedding | Compare simple admission and CoDel-inspired shedding on explicitly droppable messages; report losses and queue delay. |
| 26 | External flight recorder | Read a hung fixture's bounded snapshot/events from a separate process; show sequence gaps and redaction. |
| 27 | Sparse-work driver yield | Replay sparse bursts; compare wake latency and idle CPU, including a missed-wakeup check. |
| 28 | Session types | Demonstrate a valid two-party conversation and a compile-fail invalid sequence, with the proof's limits stated. |
| 29 | Fair join patterns | Match votes by round with bounded unmatched storage, cancellation and competing ready matches. |
| 30 | Durable outbox / idempotency | Crash after committing intent, retry and deduplicate effects using application storage; no arbitrary-envelope persistence. |
| 31 | Durable reminders | Restart a process and demonstrate missed-tick and duplicate-delivery policy against persisted state. |
| 32 | Distribution | Authenticate peers, reject incompatible wire versions and demonstrate a partition's ambiguous request outcome. |
| 33 | Virtual actors | Activate/deactivate a logical identity and demonstrate replacement, persistence and concurrent-activation policy. |
| 34 | Durable workflows | Replay persisted decisions after a crash and distinguish replayed decisions from external effects. |

Full deterministic environment simulation remains research. Existing controlled
handler-progress replays, failure windows and request-history checks are in
[`concurrency/tests`](../../concurrency/tests) and documented in
[testing](../../docs/TESTING.md); they do not implement the future capabilities
listed above. A real Ethrex/Ethlambda workload should validate runtime and
admission choices alongside implementation, not wait for the entire list.
