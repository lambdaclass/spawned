# Migrating to Spawned 0.5

Spawned 0.5 replaces the enum-based `Actor` trait with protocol macros. The new API eliminates message enum boilerplate and manual dispatch — you define a trait, implement handlers, and call methods directly on actor references.

## Concept Mapping

| 0.4 (old) | 0.5 (new) | Notes |
|-----------|-----------|-------|
| `type Request = MyEnum` | `#[protocol]` trait | Protocol trait methods become message structs |
| `type Message = MyEnum` | `#[protocol]` trait (no return) | Send-only methods |
| `type Reply = MyEnum` | Method return types | `Response<T>` (request) or `Result<(), ActorError>` (send) |
| `type Error = E` | *(removed)* | Errors returned per-handler, not per-actor |
| `handle_request()` | `#[request_handler]` methods | One handler per message, no `match` |
| `handle_message()` | `#[send_handler]` methods | One handler per message |
| `init()` | `#[started]` | Lifecycle hook |
| `teardown()` | `#[stopped]` | Lifecycle hook |
| `RequestResponse::Reply(val)` | Return the value directly | Handler return type = reply |
| `RequestResponse::Stop(val)` | `ctx.stop()` + return value | Explicit stop via context |
| `MessageResponse::NoReply` | *(implicit)* | Send handlers don't reply |
| `Actor::start()` | `ActorStart::start()` | Same call, different trait |
| `actor_ref.request(enum)` | `actor_ref.method(args)` | Direct method call |
| `actor_ref.send(enum)` | `actor_ref.method(args)` | Direct method call, sync for sends |
| `handle.cast(msg).await` | `actor_ref.method(args)` | Send is now sync — no `.await` needed |
| `send_after(dur, handle, msg)` | `send_after(dur, ctx, msg)` | Takes `Context<A>`, not `ActorRef<A>` |
| `send_interval(dur, handle, msg)` | `send_interval(dur, ctx, msg)` | Takes `Context<A>`, not `ActorRef<A>` |
| `GenServerHandle<A>` | `ActorRef<A>` | Renamed |
| `Unused` | *(removed)* | No placeholder types needed |

## Before/After: Messages

**0.4** — Hand-written enums for requests and replies:

```rust
#[derive(Debug, Clone)]
pub enum NameServerInMessage {
    Add { key: String, value: String },
    Find { key: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum NameServerOutMessage {
    Ok,
    Found { value: String },
    NotFound,
    Error,
}
```

**0.5** — A protocol trait; message structs are generated:

```rust
use spawned_concurrency::Response;
use spawned_concurrency::protocol;

#[derive(Debug, Clone, PartialEq)]
pub enum FindResult {
    Found { value: String },
    NotFound,
}

#[protocol]
pub trait NameServerProtocol: Send + Sync {
    fn add(&self, key: String, value: String) -> Response<()>;
    fn find(&self, key: String) -> Response<FindResult>;
}
// Generates: name_server_protocol::Add, name_server_protocol::Find
```

`Response<T>` works in both execution modes — use `.await.unwrap()` in tasks mode or `.unwrap()` directly in threads mode. A single protocol definition works for both runtimes.

### Generated module naming

`#[protocol]` generates a submodule containing the message structs. The naming rules:

- **Module name**: trait name converted to `snake_case` — e.g., `trait FooBarProtocol` → `mod foo_bar_protocol`
- **Message structs**: method name converted to `PascalCase` — e.g., `fn do_thing(...)` → `DoThing`
- **Type-erased ref**: strips `Protocol` suffix → `{Base}Ref` — e.g., `FooBarProtocol` → `FooBarRef = Arc<dyn FooBarProtocol>`

Import message structs from the generated module:

```rust
use crate::protocols::foo_bar_protocol::{DoThing, GetStatus};
```

### Mixed send + request protocols

A single protocol can have both request (awaitable) and send (fire-and-forget) methods. The return type determines the kind:

```rust
#[protocol]
pub trait WorkerProtocol: Send + Sync {
    /// Request — caller awaits the reply
    fn get_status(&self) -> Response<Status>;

    /// Send — fire-and-forget, no reply
    fn notify(&self, event: Event);
}
```

On the actor side, annotate each handler to match:

```rust
#[actor(protocol = WorkerProtocol)]
impl Worker {
    #[request_handler]
    async fn handle_get_status(&mut self, _msg: GetStatus, _ctx: &Context<Self>) -> Status {
        self.status.clone()
    }

    #[send_handler]
    async fn handle_notify(&mut self, msg: Notify, _ctx: &Context<Self>) {
        tracing::info!("event: {:?}", msg.event);
    }
}
```

