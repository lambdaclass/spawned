# Actor testing and reliability campaigns

Canary regressions: `cargo test -p spawned-concurrency --features diagnostics,metrics --test canary_feedback --test context_exit`
checks named incomplete reports, active-turn cleanup, explicit exit reasons and
structured supervisor events. Run the same tests without `diagnostics` to check
the uninstrumented path. CI pipes the metrics example through
`scripts/check-metrics-exposition.py` (Prometheus Python client 0.21.1) and rejects
the old undeclared-max output as a negative control. Explicit exit reasons also
produce a real lifecycle trace.

Run `cargo test -p spawned-concurrency --test actor_scenarios`.

The [capability examples](../examples/capabilities/README.md) demonstrate useful
application behavior separately from these test oracles. CI runs their finite
programs and assertions; examples for future APIs are listed in the
[roadmap](../ROADMAP.md#examples-to-add).

## Testing user actors: `spawned-test`

The `spawned-test` crate (`testing/`) is the user-facing test kit: closure-backed
mocks that answer any `#[protocol]`, `Recipient<M>` probes, harness helpers that
fail with a report instead of hanging, and fault injection through public
lifecycle APIs. See the [guide](GUIDE.md#testing-your-actors) and the
[test kit example](../examples/capabilities/src/bin/test_kit.rs).

```sh
cargo test --locked -p spawned-test
cargo test --locked -p spawned-test --no-default-features   # Tokio-free threads helpers
cargo run --locked -p spawned-capability-examples --bin test_kit
```

Its own suites cover both modes (`tasks_kit`, `threads_kit`) and the Ethlambda
protocol-stub pattern it replaces (`ethlambda_pattern`). `linearizability`
validates the history checker (`spawned_test::linearizability`) against known
register, key-value and FIFO-queue histories (stale read, lost write,
read-your-write, new-old inversion, pending operations that must or may have
taken effect), a brute-force reference on 512 generated histories, generated
sequential histories that must always pass, and a 5,000-operation history; run
`cargo test --release -p spawned-test --test linearizability -- --ignored --nocapture`
for the scale table. It has no controlled
scheduler: ordering exploration and replay (roadmap ranks 20-22) are later work
in the same crate.

## How spawned is verified

No single tool shows that an actor runtime is correct. Spawned uses layers, and
each one catches bugs the others cannot:

1. **Design.** TLA+ models of supervision (`Supervision`), restart cohorts
   (`Cohorts`), supervisor trees (`NestedSupervision`) and ordered system
   shutdown (`Coordinator`). TLC explores every reachable state of a small
   configuration and checks the invariants: at most one live generation, the
   restart budget is never exceeded, shutdown runs in reverse order, nothing is
   admitted after close. Apalache then proves the safety invariants
   *inductive*: every step from any state that satisfies them keeps them, so
   the result covers all states of a configuration size, with no depth bound.
   Every proof has negative controls (deliberately broken models, including
   real bugs we fixed) that must fail, so a check cannot pass vacuously.
2. **Model to code.** Tests record lifecycle traces from the real Rust code and
   TLC checks that each trace is a behaviour the model allows. A trace the
   model rejects means the code and the design disagree.
3. **Interleavings.** Loom explores every thread interleaving of small
   primitives (cancellation, ready gate, wakeups) under the Rust memory model.
   The deterministic simulation backend runs whole tests on a seeded,
   single-threaded executor with virtual time; every scheduling choice is
   recorded, so a failure replays exactly from its seed and shrinks to a
   minimal schedule. Besides random schedules it can search with PCT and
   bounded depth-first exploration. ThreadSanitizer and single-CPU runs cover
   the real multithreaded runtime.
4. **Faults.** Failpoints inject panics, errors, delays and yields at 16 named
   sites in spawned's own lifecycle code. The panic-safety suite panics in
   every user callback in turn (including `Drop`, tracing subscribers and
   factories); the cancellation-safety suite drops every public future at
   every await point; LeakSanitizer and leak assertions check that nothing
   outlives shutdown.
5. **Behaviour.** Proptest generates crash, exit and shutdown scripts for every
   supervision strategy. Metamorphic tests run one workload on every backend
   and require the same observable result. A Porcupine-style linearizability
   checker (`spawned_test::linearizability`) checks concurrent request
   histories of about 1,000 operations against a real actor. Coverage-guided
   [scenario fuzzing](#scenario-fuzzing-fuzz) generates whole programs
   (actors, links, monitors, timers, supervisor trees, system shutdowns) on
   the simulator and checks the semantics contract after every run.
6. **Code and tests.** Kani proves small functions (backoff, jitter, the
   restart window, recovery ids) free of overflow and panics for all inputs in
   a bound. `cargo-mutants` breaks the code on purpose and requires some test
   to fail. Size and stack-depth budgets catch handle, future and stack
   growth. Every crate is `#![forbid(unsafe_code)]`, so the compiler rules out
   undefined behaviour and Miri is not needed yet. `cargo-semver-checks`,
   `cargo-hack`, `cargo-deny` and `cargo-audit` guard the public API, feature
   combinations and dependencies.
7. **Real workloads.** Client compatibility CI ([details](#client-compatibility)) builds and tests
   Ethrex and Ethlambda against spawned changes, and canary nodes run both clients on the
   Hoodi testnet, comparing shutdown time, restarts, saved state and resource
   use with the clients' main branches, including runs that kill supervised
   actors on purpose.

The [semantics contract](design/semantics.md) maps each guarantee to the layer
that checks it. In short: TLA+ and Apalache check the design, trace validation ties the code to
the design, simulation and Loom cover interleavings, failpoints and the safety
suites cover faults, Kani and mutation testing harden functions and tests, and
the clients and canaries check real use. A bug fix always comes with a test
that reproduces it ([reproducing-test rule](#reproducing-test-rule)).

## Verification layers

Status of each layer. "Planned" items are tracked in the
[roadmap](../ROADMAP.md#database-grade-testing-program).

| Layer | Tool | Catches | Status |
|---|---|---|---|
| Design | TLA+ and TLC | Holes in lifecycle, restart and shutdown rules | `Supervision`, `Cohorts`, `NestedSupervision` and `Coordinator` models in `verification/tla`, run in CI |
| Design, unbounded depth | Apalache inductive invariants | An invariant that a state TLC never reached would break | Safety invariants of all four models proven inductive at fixed sizes, with negative controls; `tla-apalache` CI job |
| Design, any size | TLAPS or Veil | An invariant that fails only for larger configurations | Planned ([verification tools](research/verification-tools.md)) |
| Spec and code | Trace validation | The implementation doing something the model forbids | Real OneForOne, cohort and nested traces plus invalid-trace and model-mutation controls |
| Spec and code coverage | Trace coverage (`verification/tla/trace_coverage.py`) | Model actions, states and transitions no test ever reaches | Report on every PR (actions covered by the dedicated trace tests may not regress); whole test suite weekly. See [trace coverage](#trace-coverage) |
| Primitives | Loom | Atomic and memory-ordering bugs in cancellation, ready gate and wakeups | In CI; expand to reply slots and mailbox publication versus shutdown |
| Whole runtime, deterministic | Simulation backend for `spawned-rt::tasks` (random, PCT, bounded DFS) | Async races, replayed from a seed with virtual time | In CI (`sim` job, [simulated suites in CI](#simulated-suites-in-ci)) |
| Resource stability | Soak harness (`concurrency/benches/soak`) | Memory, file descriptors, threads, tasks and registry entries growing over hours of actor churn and restarts | Run by hand before each release, never in CI ([soak harness](#soak-harness)) |
| Behaviour drift | Behaviour snapshots | Silent changes to lifecycle traces, shutdown reports and exit reasons in about 30 standard scenarios | Planned |
| Model-derived tests | Tests generated from the TLA+ models | Interesting model states (a crash during backoff while shutdown starts, a restart racing a deadline) that no hand-written test reaches | Planned, after trace coverage |
| Time | Clock-jump tests | Timers and backoff misbehaving on paused time or large forward and backward clock jumps | Planned |
| Continuous runs | Nightly runs on a dedicated server | Bugs that need hours of simulator seeds, fuzzing, failpoint schedules and soak to appear | Planned (on hold) |
| Generated scenarios | Coverage-guided fuzzing on the simulator (`cargo-fuzz`, `arbitrary`) | Bugs in operation sequences nobody wrote a test for, each replayable from a seed | Planned, after the simulation stack |
| Whole runtime, randomized | Shuttle | Scheduling races in multi-actor tests too large for Loom | Not started |
| Memory safety | ThreadSanitizer | Data races | In CI; Miri waits until unsafe code is proposed (all crates `#![forbid(unsafe_code)]`) |
| Scheduling pressure | `taskset -c 0`, one Tokio worker | Code that assumes parallel progress; races that need preemption on one core | `single-cpu` CI job |
| Rare lifecycle paths | Failpoints (`BUGGIFY`-style) | Restart, shutdown, placement and registration windows reached on purpose | `failpoints` feature, compiled out by default ([failpoints](#failpoints)) |
| Callback panics | Panic-safety suite | A panic in user or foreign code (handlers, `Drop`, subscribers, factories) losing the exit reason or skipping cleanup | In CI ([cross-backend, cancellation and panic safety](#cross-backend-cancellation-and-panic-safety)) |
| Cancellation | Cancellation-safety suite | State left inconsistent when a public future is dropped at an await | In CI |
| Leaks | LeakSanitizer, `leaks.rs` | Actors, threads, tasks, registry entries and handle cycles that outlive shutdown | LSan job beside TSAN; leak assertions in every test run ([CI hardening](#ci-hardening)) |
| Behaviour | Proptest supervisor scripts | Wrong outcomes for generated crash, exit and shutdown sequences | In CI ([generated supervisor policy scripts](#generated-supervisor-policy-scripts)) |
| Cross-backend | Metamorphic tests | Backend-specific differences in replies, exits and per-sender order | In CI |
| Request semantics | Linearizability checker (`spawned_test::linearizability`) | Lost writes, stale reads and reordering in concurrent request histories | In CI |
| Full functional proofs | Verus or Creusot | Restart budget, backoff window and generation logic wrong for inputs beyond Kani's bound | Later, once the APIs settle |
| Pure arithmetic | Kani | Overflow, panics and off-by-one in backoff, jitter, the restart window and recovery ids, for all inputs in a bound | `concurrency/src/kani_proofs.rs`, weekly and on `concurrency/src` PRs |
| Test quality | cargo-mutants | Tests that still pass when the code is broken | Weekly over supervision and system code |
| Resource budgets | `future_layout`, `stack_budget` | Growth of handles, futures and lifecycle stack depth (a real client stack overflow before v0.7.1) | Sizes in CI; stack budgets in review (PR #220) |
| Compatibility | cargo-semver-checks | Unexpected public API removals | In CI |
| Macro misuse | trybuild compile-fail tests | `#[protocol]` and `#[actor]` misuse that compiles or gives an unclear error | In CI on the pinned toolchain ([macro diagnostics](#macro-diagnostics)) |
| Feature separation | cargo-hack | Missing dependencies hidden by dev-dependency feature unification | CI builds each library feature, default, no-default and all features without dev dependencies |
| Supply chain | cargo-deny, cargo-audit | Vulnerable, unmaintained or unsound dependencies; license and source drift | `supply-chain` CI job; accepted advisories listed in `deny.toml` |
| Flakiness | cargo-nextest with retries | Tests that pass only on retry | Nightly `flaky-tracking` job; a retry is reported, never silently passed |
| Client compatibility | Ethrex and Ethlambda builds and tests against spawned | Changes that break real users (API, behaviour, stack depth) | `client-compat` workflow: nightly, push to main, `client-compat` PR label ([details](#client-compatibility)) |
| Production | Hoodi canaries; lifecycle monitors derived from the TLA+ invariants | Regressions in real nodes; a live run breaking a modelled invariant | Canaries run manually; monitors planned ([roadmap](../ROADMAP.md) rank 5c) |
| Production, under pressure | Canary faults: cgroup memory and CPU limits, `tc netem` or toxiproxy network faults, clock skew | Restart and shutdown bugs that appear only under resource pressure | Planned |
| Performance regressions | Benchmark regression gate; dhat allocation budgets; open-loop load (HdrHistogram) | Per-message allocation growth; tail latency hidden by coordinated omission | Planned ([test techniques](research/test-techniques.md)) |
| Platforms | Linux and macOS, x86 and arm64 CI jobs | Ordering, affinity and atomics assumptions tied to one platform | Planned ([test techniques](research/test-techniques.md)) |
| Distribution | turmoil, Jepsen-style histories | Loss, delay, partitions and reordering between nodes | Only with a remote transport ([roadmap](../ROADMAP.md) rank 22a) |

The fuller rationale is archived in the
[verification strategy note](../legacy/docs/research/verification-strategy.md).

### API and feature checks

```sh
python3 scripts/check-api.py features
python3 scripts/check-api.py semver --baseline v0.6.0
```

Install cargo-hack 0.6.46 for the first command. CI pins cargo-semver-checks
0.51.0 and its Rust 1.99.1 rustdoc producer together through Nix for the second;
see `.github/workflows/ci.yml`. The feature matrix uses Rust 1.93.0. It refreshes
an isolated copy at `target/feature-check/source` because cargo-hack's
`--no-dev-deps` rewrites manifests and the lockfile. Builds use the stable
`target/feature-check/build` directory, separate from ordinary Cargo builds.
A nonblocking POSIX file lock prevents concurrent checks from modifying the same
copy; Linux and macOS are supported. A custom `CARGO_TARGET_DIR` relocates this
whole feature-check subtree. The baseline policy is validated before any builds.
The semver tool handles `--baseline-rev` directly; no Python 3.12 tarfile APIs are
used. The script is compatible with Python 3.9+.

Defaults and metrics must pass the strict patch policy against v0.6.0. The
no-default configuration is intentionally breaking in v0.7: CI also compares it
with and without metrics, and requires exactly the reviewed missing-item list in
`verification/api/v0.6.0-no-default.json`. New removals, missing diagnostics and
tool/build failures fail the job. It separately requires the documented migration
(`default-features = false, features = ["async"]`) to pass the strict comparison.
This does not make raw no-default builds patch-compatible; see the
[migration guide](MIGRATION.md#cargo-features-and-protocol-attributes).
Negative controls exercise the narrow exception. These checks do not prove
behavioral, macro expansion or every generic API interaction; native integration
and dependency-graph checks remain separate.

Protocol expansion regressions cover `Debug` and custom supertraits, direct and
configured handles, handwritten fakes, cfg methods and rejected conditional
timeouts, plus owner-crate `Box<T>`/`&T` forwarding implementations of protocols
and converters. Public impls remain concrete ActorRef/configured-ActorRef types;
shared private transport helpers do not introduce an uncovered blanket impl.
A real two-crate contract/consumer fixture also checks concrete and
generic downstream fakes and `where Self: Debug`; a same-crate compile test alone
would not establish that coverage.

```sh
python3 -m unittest discover -s scripts/tests
cargo test --locked -p spawned-concurrency --test protocol_compile --test protocol_timeouts
cargo test --locked -p spawned-capability-examples --test cross_crate_protocol
```

### Generated supervisor policy scripts

`supervisor_policies` runs up to four children through generated crash, normal
stop, explicit shutdown and tree-close scripts. An independent generation and
retirement oracle checks all restart strategies and modes, collateral Temporary
retirement, fresh factory generations and no resurrection after retirement or
closure. Successful request fences verify the generation actually serving work.
A fixed case keeps retired Transient/Temporary slots across later recoveries.
Every child is inspected before waiting on existing generation notifications;
a negative control keeps a stalled earlier child from hiding a later extra
reconstruction. Typed generated actions and policies shrink directly.

This generated suite uses zero backoff and a high, unexhausted restart budget;
it does not claim budget-exhaustion or shutdown-during-backoff coverage.
Those are controlled cases in `supervision` (`restart_budget_is_shared_between_siblings_and_exhaustion_closes_tree`,
`shutdown_during_backoff_prevents_new_factory_invocation`) and
`supervision_strategies` (`cohort_reconstruction_consumes_shared_factory_attempt_budget`).

```sh
cargo test --locked -p spawned-concurrency --test supervisor_policies
PROPTEST_CASES=512 PROPTEST_RNG_SEED=20261109 cargo test --locked -p spawned-concurrency --test supervisor_policies
```

The default is 32 generated scripts; the extended workflow runs 1,000 and TSAN
runs eight. Inputs shrink and failure seeds persist through proptest. Each
action waits for a settled boundary; this suite does not control Tokio/OS
scheduling or replace the concurrent recovery/shutdown tests and trace models.
Real client recovery under fault injection remains a separate application rollout gate.

## Semantics coverage

Audit of every guarantee in the [semantics contract](design/semantics.md)
(2026-11-11, PR #246). A guarantee counts as **covered** when every clause has
a check that fails if the clause breaks; a test that only runs the path, a
sleep-bounded "nothing happened" window or an example that prints does not
count. **Partial** means some clause is only exercised or holds by
construction. The contract's "Checked by" lines name the checks.

Modes: A `Backend::Async`, B `Blocking`, T `Thread`, N threads mode,
G `async_group`, TG `thread_group`, P `pinned_group` (Linux), Sim the
deterministic simulator (seed sweeps, or PCT and bounded DFS where marked),
Fz the scenario fuzzer's oracles (simulator, Async and async groups only),
Model TLA+/Apalache/Kani.

Totals: 50 guarantees. Before this audit 31 covered, 18 partial, 1 unchecked
(D5, only a nightly `#[ignore]` stress test). After: 46 covered, 4 partial;
with the follow-up PR #255 (T4 threads clocks), 47 covered, 3 partial.
Mutation checks on the new tests: removing the run loop's stop-before-receive
check fails `sim_ordering::stop_discards_*` and `tasks_contract::stop_window_*`;
giving collateral children their own backoff fails
`supervision_contract::collateral_children_add_no_backoff` (600 ms instead of 200 ms).

| Id | Strongest checks | Modes | Before → after | Still unchecked |
|---|---|---|---|---|
| M1 | `request_histories` linearizability; `*_contract::hooks_and_turns_*` (read-modify-write across an await); `sim_ordering::turns_*` | A B T N G TG, Sim (PCT, DFS) | partial → covered | |
| M2 | `semantics_gaps::*::exit_and_down_follow_*` (`Down`/`Exit` behind messages) | A B T N | covered | no priorities: by construction |
| M3 | `cross_backend` (independent oracle per workload) | A B T N G TG P, Sim (A, G) | covered | Sim skips fan-out and monitor workloads |
| M4 | `supervision::one_for_one_uses_fresh_state_*`; `*_contract::actor_ids_are_never_reused` | A B T N G TG, Sim | partial → covered | |
| M5 | `*_contract::dropping_every_actor_ref_keeps_the_actor_running`; `leaks` | A B T N G TG | partial → covered | |
| D1 | `actor_scenarios` fences; `cross_backend`; `sim_ordering::per_sender_fifo_*`; Fz | A B T N G TG P, Sim (PCT, DFS) | covered | |
| D2 | `actor_scenarios` exact logs; `request_histories`; Fz | A B T N, Sim | covered | |
| D3 | `*_contract::stop_window_*` (sends after the stop and from `stopped()` are `Ok`, never handled); `mailbox_admission_loom` | A B T N G TG, Loom | partial → covered | |
| D4 | threads/tasks unit tests; `mailbox_drop_panic`; `*_contract::stop_window_*` and `sim_ordering::stop_discards_*` (payloads dropped and reply senders closed when `join()` returns) | A B T N G TG, Sim (PCT, DFS) | partial → covered | |
| D5 | `*_contract::unbounded_mailbox_*` (100,000 sends behind a blocked handler) | A B T N G TG | unchecked → covered | |
| R1 | `actor_scenarios`; `failure_windows` (all three outcomes); Fz | A B T N, Sim | covered | `RequestTimeout` not on groups |
| R2 | `actor_scenarios`; `sim_ordering::per_sender_fifo_*`; Fz | A B T N, Sim (PCT, DFS) | covered | |
| R3 | `protocol_timeouts` precedence; `tasks_contract::default_request_timeout_*` (4.9 s Ok, 5.1 s timeout, no timer before first poll) | A N, Sim | partial → covered | send-only methods: by type (no `Response`) |
| R4 | `cancellation_safety`; `request_histories` pending ops | A B T G TG, Sim | covered | |
| R5 | `semantics_gaps::*::panicking_request_handler_*`; `cross_backend` | A B T N G TG P | covered | |
| L1 | `sim_ordering::turns_*`; `*_contract::hooks_and_turns_*`; Fz | A B T N G TG, Sim (PCT, DFS) | covered (A only) → covered | |
| L2 | `startup_admission`; `execution_groups::failed_startup_*`; `tasks_contract::managed_readiness_*` (historical `ready()`) | G TG, Sim (G) | partial → covered | |
| L3 | `context_exit` (first reason wins); `sim_ordering::turns_*` (stop never splits a turn) | A N, Sim | partial | cleanup panic after a committed reason; owned by #240 (rank 5e) |
| L4 | `exit_signal_matrix`, `context_exit`, `panic_safety` | A B T N G TG | covered | owned by #240 |
| L5 | `foreign_panics` (strict); `tasks_contract::panics_in_hooks_*` | A B T N G TG | covered (B, T unchecked) → covered | |
| L6 | `mailbox_drop_panic` (5 placements x 1/2/256), `native_mailbox_drop`, lifecycle traces | A B T N G TG | covered | not simulated (subprocesses) |
| L7 | `shutdown_publication`; `*_contract::stop_window_*`; `sim_ordering::stop_discards_*` | A B T N G TG, Sim | partial | outgoing monitors cancelled before publication (after #240) |
| X1 | `actor_scenarios` monitor races; `exit_signal_matrix`; Fz | A B T N, Sim | covered | owned by #240 |
| X2 | unit `demonitor_*`, `closed_monitor_table_*`; `leaks` | A N | covered | demonitor checks use a bounded window |
| X3 | unit link tests; `exit_signal_matrix`; Fz | A N, Sim | covered | owned by #240; threads lacks the Normal and chain variants (after #240) |
| X4 | `kill_links` | A N | covered | owned by #240 |
| X5 | unit link tests; `link_regression_tests` | A N | covered | owned by #240 |
| X6 | `link_regression_tests` | A | covered | owned by #240 |
| X7 | `semantics_gaps::*::exit_and_down_follow_*`; `sim_ordering::exit_and_down_follow_the_last_message_*`; Fz | A B T N, Sim (PCT, DFS) | partial | a non-trapping linked stop after the current handler (exit signals, #240) |
| V1 | `supervisor_policies` oracle; `supervision::restart_modes_*` (real `Normal`, `Shutdown` and panic exits); `Supervision!RestartFollowsPolicy`, `TemporaryStartsOnce` (`MCSupervisionTemporary`) | G, Sim, Model | covered | |
| V2 | Cohorts invariants; `supervision_strategies`; `cohort_trace`; `supervision_contract::concurrent_failures_coalesce_*` | G, Sim, Model | covered | reverse stop order only via trace conformance |
| V3 | `*!AtMostOneGeneration`; factory asserts no live generation; Fz | G TG, Sim, Model | covered | |
| V4 | `*!PublishedIsLive`; `supervision_contract::queued_messages_are_not_replayed_*`; Fz | G, Sim, Model | partial → covered | |
| V5 | `Supervision!BudgetBounded`; Kani; budget tests | G, Sim, Model | covered | owned by #240 (rank 5f) |
| V6 | Kani backoff harnesses; `backoff_exponent_resets_*`; `supervision_contract::collateral_children_add_no_backoff` | G, Sim, Kani | partial → covered | jitter only by Kani and unit tests |
| V7 | `Supervision!ReadyMeansAllStarted`; startup tests | G, Sim, Model | covered | owned by #240 (rank 5f) |
| V8 | `NestedSupervision` invariants; `nested_supervision`; `tree_trace` | G TG, Sim, Model | covered | |
| V9 | Supervision invariants; failpoints; `supervision_contract::message_backlog_does_not_delay_restart`; Fz | G TG P, Sim, Model | partial → covered | |
| S1 | `Coordinator!Safety`; `execution_groups`; `execution_profiles` (`Capacity`); `supervision_contract::admission_is_closed_*` | G TG, Sim, Model | partial → covered | |
| S2 | `Coordinator!OrderedShutdown`; `system_shutdown_orders_*`; `cross_backend::ordered_system_shutdown_matches` | G TG P, Sim, Model | covered | |
| S3 | `Coordinator!Safety`, `CoordinatorInd` steps; `execution_groups`; `supervision_contract::dropping_a_system_shuts_down_*` (non-blocking drop, 5 s grace) | G, Sim, Model | partial → covered | |
| S4 | `src/system.rs::coordinator_fault_*`; failpoints | G, Model | covered | not simulated (failpoints) |
| S5 | `execution_groups`, `supervision`, `src/registry.rs` tests; Fz | G, Sim | covered | |
| S6 | `execution_groups::ambient_actor_*`; `tasks_contract::unmanaged_tasks_and_threads_*` | G | partial → covered | |
| T1 | `semantics_gaps::tasks::send_after_fires_once_*` (paused time); `resource_cleanup`; `cross_backend::timers_match` | A B T N G TG P | covered | |
| T2 | `semantics_gaps::tasks::send_interval_is_fixed_delay_*` (paused time) | A | covered | threads by construction |
| T3 | `semantics_gaps::tasks::timers_are_not_carried_*`; `tasks_contract::managed_timers_run_on_the_group_*` | G | partial → covered | `send_message_on` in threads |
| T4 | paused-time tests and the simulator clock; `src/threads/timer_tests.rs::threads_send_*`; `supervision::native_restart_window_expires_on_the_monotonic_clock` | A N TG, Sim | partial → covered | |
| P1 | `panic_safety`; `foreign_panics`; `mailbox_drop_panic`; failpoints | A B T N G TG | covered | not simulated |
| P2 | `panic-abort` CI job; `foreign_panics::panicking_panic_hook_*` | A N | covered | no observer for `Down`/`Exit` |

Bugs found: none. Every gap test passed on the first correct run in both
builds; the two initial `sim_ordering` failures were test errors (a stop
before the burst was dequeued, which D4 discards, and a busy-wait that PCT
starved), fixed in the tests.

Citation fixes made by the audit: L2 cited `ready_gate_loom`, which checks the
benchmark scheduler's gate rather than readiness; L1 cited `cross_backend`,
which records `stopped()` only for shutdown order; S1 cited a supervisor
`max_children` test for `thread_group` capacity; the `Coordinator!*Step`
properties live in `CoordinatorInd.tla` (Apalache); T1 named a threads-only
test for tasks. Fixed in PR #255:
`supervision::restart_modes_distinguish_normal_shutdown_and_panic` used
`shutdown()` for its "normal" branch (it now stops with `Normal`, and fails
if a Transient child restarts after `Normal`); `Supervision!TemporaryStartsOnce`
was vacuous in the per-PR `MCSupervision.cfg` (no Temporary child), so the
per-PR `MCSupervisionTemporary.cfg` (a Temporary and a Permanent child) now
checks it, with a `negative_checks.py` mutation that restarts a Temporary
child after a panic and must violate it.

```sh
cargo test --locked -p spawned-concurrency --test semantics_gaps --test sim_ordering
RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" CARGO_TARGET_DIR=target/sim \
  cargo test --locked -p spawned-concurrency --test semantics_gaps --test sim_ordering
```

## Controlled failures, histories and simulations

| Suite | Evidence provided | Limit |
|---|---|---|
| `failure_windows` | Nine deliberately reached lifecycle/request windows across async, blocking, dedicated-thread and synchronous-thread actors; optional `SPAWNED_FAILURE_REPORT` records coverage | A bounded demonitor observation is not exhaustive proof over schedules |
| `request_histories` | Porcupine-style linearizability checking (`spawned_test::linearizability`) of real-actor histories: the append-log worker across four backends (about 50 operations each), and PropEr-style `parallel_commands` programs (sequential prefix, then 2-4 concurrent branches, up to about 1,000 operations with zero-timeout pending requests) against a key-value/queue store actor in tasks (`Async`, `Thread`) and threads modes. Negative controls: corrupted observed histories, and store actors with injected lost-write, stale-read and reorder bugs, caught with and without a tripwire prefix. The old bounded oracle survives only to show the new checker agrees with it on 4,000 generated histories it can decide | One OS/Tokio interleaving per run, not replayed; per-partition search is exponential in the worst case, and an exhausted budget is `Unknown` and fails, not a pass |
| `delivery_simulation` | Enumerate 125 states of a finite cooperative-stop model and replay 500 witnesses across four backends, plus seeded scripts and an audit digest | Controls handler-progress gates, not arbitrary OS/Tokio scheduling, network, clocks or all lifecycle signals |
| `payload_stress` | Unique owned payloads from 128 concurrent producers; verify FIFO delivery or complete destruction on stop | Explicitly bounded payload volume; not a whole-process memory proof |
| `process_failures` (Unix) | Abort or kill an owned subprocess after an actor effect; check process failure does not promise cleanup callbacks or rollback | Temporary marker is not a storage durability acknowledgement |
| `benchmark_startup` | Startup rendezvous opens for all participants, aborts after setup failure and has a finite timeout | Tests the research driver, not production actor supervision |

```sh
cargo test --locked -p spawned-concurrency --test failure_windows --test request_histories --test delivery_simulation --test process_failures --test benchmark_startup
SPAWNED_HISTORY_CASES=128 SPAWNED_HISTORY_SEED=20261106 SPAWNED_LINEARIZABLE_CASES=256 cargo test --locked -p spawned-concurrency --test request_histories -- --nocapture
SPAWNED_SIM_CASES=256 SPAWNED_SIM_SEED=20261106 cargo test --locked -p spawned-concurrency --test delivery_simulation -- --nocapture
SPAWNED_STRESS_PRODUCERS=128 SPAWNED_STRESS_MESSAGES=256 SPAWNED_STRESS_PAYLOAD=1024 cargo test --locked -p spawned-concurrency --test payload_stress -- --ignored --nocapture
```

A timeout, dropped caller or closed reply after dispatch has an **unknown
effect outcome**. The history oracle explores both effect/no-effect possibilities
without pretending the caller timeout ended the operation. A proven enqueue
rejection permits no effect. Normal successful replies require their matching
effect; the checker never treats ambiguous operations as acknowledged success.
No automatic retry or exactly-once guarantee is introduced.

The Linux campaign driver runs isolated owned process groups with CPU affinity,
nice level, wall-time, sampled RSS, thread and descriptor budgets. Builds have a
separate memory allowance from stress executables. It kills only its own process
group on failure and writes commands, source revision, environment and resource
peaks. The default 768-thread ceiling accommodates 256 offered-load producers
and their collectors; they still share the declared CPU mask. Shared-page RSS
can be counted more than once and short peaks can escape sampling.

```sh
python3 benchmarks/run_reliability.py --source . --out /tmp/spawned-reliability --cpus 2,3,4,5 --phase all --seconds 30 --replicates 3
```

Use an available CPU set appropriate to the machine. The benchmark phase records
closed-loop requests, actor-to-actor graphs, concentrated offered load and actor
churn/idle soaks. Benchmarks describe their generator as well as actor work; high
producer counts do not imply that many actor workers. See the
[recorded campaign](../legacy/research/reliability/2026-11-06/README.md) for results,
excluded preflights and reproducibility details. No throughput threshold is
enforced in shared CI. Nightly CI retains failure coverage and stress logs.

The suite generates 2–8 worker actors and an observer, with sequences of sends,
requests, and dropped reply receivers. Every generated scenario runs on Tokio
async, Tokio blocking-pool, dedicated-thread, and synchronous-thread backends.
It then races monitor registration and requests against clean shutdown or a
handler panic. The fixed regression case exercises every termination mode.

## Contract checked

- A request fence observes every earlier message from the same sender in order,
  exactly once. Dropping a reply receiver does not cancel queued work.
- A request racing shutdown either returns its matching reply or ActorStopped.
  Accepted sends alone do not guarantee execution across termination.
- Termination resolves within a five-second test watchdog. Post-exit requests
  return ActorStopped.
- Each uncancelled monitor observes the matching exit reason, including when
  registration races termination. Unknown or duplicate observed notifications
  fail the test. Clean shutdown and injected handler panic remain distinct.

The reference model is the per-actor sequence of submitted message IDs. During
shutdown it permits the final racing message to execute or be discarded; a
successful reply requires execution. It does not impose cross-sender ordering.

## Replay and shrinking

Proptest prints and shrinks failing inputs and persists failure seeds alongside
this integration test (`actor_scenarios.proptest-regressions`). Retain that file
and the test output; commit useful regressions. A minimized input can also be
added as a fixed `scenario(...)` invocation in `lifecycle_race_regression`.

Run a larger campaign:

```sh
PROPTEST_CASES=1000 cargo test -p spawned-concurrency --test actor_scenarios
```

Set `PROPTEST_RNG_SEED` to a reported seed to regenerate a campaign. A seed
reproduces input generation, **not the operating system or Tokio schedule**.
Repeat a minimized case to investigate scheduling-dependent failures. Nightly
CI retains regression files on failure and has an outer job timeout to catch
hangs that prevent an in-process watchdog from making progress.

### Seeds and decision tapes under `--cfg spawned_sim`

`RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable"` runs async tests on the
deterministic simulator in `spawned_rt::tasks::sim` (one thread, seeded choice
of the next ready task, virtual time). `tokio_unstable` is required: the
simulator seeds Tokio's thread-local RNG (`select!` branch order, `watch`
waiter shards) per run, and Tokio only exposes that seed under its own cfg; a
build with `spawned_sim` alone fails with a `compile_error!`.

Tests written with `#[spawned_macros::sim_test]` (or
`spawned_rt::tasks::sim_test`) run once on Tokio in normal builds and over
seeds `0..SPAWNED_SIM_SEEDS` (default 8) under the cfg. A failure prints the
seed, a `SPAWNED_SIM_SEED=n` replay command (carrying `SPAWNED_SIM_SPIN`,
`SPAWNED_SIM_MAX_STEPS`, `SPAWNED_SIM_FAIR_AFTER` and
`SPAWNED_SIM_TURN_BUDGET` if they were set), and the path of a decision tape
with a `SPAWNED_SIM_TAPE=path` replay command. The tape records the seed, the
spin limit and every pick and clock jump. Tapes replay by task id, so they
survive changes in random-number consumption. Replay is strict: clock jumps
come from the tape, and a pick whose task is not ready, a tape that runs out,
or decisions left over (also after a failure, and for a bare
`Runtime::replay`) fail the run as `replay diverged` rather than running a
different schedule. Each clock jump must stay between the current time and
the earliest pending timer (and equal that timer when nothing is ready), and
each canonical pick is checked against the task id the tape recorded. The harness also writes a `.min.tape` shrunk toward the
canonical schedule (`SPAWNED_SIM_SHRINK=0` disables shrinking); the shrinker
guides candidates permissively but keeps one only if its recorded tape fails
the same way under strict replay. Library time (restart windows, backoff
jitter seeding, shutdown deadlines, metrics) reads `spawned_rt::time`, which is
virtual under the cfg.

**Actor turn budget (rank 8b).** Under the cfg, a run fails when one actor
turn runs longer than `SPAWNED_SIM_TURN_BUDGET` milliseconds of wall-clock
time inside a single poll (default 250; `0` disables the check;
`sim::set_turn_budget` / `Runtime::set_turn_budget` set it per run). That is
a handler that blocks the thread (`std::thread::sleep`, a blocking lock or
I/O call) or computes in a loop with no `.await`: on Tokio it holds a worker
and every actor queued behind it; on the BEAM the scheduler would have
preempted it. The failure names the actor and the message type, prints the
measured time on stderr, and carries the usual seed and tape replay lines:

```text
spawned_sim: turn over budget: actor `sim_turn_budget::Worker` handling
`sim_turn_budget::Nap` ran for more than 250 ms in one poll without yielding
(a blocking call or a loop with no .await stalls every actor on its worker;
SPAWNED_SIM_TURN_BUDGET=ms sets the budget, 0 disables it)
```

The actor dispatch loop marks each turn (a `#[cfg(spawned_sim)]` guard in
`tasks/actor.rs`); the executor times only the stretches of a poll that run
inside a turn and keeps the turn of a task suspended at an `.await`, so a
poll that handles several messages charges each turn separately and the
budget applies to each stretch between awaits, not to the whole turn. A
handler that awaits (including long virtual sleeps) never trips it. Only
`Handler::handle` turns are timed, not `started`/`stopped` or plain tasks.

Why wall-clock time, given that the rest of the simulator is deterministic:
one thread cannot preempt a poll, so a blocking or spinning section can only
be measured after it returns, and nothing a poll does is countable without
instrumenting user code (the BEAM counts reductions; Rust has no equivalent).
The deterministic alternatives miss the problem: counting polls of a turn
that keeps waking itself (it yields every time, so it starves no one beyond
what the seeded scheduler already explores; the spin limit and
`SPAWNED_SIM_MAX_STEPS` already bound it), or counting how many scheduling
steps other ready tasks wait (each step is one poll, so a blocking poll is a
single step). Timing never changes a schedule: it only decides whether the
run fails. What it measures is a property of the code, so the default is
generous (thousands of times Tokio's guidance of 10 to 100 µs between
awaits, 25 times the 10 ms debug-build `WarnOnBlocking` warning) and a failing seed or tape
reproduces it; an overloaded machine or a debugger pause can still trip it,
in which case raise the budget. A production-side watchdog for the same
handlers is rank 8, cooperative budgets rank 8a.

Other `#[test]`s in a simulated binary that build a runtime with
`spawned_rt::tasks::Runtime::new()` run on the simulator too, but only with
seed 0 (or `SPAWNED_SIM_SEED`); they are not seed sweeps.

**What the simulator does not model.** Determinism and replay hold only within
these limits:

- Everything a task does inside one poll is atomic: there is no OS preemption
  between two statements without an `.await`, and no weak-memory reordering.
  Races that need one (the PR #209 readiness/gate race) are invisible.
- Tokio's cooperative budget is emulated (each poll runs inside a per-run
  Tokio context, so a task is forced to yield after 128 ready channel
  operations, as on Tokio), but Tokio's timer, I/O and blocking pools are not:
  a direct `tokio::spawn` fails the run, and direct `tokio::time` use panics.
- Timers fire only when no task is ready, or after `SPAWNED_SIM_SPIN` busy
  polls (default 10,000) when the clock creeps by 1 ms; every 100 creeps
  without an idle moment that threshold halves, down to one poll per
  millisecond. A real clock would fire timers at wall-clock moments in the
  middle of busy work, at a rate unrelated to poll counts.
- One thread: Thread, Blocking, native and pinned placements, dedicated
  runtimes and `spawn_blocking` do not run. A poll with no await point that
  never returns trips a wall-clock watchdog (`SPAWNED_SIM_POLL_TIMEOUT`,
  default 60 s) that aborts the process; one that returns after more than
  `SPAWNED_SIM_TURN_BUDGET` (250 ms) inside an actor turn fails the run.
- Direct `std::time`/`SystemTime` reads, `std::thread`, Tokio I/O and
  direct Tokio timers are outside its control.
- Remaining nondeterminism between runs in one process: actor, monitor and
  registration ids come from process-global counters (their order within a
  run is fixed, their values depend on earlier tests), and the name
  registry, failpoint configuration and their counters are process-global
  state shared by every world. `spawned_rt::time::Instant` read on a thread
  with no live simulation falls back to wall-clock time. The poll watchdog is
  wall-clock based: a debugger pause or an overloaded machine can trip it
  (it only ever aborts, never changes a schedule); `SPAWNED_SIM_POLL_TIMEOUT=0`
  disables it. The actor turn budget is wall-clock based too and can fail a
  run on such a machine; `SPAWNED_SIM_TURN_BUDGET=0` disables it. `registry::registered()` returns names sorted, so its order
  does not depend on the hash map's random state. Also wall-clock based:
  the experimental time slice of `backend-experiments` actors
  (`std::time::Instant`) and `FailScenario::setup()` without
  `SPAWNED_FAILPOINTS_SEED` (seeded from `SystemTime`; use `with_seed`).
  The harness's panic-output filter installs one process-wide panic hook.

Excluded under the cfg (compiled out with `#[cfg(not(spawned_sim))]`, so not
simulated at all): tests that need OS threads or a real Tokio runtime, in
`actor_scenarios`, `backend_resource_limits`, `canary_feedback`,
`cancellation_safety`, `cross_backend` (non-async placements and the fan-out
and monitor workloads), `dedicated_runtime`, `delivery_simulation`,
`execution_groups`, `execution_profiles`, `failure_windows`, `leaks`,
`mailbox_drop_panic`, `nested_supervision`, `panic_safety`,
`process_failures`, `protocol_compile`, `request_histories`,
`resource_cleanup`, `semantics_gaps` (its `tasks` and `tasks_contract` modules), `shutdown_publication`, `stack_budget`, `startup_admission`,
`supervision_strategies` and `supervision`. `leaks` replaces its excluded
async checks with a `sim_leaks` module (actor, cycle, timer, monitor-waiter,
system-actor and helper releases, counted from the simulator's task table over
8 seeds), and `cross_backend` runs each async placement over 4 simulated seeds
and compares all of them. The `testing` crate's kits and the
threads-mode tests run their native threads unsimulated even in a simulator
build.

```sh
RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" CARGO_TARGET_DIR=target/sim \
  cargo test -p spawned-concurrency --test supervision --test sim
```

### Simulated suites in CI

The `sim` job in `.github/workflows/ci.yml` builds with the cfg and runs
every integration test of `spawned-concurrency` and `spawned-rt`, plus
`spawned-rt`'s unit tests (the executor itself), through
`scripts/run-sim-tests.py`: one test per process (`--exact`), four processes
at a time, each killed after 120 s, so one wedged schedule fails that test
instead of hanging the binary. Failing seeds leave their decision tapes in the
`sim-tapes` artifact. The run takes under a minute after the build; the
seed counts are the defaults (8 per `sim_test`, 200 for the `sim.rs`
sweeps). Larger sweeps (`SPAWNED_SIM_SEEDS`, `SPAWNED_SIM_ITERATIONS`, the
ignored `explore_budget_table`) stay manual. Same run locally:

```sh
RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" CARGO_TARGET_DIR=target/sim \
  python3 scripts/run-sim-tests.py -p spawned-concurrency -p spawned-rt:lib --jobs 4 --timeout 120
```

`spawned-concurrency`'s own unit tests (`--lib`) are not simulated suites:
many build real multi-thread runtimes, Blocking or Thread backends, or drop
runtimes mid-run, which the simulator does not model; their `sim_test`s also
run in the normal build.

### Exploration strategies (`sim::explore`)

The random walk can be swapped for a systematic strategy with
`SPAWNED_SIM_STRATEGY` or `#[sim_test(explore = "...", iterations = N)]`
(`SPAWNED_SIM_ITERATIONS` overrides the budget; default 8 runs for random, 100
otherwise):

| Strategy | What it does |
|---|---|
| `random` | Seeded uniform choice among ready tasks, by rank of task id. |
| `pct:d=3[,k=N][,fair=N]` | PCT (Burckhardt et al. 2010) with a poll as the step: random task priorities (drawn in task-id order) and `d - 1` change points `k_1..k_{d-1}` over an estimated run length `k` in polls (default: the longest run so far). The task that runs poll `k_i` then drops to priority `i`, independently of where `k_i` falls in time, as in the paper; a unit test checks this against an independent implementation of the paper's algorithm, with blocking and re-enabled tasks. One seed per run. |
| `dfs:bound=2[,depth=N][,fair=N]` | Stateless depth-first enumeration of every schedule with at most `bound` delays, where a delay permanently demotes the task about to run and zero delays is the canonical schedule. Under `SPAWNED_SIM_SEED=n` every schedule gets input seed `n`. Reports whether the bounded space was exhausted and whether it was truncated. |

Every strategy records into the same decision tape, so a failure replays with
`SPAWNED_SIM_TAPE` and shrinks like a random-walk failure; PCT also prints a
`SPAWNED_SIM_STRATEGY=pct:d=..,k=..,fair=.. SPAWNED_SIM_SEED=n` command with
`k` and the fairness cutoff filled in (rerunning a PCT seed without `k` uses a
different estimate and is a different run). After `SPAWNED_SIM_FAIR_AFTER`
polls (default 20,000, or `fair=N` in the strategy) PCT and DFS runs switch to
a fair rule (least recently polled ready task) so spin-waits cannot starve.
Polls after that cutoff, and DFS polls past `depth`, are not choice points: a
DFS run that had alternatives there reports the space as *truncated*, and the
harness then says "exhausted up to the depth/fairness cutoff" instead of
"exhausted". A DFS run whose recorded prefix does not replay (a body that is
not deterministic under the simulator) fails the test.

No probability bound is claimed for PCT. The paper's `1/(n·k^(d-1))` needs
`k` to bound the steps of every execution and a bug of depth `d` in the
paper's sense; here `k` is an estimate (early runs can underestimate it),
polls after the fairness cutoff are not PCT steps, and a poll is coarser than
the paper's step. That `pct:d=2` or `dfs:bound=1` finds a bug does not
establish its depth either. The counts below are measurements on these
workloads.

There is no partial-order or sleep-set reduction: the executor sees wakers,
not which shared state a poll touches, so DFS counts schedules that differ only
in independent steps. Mailbox delivery has no separate choice point: `send`
enqueues inside the sender's poll, so the order of messages from different
senders at one mailbox is the order of the senders' polls, which the strategies
already choose; per-sender FIFO is never broken.

`concurrency/tests/sim_explore.rs` holds planted ordering bugs between real
actors (lost updates through a store actor's get-then-set, logged by the
store) and the PR #204 never-started-child race. Each planted bug fails with
its own message, and the tests check that message, so a deadlock or unrelated
panic does not count as finding it. Every write is `read + 1` from 0, so all
writes being 1 means every read came before every write: the two-worker bug
needs both reads before either write, and the three-worker bug needs all three
reads before any write (a final value of 1 alone does not:
`A:Get(0) B:Get(0) A:Set(1) C:Get(1) C:Set(2) B:Set(1)`).
`depth3_bug_requires_all_reads_before_any_write` checks that equivalence over
every schedule with at most 3 delays of a smaller instance (1,038 schedules)
and 2,000 PCT runs of the real one. The PR #204 `Stopped` outcome, closed by PR #209, must not be found;
reaching `Failed` is checked only as a sanity check that each strategy drives
the failed placement (the canonical schedule already does), not as evidence
about rare races. Runs to the first failure (release build; hit rate over
10,000 runs in parentheses):

| Bug | random | `pct:d=2` | `pct:d=3` | `dfs:bound=1` | `dfs:bound=2` | `dfs:bound=3` |
|---|---|---|---|---|---|---|
| Both of two workers read before either writes (one worker 12 polls behind) | 384 (0.33%) | 2 (23%) | 8 (17%) | 5 | 51 | 95 |
| All three workers read before any writes (noise 0/12/24) | not found in 100,000 (0%) | 21 (9.7%) | 12 (4.3%) | 24 | 693 | 6,656 |
| PR #204: never-started child ends `Stopped` | not found in 100,000 (0%) | not found in 100,000 (0%) | not found in 100,000 (0%) | exhausted (14) | exhausted (151) | exhausted (1,148) |

Hit rates use `k` fixed at the longest run seen by the first-failure search.
One demotion of the quick worker after its read already reaches the
three-worker ordering (`dfs:bound=1` finds it), so the row says nothing about
a "depth 3" bug. Since PR #209 a failed initial placement finishes every
never-started child as `Failed`, so `Stopped` is unreachable: DFS exhausts
each bounded space without finding it. Deeper DFS bounds find a bug later
because depth-first order tries late delays first. The table was re-measured
after the second review round (PCT now demotes the task that ran the change
point, as in the paper).

```sh
RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test --release -p spawned-concurrency \
  --test sim_explore -- --include-ignored --nocapture
```

### Scenario fuzzing (`fuzz/`)

`fuzz/` is a cargo-fuzz (libFuzzer) crate, outside the workspace, whose
target `scenario` decodes each input with `arbitrary` into a `Scenario`: a
simulator seed, a scheduling strategy and up to 64 operations. Everything that
can change a run is in the input, so every execution is deterministic and
replays from the input alone.

| Part | Contents |
|---|---|
| Schedule | `Random` (seeded walk), `Pct { d = 1..4, k = 1..4000 }` (one PCT run) or `Canonical` (smallest ready task), all from `sim::explore` |
| Actors | spawn unmanaged (`start`), managed (`start_on` an async group of up to three systems) or `start_linked`; hooks that sleep (virtual time) or panic in `started()`, `stopped()` or the state destructor |
| Messages | `send` (by `ActorRef` or `Recipient`), requests inline or in their own task with no or any timeout; handler commands: no-op, panic, sleep, stop self with any `ExitReason`, send a `Note`, link, unlink, monitor, trap exits, register a name, stop another actor |
| Signals | link, unlink, monitor, demonitor and `trap_exit` from outside; `stop`, `shutdown`, `stop_with_reason(Kill/Panic)` |
| Timers and time | `send_after`, `send_interval`, cancel; advance virtual time; yield |
| Supervision | trees of up to four children, workers or nested supervisors (two levels), every strategy and restart mode, budgets (`max_restarts` 0-6, window 50 ms-131 s), backoff with or without jitter (fixed seed), startup and shutdown timeouts; per-generation factory errors, factory panics and hook failures; crash or stop the current generation, stop a nested supervisor, stop or shut down a root |
| Systems | create systems and async groups, `shutdown(deadline)` (generous, short, or after quiescing with an order check), drop a system |

Only `Backend::Async`, async groups and supervisors run: the simulator has
one thread (Thread, Blocking and native placements are outside it, see the
fidelity limits above). A supervisor's initial `start()` is cancelled after
30 s of virtual time, because a subtree that restarts more slowly than its
parent's budget refills legitimately retries forever.

Oracles, checked during and after every run (the failure message is the
oracle's name, so shrinking can match it):

| Contract | Check |
|---|---|
| P1 | No panic escapes the framework into the driver; the simulator reports no deadlock and no livelock (2 million polls) |
| M1, L1, L5 | No handler before `started()` returned or after `stopped()` began; each hook at most once; no `stopped()` after a panic in `started()` |
| D1, D2 | Per-sender FIFO on the driver's lane; no message handled twice |
| R1-R3 | Every request resolves; `Ok` carries its own nonce and the handler ran; `RequestTimeout` never before the timeout |
| X1 | `Down` only for the watcher's own monitor, at most once, with the target's published reason; after quiescing, a live watcher has a `Down` for every exited, non-demonitored target |
| X3, X5 | `Exit` carries the peer's published reason; a trapping, still-linked actor gets it (no more often than links were made); a non-trapping one does not survive a peer's abnormal exit |
| X7 | No `Note` from a source after its `Down`/`Exit` at the same receiver |
| L4, M | An actor that panicked exits abnormally; a published reason never changes; an actor nothing could have stopped is still alive |
| V1, V3, V5 | A Temporary child is built at most once per supervisor instance; no factory runs while an earlier generation of its slot (or subtree) is alive; retries per rolling window never exceed `max_restarts` |
| V4, V9 | `current()` never returns an exited generation; a finished supervisor has every child `Stopped` or `Failed`, no live generation, and runs no factory afterwards; children stop in reverse declaration order |
| S1-S4 | A complete system shutdown leaves no managed actor running and no coordinator failure; a shut-down system admits nothing; targets stop in reverse admission order |
| M5, S3, S5 | After teardown: every actor state dropped, no registered name left, and the simulator's task count back to its starting value |

Order checks run only after quiescing (timers cancelled, requests collected,
every actor fenced, no child starting or backing off) and when every
`stopped()` fits its shutdown timeout: a missed timeout signals the remaining
siblings early by design. Link predictions skip pairs whose trap flag or links
changed inside a handler, pairs that were unlinked, and links made from an
exited actor's context (a no-op).

Running it needs the pinned nightly (`nightly-2026-10-20`, for libFuzzer's
SanitizerCoverage flags) and cargo-fuzz 0.13. cargo-fuzz appends
`RUSTFLAGS`, so every crate in the target, `spawned-rt` and
`spawned-concurrency` included, is instrumented. The crates forbid
`unsafe`, so the runs use `-s none` (no AddressSanitizer):

```sh
rustup toolchain install nightly-2026-10-20 --profile minimal -c llvm-tools-preview
cargo install cargo-fuzz --locked --version 0.13.2
cd fuzz
export RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable"
cargo +nightly-2026-10-20 fuzz run -s none scenario corpus -- -max_total_time=600
./campaign.sh 3600 2 /tmp/campaign      # fork mode (2 workers), coverage.csv every minute
cargo test --release --test corpus      # seeds and generated inputs, stable Rust, no libFuzzer
```

A failing run prints the oracle, its details, the strategy and seed, and the
path of its decision tape (`SPAWNED_FUZZ_TAPE_DIR`, default
`$TMPDIR/spawned-fuzz`), then aborts so libFuzzer saves the input. To turn
it into a regression:

```sh
cargo +nightly-2026-10-20 fuzz tmin -s none scenario artifacts/scenario/crash-<sha>
cargo run --release --example repro -- artifacts/scenario/minimized-from-<sha>          # decode, run, shrink the tape
cargo run --release --example repro -- artifacts/scenario/minimized-from-<sha> --emit name
cargo test --release --test name
```

`repro --emit` writes `regressions/name.{input,tape}` and `tests/name.rs`:
one test replays the shrunk tape strictly (or, once a fix changed the task
structure so the tape diverges, follows it permissively) and requires the
scenario to pass, and a `#[sim_test(seeds = 32)]` runs the same operations
over other seeds. `--emit` also keeps a passing input (a fixed bug or a
fixed harness false positive) with its own tape; `fuzz/tests/` holds them.
`SPAWNED_FUZZ_TRACE=1` prints each operation with virtual time and poll
count; `SPAWNED_FUZZ_VERBOSE=1` keeps panic output.

TLA+ trace validation of a sample: with the `lifecycle-trace` feature the
harness writes the lifecycle trace of every finished flat tree (OneForOne
for `Supervision.tla`, OneForAll and RestForOne, cohort actions only, for
`Cohorts.tla`; nested trees are not exported), and
`fuzz/tla-sample.sh CORPUS [MAX] [OUT]` replays a corpus and runs
`trace_check.py` on them (`nix develop -c fuzz/tla-sample.sh ...`).

CI (`fuzz.yml`) runs rustfmt, clippy and the harness tests (seeds,
regressions, generated inputs) and a 60 s single-process smoke run on pull
requests that touch `rt`, `concurrency`, `macros` or `fuzz`; longer
campaigns are `workflow_dispatch` only. There is no scheduled run.

**First campaign** (2026-11-11, a shared 30-core Linux server, libFuzzer fork
mode with 4 then 2 workers at `nice 19`, `-s none`): about 3 h 10 min and
20 million executions over eight segments; each segment restarted from the
previous corpus after a harness fix. Throughput was 300-2,400 executions per
second (median about 700) depending on machine load; a single process on
an idle GitHub runner does about 1,100. Edge coverage (libFuzzer `cov`) grew
from 7,900 after one minute to 9,400 after the first hour and 9,965 at the
end (feature count 59,300, corpus 4,700 inputs), still rising slowly.
`fuzz/coverage.sh` on the final corpus: lines covered in
`supervision.rs` 83%, `tasks/actor.rs` 79%, `system.rs` 73%,
`supervision/nested.rs` 75%, `link.rs` 91%, `monitor.rs` 77%, `tasks/time.rs`
100%; threads mode, streams and protocol `Response` are not reachable from
the harness. `fuzz/tla-sample.sh` on the minimized corpus exported 238 lifecycles of
finished flat trees (62 OneForOne, 176 OneForAll/RestForOne) and TLC
accepted all 238.

Every failure was a harness false positive; no framework bug and no
simulator fidelity problem was found. Each is kept in `fuzz/tests/` with the
input and, for random schedules, its tape:

| Oracle | Cause (harness) |
|---|---|
| liveness (fence) | a 1 ms `send_interval` of a 4 s sleep queued hours of work (`interval_sleep_fence`) |
| S2 order | a later unit's abnormal exit stopped an earlier, linked unit through the link before the coordinator reached the units in between (`link_skips_shutdown_order`) |
| V9 order | a `shutdown(0 ms)` that timed out signals every remaining child at once, by design (`short_shutdown_then_ordered`) |
| M5/S3 leak | a dropped system's background shutdown released state after 3.4 s of virtual time, past a 3 s wait (`dropped_system_slow_cleanup`) |
| V9 order | under the canonical schedule a yielding driver starved a child that was already stopping; quiescing now sleeps (`stopping_child_settles_before_order`) |
| X3 | `ctx.link` from an exited actor's context is a no-op (`register_link` sees its own table closed), while linking to an exited peer delivers its reason (X5); found by the generated-input test before the campaign. The asymmetry is not stated in the semantics contract. |
| livelock | a nested subtree that restarts more slowly than its parent's budget refills retries forever; the start is now bounded at 30 s |

## Scope

This is generated, model-checked scenario testing on real runtimes, not an
exhaustive scheduler; the coverage-guided fuzzer above runs on the simulator
only. It does not cover bounded mailboxes or supervisor restart policies.
Demonitor races permit an already-queued notification; tests use bounded
observation windows and do not claim exhaustive delivery verification.
Do not interpret a passing campaign as proof of race freedom.

## Additional checks

- Concurrent producers verify per-producer FIFO, exactly-once processing during
  normal operation, and reply identity across all four backends.
- Monitor cleanup unit tests verify waiter-owned references are released while
  the target remains alive, after demonitor or owner termination.
- Resource cleanup tests verify timer payloads and pending async streams are
  dropped after shutdown. Blocking iterator `next()` calls are not preemptible;
  the listener checks cancellation before invoking the next call.
- Compile tests (trybuild) accept valid `#[protocol]`/`#[actor]` edge cases and
  snapshot the diagnostics for invalid ones; see
  [Macro diagnostics](#macro-diagnostics).
- Early-stop regressions ensure a cleanup panic remains an abnormal exit.

The Loom tests instrument the actual thread CancellationToken implementation:

```sh
RUSTFLAGS="--cfg spawned_loom" cargo test -p spawned-rt --test cancellation_loom
```

The custom cfg isolates instrumentation from Tokio's own Loom configuration.
These models cover registration/cancellation races and reentrant callbacks,
not the entire actor runtime. Do not run ordinary actor tests with this cfg.

Run sustained actor creation, traffic, monitoring, and shutdown:

```sh
SPAWNED_STRESS_ROUNDS=100 cargo test -p spawned-concurrency --test actor_scenarios sustained_actor_churn -- --ignored --nocapture
```

This is a correctness/churn campaign with elapsed-time logging, not a comparative
performance benchmark. For process memory measurement, wrap the command with
`/usr/bin/time -v` on Linux or `/usr/bin/time -l` on macOS. No portable RSS threshold
is asserted; allocator retention can differ between platforms.

## Bugs reproduced by these regressions

1. Demonitor and owner shutdown suppressed delivery but retained the monitor
   waiter until target exit, in both execution modes. Waiters are now explicitly
   cancelled and outgoing monitors are drained on every actor exit path.
2. Thread listeners called `next()` even when their actor was already stopped.
   The listener now checks cancellation before polling the iterator.
3. Stopping inside `started()` swallowed a subsequent `stopped()` panic and
   reported Normal. Both execution modes now preserve the panic exit reason.
4. PR #166, links: a `Context::link` made after the target drained its link
   table but before it published completion received no exit signal
   (`linking_in_post_drain_pre_completion_window_delivers_exit`). Link closure now
   stores the terminal reason under the registration mutex.
5. PR #166, links: dropping the owning runtime before a linked actor was first
   polled left its peer running instead of propagating the abnormal exit
   (`never_polled_actor_propagates_runtime_shutdown_to_peer`). Cleanup is now
   built before dispatch. Both #166 regressions are in
   `concurrency/src/link_regression_tests.rs` and failed on the original patch.
6. PR #168, thread mailboxes: the original patch replaced the 100 ms mailbox
   poll with a blocking receive but only woke it from `Context::stop` and `ChildHandle` stop, leaving
   linked exits and framework cancellation unable to wake an idle
   thread actor. One cancellation callback registered at mailbox creation now
   wakes it on every route (`every_stop_route_wakes_an_idle_thread`,
   `cancellation_wakes_the_real_blocking_receive_boundary`).
7. PR #168, thread mailboxes: in the original patch each stop call queued another private Shutdown item,
   so repeated stops during a busy handler grew the control queue without bound.
   At most one is now sent per actor
   (`concurrent_cancellation_enqueues_exactly_one_shutdown`,
   `repeated_stop_during_a_handler_discards_queued_requests`). The #168 tests are
   in `concurrency/src/threads/actor.rs`.

Reviews and evidence: [PR #166](../legacy/docs/reviews/pr-166-links.md) and
[PR #168](../legacy/docs/reviews/pr-168-security.md).

Thread actor completion now uses a watch notification, so async exit waits can
be cancelled without leaving a blocking-pool worker waiting for target exit.
A regression with a one-worker blocking pool checks that property.

## Cross-backend, cancellation and panic safety

`mailbox_drop_panic` runs a subprocess matrix of five task placements, 1/2/256
queued panicking destructors and both ordinary/handler-panic shutdown. Its enabled
parent test checks that the child actually ran all 30 cases, observes one Exit
and one Down, verifies pending reply release and preserves the earlier failure.
Both stdout and stderr are drained concurrently, with a 128 KiB stdout control.
Only subprocess helpers are ignored; enabled parent regressions run in normal CI.
Additional cases cover never-polled runtime teardown, cancellation mid-handler,
state-destructor unwind, preserved Shutdown/Kill/linked-panic causes, and native
queued-reply release while ActorRefs remain alive. Native coverage runs without
async features. A never-polled Tracked future test checks cleanup before its lease
is released. The production native send-gate algorithm is also used directly by
`mailbox_admission_loom`; CI explores close/publication races and lost notifications.
`startup_admission` forces a managed root to stay unpolled and contrasts its
no-hook cleanup with legacy ambient startup. The `queued_drop_panic` lifecycle
trace checks transient restart, with a negative control that erases the panic.

Batch 1 of the [database-grade testing program](../ROADMAP.md). All three
suites use handshakes and bounded waits (10 s per wait), never sleeps as
synchronization.

| Suite | Evidence provided | Limit |
|---|---|---|
| `cross_backend` | Request/reply chains, fan-out/fan-in, links with trapped exits, monitors, timers, supervisor restarts (all three strategies) and ordered system shutdown are defined once and run on tasks `Async`/`Blocking`/`Thread`, threads mode, and `async_group`, `thread_group` and (Linux only) `pinned_group`. Outcomes must be identical across placements and match an independent oracle. | Supervisor and system-shutdown workloads run only on execution groups. Six proptest cases per workload by default; not a schedule exploration |
| `cancellation_safety` | Every public async operation (`request`, `request_with_timeout`, `tasks::request`, protocol calls, monitor/link setup requests, `join`/`wait_exit`/`wait_exit_async`, `ready`, `SupervisorBuilder::start` with instant and in-flight factories, `Supervisor::shutdown`/`join`, `SupervisedChild::wait_ready`/`wait_for_generation`, `ActorSystem::shutdown`, `ShutdownReport::wait`, `ScopedTask::join`) is dropped after N polls for N = 0 until it completes or parks. Afterwards the actor stays usable or stops cleanly, effects occur at most once, monitors/links fire exactly once, registrations are released, actor state is freed and shutdown completes | Drop points are poll boundaries reached by the operation's own wakeups; an operation waiting on an external event is dropped once while parked |
| `panic_safety` | A panic in `started`, a send handler, a request handler, `stopped`, trapped `exit_received`, the `Down` handler, a message `Drop`, an abandoned reply's `Drop`, a queued message's `Drop` and `ChildSpec` factories (sync and async). Both modes, every tasks backend and both group kinds, plus OneForOne supervision. Checks the exact exit reason, exactly one `Down` and one `Exit`, registry release, a working bystander and complete shutdown | A queued-drop panic is a single destructor; multi-destructor and per-placement counts live in `mailbox_drop_panic` |
| `foreign_panics` | Panicking global tracing subscribers, process panic hooks and abandoned reply destructors. Bounded subprocesses isolate global state and abort-sensitive cases. Strict enabled tests preserve lifecycle panic reasons and stopped hooks and require supervised replacement to succeed | A panicking process panic hook aborts in plain Rust too; the test documents that limit |

```sh
cargo test --locked -p spawned-concurrency --test cross_backend --test cancellation_safety --test panic_safety --test foreign_panics
cargo test --locked -p spawned-concurrency --no-default-features --features threads --test panic_safety
PROPTEST_CASES=256 cargo test --locked -p spawned-concurrency --test cross_backend
SPAWNED_CANCEL_REPORT=1 cargo test --locked -p spawned-concurrency --test cancellation_safety -- --nocapture
```

Proptest prints the shrunk failing workload parameters and persists the seed
under `concurrency/tests/*.proptest-regressions`; replay with
`PROPTEST_RNG_SEED`. The `robustness` workflow runs these suites with more
cases and repeated runs.

**Compared across placements:** reply values, per-sender message order, exit
reasons including panic text, Down/Exit counts and reasons, timer delivery sets,
restart generations and factory calls, supervisor and system shutdown order.
**Allowed to differ:** timing, thread identity, `ActorId`/`MonitorRef` values
(mapped to logical indices), cross-sender interleaving at one actor, and
arrival order between distinct timer deadlines. One lifecycle difference is
the intended contract, asserted by
`stop_before_first_poll_follows_managed_and_legacy_contract`: a managed actor
canceled before startup skips both hooks and drops its state; legacy unmanaged
tasks and threads actors retain their existing behavior of running both hooks.

**Cancellation contract observed:** `request`-family futures are `async fn`
and enqueue nothing until first polled; after that, dropping the caller never
cancels the effect. Protocol methods send when called, so dropping the returned
`Response` (even unpolled) never prevents the effect. `Supervisor::shutdown`
and `ActorSystem::shutdown` request stop on their first poll; an unpolled future
requests nothing. A dropped `SupervisorBuilder::start` closes the tree and
cancels in-flight factories.

**Queued-message destructor panics:** a queued message whose `Drop` panics
while a stopping actor discards its mailbox publishes the trappable
`Panic("queued message destructor: <panic payload>")` in both modes, on every tasks placement
and under supervision; a trapping linked observer receives one `Exit` and one
`Down` and survives. The receiver-owned boundary also runs when an actor future is canceled or never
polled. A previously committed Shutdown/Kill/linked-panic cause takes precedence;
`ChildHandle::mailbox_cleanup_failure()` retains the secondary queued failure.
The enabled subprocess matrix covers multiple panicking destructors and actor
state destruction without depending on the last ActorRef being dropped.

## Toolchain

The repository's `.tool-versions` selects Rust 1.93.0. The workspace tests,
including generated cases and compile diagnostics, have been validated with
that compiler. Proptest and trybuild are dev-dependencies; proptest's unused
default features are disabled. Cargo generates the lockfile.

### Macro diagnostics

`concurrency/tests/protocol_compile.rs` runs trybuild over
`concurrency/tests/ui-pass/*.rs` (must compile and run) and
`concurrency/tests/ui/*.rs` (must fail with the exact `.stderr` snapshot).
Fail cases are named `protocol_*` or `actor_*` after the macro under test and
cover receivers, generic and borrowed parameters, return types, timeout
attributes and options, duplicate and colliding methods, non-trait/non-impl
items, handler and lifecycle-hook signatures, async/sync mismatches, missing
protocol handlers and attribute parameters.

Snapshots depend on the exact compiler, so the test compares `rustc --version`
with the version in `.tool-versions` (the one every CI job pins) and, on any
other compiler, prints a notice and passes without running the cases.

```sh
cargo test --locked -p spawned-concurrency --test protocol_compile              # pinned toolchain
SPAWNED_UI_TESTS=1 cargo test -p spawned-concurrency --test protocol_compile    # force on another toolchain
TRYBUILD=overwrite cargo test -p spawned-concurrency --test protocol_compile    # regenerate snapshots (pinned toolchain only)
```

When bumping the pinned toolchain, update `.tool-versions` and the CI
`dtolnay/rust-toolchain@` pins together and regenerate the snapshots in the
same change.

## Debug future layouts

`future_layout.rs` pins the shapes that application futures inherit. Run it
with default features and `--features metrics`.

- `tasks::ActorRef` and `Context` must be exactly one pointer (also inside
  `Option`): both share one `Arc`'d state. Owned handles are moved through
  application futures, and debug builds give every move its own stack slot, so
  each handle word is repeated across callers' frames. The 10-11 word handles of
  0.7.1 (0.5.0: 4 words) cost Ethrex's debug `rpc::` tests ~136 KiB of thread
  stack through an RPC context holding three handles; one-pointer handles took
  its minimum from 2052 KiB to about 1850 KiB (0.5.0: 1919 KiB).
- Send, protocol-call and an RPC-shaped nested dispatch future are compared
  with the same shapes over a one-word stand-in handle, which catches handle
  growth.
- On Linux x86_64 (Rust 1.93) absolute budgets also cover `Response<T>`, the
  protocol call and RPC dispatch futures, and owned/configured request futures,
  so growth of `Response<T>` or the request path fails even when the handles
  stay one pointer.

Matching layout is a regression check, not proof that Ethrex's RPC tests pass
at a 2 MiB debug stack; rerun them (`RUST_MIN_STACK` bisection of
`cargo test -p ethrex-test --test ethrex_tests rpc::`) when handles or request
futures change. Earlier probes comparing fc35abaf, 35bea650 and the compact
sender are in the [round-2 evidence](../legacy/verification/evidence/v071-review-round2/README.md).

## Stack-depth budgets

`stack_budget.rs` complements `future_layout.rs`: instead of `size_of`, it
bounds the stack a real workload uses. A nested request chain mirrors a client
RPC path (A awaits a protocol request to B, B to C, C to D; 1 KiB inline
payload plus a 64 KiB body; `SPAWNED_STACK_DEPTH` changes the depth) on an
`async_group`, a `thread_group`, the ambient runtime and in threads mode, plus
supervisor start, restart and shutdown and `ActorSystem::shutdown`. Each
scenario runs in a child process with `RUST_MIN_STACK` set to its budget, so
framework, tokio and libtest threads all get that stack, and the client is
driven from a `std::thread` with the same `stack_size`. An overflow aborts the
child and fails the parent.

Budgets are about 1.5x the measured debug minimum. aarch64-apple-darwin: 104
KiB for the request chains (minimum 68), 80 KiB for supervision (52) and 32
KiB for threads mode (20); release minimums are 8 to 20 KiB. x86_64 Linux
(Rust 1.93, v0.7.1): 132 KiB for the chains (80 to 84), 120 KiB for
supervision (80) and 68 KiB for threads mode (40); release chains need 28 KiB.
The async chain does not grow with depth, since each hop is its own task; the
deepest single poll on a group worker sets the minimum. Linux aarch64 cannot go
below glibc's 128 KiB thread minimum, so there the test only guards against
gross regressions. The table with all numbers is at the top of
`stack_budget.rs`. Re-measure with:

```sh
cargo test -p spawned-concurrency --test stack_budget -- --ignored probe --nocapture
cargo test --release -p spawned-concurrency --test stack_budget -- --ignored probe --nocapture
```

## Execution guarantees

The full list of guarantees, each mapped to the invariant, test or example in
this document that checks it (and the ones not yet checked), is the
[semantics contract](design/semantics.md).

Protocols continue to separate application logic from message dispatch. Each
actor handles one message at a time, with both async and synchronous thread
execution supported. No restart or queue-capacity policies are added by these
tests. See CHANGELOG.md for the observable lifecycle corrections.


<a id="managed-execution-and-recovery-main-unreleased"></a>

## Managed execution and recovery (v0.7)

`execution_groups` exercises foreign-thread helper routing, placement closure,
readiness, shutdown report retention, scoped registrations, helper panic/abort,
and native helper completion. `supervision` exercises fresh identities/state,
restart modes, shared retry budgets, construction failures/panics, startup
cancellation/rollback, reverse dependency shutdown, backoff cancellation and
stuck helper/`stopped()` cleanup with no overlapping replacement. Policy tests
exercise retry-window boundaries and reproducible capped backoff/jitter.
These are controlled failure tests, not a deterministic executor or a claim that
all OS schedules were explored. CI also runs both asserted capability examples.

The [implementation review](../legacy/docs/reviews/execution-groups-supervision-implementation.md)
archives 33 focused integration tests, full workspace/default/all-feature gates,
strict Clippy, eight examples and 20 fresh-process repetitions of the 32-iteration
crash/producer/shutdown race (640 iterations). These explore ordinary OS/Tokio
scheduling; they are not deterministic delivery simulation.

## Formal model, trace validation and mutation testing

`verification/tla/Supervision.tla` models the implemented OneForOne supervisor
(`run_supervisor`/`run_child`): restart modes, the shared rolling budget,
ordered startup, reverse shutdown, shutdown deadlines and stuck cleanup. TLC
checks seven safety invariants (including at most one live generation per child,
no publication of a stale generation, and no ready report for a partly started
tree), action properties (no start after close, restarts follow the design's
mode table, reverse-order shutdown) and liveness (a closed supervisor finishes).
Five bugs injected into the model are each caught, so the properties are not
vacuous. See [the model README](../verification/tla/README.md).

With the `lifecycle-trace` feature (off by default; compiled out otherwise) the
supervisor records one event per model action. `supervision_trace` drives real
actors through restart, failure, backoff, timeout and deadline scenarios plus
seeded random runs, and TLC checks every recorded trace is a behavior of the
model. Hand-edited traces are rejected at the edited event. Trace validation
found one model gap (a running child may exit with `Shutdown` while the
supervisor is open, via `ChildHandle::shutdown`); no implementation deviation.

Apalache checks that the safety invariants of `Supervision`, `Cohorts`,
`NestedSupervision` and `Coordinator` are inductive (`Init => IndInv`,
`IndInv /\ Next => IndInv'`), so they hold at any depth, for the configured
sizes (5 children and `max_restarts` 4, also 10/10, for `Supervision`; 8 and
32 shutdown targets for `Coordinator`); guard-removal negative
controls must break induction. See [the model README](../verification/tla/README.md#inductive-invariants-apalache).

```bash
nix run .#check-tla                                   # model check (TLC via the flake)
nix run .#check-apalache -- --negative-controls       # inductive invariants (Apalache)
cargo test -p spawned-concurrency --features lifecycle-trace --test supervision_trace
nix run .#check-tla -- --traces target/lifecycle-traces
```

### Trace coverage

`verification/tla/trace_coverage.py` measures which model actions, projected
states and transitions the recorded traces reach, so unreached model behavior
becomes a list of missing tests. Keys are defined in TLA+ and computed by TLC
on both sides: over every reachable state of the CI configurations, and along
each accepted trace. With `LIFECYCLE_TRACE_ALL=DIR` every finished supervisor
in any test binary built with `lifecycle-trace` writes its traces there, so the
whole suite (integration, proptest policies, cross-backend, failpoints)
contributes, not only the dedicated trace tests. `coverage_trace` adds six
tests for the top-ranked gaps (an attempt abandoned when startup is dropped,
startup cancelled during backoff, shutdown while a restart is under
construction, a Transient replacement exiting in `started()`, a crash while a
sibling is starting, a nested leaf restart). Every PR prints the report and
fails only if an action covered in `coverage-baseline.json` is lost. Numbers,
method and the remaining gaps: [trace coverage](../verification/tla/README.md#trace-coverage).

```bash
cargo test -p spawned-concurrency --features lifecycle-trace,failpoints --test supervision_trace --test cohort_trace --test tree_trace --test nested_supervision --test coverage_trace
nix develop -c python3 verification/tla/trace_coverage.py --traces dedicated=target/lifecycle-traces --model-keys verification/tla/coverage-model.json.gz
```

All crates are `#![forbid(unsafe_code)]`, so Miri is not run; ThreadSanitizer
runs the lifecycle tests in CI. `cargo mutants` over `supervision.rs` and
`system.rs` runs weekly. The remaining surviving mutants are race-only guards
(`can_start`, `register`, `lease` checks only reachable in narrow windows),
redundant system-level safety nets (group teardown also stops supervised
children) and simple accessors. The backoff and jitter seed mixing mutants are
killed by the Kani proofs below, and by unit tests so that `cargo mutants`
(which does not run Kani) sees them caught too.

Most other survivors are equivalent in a default build: arguments of
`lifecycle-trace` events (`count`, trace indices), code behind `metrics`,
`diagnostics` or `backend-experiments`, the random seed fallback, and the
`with_pair` lock order flipped to `>`/`<=` (any consistent order is
deadlock-free; `==`, which is not, is caught). Run cargo-mutants with the
feature that compiles a mutated line before treating it as a gap. Code inside
`macro_rules!` bodies is not mutated at all, so keep lifecycle code out of
single-use macros.

## Kani proofs of supervisor arithmetic

[Kani](https://model-checking.github.io/kani/) checks every input, not samples,
for small pure functions the supervisor depends on. The harnesses live in
`concurrency/src/kani_proofs.rs` behind `cfg(kani)`, so normal builds never see
them. Kani cannot run async code or threads; the supervisor calls the same pure
helpers (`Backoff::bound`/`delay`, `jitter_seed`, `admit_in_window`,
`RecoveryQueue::select`/`request`) that the harnesses check.

```bash
cargo install --locked kani-verifier && cargo kani setup   # once
cargo kani -p spawned-concurrency                          # all harnesses
cargo kani -p spawned-concurrency --harness backoff_jitter_stays_in_range
```

| Harness | Proves | Bound | Time |
|---|---|---|---|
| `backoff_bound_is_capped` | Pre-jitter bound in `[initial, maximum]`, first retry waits `initial`, no panic | Any `Duration`s, any `u32` failures (Kissat) | 1.4 s |
| `backoff_delay_without_jitter_is_bound` | No jitter: `delay == bound`, stream not advanced | Sub-second configs | 1.4 s |
| `backoff_bound_is_monotone_whole_seconds` | Bound non-decreasing in failures | Whole-second configs (any `u64` s) | 4.0 s |
| `backoff_bound_is_monotone_subsecond` | Same | Sub-second configs | 1.2 s |
| `jitter_draw_stays_in_range` | Draw in `0..=nanos`, no panic (incl. 0 and `u64::MAX`) | All `u64` | <0.1 s |
| `jitter_draw_reaches_both_ends` | Both ends of the draw range reachable | All `u64` | 0.7 s |
| `jitter_clamp_stays_within_bound` | `u64` clamp never exceeds the true nanosecond count | Any `Duration` | 46 s |
| `jitter_conversion_stays_within_bound` | Draw converted back to `Duration` stays within bound | Sub-second bounds | <0.1 s |
| `backoff_jitter_stays_in_range` | End to end: jittered delay `<= bound <= maximum`, no panic | Sub-second configs, any failures and stream | 1.6 s |
| `backoff_jitter_zero_bound` | Zero `initial` jitters to exactly zero, no panic | Any `maximum` | 0.1 s |
| `jitter_seed_is_injective_in_seed` | Distinct seeds give distinct per-child streams | Names up to 4 bytes | 1.3 s |
| `restart_window_admits_exactly_within_budget` | Admit iff fewer than `max` admissions inside the window, checked against an independent log | `max <= 2`, window <= 255 ns, 3 non-decreasing stamps (Kissat) | 5.6 s |
| `recovery_rounds_are_fresh_and_monotone` | Round ids nonzero and strictly increasing, coverage never moves back, stale requests rejected, rounds clear what they cover | 3 children, 4 request/select steps, either strategy | 7.2 s |

The config domain is what `SupervisorBuilder` accepts (`initial <= maximum`).
The proofs kill the surviving cargo-mutants in this code: `nanos + 1` to
`- 1`/`* 1` (overflow / remainder by zero at a zero bound) and the seed `^` to
`|`/`&` (seed collision). Mixed seconds-and-nanos monotonicity and a deeper
restart window (`max <= 3`, 5 stamps) time out after 5 minutes; they are kept
behind `cfg(kani_slow)` (`RUSTFLAGS="--cfg kani_slow" cargo kani ...`).
Symbolic `Duration` multiplication and `u64` division over the full domain is
the expensive part, which is why some harnesses split the domain.
Not proved: `ChildSnapshot::generation` is a `u64` incremented once per start
attempt and recovery ids use `checked_add`; neither can wrap before 2^64
attempts or rounds (about 584 years at one per nanosecond), so the harnesses
start from the real initial values and do not explore exhaustion. The
`changed` watch counters use `wrapping_add` but only signal a change; their
values are never compared. Times are from Kani 0.68 on an Apple M-series
laptop; CI pins the same Kani version (`.github/workflows/kani.yml`).

## Failpoints

With the `failpoints` feature (off by default; the sites compile to nothing
otherwise, and CI checks a default release binary contains no failpoint strings
or symbols) Spawned's own lifecycle code has named fault-injection sites, in the
style of FoundationDB `BUGGIFY` and etcd `gofail`. Tests configure a site at
runtime through `spawned_concurrency::failpoints`:

```rust
let _scenario = FailScenario::setup(); // serializes tests, clears sites on drop
failpoints::configure("child.factory", FailPoint::new(Action::Error).times(3))?;
failpoints::configure("supervisor.before_restart", Action::Delay(Duration::from_millis(300)))?;
failpoints::configure("registry.before_register", FailPoint::new(Action::Error).probability(0.2))?;
```

Actions are `Off`, `Panic`, `Error` (only at sites with an error path),
`Delay(d)` (async sleep at async sites, thread sleep otherwise) and `Yield`;
`probability(p)`, `skip(n)` and `times(n)` refine when a site fires, and
`stats(site)` reports hits and fires. Probabilities draw from a per-site
generator derived from one seed; `FailScenario` reads `SPAWNED_FAILPOINTS_SEED`
and prints the seed when a test fails. Configuration is process-global.

| Site | Error path | Models |
|---|---|---|
| `actor.before_started` | no | Failing or slow `started()` (tasks and threads) |
| `actor.after_started_before_ready` | no | Owner shutdown racing a managed actor's readiness |
| `actor.before_stopped` | no | Failing or slow `stopped()` (tasks and threads) |
| `child.factory` | yes | Failed, panicking or slow child construction |
| `child.after_ready_before_publish` | yes | Stop or recovery racing publication of a ready generation |
| `child.before_cleanup_join` | no | Slowly scheduled controller before its cleanup deadline |
| `supervisor.before_restart` | no | Stop racing a restart decision; controller panic |
| `supervisor.during_backoff` | no | Stop racing the retry after backoff |
| `shutdown.before_stop_child` | no | Supervisor stop turn consuming its deadline |
| `system.shutdown_before_target` | no | System shutdown coordinator consuming its grace |
| `system.group_start` | yes | Group construction failure after runtimes were built (rollback) |
| `system.reserve` | yes | Placement failure (`SystemError::Capacity`) |
| `registry.before_register` | yes | Shutdown racing managed name registration |
| `registry.before_unregister` | no | Slow removal of an owned name before a replacement starts |
| `link.before_install` | no | Peer exiting while a link is installed (tasks and threads) |
| `monitor.before_install` | no | Target exiting while a monitor is installed (tasks and threads) |

`tests/failpoints.rs` drives every site and checks invariants through public
APIs: restarts follow the budget (generation count equals injected failures
plus started actors), a stop during a restart or publication window starts or
publishes nothing, shutdown reports at its deadline and later completes,
discarded generations are stopped, and registry entries never outlive their
owner.

```bash
cargo test -p spawned-concurrency --features failpoints --test failpoints
```

Panics injected into the supervisor coordinator (`shutdown.before_stop_child`)
and the system shutdown coordinator (`system.shutdown_before_target`) are
reported (`ShutdownReport::failure`, `coordinator_failures`) and followed by an
emergency stop-and-join, and a child whose supervisor fails during initial
placement ends `Failed` deterministically. Before v0.7.1 these were ignored
known-gap tests.

## Native integration matrix

The shared actor scenarios, controlled failure windows, history checker,
delivery simulation, payload stress, synchronous trapped-exit macro and native
timer/resource-cleanup cases compile with `--no-default-features`. Async
variants are gated inside those suites. CI checks required test names in the
native test inventory so a file-wide async gate cannot silently remove them.
Tokio is still a dev dependency of the test harness; the isolated thread-only
example/dependency-graph check verifies the library's Tokio-free build.

```sh
cargo test --locked -p spawned-concurrency --no-default-features --features threads,metrics
cargo test --locked -p spawned-concurrency --no-default-features --test payload_stress -- --ignored
```

## Client compatibility

`.github/workflows/client-compat.yml` builds and tests the two production
clients, [ethrex](https://github.com/lambdaclass/ethrex) and
[ethlambda](https://github.com/lambdaclass/ethlambda), against the spawned
commit under test. Each client keeps a `spawned-v0.7` branch that tracks
spawned; one GitHub-hosted job per client checks it out next to spawned.

`.github/scripts/client-compat-patch.sh` points the client at the local
checkout without editing its manifests. It reads where the client's
`Cargo.lock` takes `spawned-concurrency` from (crates.io or a git URL), appends
a `[patch.<that source>]` section for `spawned-rt`, `spawned-macros`,
`spawned-concurrency` and `spawned-test` to the client's `.cargo/config.toml`,
re-locks those four packages and fails unless `cargo metadata` resolves every
`spawned-*` package to the local path (an unused patch is only a warning in
cargo). With a crates.io source the local version must satisfy the client's
requirement: a pre-release suffix on main (`0.7.2-dev`) is stripped in the CI
checkout so `^0.7.1` matches, and a different `0.MINOR` fails with a message
saying the client must move first. A git source carries no version requirement.

The gates (`.github/scripts/client-compat-gates.sh`) use the client's own
`rust-toolchain.toml` and never set `RUST_MIN_STACK`. ethrex runs in debug
(line-tables-only debuginfo, which leaves stack frames unchanged); ethlambda's
tests run under its `release-fast` profile, the only one its suite supports (in
plain debug its rpc fixtures overflow `u8` arithmetic, unrelated to spawned).
In debug, test
threads keep the default 2 MiB stack, which is how the pre-0.7.1 ethrex stack
overflow was found. Every gate runs even after a failure; the job summary has a
table of gate, result and duration, and failed jobs upload the gate logs, the
patched `Cargo.lock` and `.cargo/config.toml`.

| Client | Gates |
|--------|-------|
| ethrex | `cargo check --workspace --all-targets`; clippy `-D warnings` with `--all-targets` on the workspace members that depend on spawned (found with `cargo metadata`); `cargo test -p ethrex-p2p`; the `p2p::` and `rpc::` groups of `ethrex-test`'s `ethrex_tests`; `ethrex-rpc` and `ethrex` lib tests |
| ethlambda | `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --profile release-fast --no-fail-fast` |

It runs nightly, on every push to main, and on a pull request only while it
carries the `client-compat` label (adding the label starts a run; later pushes
rerun it). To run it by hand, or against another client branch:

```sh
gh workflow run client-compat.yml --ref <branch> \
  -f ethrex_ref=spawned-v0.7 -f ethlambda_ref=spawned-v0.7
# Same gates against the spawned the client pins, without the patch:
gh workflow run client-compat.yml --ref <branch> -f baseline=true
```

When a gate fails, first run it with `baseline=true`: if it fails there too,
the failure is the client's (or already in the released spawned), not the
change under test. When a spawned change needs a client change, push the client
change to a branch made from `spawned-v0.7`, dispatch the workflow with that
ref, and land the spawned PR once both are green; then move the client branch. The `spawned-v0.7` client
branches back long-lived draft PRs and are never merged from here.

## Soak harness

Seconds-long tests cannot see a leak of a few bytes per restart; production
can (ethrex's mailbox leak grew 1.5 GB a day). `concurrency/benches/soak` runs
hours of seeded actor churn and fails when a resource grows. It is a
`harness = false` bench, so `cargo test` never builds or runs it; the
`--all-targets` clippy job keeps it compiling. It needs Linux (`/proc`).

```sh
# on fede, from the repository root; release build with line tables
CARGO_INCREMENTAL=0 CARGO_PROFILE_BENCH_DEBUG=line-tables-only \
  nice -n 19 cargo bench -p spawned-concurrency --features metrics --bench soak --no-run
nice -n 19 taskset -c 26-29 target/release/deps/soak-<hash> \
  --duration 4h --seed 1 --out ~/spawned-work/soak/out/release-4h
```

`cargo bench ... --bench soak -- --duration 4h --seed 1` also works. Options:
`--duration` (`4h`, `10m`, `1h30m`), `--seed`, `--sample` (default: duration
/ 360, between 1 s and 10 s), `--warmup`, `--lanes` (async lanes, default 4),
`--workers` (Tokio workers, default 4), `--out`, `--config` (default
`concurrency/benches/soak/thresholds.toml`) and `--inject-leak`. Exit code 0 is
PASS, 1 FAIL, 2 a usage error.

**Workload.** Four async lanes and one threads-mode lane each pick scenarios
from a seeded RNG until the duration ends; every scenario checks its own
outcome (exit reasons, replies, Down and Exit counts, registry contents) and
a scenario stuck for 60 s is an error. The scenarios: start, request and end
(crash, stop or `Shutdown`) actors on each backend (Async, Blocking, Thread)
and in threads mode; managed actors on a long-lived async group and thread
group with owned registrations; supervisors with each strategy, and nested,
crashing children and waiting for the replacement generation; links (trapped,
propagated, `start_linked`, unlink); monitors (demonitor, monitoring a dead
actor, the watcher dying first); `send_after` and `send_interval` cancelled,
fired or live at stop; unmanaged registry register, duplicate and unregister;
requests that time out, are dropped mid-flight or hit a stopped actor;
finite and endless stream listeners; a whole `ActorSystem` with running
actors and a supervisor shut down; and bursts of 1,000 to 21,000 messages
drained or discarded at stop. The seed fixes each lane's scenario sequence and
parameters; thread interleavings are not reproducible. A 3-minute run on fede
starts about 700,000 actors, so a 4-hour run executes every path millions of
times.

**Measurements**, one CSV row (`samples.csv`) per sample interval, plus a
`phase` column (`baseline`, `warmup`, `steady`, `quiesce`):

| Column | Source |
|---|---|
| `rss_mb` | `VmRSS` in `/proc/self/status` |
| `heap_mb` | live heap bytes from a counting global allocator installed in the harness binary only |
| `threads`, `fds` | entries in `/proc/self/task` and `/proc/self/fd` |
| `tokio_tasks` | `RuntimeMetrics::num_alive_tasks()` of the harness runtime |
| `registry` | `registry::registered().len()` |
| `actors_live` | harness actor states constructed and not yet dropped (started minus fully stopped) |
| `actors_active`, `mailbox_depth`, `handlers_active`, `requests_active` | spawned's own `metrics` gauges |
| `actors_started`, `scenarios`, `errors` | progress counters (not judged) |

Framework-internal `ActorRef` and `Context` clones are not countable through
public APIs; a leaked one keeps its actor's shared state and mailbox alive, so
it shows in `heap_mb` (and LeakSanitizer covers them at process exit, see
[leak tests](#leak-tests-and-leaksanitizer)).

**Pass/fail** (`thresholds.toml`, every value documented there). The run
first primes every scenario once and lets the runtime settle, then takes the
baseline. After warm-up (a quarter of the run, at most 30 minutes) each series
gets a least-squares slope per hour; it fails when the slope is above its
limit *and* at least 3 standard errors above zero, so short noisy runs do not
fail but a steady leak does. Limits: RSS 16 MiB/h, heap 4 MiB/h, threads and
fds 2/h, registry 2/h, Tokio tasks and live actors 20/h. At the end the lanes
stop, the long-lived system shuts down, and within 60 s every count must
return exactly to its baseline and the heap to within 8 MiB of it (RSS is not
checked here: the allocator keeps freed pages). Any scenario error, a stuck
lane or an incomplete system shutdown also fails the run.

**Reading the results.** `summary.txt` (also printed) has the verdict, the
first scenario errors, a table with each series' baseline, steady maximum,
final quiesce value, slope ± standard error, limit and the two checks, and
the run count of every scenario. A `trend` FAIL with a clean `settle` usually
means growth that is released on shutdown (an unbounded cache or queue); a
`settle` FAIL means something outlived everything that owned it. Plot the
CSV columns against `elapsed_s` to see when growth starts. A real leak gets a
minimal repro and a failing test, never a raised threshold.

**Negative control.** `--inject-leak registry,arc,task` (or `all`) plants
known leaks: one in four unmanaged registry names is never unregistered, every
supervised (re)start leaks a 4 KiB `Arc` and an actor-state token, and every
supervisor episode leaks a pending Tokio task. A 3-minute run with `all` must
FAIL on `registry`, `heap_mb`, `actors_live` and `tokio_tasks`; run it after
changing the harness or the thresholds.

```sh
nice -n 19 taskset -c 26-29 target/release/deps/soak-<hash> \
  --duration 3m --seed 1 --inject-leak all --out ~/spawned-work/soak/out/self-test
```

Runs are by hand only: no schedule. Run a 4-hour soak before each release
([release checklist](#release-checklist)).

## Release checklist

Before tagging a release (`gh release create` triggers `release.yaml`, which
publishes to crates.io):

1. CI green on the release PR, including the `client-compat` gate.
2. A 4-hour [soak](#soak-harness) on fede passes on the release commit; link
   its `summary.txt` in the release PR.

## CI hardening

Batch 1 of the [database-grade testing program](../ROADMAP.md#database-grade-testing-program).

### Reproducing-test rule

Every bug fix ships with a test that fails without the fix: run it against the
unfixed code before pushing, and name it in the PR (the
[PR template](../.github/pull_request_template.md) asks for it). The list in
[Bugs reproduced by these regressions](#bugs-reproduced-by-these-regressions)
follows this rule.

### Single-CPU runs

The `single-cpu` job in `ci.yml` builds the tests normally, then runs
`spawned-concurrency` with `TOKIO_WORKER_THREADS=1` and the whole workspace,
both under `taskset -c 0`. `TOKIO_WORKER_THREADS` sizes every runtime built
without an explicit worker count (`spawned_rt::tasks::run`, `Runtime::new`,
`#[tokio::test(flavor = "multi_thread")]`). Tests that build runtimes with an
explicit count (`worker_threads = 2`) keep it; `taskset` still confines all of
their workers, dedicated actor threads and blocking-pool threads to one CPU.
Tests that need two allowed CPUs check `affinity::available_cpus()` and skip
with a message (`pinned_shards_preserve_explicit_cpu_placement_and_cross_shard_requests`,
the partial-construction case in `execution_profiles`). Locally on Linux:

```sh
TOKIO_WORKER_THREADS=1 taskset -c 0 cargo test --locked -p spawned-concurrency
taskset -c 0 cargo test --locked --workspace
```

macOS has no `taskset`; `TOKIO_WORKER_THREADS=1` alone exercises the worker
setting but not CPU confinement.

### Supply chain

`deny.toml` configures cargo-deny (advisories, licenses, duplicate versions as
warnings, crates.io as the only source) and `.cargo/audit.toml` configures
cargo-audit; the `supply-chain` CI job runs both. Unmaintained, unsound and
yanked crates fail the job like vulnerabilities. Accepted advisories, each
reachable only through a private experiment feature:

| Advisory | Crate | Path | Why accepted |
|---|---|---|---|
| RUSTSEC-2026-0194, RUSTSEC-2026-0195 | quick-xml 0.26 | pprof 0.15 → inferno 0.11, `profiling-experiments` (Linux) | Writes flamegraphs of our own profiles, parses no untrusted XML; pprof 0.15 is the latest release |
| RUSTSEC-2025-0057 (unmaintained) | fxhash 0.2 | monoio 0.2, `io-experiments` | Benchmark-only backend; no monoio release drops it |

The only license exception is CDDL-1.0 for `inferno` (same pprof path). Keep
both ignore lists identical and drop entries when the upstream path moves.

```sh
cargo deny --all-features check
cargo audit
```

### Leak tests and LeakSanitizer

`concurrency/tests/leaks.rs` checks, through public APIs, that shutdown
releases what an actor owned:

| Resource | How it is observed |
|---|---|
| Actor state | A `Weak` sentinel held in the actor struct |
| Dedicated actor threads, execution-group workers | A sentinel moved into a thread-local; only the thread-exit destructor releases it |
| Tokio tasks (run loops, timers, monitor waiters) | `num_alive_tasks()` on a dedicated runtime, including demonitor, owner exit and target exit |
| Registry entries | `registry::registered()` after supervisor and system shutdown, and after an owned guard drops |
| User-held handles | `Arc::downgrade` of a `Recipient` (which wraps an `ActorRef`) stored in another actor |
| Supervised generations | One sentinel shared by every generation of every child |

It covers native actors, all three `tasks` backends, supervisors with restarts,
`ActorSystem` groups and parked helpers. An actor reference cycle (A holds B's
`ActorRef` and `Recipient`, B holds A's, plus a link) is released once both stop.
Actors are not reference counted: an unstopped cycle keeps running after every
external handle is dropped, by design, until something stops it
(`unstopped_cycle_keeps_running_until_explicit_stop`).

Framework-internal clones of `ActorRef`/`Context`, link tables and native
monitor waiter threads have no public introspection. The `lsan` job in
`verification.yml` covers them: it runs the lifecycle suites and `leaks.rs`
with `-Zsanitizer=leak` on nightly (same `-Zbuild-std` setup as `tsan`), and
LeakSanitizer reports any allocation unreachable at process exit, such as an
`Arc` cycle between handles.

The linked-delivery callback/subscriber panic regression runs in a bounded
subprocess, so a containment regression cannot abort the test harness. Its
panicking payload destructors raise zero-sized secondary payloads: `dispose_panic`
intentionally forgets those to bound recursive destructor panics, and the test
asserts their Drop is never called. Non-zero-sized secondary payloads deliberately
retain their box allocation (the original boxed `&str` payloads leaked 16 bytes
each). LSan stays enabled in both processes; this test adds no sanitizer
suppression or environment override.

```sh
RUSTFLAGS=-Zsanitizer=leak cargo +nightly test -Zbuild-std \
  --target x86_64-unknown-linux-gnu -p spawned-concurrency --lib --test leaks
```

### Flaky tests

The nightly `flaky-tracking` job (`actor-scenarios.yml`) runs the workspace
five times with cargo-nextest's `flaky` profile (`.config/nextest.toml`: two
retries, JUnit output). `scripts/flaky-report.py` merges the JUnit reports and
fails the job if any test needed a retry or both passed and failed across runs;
a retry never turns a flaky test into a pass. The reports and a summary table
are uploaded as the `flaky-test-reports` artifact. A flaky test is a bug:
reproduce it, fix it under the reproducing-test rule, or file it.

```sh
cargo nextest run --workspace --profile flaky
python3 scripts/flaky-report.py target/nextest/flaky/junit.xml
```

Tests wait on the effect they need, not on the clock. A fixed sleep before an
assertion, or a short timeout used as a latency budget, fails on a loaded
machine even when the code is correct. In unit tests, `test_wait::wait_for`
and `wait_for_async` (`concurrency/src/test_wait.rs`) poll the real condition
with a 10 s bound, and the test then asserts on the value they return; prefer
`join`/`wait_exit` or an explicit release channel when the code under test
offers one. Bounds such as `LIMIT` are hang detectors. A fixed sleep remains
only where a test checks that something does *not* happen (a demonitored
`Down`, an unlinked `Exit`, a duplicate signal): it can let a bug through, but
cannot fail a correct run.
