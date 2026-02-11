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
| `ActorRef<T>` | Handle to communicate with a running actor |
| `ActorRef::request()` | Sync call, waits for reply (like Erlang `call`) |
| `ActorRef::send()` | Async fire-and-forget (like Erlang `cast`) |
| `ActorRef::join()` | Wait for actor to stop |
| `CancellationToken` | Signal cancellation to timers/actors |
| `TimerHandle` | Handle for `send_after`/`send_interval` |

## Actor Lifecycle

1. `init()` - Setup before main loop
2. `handle_request()` / `handle_message()` - Process messages
3. `teardown()` - Cleanup after stop

## Common Patterns

```rust
// Start an actor
let mut handle = MyActor::new().start();

// Request (sync call)
let reply = handle.request(MyRequest::GetValue).await?;

// Send (fire-and-forget)
handle.send(MyMessage::DoSomething).await?;

// Timer that wakes on cancellation
send_after(Duration::from_secs(5), handle.clone(), Msg::Timeout);

// Wait for actor to stop
handle.join().await;
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