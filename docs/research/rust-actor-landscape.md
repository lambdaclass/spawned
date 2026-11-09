# Rust actor frameworks: actix, ractor, kameo, xtra, coerce, lunatic, bastion

Research note, 2026-11-09. Background for ranks 6, 12a, 20, 26, 30 and 44 of the
[roadmap](../../ROADMAP.md#ranked-backlog). The v0.5-era comparison
([framework comparison](../../legacy/docs/design/FRAMEWORK_COMPARISON.md)) and the
API alternatives it informed ([API alternatives](../../legacy/docs/design/API_ALTERNATIVES_SUMMARY.md))
are archived; this note updates the Rust part for v0.7.0 and corrects it where
it was wrong (see [What it means for Spawned](#what-it-means-for-spawned)).
Supervision semantics across Erlang, Akka and these crates are compared in
[supervision models](supervision-models.md).

## What it is

Seven Rust crates that put stateful message handlers behind an address, compared
on what Spawned cares about: supervision, typed messaging, request/reply, links
and monitors, distribution, testing tools, verification and maintenance. Facts
come from each crate's docs.rs page, README or repository as of 2026-11-09;
release dates come from the crates.io API. Absence claims ("none found") mean we
searched the docs and repository and did not find it, not that it cannot exist.

## How it works

### actix

The oldest widely used crate. An actor implements `Handler<M>` per message type
and `Message` declares the reply type; `Recipient<M>` "allows to send one
specific message to an actor" ([docs.rs](https://docs.rs/actix/latest/actix/)).
Actors run on actix's own Arbiter/System runtime; `SyncArbiter` runs synchronous
actors on a thread pool. Supervision is in-place restart only: "A Supervisor does
not re-create their actor, it just calls the `restarting()` method", and the
message being processed during a failure is lost, the sender getting
`Err(Cancelled)` ([`Supervisor`](https://docs.rs/actix/latest/actix/struct.Supervisor.html)).
No strategies, restart limits or supervision tree are documented on that page.
Testing: `#[actix::test]` and `actors::mocker::Mocker`, a "mocking utility actor"
substituted through a `cfg(test)` type alias and the service registry; replies
are built from a `Box<Any>` closure ([mocker](https://docs.rs/actix/latest/actix/actors/mocker/index.html)).
No distribution in the crate (third-party crates exist; not evaluated).

### ractor

Modelled on `gen_server` ([docs.rs](https://docs.rs/ractor/latest/ractor/)). An
actor has one message type (usually an enum); recent versions generate
exhaustive dispatch for "focused" handlers so an unhandled variant is a compile
error ([README](https://github.com/slawlor/ractor)). Request/reply goes through
`RpcReplyPort` with `call!` (no time limit), `call_t!` (timeout), `cast!` and
`forward!`. Supervision in the core is linking: a supervisor receives
`SupervisionEvent`s when a child starts, stops or panics, and "Supervision is
presently left to the implementor". Erlang-style monitors are an opt-in
`monitors` feature; `pg` provides named process groups; factories provide
worker pools. OTP strategies live in a separate crate, `ractor-supervisor`
(static `Supervisor` with OneForOne/OneForAll/RestForOne, `DynamicSupervisor`
with `max_children`, `TaskSupervisor`, Permanent/Transient/Temporary, and a
supervisor-wide "meltdown" window with `reset_after`)
([ractor-supervisor](https://github.com/simke9445/ractor-supervisor)).
Distribution: `ractor_cluster`, an EPMD-like node layer whose README says it
"shouldn't be considered production ready".

### kameo

Typed per-message handlers: `Message<T>` declares `type Reply`, `ask` awaits a
reply and the docs advertise bounded and unbounded mailboxes with backpressure
([docs.rs](https://docs.rs/kameo/latest/kameo/)). Supervision is OTP-shaped:
OneForOne (default), OneForAll and RestForOne; Permanent (default), Transient and
Never restart policies; and a restart limit (default 5 within 5 s) that is **per
child**: "If a child exceeds this limit, the supervisor stops restarting it",
rather than failing the supervisor
([supervision guide](https://github.com/tqwewe/kameo/blob/main/docs/core-concepts/supervision.mdx)).
Links are separate from supervision: by default an actor stops when a linked
actor dies abnormally, and `on_link_died` can return `ControlFlow::Continue` to
trap it (same guide). Distribution is built in: named registration and
`RemoteActorRef` lookup over libp2p (TCP, WebSocket, QUIC). An optional `console`
feature provides a terminal UI of the supervision tree, throughput, mailbox
backpressure, restarts and "deadlocks" (docs.rs page; we did not run it or
check how deadlocks are detected).

### xtra

"A tiny, fast, and safe actor system" ([docs.rs](https://docs.rs/xtra/latest/xtra/)).
Typed `Handler<M>` with a per-message return; `send` returns a future that
resolves to the handler's return value. Runtime-agnostic (`spawn_tokio`,
`spawn_smol`, `spawn_async_std`, `spawn_wasm_bindgen`), with an optional
`tracing` integration. No supervision, links, monitors or distribution; an
actor stops itself with `stop_self`.

### coerce

An actor runtime plus distributed-systems layer
([repository](https://github.com/LeonHartley/Coerce-rs)): "supervision / child
spawning" (details not documented on the README), remoting over Protobuf,
cluster sharding with rebalancing and restart on healthy nodes, persistence with
journaling, event sourcing and snapshots (in-memory and Redis providers), and
built-in metrics with an HTTP diagnostics API. The closest Rust analogue to Akka
Cluster Sharding plus Akka Persistence.

### lunatic

Not a library over Tokio but a WebAssembly runtime: each process is an isolated
Wasm instance with its own heap, preempted by the VM, which is the closest Rust
gets to BEAM process isolation. The Rust SDK (`lunatic` crate) offers
`AbstractProcess`, links and a `Supervisor` that "can detect failures (panics)
inside AbstractProcesses and restart them", with OneForOne, OneForAll and
RestForOne strategies ([supervisor](https://docs.rs/lunatic/latest/lunatic/supervisor/index.html),
[`SupervisorStrategy`](https://docs.rs/lunatic/latest/lunatic/supervisor/enum.SupervisorStrategy.html)).
Code must compile to Wasm, so ordinary Tokio libraries do not run inside it.
Distributed nodes are advertised in the project README (not verified here).

### bastion

A "fault-tolerant runtime" with supervisors over *children groups*: OneForOne
(default), OneForAll and RestForOne, each restarting groups "in the order they
were added" ([`SupervisionStrategy`](https://docs.rs/bastion/latest/bastion/supervisor/enum.SupervisionStrategy.html)),
and per-actor restart delays `Immediate`, `LinearBackOff` and `ExponentialBackOff`
([`ActorRestartStrategy`](https://docs.rs/bastion/latest/bastion/supervisor/enum.ActorRestartStrategy.html)).
Messages are untyped (matched at runtime with a `msg!` macro, from our reading of
older docs; not re-checked). The last release is from January 2022 and the last
push to the repository from April 2023, so we treat it as unmaintained.

### Comparison

| | actix | ractor | kameo | xtra | coerce | lunatic | bastion | Spawned 0.7.0 |
|---|---|---|---|---|---|---|---|---|
| Message typing | `Handler<M>` per type | One enum per actor | `Message<T>` per type | `Handler<M>` per type | Per type (README example) | Not checked | Untyped (runtime match; not re-checked) | `Handler<M>` per type plus `#[protocol]` traits |
| Request/reply | `send` future (and fire-and-forget variants) | `call!`/`call_t!` with `RpcReplyPort` | `ask` | `send` future | `send` returns a result (README example) | Not checked | Not checked | `request`/protocol methods returning `Response<T>`, per-method timeouts |
| Supervision strategies | In-place restart, none | Events only in core; OTP strategies in `ractor-supervisor` | OneForOne/OneForAll/RestForOne | None | Child spawning (undocumented) | OneForOne/OneForAll/RestForOne | OneForOne/OneForAll/RestForOne | OneForOne/OneForAll/RestForOne |
| Restart limit | None documented | Supervisor-wide meltdown (`ractor-supervisor`) | Per child, then stops restarting | — | Not documented | Not checked | Not checked | Supervisor-wide rolling window; exhaustion fails the supervisor and escalates |
| Nested trees | No | Yes (linking) | Yes | — | Not documented | Yes | Yes | Yes (`SupervisorSpec`) |
| Dynamic children | — | `DynamicSupervisor` (`ractor-supervisor`) | Not checked | — | — | Not checked | Not checked | Planned (rank 12a) |
| Links / monitors | No | Links; monitors (feature) | Links with trap | No | Not checked | Links | Not checked | Links, monitors, `trap_exit` |
| Distribution | No | `ractor_cluster` (not production ready) | libp2p remote refs | No | Remoting, sharding | Nodes (README, not verified) | Not checked | No (rank 44, research) |
| Persistence | No | No | No | No | Event sourcing, snapshots | No | No | No (ranks 49-51, research) |
| Runtime | Own Arbiter | Tokio | Tokio | Any | Tokio | Wasm VM | Own executor | Tokio, blocking pool, OS threads, native pools, pinned shards; Tokio-free sync build |
| Test tooling | `Mocker`, `#[actix::test]` | None found | None found | None found | None found | None found | None found | `spawned-test` (merged #198, unreleased) |
| Verification beyond tests | None found | None found | None found | None found | None found | None found | None found | TLA+ with trace replay, Loom, TSAN, mutation testing |
| Latest release | 0.13.6, 2026-11-08 | 0.16.6, 2026-11-06 | 0.22.2, 2026-07-18 | 0.6.0, 2024-02-02 | 0.8.11, 2023-10-16 | 0.14.1, 2023-08-09 | 0.4.5, 2022-01-07 | 0.7.0 |

"None found" for verification: GitHub code search for `loom` and `proptest` in
the actix, ractor and kameo repositories returned no results (2026-11-09); we
found no TLA+ or model-checking material for any of the seven. Release versions
and dates are from `https://crates.io/api/v1/crates/<name>/versions`.

## Who uses it

- **actix**: about 21 million all-time crates.io downloads, 2.75 million in the
  last 90 days (crates.io API, 2026-11-09). Many downloads may be transitive;
  we did not check which dependents use actix actors rather than its runtime.
- **ractor**: its README states it "is used as the basis for Meta's Rust thrift
  overload protection scheme", presented at RustConf 2024
  ([README](https://github.com/slawlor/ractor)). Only the project's own claim.
- **kameo**: about 1.2 million downloads; ractor about 1.5 million (crates.io).
  We did not verify named production users for kameo, xtra, coerce, lunatic or
  bastion. Spawned (`spawned-concurrency`) has about 310,000 downloads; its
  production users are Ethrex and Ethlambda (roadmap rank 5).

## What it means for Spawned

- **Where Spawned stands.** On supervision Spawned now matches the OTP shape that
  kameo, lunatic, bastion and `ractor-supervisor` also offer, with two
  differences: its restart budget is supervisor-wide and escalates on
  exhaustion (as OTP; kameo stops restarting one child instead), and restarts
  build a fresh actor from a factory (as OTP; actix calls `restarting()` on the
  old value). It is the only crate here with two execution modes, placement
  through execution groups, and Tokio-free synchronous builds. It lacks dynamic
  supervisors (ractor-supervisor has one; [rank 12a](../../ROADMAP.md#ranked-backlog)),
  distribution (ractor, kameo, coerce; [rank 44](../../ROADMAP.md#ranked-backlog)),
  persistence (coerce; [ranks 49-51](../../ROADMAP.md#ranked-backlog)) and a live
  inspection tool (kameo's console; [rank 30](../../ROADMAP.md#ranked-backlog)).
- **Verification is the differentiator.** No crate here publishes a model of its
  lifecycle, trace validation, Loom or mutation testing. That supports the
  roadmap's choice to put the [database-grade testing program](../../ROADMAP.md#database-grade-testing-program)
  before new capabilities and to make deterministic exploration
  ([rank 20](../../ROADMAP.md#ranked-backlog)) the flagship.
- **Testing tools are thin everywhere.** actix's `Mocker` uses `Box<Any>`
  replies and the service registry; nobody else ships a test kit. Spawned's
  typed mocks and probes ([rank 6](../../ROADMAP.md#ranked-backlog)) fill a real
  gap; see [actor testing](actor-testing.md) for non-Rust prior art.
- **kameo's console is the nearest prior art for `spawned-top`** (rank 30): a TUI
  showing the tree, mailbox pressure and restarts. Worth reading before
  designing rank 30's snapshot format; see [observability](observability.md).
- **Corrections to the archived comparison.** Its table credited actix with
  OneForOne/OneForAll and ractor with full Erlang-style supervision, RestForOne
  and dynamic supervisors. Per the current docs, actix has in-place restart only,
  and ractor's strategies and dynamic supervisor are in the separate
  `ractor-supervisor` crate, not ractor itself.

## Sources

- actix: [docs.rs](https://docs.rs/actix/latest/actix/), [`Supervisor`](https://docs.rs/actix/latest/actix/struct.Supervisor.html), [`Mocker`](https://docs.rs/actix/latest/actix/actors/mocker/index.html)
- ractor: [docs.rs](https://docs.rs/ractor/latest/ractor/), [README](https://github.com/slawlor/ractor), [ractor-supervisor](https://github.com/simke9445/ractor-supervisor)
- kameo: [docs.rs](https://docs.rs/kameo/latest/kameo/), [supervision guide](https://github.com/tqwewe/kameo/blob/main/docs/core-concepts/supervision.mdx)
- xtra: [docs.rs](https://docs.rs/xtra/latest/xtra/)
- coerce: [repository](https://github.com/LeonHartley/Coerce-rs)
- lunatic: [`supervisor`](https://docs.rs/lunatic/latest/lunatic/supervisor/index.html), [`SupervisorStrategy`](https://docs.rs/lunatic/latest/lunatic/supervisor/enum.SupervisorStrategy.html), [lunatic](https://github.com/lunatic-solutions/lunatic)
- bastion: [`SupervisionStrategy`](https://docs.rs/bastion/latest/bastion/supervisor/enum.SupervisionStrategy.html), [`ActorRestartStrategy`](https://docs.rs/bastion/latest/bastion/supervisor/enum.ActorRestartStrategy.html), [repository](https://github.com/bastion-rs/bastion)
- Release data: crates.io API (`/api/v1/crates/<name>/versions`), queried 2026-11-09
