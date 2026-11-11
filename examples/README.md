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
| Named shutdown reports and active turns (`diagnostics` feature) | [shutdown_diagnostics](capabilities/src/bin/shutdown_diagnostics.rs): recovery, in-flight message/age, waiting again on incomplete cleanup | n/a (ActorSystem is async-only) |
| Protocols, `#[actor]`, request/reply and sends | [name_server](name_server), [bank](bank), [ping_pong](ping_pong) | [bank_threads](bank_threads), [ping_pong_threads](ping_pong_threads) |
| Type-erased protocol refs (`XRef`) between actors | [chat_room](chat_room) | [chat_room_threads](chat_room_threads) |
| Stream listeners and per-message recipients | [streams_and_recipients](capabilities/src/bin/streams_and_recipients.rs) | Same example |
| Shared protocol contracts and downstream handwritten fakes | [contract library](protocol_contract/src/lib.rs), [consumer test](capabilities/tests/cross_crate_protocol.rs) | Same test covers native actors |
| Lifecycle hooks (`started`/`stopped`) and `ctx.stop()` | [bank](bank), [updater](updater), [signal_test](signal_test) | [bank_threads](bank_threads), [updater_threads](updater_threads), [signal_test_threads](signal_test_threads) |
| Timers (`send_after`, `send_interval`) | [updater](updater), [signal_test](signal_test), [blocking_genserver](blocking_genserver) | [updater_threads](updater_threads), [signal_test_threads](signal_test_threads) |
| `send_message_on` and Ctrl-C handling | [signal_test](signal_test) | [signal_test_threads](signal_test_threads) |
| Backend selection (`Async`, `Blocking`, `Thread`) | [blocking_genserver](blocking_genserver), [signal_test](signal_test) | n/a |
| Blocking-handler warning | [busy_genserver_warning](busy_genserver_warning) | n/a |
| Dedicated-thread actor with async helpers | [dedicated_thread](capabilities/src/bin/dedicated_thread.rs) | n/a |
| Global registry (`registry::register`/`whereis`) | [service_discovery](service_discovery), [registrations and fakes](capabilities/src/bin/registrations_and_fakes.rs) | [registrations and fakes](capabilities/src/bin/registrations_and_fakes.rs), [thread_only](thread_only/README.md) |
| Exit reasons, `join`, `wait_exit`, monitors, links, trapped exits | [exit_reason](exit_reason) | [native_lifecycle](capabilities/src/bin/native_lifecycle.rs) |
| Execution groups (`ActorSystem`, `start_on`, owned helpers), v0.7 | [execution_groups](capabilities/src/bin/execution_groups.rs) | n/a |
| Supervision, v0.7 | [supervision_recovery](capabilities/src/bin/supervision_recovery.rs) | n/a |
| Fixed native actor pools / parked Linux pinned shards, v0.7 | [execution_profiles](capabilities/src/bin/execution_profiles.rs) | n/a |
| Nested supervisors, OneForAll / RestForOne, v0.7 | [supervision_tree](capabilities/src/bin/supervision_tree.rs) | n/a |
| Tokio-free dependency graph, protocols, lifecycle, monitors and links, v0.7 | n/a | [thread_only](thread_only/README.md) (isolated workspace) |
| Actors on separate runtimes | [runtime_isolation](capabilities/src/bin/runtime_isolation.rs) | n/a |
| Request timeout outcomes | [timeout_outcomes](capabilities/src/bin/timeout_outcomes.rs), [protocol_timeouts](capabilities/src/bin/protocol_timeouts.rs) | [protocol_timeouts](capabilities/src/bin/protocol_timeouts.rs) |
| Draining a finite batch before stop | [graceful_drain](capabilities/src/bin/graceful_drain.rs) | gap |
| Byte-budgeted pipeline (application adapter) | [byte_budget_pipeline](capabilities/src/bin/byte_budget_pipeline.rs) | gap |
| Testing actors: mocks, probes, harness and fault injection (`spawned-test`, since v0.7.1) | [test_kit](capabilities/src/bin/test_kit.rs) | [test_kit](capabilities/src/bin/test_kit.rs) (native mock and injected stop) |
| Deterministic simulation tests (`#[spawned_concurrency::sim_test]`, `--cfg spawned_sim`): seed sweeps, virtual time, replay | [deterministic_simulation](capabilities/tests/deterministic_simulation.rs) | n/a (simulator runs async tasks only) |
| Framework metrics (`metrics` feature) | [framework_metrics](capabilities/src/bin/framework_metrics.rs) | [thread_only](thread_only/README.md) with metrics |

"n/a" means the feature exists only in tasks mode; "gap" means it exists in
threads mode but has no example yet.

## Gaps

Remaining gaps are marked in the feature index. The
[roadmap's examples to add](../ROADMAP.md#examples-to-add) describes examples for
planned capabilities. When a feature lands, add its runnable, asserted example
and a row here in the same change.


The [streams and recipients example](capabilities/src/bin/streams_and_recipients.rs)
covers both runtime modes. [Protocol fakes and named recovery](capabilities/src/bin/registrations_and_fakes.rs)
show plain protocol fakes and registration patterns; the [test kit](capabilities/src/bin/test_kit.rs)
shows `spawned-test` mocks, probes and fault injection; [coordinated shutdown](capabilities/src/bin/coordinated_shutdown.rs)
supports the [Ethrex/Ethlambda migration guide](../docs/MIGRATION.md#preparing-for-07).
