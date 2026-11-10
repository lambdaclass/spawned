# Changelog

## Unreleased

- **Breaking (0.8), in progress:** deferred replies and pipe-to-self
  (ROADMAP rank 11), implementing
  [the design](docs/design/deferred-replies.md). Every request now has a reply
  slot: one packed atomic word (state, token, registry and running-job count)
  whose transitions are checked with Loom (`reply_slot_loom`). See
  [migration](docs/MIGRATION.md#unreleased).

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
- Design (not implementation) for rank 11, deferred typed replies and
  pipe-to-self, revised over four passes after independent reviews:
  `docs/design/deferred-replies.md`, client mapping for ethrex and ethlambda,
  and the `DeferredReplies` (reply slots) and `DeferredJobs` (pipe jobs) TLA+
  models with 22 negative controls, each checked against its named
  invariant.

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