## Before/After: Actor Implementation

**0.4** — Single `handle_request` with `match`:

```rust
use spawned_concurrency::tasks::{Actor, ActorRef, RequestResponse};

impl Actor for NameServer {
    type Request = InMessage;
    type Message = Unused;
    type Reply = OutMessage;
    type Error = std::fmt::Error;

    async fn handle_request(
        &mut self,
        message: Self::Request,
        _handle: &ActorRef<Self>,
    ) -> RequestResponse<Self> {
        match message {
            InMessage::Add { key, value } => {
                self.inner.insert(key, value);
                RequestResponse::Reply(OutMessage::Ok)
            }
            InMessage::Find { key } => match self.inner.get(&key) {
                Some(value) => RequestResponse::Reply(OutMessage::Found { value: value.clone() }),
                None => RequestResponse::Reply(OutMessage::NotFound),
            },
        }
    }
}
```

**0.5** — One handler per message, return values directly:

```rust
use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_concurrency::actor;

#[actor(protocol = NameServerProtocol)]
impl NameServer {
    pub fn new() -> Self {
        NameServer { inner: HashMap::new() }
    }

    #[request_handler]
    async fn handle_add(&mut self, msg: Add, _ctx: &Context<Self>) {
        self.inner.insert(msg.key, msg.value);
    }

    #[request_handler]
    async fn handle_find(&mut self, msg: Find, _ctx: &Context<Self>) -> FindResult {
        match self.inner.get(&msg.key) {
            Some(value) => FindResult::Found { value: value.clone() },
            None => FindResult::NotFound,
        }
    }
}
```

### Handler attributes

The `#[actor]` macro supports three handler attributes:

| Attribute | Use for |
|-----------|---------|
| `#[request_handler]` | Messages that expect a reply (`Response<T>`) |
| `#[send_handler]` | Fire-and-forget messages (no return or `-> ()`) |
| `#[handler]` | Generic — works for either kind |

All three generate the same `impl Handler<M>` code. The distinction is semantic: use `#[request_handler]` and `#[send_handler]` to document intent, or `#[handler]` when you don't want to commit to a specific kind.

Handler signature: `fn name(&mut self, msg: MessageType, ctx: &Context<Self>) -> ReturnType`

## Before/After: Calling an Actor

**0.4** — Static methods wrapping `actor_ref.request(enum)`:

```rust
let mut ns = NameServer::new().start();

// Static method that constructs enum + calls request
let result = NameServer::add(&mut ns, "Joe".into(), "At Home".into()).await;
assert_eq!(result, NameServerOutMessage::Ok);

let result = NameServer::find(&mut ns, "Joe".into()).await;
assert_eq!(result, NameServerOutMessage::Found { value: "At Home".into() });
```

**0.5** — Call protocol methods directly on ActorRef:

```rust
let ns = NameServer::new().start();

// Method call on ActorRef — no enum, no static wrapper
ns.add("Joe".into(), "At Home".into()).await.unwrap();

let result = ns.find("Joe".into()).await.unwrap();
assert_eq!(result, FindResult::Found { value: "At Home".into() });
```

### Send methods are now synchronous

In 0.4, `handle.cast(msg).await` was async. In 0.5, send-only protocol methods return `Result<(), ActorError>` synchronously — no `.await` needed:

```rust
// 0.4:
handle.cast(CastMessage::Notify(data)).await;

// 0.5:
actor_ref.notify(data); // returns Result<(), ActorError>, no .await
```

This means callers that only send messages no longer need `&mut self` or `async`:

```rust
// 0.4:
pub struct MyService { handle: GenServerHandle<MyServer> }
impl MyService {
    pub async fn do_thing(&mut self, data: String) {
        let _ = self.handle.cast(MyMsg::DoThing(data)).await;
    }
}

// 0.5:
pub struct MyService { handle: ActorRef<MyServer> }
impl MyService {
    pub fn do_thing(&self, data: String) {
        let _ = self.handle.do_thing(data);
    }
}
```

## Before/After: Timers

Timer functions (`send_after`, `send_interval`) now take `Context<A>` instead of `ActorRef<A>`.

**0.4:**
```rust
use spawned_concurrency::tasks::send_after;

// Inside a handler — clone the handle
send_after(duration, handle.clone(), CastMessage::Tick);
```

**0.5:**
```rust
use spawned_concurrency::tasks::send_after;

// Inside a handler — ctx is available directly
send_after(duration, ctx.clone(), Tick);

// Outside a handler — convert ActorRef to Context
send_after(duration, actor_ref.context(), Tick);
```

