# Migration guide

Upgrade notes per release, newest first. The [CHANGELOG](../CHANGELOG.md) lists
every change; this guide covers what you need to edit when upgrading.

## Unreleased

**Simulation builds only (`--cfg spawned_sim`; determinism D1-D3).**

- `sim_test` / `SimTest::run`, `sim::run_once`, `run_with`, `shrink` and
  `explore` bodies must be `Sync` (each simulated run gets a fresh thread). A
  closure that captures an `Rc` or `Cell` needs an `Arc`/`Mutex` instead; the
  bound applies in normal builds too, so a test compiles the same way in both.
  State a body leaves in a `thread_local!` is on the run's thread, not the
  test's: collect results through an `Arc<Mutex<..>>` or a static keyed by
  `std::thread::current().name()` (the run thread is named like the test).
- `std::time::Instant::now()` and `SystemTime::now()` inside a simulated run
  read virtual time, and OS entropy (`HashMap` keys, `thread_rng`, `OsRng`) is
  seeded by the run. A test that measured real time inside a run must use
  `sim::wall_now()`; a loop that spins on `Instant::elapsed()` with no
  `.await` never ends (the watchdog aborts it).
- `spawn_blocking` no longer panics: it runs inline as one step.
  `Backend::Blocking` actors and nested `block_on` still fail, now by failing
  the run.

## 0.7.3

**Behaviour change (supervision).** A Transient child's replacement (any generation after the child was first
ready) that stops `Normal` or `Shutdown` from `started()`, or is shut down
before readiness, no longer fails its supervisor. It is a failed restart,
retried within `max_restarts` like a failed factory (factory errors are
retried too). Under OneForAll a failed replacement now starts a new recovery
round, so the whole cohort is stopped and rebuilt again, as in OTP; before,
only the failed child was retried. If you relied on a failing child to stop a
tree, stop it explicitly with `Supervisor::stop` or `Supervisor::shutdown`:
budget exhaustion is not a reliable stop, since an expiring
`restart_window` can keep granting retries. The first startup is unchanged.

Users of the simulation backend need `RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable"`;
normal builds are unaffected.

## 0.7.2

No API changes; update the version. Two inputs the macros used to accept now
fail to compile with a clear error, because they were already wrong:

- `#[actor(protocol = A protocol = B)]` without a comma between entries. Add
  the comma.
- `#[actor]` on a trait impl such as `impl Default for X {}`, which silently
  generated an `Actor` impl. Put `#[actor]` on the actor's inherent impl block.

Macro error messages are clearer and point at the offending tokens; code that
compiled before (apart from the two cases above) compiles unchanged.
`#[actor(protocol = P)]` on a generic actor now compiles.

## 0.7.1

`ActorId` Display now prints a bare decimal number, for example `3` instead of
`ActorId(3)`. Debug (`{:?}`) retains the tagged `ActorId(3)` representation.

Upgrade all Spawned dependencies together when adopting 0.7.1. This release
also introduces `spawned-test` as a published dev-dependency. Existing
default-feature and Tokio-free builds retain their feature selection.

A Kill received over a link is now trappable. Only a direct Kill (for example,
`child.stop_with_reason(ExitReason::Kill)`) bypasses `trap_exit`; this prevents a
direct kill from killing an entire graph of trapping observers. No exit-reason
variant was added. Committed abnormal/Shutdown causes survive queued-message
and actor-state destructor failures during cleanup.
Destructor diagnostics now distinguish `actor state destructor: ...` from
`actor lifecycle unwound: ...` (previously `actor lifecycle destructor: ...`).
Match the `ExitReason::Panic` variant rather than relying on its message text.

Restart capacity is reserved before backoff. Exhaustion fails immediately;
interrupted reservations are refunded, and only actual grants enter the rolling
window. Charging and retry-generation admission are atomic; a placement or
supersession that prevents admission refunds its reservation. A constructor or
`started()` failure after admission still consumes that retry grant. Cohort
recovery retains v0.7's per-child accounting: collateral OneForAll/RestForOne
replacements also consume grants (six replacements need six grants).
Pending backoffs now reserve capacity as well: with `max_restarts=1`, a one-second
window and a three-second backoff, a second child failing while the first waits
can exhaust the supervisor. Pending reservations do not expire with the rolling
window, so concurrency is capped even when `backoff.maximum > restart_window`.

Once admission closes, `SupervisedChild::current()` and `wait_for_generation()`
can now return `ChildUnavailable(Failed)` or `ChildUnavailable(Stopped)` for a
terminal owner/child, rather than always `ChildUnavailable(Stopping)` as in
0.7.0. Handle these terminal states as well as in-progress stopping.

