# Spawned API Guide

Complete reference for the spawned actor framework. For a quick introduction, see the [README](../README.md).

## Table of Contents

- [Actor Lifecycle](#actor-lifecycle)
- [Context](#context)
- [ActorRef](#actorref)
- [Backend Selection (tasks mode)](#backend-selection-tasks-mode)
- [Timers](#timers)
- [send\_message\_on](#send_message_on)
- [spawn\_listener](#spawn_listener)
- [Type Erasure: Recipient and Receiver](#type-erasure-recipient-and-receiver)
- [Registry](#registry)
- [Response\<T\>](#responset)
- [Message Trait](#message-trait)
- [Error Handling](#error-handling)
- [spawned-rt](#spawned-rt)

---

## Actor Lifecycle

Every actor goes through three phases:

1. **`started()`** — called once before the actor begins processing messages. Use this for initialization: starting timers, registering with the registry, etc.
2. **Message loop** — the actor processes messages one at a time via `Handler<M>` implementations.
3. **`stopped()`** — called once after the message loop exits. Use this for cleanup.

```rust
#[actor(protocol = MyProtocol)]
impl MyActor {
    #[started]
    async fn started(&mut self, ctx: &Context<Self>) {
        // Start a periodic timer
        send_interval(Duration::from_secs(10), ctx.clone(), Tick);
    }

    #[stopped]
    async fn stopped(&mut self, _ctx: &Context<Self>) {
        tracing::info!("actor shutting down");
    }

    #[send_handler]
    async fn handle_tick(&mut self, _msg: Tick, _ctx: &Context<Self>) {
        // periodic work
    }
}
```

### Panic Recovery

All three phases are wrapped in `catch_unwind`:

- **Panic in `started()`** — the actor stops immediately. No messages are processed, `stopped()` is not called.
- **Panic in a handler** — the current message is lost. The actor stops and `stopped()` is called.
- **Panic in `stopped()`** — the panic is logged but `join()` still returns normally.

In all cases, `join()` will eventually return and subsequent `send()`/`request()` calls return `Err(ActorStopped)`.

### Stopping an Actor

Call `ctx.stop()` from inside a handler or lifecycle hook. This cancels the actor's internal token, causing the message loop to exit after the current handler finishes.

From outside, there is no direct stop method on `ActorRef`. Design your protocol with an explicit shutdown message, or use `ctx.stop()` from within the actor.

---

## Context

`Context<A>` is the handle passed to every handler and lifecycle hook. It provides access to the actor's own mailbox and lifecycle controls.

| Method | Description |
|--------|-------------|
| `ctx.stop()` | Signal the actor to stop after the current handler finishes |
| `ctx.send(msg)` | Send a fire-and-forget message to this actor (self-send) |
| `ctx.request(msg)` | Send a request and wait for the reply (tasks: async, threads: blocking) |
| `ctx.request_with_timeout(msg, duration)` | Send a request with a custom timeout |
| `ctx.request_raw(msg)` | Send a request and get a raw oneshot receiver |
| `ctx.recipient::<M>()` | Get a type-erased `Recipient<M>` for this actor |
| `ctx.actor_ref()` | Get an `ActorRef<A>` from the context |

`Context::from_ref(&actor_ref)` creates a context from an `ActorRef`, useful for setting up timers or stream listeners from outside the actor.

### Self-scheduling example

```rust
#[send_handler]
async fn handle_tick(&mut self, _msg: Tick, ctx: &Context<Self>) {
    self.do_work();
    // Schedule the next tick
    send_after(Duration::from_secs(5), ctx.clone(), Tick);
}
```

---

## ActorRef

`ActorRef<A>` is the external handle to a running actor. Cloneable, `Send + Sync`.

| Method | Description |
|--------|-------------|
| `actor_ref.send(msg)` | Fire-and-forget. Returns `Result<(), ActorError>` |
| `actor_ref.request(msg)` | Request with default 5s timeout. Tasks: `.await`, Threads: blocking |
| `actor_ref.request_with_timeout(msg, duration)` | Request with custom timeout |
| `actor_ref.request_raw(msg)` | Returns a raw oneshot receiver |
| `actor_ref.recipient::<M>()` | Get a type-erased `Recipient<M>` |
| `actor_ref.context()` | Get a `Context<A>` (for timer setup, etc.) |
| `actor_ref.join()` | Wait until the actor has fully stopped (tasks: `.await`, threads: blocking) |

### Starting an actor

```rust
use spawned_concurrency::tasks::ActorStart as _;

let actor_ref = MyActor::new().start();

// tasks mode: choose a backend
let actor_ref = MyActor::new().start_with_backend(Backend::Thread);
```

---

## Backend Selection (tasks mode)

The `Backend` enum controls where the actor's message loop runs. Only available in `tasks` mode.

| Backend | When to use |
|---------|-------------|
| `Backend::Async` (default) | Standard async actors. Runs on the tokio runtime. Handlers must not block. |
| `Backend::Blocking` | Actors that do blocking I/O (file system, synchronous HTTP). Runs on tokio's blocking thread pool. |
| `Backend::Thread` | CPU-bound work or full isolation. Runs on a dedicated OS thread with its own tokio runtime. |

```rust
// Default — async on tokio
let a = MyActor::new().start();

// Blocking I/O
let b = MyActor::new().start_with_backend(Backend::Blocking);

// Dedicated thread
let c = MyActor::new().start_with_backend(Backend::Thread);
```

`Backend::Async` will emit a tracing warning (debug builds only) if a poll takes longer than 10ms — use `Backend::Blocking` or `Backend::Thread` for slow work.

---

## Timers

Timers send messages to actors after a delay or at regular intervals.

### `send_after`

Sends a single message after a delay.

```rust
use spawned_concurrency::tasks::{send_after, Context};

// Inside a handler or started()
let timer = send_after(Duration::from_secs(5), ctx.clone(), MyMessage);

// Cancel before it fires
timer.cancellation_token.cancel();
```

### `send_interval`

Sends a message repeatedly at a fixed interval. The message type must implement `Clone`.

```rust
use spawned_concurrency::tasks::send_interval;

let timer = send_interval(Duration::from_secs(1), ctx.clone(), Tick);

// Stop the interval
timer.cancellation_token.cancel();
```

### `TimerHandle`

Both functions return a `TimerHandle` with two public fields:

- `join_handle` — the spawned task/thread handle
- `cancellation_token` — cancel the timer before it fires (or stop an interval)

Timers are automatically cancelled when the actor stops.

---

## send_message_on

Sends a message to an actor when an external event completes.

**Tasks mode** — takes a `Future`:

```rust
use spawned_concurrency::tasks::send_message_on;

// Send Shutdown to the actor when ctrl_c resolves
send_message_on(ctx, rt::ctrl_c(), Shutdown);

// Send DataReady after an async operation completes
send_message_on(ctx, fetch_data(), DataReady);
```

**Threads mode** — takes a `FnOnce()` closure:

```rust
use spawned_concurrency::threads::send_message_on;

// Send Shutdown when the closure returns (blocking call)
send_message_on(ctx, rt::ctrl_c(), Shutdown);
```

If the actor stops before the event completes, the message is not sent.

---

## spawn_listener

Forwards items from a stream (tasks) or iterator (threads) to an actor as messages.

**Tasks mode** — takes an async `Stream`:

```rust
use spawned_concurrency::tasks::spawn_listener;

let stream = ReceiverStream::new(rx);
let handle = spawn_listener(ctx, stream);
```

**Threads mode** — takes an `IntoIterator`:

```rust
use spawned_concurrency::threads::spawn_listener;

let items = vec![Push { value: 1 }, Push { value: 2 }];
let handle = spawn_listener(ctx, items);
```

The listener stops when:
- The stream/iterator is exhausted
- The actor stops (cancellation token is triggered)
- Sending to the actor's mailbox fails

---

## Type Erasure: Recipient and Receiver

When you need to send a specific message type to an actor without knowing its concrete type, use `Recipient<M>`.

```rust
pub type Recipient<M> = Arc<dyn Receiver<M>>;
```

`Receiver<M>` is the object-safe trait that provides `send()` and `request_raw()` for a single message type.

### Getting a Recipient

```rust
let recipient: Recipient<Notify> = actor_ref.recipient();
// or from inside a handler:
let recipient: Recipient<Notify> = ctx.recipient();
```

### Using a Recipient

```rust
// Fire-and-forget
recipient.send(Notify { text: "hello".into() })?;

// Request with timeout (tasks mode)
let result = request(&*recipient, GetCount, Duration::from_secs(5)).await?;

// Request with timeout (threads mode)
let result = request(&*recipient, GetCount, Duration::from_secs(5))?;
```

### When to use

- Passing actor references to other actors without exposing the concrete type
- Storing heterogeneous actor references in collections
- Cross-module boundaries where you want to depend on a message type, not an actor type

Note: For most cases, protocol-generated `XRef` types (e.g., `NameServerRef = Arc<dyn NameServerProtocol>`) are a better fit since they expose the full protocol interface. `Recipient<M>` is the escape hatch for single-message type erasure.

---

## Registry

Global name-based registry for discovering actors at runtime. Stores any `Clone + Send + Sync + 'static` value.

```rust
use spawned_concurrency::registry;
```

| Function | Description |
|----------|-------------|
| `registry::register(name, value)` | Register a value by name. Returns `Err(AlreadyRegistered)` if the name is taken. |
| `registry::whereis::<T>(name)` | Look up a value by name. Returns `None` if not found or wrong type. |
| `registry::unregister(name)` | Remove a registration. |
| `registry::registered()` | List all registered names. |

### Example

```rust
// Register a protocol reference
let ns_ref = ns.to_name_server_ref();
registry::register("name_server", ns_ref)?;

// Look it up elsewhere
let ns: Option<NameServerRef> = registry::whereis("name_server");
if let Some(ns) = ns {
    let result = ns.find("Joe".into()).await.unwrap();
}
```

The registry uses `Any`-based downcasting, so `whereis` returns `None` if the stored type doesn't match the requested type.

---

## Response\<T\>

`Response<T>` is the return type for protocol request methods. It works in both execution modes:

- **Tasks mode** — wraps a oneshot receiver. Use `.await` to get `Result<T, ActorError>`.
- **Threads mode** — wraps a pre-computed result. Use `.unwrap()` or `.expect()` directly.

### Methods (sync, for threads mode)

| Method | Description |
|--------|-------------|
| `.unwrap()` | Extract the value, panic on error |
| `.expect(msg)` | Extract the value, panic with custom message on error |
| `.is_ok()` | Returns `true` if the response contains `Ok` |
| `.is_err()` | Returns `true` if the response contains `Err` |
| `.map(f)` | Transform the inner value if `Ok` |

### Async usage (tasks mode)

```rust
// .await returns Result<T, ActorError>
let result = ns.find("Joe".into()).await.unwrap();
```

### Sync usage (threads mode)

```rust
// .unwrap() extracts the value directly
let result = ns.find("Joe".into()).unwrap();
```

`Response::ready(result)` creates a pre-computed response — this is what the `#[protocol]` macro generates for threads-mode blanket impls.

---

## Message Trait

The `Message` trait defines a message type and its expected reply type:

```rust
pub trait Message: Send + 'static {
    type Result: Send + 'static;
}
```

You rarely need to implement this manually — `#[protocol]` generates message structs that implement `Message` automatically. For cases where you need a standalone message without a protocol:

```rust
struct Ping;
impl Message for Ping {
    type Result = ();
}

struct GetCount;
impl Message for GetCount {
    type Result = u64;
}
```

---

## Error Handling

All actor communication can fail with `ActorError`:

```rust
pub enum ActorError {
    ActorStopped,    // The actor has stopped or its mailbox is closed
    RequestTimeout,  // A request exceeded the timeout (default 5s)
}
```

- `send()` returns `Err(ActorStopped)` if the actor has stopped.
- `request()` returns `Err(ActorStopped)` if the actor stops before replying, or `Err(RequestTimeout)` if the reply doesn't arrive in time.
- `request_with_timeout()` lets you specify a custom timeout.

---

## spawned-rt

`spawned-rt` provides runtime abstractions used by `spawned-concurrency`. Users import it for `run()`, `sleep()`, and signal handling.

### tasks module (`spawned_rt::tasks`)

Wraps tokio primitives:

| Item | Description |
|------|-------------|
| `run(future)` | Create a tokio runtime, initialize tracing, and block on the future |
| `block_on(future)` | Block on a future using the current tokio runtime handle |
| `spawn(future)` | Spawn an async task |
| `spawn_blocking(f)` | Spawn a blocking closure on tokio's blocking pool |
| `sleep(duration)` | Async sleep |
| `timeout(duration, future)` | Wrap a future with a timeout |
| `Runtime` | Tokio runtime (re-export) |
| `JoinHandle` | Handle to a spawned task |
| `CancellationToken` | Cooperative cancellation (from tokio-util) |
| `mpsc` | Multi-producer, single-consumer channel |
| `oneshot` | Single-use channel |
| `watch` | Watch channel for broadcasting state changes |
| `ctrl_c()` | Returns a future that resolves on Ctrl+C |

### threads module (`spawned_rt::threads`)

Wraps standard library primitives:

| Item | Description |
|------|-------------|
| `run(f)` | Initialize tracing and call `f()` |
| `block_on(future)` | Create a temporary tokio runtime and block on a future |
| `spawn(f)` | Spawn an OS thread |
| `sleep(duration)` | Block the current thread |
| `JoinHandle` | Handle to a spawned thread |
| `CancellationToken` | Cooperative cancellation with callback support via `on_cancel()` |
| `mpsc` | Multi-producer, single-consumer channel (wraps `std::sync::mpsc`) |
| `oneshot` | Single-use channel (wraps `std::sync::mpsc`) |
| `ctrl_c()` | Returns a closure that blocks until Ctrl+C. Supports multiple subscribers. |

### Choosing tasks vs threads

Use **`tasks`** when you need async I/O, high actor counts (thousands), or integration with async libraries.

Use **`threads`** when you want simplicity, no async runtime, or CPU-bound actors that benefit from dedicated OS threads. Each actor gets its own thread, so this mode works best with a moderate number of actors.

Both modes provide the same `Actor`, `Handler<M>`, `ActorRef<A>`, and `Context<A>` types. Switching requires changing imports and adding/removing `async`/`.await` on handlers and lifecycle hooks.