`ActorRef::context()` converts an `ActorRef<A>` to a `Context<A>`. Use it when scheduling timers from outside a handler (e.g., in a `spawn()` block or from `main()`).

### Self-rescheduling tick pattern

A common pattern is an actor that reschedules itself with variable delays:

**0.4:**
```rust
CastMessage::Tick => {
    self.do_work();
    let next_delay = self.compute_delay();
    send_after(next_delay, handle.clone(), CastMessage::Tick);
    CastResponse::NoReply
}
```

**0.5:**
```rust
#[send_handler]
async fn handle_tick(&mut self, _msg: Tick, ctx: &Context<Self>) {
    self.do_work();
    let next_delay = self.compute_delay();
    send_after(next_delay, ctx.clone(), Tick);
}
```

## Before/After: Lifecycle Hooks

**0.4:**
```rust
impl Actor for MyActor {
    // ...
    async fn init(&mut self, _handle: &ActorRef<Self>) { /* setup */ }
    async fn teardown(&mut self) { /* cleanup */ }
}
```

**0.5:**
```rust
#[actor]
impl MyActor {
    #[started]
    async fn on_start(&mut self, _ctx: &Context<Self>) { /* setup */ }

    #[stopped]
    async fn on_stop(&mut self, _ctx: &Context<Self>) { /* cleanup */ }
}
```

**Panic behavior:** If `#[started]` panics, the panic is caught, logged via `tracing::error!`, and the actor exits immediately — subsequent sends will receive `ActorError::ActorStopped`. The `#[stopped]` hook is *not* called in this case. This applies to both tasks and threads modes.

## Before/After: Stopping an Actor

**0.4** — Return `RequestResponse::Stop`:
```rust
RequestResponse::Stop(MyReply::Goodbye)
```

**0.5** — Call `ctx.stop()` and return normally:
```rust
#[request_handler]
async fn handle_shutdown(&mut self, _msg: Shutdown, ctx: &Context<Self>) {
    ctx.stop();
}
```

## New: Type-Erased Protocol References

Each `#[protocol]` trait generates an `XRef` type alias — e.g., `NameServerRef = Arc<dyn NameServerProtocol>`. This lets actors hold references to protocol implementors without knowing the concrete type:

```rust
pub struct Logger {
    target: NameServerRef, // any actor implementing NameServerProtocol
}
```

Convert an `ActorRef<A>` to an `XRef` via the generated `ToXRef` trait:

```rust
let ns: ActorRef<NameServer> = NameServer::new().start();
let ns_ref: NameServerRef = ns.to_name_server_ref();
```

## New: Actor Registry

A global, name-keyed registry for discovering actors at runtime:

```rust
use spawned_concurrency::registry;

// Register
registry::register("main_server", ns.clone()).unwrap();

// Look up
let found: Option<ActorRef<NameServer>> = registry::whereis("main_server");

// List all names
let names: Vec<String> = registry::registered();

// Unregister
registry::unregister("main_server");
```

## Import Changes

| 0.4 | 0.5 |
|-----|-----|
| `spawned_concurrency::tasks::Actor` | `spawned_concurrency::tasks::{Actor, ActorStart}` |
| `spawned_concurrency::tasks::ActorRef` | *(same)* |
| `spawned_concurrency::messages::Unused` | *(removed)* |
| `spawned_concurrency::tasks::RequestResponse` | *(removed — return values directly)* |
| `spawned_concurrency::tasks::MessageResponse` | *(removed)* |
| *(n/a)* | `spawned_concurrency::tasks::{Context, Handler}` |
| *(n/a)* | `spawned_concurrency::Response` |
| *(n/a)* | `spawned_concurrency::{actor, protocol}` |
| *(n/a)* | `spawned_concurrency::registry` |

## spawned-rt Changes

`spawned-rt` has **no breaking API changes** in 0.5. Both `spawned_rt::tasks` and `spawned_rt::threads` retain the same public exports (`spawn`, `sleep`, `CancellationToken`, `mpsc`, `oneshot`, etc.). Update the version in your `Cargo.toml` but no code changes are needed for `spawned-rt` imports.

## Escape Hatches

If the macros don't fit your use case:

- **Manual `Message` impls** — Define message structs and implement `Message` by hand instead of using `#[protocol]`.
- **Manual `Handler<M>` impls** — Skip `#[actor]` and implement `Handler<M>` for each message type by hand.
- **`Recipient<M>`** (`Arc<dyn Receiver<M>>`) — Type-erased reference for a single message type, available via `ctx.recipient::<M>()`.
