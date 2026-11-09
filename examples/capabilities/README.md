# Capability examples

These finite programs use the current public API and assert their expected
outcomes. Run commands from the repository root. They use small worker budgets
and no external services. The [proposed example catalog](../../ROADMAP.md#examples-to-add)
describes capabilities that are still on the roadmap.

| Program | Why it is useful | What it demonstrates |
|---|---|---|
| [execution_groups](src/bin/execution_groups.rs) | Own subsystem runtime budgets and cleanup | Router and worker communicate across named parked async groups; registered helpers use the assigned group, and system shutdown closes placement and joins cleanup. |
| [execution_profiles](src/bin/execution_profiles.rs) | Choose a resource model explicitly | Fixed native roots share a helper runtime and reject excess admission; parked Linux shards pin stages to selected CPUs. |
| [supervision_tree](src/bin/supervision_tree.rs) | Recover dependent subsystems | Nested failure escalates to OneForAll/RestForOne; reconstruction preserves ordering and old references stay stale. |
| [supervision_recovery](src/bin/supervision_recovery.rs) | Recover a failed service without changing handlers | A OneForOne factory reconstructs fresh state in the same group; the old reference fails and the caller explicitly resolves the replacement. |
| [dedicated_thread](src/bin/dedicated_thread.rs) | Isolate a blocking actor while allowing its async helpers to run | One dedicated actor root and one async helper worker, rather than a machine-sized pool per actor. This remains distinct from a Tokio-free synchronous actor. |
| [runtime_isolation](src/bin/runtime_isolation.rs) | Keep a critical subsystem on a separate worker budget | Two owned Tokio runtimes; an actor on one requests an actor on the other, then both stop and join before runtime destruction. Placement uses `Runtime::enter()` today. |
| [byte_budget_pipeline](src/bin/byte_budget_pipeline.rs) | Limit payload allocation across multiple stages | Eight producers reserve bytes before constructing payloads; a permit travels through two actors and returns after payload destruction. This is an application adapter, not a bounded framework mailbox. |
| [timeout_outcomes](src/bin/timeout_outcomes.rs) | Understand whether retrying a timeout is safe | A gate proves an effect happened before the caller timed out. The actor finishes afterward; timeout is not cancellation or rollback. |
| [graceful_drain](src/bin/graceful_drain.rs) | Finish a finite batch before shutdown | Join every producer, issue a FIFO fence, then stop/join the actor. This recipe requires exclusive control of admission; ordinary stop still discards queued work. |
| [framework_metrics](src/bin/framework_metrics.rs) | Observe actor activity without modifying handlers | Configure sampled timing, issue requests and render process-wide Prometheus text. Metrics are explicitly enabled for this binary. |

```sh
cargo run --locked -p spawned-capability-examples --bin execution_groups
cargo run --locked -p spawned-capability-examples --bin execution_profiles
cargo run --locked -p spawned-capability-examples --bin supervision_tree
cargo run --locked -p spawned-capability-examples --bin supervision_recovery
cargo run --locked -p spawned-capability-examples --bin dedicated_thread
cargo run --locked -p spawned-capability-examples --bin runtime_isolation
cargo run --locked -p spawned-capability-examples --bin byte_budget_pipeline
cargo run --locked -p spawned-capability-examples --bin timeout_outcomes
cargo run --locked -p spawned-capability-examples --bin graceful_drain
cargo run --locked -p spawned-capability-examples --features metrics --bin framework_metrics
```

Runtime isolation consumes additional threads and can strand capacity. It does
not pin workers or establish a performance improvement; measure an application
under equal CPU budgets. See [backend guidance](../../docs/GUIDE.md#execution-modes-and-backends).

The byte budget covers the illustrated payload vectors, not envelopes, task
stacks or all application memory. All participating producers must acquire
credits; bypassing the adapter bypasses the bound. A permit records ownership,
not business success or durability. Production credit admission still needs
defined cancellation, deadlines and rejection behavior.

The drain fence covers completed actor handlers, not detached helpers or a
durable external commit. An open service needs an admission gate and an explicit
shutdown deadline. See [lifecycle semantics](../../docs/GUIDE.md).

Metrics timings add overhead. Choose sampling for the workload and configure it
before creating actors. There is no automatic network exporter or per-actor
label set; see [metrics accounting](../../docs/GUIDE.md#metrics).

When a proposed feature lands, add a runnable, asserted example here (or a
dedicated example package), its run command and its operational limits in the
same change. Do not present speculative API names as working code.

Execution groups and automatic supervision are available on main, unreleased.
Groups support parked async pools, fixed native actor-thread pools and parked
Linux pinned shards. Supervisors support nested trees, OneForAll and RestForOne.
Budgets describe actor workers/slots/shards, not all OS threads. See the
[ownership and recovery guide](../../docs/GUIDE.md#execution-groups-and-supervision) for platform limits,
cooperative shutdown, helper admission, deadlines and reference semantics.

The separate [thread-only example](../thread_only/README.md) proves the native
synchronous API builds without Tokio. It is an independent Cargo workspace;
ActorSystem/supervision still require the async feature.
