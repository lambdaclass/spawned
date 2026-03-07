# Spawned

An actor framework for Rust, inspired by Erlang/OTP.

[![Crates.io](https://img.shields.io/crates/v/spawned-concurrency.svg)](https://crates.io/crates/spawned-concurrency)
[![docs.rs](https://img.shields.io/docsrs/spawned-concurrency)](https://docs.rs/spawned-concurrency)
[![CI](https://github.com/lambdaclass/spawned/actions/workflows/ci.yml/badge.svg)](https://github.com/lambdaclass/spawned/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## Quick Example

Define a protocol, implement it on an actor, and call it:

```rust
// protocols.rs — define the message interface
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

// server.rs — implement the actor
use std::collections::HashMap;
use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_concurrency::actor;
use crate::protocols::name_server_protocol::{Add, Find};
use crate::protocols::{FindResult, NameServerProtocol};

pub struct NameServer {
    inner: HashMap<String, String>,
}

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

// main.rs — use it
use protocols::{FindResult, NameServerProtocol};
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        let ns = NameServer::new().start();

        ns.add("Joe".into(), "At Home".into()).await.unwrap();

        let result = ns.find("Joe".into()).await.unwrap();
        assert_eq!(result, FindResult::Found { value: "At Home".to_string() });
    })
}
```

No message enums, no manual dispatch — just define a trait, implement the handlers, and call methods directly on the actor reference.

## Features

- **Protocol macros** — `#[protocol]` generates message types, blanket impls, and type-erased refs from a trait definition
- **Actor macros** — `#[actor]` generates `impl Actor` and `Handler<M>` impls from annotated methods
- **Dual execution modes** — async/tokio (`tasks`) or blocking OS threads (`threads`) with the same API
- **Type-erased protocol refs** — `XRef` types let actors communicate through protocol interfaces without knowing concrete types
- **Actor registry** — global name-based registry for discovering actors at runtime
- **Timers** — `send_after` and `send_interval` for delayed and periodic messages
- **Signal handling** — `send_message_on` to deliver messages on cancellation token signals
- **Backend selection** — async runtime, blocking thread pool, or dedicated OS thread (tasks mode)

## How It Works

**Protocols** define the message interface for an actor. `#[protocol]` on a trait generates one message struct per method, a type-erased reference type (`XRef`), and blanket implementations that let any `ActorRef<A>` call the protocol methods directly — as long as `A` implements the right handlers.

The return type on each protocol method determines the message kind:
- `Response<T>` — request (both modes), caller uses `.await.unwrap()` (tasks) or `.unwrap()` (threads)
- `Result<(), ActorError>` — fire-and-forget send (both modes), returns the send result
- No return / `-> ()` — fire-and-forget send (both modes), discards the send result

**Actors** implement message handlers with `#[actor]`. Each handler method is annotated with `#[request_handler]`, `#[send_handler]`, or `#[handler]` and receives a single message struct plus a `Context`. The macro generates the `Actor` trait impl and one `Handler<M>` impl per method.

**Using an actor** is just calling trait methods on its `ActorRef`. Because `#[protocol]` generates `impl NameServerProtocol for ActorRef<NameServer>`, you write `ns.add(...)` and `ns.find(...)` — the macro handles message construction and mailbox dispatch behind the scenes.

## Examples

| Example | Mode | Description |
|---------|------|-------------|
| [`name_server`](examples/name_server) | tasks | Key-value store — the classic Erlang name server |
| [`bank`](examples/bank) | tasks | Bank account — deposit, withdraw, balance with error handling |
| [`bank_threads`](examples/bank_threads) | threads | Bank account — same API, thread-based |
| [`chat_room`](examples/chat_room) | tasks | Multi-actor chat — rooms and users via type-erased protocol refs |
| [`chat_room_threads`](examples/chat_room_threads) | threads | Multi-actor chat — thread-based |
| [`ping_pong`](examples/ping_pong) | tasks | Producer/consumer — bidirectional messaging between actors |
| [`ping_pong_threads`](examples/ping_pong_threads) | threads | Producer/consumer — thread-based |
| [`service_discovery`](examples/service_discovery) | tasks | Registry — register and discover actors by name |
| [`signal_test`](examples/signal_test) | tasks | Timers — `send_interval` and `send_message_on` for cancellation |
| [`signal_test_threads`](examples/signal_test_threads) | threads | Timers — thread-based |
| [`updater`](examples/updater) | tasks | Periodic HTTP — recurrent timer-driven requests |
| [`updater_threads`](examples/updater_threads) | threads | Periodic HTTP — thread-based |
| [`blocking_genserver`](examples/blocking_genserver) | tasks | Backend comparison — async vs blocking vs thread isolation |
| [`busy_genserver_warning`](examples/busy_genserver_warning) | tasks | Blocking detection — runtime warning for slow handlers |

## Erlang to Spawned

For developers familiar with Erlang/OTP:

| Erlang/OTP | Spawned | Description |
|------------|---------|-------------|
| Module exports (client API) | `#[protocol]` trait | Define the public message interface |
| `-behaviour(gen_server)` | `#[actor]` | Declare a module as an actor implementation |
| `handle_call/3` | `#[request_handler]` | Handler for sync requests |
| `handle_cast/2` | `#[send_handler]` | Handler for fire-and-forget messages |
| `init/1` | `#[started]` | Initialization callback |
| `terminate/2` | `#[stopped]` | Cleanup callback |
| `gen_server:call/2` | `ns.find(...)` | Direct method call on ActorRef |
| `gen_server:cast/2` | `ns.notify(...)` | Direct method call (send variant) |
| `Pid` | `ActorRef<T>` | Handle to a running actor |
| `start_link/0` | `actor.start()` | Start the actor |
| `register/2` | `registry::register(name, ref)` | Register an actor by name |
| `whereis/1` | `registry::whereis(name)` | Look up an actor by name |

**Erlang:**
```erlang
-module(name_server).
-behaviour(gen_server).

handle_call({find, Key}, _From, State) ->
    Reply = maps:get(Key, State, not_found),
    {reply, Reply, State}.
```

**Spawned:**
```rust
#[request_handler]
async fn handle_find(&mut self, msg: Find, _ctx: &Context<Self>) -> FindResult {
    match self.inner.get(&msg.key) {
        Some(value) => FindResult::Found { value: value.clone() },
        None => FindResult::NotFound,
    }
}
```

## Two Execution Modes

- **`tasks`** — async/await on a tokio runtime. Use `spawned_concurrency::tasks` and `spawned_rt::tasks`. Supports `Backend::Async`, `Backend::Blocking`, and `Backend::Thread`.
- **`threads`** — blocking, no async runtime needed. Use `spawned_concurrency::threads` and `spawned_rt::threads`. Each actor runs on a native OS thread.

Both provide the same `Actor`, `Handler<M>`, `ActorRef<A>`, and `Context<A>` types. Switching between them requires changing imports and adding/removing `async`/`.await`.

## Project Structure

```
spawned/
├── concurrency/   # Main library: Actor, Handler, protocols, registry
│   └── src/
│       ├── tasks/     # Async implementation (tokio)
│       └── threads/   # Sync implementation (OS threads)
├── macros/        # Proc macros (#[protocol], #[actor]) — re-exported by concurrency
├── rt/            # Runtime abstraction (wraps tokio, provides CancellationToken)
└── examples/      # 14 usage examples
```

Users depend only on `spawned-concurrency` (which re-exports the macros) and `spawned-rt`.

## Rationale

Inspired by Erlang/OTP, the goal of `spawned` is to keep concurrency logic separated from business logic. As Joe Armstrong wrote in *Programming Erlang*:

> The callback had no code for concurrency, no spawn, no send, no receive, and no register. It is pure sequential code—nothing else. *This means we can write client-server models without understanding anything about the underlying concurrency models.*

Protocols make this separation explicit: the trait defines *what* an actor does, the `#[actor]` impl defines *how*, and the framework handles message routing, mailboxes, and lifecycle. Your business logic is plain Rust methods — no channels, no `match` on enums, no concurrency primitives.

## Roadmap

- **Supervision trees** — monitor, restart, and manage actor lifecycles with Erlang-style supervision strategies
- **Observability and tracing** — built-in instrumentation for actor mailboxes, message latency, and lifecycle events
- **Custom runtime** — replace tokio with a purpose-built runtime tailored for actor workloads
- **Preemptive scheduling** — explore preemptive actor scheduling to prevent starvation from long-running handlers
- **Virtual actors** — evaluate location-transparent, auto-activated actors inspired by [Orleans](https://learn.microsoft.com/en-us/dotnet/orleans/)
- **Deterministic runtime** — reproducible execution for testing, inspired by [commonware](https://commonware.xyz)
- **Landing page** — project website with guides, API reference, and interactive examples

## Inspiration

- [Erlang/OTP](https://www.erlang.org/)
- [Commonware](https://commonware.xyz)
- [Actix](https://actix.rs/)
- [Orleans](https://learn.microsoft.com/en-us/dotnet/orleans/)
- [Ractor](https://slawlor.github.io/ractor/)
- [Tokio](https://tokio.rs/)
- [Actors with Tokio](https://ryhl.io/blog/actors-with-tokio/)
- [Vale](https://vale.dev/)
- [Gleam](https://gleam.run/)
