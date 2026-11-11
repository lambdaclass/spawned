# Spawned Guide

The user guide for the spawned actor framework: lifecycle, messaging, execution
modes, timers, type erasure, the registry, links, execution groups, supervision,
metrics and testing. For a quick introduction, see the [README](../README.md); for
upgrades, see [MIGRATION.md](MIGRATION.md); for runnable programs, see the
[examples index](../examples/README.md). The guarantees themselves, with the test
or model that checks each one, are collected in the
[semantics contract](design/semantics.md).

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
- [Deferred replies and pipe-to-self](#deferred-replies-and-pipe-to-self) (v0.8)
- [Message Trait](#message-trait)
- [Error Handling](#error-handling)
- [spawned-rt](#spawned-rt)
- [Links, monitors and exit signals](#links-monitors-and-exit-signals) (v0.8)
- [Execution groups and supervision](#execution-groups-and-supervision) (v0.7)
- [Metrics](#metrics)
- [Testing your actors](#testing-your-actors)
- [Further reading](#further-reading)

---

## Actor Lifecycle

Every actor goes through three phases:

1. **`started()`** — called once before the actor begins processing messages. Use this for initialization: starting timers, registering with the registry, etc.
2. **Message loop** — the actor processes messages one at a time via `Handler<M>` implementations.
3. **`stopped()`** — called once after the message loop exits, unless the exit was a kill or an exit signal (see [When `stopped()` does not run](#when-stopped-does-not-run)). Use this for cleanup.

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
- **Panic in `stopped()`** — the panic is logged and `join()` still returns, but the exit reason becomes `Panic("panic in stopped(): ...")` unless the actor was killed (a kill's `Killed` stands).

In all cases, `join()` will eventually return and subsequent `send()`/`request()` calls return `Err(NoProc)`.

### Stopping an Actor

Call `ctx.stop()` from inside a handler or lifecycle hook. This cancels the actor's internal token, causing the message loop to exit after the current handler finishes.

From outside, use the actor's `ChildHandle` (`actor_ref.child_handle()`): `stop()` exits `Normal`, `shutdown()` exits `Shutdown`, and `stop_with_reason(reason)` picks the reason. All are cooperative: the running handler finishes and `stopped()` runs. They ignore `trap_exit`. `ActorRef` itself has no stop method; an explicit shutdown message in your protocol also works.

Stopping discards messages still queued: once the stop is observed no further message is handled, and queued requests fail with `ActorStopped(reason)`, the actor's final exit reason.

### When `stopped()` does not run

As in OTP (`terminate/2` runs only for exits the process takes itself), `stopped()` runs for `ctx.stop()`, an administrative stop (`ChildHandle::stop`/`shutdown`/`stop_with_reason`), a panic in a handler, and a parent's exit to a **trapping** actor. It is **skipped** when the exit is begun by:

- a kill (`ChildHandle::kill()`, `exit(Kill)`, a supervisor's kill escalation at `shutdown_timeout`), unless `stopped()` already began;
- an untrapped exit signal or link notice (`ChildHandle::exit(reason)`, a linked peer's abnormal exit);
- a supervisor's or system's shutdown, or a `start_linked` parent's exit, to an actor that does **not** trap exits: the parent's exit is an exit signal (design E3);
- the runtime dropping the actor (`Killed`).

Every skip is logged at `debug` with `event = "stopped_skipped"`, the actor id and type, `cause` (`kill`, `exit_signal` or `runtime_drop`) and `reason`. The log is emitted whether or not the actor overrides `stopped()` (there is no cheap way to tell), so filter on the actor type. The lifecycle trace (feature `lifecycle-trace`) records the same decision as `Hook` `skip`.

If cleanup must run on a supervisor's shutdown, trap exits first thing in `started()`; the parent's exit then ends the actor through `stopped()`, after the messages queued before it:

```rust
async fn started(&mut self, ctx: &Context<Self>) {
    ctx.trap_exit(true); // shutdown runs stopped(); linked peers' deaths arrive as `Exit`
}
```

A trapping actor still handles the messages queued before the parent's exit. To refuse late work, check `ctx.exit_pending()`: it is `true` once the actor's exit has begun (`ctx.stop()`, an administrative stop, a kill, an untrapped signal, a panic; also in `stopped()`) or a parent exit is queued behind the current message. A trapped `Exit` from a linked peer or a non-parent `exit` is not a pending exit. It is advisory: a signal can arrive right after it returns `false`.

```rust
#[request_handler]
async fn handle_work(&mut self, msg: Work, ctx: &Context<Self>) -> Result<(), Refused> {
    if ctx.exit_pending() {
        return Err(Refused::ShuttingDown);
    }
    // ...
}
```

State that must be released on every exit, including kills and runtime drops, belongs in `Drop`.

---

## Context

`Context<A>` is the handle passed to every handler and lifecycle hook. It provides access to the actor's own mailbox and lifecycle controls.

| Method | Description |
|--------|-------------|
| `ctx.stop()` | Signal the actor to stop after the current handler finishes |
| `ctx.send(msg)` | Send a fire-and-forget message to this actor (self-send) |
| `ctx.request(msg)` | Send a request and wait for the reply (tasks: async, threads: blocking) |
| `ctx.request_with_timeout(msg, duration)` | Send a request with a custom timeout |
| `ctx.send_request(msg, deadline)` | Send a request without waiting (`gen_server:send_request/3`) and get its `ReplyReceiver<R>` (tasks: a future, threads: `recv()`/`try_recv()`) yielding `Result<R, ActorError>`; the deadline (a `Duration`, an `Instant` or `Deadline::Never`) is fixed at send |
| `ctx.recipient::<M>()` | Get a type-erased `Recipient<M>` for this actor |
| `ctx.actor_ref()` | Get an `ActorRef<A>` from the context |
| `ctx.id()` | This actor's `ActorId` |
| `ctx.monitor(&child_handle)` | Receive a `Down` message when the target exits; returns a `MonitorRef` |
| `ctx.demonitor(monitor_ref)` | Cancel a monitor (a fence with OTP's `flush`: no `Down` afterwards, not even one already queued) |
| `ctx.link(&child_handle)` / `ctx.unlink(&child_handle)` | Bidirectional exit propagation (see [Links, monitors and exit signals](#links-monitors-and-exit-signals)) |
| `ctx.try_link(&child_handle)` | Link, or `Err(NoProc)` if the target is already gone |
| `ctx.exit(&child_handle, reason)` | Send an exit signal from this actor (OTP `exit/2`) |
| `ctx.trap_exit(bool)` | Receive exit signals and link notices as `Exit` messages instead of exiting |
| `ctx.exit_pending()` | Whether this actor's exit has begun or a parent exit is queued |
| `ctx.spawn_helper(future)` / `ctx.spawn_blocking_helper(f)` | Actor-owned helper tasks, cancelled and joined before cleanup completes (v0.7) |
| `ctx.register_name(name)` | Register this actor under a name, released with its generation (v0.7) |
| `ctx.execution_group()` | The `ExecutionGroup` a managed actor runs on (v0.7) |
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
| `actor_ref.with_request_timeout(duration)` | Clone a configured handle for generated protocol methods (v0.7) |
| `actor_ref.send_request(msg, deadline)` | Returns the request's `ReplyReceiver<R>` (as `ctx.send_request`) |
| `actor_ref.recipient::<M>()` | Get a type-erased `Recipient<M>` |
| `actor_ref.context()` | Get a `Context<A>` (for timer setup, etc.) |
| `actor_ref.join()` | Wait until the actor has fully stopped (tasks: `.await`, threads: blocking) |
| `actor_ref.id()` | The actor's `ActorId` |
| `actor_ref.child_handle()` | Type-erased `ChildHandle` for monitors, links and exit observation |
| `actor_ref.exit_reason()` / `actor_ref.wait_exit()` | The `ExitReason` once stopped |
| `actor_ref.ready()` | Wait until a managed actor's `started()` completed (v0.7) |
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
Archived backend benchmarks found these limits:

- **`Backend::Blocking` stalls past the blocking pool.** Tokio's blocking pool
  defaults to 512 threads. With 1,024 persistent blocking actors every run hit
  the benchmark watchdog; with 512 every run completed (macOS and Linux). Raise
  `max_blocking_threads` on your runtime or keep the population below it.
- **Per-actor runtimes cost file descriptors.** Each isolated runtime owns a
  reactor. A per-actor current-thread runtime variant failed to start 1,024
  actors under a 4,096-descriptor soft limit and completed once the limit was
  raised to 16,384. `Backend::Thread` also creates one runtime per actor, so
  check `ulimit -n` before running about a thousand of them.
- **Spinning shards burn their cores when idle.** The private spinning pinned
  profile measured about one busy core per worker with no traffic (eight
  busy-core equivalents on eight workers). The public pinned groups park instead.

Sources: [design findings](../legacy/benchmarks/DESIGN-FINDINGS.md) and the
[backend study findings](../legacy/research/backend-experiments/2026-11-06-fede/FINDINGS.md);
throughput conclusions are in the [benchmarks README](../benchmarks/README.md#conclusions).

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

The runtime is deliberately not `current_thread`: there, async helpers only
progress while the root thread polls them, so a handler that synchronously waits
for a local async helper would deadlock, and `block_in_place` would panic. See
the [dedicated-runtime review](../legacy/docs/reviews/dedicated-runtime.md).

### Actors on different runtimes

Actors hosted on separate Tokio runtimes exchange ordinary messages and
request/reply through `ActorRef`. Sending an `ActorRef` does not migrate an actor
or its Tokio I/O resources. Separate runtimes add threads and inter-runtime
wakeups, and their capacity cannot rebalance automatically; communication
compatibility alone is not a reason to create more runtimes. For owned,
budgeted runtimes use [execution groups](#execution-groups-and-supervision).

Public owned parked pinned shards and bounded native pools are described under
[execution groups](#execution-groups-and-supervision). The `backend-experiments`
feature still contains private research selectors (pinned
and spinning shards, alternative schedulers). They are not stable configuration
APIs, and Tokio stays the default. Their measurements and reproduction guides are
archived in [the backend notes](../legacy/docs/BACKENDS.md) and the
[backend research index](../legacy/research/backend-experiments/README.md).

---

<a id="tokio-free-synchronous-builds-unreleased"></a>

### Tokio-free synchronous builds (v0.7)

Disable the default `async` feature on both Spawned crates to use only the
synchronous `threads::Actor` API without compiling Tokio. Native actors still
use one OS thread per actor and native helper threads; this is a dependency
boundary, not a shared worker pool. ActorSystem and supervision require async.
Cargo feature unification can re-enable Tokio if another dependency enables async.

The [isolated example](../examples/thread_only/README.md) exercises protocols,
requests, links, monitors, timers, completion and optional metrics. Its independent
workspace and graph check avoid accidental activation by other workspace examples:

```sh
cargo run --locked --manifest-path examples/thread_only/Cargo.toml --features metrics,async
python3 scripts/check-thread-only.py
```

The fixture's own `async` feature is deliberately unrelated to the dependency's,
so this also checks feature-aware protocol generation.

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

`Receiver<M>` is the object-safe trait that provides `send()` and `send_request()` for a single message type (on a `Recipient`, `send_request` takes a `Deadline`: `Deadline::after(d)` or `d.into()`).

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

<a id="protocol-timeout-configuration-unreleased"></a>

### Protocol timeout configuration (v0.7)

Generated requests retain the five-second default. Set a default for one
protocol and override selected request methods:

```rust
#[protocol(timeout = std::time::Duration::from_secs(30))]
trait ImportProtocol: Send + Sync {
    fn import(&self, block: Vec<u8>) -> Response<()>;
    #[request_timeout(std::time::Duration::from_secs(2))]
    fn status(&self) -> Response<u64>;
}
```

Use `actor.with_request_timeout(limit).status()` for a call-specific override.
It returns a cloned configured handle, so retaining it applies that setting to
its subsequent protocol calls. The original handle is unchanged. Priority is
handle setting, method attribute, protocol default, five seconds.

Configure before converting to a protocol reference:
`actor.with_request_timeout(limit).to_import_ref()`. The configured handle also
implements the protocol, including cfg-gated methods and its `ToXRef` converter.
Existing manually implemented protocol fakes need no new methods. An already
erased `ImportRef` does not expose the concrete-handle builder.

Settings are `std::time::Duration` expressions; method expressions are evaluated
before message arguments move. Send-only methods have no timeout, and a
`request_timeout` attribute on one is a compile error. Manual message calls use
`request_with_timeout(msg, limit)`.

Enqueue still happens when the generated method is called. The deadline is
fixed then, at send (send time plus the timeout, as OTP's `call`), and travels
with the request; in async mode the response future waits until that deadline
when polled. In native mode the protocol call blocks immediately. Timeout/drop does not cancel accepted work or
roll back side effects; decide retry safety in application code. These settings
do not propagate deadlines to downstream requests.

Run the [asserted example](../examples/capabilities/src/bin/protocol_timeouts.rs)
in both modes, or follow the [0.7 client migration guide](MIGRATION.md#preparing-for-07).

## Deferred replies and pipe-to-self

A handler that would await downstream work blocks its whole mailbox. Instead,
take the request's reply obligation out of the turn and answer it later; the
caller and the protocol do not change.

Every `Handler::handle` receives a `PendingReply<'_, R>`. `#[request_handler]`
methods answer it for you with their return value. A `#[deferred_handler]`
receives it:

```rust
#[actor(protocol = PeerTableProtocol)]
impl PeerTable {
    #[deferred_handler]
    async fn wait_for_peer(&mut self, msg: WaitForPeer, reply: PendingReply<'_, Lease>,
                           _ctx: &Context<Self>) {
        if let Some(lease) = self.take_lease(&msg.caps) {
            reply.reply(lease);
            return;
        }
        match reply.defer() {
            Ok(token) => self.waiters.push_back((msg.caps, token)), // a ReplyTo<Lease>
            Err(DeferError::Overloaded) => {} // the caller was already answered Overloaded
            Err(DeferError::Closed) => {}     // the caller timed out or left
            Err(DeferError::NoCaller) => {}   // reached by `send`
            Err(_) => {}                      // `DeferError` is #[non_exhaustive]
        }
    }
}
```

Rules (the [design](design/deferred-replies.md) has the full contract):

- `defer()` takes one of the instance's `Actor::MAX_OUTSTANDING` credits
  (default 1024). At the bound the caller gets `Err(Overloaded)` at once.
- A `ReplyTo<R>` is `Send + 'static` and answers exactly once:
  `token.reply(v)` returns a `Delivery` (`Delivered`, `CallerGone`, `Expired`,
  `OwnerExited`); `token.overloaded()` answers `Err(Overloaded)`.
- Dropping an unanswered token answers `ReplyDropped`. If the turn panicked or
  the actor is exiting, it is held instead and answered at the actor's death
  with its final reason. `stopped()` may still answer stored tokens; at the
  actor's death every open token answers `ActorStopped(reason)`.
- The deadline is the caller's, fixed at send. `token.is_closed()` tells you
  the caller can no longer be answered; the actor's deadline heap expires
  stored tokens on time and returns their credits.
- `PendingReplies<K, R>` is a bounded table of tokens by key (purges terminal
  entries on insert; `InsertError::{Duplicate, Full}` hands the token back).

**Answering with a reason.** The framework's errors (`ReplyDropped`,
`ActorStopped`, `Overloaded`, `RequestTimeout`) say what happened to the
request, not why your actor could not serve it. To carry your own reason, make
the reply type a `Result`, as `gen_server` replies `{error, Reason}`:

```rust
impl Message for GetPeer { type Result = Result<Lease, PeerError>; }

// a ReplyTo<Result<Lease, PeerError>> answers either way, exactly once:
token.reply(Err(PeerError::Disconnected));
// the caller sees Ok(Err(PeerError::Disconnected)): the request worked,
// the answer is "no"; Err(ActorError::..) stays the transport's.
```

**Send-only handlers.** A manual `impl Handler<M>` for a message whose
`Result` is `()` can leave out the reply parameter with `#[send_handler]` on
the impl; the impl answers `()` once `handle` returns (also after an early
`return`), so a `request` gets `Ok(())` rather than `ReplyDropped`. This is
the manual form of a `#[send_handler]` method in an `#[actor]` block:

```rust
#[send_handler]
impl Handler<Tick> for Poller {
    async fn handle(&mut self, _msg: Tick, ctx: &Context<Self>) {
        self.poll().await;
    }
}
```

**Pipe-to-self jobs** run work owned by the actor and deliver the result as a
message in a later turn (OTP's `Task.Supervisor.async_nolink` plus
`handle_info`). A gossip verdict checked off the actor, with a deadline, that
reports exactly one outcome per object even when the actor is at a bound:

```rust
#[send_handler]
async fn on_gossip(&mut self, msg: Gossip, ctx: &Context<Self>) {
    // Reserve first: on refusal nothing has been moved into the job yet.
    let opts = JobOptions::new().deadline(VERDICT_DEADLINE);
    let Ok(permit) = ctx.reserve_blocking_job(opts) else {
        return self.report(msg.id, Verdict::Ignore(Overloaded));
    };
    let Gossip { id, object } = msg;
    permit.pipe_blocking(move |_job| check(&object), move |res| Checked { id, res });
}

#[send_handler]
async fn on_checked(&mut self, msg: Checked, _ctx: &Context<Self>) {
    let verdict = match msg.res {
        Ok(outcome) => outcome,
        Err(JobError::DeadlineExceeded) => Verdict::Ignore(TimedOut),
        Err(_) => Verdict::Reject(CheckFailed), // a panic in `check`
    };
    self.report(msg.id, verdict);
}

// Delegation: the obligation and its credit stay here.
let token = reply.defer()?;
let downstream = other.send_request(Lookup(key), token.deadline()); // same deadline as our caller
ctx.pipe_reply(JobOptions::new(), token, |_| async move { downstream?.await }, |res, token| Delegated { res, token })?;
```

- Four calls, one per shape: `ctx.pipe` (async work), `ctx.pipe_blocking`,
  `ctx.pipe_reply` and `ctx.pipe_reply_blocking` (linked to a deferred
  reply). Each takes `JobOptions` first, built with `JobOptions::new()` and
  `.key(k)`, `.deadline(d)` (a `Duration`, `Instant` or `Deadline`),
  `.emit_credits(n)` and, for `pipe_reply*` only,
  `.cancel_on_caller_gone()`. The work receives the job's `JobCtx` by
  value (`|_| fut` when it needs neither cancellation nor emits).
- `done(outcome)` runs in the owner's turn. `JobOutcome<T>` is
  `Result<T, JobError>`; `Err` carries `JobError::Panicked` or
  `JobError::DeadlineExceeded`. `JobError` and `PipeError` are
  `#[non_exhaustive]`: match with a wildcard arm.
- `ctx.pipe*` reject at once at a bound (`PipeError`, never waiting) and drop
  `work` and `done`. When the caller must still produce a result,
  `ctx.reserve_job(opts)` / `ctx.reserve_blocking_job(opts)` take the
  admission first (owner running, a credit, and for blocking jobs a native
  slot) and return a permit whose `pipe` / `pipe_blocking` cannot fail.
  `pipe_reply*` errors hand the token back.
- `JobOptions::key` cancels the previous job with that key and
  fences its queued messages; `JobHandle::cancel` fences one job.
- `JobCtx::emit_blocking` (and `emit` for async jobs) sends fenced messages,
  bounded by `JobOptions::emit_credits`; the terminal message is always last.
  `JobCtx::emit_at(at, msg)` (`emit_at_blocking` in blocking jobs) delivers
  at `at`, fenced and credit-accounted the same way: it never reaches a newer
  job's turn or a newer instance, it is dropped if the job is cancelled or
  replaced first, and the job's terminal message waits for it. An `at`
  already reached, or in the past, is an immediate emit (enqueued at the
  call, ahead of later emits): no need to branch to `emit` yourself. An
  `at` past `JobOptions::deadline` is accepted, not an error: if the
  deadline passes first it takes the notification and the emit is dropped
  at its instant (if the job finishes first, the emit and then the result
  arrive as usual).
- **Job deadlines are hard.** When `JobOptions::deadline` passes, `done` gets `DeadlineExceeded` at once, later
  emits fail with `JobGone` and the work's eventual result is dropped. The
  work itself keeps running (cooperative cancellation) and keeps its credit
  and native slot until it returns. For a **soft deadline** (stop starting
  new work, finish and deliver what is in hand), leave `JobOptions::deadline`
  unset and check the time in the work:

  ```rust
  let stop_starting_at = Instant::now() + SOFT_DEADLINE;
  ctx.pipe_blocking(JobOptions::new(), move |job| {
      let mut done = Vec::new();
      for item in batch {
          if job.is_cancelled() || Instant::now() >= stop_starting_at { break; }
          let out = prove(item);                 // a started item finishes
          let _ = job.emit_blocking(Proven(out.id)); // and is delivered
          done.push(out);
      }
      Summary::from(done)
  }, |res| BatchDone { res })?;
  ```

  `spawned_concurrency::Instant` (also `tasks::Instant`, `threads::Instant`)
  is the deadline clock: `std::time::Instant`, or the simulated clock under
  `--cfg spawned_sim`, so code need not depend on `spawned-rt`.
- **Bounds.** Each unlinked job holds one `MAX_OUTSTANDING` credit; blocking
  jobs also count against `Actor::MAX_NATIVE_JOBS` (default
  `min(4, available_parallelism)`).

  > **Size the native budget.** The default of at most 4 blocking jobs per
  > actor is easy to outgrow now that blocking jobs run on Tokio's blocking
  > pool: an actor that verifies, proves or does I/O in blocking jobs
  > usually needs tens or hundreds (one client port needed 352). Under load
  > the only symptom is `PipeError::NativeBudget` refusals; the first one
  > per actor instance is logged as a `native_budget_exhausted` warning.

  Set both in the macro,
  `#[actor(max_native_jobs = 64, max_outstanding = 4096)]` (or as `Actor`
  constants in a manual impl), and override them per supervised child with
  `ChildSpec::max_native_jobs(n)` / `ChildSpec::max_outstanding(n)`.
- **Executors.** In `tasks` mode async jobs run on the owner's runtime and
  blocking jobs on Tokio's blocking pool (`spawn_blocking` of the actor's
  group runtime, or the current runtime), still bounded by the native
  budget; a runtime that is dropped waits for blocking jobs still running,
  as with any `spawn_blocking`. `threads` mode offers `pipe_blocking`
  and `pipe_reply_blocking` on owned native threads (a reservation
  spawns the thread, parked until the job starts).
- Owner exit cancels every job; after death the jobs are joined, bounded by
  the cleanup deadline, and stragglers are reported (`unfinished_at_stop`).

Test a manual handler directly with `spawned_test::reply_pair()`, and run
code inside a live actor's turn (to start a job or inspect state) with
`spawned_test::tasks::in_actor` / `spawned_test::threads::in_actor`. Run the
[deferred replies example](../examples/capabilities/src/bin/deferred_replies.rs).

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
#[non_exhaustive]
pub enum ActorError {
    ActorStopped(ExitReason), // The actor died before replying, with this final reason
    NoProc,                   // The actor was already gone when sent to (OTP noproc)
    RequestTimeout,           // A request exceeded its deadline (default 5s)
    ReplyDropped,             // The handler dropped the request unanswered
    Overloaded,               // The actor rejected the request at a bound
}
```

- `send()` returns `Err(NoProc)` once the actor is past its death point. While the actor is stopping (after a stop request, including during `stopped()`), `send()` still returns `Ok(())` and the message is dropped without being handled: `Ok` means accepted, not handled.
- `request()` returns `Err(ActorStopped(reason))` if the actor dies before replying, with the reason `wait_exit()` and monitors report (`Killed`, `Panic(..)`, the exit signal's reason, ...), `Err(NoProc)` if it was already gone, or `Err(RequestTimeout)` if the reply doesn't arrive in time. A request whose handler panicked is answered when the actor dies (after `stopped()`), not at once.
- `request_with_timeout()` lets you specify a custom timeout.

`ActorError` is `#[non_exhaustive]`. Classify it with the helpers instead of an exhaustive match:

| Variant | `target_gone()` | `is_timeout()` | `is_transient()` | `exit_reason()` |
|---|---|---|---|---|
| `ActorStopped(r)` | `true` | `false` | `false` | `Some(&r)` |
| `NoProc` | `true` | `false` | `false` | `None` |
| `RequestTimeout` | `false` | `true` | `true` | `None` |
| `ReplyDropped` | `false` | `false` | `false` | `None` |
| `Overloaded` | `false` | `false` | `true` | `None` |

`target_gone()`: retrying the same reference cannot succeed (look the actor up again; a supervisor may have restarted it). `is_transient()`: retrying the same target may succeed. `ReplyDropped` is neither: the actor is alive but its handler chose not to answer (a handler that means "not now, try again" answers `reply.overloaded()`, which is transient; a caller whose protocol retries dropped requests matches `ReplyDropped` explicitly). `ExitReason::is_abnormal()` is `false` only for `Normal` and `Shutdown` (`Killed` and `NoProc` are abnormal).

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

<a id="links-and-trapped-exits"></a>
<a id="links-and-trapped-exits-unreleased-main"></a>

## Links, monitors and exit signals

Exit signals follow Erlang/OTP (v0.8, [design](design/otp-exit-signals.md)).
Everything in this section works in both modes and across modes; use the
`threads` imports and synchronous hooks in synchronous mode. The
[exit_signals example](../examples/capabilities/src/bin/exit_signals.rs) runs
each rule below.

### Exit signals

`ChildHandle::exit(reason)` sends an exit signal from outside any actor
(`from` is `ActorId::EXTERNAL`); `ctx.exit(&target, reason)` sends it from
the calling actor; `ChildHandle::kill()` is `exit(ExitReason::Kill)`. The
target applies OTP's `exit_signal/2` rules, with the trap flag read when the
signal arrives:

| Signal | Non-trapping target | Trapping target |
|---|---|---|
| `Kill` | ends `Killed` | ends `Killed` (untrappable) |
| `Normal` | ignored | `Exit { from, reason: Normal }` queued |
| any other reason | ends with that reason after its current handler, without `stopped()`; its queue is discarded | `Exit { from, reason }` queued |
| from the target's parent | as above | a parent exit is queued (see [Parent exits](#parent-exits)) |

A kill supersedes any other reason until the actor's death point: an actor
already stopping (even inside `stopped()`) ends `Killed`, and a panic after
the kill does not replace it. `stopped()` is skipped if it has not begun; a
`stopped()` that is running finishes, since Rust cannot interrupt it. A
non-kill signal to a non-trapping actor that is already exiting replaces its
reason, unless it was killed; a signal after the death point does nothing.
Signals that arrive while `started()` runs take effect when it returns, with
the trap flag read then, so `ctx.trap_exit(true)` first thing in `started()`
covers them.

`stop()`, `shutdown()` and `stop_with_reason(r)` (on a `ChildHandle` or the
`Context`) are not exit signals. They are administrative stops, as
`gen_server:stop`: untrappable, `stopped()` runs, and the first stop's reason
wins over later stops. `stop_with_reason(ExitReason::Kill)` is such a stop
and ends `Kill`; only a `Kill` exit signal (`kill()`, `exit(Kill)`)
produces `Killed`.

### Links and trapped exits

Links are bidirectional. Use `ctx.link(&child_handle)` and
`ctx.unlink(&child_handle)` for existing actors, or
`child.start_linked(&parent_ctx)`, which links before the child's `started()`
and makes the caller its parent. Repeated links and links to oneself are
no-ops. When a linked actor dies, each peer receives a link notice with its
final reason, under the same rules as an exit signal except that a `Kill`
notice is trappable: a non-trapping peer ignores `Normal` and otherwise ends
with the same reason, without `stopped()`, and propagates it to its own links.

Call `ctx.trap_exit(true)` to receive signals and notices through
`Actor::exit_received` instead:

```rust
use spawned_concurrency::{actor, Exit};
use spawned_concurrency::tasks::{Actor, Context};

struct Watcher;

#[actor]
impl Watcher {
    #[started]
    async fn setup(&mut self, ctx: &Context<Self>) {
        ctx.trap_exit(true);
    }

    #[exit_received]
    async fn peer_exit(&mut self, exit: Exit, _ctx: &Context<Self>) {
        tracing::warn!(actor = %exit.from, reason = ?exit.reason, "Linked actor exited");
    }
}
```

The hook is optional and defaults to doing nothing. `Down` carries the
monitor's `MonitorRef`, the monitored actor's `ActorId` (`actor`, OTP's
`Object`) and its reason; `Exit` carries `from` and `reason`. Both are
`#[non_exhaustive]`: match them with `..`. `Exit`, `Down` and
parent exits are ordinary mailbox messages: they queue behind messages already
there and never interrupt a running handler. A death notice follows every
message the dying actor sent earlier to the same receiver. A link notice is
queued before the dying actor's `wait_exit()` returns; a `Down` is queued by
the monitor shortly after.

A link or monitor made through a `Context` whose actor is already exiting (for
example from a helper that outlives it) is inert: no record and no notice.

### Parent exits

An actor's parent is fixed when it starts: the actor passed to
`start_linked`, the supervisor of a supervised child, or the `ActorSystem` of
a standalone `start_on` actor; a plain `.start()` actor has none. A signal
from the parent to a **trapping** actor is queued as a parent exit: when
dispatched, the actor exits with that reason (`Normal` included) and
`stopped()` runs; `exit_received` never sees it. A non-trapping actor treats
it as any exit signal.

Supervisor and system shutdown are the parent's `Shutdown` signal. A
non-trapping child ends `Shutdown` without `stopped()`. A trapping child
handles the messages queued before the shutdown, then runs `stopped()`
([`exit_pending()`](#when-stopped-does-not-run) reports the queued shutdown).
A child that has not reached its death point `shutdown_timeout` after its
signal is killed, and reported stuck after a second interval; a system kills
the actors left when its `grace` expires.

### NoProc

An actor is gone from its death point on, which comes right after `stopped()`
returns (or is skipped). After it:

- `ctx.link(&h)` delivers one `Exit { from: h, reason: NoProc }` to a trapping
  caller; a non-trapping caller receives that exit signal and ends `NoProc`;
- `ctx.try_link(&h)` returns `Err(NoProc)` and keeps no link;
- `ctx.monitor(&h)` delivers one `Down { reason: NoProc }`;
- `send`/`request` return `Err(ActorError::NoProc)`.

Before it, including while the target runs `stopped()`, links and monitors
register and fire with the final reason. `ChildHandle::exit_reason()` and
`wait_exit()` give the real reason once it is published, shortly after the
death point.

### Unlink and demonitor are fences

Once `ctx.unlink(&h)` returns, that link has no further effect on the caller;
once `ctx.demonitor(r)` returns, no `Down` of that monitor is queued or
handled: one already in the mailbox is dropped unhandled, as OTP's
`demonitor(Ref, [flush])`. An `Exit` already in the mailbox stays. Linking
again creates a new link that gets its own notice.

### Runtime drop

An actor whose runtime drops it (runtime shutdown, abort, never polled) ends
`Killed` in both modes, unless a reason other than `Normal` was already
requested. `stopped()` does not run; links receive `Killed` and monitors
`Down { reason: Killed }`.

Callers see the same final reason: see [Error Handling](#error-handling)
(`ActorStopped(reason)`, `NoProc`). Supervisor startup rules are in
[Construct fresh children after failure](#construct-fresh-children-after-failure).

---

## Execution groups and supervision

Available in v0.7.0. These are opt-in capabilities for async actors;
ordinary `.start()`, message types, handlers and protocols retain their behavior.
Parked async pools, persistent native actor-thread pools and parked Linux pinned
shards share the same ownership API. Supervisors support nested child specs,
OneForOne, OneForAll and RestForOne. Spinning remains experimental.

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
Construct groups during application setup: construction is synchronous and
fallible, and no actor is admitted to a failed group.

| Constructor | Actor execution | Admission and thread cost |
|---|---|---|
| `async_group_on(name, &Handle)` | Managed scopes on an existing Tokio runtime | No new async pool; caller keeps the shared runtime alive and driven through shutdown. One join coordinator; worker count describes shared capacity. |
| `async_group(name, workers)` | Cooperative actors on a multithread Tokio pool | Any actor count; `workers` async workers plus one runtime owner and one join coordinator. |
| `thread_group(name, workers)` / `thread_group_with_helpers(name, workers, helper_workers)` | Fixed native workers, one actor root per slot | At most `workers` live scopes; one shared helper runtime with one async worker by default (or the explicit helper budget), one owner and one join coordinator. Both worker budgets must be between 1 and 1024. Idle actors retain slots. Full admission returns `SystemError::Capacity`. |
| `pinned_group(name, cpus)` | One parked current-thread Tokio runtime per specified Linux CPU | Any actor count; one native runtime owner per CPU plus one join coordinator. Actors sharing a shard cooperate. |

Each system also has one control thread. Blocking helpers can create additional
Tokio blocking threads; these profiles do not impose a global thread or memory
limit. `worker_budget()` counts async workers, native actor slots, or pinned
shards respectively. `profile()` describes the resource model explicitly.

Pinned construction validates unique CPUs against the caller's allowed affinity
mask, then pins each owner before building its reactor. Failure unwinds all
started owners before returning an error. Hard affinity is Linux-only and does
not reserve exclusive CPUs. Use `group.shard(index)` to place a pipeline stage
or select a native worker slot; shared async pools have no stable shard index.
Explicit selection survives supervised restarts. Default placement may choose a
different shard for each generation. Pinned shards park when idle; no spin loop
or Flux broadcast/loss semantics are introduced.

```rust
let roots = system.thread_group("blocking", 4)?;
let cpus = spawned_rt::tasks::affinity::available_cpus()?; // Linux
let pipeline = system.pinned_group("pipeline", cpus.into_iter().take(2).collect())?;
let stage = MyActor::new().start_on(&pipeline.shard(0)?)?;
```

Actors started through `ctx.execution_group()` retain explicit pinned-shard placement. Native thread slots are exclusive, so a native actor’s context-derived group admits a new actor on any free slot instead of inheriting its occupied slot. Use `group.shard(i)` when reserving a particular native slot explicitly.

Native actor roots share a single async helper runtime, rather than creating a
runtime per actor. Handlers/hooks execute on their reserved native worker;
registered helpers and child factories execute on the shared helper runtime.
This supports blocking actor turns while async timers/I/O helpers keep running.
It is a persistent actor pool, not per-handler worker leasing. Helpers must finish
before a slot is reusable. Thread-local resources belong to the persistent worker,
not to each actor generation; they are destroyed when the group terminates.
Pinned handlers and helpers run on their assigned shard; blocking either can
stall the whole shard. Group completion waits for actual native owner joins,
including runtime teardown and thread-local destructors. Join failures are reported
in `ShutdownReport::group_failures`; `complete` means termination, not business success.
See the [runnable profile example](../examples/capabilities/src/bin/execution_profiles.rs)
and [backend studies](#execution-modes-and-backends). Published experiment ratios are not a performance
claim for this public implementation.

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
keeps closed owned runtimes alive until tracked work actually finishes, even if an
incomplete report is dropped. An uncooperative operation can therefore keep
threads/resources alive indefinitely; the host decides process-level escalation.

To adopt ownership incrementally, use
`system.async_group_on("client", &tokio::runtime::Handle::current())?` and
`actor.start_on(&group)`. Group shutdown closes admission and joins only managed
actors and their registered helpers. It leaves the borrowed runtime and ambient
`.start()`/`tokio::spawn` tasks running. Keep the runtime alive and driven until
cleanup completes; cloning a Tokio handle does not keep its runtime alive. On a
current-thread runtime, keep calling `Runtime::block_on` while awaiting shutdown.
Use an owned `async_group` for independent scheduling capacity and runtime
lifetime guarantees. Ambient `.start()` remains unmanaged, including inside a
managed handler; use `ctx.execution_group().start(...)` for owned child work.

The restart limit counts restart actions across the supervisor, as OTP's
intensity does: a OneForOne child retry is one, and a OneForAll/RestForOne
recovery round is one however many children it rebuilds (a six-child
OneForAll round needs one available grant). The round takes its grant when it
is selected, before the cohort is stopped; a replacement that fails inside the
round requests a new round, which takes another.

Retry capacity is checked and reserved before a backoff. An exhausted window
fails immediately, even if an older grant would expire during the proposed wait.
Concurrent children cannot reserve the same remaining capacity. A canceled or
superseded backoff releases its reservation; the rolling window records the
actual retry grant after the wait. `restart_scheduled` reports only a granted
replacement, after backoff and supersession checks, rather than promising a
replacement that may never be admitted.

### Shutdown diagnostics and ordering

An incomplete `ShutdownReport` includes `unfinished_actor_details`: ActorId,
ChildSpec name (or first successful managed registration), and Rust actor type.
An actor can already appear in `exits` while its registered helper keeps its
scope unfinished. Enable `spawned-concurrency/diagnostics` to also capture the
active message type and elapsed turn time. This costs a clock read and two accesses to a dedicated diagnostic slot
per managed turn, without locking lifecycle state or cloning scope ownership; it is disabled by default and independent of metrics
sampling. Idle actors, lifecycle hooks and helper-only cleanup have no active
message/turn age. Snapshots are approximate; no message payloads are retained.

Standalone actors stop in reverse admission order; there is no dependency DAG
API. For dependencies, declare children dependency-first in a supervisor, as in
[coordinated shutdown](../examples/capabilities/src/bin/coordinated_shutdown.rs).
That supplies reverse declaration order within that tree. Across independent
trees, close ingress and coordinate dependent subsystems explicitly.

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
cancelled and joined before a replacement is retried, including when the
factory never returns an actor. Existing `started()` and `stopped()` hooks keep their signatures. Readiness
commits after `started()` returns and admission remains open; no ordinary handler
runs before that barrier. Enqueued startup messages are discarded if startup fails.
Calling `ready()` later reports that historical barrier, not current liveness.
If a managed actor's admission closes before `started()` begins, neither
`started()` nor `stopped()` runs; the unstarted state and queued messages are
dropped. This applies to standalone `start_on` actors as well as supervised
children. Resources acquired by a constructor must therefore be released by
Drop, not only by `stopped()`. Legacy unmanaged `.start()` and synchronous
`threads` actors retain their behavior of running both hooks after an early
cooperative stop, provided startup itself succeeds.
Failed readiness waits for the actual startup outcome, so a non-returning hook
can keep it pending; use a timeout.

The initial start follows OTP's `start_link` (v0.8):

- A child's **first start** that fails for any reason (factory error or panic,
  panic or stop in `started()`, a missed `startup_timeout`, placement or group
  closure) fails `start()` whatever the restart mode; it is not retried. Retry
  in the application if a dependency may be briefly unavailable at boot.
- The started children roll back in reverse order, and `start()` returns its
  error only after every child is terminal (or `Failed` once its cleanup is
  stuck): `SupervisorError::FailedToStartChild { child, cause, rollback_failures }`,
  or `StartupCleanupStuck`.
- A ready child that exits while later siblings are still starting is
  recorded; its exit is handled under the strategy once the whole tree is
  ready, as OTP handles a child's `'EXIT'` after `init` returns.
- A replacement that fails before readiness is a failed restart attempt and
  is retried under the restart policy and budget.

A supervisor accepts only groups owned by its ActorSystem. ActorRefs can still
communicate between independent systems. Children start in declaration order
and shut down in reverse order. Startup
future cancellation requests rollback until readiness commits. After that
historical commitment, the tree remains ActorSystem-owned even if admission
closes before `start()` is polled to completion or the startup future is dropped.
Startup success does not promise continued liveness: inspect `failure()` or the
shutdown report for failures occurring after readiness, including before return.
Each spec has a startup and cleanup timeout (default five seconds). A stuck actor, startup
hook or registered helper fails the supervisor and suppresses replacements until
actual cleanup completes. There is at most one live generation per child slot.
Stop observation uses internal tokens/completion state, independently of ordinary
mailbox traffic. It does not interrupt a running handler. If a supervisor report deadline expires,
remaining siblings are also signalled before returning the incomplete report;
actual completion still waits for the stuck work.

Set `ChildSpec::startup_timeout` and `shutdown_timeout` explicitly for latency
sensitive services, for example 500 ms and 1 s with a 3 s overall system deadline.
Their defaults are each 5 s per attempt/generation, not shares of the overall
deadline. System deadline expiry signals remaining children and returns a report;
it does not kill Rust code or release resources before actual cleanup finishes.
Defaults cannot be derived from a deadline supplied only at future shutdown.

Supervision emits structured `tracing` events: `restart_scheduled` and
`restart_started` at INFO; `restart_budget_exhausted` and `supervision_give_up`
at WARN. Fields include `child`, `generation`, `reason` and an available error.
Enable an application tracing subscriber to collect them; Spawned installs none.

| Restart mode while supervisor is running | Normal / Shutdown | Panic / Kill / Killed / NoProc |
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
Receiver-owned cleanup discards queued envelopes individually during completion,
cancellation and unwinding in both modes. Queued messages and the actor state
are dropped after the death point, so a panicking destructor does not change
the exit reason; it is recorded in `child_handle.cleanup_failure()`, and a
Transient child whose reason is `Normal` or `Shutdown` restarts when its
cleanup failed. An actor dropped by its runtime ends `Killed`. Every accepted
queued request is answered before actor completion, even while ActorRefs
remain alive.

Async sends retain their channel fast path; receiver cleanup closes Tokio's
channel and receives through EOF without cooperative-budget deferral. Native
sends use a publication gate (two atomic operations per send, no per-send heap
allocation); receiver teardown closes it and waits for accepted sends before
emptying the channel. Cleanup is synchronous and cannot preempt a blocked user
Drop implementation or an unfinished send. This relies on unwinding:
`panic=abort`, an aborting panic hook, or multiple panicking fields in the same
user value cannot be recovered by the envelope boundary.

`ctx.stop_with_reason(ExitReason::Shutdown)` requests a cooperative shutdown;
`ExitReason::Kill` requests an abnormal exit without panicking. Both modes finish
the current handler and run `stopped()`. The first stop's reason wins over later
stops; a panic, a kill or an untrapped exit signal before the death point still
replaces it. A transient supervisor restarts Kill exits but leaves Shutdown
exits stopped. `ctx.stop()` keeps Normal.

The default shared budget permits five retry attempts per rolling 30-second
window. Initial construction does not consume a retry; every subsequent attempt
does, including failed factories and normal replacements of Permanent children.
A zero retry budget fails immediately without waiting for backoff. Otherwise
retry accounting is stamped immediately before factory admission, after the
cancellable delay. Pending backoffs reserve capacity until admission or refund;
their reservations do not expire with the window. This also caps concurrent
pending retries when backoff exceeds the window. Completed grants expire with
the rolling window, so this is not a lifetime retry cap. Exhaustion fails the
whole supervisor and stops its remaining children. Backoff
is capped exponential with optional jitter, independently per child. With jitter,
`initial` is a cap rather than a minimum: `initial=1s` allows a first delay from
zero through one second, including 0.15–0.8s. Subsequent caps double up to
`maximum`; disable jitter for a delay equal to the cap. A stable
ready lifetime resets that child's exponent. Cleanup time is not healthy uptime.
Use `Backoff::seed` for reproducible policy tests, not security randomness.

`SupervisorConfig` and `Backoff` are `#[non_exhaustive]` (OTP's supervisor
flags grew in OTP 24, and so will these): build them with the setters,

```rust
let config = SupervisorConfig::new()
    .strategy(SupervisorStrategy::OneForAll)
    .max_restarts(3)
    .restart_window(Duration::from_secs(10))
    .backoff(Backoff::new().initial(Duration::from_millis(50)).jitter(false));
```

or assign the public fields of a `default()` value.

### Recovery strategies and nested trees

Set `SupervisorConfig::strategy` explicitly; OneForOne remains the default.

| Strategy | After a restartable child exits |
|---|---|
| OneForOne | Replace that child only. |
| OneForAll | Stop the child cohort, then rebuild it in declaration order. |
| RestForOne | Stop that child and all later declared siblings, then rebuild those in order. |

Cohorts stop in reverse order and wait for **all affected scopes** to finish
before constructing any replacement. Stop/cleanup deadlines fail the tree instead
of overlapping generations. Temporary collateral children stop without replacement;
other collateral children are reconstructed even if their own exit was a normal
shutdown. Non-restartable origin exits do not initiate a cohort. Concurrent failure
notifications are coalesced into a bounded per-child pending set, with one recovery
cohort at a time. A late old-generation failure is absorbed by its assigned
round. A new upstream failure interrupts reconstruction and starts a fresh
cleanup round, so dependent factories cannot hold recovery behind a dead upstream.
Initial startup is serial and does not retry a failed first start. If an
already-ready dependency exits before the whole tree commits readiness, its
exit is deferred until then and handled under the strategy, so it is not
replaced alone underneath initializing children.

The rolling restart budget charges every subsequent factory invocation, including
collateral reconstruction and failed constructors. For a large OneForAll tree,
configure `max_restarts` to accommodate the whole cohort. The origin uses its
backoff; collateral children incur no extra exponent delay. Shutdown interrupts
pending backoff and reconstruction.

`SupervisorBuilder::add_supervisor(SupervisorSpec::new(name, &group, config, configure))`
adds an ordered subtree slot. The configure closure declares a fresh child set for
each subtree generation; async setup belongs in the child factories. Its placement
group hosts an internal lifecycle adapter and declaration callback. An async group
is usually the right adapter placement so it does not occupy a scarce native slot.
Leaf specs can use other groups within the same ActorSystem.
Use this API for parent-owned recovery. Calling a separate builder's public
`start()` inside a factory registers an independent ActorSystem root; it does
not attach that tree to the factory's parent supervisor.

A failed subtree escalates through the adapter to its parent's restart policy.
The adapter's ownership lease persists until all descendants actually finish,
including startup cancellation and blocking helper cleanup after adapter exit.
Late descendant cleanup failures are retained in the parent and system reports;
they fail the parent rather than reporting a clean shutdown.
Subtree generation replacement never implies durable state recovery. Old
`Supervisor` handles remain attached to their old tree; resolve the current tree
through `SupervisedSupervisor::current()` or `wait_for_generation()`.
See the [nested example](../examples/capabilities/src/bin/supervision_tree.rs).

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

`Context::spawn_helper` and `spawn_blocking_helper` require an actor started with
`start_on(&group)` (or by a supervisor). An ambient `.start()` returns
`SystemError::UnmanagedActor` from these methods, even inside a Tokio runtime.
Use an ActorSystem to establish cleanup ownership; no detached fallback is used.

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
- [Execution profiles](../examples/capabilities/src/bin/execution_profiles.rs): fixed native capacity, shared helpers and parked CPU placement.
- [Supervision tree](../examples/capabilities/src/bin/supervision_tree.rs): nested recovery with OneForAll/RestForOne.
- [Supervision recovery](../examples/capabilities/src/bin/supervision_recovery.rs): crash, new identity/fresh state, stale reference failure and explicit replacement lookup.

```sh
cargo run --locked -p spawned-capability-examples --bin execution_groups
cargo run --locked -p spawned-capability-examples --bin execution_profiles
cargo run --locked -p spawned-capability-examples --bin supervision_tree
cargo run --locked -p spawned-capability-examples --bin supervision_recovery
```

The [design contract](design/execution-groups-supervision.md) records the fuller
boundaries. [Failure tests](TESTING.md#managed-execution-and-recovery-main-unreleased)
exercise readiness, cancellation, rollback, restart limits and incomplete cleanup.
These APIs do not establish a full-client performance improvement; measure actual
application workloads with equal resource budgets.

To test how your application survives framework-level faults (a failing child
factory, a slow `stopped()`, a stop racing a restart, a placement failure),
enable the test-only `failpoints` feature in dev-dependencies and arm named
sites from tests; see [failpoints](TESTING.md#failpoints). Never enable it in
production builds.




## Metrics

Enable the optional `metrics` feature on `spawned-concurrency` (available in v0.6.0):

```toml
spawned-concurrency = { version = "0.7.1", features = ["metrics"] }
```

The default build compiles out collection, timing metadata and response observers.
No handler, protocol, message or result type needs changing. Metrics do not change
request timeouts, cancel queued work, restart actors or change mailbox capacity.

Configure before starting actors or constructing observed requests:

```rust
use spawned_concurrency::metrics::{self, Config};

metrics::configure(Config::new().timing_sample_every(16)).unwrap();
// Counters include every operation; one in 16 observations gets timing samples.
let snapshot = metrics::snapshot();
let prometheus_text = metrics::render_prometheus();
```

Default sampling is 1 (all timings). Use 0 for counters/gauges without clocks or
histogram observations. Configuration is process-wide. The one-time `configure` call returns
`Err(config)` if already initialized. The opt-in composable API below can merge
requirements later, including with a policy previously set by `configure`.

For embedded components that initialize independently, use
`metrics::configure_composable(Config::new().timing_sample_every(16))` instead.
It can be called after actors start. The smallest nonzero interval requested by
any component wins; zero means “I only require counters” and never disables
another component's timings. Requirements last for the process lifetime. Instrumentation initially samples every observation until an explicit policy
is supplied; that implicit default is not a competing component requirement.
A zero policy in the host can disable clocks until components opt into timings. Inspect the
effective policy with `metrics::configuration()`. This composes sampling, while
counters and histograms still aggregate the whole process.

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
| Deferred replies (`spawned_deferred_*`) | `deferred_tokens_total` (successful `defer()`), `deferred_pending` (open slots with a live token, all actors), how each deferred slot ended: `replied`, `expired` (its deadline), `dropped`, `stopped` (owner exit or death), `abandoned` (caller gave up), each `_total`; `deferred_overloaded_total` (every `Overloaded` answer, at `defer()` or `ReplyTo::overloaded`); `deferred_reply_latency_seconds`, a sampled histogram from `defer()` to a delivered reply. |
| Jobs (`spawned_jobs_*`) | `jobs_started_total` (admitted), `jobs_running` (executing, cancelled or timed-out work included), terminal notifications `jobs_completed_total`, `jobs_panicked_total`, `jobs_deadline_exceeded_total`; `jobs_cancelled_total` (cancelled while still running: handle, key replacement, owner exit, caller give-up); `jobs_fenced_total` (job messages dropped at dispatch); `jobs_rejected_total` (at the credit bound or the native budget); `jobs_emits_total`; `jobs_unfinished_at_stop_total` (still running at the owner's cleanup deadline). |

Counters end in `_total`. Duration histograms use fixed nanosecond buckets
internally and seconds in Prometheus export; their count/sum/max describe sampled
observations only. Counts are not multiplied by the sampling rate. Do not compare
sampled histogram counts to total request counters as if every request was timed.
Sampling is deterministic round-robin per message/request category, not random;
periodic traffic can bias samples. Quantiles are bucket-based estimates.
Each exported `*_max` is a separate gauge holding the sampled lifetime maximum;
it is not a histogram component or a rolling-window maximum.

Mailbox depth reserves accounting before publication to avoid consumer races.
A stalled producer may temporarily appear in depth before send acceptance. A
failed send rolls that reservation back and increments rejection. This is not a
transactional channel-length API. Counters/gauges are independent atomic reads;
conservation relationships apply after operations quiesce:

- accepted messages = handled + discarded + waiting;
- handled = completed handlers + panicked handlers + aborted handlers + active;
- observed requests = replied + timed out + actor stopped + cancelled + active;
- spawned actor instances = exited + active.
- deferred tokens = replied + expired + dropped + stopped + abandoned +
  pending (an `Overloaded` defer takes no token);
- started jobs = running + jobs whose execution ended (a job's terminal
  notification is completed, panicked, deadline exceeded, or none when it was
  cancelled first).

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

`send_request` returns a `ReplyReceiver<R>`; it does not observe how the caller
waits on it, so it is not a caller-observed request. Its deadline clock is
part of the contract: awaited directly, a raw receiver is expired by the
owner's deadline heap on the real clock (the simulated one under
`spawned_sim`), not by a runtime timer, so it does not follow paused Tokio
time; that saves a timer per request on fan-out paths. Wrap it in
`Response::from(rx)`, or use `request`, when a test pauses Tokio time. Such requests
still contribute message, handler, queue and reply-delivery metrics, but only a
framework request/Response wrapper contributes caller-outcome/latency metrics.
Reply delivery means the reply was stored in the request's slot, not proof the
caller consumed it.
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

## Testing your actors

The `spawned-test` crate, first published with v0.7.1, is a dev-dependency
for unit-testing actors in both execution modes. It uses only the public
`spawned-concurrency` API. The `tasks` helpers need its default `async`
feature; with `default-features = false` it builds Tokio-free with the
`threads` helpers.

```toml
[dev-dependencies]
spawned-test = "0.7.1"
```

### Mock a dependency

`Mock` is an actor that implements `Handler<M>` for **every** message type, so
`ActorRef<Mock>` satisfies every `#[protocol]` blanket impl: call protocol
methods on it, or convert it with the generated `to_x_ref()` and hand it to the
code under test. Code that holds a protocol reference (`XRef`) or a
`Recipient<M>` needs no change.

```rust
use spawned_test::tasks::{Mock, Probe};

let rates = Mock::new()
    .on::<rates_protocol::Rate>(|msg| if msg.currency == "EUR" { 110 } else { 100 })
    .start();
let out = Probe::<Converted>::new();
let converter = Converter { rates: rates.to_rates_ref(), out: out.recipient() }.start();

assert_eq!(converter.convert("EUR".into(), 3).await.unwrap(), Ok(330));
assert_eq!(out.expect(Duration::from_secs(1)).await.cents, 330);
let asked: rates_protocol::Rate = rates.expect(Duration::from_secs(1)).await;
assert_eq!(asked.currency, "EUR");
out.expect_no_message(Duration::from_millis(20)).await;
```

- `on::<M>(|msg: &M| reply)` answers each `M`; the message is still logged.
- `allow_sends()` acknowledges every message whose reply type is `()` without
  a rule; stubbing a wide protocol to observe one method is then a single
  line. A handler cannot tell a send from a request through the public API,
  so this also answers requests returning `Response<()>`; requests with other
  reply types still fail loudly. `allow_send::<M>()` acknowledges one type
  only.
- A message without a rule makes the mock panic with the type name and the
  rule to add. The caller sees `ActorStopped(Panic(..))`; `received()` lists it as
  `Outcome::Unexpected` and `assert_no_unexpected()` fails the test.
- `expect::<M>(timeout)` returns the oldest not-yet-expected `M` (other types
  stay queued); `expect_no_message(window)`, `count::<M>()`, `take_all::<M>()`,
  `received()` and `settle()` (wait until everything sent before is handled;
  5 s bound, `settle_within(d)` to choose it) read the same log.
- A message that gets a reply is logged `Outcome::InFlight` on arrival and
  becomes `Handled` (or `Discarded`) only when the mock's handler returns the
  reply, after its reply closure and any `delay`. The framework sends the
  reply right after; a handler cannot observe that send, so `expect` can
  return a moment before the reply reaches the caller's channel (awaiting the
  caller's request still gets it). `expect_no_message` counts in-flight
  messages; a reply closure that panics logs its message as `Crashed`.
- The log keeps one entry per message for the mock's lifetime, so it suits
  unit tests rather than soak runs with millions of messages.

`MockRef` dereferences to `ActorRef<Mock>`. Its own methods win over protocol
methods with the same name; call those through `actor_ref()` or the `XRef`.

A field typed as a concrete `ActorRef<B>` cannot hold a mock: substituting the
handler behind a concrete reference needs a core hook (an envelope downcast)
that phase A does not add. Declare dependencies as protocol references, as
Ethlambda does (`P2PToBlockChainRef`, `RpcToP2PRef`), or as `Recipient<M>`.
Plain trait fakes (`impl RatesProtocol for FixedRates`) still work and need no
runtime; a mock adds the message log, strictness and faults.

### Probe a recipient

`Probe<M>` is a `Recipient<M>` that records what it is sent, without an actor.
`Probe::replying(|msg| reply)` also answers requests; a plain probe logs a
request as unexpected and drops its reply. `close()` makes it behave like a
stopped recipient: once it returns, no further send or request is logged.

There is no actor behind a probe: the reply closure runs on the requester's
thread, inside its request, and a panic in it unwinds into the requester
(the actor under test). Concurrent requests run the closure one at a time,
so keep it short. It may call `close()` or send to the probe; a request to
the same probe from inside its own closure panics instead of deadlocking. A
probe logs a request `Handled` only once the reply is in the requester's
channel.

### Harness helpers

These helpers fail with a report instead of hanging the test:

| Helper | Fails when |
|---|---|
| `settle(&actor, fence, timeout)` | a cheap request does not return, so earlier sends from this caller are not all handled |
| `exit_within(&handle, deadline)` / `join_within(&actor, deadline)` | the actor has not exited, naming it |
| `wait_until(timeout, "what", condition)` | a condition never holds |
| `wait_for_generation(&child, after, deadline)` | a supervised child does not reach a new generation, printing its snapshot |
| `shutdown_supervisor(&supervisor, deadline)` | the supervisor report is incomplete, listing its children |
| `shutdown_system(&system, deadline)` / `TestSystem::shutdown` | the `ActorSystem` report is incomplete, listing unfinished groups, actors, exits and failures |
| `in_actor(&actor, \|state, ctx\| ..)` | the actor is gone or the closure panicked; otherwise runs the closure in one of the actor's turns (after earlier sends) and returns its result, so a test can reach `ctx` (start a job, defer a reply) or inspect state without an ad hoc message or a `cfg(test)` bypass. It runs only after `started()` returned: work in `started()` a test must skip belongs in the constructor or a message |

`TestSystem::new()` owns an `ActorSystem` with a two-worker async group named
`test`, and starts actors and mocks on it. The `threads` module has the
blocking versions of `settle`, `exit_within`, `join_within`, `wait_until` and
`in_actor`.
There is no generic "actor is idle" check: an actor's mailbox can receive from
any sender at any time, so `settle` fences the senders the test controls.
Every async wait in `tasks` (`expect`, `expect_no_message`, `wait_until`,
`exit_within` and the rest) is bounded by the Tokio timer, so the helpers also
work under `#[tokio::test(start_paused = true)]`. Failure messages name the
test line that called the helper (`called at file:line`); the async helpers
capture it when called, since `#[track_caller]` does not reach into an
`async fn`.

### Fault injection

| Fault | How |
|---|---|
| Send an exit signal with a chosen reason | `faults::exit(&handle, reason)` is `ChildHandle::exit(reason)` (see [Exit signals](#exit-signals)): `Normal` is ignored by a non-trapping target; `Kill` ends the target `Killed`, trapping or not; any other reason (`Panic(msg)` included) ends a non-trapping target with exactly that reason, without `stopped()`, and reaches a trapping one as an `Exit`. For an untrappable stop that runs `stopped()`, use `ChildHandle::stop`/`stop_with_reason`. An already-exited target is left alone. Never blocks the caller, even on a `current_thread` runtime. |
| Crash a supervised child and get its replacement | `tasks::restart_child(&child, reason, deadline)`: the returned instance always replaces the one it targeted, even if the child was already restarting; `deadline` bounds the whole call |
| Slow dependency / forced request timeout | `Mock::delay::<M>(d)` with a caller timeout shorter than `d` (`with_request_timeout`, `#[protocol(timeout = ..)]`) |
| Lost message | `Mock::discard::<M>()` (fire-and-forget messages: a request returning `Response<()>` still gets `()`; to lose a request, `delay` it past the caller's timeout) |
| Crashed dependency | `Mock::crash_on::<M>(reason)` |
| Stopped recipient | `Probe::close()` |

Stopping is cooperative: a running handler finishes first, and blocking code is
never preempted. There is no controlled scheduler yet: tests run on the real
Tokio or OS scheduler, so they observe one interleaving per run. Ordering
exploration and replay (roadmap ranks 20-22) come later in the same crate.

The [test kit example](../examples/capabilities/src/bin/test_kit.rs) runs a mock,
a probe and each fault with assertions in both modes.

### Check request histories for linearizability

An actor serialises its requests, so whatever concurrent callers observe must be
explainable by *some* sequential order that respects real time: each request
takes effect at one instant between its call and its reply.
`spawned_test::linearizability` checks a recorded history against a sequential
model you write (Porcupine's algorithm: memoised backtracking, per-key
partitions). It needs no feature and works in both modes.

```rust
use spawned_test::linearizability::{assert_linearizable, partition_by_key, Model, Operation, Recorder};

struct Kv; // Put returns the previous value, Get reads.
impl Model for Kv {
    type State = Option<u64>;
    type Input = KvOp;
    type Output = Option<u64>;
    fn init(&self) -> Option<u64> { None }
    fn step(&self, s: &Option<u64>, op: &KvOp, out: Option<&Option<u64>>) -> Option<Option<u64>> {
        let (next, produced) = match op {
            KvOp::Put(_, v) => (Some(*v), *s),
            KvOp::Get(_) => (*s, *s),
        };
        // `out` is None for a request whose reply never arrived.
        out.is_none_or(|seen| *seen == produced).then_some(next)
    }
    fn partition(&self, h: &[Operation<KvOp, Option<u64>>]) -> Vec<Vec<usize>> {
        partition_by_key(h, |op| op.key()) // keys are independent
    }
}

let recorder = Recorder::new();
// In each client thread or task:
let call = recorder.call(client_id, op);
if let Ok(reply) = actor.request_with_timeout(op, timeout).await {
    call.complete(reply);
} // dropping `call` records it as pending: it may or may not have taken effect
// Afterwards:
assert_linearizable(&Kv, &recorder.take());
```

- A timed-out or cancelled request is *pending*: the checker may place it
  anywhere after its call or leave it out. Record a request whose send was
  rejected as completed with an output your model treats as a no-op.
- The verdict is `Linearizable`, `Illegal` or `Unknown` (the time or state
  budget in `CheckOptions` ran out; never a pass).
- On failure, `Report::explain` prints the longest linearizable prefix per
  partition with the model state after each step, the operation that cannot
  be placed and why each candidate was refused. `Report::to_html` renders a
  Porcupine-style timeline; `assert_linearizable` writes it to
  `$SPAWNED_LINEARIZABILITY_DIR` when set.
- Partition whenever you can: thousands of operations split by key check in
  milliseconds, while a single heavily concurrent partition is exponential in
  the worst case (a corrupted read deep in a 1,000-operation single-key
  history with 16 overlapping clients takes seconds in release builds).

---

## Further reading

- [Semantics contract](design/semantics.md): every guarantee in one place (ordering, delivery, request outcomes, exit propagation, restarts, shutdown, timers, panics), each linked to what checks it, and what is not guaranteed.
- [Execution groups and supervision design contract](design/execution-groups-supervision.md): the fuller boundaries of the v0.7 APIs.
- [Testing](TESTING.md): scenario tests, failure windows, the TLA+ model and trace validation.
- [Roadmap](../ROADMAP.md): what is planned and why.
- [Legacy](../legacy/README.md): archived backend research, benchmark reports, reviews and earlier designs.
