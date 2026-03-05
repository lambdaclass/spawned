# Migrating to Spawned 0.5

Spawned 0.5 replaces the enum-based `Actor` trait with protocol macros. The new API eliminates message enum boilerplate and manual dispatch — you define a trait, implement handlers, and call methods directly on actor references.

## Concept Mapping

| 0.4 (old) | 0.5 (new) | Notes |
|-----------|-----------|-------|
| `type Request = MyEnum` | `#[protocol]` trait | Protocol trait methods become message structs |
| `type Message = MyEnum` | `#[protocol]` trait (no return) | Send-only methods |
| `type Reply = MyEnum` | Method return types | `Response<T>` or `Result<T, ActorError>` |
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
| `actor_ref.send(enum)` | `actor_ref.method(args)` | Direct method call |
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
use spawned_concurrency::tasks::Response;
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
| *(n/a)* | `spawned_concurrency::tasks::{Context, Handler, Response}` |
| *(n/a)* | `spawned_concurrency::{actor, protocol}` |
| *(n/a)* | `spawned_concurrency::registry` |

## Escape Hatches

If the macros don't fit your use case:

- **`messages!` / `request_messages!` / `send_messages!`** — Declarative macros for defining `Message` structs manually, without `#[protocol]`.
- **Manual `Handler<M>` impls** — Skip `#[actor]` and implement `Handler<M>` for each message type by hand.
- **`Recipient<M>`** (`Arc<dyn Receiver<M>>`) — Type-erased reference for a single message type, available via `ctx.recipient::<M>()`.
