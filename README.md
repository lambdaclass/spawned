# Spawned

An actor framework for Rust, inspired by Erlang/OTP, built so that its guarantees
are verified once and inherited by every application, and so that concurrency
tests replay exactly.

[![Crates.io](https://img.shields.io/crates/v/spawned-concurrency.svg)](https://crates.io/crates/spawned-concurrency)
[![docs.rs](https://img.shields.io/docsrs/spawned-concurrency)](https://docs.rs/spawned-concurrency)
[![CI](https://github.com/lambdaclass/spawned/actions/workflows/ci.yml/badge.svg)](https://github.com/lambdaclass/spawned/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## Why Spawned

More and more code is written by LLMs and reviewed by people who did not write it. Concurrency is where that goes wrong: each call site looks plausible, and the bugs live in how the call sites interact. Spawned matters for two reasons.

**The guarantees are verified once, centrally.** Nobody can model-check and fuzz every hand-rolled Tokio pattern in an application. One framework can be. Message ordering, request outcomes, links, monitors, exit reasons, supervision and shutdown are written down in a [semantics contract](docs/design/semantics.md). They are checked by:

- TLA+ models (with Apalache proofs of the safety invariants);
- trace validation of the real code against those models;
- Loom;
- a deterministic simulator;
- a coverage-guided scenario fuzzer;
- failpoints, sanitizers, Kani and mutation testing;
- the ethrex and ethlambda test suites in CI.

Every application built on Spawned inherits that work instead of redoing it. [TESTING.md](docs/TESTING.md) describes each layer.

**Tests are deterministic.** Under `--cfg spawned_sim`, async tests written with `#[spawned_macros::sim_test]` run on a seeded, single-threaded simulator with virtual time, across many schedules. A failure prints its seed and a decision tape. The tape replays the exact interleaving and is shrunk to a minimal one. With raw Tokio, the same race shows up as a flaky CI run that nobody can reproduce. The simulator covers `tasks` mode. Code that calls `tokio::spawn` or `tokio::time` directly, instead of going through Spawned, cannot run on it. See [what the simulator does not model](docs/TESTING.md#seeds-and-decision-tapes-under---cfg-spawned_sim).

## Project Status

**Spawned is actively maintained and used in production.** It powers the concurrency layer of [ethrex](https://github.com/lambdaclass/ethrex) (Ethereum L1/L2 execution client) and [ethlambda](https://github.com/lambdaclass/ethlambda) (Lean Ethereum consensus client), both developed by [LambdaClass](https://github.com/lambdaclass).

The core API is stable, so periods without commits are expected (they mean things are working, not that the project is abandoned). New features land as those projects need them (see the [roadmap](ROADMAP.md)). Bug reports, questions, and pull requests are welcome and get answered.

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

For every feature (timers, type erasure, registry, backends, links, supervision, metrics and more), see the **[Guide](docs/GUIDE.md)**.

## Documentation

| Document | Contents |
|---|---|
| [Guide](docs/GUIDE.md) | The user guide: lifecycle, messaging, execution modes and backends, timers, registry, links, execution groups, supervision and metrics |
| [Examples](examples/README.md) | Every feature mapped to a runnable example |
| [Migration](docs/MIGRATION.md) | Upgrade notes per version |
| [Changelog](CHANGELOG.md) | Release history |
| [Roadmap](ROADMAP.md) | Planned work and the ranked backlog |
| [Semantics](docs/design/semantics.md) | Every guarantee Spawned gives (messages, requests, lifecycle, links, supervision, shutdown, timers, panics), what checks it, and what is not guaranteed |
| [Testing](docs/TESTING.md) | Test suites, verification layers and the formal model |
| [Design](docs/design/README.md) | Active design contracts |
| [Research](docs/research/README.md) | Research notes behind roadmap items: virtual actors, systematic testing, Ethereum client testing, request semantics |
| [Benchmarks](benchmarks/README.md) | Running the benchmark harness |
| [Legacy](legacy/README.md) | Archived designs, reviews, research notes and raw experiment data (not maintained) |

## Features

- **Verified semantics** — every guarantee is written down in the [semantics contract](docs/design/semantics.md) and checked by TLA+ models, trace validation, Loom, the simulator, fuzzing and the client test suites
- **Deterministic simulation testing** — `#[spawned_macros::sim_test]` runs your async actor tests over many seeded schedules with virtual time; a failure replays exactly from its seed or decision tape
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

The [examples index](examples/README.md) maps every user-facing feature to a
runnable example, in `tasks` and `threads` mode, with run commands. Run one with
`cargo run --locked -p <package>` from the repository root, for example
`cargo run --locked -p name_server`.

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
| `start_link/0` | `actor.start_linked(&ctx)` | Start the actor |
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

## Choosing a backend

Use Tokio (`Backend::Async`) for general async applications and dynamic actor
populations. Use `Backend::Blocking` for persistent blocking work, or
`Backend::Thread` when an actor needs a dedicated thread. Each blocking actor
occupies a worker until it stops; each dedicated actor has its own root thread
and an isolated runtime with one async helper worker. See
[execution modes and backends](docs/GUIDE.md#execution-modes-and-backends) for
resource sizing. Owned parked pinned groups and fixed native actor pools are
available in v0.7.0. Spinning remains private research; Tokio stays the async default.
A [thread-only example](examples/thread_only/README.md) also builds without Tokio.

## Project Structure

```
spawned/
├── concurrency/   # Main library: Actor, Handler, protocols, registry
│   └── src/
│       ├── tasks/     # Async implementation (tokio)
│       └── threads/   # Sync implementation (OS threads)
├── macros/        # Proc macros (#[protocol], #[actor]) — re-exported by concurrency
├── rt/            # Runtime abstraction (wraps tokio, provides CancellationToken)
├── examples/      # Usage examples and capability guides
├── docs/          # Guide, migration, testing, design contracts
├── benchmarks/    # Benchmark harness
├── verification/  # TLA+ model
└── legacy/        # Archived research and history (not maintained)
```

Users depend only on `spawned-concurrency` (which re-exports the macros) and `spawned-rt`.

## Rationale

Inspired by Erlang/OTP, the goal of `spawned` is to keep concurrency logic separated from business logic. As Joe Armstrong wrote in *Programming Erlang*:

> The callback had no code for concurrency, no spawn, no send, no receive, and no register. It is pure sequential code—nothing else. *This means we can write client-server models without understanding anything about the underlying concurrency models.*

Protocols make this separation explicit: the trait defines *what* an actor does, the `#[actor]` impl defines *how*, and the framework handles message routing, mailboxes, and lifecycle. Your business logic is plain Rust methods — no channels, no `match` on enums, no concurrency primitives.

Links, trapped exits and atomic linked startup are available in v0.6.0
([links](docs/GUIDE.md#links-and-trapped-exits)). Nested supervision with OneForOne/OneForAll/RestForOne and
owned async/native/pinned execution groups are available in v0.7.0
([execution groups and supervision](docs/GUIDE.md#execution-groups-and-supervision)).

## Roadmap

Preserve typed protocols, dynamic actors, and both async and synchronous thread
execution while improving production visibility and recovery. Next: verify the
base lifecycle and validate v0.7 in client workloads, then add deferred replies,
bounded admission and deadlines as client workloads require. See the
[roadmap](ROADMAP.md) for the phases and the full ranked backlog.

## Optional framework metrics

Enable the `metrics` Cargo feature for mailbox depth, sampled queue/handler/request
latency, request outcomes and actor lifecycle counters, with pull-based Prometheus
text export. Metrics are compiled out by default and need no handler changes. See
[metrics](docs/GUIDE.md#metrics).

## Inspiration

- [Erlang/OTP](https://www.erlang.org/)
- [Commonware](https://commonware.xyz)
- [Actix](https://actix.rs/)
- [Orleans](https://learn.microsoft.com/en-us/dotnet/orleans/)
- [Ractor](https://slawlor.github.io/ractor/)
- [Tokio](https://tokio.rs/)
- [Silver](https://github.com/gattaca-com/silver) — fixed-core pipeline and latency-oriented design research
- [Flux](https://github.com/gattaca-com/flux) — tile/spine architecture, placement and transport research
- [Actors with Tokio](https://ryhl.io/blog/actors-with-tokio/)
- [Vale](https://vale.dev/)
- [Gleam](https://gleam.run/)

## Performance

The [benchmark harness](benchmarks/README.md) measures the existing backends in
fresh processes. Microbenchmarks document limits; they are not client-level
results. Earlier reports and backend studies are archived in [legacy](legacy/README.md).

## Testing

Run `cargo test --workspace`.

### Testing your own actors deterministically

Add `spawned-macros` and `spawned-rt` as dev-dependencies and mark async tests
with `#[spawned_macros::sim_test]`. In a normal build they run
once on Tokio. Built with `RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable"`,
each one runs on the simulator over seeds `0..SPAWNED_SIM_SEEDS` (default 8)
with virtual time, so timers and restart backoff take no wall-clock time:

```rust
#[spawned_macros::sim_test]
async fn find_returns_what_add_stored() {
    let ns = NameServer::new().start();
    ns.add("Joe".into(), "At Home".into()).await.unwrap();
    assert_eq!(
        ns.find("Joe".into()).await.unwrap(),
        FindResult::Found { value: "At Home".to_string() }
    );
}
```

A failing run prints its seed and the path of a decision tape. Rerun with
`SPAWNED_SIM_SEED=n` or `SPAWNED_SIM_TAPE=path` to get exactly the same
interleaving, and use the `.min.tape` the harness writes for a shrunk one.
[TESTING.md](docs/TESTING.md#seeds-and-decision-tapes-under---cfg-spawned_sim)
covers PCT and bounded exploration and what the simulator does not model.

### How Spawned itself is verified

Spawned is verified in layers. TLA+ models of supervision, restart cohorts,
supervisor trees and system shutdown are model-checked with TLC and their
safety invariants proven inductive with Apalache; recorded traces from the
Rust code are checked against those models. A deterministic simulation
backend, Loom and ThreadSanitizer cover interleavings; failpoints and
panic-, cancellation- and leak-safety suites cover faults; Kani, mutation
testing and size budgets harden individual functions and tests; and Ethrex
and Ethlambda canaries check real nodes. See
[how spawned is verified](docs/TESTING.md#how-spawned-is-verified) for what
each layer catches and its status.
