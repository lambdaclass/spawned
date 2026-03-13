# Spawned - Project Context

Rust actor framework inspired by Erlang OTP. Provides `Actor` trait (similar to GenServer) that separates concurrency logic from business logic.

## Project Structure

```
spawned/
├── concurrency/     # Main library: Actor trait, timers, streams
│   └── src/
│       ├── tasks/   # Async version (requires tokio runtime)
│       └── threads/ # Sync version (native OS threads)
├── rt/              # Runtime abstractions (wraps tokio, provides CancellationToken)
│   └── src/
│       ├── tasks/   # Tokio-based runtime
│       └── threads/ # Thread-based runtime
└── examples/        # Usage examples (name_server, bank, ping_pong, etc.)
```

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