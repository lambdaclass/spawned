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

## Verification layers

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

No single tool covers lifecycle correctness; each layer catches a different
class of bug. Coverage for v0.7.0:

| Layer | Tool | Catches | Status |
|---|---|---|---|
| Design | TLA+ | Holes in lifecycle and restart rules | Bounded OneForOne, cohort strategy and nested ownership models (`verification/tla`); wider base lifecycle remains |
| Spec and code | Trace validation | The implementation doing something the model forbids | Real OneForOne/cohort/nested trace projections plus invalid-trace and model-mutation controls; not all runtime schedules |
| Primitives | Loom | Atomic and memory-ordering bugs in cancellation, ready gate and wakeups | Exists; expand to mailbox publication versus shutdown |
| Whole runtime | Shuttle | Scheduling races in multi-actor tests too large for Loom | Not started |
| User level | Actor-level ordering exploration | Bugs in users' actor systems, reproducible from a seed | Planned ([roadmap](../ROADMAP.md) rank 20) in `spawned-test`; mocks, probes and fault injection exist (phase A) |
| Production | Lifecycle monitors derived from the TLA+ invariants | A live node or recorded canary run breaking a modelled invariant | Planned ([roadmap](../ROADMAP.md) rank 5c; [design](research/systematic-testing.md#a-pobserve-for-spawned)) |
| Memory safety | ThreadSanitizer, Miri | Data races and undefined behavior | TSAN in CI; Miri waits until unsafe code is proposed (all crates `#![forbid(unsafe_code)]`) |
| Leaks | LeakSanitizer, `leaks.rs` | Actors, threads, tasks, registry entries and handle cycles that outlive shutdown | LSan job beside TSAN; leak assertions in every test run ([CI hardening](#ci-hardening)) |
| Scheduling pressure | `taskset -c 0`, one Tokio worker | Code that assumes parallel progress; races that need preemption on one core | `single-cpu` CI job |
| Supply chain | cargo-deny, cargo-audit | Vulnerable, unmaintained or unsound dependencies; license and source drift | `supply-chain` CI job; accepted advisories listed in `deny.toml` |
| Flakiness | cargo-nextest with retries | Tests that pass only on retry | Nightly `flaky-tracking` job; a retry is reported, never silently passed |
| Pure arithmetic | Kani | Overflow, panics and off-by-one in backoff, jitter, the restart window and recovery ids, for all inputs in a bound | `concurrency/src/kani_proofs.rs`, weekly and on `concurrency/src` PRs |
| Test quality | cargo-mutants | Tests that still pass when the code is broken | Weekly over supervision and system code |
| Compatibility | cargo-semver-checks | Unexpected public API removals | Defaults/metrics and the explicit-async migration must remain patch-compatible with v0.6; raw no-default removals are checked against an exact declared list, not reported as compatible. Protocol macros also need trybuild. |
| Feature separation | cargo-hack | Missing dependencies hidden by dev-dependency feature unification | CI builds each library feature, default, no-default and all features without dev dependencies; not the full feature powerset |
| Small arithmetic | Kani | Overflow, panics and wrong bounds in backoff, jitter, the restart-budget window and generation counters | Planned ([roadmap](../ROADMAP.md#database-grade-testing-program) batch 1; [verification tools](research/verification-tools.md)) |
| Rare lifecycle paths | Failpoints (`BUGGIFY`-style) | Restart, shutdown and registration windows reached on purpose | In review (PR #204) |
| Whole runtime, deterministic | Simulation backend for `spawned-rt::tasks` | Async races replayed from a seed with virtual time | Planned spike ([roadmap](../ROADMAP.md#simulation-backend-spike)); first step toward rank 20 |
| Cancellation and panic safety | Drop each public future at each await; panic in each user callback | State left inconsistent by a dropped future or a callback panic | Planned ([test techniques](research/test-techniques.md#cancellation-safety-in-async-rust)) |
| Cross-backend | Metamorphic tests | Backend-specific differences in replies, exits and per-sender order | Planned ([test techniques](research/test-techniques.md)) |
| Performance regressions | dhat allocation budgets; open-loop load (HdrHistogram) | Per-message allocation growth; tail latency hidden by coordinated omission | Planned ([test techniques](research/test-techniques.md)) |
| Platforms | Linux and macOS, x86 and arm64 CI jobs | Ordering, affinity and atomics assumptions tied to one platform | Planned ([test techniques](research/test-techniques.md)) |

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

## Scope

This is generated, model-checked scenario testing on real runtimes, not an
exhaustive scheduler or coverage-guided fuzzer. It does not cover bounded mailboxes or supervisor restart policies.
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
- Compile tests accept a working actor/protocol and snapshot diagnostics for
  unsupported async methods, associated types, and missing receivers.
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

## Debug future layouts

`future_layout.rs` compares owned handle, send and protocol-call shapes with the
pre-guard layout and checks concrete request-future budgets on Linux x86_64.
Run it with default features and `--features metrics`. The receiver-guard change
enlarged ActorRef/Context by one word; enclosing owned RPC futures repeated that
growth even though borrowed request futures stayed unchanged. Shared sender
metadata restores the measured layouts without per-message boxing.

The [expanded probe and results](../legacy/verification/evidence/v071-review-round2/README.md)
compare fc35abaf, 35bea650 and the compact sender, including an RPC-shaped nested
future. This supersedes the earlier borrowed-request-only conclusion. Matching
layout is a regression check, not proof that Ethrex's RPC tests pass at a 2 MiB
debug stack; that client rerun remains a review gate.

## Execution guarantees

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

```bash
nix run .#check-tla                                   # model check (TLC via the flake)
cargo test -p spawned-concurrency --features lifecycle-trace --test supervision_trace
nix run .#check-tla -- --traces target/lifecycle-traces
```

All crates are `#![forbid(unsafe_code)]`, so Miri is not run; ThreadSanitizer
runs the lifecycle tests in CI. `cargo mutants` over `supervision.rs` and
`system.rs` runs weekly. The remaining surviving mutants are race-only guards
(`can_start`, `register`, `lease` checks only reachable in narrow windows),
redundant system-level safety nets (group teardown also stops supervised
children) and simple accessors. The backoff and jitter seed mixing mutants are
killed by the Kani proofs below.

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
