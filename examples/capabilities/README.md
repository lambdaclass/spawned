# Capability examples

These finite programs use the current public API and assert their expected
outcomes. Run commands from the repository root. They use small worker budgets
and no external services. The [proposed example catalog](../planned/README.md)
describes capabilities that are still on the roadmap.

| Program | Why it is useful | What it demonstrates |
|---|---|---|
| [dedicated_thread](src/bin/dedicated_thread.rs) | Isolate a blocking actor while allowing its async helpers to run | One dedicated actor root and one async helper worker, rather than a machine-sized pool per actor. This remains distinct from a Tokio-free synchronous actor. |
| [runtime_isolation](src/bin/runtime_isolation.rs) | Keep a critical subsystem on a separate worker budget | Two owned Tokio runtimes; an actor on one requests an actor on the other, then both stop and join before runtime destruction. Placement uses `Runtime::enter()` today. |
| [byte_budget_pipeline](src/bin/byte_budget_pipeline.rs) | Limit payload allocation across multiple stages | Eight producers reserve bytes before constructing payloads; a permit travels through two actors and returns after payload destruction. This is an application adapter, not a bounded framework mailbox. |
| [timeout_outcomes](src/bin/timeout_outcomes.rs) | Understand whether retrying a timeout is safe | A gate proves an effect happened before the caller timed out. The actor finishes afterward; timeout is not cancellation or rollback. |
| [graceful_drain](src/bin/graceful_drain.rs) | Finish a finite batch before shutdown | Join every producer, issue a FIFO fence, then stop/join the actor. This recipe requires exclusive control of admission; ordinary stop still discards queued work. |
| [framework_metrics](src/bin/framework_metrics.rs) | Observe actor activity without modifying handlers | Configure sampled timing, issue requests and render process-wide Prometheus text. Metrics are explicitly enabled for this binary. |

```sh
cargo run --locked -p spawned-capability-examples --bin dedicated_thread
cargo run --locked -p spawned-capability-examples --bin runtime_isolation
cargo run --locked -p spawned-capability-examples --bin byte_budget_pipeline
cargo run --locked -p spawned-capability-examples --bin timeout_outcomes
cargo run --locked -p spawned-capability-examples --bin graceful_drain
cargo run --locked -p spawned-capability-examples --features metrics --bin framework_metrics
```

Runtime isolation consumes additional threads and can strand capacity. It does
not pin workers or establish a performance improvement; measure an application
under equal CPU budgets. See [backend guidance](../../docs/BACKENDS.md).

The byte budget covers the illustrated payload vectors, not envelopes, task
stacks or all application memory. All participating producers must acquire
credits; bypassing the adapter bypasses the bound. A permit records ownership,
not business success or durability. Production credit admission still needs
defined cancellation, deadlines and rejection behavior.

The drain fence covers completed actor handlers, not detached helpers or a
durable external commit. An open service needs an admission gate and an explicit
shutdown deadline. See [lifecycle semantics](../../docs/API-GUIDE.md).

Metrics timings add overhead. Choose sampling for the workload and configure it
before creating actors. There is no automatic network exporter or per-actor
label set; see [metrics accounting](../../docs/METRICS.md).

When a proposed feature lands, add a runnable, asserted example here (or a
dedicated example package), its run command and its operational limits in the
same change. Do not present speculative API names as working code.
