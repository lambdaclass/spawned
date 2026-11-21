# Changelog

## Unreleased

- CI reliability: drop `magic-nix-cache-action` (its GitHub Actions cache
  backend failed runs with FlakeHub authentication, cache-service and SSL
  errors); give TLC 75% of RAM (`TLC_HEAP`) so liveness checking no longer runs
  out of the JVM's default heap. Client-compat now warns, in the job summary,
  when a client's graph holds a second spawned version. The PR template asks
  for an independent review of concurrency changes and trace-validated models.
- `#[actor]` marks the generated await-free dispatch
  (`__spawned_handle_immediate`) `#[inline]`. Since the module split (#278)
  the compiler had stopped inlining it into the envelope, adding 22
  instructions per `send` (+3.6% over 0.8.0 on `async_ct_send_*`, +0.4% to
  +1.2% on the async request benchmarks). With the hint every gated async
  benchmark is below 0.8.0 (send -3.4%, requests -0.6% to -0.9%).
- Fixed: in `threads` mode every `send_after` / `send_interval` left a
  callback (and its wake-up channel) on the actor's stop token until the actor
  stopped, so a long-lived threads actor that kept arming timers grew without
  bound. `spawned_rt::threads::CancellationToken` gains
  `register_on_cancel`, whose `CancelRegistration` removes the callback in
  O(1) when dropped (callbacks still fire in registration order), and
  `callbacks_registered`; the timer thread now holds its registrations and
  drops them when it fires or is cancelled. Tasks-mode timers were not
  affected (Tokio removes a cancellation waiter when its future is dropped).
- `metrics`: gauges of spawned's own retained structures, so memory growth
  can be attributed to the framework (or ruled out) from one scrape, without a
  heap-profiling build: `spawned_reply_slots_alive`, `reply_tokens_alive`,
  `pending_replies_entries`, `reply_registries_alive`,
  `deadline_heap_entries`, `deadline_service_entries`,
  `deadline_service_actors`, `jobs_alive`, `monitors_alive`,
  `link_records_alive`, `registered_names` and `timers_alive` (docs/GUIDE.md,
  "Retained internal structures"). Counted by a zero-sized field at each
  structure's creation and drop (one relaxed atomic each end, no lock); the
  deadline service's sizes are read at scrape. Compiled out without the
  feature. A churn test in both backends checks that every gauge returns to
  its baseline (`tests/metrics_retained.rs`), which doubles as a leak test.
  `timers_alive` is how the threads-mode timer callback leak above was
  found.
- `metrics`: the disposal worker pool's gauges, `spawned_disposal_queue_entries`,
  `disposal_workers_alive` and `disposal_workers_idle` (read at scrape under
  the pool's lock), and the counter `spawned_disposal_workers_capped_total`
  (times all 64 workers were busy, once per episode, beside the existing
  warning). Compiled out without the feature; no change on the hand-off path
  otherwise. A test expires deadlines in both backends with blocking and
  non-blocking disposal and checks the queue drains and every worker retires
  after its keep-alive (`disposal.keep_alive` failpoint with `failpoints`).
  The soak harness judges the three gauges.
- `perf-gate` also holds every PR to a cumulative budget: the head may grow
  at most 3% instructions per operation over the last release tag
  (`git describe --match 'v*'`), not only over its merge base, so small
  per-PR regressions cannot add up across a release cycle. The job summary
  table shows release, base and head with both changes. The release binary
  is built once per release and cached (`hotpath.py build-release` on pushes
  to main). Callgrind runs are pinned to one CPU and
  `threads_request_seq` takes the median of nine run pairs: under load the
  host scheduler had swung it by several percent, as much as the threshold.
- Fix the flaky `supervision::transient_startup_timeout_retries_after_actual_cleanup`
  (a 20 ms startup deadline raced the fast starts on a loaded host, which also
  made cargo-mutants report surviving mutants as caught): the replacement's
  `started()` is now held until the test sees its deadline fire, and the test
  also checks that the retry waits for that attempt's cleanup. The trace
  scenarios `trace_first_start_timeout_fails_start` and
  `trace_startup_timeout_retries` (same 20 ms / 200 ms race) hold their slow
  start the same way; their recorded traces are unchanged event for event.
  `tasks::actor::monitor_cleanup_regressions` no longer hangs when built with
  `--cfg spawned_sim`: it polled with a blocking sleep without driving the
  simulated runtime, on a thread whose clock is virtual.
- ROADMAP: 0.9 is now soundness and introspection; the unified start API
  (G1, G2, G8) and `{shutdown, Term}` exits (G7) move into 0.9, and the
  admission package is designed during 0.9.
- `model-sync`: a design-only TLA+ model (`design_only = true`: Start,
  Introspection, DynamicSupervisor) stays allowlisted as TLC-only only while
  no code maps to it; the PR that implements it must add its trace
  validation.
- Kill the meaningful surviving `cargo mutants` in the supervisor
  (`supervision.rs`, `supervision/*.rs`) with deterministic tests: unit tests
  in `supervision/guard_tests.rs` for the supersession check of
  `finish_unless_recovering`, admission closure, stop escalation, start
  errors, escaped controller failures and the stop-during-round and
  self-exit kill paths; a failpoint test for a stuck cleanup's late exit; and
  a simulator seed sweep (`supervision_races`) for a OneForAll child that
  decides to stop while a recovery round is assigned to it. Test-only; the
  per-mutant classification is in `docs/TESTING.md`.
- Fix the flaky `tla-traces` coverage gate (`LOST coverage: Cohorts
  A|OneForAll|CEnd/complete`): the cohort trace tests now wait for the restart
  round to record `CEnd complete` before shutting down, instead of racing it
  (the selection trace missed it in 24 of 30 runs). New
  `lifecycle_trace::recorded_with` test helper.

- **Breaking:** remove the no-op `backend-experiments`, `io-experiments` and
  `profiling-experiments` features from `spawned-rt` and
  `spawned-concurrency` (they only enabled `async` since 0.8.0). The
  experiments patch in `benchmarks/experiments` re-adds what it needs. The API
  policy now declares 0.9 a breaking release against v0.8.0.
- Trace validation for the `Coordinator` TLA+ model (#266): with
  `lifecycle-trace`, `ActorSystem` shutdown records one `Coordinator.tla`
  action per step (ordered stop, grace expiry and kill, panic fallback,
  runtime release; compiled out otherwise). New `coordinator_trace` test,
  `TraceCoordinator.tla`, seven trace negative controls and a coverage floor
  of 9 of 9 action keys; `Coordinator` is no longer TLC-only.
- Check the last three partially checked guarantees of the semantics
  contract, so every one of the 50 now has a check that fails if it breaks:
  L3 (reason precedence through `stopped()` and destructor panics, every
  requested reason, both modes and every placement), L7 (an exiting
  observer's monitors are cancelled before its exit is published) and X7 (a
  non-trapping linked stop waits for the running handler and discards the
  queue). Each check was shown to fail under local mutations of the code.

- Three new scenario-fuzzer oracles, so the fuzzer now catches planted bugs
  15, 17 and 22, which it used to miss: X2 (no `Down` handled after
  `demonitor` returned), T2 (a `send_interval` starts no round after its
  actor's exit is published) and L7 (an exited observer's monitor waiter does
  not outlive it, checked by an end-of-run probe). The minimized bug-15 input
  is kept as the regression `fuzz/tests/down_after_demonitor.rs`. Test-only:
  no library change.
- **Breaking (0.9):** clean exits can carry a payload, OTP's
  `{shutdown, Term}` (roadmap G7). `ExitReason::Shutdown` becomes
  `Shutdown(Option<Arc<str>>)`; build the payload form with
  `ExitReason::shutdown_with(why)` and pass it to `stop_with_reason` or
  `exit`. Restarts, crash metrics, link propagation, `trap_exit` and the TLA+
  trace classes treat it exactly like `Shutdown`; `Down`, `Exit`, `join()`
  and `ChildSnapshot::last_exit` carry the full reason. New `is_shutdown()`
  and `shutdown_payload()` helpers; `ExitReason` now derives `Eq`; its size
  is unchanged (24 bytes). See docs/MIGRATION.md, "0.8 to 0.9".
- Fix the flaky `tla-traces` coverage gate (`LOST coverage: Cohorts
  A|OneForAll|CEnd/complete`): the cohort trace tests now wait for the restart
  round to record `CEnd complete` before shutting down, instead of racing it
  (the selection trace missed it in 24 of 30 runs). New
  `lifecycle_trace::recorded_with` test helper.

- Keep user code off the shared deadline-service thread (the known 0.8
  limitation). The thread still decides expiry and returns credits, but the
  wakers and payload destructors an expiry releases (a caller's waker, a
  discarded scheduled emit, a displaced job result, a `done` closure and its
  captures) now run on an elastic pool of disposal workers, one item at a
  time, so a blocking destructor or waker no longer delays other actors'
  deadlines or other callers of the same actor. A payload handed off still
  waits for its owner's death point (R6). No cost
  on requests that do not expire. New regression tests (tasks and threads),
  a Loom case for the handed-off wake-up, an `InlineDisposal` negative
  control in `DeferredReplies.tla`, and a new trace-validated model,
  `Disposal.tla` (expiry and hand-off, per-item queueing, the pool's start
  rule, idle count and cap, worker wake-ups after the transition, two-step
  payload drops held until the death point), validated against traces that
  record every hand-off and worker step (`disposal_trace`, with
  `disposal.item`, `disposal.keep_alive` and `disposal.batch_serial`
  failpoints; the last records a control trace from code with the
  shared-batch bug, which must be rejected) and five negative controls.
  Empty payloads are no longer handed off, and a due entry is dropped in
  place unless it may hold an unread reply.
- Regenerate the 11 planted-bug patches (04, 10-17, 19, 20) that no longer
  applied, each planting the same fault where the code now lives, and check in
  the required `model-sync` job that every planted patch still applies.
- Move the private `backend-experiments`, `io-experiments` and
  `profiling-experiments` features out of `spawned-rt` and
  `spawned-concurrency` into a standalone, unpublished crate in
  `benchmarks/experiments/`. The published crates lose their optional
  dependencies (monoio, pprof, async-task, crossbeam-deque) and the experiment
  branches in the dispatch loop; the feature names stay as no-ops until 0.9 so
  0.8 stays patch-compatible. Default builds are unchanged. The
  experiments stay runnable: `benchmarks/experiments/prepare.sh` applies the
  hooks to a scratch tree (see `benchmarks/README.md`).
- Split the largest source files without changing behaviour or API:
  `tasks/actor.rs` and `threads/actor.rs` into `actor/{envelope,context,actor_ref,lifecycle}.rs`
  plus one file per test module, `supervision.rs` into
  `supervision/{child,state,builder,run,policy_tests}.rs`, and the macros
  crate into one module per macro (`protocol`, `actor`, `lints`,
  `send_handler`, `sim_test`). Test names are unchanged.
- Clarify dispatch admission when a stop races a message, and check deferred
  token ownership before drop callbacks in debug builds. Release behavior is unchanged.
- Keep the Loom models from drifting: a clippy ban (`concurrency/clippy.toml`,
  checked under the Loom cfg) on direct std/`parking_lot` synchronization in the
  Loom-checked cores, and `model-sync` now requires a PR changing such a core to
  update its Loom harness (`[loom.*]` in `verification/model-map.toml`) or carry
  the `no-model-change` waiver.
- Threads mode: `ActorRef` and `Context` share one `Arc<ActorShared<A>>`, as
  tasks mode does, instead of inlining thirteen fields each (one word per
  handle, one reference-count increment per clone; step 0 of the 0.9
  introspection design). No API or behaviour change. Callgrind Ir/op against
  `main`: threads request +0.9–1.0% (gate ≤ 3%), tasks gated paths −0.1 to
  −0.3%, threads handle clone −91%; threads handles shrink from 128 to 8 bytes.

## 0.8.0 — 2026-11-15

0.8 is a breaking release. It adds deferred replies and jobs, aligns exit
signals, startup and runtime drop with Erlang/OTP, and cleans up the API before
the road to 1.0. Upgrade notes, with an old-call → new-call table, are in
[docs/MIGRATION.md](docs/MIGRATION.md) under "0.7 to 0.8".

### Highlights

- **Deferred replies and jobs (gen_server `noreply` + `reply/2`).** A handler
  can answer later through a typed `ReplyTo<R>` token with an explicit reply obligation
  (`PendingReply`, `PendingReplies`, `#[deferred_handler]`). Pipe-to-self jobs
  (`pipe`, `pipe_blocking`, `pipe_reply`, `pipe_reply_blocking`, permits via
  `reserve_job`/`reserve_blocking_job`) run work off the actor and deliver the
  result as a message, with keyed replacement, deadlines, `emit`/`emit_at`,
  bounded outstanding work (`max_outstanding`, `max_native_jobs`) and fencing
  when the actor stops.
- **Exit signals as in OTP.** `kill`/`Killed`, `ctx.exit` with `exit/2`
  semantics, `NoProc` for links and requests to dead actors, `try_link`,
  fenced `unlink`/`demonitor` (`demonitor` flushes a queued `Down`, as
  `demonitor(Ref, [flush])`), `stopped()` skipped on kill, an untrapped
  signal or runtime drop, supervised non-trapping children skip `stopped()` on shutdown.
- **One death point.** However an actor dies (normal stop, panic, kill,
  runtime drop, never polled), outstanding callers are answered and signals
  fenced before post-death destructor, waker or tracing callbacks run;
  terminal publication comes last.
- **Supervision as in OTP.** The restart budget counts recovery rounds, not
  rebuilt children; first-start failures abort startup
  (`SupervisorError::FailedToStartChild`); one exit reason when the runtime
  drops an actor.

### Breaking changes

This is a summary; the [migration guide](docs/MIGRATION.md#07-to-08-api-call-site-table)
contains the complete API call-site table and behavior changes.

- `Handler::handle` receives a reply token. Replace `#[handler]` with
  `#[request_handler]` or `#[send_handler]`; send-only handlers return `()`.
- `request_raw`/`request_raw_until` → `send_request` with one `Deadline` type
  (`Duration`, `Instant` or `Deadline`); pass `Deadline::Never` to preserve
  the unbounded 0.7 raw-request behavior. Ordinary `request()` keeps its 5 s
  timeout. Only the caller's expiry decides timeout; a late reply that wins
  before expiry is delivered, as in OTP.
- Job API: one call per shape, options first; `JobOptions`, `SupervisorConfig`,
  `Backoff` and metrics config are `#[non_exhaustive]` with builders.
- `ActorError`, `DeferError`, `Delivery`, `InsertError` and other growing
  enums are `#[non_exhaustive]`; `ActorError` gains `target_gone`,
  `is_transient`, `is_timeout`, `exit_reason`. `InsertError::kind()` and
  `into_parts()` let a caller always answer a refused token.
- `ExitReason` gains `Killed`/`NoProc` and is non-exhaustive; exhaustive
  matches need a wildcard. Read-only structs such as `Down`/`Exit` require
  `..` in patterns; `Down` gains the monitored actor id.
- `ActorError::ActorStopped` carries the final exit reason.
- `Receiver` is sealed. `Response::is_ok`/`is_err` now panic on an un-awaited
  response; await it first (or use `into_result()` off a runtime).
- Behaviour: the restart budget counts rounds; a supervised child that does
  not trap exits skips `stopped()` on shutdown; `demonitor` flushes.

### Added

- Simulator D1–D3: deterministic clock/entropy interception in the separate
  simulation-only `spawned-sim-libc` crate, inline simulated blocking work,
  runtime timer/interval/JoinSet adapters and strict escape diagnostics.
  Simulated runs use fresh threads; test bodies must be `Sync`. The platform
  and randomness-interception limits are documented in TESTING and MIGRATION.
- `#[actor]` compile-time lints: shared mutable state (`Arc<Mutex<_>>` and
  similar) in an actor, and `panic = "abort"`; opt-outs
  `allow_shared_state`, `allow_panic_abort`.
- `#[spawned_concurrency::sim_test]` re-exported; deterministic simulation
  example.
- The simulator fails a test whose actor turn runs too long without yielding
  (`SPAWNED_SIM_TURN_BUDGET`).

### Verification and testing

- Await-free generated async handlers dispatch without a boxed turn future
  when their messages have no destructors and their bodies contain no opaque
  macros, nested items, closures or async blocks. Other handlers retain the
  original future adapter. Regressions cover lazy direct calls, local item and
  anonymous type identity, cooperative scheduling, drop order, panic
  containment, macro hygiene and causal tracing. Deferred-token retirement
  and the reduced turn bookkeeping have production-core Loom checks.
- TLA+ models `Lifecycle`, `DeferredReplies` and `DeferredJobs`, each tied to
  the code by trace validation with full action coverage, alongside the
  updated `Supervision`, `Cohorts` and `Coordinator`; named negative
  controls check key safety and liveness properties.
- Required CI checks include `sim` (simulated suites), `model-sync`
  (models must change with the code they model), and `perf-gate` (no gated
  hot-path benchmark may grow more than 3% in instructions per operation).
- Request optimization keeps monotonic service deadlines separate from
  relative runtime waits, and reduces envelope and future storage; no coarse
  clock is introduced. Paused-clock regressions cover the clock domains.
  The receiver timer model and production-slot Loom races cover the bookkeeping.
- Semantics coverage: 47 of 50 guarantees have a check that fails if they
  break. Planted-bug matrix: 23 of 24 realistic bugs caught, each by two or
  more layers. Scenario-fuzzer oracles for restarts, budgets, backoff and
  links. Soak harness for leaks. Bounded waits: hung tests fail by name.

### Scheduling limitation

The shared deadline-service thread may run user wakers and payload destructors.
A blocking callback delays other actors' heap-driven expiry and scheduled job
emits. The [guide](docs/GUIDE.md#deferred-replies-and-pipe-to-self) describes which timers are
separate; callback isolation is follow-up work. No runtime behavior changed
when documenting this limitation.

### Fixes and implementation hardening

This includes fixes to new 0.8 features found during pre-release validation.

- A stop racing mailbox receive cannot dispatch a request sent after stop
  returned. Rejected receives stay held until death-point caller discharge,
  including runtime drop, before payload destruction. Untraced actor and
  shared-core Loom regressions cover the gap without changing deadlines or
  adding a second normal-path stop read.
- Actor message loops use a private atomic stop flag instead of locking
  Tokio's cancellation state before every message. The latched Tokio waiter
  still wakes parked actors and helpers. Cancellation and deadlines retain
  their existing semantics; the real stop core has Loom ordering/race checks.
- Link-table transactions acquire locks in immutable actor-ID order, keeping
  the total-order deadlock rule independent of allocator addresses. Loom's
  three-actor race covers all six ID orderings and parallel model replay.
- Failed deadline-worker initialization releases its service allocation on
  error or unwind and leaves initialization retryable. Startup still fails
  rather than silently accepting an unserviceable deadline.
- Simulation rejects native actor creation and message/request admission from
  a virtual world, preventing comparisons between real and virtual deadline
  ticks. Native tests outside a world retain real time.
- Simulator owners include setup before `block_on`; test reply pairs have
  fresh per-world owners, and pending tasks release owner/runtime cycles
  during runtime drop. Native helpers retain real time without creating a
  virtual world.
- Refused job notifications and emits drop captures outside the trace lock;
  linked-exit diagnostics and panic-payload disposal run after the receiver
  table and outermost trace section, including late `NoProc` delivery.
- Refused job results and immediate emits contain destructor panics in both
  execution modes. Cancellation, deadlines, keyed replacement and owner
  discharge retain containment for held results.
- Simulated deadline services stay with their owner rather than the caller's
  current context; foreign-thread scheduling fails the owning simulation as
  a determinism escape. Native owners without a simulation use real time.
- Compact reply slots keep ordinary values inline and deferred metadata in a
  cold allocation, with a 56-byte scalar layout budget. Slot handoff uses a
  non-poisoning parking_lot lock through the Loom synchronization seam. A
  refused second publisher preserves the first winning reply.
- Unused job completion factories drop outside the lifecycle trace lock.
  The destructor regression runs with tracing enabled in CI.
- The scenario-fuzzer oracle includes cleanup failures when deciding whether
  a Transient child must restart, even when its published reason is Normal.
  The original failing tape replays strictly and has additional seed coverage.
- Job completion closes emit admission atomically. Retaining a `JobCtx`
  cannot schedule new messages during held-result release; blocked emitters
  wake at completion. Join waiters are deduplicated and woken outside locks.
- Request envelopes use message presence as their dispatch marker and
  inline construction. The performance gate also covers manifests, lockfiles
  and toolchain changes; its 3% instruction limit remains unchanged.
- Request deadlines remain correct when Tokio time is paused while real
  time advances. Runtime clock instants are not stored as service deadlines.

## 0.7.3 — 2026-11-10

Patch release with one supervision behaviour change: a failed restart (a
replacement that stops before readiness) is retried within the restart
budget, and OneForAll rebuilds the whole cohort, as in OTP; see the
[migration notes](docs/MIGRATION.md#073). Also: a deterministic simulation
backend with replay, shrinking and PCT/DFS exploration (opt-in via
`--cfg spawned_sim --cfg tokio_unstable`), stack-depth budgets, trace coverage
against the TLA+ models, client compatibility CI and test reliability fixes.
API changes are additive only (`spawned_rt::time`, `spawned_rt::tasks::{SimTest,
sim_test, yield_now}` and `#[spawned_macros::sim_test]`, which runs a normal
Tokio test unless the simulation cfg is set).

- Fix: a Transient child's replacement that stopped `Normal` or `Shutdown`
  before readiness failed the whole supervisor ("non-restartable child failed
  startup"). It is now a failed restart attempt, retried within the restart
  budget, as OTP retries a failed restart whatever the child's restart type.
  A child's first startup is unchanged: a Transient child stopping `Normal` or
  `Shutdown` before its initial readiness still fails the start (semantics V7,
  as OTP fails `init` when a child's start fails). Found by trace coverage:
  trace validation rejected a replacement stopping with `Shutdown` while the
  supervisor was open, which `Supervision.tla` allowed only after closure; the
  model now allows it, `Decide` retries failed restart attempts, and
  `negative_checks.py` rejects a trace edited back to failing the tree.
  Under OneForAll a replacement failing before readiness (including a factory
  error, which already took this path) now starts a new recovery round, so the
  whole cohort is rebuilt again as OTP does; before, only that child was
  retried within the round. RestForOne already matched. `Cohorts.tla` allows
  one start per child and round (`OneStartPerRound`), with the old in-round
  retry as a negative control (`Fault = "InPlaceRetry"`, TLC and Apalache).
- Simulation backend spike: `RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable"` swaps
  `spawned_rt::tasks` spawning and time for a deterministic single-thread
  executor (`spawned_rt::tasks::sim`: seeded choice of the next ready task,
  virtual time that advances only when every task is idle). Unmodified async
  actors, supervisors and async execution groups run on it; `concurrency/tests/sim.rs`
  sweeps the PR #204 never-started-child race (closed by the v0.7.1 lifecycle
  fixes) and asserts no seed reproduces it. Thread and pinned
  groups are rejected, `spawn_blocking`/`block_in_place` panic, and foreign-thread
  wakeups, deadlocks and livelocks fail the run. Each poll runs inside a
  per-run, driverless Tokio runtime, so Tokio's cooperative budget forces a
  task that keeps receiving from a ready channel to yield (as on Tokio), and
  Tokio's `select!`/`watch` RNG is seeded from the run's seed; that seeding
  needs `--cfg tokio_unstable` alongside `--cfg spawned_sim`. A direct
  `tokio::spawn` fails the run, a poll that never returns trips a wall-clock
  watchdog (`SPAWNED_SIM_POLL_TIMEOUT`), a panicking task destructor becomes
  that task's `JoinError`, and `sleep(Duration::ZERO)` yields once.
  Native actors, the wall clock and direct Tokio timers/I/O stay outside its
  control. Default builds are unchanged.
- Simulation test harness: `#[spawned_macros::sim_test]` and
  `spawned_rt::tasks::{sim_test, SimTest}` run one async test body on Tokio
  normally and over a seed range under `--cfg spawned_sim`, printing the
  failing seed, a `SPAWNED_SIM_SEED`/`SPAWNED_SIM_TAPE` replay command and a
  decision-tape path. Tapes record every scheduling choice and clock jump,
  replay by task id (so schedules survive RNG-consumption changes), and are
  shrunk toward the canonical schedule. The supervision, group, cancellation
  and canary suites now run under both builds. New `spawned_rt::time`
  (`Instant`, `entropy`) is `std` time normally and virtual time under the cfg;
  restart windows, backoff jitter seeding, shutdown deadlines and metrics
  timings read it. Tape replay is strict (clock jumps come from the tape;
  a pick that does not apply, a short tape or leftover decisions fail as
  `replay diverged`), tapes record the spin limit, and the shrinker only keeps
  candidates that reproduce under strict replay (and stays within its run
  budget). Strict replay also checks every clock jump against the pending
  timers and the id of each canonical pick, and fails a run that stops with
  decisions left over. The busy-poll threshold halves every 100 creeps
  without an idle moment, so multi-second timeouts fire before the livelock
  guard even while other timers keep firing. `docs/TESTING.md` lists what the simulator
  does not model and which suites are compiled out under the cfg.
- Simulator exploration strategies (`spawned_rt::tasks::sim::explore`, under
  `--cfg spawned_sim`): PCT and a delay-bounded DFS that reports exhaustion,
  recorded into the decision tape and selected with `SPAWNED_SIM_STRATEGY` or
  `#[sim_test(explore = "pct:d=3", iterations = N)]`, plus planted-bug tests.
  PCT follows the paper's algorithm with a poll as the step (the task that
  runs change point `k_i` drops to priority `i`, independently of time
  order; checked against an independent implementation, including blocking
  tasks), but no probability bound is claimed. DFS reports when the depth or
  fairness cutoff truncated the space, checks that each replayed prefix met
  the same ready sets, stops at a divergence (`Report::complete()` is false
  then), keeps `SPAWNED_SIM_SEED` as every schedule's input, and the harness
  fails a body that does not replay; strategies accept `fair=N`. Each
  planted bug is checked by its own failure message, and the three-worker
  control fails only when all reads precede every write.
- Add the `stack_budget` test suite: nested request chains (async group,
  thread group, ambient runtime, threads mode), supervisor start/restart/
  shutdown and `ActorSystem::shutdown` run in child processes under a
  `RUST_MIN_STACK` budget of about 1.5x the measured minimum, so a stack-depth
  regression aborts the child and fails the test. Brought over from #217. See
  [testing](docs/TESTING.md#stack-depth-budgets).
- Test reliability: tests wait on the condition they need instead of fixed
  sleeps or tight latency budgets (`test_wait::wait_for`, `join`, explicit
  release channels). Fixes flakes seen under load in
  `threads::timer_tests::test_send_interval_and_cancellation` (asserted an
  exact tick count), `tasks::actor::tests::panic_in_started_stops_actor`,
  `monitor_observes_panic_reason`, the badly-behaved/backend isolation tests,
  `placement_failure_is_reported_and_fails_the_supervisor` (asserted the
  child's terminal state before its controller published it), two supervisor
  startup tests whose first `poll` raced the controller, a std `Barrier`
  inside async handlers that could deadlock a worker, and a proptest case
  count that broke once a regression file existed. New tests cover the
  context/request-error/unlink/Debug APIs in both modes, closed-system
  admission, nested `wait_ready`/`startup_timeout`, supervisor config limits,
  shard bounds, factory cancellation, cohort backoff, monitor registration
  cleanup and opposite-direction link lock ordering; they kill 20 surviving
  cargo-mutants. The native mailbox no longer goes through a single-use
  macro (cargo-mutants does not mutate macro bodies, so 21 mutants were
  hidden), and shard selection asserts its invariant instead of re-checking
  it. A new test pins that the nested-supervisor adapter stops its subtree in
  `stopped()`, so a panic there (e.g. from an inner waiter's waker) stays a
  `stopped()` panic with `Drop` as the backstop.
- Trace coverage against the TLA+ models: `verification/tla/trace_coverage.py`
  maps every recorded lifecycle trace onto keys defined in TLA+ (actions with
  outcome, per-child states, transitions in context, and a coarse transition
  level) of `Supervision`, `Cohorts` and `NestedSupervision`, compares them
  with the keys TLC reaches in the CI configurations, and ranks the unreached
  ones. With the `lifecycle-trace` feature, `LIFECYCLE_TRACE_ALL=DIR` makes
  every finished supervisor in any test binary write its traces (no behaviour
  change; the feature stays off by default), so the whole test suite
  contributes. The `tla` CI job prints the report on every PR and fails only
  if an action key in `coverage-baseline.json` is lost; weekly it covers every
  test binary. Six new trace tests (`coverage_trace`) reach the top gaps:
  Supervision actions in the dedicated suites go from 33 to 35 of 40 (40 of 40
  with the whole suite) and coarse transitions from 63 to about 87 of 544. One
  rejected whole-suite trace led to a model gap and a supervision bug, fixed
  separately (#233). See
  [trace coverage](verification/tla/README.md#trace-coverage).
- CI: client compatibility workflow (`client-compat.yml`) builds and tests
  ethrex and ethlambda `spawned-v0.7` against the commit under test, nightly,
  on push to main and on PRs labeled `client-compat`.

## 0.7.2 — 2026-11-10

Patch release. The main fix restores ethrex's debug RPC tests under the
default 2 MiB stack by making `ActorRef`/`Context` one pointer again. The
macros now reject two invalid inputs they used to accept (see below). No other
API changes.

- `tasks::ActorRef` and `tasks::Context` are one pointer wide again (8 bytes,
  was 80, 88 with `metrics`; spawned 0.5 was 32): both share one `Arc`'d
  state. Applications move handles by value through their futures, and debug
  builds give every move its own stack slot, so the 0.7 handles cost ethrex's
  debug RPC dispatch ~136 KiB of extra thread stack. Clone is one refcount
  increment; one extra allocation per actor start, none per message.
  `future_layout` now pins the one-pointer size and tighter future budgets.
- trybuild coverage for `#[protocol]` and `#[actor]`: 64 new compile-fail
  cases (73 in total) and 4 new pass cases, run only on the toolchain pinned in
  `.tool-versions` (`SPAWNED_UI_TESTS=1` forces them elsewhere). The macros now
  reject, with an error on the offending tokens, inputs that previously failed
  inside the expansion or were silently accepted: generic, `unsafe`/`extern`/`const`,
  duplicate and message-name-colliding protocol methods; borrowed and
  `impl Trait` protocol parameters; generic handlers, handlers without a
  `&self`/`&mut self` receiver or with a by-value context; two handler
  attributes, or `#[started]` with `#[stopped]`, on one method; `mut self`
  lifecycle hooks; mixed async and sync methods in one `#[actor]` block;
  `#[actor]` on a trait impl; module paths and missing commas in
  `#[actor(protocol ...)]`. Handler errors (wrong return or context type,
  duplicate handlers, async handlers in a threads actor) now point at the
  handler instead of `#[actor]`. `#[actor(protocol = ...)]` on a generic actor
  no longer fails with "cannot find type `T`". See
  [testing](docs/TESTING.md#macro-diagnostics).
- Optional `failpoints` feature (off by default, compiled out otherwise): 16
  named fault-injection sites in actor startup/stop, supervision, shutdown,
  placement, registration, links and monitors, configurable from tests with
  panic, error, delay, yield, seeded probability and count limits
  (`spawned_concurrency::failpoints`). Ported onto the v0.7.1 lifecycle code;
  the coordinator-panic and never-started-child scenarios that were known gaps
  before v0.7.1 now run as regular tests. See
  [testing](docs/TESTING.md#failpoints).
- Tests for the semantics guarantees that were not yet checked
  (`concurrency/tests/semantics_gaps.rs`, both modes where they apply): a
  trapped `Exit` and a `Down` follow every message the source sent earlier;
  `send_after` and fixed-delay `send_interval` (no catch-up burst after a clock
  jump) under paused Tokio time; a timer never delivers to a supervised
  replacement; a panicking request handler gives its caller exactly
  `ActorStopped` on every backend. A `panic-abort` CI job runs the
  `panic_abort` example under a new `panic-abort` profile: a panicking handler
  aborts the process without `stopped()` or a reply, and supervision of `Kill`
  and `Shutdown` exits still works.
- Apalache 0.62.4 inductive-invariant checks for the supervision models
  (`verification/tla/apalache.sh`, `nix run .#check-apalache`, CI job
  `tla-apalache`). `Init => IndInv` and `IndInv /\ Next => IndInv'` hold for
  every safety invariant of `Supervision` (AtMostOneGeneration,
  PublishedIsLive, BudgetBounded, TemporaryStartsOnce, ReadyMeansAllStarted,
  DoneMeansQuiescent, plus the action properties NoStartAfterClose,
  RestartFollowsPolicy, ReverseShutdown, NeverReopens) with 5 children and
  `max_restarts` 4 (also run at 10/10), of `Cohorts` (both strategies, 5
  children, 4 generations, 4 rounds) and of `NestedSupervision` (6 nodes, 3
  slots, 3 generations), for every restart-mode table. The step check covers
  all states of that size, not only reachable ones, with no depth or
  generation bound. Guard-removal negative controls must break induction.
  Specs gain Apalache type annotations (comments) and constant-bounded range
  rewrites; TLC state counts are unchanged.
- Apalache inductive-invariant checks for the system shutdown coordinator
  model (`Coordinator.tla`, `CoordinatorInd.tla`, CI step `--model
  Coordinator --targets 32`). `Safety` (admission closed after shutdown
  begins, finished within signalled, runtime ownership released only once
  every target finished, coordinator failures recorded) and two new
  invariants, `OrderedShutdown` (a root is signalled only after every
  later-admitted root finished) and `JoinAfterAllSignalled`, plus four
  action properties, are inductive at 8 and 32 shutdown targets with one
  helper lemma. Eight negative controls must break induction and also give
  a counterexample reachable from `Init`, including `RollbackReady`, the
  out-of-order rollback of a ready supervisor startup fixed in v0.7.1,
  which `negative_checks.py` also runs under TLC. Liveness stays TLC-only;
  TLC state counts are unchanged.
- Documentation: `docs/TESTING.md` gains an overview of how spawned is
  verified (design, model-to-code, interleavings, faults, behaviour, code and
  tests, real workloads) and an up-to-date status table for every layer; the
  README summarizes it.
- Roadmap: add coverage-guided scenario fuzzing on the simulator, harsher
  canary faults (cgroup limits, network faults, clock skew), a benchmark
  regression gate, trybuild compile-fail tests for the macros, and later Verus
  or Creusot proofs, turmoil/Jepsen-style tests and continuous nightly runs to
  the testing plan; record Apalache's status and the Veil spike as the next
  step toward invariants for any configuration size.
- Roadmap: per-actor memory limits (rank 18a), one semantics contract
  (rank 5d), an integrated request and lifecycle model for ranks 11, 13, 15,
  15a and 17-19, and a design-review rule: a design doc, TLA+ model, client
  mapping and two independent reviews before any feature code. The principle
  is to match Erlang/OTP as closely as possible; for v0.8: exit signals as in
  OTP (`Killed`, ignored `normal` exits, trappable `shutdown`, `NoProc`; rank
  5e), the restart budget counts recovery rounds (5f), and one exit reason when
  the runtime drops an actor (5g).
- Documentation: `docs/design/semantics.md`, one semantics contract (roadmap
  rank 5d). It states each guarantee (messages, requests, lifecycle and exit
  reasons, links/monitors/trapped exits, supervision, systems, timers, panics)
  with the TLA+ invariant, Kani harness, test or example that checks it, marks
  the ones not yet checked, lists what is not guaranteed and the planned
  changes that will alter it. Linked from the README, guide, testing docs and
  design index.
  The guide now says that a `stopped()` panic makes the exit reason `Panic`,
  that `send` returns `Ok` and drops the message while an actor is stopping,
  and how to stop an actor from outside (`ChildHandle::stop`/`shutdown`/
  `stop_with_reason`); the `ExitReason::Normal` doc comment and the supervision
  design table are corrected.

## 0.7.1 — 2026-11-10

- Contain tracing subscriber panics in caught lifecycle-panic diagnostics and
  supervision restart events. Preserve the original actor failure, run shutdown
  hooks, and allow replacement generations to start. Enable the #217 strict
  subscriber regressions and cover started/stopped-only failures too.
- Remove a macOS full-suite race in the coordinator-fault test: a short shutdown
  report deadline is not an acknowledgement that the fault has been recorded.
- Isolate the linked-delivery panic regression in a subprocess and use zero-sized
  secondary panic payloads. This avoids two intentional defensive leaks in the
  test without disabling LeakSanitizer or suppressing other allocations.
- `spawned-test` is first published with v0.7.1; it merged after the v0.7.0 tag.
- `spawned-test`, a new dev-dependency crate (`testing/`) for testing actors in
  both modes. `Mock` implements `Handler<M>` for every message, so
  `ActorRef<Mock>` satisfies any `#[protocol]` and converts to its `XRef`;
  replies are closures per message type, every message is logged (in flight
  until its handler has returned the reply, so `expect` never runs ahead of
  the handler) and unconfigured messages fail loudly. `Probe<M>` is a recording `Recipient<M>`
  with `expect`/`expect_no_message`. Harness helpers bound waits for exits,
  supervised generations and `ActorSystem`/supervisor shutdown, failing with a
  report instead of hanging, and name the calling test line. Fault injection:
  `faults::exit` with any `ExitReason` (never blocks the caller),
  `restart_child` (one deadline for the whole call), and mock
  delay/discard/crash rules. Public
  APIs only; no controlled scheduler yet. Tokio-free with
  `default-features = false` (threads helpers).
- Contain supervisor and system coordinator panics, retain owned cleanup until
  actual joins, and expose system coordinator failures in shutdown reports.
- Make link-delivered Kill trappable while direct Kill remains untrappable.
  Add `ChildHandle::stop_with_reason` for direct cooperative termination.
- Reserve restart capacity before backoff, refund interrupted reservations,
  and fail an exhausted budget immediately. Charge atomically with generation
  admission; refund superseded/unplaced attempts. Retain and document v0.7's
  per-child cohort accounting. Preserve failed terminal states;
  emit typed give-up diagnostics once for the originating child.
- Preserve ordered system shutdown when admission closes just after readiness.
- Preserve that order when an already-ready supervisor's startup future completes
  or is dropped after admission closes; readiness remains a historical commitment.
- Restore pre-mailbox-guard async handle and owned RPC-future layouts by sharing
  sender metadata outside the handle. Add owned send/request/protocol and nested
  dispatch size regressions, with and without metrics.
- Preserve a handler/start/stop panic when the actor state's destructor also
  panics. Contain link callback panic-payload destructors and tracing subscribers.
- Clarify terminal child states, pending restart reservations and full-jitter
  backoff caps in the migration guide.
- Add `ActorSystem::async_group_on` for managed work on a caller-owned Tokio
  runtime; shutdown leaves ambient work and the borrowed runtime running.
- Add composable process-wide timing requirements for independently initialized
  libraries. Diagnostic turns use a separate slot and borrowed guard; internal
  exit messages report `Exit` rather than private implementation paths.
- Enable coordinator fault regressions, extend the supervisor model with retry
  reservation/refund, and check an abstract emergency-cleanup ownership model.
- Move queued-message panic containment into receiver-owned cleanup in both modes,
  covering normal stop, unwinding, canceled futures and never-polled startup.
  Release queued replies before completion while ActorRefs remain alive. Preserve
  committed Shutdown/Kill/linked-panic reasons; retain secondary failures through
  `ChildHandle::mailbox_cleanup_failure()`. Otherwise-completed Normal exits use
  a descriptive user-destructor Panic reason. Native sends coordinate publication
  with teardown; async sends retain their channel fast path.
- Add an enabled subprocess regression across all five task placements, real
  transient-restart/Shutdown-preservation traces, Loom checks of the native send
  gate, and deterministic tests documenting managed versus
  legacy hook behavior when stopped before first poll.

- Declare Prometheus histogram maxima as separate gauge families, preserving
  series names. Add parser validation and an undeclared-max regression control.
- Add structured supervision restart and give-up events with child name,
  generation and exit reason; restart-budget exhaustion is a warning.
- Add `Context::stop_with_reason` in both modes for cooperative shutdown or
  abnormal exit without panicking. Existing `stop()` still requests Normal.
- Include names and actor types in incomplete shutdown reports. Opt-in
  `diagnostics` adds active message types and turn ages, with per-turn clock
  and lock costs; default dispatch gains no new per-turn instrumentation.
- Display ActorId as a decimal number; Debug retains `ActorId(n)`.
- Clarify ChildSpec's five-second defaults, system deadline behavior, managed
  helper requirements and the limits of dependency shutdown ordering.
- CI hardening (database-grade testing, batch 1): a single-CPU job
  (`taskset -c 0`, one Tokio worker), cargo-deny and cargo-audit
  (`deny.toml`, `.cargo/audit.toml`), a LeakSanitizer job, leak tests for
  actors, threads, tasks, registry entries, supervised generations and handle
  cycles (`concurrency/tests/leaks.rs`), a nightly flaky-test job with
  cargo-nextest, and a PR template with the reproducing-test rule.
- Lockfile: bytes 1.12.1 (RUSTSEC-2026-0007), tokio 1.54.1 (RUSTSEC-2026-0386),
  futures 0.3.34 (drops unmaintained pin-utils) and openssl 0.10.82
  (RUSTSEC-2026-0463). The `updater` examples move to reqwest 0.12
  (h2 RUSTSEC-2026-0258, unmaintained rustls-pemfile). Examples are marked
  `publish = false`.
- Kani bounded model-checking proofs (`concurrency/src/kani_proofs.rs`, weekly
  and on `concurrency/src` PRs) for backoff capping, monotonicity and jitter
  range, per-child jitter seed mixing, the rolling restart budget and recovery
  round ids. Pure helpers were extracted from the supervisor without behavior
  change.
- Add cross-backend metamorphic, cancellation-safety and panic-safety test
  suites (`cross_backend`, `cancellation_safety`, `panic_safety`) plus a
  nightly robustness workflow.
- Documentation: a database-grade testing phase (phase 4) with batches, an
  exit criterion and the reproducing-test rule in `ROADMAP.md`; maintained
  research notes in `docs/research/` on virtual actors (Orleans), systematic
  testing (Coyote, P/PObserve, FoundationDB, TigerBeetle), Ethereum client
  testing, request semantics, concurrency models (reductions versus Tokio's
  coop budget, compute pools, cancel scopes), verification tools (Kani,
  Apalache, TLAPS, Veil), observability, actor testing, the Rust actor
  landscape, supervision models, deadlock and stall detection, test techniques
  and event sourcing (#199-#202, #205, #208, #210).
- Roadmap: add future-size/stack-depth budgets, panics in framework-called code, client compatibility CI, behaviour snapshots, trace coverage against the TLA+ models and differential testing against Erlang/OTP to the testing plan.
- `spawned_test::linearizability`: a Porcupine-style linearizability checker
  (memoised Wing-Gong/Lowe search, per-key partitions, pending requests, an
  `Unknown` budget verdict, failure explanations and an HTML timeline) with a
  `Recorder` for concurrent clients. `request_histories` uses it instead of the
  bounded 16-operation oracle and adds PropEr-style parallel histories of up to
  about 1,000 operations against a real store actor in both modes, with
  injected lost-write, stale-read and reorder negative controls.

## 0.7.0 — 2026-11-09

### Breaking changes for v0.7

- Both libraries now default to the `async` feature. In v0.6, disabling defaults
  did not remove Tokio APIs; in v0.7, `default-features = false` removes `tasks`
  and its async-only exports. Existing consumers using that setting must add
  `features = ["async"]` or restore defaults. This is a pre-1.0 minor breaking
  change and must not ship as a v0.6 patch. Default-feature API checks alone do
  not establish compatibility for these consumers.
- `#[protocol(...)]` now validates its options; arguments silently ignored by
  v0.6 must be removed or replaced with supported `timeout = ...` syntax.
  Conditional `request_timeout` through `cfg_attr` is rejected explicitly.

### Added

- Generated supervision-policy scripts with an independent generation/retirement
  oracle, each-feature library builds without dev-dependency unification, and
  default public API compatibility checks against v0.6.0. Managed registration
  errors use a separate type, preserving exhaustive global-registry matches.
- Protocol regressions for Debug/custom supertraits and downstream concrete and
  generic fakes, plus explicit diagnostics for conditional timeout attributes.
  Business-facing protocol methods share one generated transport abstraction.
  Supervisor policy checks inspect every child and use generation notifications;
  stream examples use handshakes instead of scheduling deadlines. Compatibility
  CI covers metrics and the explicit-async migration, audits the declared
  no-default removals, and runs feature tooling with Python 3.9 and a stable cache.
- Optional ActorSystem with named parked async execution groups and explicit
  worker budgets. `start_on(&group)` selects placement without changing message
  handlers or ambient `.start()`. Scoped async/blocking helpers, framework
  timers/listeners/monitors and owned registrations join managed cleanup.
- OneForOne supervision with fallible fresh-instance factories, readiness,
  Permanent/Transient/Temporary modes, supervisor-wide rolling retry limits,
  per-child capped backoff/jitter and ordered startup/reverse shutdown.
  References remain instance-bound; replacement lookup is explicit. No replay
  or automatic state recovery. Expired cleanup reports retain closed runtimes
  and suppress overlapping replacements; blocking code is not preempted.
- Asserted execution-group and recovery examples plus lifecycle failure tests.
- Optional `lifecycle-trace` feature (off by default, compiled out otherwise):
  the supervisor records one event per action of its TLA+ model so recorded runs
  can be checked against the model. No behavior or cost change when disabled.
- Nested supervisor child specs and OneForAll/RestForOne strategies. Affected
  scopes stop in reverse order, join actual cleanup, then reconstruct in order.
  Collateral factory attempts count against the shared restart budget.
- Parked Linux pinned runtime shards and sized persistent native actor-thread
  pools sharing one helper runtime. Native slots reject excess admission and
  remain reserved through helper cleanup. Group shutdown joins native owners
  before completion, including runtime/thread-local teardown.
- Tokio-free synchronous builds with `async` defaults disabled, std lifecycle
  synchronization, feature-aware protocol generation and an isolated graph check.
  ActorSystem/supervision remain async-only; default async behavior is retained.
- Asserted execution-profile, nested recovery and thread-only examples, feature
  matrix CI and lifecycle/resource-failure tests.

- Protocol/method request timeout configuration and per-call configured handles,
  preserving the five-second default and handwritten protocol implementations.
  Configured handles support type-erased protocol refs in both runtime modes.
- Asserted timeout, listener/recipient, native lifecycle, registration/fake and
  coordinated shutdown examples, plus an Ethrex/Ethlambda migration guide.

### Review fixes

- Context-derived native placement selects a free slot; explicit pinned placement
  remains inherited. Helper-worker budgets are configurable and validated through
  `thread_group_with_helpers`.
- Retirement records its terminal trace event; OneForOne backoff avoids an unused
  cohort watcher. Failure/readiness decisions use the production admission lock,
  and interrupted cohort backoff does not inflate the failure streak.
- Native integration suites run without the async feature; CI checks their test
  inventory. Rollback and internal envelope/lifecycle documentation are corrected.
- Cohort/nested TLA+ projections, real trace scenarios and negative controls extend
  pre-release verification beyond OneForOne.

### Documentation

- Documentation restructured into a small live set: `README.md`,
  `CHANGELOG.md`, `ROADMAP.md` (phases plus the single ranked backlog and the
  examples still to add), `docs/GUIDE.md` (the former API guide with backends,
  execution groups and metrics folded in), `docs/MIGRATION.md`,
  `docs/TESTING.md`, the execution-groups design contract and an
  `examples/README.md` feature index. Research notes, reviews, release drafts,
  superseded designs, benchmark reports and raw experiment data moved to
  `legacy/` (kept for reference, not maintained).
- One ranked backlog replaces the separate research rankings. Other research
  documents are reference material: framework ideas, verification strategy,
  Commonware and Silver/flux notes.
- Ethrex and Ethlambda client evidence recorded in the backlog: hand-built
  deferred replies and pipe-to-self, a per-peer actor registry with a generation
  guard, a hardwired request timeout that exited a node, an unbounded-mailbox
  memory leak, hand-wired shutdown and hand-rolled timer policies. The roadmap
  records client adoption of v0.7, followed by deferred replies and further
  workload-driven capabilities.
- API guide, `CLAUDE.md` and README updated for monitors, links, owned helpers,
  name registration, execution groups and supervision.
- Durable findings promoted out of `legacy/`: backend sizing warnings and why
  `Backend::Thread` avoids a current-thread runtime (guide), benchmark
  conclusions (benchmarks README), non-goals (roadmap), the next lifecycle
  model's properties (TLA+ README) and the PR #166/#168 regressions (testing).
  About 360 MB of raw benchmark data, duplicate HTML reports and superseded
  notes were removed from `legacy/`; git history keeps them.

### Verification

- TLA+ model of the OneForOne supervisor (`verification/tla`): restart modes,
  rolling budget, ordered startup, reverse shutdown, deadlines and stuck cleanup,
  with safety, action and liveness properties. Five bugs injected into the model
  are each caught. TLC runs through a new Nix flake (`nix run .#check-tla`).
- Trace validation: real supervision scenarios and seeded random runs are
  recorded and accepted by TLC as behaviors of the model; hand-edited traces are
  rejected. It found one model gap and no implementation deviation.
- Mutation testing (`cargo-mutants`) of supervision and system code, with new
  tests for backoff reset, builder and aborted-startup cleanup, supervisor
  failure reporting, deadline signalling, child-spec validation, registration
  release, helper abort and group validation.
- All crates are `#![forbid(unsafe_code)]`. A new verification workflow runs
  TLC with trace validation and ThreadSanitizer on every PR, and the deep model
  check and mutation testing weekly.

### Fixed

- Preserve user-written `Box<T>`/`&T` protocol and converter forwarding by keeping
  generated public impls concrete. Shared transport helpers are associated
  functions, so protocol methods can use their names without ambiguity.
- Keep supervisor test watchdogs cooperative on immediate notifications; diagnose
  premature retirement. Stream examples have bounded, descriptive waits. Feature
  checks reuse an isolated locked source/build subtree, and API checks reject an
  unsupported baseline before building.

- A supervisor that was stopped normally could record a failure: the admission
  check and the failure were separate steps for a child with `max_restarts == 0`
  and for a non-restartable startup failure. The check, decision and failure are
  now one step, and a child finishing exactly at its cleanup deadline is no
  longer reported as timed out.
- Bound each dedicated async actor runtime (`tasks::Backend::Thread`) to one
  async helper worker instead of creating a machine-sized Tokio worker pool per
  actor; `spawned_rt::threads::block_on` uses the same bound. The actor remains
  on its dedicated thread; blocking-helper progress, `block_in_place`, timers
  and I/O remain supported. Blocking operations, DNS and filesystem work can
  still create extra threads. `TOKIO_WORKER_THREADS` no longer changes this
  runtime's helper count: applications that relied on a larger private helper
  pool, or run CPU-heavy async helpers there, should validate their workload.
  Actor/protocol signatures and the shared async backend are unchanged.

## 0.6.0 — 2026-11-06

### Added

- Optional `metrics` feature: fixed aggregate counters, per-actor mailbox depth,
  sampled queue/handler/request timings, caller outcomes, lifecycle accounting
  and pull-based Prometheus text export. Disabled by default; no handler edits,
  dynamic metric labels or exporter work on the collection path.
- Actor identities, type-erased `ChildHandle`, exit reasons and cancellable exit
  waits across task and synchronous-thread actors.
- Unidirectional monitors and bidirectional links, trapped exits through
  `#[exit_received]`, and linked startup registered before child dispatch.
- Generated actor scenarios, concurrent producer and cleanup regressions, macro
  diagnostics, Loom cancellation/scheduler models and sustained lifecycle tests.

### Fixed

- Close links atomically with registration, avoid strong reverse-link cycles,
  and propagate runtime shutdown even for actors never polled.
- Preserve actor/payload destruction and terminal publication ordering, including
  late peer failures during normal shutdown and monitor waiter reclamation.
- Release monitor waiters when cancelled or their owner exits. Late monitor
  registration is inert, including in `stopped()`.
- Honor observed stop before dispatching work received by an idle actor. Close
  and drain task mailboxes through reserved publications before completion.
- Wake synchronous thread mailboxes directly on every cancellation route instead
  of polling every 100 ms; repeated stops send at most one private wakeup.
- Preserve cleanup panics when shutdown starts during `started()`. Stop thread
  listeners before another iterator `next()` after observed cancellation.
- Async exit waits for thread actors no longer retain a blocking-pool worker;
  blocking waits remain available without an active Tokio executor.

### Research and compatibility

- Added reproducible backend, actor-graph, overload/fairness, pinned-core and
  communicating-runtime experiments, profiles, raw data and graphs. Alternative
  schedulers remain behind private experiment features; Tokio stays the default.
- Added backend selection/isolation usage, client applicability, Silver/flux
  research, metrics accounting/overhead and a focused roadmap.
- Ordinary typed Rust messages/replies, dynamic actors, addressed delivery,
  unbounded-mailbox availability and cooperative handlers remain the model.
  Caller timeout/drop does not cancel accepted actor work. Links do not provide
  automatic restart, persistent delivery or transparent reference replacement.
- `ExitReason::Shutdown` remains reserved; linked `Kill` is untrappable but
  cooperative. Abrupt future destruction cannot guarantee async `stopped()`.

### Upgrade notes

- All three library crates use version 0.6.0. Existing handler/protocol
  signatures are unchanged; metrics require explicit feature selection.
- Synchronous thread actors do not require an active async executor but still
  depend on Tokio synchronization through `spawned-rt`.
- Stop and linked Kill remain cooperative; running blocking code is not
  preempted. Enqueue success is not a handling or durability guarantee.
