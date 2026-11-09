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

## Verification layers

`mailbox_drop_panic` runs a subprocess matrix of five task placements, 1/2/16
queued panicking destructors and both ordinary/handler-panic shutdown. Its enabled
parent test checks that the child actually ran all 30 cases, observes one Exit
and one Down, verifies pending reply release and preserves the earlier failure.
Only the subprocess helper is ignored; the regression runs in normal CI.
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
| User level | Actor-level ordering exploration | Bugs in users' actor systems, reproducible from a seed | Planned ([roadmap](../ROADMAP.md) rank 20) |
| Production | Lifecycle monitors derived from the TLA+ invariants | A live node or recorded canary run breaking a modelled invariant | Planned ([roadmap](../ROADMAP.md) rank 5c; [design](research/systematic-testing.md#a-pobserve-for-spawned)) |
| Memory safety | ThreadSanitizer, Miri | Data races and undefined behavior | TSAN in CI; Miri waits until unsafe code is proposed (all crates `#![forbid(unsafe_code)]`) |
| Test quality | cargo-mutants | Tests that still pass when the code is broken | Weekly over supervision and system code |
| Compatibility | cargo-semver-checks | Unexpected public API removals | Defaults/metrics and the explicit-async migration must remain patch-compatible with v0.6; raw no-default removals are checked against an exact declared list, not reported as compatible. Protocol macros also need trybuild. |
| Feature separation | cargo-hack | Missing dependencies hidden by dev-dependency feature unification | CI builds each library feature, default, no-default and all features without dev dependencies; not the full feature powerset |

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
| `request_histories` | Independent bounded search for a legal sequential history, with wrong-reply/duplicate/lost-effect negative controls | At most 16 operations and 100,000 search states; exhausted search is inconclusive and fails, not a pass |
| `delivery_simulation` | Enumerate 125 states of a finite cooperative-stop model and replay 500 witnesses across four backends, plus seeded scripts and an audit digest | Controls handler-progress gates, not arbitrary OS/Tokio scheduling, network, clocks or all lifecycle signals |
| `payload_stress` | Unique owned payloads from 128 concurrent producers; verify FIFO delivery or complete destruction on stop | Explicitly bounded payload volume; not a whole-process memory proof |
| `process_failures` (Unix) | Abort or kill an owned subprocess after an actor effect; check process failure does not promise cleanup callbacks or rollback | Temporary marker is not a storage durability acknowledgement |
| `benchmark_startup` | Startup rendezvous opens for all participants, aborts after setup failure and has a finite timeout | Tests the research driver, not production actor supervision |

```sh
cargo test --locked -p spawned-concurrency --test failure_windows --test request_histories --test delivery_simulation --test process_failures --test benchmark_startup
SPAWNED_HISTORY_CASES=128 SPAWNED_HISTORY_SEED=20261106 cargo test --locked -p spawned-concurrency --test request_histories -- --nocapture
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

## Toolchain

The repository's `.tool-versions` selects Rust 1.93.0. The workspace tests,
including generated cases and compile diagnostics, have been validated with
that compiler. Proptest and trybuild are dev-dependencies; proptest's unused
default features are disabled. Cargo generates the lockfile.

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
children), jitter seed mixing and simple accessors.

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
