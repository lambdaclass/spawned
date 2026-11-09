# Spawned - Project Context

Rust actor framework inspired by Erlang OTP. Provides `Actor` trait (similar to GenServer) that separates concurrency logic from business logic.

## Project Structure

```
spawned/
├── concurrency/     # Main library: Actor trait, timers, streams
│   └── src/
│       ├── tasks/   # Async version (requires tokio runtime)
│       ├── threads/ # Sync version (native OS threads)
│       ├── system.rs       # ActorSystem, ExecutionGroup (unreleased)
│       ├── supervision.rs  # ChildSpec, all three strategies, nested trees (unreleased)
│       ├── child_handle.rs # ChildHandle, ActorId, ExitReason
│       ├── monitor.rs      # MonitorRef, Down
│       ├── link.rs         # links, Exit, trap_exit
│       └── metrics.rs      # optional `metrics` feature
├── rt/              # Runtime abstractions (wraps tokio, provides CancellationToken)
│   └── src/
│       ├── tasks/   # Tokio-based runtime
│       └── threads/ # Thread-based runtime
├── macros/          # Proc macros (#[protocol], #[actor])
├── examples/        # Usage examples (name_server, bank, ping_pong, capabilities, etc.)
├── docs/            # GUIDE.md, MIGRATION.md, TESTING.md, design/ (active contract), research/
├── benchmarks/      # Benchmark harness and render scripts
├── verification/    # TLA+ models/trace checks and explicit API compatibility policy
└── legacy/          # Archived research, reviews, reports, raw data (not maintained)
```

## Documentation Map

| File | Purpose |
|------|---------|
| `README.md` | What/why, quick start, doc map |
| `CHANGELOG.md` | The only release history (keep `Unreleased` current) |
| `ROADMAP.md` | Single planning doc: phases + ranked backlog + examples to add |
| `docs/GUIDE.md` | User guide (lifecycle, backends, links, execution groups, supervision, metrics) |
| `docs/MIGRATION.md` | Per-version upgrade notes |
| `docs/TESTING.md` | Test suites, verification layers, TLA+ model |
| `docs/design/execution-groups-supervision.md` | Active design contract |
| `docs/research/` | Maintained research notes behind roadmap items (virtual actors, systematic testing, Ethereum testing, request semantics) |
| `examples/README.md` | Feature → example index, with gaps |
| `benchmarks/README.md` | How to run the benchmark harness |
| `verification/tla/README.md` | Running the model and trace validation |

Do not add new top-level docs; extend these. Maintained research goes in `docs/research/`; put historical material in `legacy/`.

## Two Execution Modes

- **tasks**: Async/await with tokio. Use `spawned_rt::tasks` and `spawned_concurrency::tasks`
- **threads**: Blocking, no async. Use `spawned_rt::threads` and `spawned_concurrency::threads`

Both provide identical Actor API. The `tasks` module has `Backend` enum: `Async`, `Blocking`, `Thread`.

## Key Types

| Type | Description |
|------|-------------|
| `Actor` | Trait for stateful message handlers |
| `Handler<M>` | One impl per message type (RPITIT, not object-safe) |
| `ActorRef<A>` | External handle to communicate with a running actor |
| `Context<A>` | Handler-side handle (passed to handlers) |
| `Response<T>` | Unified request-response wrapper (tasks: `.await`, threads: `.unwrap()`) |
| `Recipient<M>` | Type-erased per-message reference (`Arc<dyn Receiver<M>>`) |
| `CancellationToken` | Signal cancellation to timers/actors |
| `TimerHandle` | Handle for `send_after`/`send_interval` |
| `ChildHandle` / `ActorId` | Type-erased handle and identity for monitors, links and exits |
| `ExitReason` | `Normal`, `Shutdown`, `Panic(String)`, `Kill` |
| `MonitorRef` / `Down` | Unidirectional death observation (`ctx.monitor`) |
| `Exit` / `#[exit_received]` | Bidirectional links and trapped exits (`ctx.link`, `ctx.trap_exit`) |
| `ActorSystem` / `ExecutionGroup` | Owned placement with worker budgets and ordered shutdown (unreleased) |
| `Supervisor` / `ChildSpec` / `SupervisedChild` | OneForOne, OneForAll, RestForOne and nested supervision (unreleased) |

## Actor Lifecycle

1. `started(&mut self, ctx: &Context<Self>)` - Setup before message loop
2. `Handler<M>::handle(&mut self, msg: M, ctx: &Context<Self>)` - Process messages
3. `stopped(&mut self, ctx: &Context<Self>)` - Cleanup after stop

## Common Patterns

```rust
// Start an actor (use ActorStart trait)
let ns = NameServer::new().start();

// Call protocol methods directly on ActorRef
ns.add("key".into(), "value".into()).await.unwrap();
let result = ns.find("key".into()).await.unwrap();

// Low-level send/request (without protocols)
actor.send(MyMessage { data: 42 })?;
let reply = actor.request(MyRequest { key: "foo".into() }).await?;

// Timer
send_after(Duration::from_secs(5), ctx.clone(), Tick);

// Wait for actor to stop
actor.join().await;
```

## Testing

```bash
cargo test --workspace           # Run all tests
cargo test -p spawned-concurrency  # Test concurrency crate only
```

## Conventions

- Use `tracing` for logging (not `println!`)
- Prefer `&self` over `&mut self` for thread-safe methods
- Handle poisoned mutexes with `unwrap_or_else(|p| p.into_inner())`
- Use conventional commits: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`

## PR List Format

When listing PRs, output raw markdown in a code block so it can be copied directly:
- Format: [#NUMBER](URL) title (+additions/-deletions) ✅
- Use ⏳ instead of ✅ when approvals are 0
- Group by label/topic with a header
- No bullet points on PR lines
- End with: **Summary:** X PRs | **Total:** (+A/-D) | **Net:** ±N lines
