# Proposed capability examples

This tracks example coverage and future work; rows with runnable links have an initial implementation on main (unreleased). The current implementation
order is [the roadmap](../../docs/ROADMAP.md); the [ranked research backlog](../../docs/research/idea-priorities.md)
explains value, dependencies and sources. Ranks are planning priorities, not
independent sequential implementation steps.

Every implemented capability should ship a small executable scenario that shows
both its useful behavior and the important failure/overload limit. Prefer a
program with assertions over a snippet that only compiles. Keep experimental
backend drivers and their performance evidence under `benchmarks/` and
`research/`; link them from examples when relevant.

| Rank | Proposed capability | Example to deliver with it |
|---|---|---|
| 1 | Runtime ownership / execution groups | Critical router and background workers in named groups; partial startup failure; explicit stop/join. [Owned groups example](../capabilities/src/bin/execution_groups.rs); dedicated/pinned group profiles remain planned. |
| 2 | Supervision, paired with execution groups | OneForOne with child factories, restart modes and budget/backoff; restart in the same group, show an old reference failing and resolve the replacement explicitly. [Recovery example](../capabilities/src/bin/supervision_recovery.rs) is implemented; integration tests also stop during backoff. OneForAll/RestForOne remain planned. |
| 3 | Tokio-free threads | Run protocol requests, timer, monitor and shutdown without Tokio in the dependency graph. Existing native-thread examples still compile Tokio. |
| 4 | Lifecycle-owned registries (managed generation guards implemented) | Register a service, crash it and verify its entry is removed without deleting a replacement's registration. |
| 5 | Fallible startup / readiness (factory Results and managed readiness implemented) | Fail initialization and report the error before admitting work; contrast successful readiness. |
| 6 | Coordinated shutdown | Close admission, drain dependencies within a deadline and report unfinished work. [Finite-batch recipe works today](../capabilities/src/bin/graceful_drain.rs). |
| 7 | Deferred replies / delegation / pipe-to-self | Route a slow request while accepting another message; bound pending replies and demonstrate owner exit and late completion. |
| 8 | Bounded mailboxes | A slow consumer reaches capacity; demonstrate explicit rejection and cancellable waiting without blocking actor progress. |
| 9 | Byte/work credits | Reserve before decoding a large payload; pass credit across stages and reclaim it after rejection/panic. [Payload-budget adapter works today](../capabilities/src/bin/byte_budget_pipeline.rs). |
| 10 | Inherited deadlines | Exhaust an ingress budget and skip stale queued work without granting downstream calls fresh time. [Current timeout behavior is demonstrated here](../capabilities/src/bin/timeout_outcomes.rs). |
| 11 | Actor-owned helpers (managed scope implemented) | Stop an actor with active helpers, reject new helper admission and await owned cleanup to a deadline. |
| 12 | Fair control path | Observe lifecycle control behind a data backlog while checking ordinary work still progresses; no handler preemption. |
| 13 | Causal tracing | Follow one ingress through fan-out/fan-in, preserving origin across awaits and runtime boundaries. |
| 14 | Bounded cohort metrics | Separate busy importers from idle peers with a bounded label set and correct gauge cleanup. [Aggregate metrics work today](../capabilities/src/bin/framework_metrics.rs). |
| 15 | Typed streams / completion watermarks | Pull a bounded stream, stall an early item, then show released ownership differs from explicit successful acknowledgement. |
| 16 | Blocking worker leasing | Thousands of mostly idle actors share a bounded blocking pool while each actor stays serial. |
| 17 | Batching | Batch database writes up to a count/time budget; preserve documented ordering and individual reply/failure accounting. |
| 18 | Coalescing | Replace obsolete head notifications using bounded keys; demonstrate that transactional requests are never silently coalesced. |
| 19 | Timer policies | Pause a consumer, then compare Skip/Delay/Burst and cancellation against a controlled clock. |
| 20 | Groups / topics / routers | Join workers, route requests, remove crashed members and report partial broadcast failures. |
| 21 | State machines / bounded stashing | Queue a bounded number of messages while connecting; un-stash on readiness and reject overflow. |
| 22 | Read-only inspection | Inspect last-known status and a bounded redacted history while a handler is busy. |
| 23 | Generated protocol dispatch | Keep the same caller behavior across generated and manual handlers; measure allocations separately from useful work. |
| 24 | CPU service classes | Saturate a bulk class and show a critical class's measured service share under a fixed CPU budget. |
| 25 | Delay-based shedding | Compare simple admission and CoDel-inspired shedding on explicitly droppable messages; report losses and queue delay. |
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
