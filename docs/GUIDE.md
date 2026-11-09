# Spawned Guide

The user guide for the spawned actor framework: lifecycle, messaging, execution
modes, timers, type erasure, the registry, links, execution groups, supervision
and metrics. For a quick introduction, see the [README](../README.md); for
upgrades, see [MIGRATION.md](MIGRATION.md); for runnable programs, see the
[examples index](../examples/README.md).

## Table of Contents

- [Actor Lifecycle](#actor-lifecycle)
- [Context](#context)
- [ActorRef](#actorref)
- [Execution Modes and Backends](#execution-modes-and-backends)
- [Timers](#timers)
- [send\_message\_on](#send_message_on)
- [spawn\_listener](#spawn_listener)
- [Type Erasure: Recipient and Receiver](#type-erasure-recipient-and-receiver)
- [Registry](#registry)
- [Response\<T\>](#responset)
- [Message Trait](#message-trait)
- [Error Handling](#error-handling)
- [spawned-rt](#spawned-rt)
- [Links and trapped exits](#links-and-trapped-exits)
- [Execution groups and supervision](#execution-groups-and-supervision) (unreleased)
- [Metrics](#metrics)
- [Further reading](#further-reading)

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
| `ctx.id()` | This actor's `ActorId` |
| `ctx.monitor(&child_handle)` | Receive a `Down` message when the target exits; returns a `MonitorRef` |
| `ctx.demonitor(monitor_ref)` | Cancel a monitor |
| `ctx.link(&child_handle)` / `ctx.unlink(&child_handle)` | Bidirectional exit propagation (see [Links and trapped exits](#links-and-trapped-exits)) |
| `ctx.trap_exit(bool)` | Receive linked exits as messages instead of being cancelled |
| `ctx.spawn_helper(future)` / `ctx.spawn_blocking_helper(f)` | Actor-owned helper tasks, cancelled and joined before cleanup completes (unreleased) |
| `ctx.register_name(name)` | Register this actor under a name, released with its generation (unreleased) |
| `ctx.execution_group()` | The `ExecutionGroup` a managed actor runs on (unreleased) |
| `ctx.mailbox_depth()` | Queued messages (with the `metrics` feature) |

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
| `actor_ref.id()` | The actor's `ActorId` |
| `actor_ref.child_handle()` | Type-erased `ChildHandle` for monitors, links and exit observation |
| `actor_ref.exit_reason()` / `actor_ref.wait_exit()` | The `ExitReason` once stopped |
| `actor_ref.ready()` | Wait until a managed actor's `started()` completed (unreleased) |
| `actor_ref.mailbox_depth()` | Queued messages (with the `metrics` feature) |

### Starting an actor

```rust
use spawned_concurrency::tasks::ActorStart as _;

let actor_ref = MyActor::new().start();

// tasks mode: choose a backend
let actor_ref = MyActor::new().start_with_backend(Backend::Thread);
```

---

## Execution Modes and Backends

Spawned has two execution modes with the same `Actor`, `Handler<M>`,
`ActorRef<A>` and `Context<A>` types. Switching requires changing imports and
adding/removing `async`/`.await` on handlers and lifecycle hooks.

- **`tasks`** (`spawned_concurrency::tasks`): async/await on Tokio. Use it for
  async I/O, many dynamic actors (thousands) and integration with async libraries.
- **`threads`** (`spawned_concurrency::threads`): synchronous handlers, one native
  OS thread per actor. Use it for simplicity or CPU-bound actors with a moderate
  actor count. It needs no active Tokio executor but still compiles Tokio through
  `spawned-rt`.

In `tasks` mode, the `Backend` enum controls where an actor's message loop runs.
Existing actor/protocol definitions work with every value. `Backend::Async` is the
supported default; choose another backend for a specific isolation or blocking
requirement, then measure the application.

| Mode | Appropriate work | Resource model |
|---|---|---|
| `Backend::Async` (default) | Async I/O, many dynamic actors, general request/reply | Actors share Tokio workers; a running handler must cooperate and must not block |
| `Backend::Blocking` | Persistent actors that perform blocking calls (file system, synchronous HTTP) | Each actor occupies a Tokio blocking-pool worker until it stops; excess actors queue, including startup |
| `Backend::Thread` | Actors needing a dedicated execution thread (CPU-bound or isolated work) | One dedicated root thread plus an isolated Tokio runtime with one async helper worker per actor; blocking helpers can add threads |
| `threads` module | Synchronous actor handlers | One native OS thread per actor; synchronous API |

```rust
use spawned_concurrency::tasks::{ActorStart as _, Backend};

let a = MyActor::new().start();                                // async on Tokio
let b = MyActor::new().start_with_backend(Backend::Blocking);  // blocking I/O
let c = MyActor::new().start_with_backend(Backend::Thread);    // dedicated thread
```

`Backend::Async` emits a tracing warning (debug builds only) if a poll takes
longer than 10 ms; use `Backend::Blocking` or `Backend::Thread` for slow work.

Stop and await actors when their work is finished. A dedicated thread does not
make one long-running handler preemptible, and a persistent blocking actor does
not release its worker between messages. Size persistent populations accordingly.

`Backend::Thread` drives lifecycle hooks and handlers on its dedicated root
thread. Its isolated multithread Tokio runtime has one async worker, configured
independently of `TOKIO_WORKER_THREADS`. Async helpers, timers and nested
`Backend::Async` actors progress on that worker while the root thread blocks;
`block_in_place` remains supported. One blocked async worker can still stall
other work assigned to it, so await blocking helpers instead of assuming
unlimited parallel progress. Tokio can create additional threads for
`spawn_blocking`, filesystem or DNS work, and each isolated runtime consumes
reactor descriptors. Stop and join nested actors before their owning runtime
exits: actor `join()` waits for the actor, not for detached helpers or runtime
destruction. `spawned_rt::threads::block_on` uses the same one-worker runtime.

### Actors on different runtimes

Actors hosted on separate Tokio runtimes exchange ordinary messages and
request/reply through `ActorRef`. Sending an `ActorRef` does not migrate an actor
or its Tokio I/O resources. Separate runtimes add threads and inter-runtime
wakeups, and their capacity cannot rebalance automatically; communication
compatibility alone is not a reason to create more runtimes. For owned,
budgeted runtimes use [execution groups](#execution-groups-and-supervision).

The `backend-experiments` feature contains private research selectors (pinned
and spinning shards, alternative schedulers). They are not stable configuration
APIs, and Tokio stays the default. Their measurements and reproduction guides are
archived in [the backend notes](../legacy/docs/BACKENDS.md) and the
[backend research index](../legacy/research/backend-experiments/README.md).

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

See [Execution Modes and Backends](#execution-modes-and-backends) for choosing between
`tasks` and `threads`.

<a id="links-and-trapped-exits-unreleased-main"></a>

## Links and trapped exits

Available since v0.6.0.

Links are bidirectional failure notifications. Use `ctx.link(&child_handle)` and
`ctx.unlink(&child_handle)` for existing actors, or `child.start_linked(&parent_ctx)`
to perform registration before the child starts. Child handles allow links across
execution modes. Normal peer exits are ignored by non-trapping actors; abnormal
exits request cooperative cancellation and propagate their cause.

Call `ctx.trap_exit(true)` to receive linked exits through `Actor::exit_received`
instead. The actor macro supports this lifecycle hook:

```rust
use spawned_concurrency::{actor, Exit};
use spawned_concurrency::tasks::{Actor, Context};

struct Supervisor;

#[actor]
impl Supervisor {
    #[started]
    async fn setup(&mut self, ctx: &Context<Self>) {
        ctx.trap_exit(true);
    }

    #[exit_received]
    async fn child_exit(&mut self, exit: Exit, _ctx: &Context<Self>) {
        tracing::warn!(actor = %exit.from, reason = ?exit.reason, "Linked actor exited");
    }
}
```

Use synchronous methods and the `threads` imports in synchronous mode. The hook
is optional and defaults to doing nothing. Trapped exits use the ordinary actor
mailbox; they do not interrupt a running handler. Kill bypasses trapping and
requests cancellation, also cooperatively. A normal stop already committed is
not relabelled by a later peer failure; callback failures are still reported.

A link to a closed peer creates no retained relationship and applies that peer's
exit-signal rules for the attempt. Repeated live links are no-ops. Unlink prevents
future propagation through the relationship but does not retract an exit already
in flight. Links are a supervision primitive. Managed OneForOne recovery is described in
[execution groups and supervision](#execution-groups-and-supervision); additional restart strategies and
transparent reference rebinding remain separate work. The [review and regression evidence](../legacy/docs/reviews/pr-166-links.md) is archived.

---

## Execution groups and supervision

Available on main, unreleased. These are opt-in capabilities for async actors;
ordinary `.start()`, message types, handlers and protocols retain their behavior.
The initial profiles are parked async pools and OneForOne recovery. A supervisor
owns one ordered child set; nested supervisor child specs are not implemented. Pinned or
spinning groups, sized blocking-actor pools, OneForAll and RestForOne remain planned.

### Choose an owner and a worker budget

```rust
use spawned_concurrency::{system::ActorSystem, tasks::ActorStart};
use std::time::Duration;

// MyActor is your application-defined async actor.
let system = ActorSystem::new()?;
let critical = system.async_group("critical", 1)?;
let background = system.async_group("background", 3)?;
let actor = MyActor::new().start_on(&critical)?;
actor.ready().await?;
let report = system.shutdown(Duration::from_secs(5)).await;
assert!(report.complete);
```

Groups own separate Tokio runtimes. ActorRef messages and requests cross those
runtimes normally; moving a reference does not move an actor or an I/O resource.
Use a small number of subsystem groups, rather than creating a pool per actor.
Groups do not currently expose CPU affinity or spinning.
Construct groups during application setup: construction is synchronous and
fallible, and no actor is admitted to a failed group.

The worker budget counts async workers, not all process threads. Each system
also has a control thread and each group a runtime-owner thread. Tokio blocking
operations can create extra threads. A group is neither CPU affinity nor an OS
CPU reservation; blocking a worker or a handler still requires cooperation.

ActorSystem owns managed lifetimes. Cloned group/reference/context handles do
not retain live workers after successful system shutdown. Dropping ActorSystem
requests shutdown without blocking its calling thread; await `shutdown` when
cleanup completion matters. Keeping an incomplete report lets you await again:

```rust
let report = system.shutdown(Duration::from_millis(50)).await;
if !report.complete {
    eprintln!("unfinished groups: {:?}", report.unfinished_groups);
    let final_report = report.wait(Duration::from_secs(10)).await;
    // Report/escalate unfinished work if it is still incomplete.
}
```

Timeout closes admission and reports unfinished groups/actor scopes and observed exits.
An exited actor can still have an unfinished scope while registered helpers clean
up, so its ID may appear in both fields. System shutdown orders supervisor trees
and standalone actors together by reverse admission. The first shutdown deadline
bounds these serial waits; at expiry it signals the remaining targets and retains
all actual cleanup. Drop uses a five-second ordering grace. Later report waits do
not reopen admission or extend that initial ordering grace.
It does not kill native code or abandon runtime ownership. The cleanup coordinator
keeps closed runtimes alive until tracked work actually finishes, even if an
incomplete report is dropped. An uncooperative operation can therefore keep
threads/resources alive indefinitely; the host decides process-level escalation.

### Construct fresh children after failure

```rust
use spawned_concurrency::supervision::{ChildSpec, RestartMode, SupervisorConfig};

let mut tree = system.supervisor(SupervisorConfig::default())?;
let service = tree.add(
    ChildSpec::new("service", &critical, |factory_ctx| async move {
        // This factory is invoked and polled on the assigned execution group.
        // Return a fresh, unstarted actor; loading durable state is application work.
        Ok::<_, String>(MyActor::new())
    }).restart(RestartMode::Transient)
)?;
let supervisor = tree.start().await?;
let current = service.current()?;
// After failure, explicitly resolve a generation greater than a saved snapshot.
let generation = service.snapshot().generation;
// let replacement = service.wait_for_generation(generation).await?;
let report = supervisor.shutdown(Duration::from_secs(5)).await;
```

A factory may return an error or panic. Registered construction helpers are
cancelled and joined before retry, including when the factory never returns an
actor. Existing `started()` and `stopped()` hooks keep their signatures. Readiness
commits after `started()` returns and admission remains open; no ordinary handler
runs before that barrier. Enqueued startup messages are discarded if startup fails.
Calling `ready()` later reports that historical barrier, not current liveness.
Failed readiness waits for the actual startup outcome, so a non-returning hook
can keep it pending; use a timeout. A startup deadline is a failed attempt and can retry under Permanent/Transient
policy after cleanup, whether it expired in the factory or in `started()`.
A self-requested normal stop before readiness is still a
startup failure: a Transient/Temporary spec fails the tree rather than claiming
a ready service; a Permanent spec can retry within its budget.

A supervisor accepts only groups owned by its ActorSystem. ActorRefs can still
communicate between independent systems. Children start in declaration order
and shut down in reverse order. Startup
future cancellation requests rollback of the partially started tree. Each spec
has a startup and cleanup timeout (default five seconds). A stuck actor, startup
hook or registered helper fails the supervisor and suppresses replacements until
actual cleanup completes. There is at most one live generation per child slot.
Stop observation uses internal tokens/completion state, independently of ordinary
mailbox traffic. It does not interrupt a running handler. If a supervisor report deadline expires,
remaining siblings are also signalled before returning the incomplete report;
actual completion still waits for the stuck work.

| Restart mode while supervisor is running | Normal / Shutdown | Panic / Kill |
|---|---|---|
| Permanent (default) | Restart | Restart |
| Transient | Stay stopped | Restart |
| Temporary | Stay stopped | Stay stopped |

System/supervisor shutdown suppresses every restart mode. Stopping an individual
Permanent child while its supervisor is open intentionally restarts it. Use
supervisor shutdown to close the tree. `ChildHandle::shutdown()` commits Shutdown
before cooperative cancellation while preserving a previously committed cause;
actual handler or cleanup panics remain failures. Existing link propagation still
applies and can affect siblings explicitly linked by the application.

The default shared budget permits five retry attempts per rolling 30-second
window. Initial construction does not consume a retry; every subsequent attempt
does, including failed factories and normal replacements of Permanent children.
A zero retry budget fails immediately without waiting for backoff. Otherwise
retry accounting is stamped immediately before factory admission, after the
cancellable delay; retries spaced beyond the rolling window are permitted by
design. This is a rate limit, not a lifetime retry cap. Exhaustion fails the whole supervisor and stops its remaining children. Backoff
is capped exponential with optional jitter, independently per child; a stable
ready lifetime resets that child's exponent. Cleanup time is not healthy uptime.
Use `Backoff::seed` for reproducible policy tests, not security randomness.

### References and effects

A restarted actor has fresh state, a fresh mailbox and a fresh ActorId. Old
ActorRef/Recipient values remain attached to the stopped instance. `current()`
returns a ready instance or an unavailable state; `wait_for_generation(n)` waits
for a ready generation strictly newer than `n`. Either can race with a later
failure, so delivery errors remain normal outcomes. Snapshot generations count
construction attempts, including failed attempts before an ActorId exists.
Snapshots are asynchronous diagnostics and may briefly show Running after a stop
signal; `current()` already rejects lookup when supervisor admission closes.

Queued messages are discarded, incomplete request channels fail when their owner
is dropped, and caller timeouts can occur earlier. No messages are replayed.
Effects already performed are not rolled back. Supervision supplies neither
automatic durable state recovery nor exactly-once effects.

### Owned helpers and registrations

Managed `Context` and `FactoryContext` offer `spawn_helper` and
`spawn_blocking_helper`. Dropping the returned join handle does not detach scope
ownership. Async helpers are cooperatively cancelled on actor stop; running
blocking helpers cannot be aborted and retain cleanup ownership until their
native closure returns. Helpers report cancellation/panic via `HelperError`.
Rejected helper admission returns an error. System/supervisor shutdown closes
helper/timer/monitor/registration admission for the whole managed tree immediately,
including earlier actors awaiting their stop turn. Those actors may still serve
ordinary requests, but must handle helper rejection. Prepare shutdown resources
in advance; this is stop-and-join, not an admission-preserving drain mode. Never synchronously wait for a helper
on the sole async worker it needs in order to make progress.

Framework timers, stream listeners, deferred message helpers and monitor waiters
use the managed actor's group even when registered from another runtime or a
plain thread. Their existing infallible APIs return inert/cancelled work after
closure, with no ambient fallback. Aborting their task handle releases its scope
ownership after payload destruction. Legacy timers remain tied to the old
instance; they never retarget a replacement.

`ctx.register_name(name)` owns a registry guard until that generation's actor and
registered helpers finish cleanup. Guard removal checks an ownership nonce so
an old generation cannot delete a newer registration. Named registry instances
and transparent reference rebinding are not implemented.

Arbitrary `tokio::spawn`, nested blocking/native work and ambient `.start()` do not
automatically enroll in a scope. Track them explicitly or accept their unmanaged
lifetime. An ambient actor created inside a group may be aborted by runtime
teardown without running `stopped()`. Actor `join()` still waits for that actor;
use supervisor/system shutdown to await registered helpers and runtime teardown.

### Run the examples

- [Execution groups](../examples/capabilities/src/bin/execution_groups.rs): router/worker cross-group request, target-group helper and owned shutdown.
- [Supervision recovery](../examples/capabilities/src/bin/supervision_recovery.rs): crash, new identity/fresh state, stale reference failure and explicit replacement lookup.

```sh
cargo run --locked -p spawned-capability-examples --bin execution_groups
cargo run --locked -p spawned-capability-examples --bin supervision_recovery
```

The [design contract](design/execution-groups-supervision.md) records the fuller
boundaries. [Failure tests](TESTING.md#managed-execution-and-recovery-main-unreleased)
exercise readiness, cancellation, rollback, restart limits and incomplete cleanup.
These APIs do not establish a full-client performance improvement; measure actual
application workloads with equal resource budgets.

---

## Metrics

Enable the optional `metrics` feature on `spawned-concurrency` (available in v0.6.0):

```toml
spawned-concurrency = { version = "0.6", features = ["metrics"] }
```

The default build compiles out collection, timing metadata and response observers.
No handler, protocol, message or result type needs changing. Metrics do not change
request timeouts, cancel queued work, restart actors or change mailbox capacity.

Configure before starting actors or constructing observed requests:

```rust
use spawned_concurrency::metrics::{self, Config};

metrics::configure(Config { timing_sample_every: 16 }).unwrap();
// Counters include every operation; one in 16 observations gets timing samples.
let snapshot = metrics::snapshot();
let prometheus_text = metrics::render_prometheus();
```

Default sampling is 1 (all timings). Use 0 for counters/gauges without clocks or
histogram observations. Configuration is process-wide and immutable after first
instrumentation; `configure` returns `Err(config)` if already initialized.

### What is measured

| Measurement | Definition |
|---|---|
| Mailbox depth | Waiting user envelopes, including submissions in progress; excludes the active handler and internal Exit/Shutdown controls. Read an individual actor with `actor.mailbox_depth()` or `actor.context().mailbox_depth()`. The snapshot gauge sums all actors. |
| Queue wait | Sampled submission to handler start, including transport/scheduler delay. Submission timing begins before channel publication. |
| Handler duration | Sampled dispatch to completion, panic or future destruction; includes awaited time, not just CPU time. Lifecycle hooks and internal exit callbacks are excluded. |
| Request latency | Caller-wrapper lifetime to observed reply, timeout, actor-stop error or caller cancellation. Includes time awaiting a reply, including scheduling. |
| Request outcomes | Started, active, replied, timed out, actor stopped, and caller cancelled. Transport replies are successful even if the application result itself contains an error. |
| Lifecycle | Actor instances admitted, active, and exited by Normal/Shutdown/Panic/Kill; handler starts, active handlers, completions, panics and interrupted futures. |
| Delivery accounting | Enqueue acceptance/rejection, discarded queued messages, replies sent to channels, and reply receivers already dropped. Enqueue success does not promise handling or durable delivery. |

Counters end in `_total`. Duration histograms use fixed nanosecond buckets
internally and seconds in Prometheus export; their count/sum/max describe sampled
observations only. Counts are not multiplied by the sampling rate. Do not compare
sampled histogram counts to total request counters as if every request was timed.
Sampling is deterministic round-robin per message/request category, not random;
periodic traffic can bias samples. Quantiles are bucket-based estimates.

Mailbox depth reserves accounting before publication to avoid consumer races.
A stalled producer may temporarily appear in depth before send acceptance. A
failed send rolls that reservation back and increments rejection. This is not a
transactional channel-length API. Counters/gauges are independent atomic reads;
conservation relationships apply after operations quiesce:

- accepted messages = handled + discarded + waiting;
- handled = completed handlers + panicked handlers + aborted handlers + active;
- observed requests = replied + timed out + actor stopped + cancelled + active;
- spawned actor instances = exited + active.

An actor is active from creation of its lifecycle guard until terminal cleanup,
including waiting for a blocking-pool slot, `started()` and `stopped()`. A runtime
abort before its first poll still closes lifecycle and mailbox accounting. A
handler future destroyed without returning an outcome counts as aborted; this
does not imply its business effects were rolled back. Native blocking handlers
still stop cooperatively.

### Caller and raw receiver boundaries

Direct `ActorRef::request`, `Context::request`, and erased-recipient `request`
measure from their execution to return. Async methods start when first polled.
Protocol-generated task requests use `Response`: observation starts at response
construction, after the raw send, and ends when the caller polls the result or
drops the wrapper. An unpolled wrapper dropped by its caller counts as cancelled.
A completed reply left unpolled is not a caller-observed reply yet. Timed Response
waits retain their existing first-poll timeout behavior. Synchronous protocol
requests are observed by their blocking request; `Response::ready` does not count
that result a second time. Manually constructed ready results are not requests.

`request_raw` returns the existing mode-specific raw receiver. It cannot observe
custom timeouts or how the caller waits without changing that API. Such requests
still contribute message, handler, queue and reply-delivery metrics, but only a
framework request/Response wrapper contributes caller-outcome/latency metrics.
Reply delivery means a successful channel send, not proof the caller consumed it.
Dropping/timing out a caller does not cancel accepted actor work.

### Collection and exporting limits

Collection uses fixed aggregate atomic counters and three fixed 32-bucket
histograms, plus one shared depth counter per live actor/reference set. There is
no global actor registry, label map, per-message metrics allocation, retained
payload/span, callback or exporter queue. Envelopes contain an inline observation
and reuse the actor's shared depth counter. The disabled build has no shared
metrics counter or timing metadata. Internal control messages are not reported
as application traffic.

Call `snapshot` or `render_prometheus` from your own exporter/HTTP task. Export is
pull-based: collection never calls an exporter, performs network I/O or waits for
an exporter lock. Text output has a fixed set of series without actor/message
names, payloads or actor IDs. An application can add bounded, explicit labels
outside the library. Histogram sums saturate at `u64::MAX` nanoseconds; counters
can wrap after `u64::MAX` events. Scrapes are approximate concurrent observations.

There is no built-in HTTP server, OpenTelemetry exporter, causal tracing,
oldest-queued-message age, per-actor histogram or active-monitor gauge in this
initial implementation. These can be evaluated independently of the existing
metrics and without inserting exporter work into handlers.

Use application measurements before choosing a sampling rate; shared atomic
counters themselves can contend under high actor/producer concurrency. The
original [overhead measurements](../legacy/research/framework-metrics/2026-11-06/README.md)
are archived.

---

## Further reading

- [Execution groups and supervision design contract](design/execution-groups-supervision.md): the fuller boundaries of the unreleased APIs.
- [Testing](TESTING.md): scenario tests, failure windows, the TLA+ model and trace validation.
- [Roadmap](../ROADMAP.md): what is planned and why.
- [Legacy](../legacy/README.md): archived backend research, benchmark reports, reviews and earlier designs.