Readiness is a historical commitment: if system admission closes after startup
commits readiness but before `start()` returns, startup still succeeds and the
tree waits for its ordered shutdown turn. Dropping that already-ready startup
future does not roll back the tree; ActorSystem still owns it.
Likewise, failure after readiness but before the caller resumes does not undo
startup success. Inspect `Supervisor::failure()`/the shutdown report for later
failure; `Ok(Supervisor)` does not guarantee continued liveness.

With jitter enabled, `Backoff::initial` is the first delay cap, not a minimum:
`initial=1s` allows any first delay from zero through one second. Disable jitter
to use the capped exponential delay exactly.

Give-up logs identify the originating child once. `restart_scheduled`
now reports a granted replacement after backoff/placement checks.

Embedded libraries can use `metrics::configure_composable` after actors start;
the smallest explicitly requested nonzero timing interval wins. A zero request
never disables another library's timings. Legacy `configure` retains its
first-initializer contract. For incremental managed ownership on an existing
Tokio runtime, use `system.async_group_on(name, &handle)` and keep the runtime
alive and driven through shutdown. Ambient `.start()` stays unmanaged.


- **0.7:** [timeout configuration and client shutdown migration](#preparing-for-07).
- **0.6:** no breaking API changes; existing handler/protocol signatures are
  unchanged. See the [CHANGELOG 0.6.0 section](../CHANGELOG.md#060--2026-11-06).

<a id="preparing-for-07"></a>

## Upgrading to 0.7

These APIs are available in v0.7.0. Keep ordinary `.start()`
and the default Tokio backend unless adopting explicit ownership/placement.
A dependency upgrade alone does not place existing actors in an `ActorSystem`.

### Direct 0.5 to 0.7 upgrade for Ethrex and Ethlambda

Both audited clients pin Spawned 0.5.0. Upgrade `spawned-concurrency` and
`spawned-rt` together to v0.7.0. This path includes v0.6's lifecycle corrections as well as the new
v0.7 capabilities. Ordinary 0.5 protocol/handler signatures stay in place; the
older enum-to-protocol rewrite under [Upgrading to 0.5](#upgrading-to-05) is not
needed again.

1. Keep their existing default features and backend initially. Other consumers
   that disabled defaults must apply the explicit-`async` change below.
2. Recheck shutdown assumptions: after stop is observed, queued work can be
   discarded and pending requests can return `ActorStopped`. Do not rely on a
   previously racing handler completing, retrying safely, or an arbitrary sleep
   proving cleanup. Timeouts still do not roll back effects.
3. v0.6 adds identities, monitors, links and trapped exits; they are optional.
   They do not automatically restart actors or recover persisted state.
   Abrupt future/process destruction still cannot guarantee async `stopped()`.
4. Adopt system-owned startup/shutdown and supervision subsystem by subsystem
   using the client recipe below. Replace hand-wired signals only after their
   snapshot, persistence and dependency ordering are explicit.
5. Set per-method timeouts from application requirements, then run matched
   baseline/candidate node or actor-backed workloads and fault-injected recovery.
   An isolated compile check alone does not validate client behavior.

The [0.6 changelog](../CHANGELOG.md#060--2026-11-06) records the intervening fixes.
The rest of this section provides the Cargo, timeout and shutdown details for
this complete 0.5 to 0.7 path.

### Cargo features and protocol attributes

**The no-default configuration is breaking in v0.7.** v0.6 had no default feature
set and always exported Tokio APIs. Both libraries now enable `async` by default;
disabling defaults removes `tasks` and async-only exports, including the async
`Response` constructors. Existing no-default users needing those APIs must
explicitly enable `async` on the dependency, or restore default features:

```toml
spawned-concurrency = { version = "0.7", default-features = false, features = ["async"] }
spawned-rt = { version = "0.7", default-features = false, features = ["async"] }
```

These settings select the published v0.7 line.
Thread-only applications intentionally leave `async` disabled. Optional `metrics`
does not enable it. The audited Ethrex `1dfaa09ec` and Ethlambda `c3f5f566` manifests
both use default features with Spawned 0.5.0; they do not need this Cargo edit.

v0.6 silently ignored `#[protocol(...)]` arguments. Remove obsolete arguments;
v0.7 accepts `timeout = DurationExpression` and rejects unknown or duplicate
options. Put `#[request_timeout(...)]` directly on a request method. Conditional
`#[cfg_attr(..., request_timeout(...))]`, including nested forms and false
predicates, is rejected with a specific diagnostic.

Protocols requiring `Debug` work with ordinary and timeout-configured handles.
Other custom supertraits do not become automatic implementations on
`RequestOptions`: their existing ordinary ActorRef implementations remain valid,
while configured calls require the wrapper to satisfy the protocol's supertraits.

The global registry retains `RegistryError::AlreadyRegistered`, including
exhaustive matches written against v0.6. The new managed
`Context::register_name` returns `ManagedRegistrationError`: `Registry(error)`
for a name collision, or `OwnerClosed` for an unmanaged or closing owner.
Manual `registry::register_owned` still returns `RegistryError`; its guard
needs to be retained by its caller.

### Protocol request timeouts

The default remains five seconds. Choose a protocol default, override individual
request methods, or configure one cloned concrete handle:

```rust
#[protocol(timeout = std::time::Duration::from_secs(30))]
trait ChainProtocol: Send + Sync {
    fn import_block(&self, bytes: Vec<u8>) -> Response<()>;
    #[request_timeout(std::time::Duration::from_secs(2))]
    fn status(&self) -> Response<Status>;
}

// actor is a concrete ActorRef implementing ChainProtocol.
let status = actor.with_request_timeout(std::time::Duration::from_secs(5))
    .status().await?;
```

Precedence is handle override, method attribute, protocol attribute, five-second
default. Configure before type erasure:
`actor.with_request_timeout(limit).to_chain_ref()`. A previously erased `ChainRef`
does not expose concrete-handle configuration. Existing handwritten protocol
fakes need no additional trait methods.

The synchronous API uses the same settings and blocks at the protocol method
call, returning `Response::ready`; extract with `.unwrap()`/`.expect()` or inspect
the result. A timeout still leaves the outcome of accepted work indeterminate:
queued or running handlers may execute after the caller returns. It is not a
cancellation or rollback mechanism. Retrying a state-changing operation still
needs an application-level idempotency decision.

### Ethrex and Ethlambda: own startup and shutdown

The [client audit](../ROADMAP.md#client-sweep-production-pain) pins the inspected
revisions: Ethrex `1dfaa09ec`, Ethlambda `c3f5f566`. Verify deployment revisions
before applying a recipe. This guide describes a migration, not a claim that
either client has already adopted these APIs.

| Existing pattern | Migration |
|---|---|
| Independent Ctrl-C listeners in several Ethrex actors | Keep one application signal owner. Stop ingress/producers, then invoke one system shutdown. Preserve SIGTERM handling for containers as well as SIGINT. Spawned does not install OS signal handlers. |
| Cancel the main token, query peer state after actors may have stopped, or save an empty list on `ActorStopped` | Query/snapshot and persist required state while dependencies are still live. Treat a failed snapshot explicitly; an empty fallback can overwrite useful peer data. |
| Fixed sleeps to allow shutdown | Await `system.shutdown(deadline)`, inspect the report and retain it if cleanup is incomplete. A sleep is not a join. |
| Ethlambda's hand-wired actor stops with no common deadline | Declare dependencies once, stop dependents before their dependencies, and choose a process/container grace period that includes snapshot persistence and cleanup. |
| Sequential startup that logs a failed child and continues with a partly initialized subsystem | Put fallible initialization in `ChildSpec` factories, await tree startup, and propagate failure. Failed partial startup rolls back owned resources. |
| Detached workers or listeners that outlive an actor generation | Use managed helpers where appropriate; keep existing session/generation fencing for late external results. Arbitrary raw spawns do not become owned automatically. |

A migration can proceed subsystem by subsystem:

1. Keep one `ActorSystem` alive at the application root. Build a small number of
   named `async_group`s with explicit worker budgets. Both clients depend on
   Tokio; a Tokio-free build is not this migration.
2. Admit standalone managed actors with `start_on(&group)` or declare a
   supervisor tree with `ChildSpec` factories. Declare dependencies before
   dependents and resolve dependencies again inside each fresh factory.
3. Start with OneForOne for independent children. If a dependent retains an
   instance-bound reference to an earlier child, use RestForOne or explicitly
   re-resolve the dependency after failure. Rebuilding only the upstream child
   does not rebind the downstream actor's old reference.
4. Await readiness before opening external ingress. Choose restart mode, shared
   retry intensity/window and backoff from the subsystem's recovery contract.
   A restart creates fresh state; storage recovery is application code.
5. On shutdown, stop admission at the application boundary and join producers.
   Take required snapshots, flush application work and await persistence.
   Only then close the system. Ordinary framework stop discards queued work;
   it does not automatically drain mailboxes.
6. Await the report. An incomplete report retains cleanup ownership and can be
   waited on again. Do not drop the owner and assume a stuck blocking helper was
   killed: handlers and native closures must return cooperatively.

The runnable [coordinated shutdown example](../examples/capabilities/src/bin/coordinated_shutdown.rs)
asserts a two-group dependency tree, a joined finite producer, a snapshot before
closure and reverse shutdown. Its FIFO fence relies on all producers already
having joined and does not acknowledge disk durability. The
[named registration example](../examples/capabilities/src/bin/registrations_and_fakes.rs)
shows replacement lookup and automatic managed-name cleanup.

Set restart budgets and container termination budgets separately. A shutdown
report deadline bounds how long the caller waits for a report; it cannot bound
how long arbitrary blocking application code takes to release resources.
Use `ctx.spawn_helper`/`spawn_blocking_helper` for scoped work and await its
resource cleanup. The framework does not infer an application's persistence
dependencies or retry safety.

### Validation before rollout

v0.7.0 passed the library checks and an isolated Ethlambda compile check.
A running-node canary with fault injection has not yet been completed; keep it
as the application rollout gate rather than inferring runtime results from compilation.

Pin the same client source/configuration and compare the same replay or canary
workload with only the Spawned dependency changed. Record startup, ready/stop
outcomes, unexpected restarts, queue wait, handler time, request timeouts, CPU,
RSS and OS thread count. Separate warmup from steady state and match imported
blocks; unrelated EngineClient or storage differences are not framework evidence.

Ethlambda's current [import replay implementation](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/bin/ethlambda/src/benchmark/import/replay.rs)
calls the import function directly, bypassing actor mailboxes, the tick loop and
P2P. `replay-era` similarly targets the state transition. They are useful
component checks; neither alone validates the actor runtime. Use a real node
canary or an actor-backed replay path for application rollout validation.

The library's tests, bounded models and examples are necessary checks. Full-client
replay or a same-node Ethlambda canary remains the application rollout gate. Do not promote
synthetic backend speedup ratios to client claims. Pinned/native groups remain
opt-in; profile the client before changing its placement.

Exercise failure recovery in a canary subsystem that actually adopts supervision;
upgrading the dependency alone cannot validate automatic restarts. Inject a
controlled actor failure, verify a fresh identity/generation becomes ready and
dependents resolve its replacement, and check pending requests fail cleanly.
Then exercise restart-budget exhaustion and shutdown during backoff, confirming
the client reports failure and releases owned resources. Keep these hooks out of
ordinary production ingress. Preserve client revisions, configuration, inputs,
logs and matched baseline measurements with the results before promoting the client migration.

## Upgrading to 0.5

Spawned 0.5 replaces the enum-based `Actor` trait with protocol macros. The new API eliminates message enum boilerplate and manual dispatch — you define a trait, implement handlers, and call methods directly on actor references.

### Concept Mapping

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

### Before/After: Messages

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

#### Generated module naming

`#[protocol]` generates a submodule containing the message structs. The naming rules:

- **Module name**: trait name converted to `snake_case` — e.g., `trait FooBarProtocol` → `mod foo_bar_protocol`
- **Message structs**: method name converted to `PascalCase` — e.g., `fn do_thing(...)` → `DoThing`
- **Type-erased ref**: strips `Protocol` suffix → `{Base}Ref` — e.g., `FooBarProtocol` → `FooBarRef = Arc<dyn FooBarProtocol>`

Import message structs from the generated module:

```rust
use crate::protocols::foo_bar_protocol::{DoThing, GetStatus};
```

#### Mixed send + request protocols

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

### Before/After: Actor Implementation

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

#### Handler attributes

The `#[actor]` macro supports three handler attributes:

| Attribute | Use for |
|-----------|---------|
| `#[request_handler]` | Messages that expect a reply (`Response<T>`) |
| `#[send_handler]` | Fire-and-forget messages (no return or `-> ()`) |
| `#[handler]` | Generic — works for either kind |

All three generate the same `impl Handler<M>` code. The distinction is semantic: use `#[request_handler]` and `#[send_handler]` to document intent, or `#[handler]` when you don't want to commit to a specific kind.

Handler signature: `fn name(&mut self, msg: MessageType, ctx: &Context<Self>) -> ReturnType`

### Before/After: Calling an Actor

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

#### Send methods are now synchronous

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

### Before/After: Timers

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

#### Self-rescheduling tick pattern

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

### Before/After: Lifecycle Hooks

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

### Before/After: Stopping an Actor

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

### New: Type-Erased Protocol References

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

### New: Actor Registry

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

### Import Changes

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

### spawned-rt Changes

`spawned-rt` has **no breaking API changes** in 0.5. Both `spawned_rt::tasks` and `spawned_rt::threads` retain the same public exports (`spawn`, `sleep`, `CancellationToken`, `mpsc`, `oneshot`, etc.). Update the version in your `Cargo.toml` but no code changes are needed for `spawned-rt` imports.

### Escape Hatches

If the macros don't fit your use case:

- **Manual `Message` impls** — Define message structs and implement `Message` by hand instead of using `#[protocol]`.
- **Manual `Handler<M>` impls** — Skip `#[actor]` and implement `Handler<M>` for each message type by hand.
- **`Recipient<M>`** (`Arc<dyn Receiver<M>>`) — Type-erased reference for a single message type, available via `ctx.recipient::<M>()`.
