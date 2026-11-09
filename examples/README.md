# Spawned examples

Every user-facing feature mapped to the runnable examples that show it. Run a
package from the repository root with `cargo run --locked -p <package>`; the
capability programs run with
`cargo run --locked -p spawned-capability-examples --bin <name>` (see the
[capability guide](capabilities/README.md) for their commands, purpose and
limits). Some older examples use external HTTP services or run until stopped.
Usage of each feature is documented in the [guide](../docs/GUIDE.md).

## Feature index

| Feature | tasks mode | threads mode |
|---|---|---|
| Protocols, `#[actor]`, request/reply and sends | [name_server](name_server), [bank](bank), [ping_pong](ping_pong) | [bank_threads](bank_threads), [ping_pong_threads](ping_pong_threads) |
| Type-erased protocol refs (`XRef`) between actors | [chat_room](chat_room) | [chat_room_threads](chat_room_threads) |
| Lifecycle hooks (`started`/`stopped`) and `ctx.stop()` | [bank](bank), [updater](updater), [signal_test](signal_test) | [bank_threads](bank_threads), [updater_threads](updater_threads), [signal_test_threads](signal_test_threads) |
| Timers (`send_after`, `send_interval`) | [updater](updater), [signal_test](signal_test), [blocking_genserver](blocking_genserver) | [updater_threads](updater_threads), [signal_test_threads](signal_test_threads) |
| `send_message_on` and Ctrl-C handling | [signal_test](signal_test) | [signal_test_threads](signal_test_threads) |
| Backend selection (`Async`, `Blocking`, `Thread`) | [blocking_genserver](blocking_genserver), [signal_test](signal_test) | n/a |
| Blocking-handler warning | [busy_genserver_warning](busy_genserver_warning) | n/a |
| Dedicated-thread actor with async helpers | [dedicated_thread](capabilities/src/bin/dedicated_thread.rs) | n/a |
| Global registry (`registry::register`/`whereis`) | [service_discovery](service_discovery) | gap |
| Exit reasons, `join`, `wait_exit`, monitors, links, trapped exits | [exit_reason](exit_reason) | gap |
| Execution groups (`ActorSystem`, `start_on`, owned helpers), unreleased | [execution_groups](capabilities/src/bin/execution_groups.rs) | n/a |
| OneForOne supervision, unreleased | [supervision_recovery](capabilities/src/bin/supervision_recovery.rs) | n/a |
| Actors on separate runtimes | [runtime_isolation](capabilities/src/bin/runtime_isolation.rs) | n/a |
| Request timeout outcomes | [timeout_outcomes](capabilities/src/bin/timeout_outcomes.rs) | gap |
| Draining a finite batch before stop | [graceful_drain](capabilities/src/bin/graceful_drain.rs) | gap |
| Byte-budgeted pipeline (application adapter) | [byte_budget_pipeline](capabilities/src/bin/byte_budget_pipeline.rs) | gap |
| Framework metrics (`metrics` feature) | [framework_metrics](capabilities/src/bin/framework_metrics.rs) | gap |

"n/a" means the feature exists only in tasks mode; "gap" means it exists in
threads mode but has no example yet.

## Gaps

These features ship today without a dedicated example: stream listeners
(`spawn_listener`), `Recipient<M>` type erasure, `request_with_timeout`, monitors
and links in thread mode, `ctx.register_name`, and testing actors. The
[roadmap's examples to add](../ROADMAP.md#examples-to-add) describes the example
each should get, plus the examples planned with future capabilities. When a
feature lands, add its runnable, asserted example and a row here in the same
change.
