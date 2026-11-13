# Changelog

## Unreleased

- Generated await-free async handlers can dispatch without a boxed turn future
  when their message has no destructor. Shared/explicit-lifetime receivers,
  opaque macros/attributes, suspending handlers and payloads with destructors
  retain the future adapter. Public direct calls remain lazy; panic fencing,
  drop order and causal actor tracing are preserved.
- Request and turn bookkeeping avoids redundant atomic exchanges and dynamic
  default-budget storage. Deferred-token ownership is tracked through the end
  of destruction; production-core Loom races cover last-token retirement.
- Performance: request envelopes reuse message presence as their dispatch
  marker and inline construction, removing duplicate state without moving
  dispatch or discharge. The instruction gate also covers manifest, lockfile
  and compiler changes, including release version bumps.
- Release-review fixes: job completion atomically closes emit admission,
  including the gap after a held terminal is released; blocked emitters wake
  on completion. Join waiters are deduplicated and woken outside their lock.
  Production-core Loom regressions and a TLA+ fault cover retained JobCtx races.
- Timer regression: paused Tokio time is never stored as a service-clock
  deadline. Monotonic clock conversion is inlined without caching time.

- Release validation: client compatibility uses the prepared `spawned-v0.8`
  ports for the breaking API, rather than the incompatible 0.7 client branches.

- **Behaviour (0.8)**: request timeouts are the caller's, as OTP's
  `gen_server:call`: a reply reads no clock, and only the caller's expiry
  (its timer, the owner's deadline heap, a deadline check) times a request
  out. A reply that loses that race is dropped (`Delivery::Expired`, the
  caller sees `RequestTimeout`); one that lands after the deadline but
  before the expiry runs is delivered. The `WRITING` slot state is gone and
  no reply path reads the clock. `DeferredReplies.tla` models it (a reply
  delivers while the slot is open; `LateSuccess` now overrides an expired
  slot).
- Performance (0.8 request path, no API change): the reply slot keeps one
  lock for value and waker with lazily created deferral state (no condvar:
  a blocking `recv()` parks its thread); the tasks envelope lends its
  request slot to the turn instead of cloning it; a turn that finishes in
  its first poll skips the runtime-drop guard; `request` captures one precise
  runtime timer before enqueue, keeping its relative budget separate from
  the service-clock slot deadline so paused clocks stay correct (no coarse clock); a request without a
  deadline holds no reference to the owner's registry (the owner's close
  records the final reason in the slot); a caller that owns an answered
  slot alone takes the value without the lock; and the slot no longer
  holds its owner at all (two flags became word bits), which keeps it in
  glibc's fast bins for typical reply sizes. Reply ownership and terminal
  transitions retain their linearization points. A receiver timer
  refinement (`ReplyTimer.tla`), two negative controls, production-slot Loom
  races, and paused-clock/overflow tests cover the timer bookkeeping.
- `InsertError::kind()` (a `#[non_exhaustive]` `InsertErrorKind`) and
  `InsertError::into_parts()`: a refused insert's reason and token come
  together, so a caller answers the token for any reason, including one
  added later, instead of dropping it in a wildcard arm.
- Docs: an async job's `work` is a factory run at once in the current turn
  (its panic is a handler panic; a job refused by the owner's exit has run
  it); only its future is the job (`pipe` rustdoc, GUIDE).
- **Behaviour (0.8)**: `demonitor` flushes, as OTP's `demonitor(Ref, [flush])`:
  once it returns, in both modes, no `Down` of that monitor is enqueued or
  handled, including one already in the mailbox. Framework `Down`s travel in
  a private envelope that checks the monitor table at dispatch, so ordinary
  messages pay nothing and a `Down` user code sends is unaffected. New
  failpoint site `monitor.down_before_dispatch` (#247).
- Fix: `PendingReplies::drain` yields live tokens in insertion order. It
  walked a `HashMap` with a per-process `RandomState`, so deferred replies
  were answered in a different order in each process, and a same-seed
  simulator run did not reproduce its schedule (#261). `take` is now a
  linear scan of the capped table.
- **Breaking (0.8), API pass before publishing** (each change is a row of the
  [0.7 to 0.8 call-site table](docs/MIGRATION.md#07-to-08-api-call-site-table)):
  - Jobs: one call per shape, options first (`pipe`, `pipe_blocking`,
    `pipe_reply`, `pipe_reply_blocking`, `reserve_job(opts)`,
    `reserve_blocking_job(opts)`); every `work` receives its `JobCtx` by
    value. `pipe_with`, `pipe_with_ctx`, `pipe_keyed`, `pipe_blocking_with`,
    `pipe_reply_with` and `pipe_reply_blocking_with` are gone.
    `JobOptions` is `#[non_exhaustive]` with a builder (`new`, `key`,
    `deadline(impl Into<Deadline>)`, `emit_credits(NonZeroU16)`,
    `cancel_on_caller_gone`, which `debug_assert!`s outside `pipe_reply*`);
    `deadline_after` is folded into `deadline`. `JobOutcome<T>` is
    `Result<T, JobError>`. `PipeError` is the `#[non_exhaustive]` enum (no
    `kind` wrapper); `PipeReplyError` is `#[non_exhaustive]` and its
    `Failed` variant carries `error`. `Deadline` implements
    `From<Duration>` and `From<Instant>`. `DEFAULT_EMIT_CREDITS` and
    `default_max_native_jobs` are no longer public.
  - Requests and deadlines: `request_raw` and `request_raw_until` become
    `send_request(msg, impl Into<Deadline>)` (OTP's
    `gen_server:send_request/2` with an explicit deadline; on `Receiver<M>`, a `Deadline`). 0.7's
    `request_raw` had no deadline: `Deadline::Never` keeps that.
    `PendingReply::deadline`, `ReplyTo::deadline` and `ReplyReceiver::deadline`
    return `Deadline`, so a deadline forwards as is;
    `ReplyTo::with_deadline` is `shorten_deadline(impl Into<Deadline>)`.
    Removed: `PendingReply::has_caller` (`defer()` returns `NoCaller`),
    `PendingReplies::{purge, retained, live}` (now `len`/`is_empty`; purging
    stays automatic), the public `DEFAULT_MAX_OUTSTANDING` (the default,
    1024, is documented at `Actor::MAX_OUTSTANDING`).
    `PendingReply::detached` is hidden (`spawned_test::detached_reply()`).
    `InsertError<R>` holds `ReplyTo<R>` and is `#[non_exhaustive]`.
    `DEFAULT_REQUEST_TIMEOUT` is re-exported at the crate root. The
    `Duration` parameters of `ActorSystem::shutdown`, `Supervisor::shutdown`,
    `ShutdownReport::wait` and the test kit's waits are named `grace` or
    `limit` (a deadline is an instant). `PendingReplies` and
    `Context::deferred_outstanding` are documented as experimental.
  - `#[non_exhaustive]` on the enums that will grow (`SupervisorError`,
    `SystemError`, `HelperError`, `RegistryError`, `SupervisorStrategy`,
    `ExecutionProfile`, `ChildState`, `Backend`, `spawned_test::Outcome`)
    and on the read-only structs (`Down`, `Exit`, `ChildSnapshot`,
    `SupervisorReport`, `ActorExit`, `metrics::Snapshot`,
    `HistogramSnapshot`). `Down` gains `actor`, the monitored `ActorId`
    (OTP's `Object` in `{'DOWN', Ref, process, Object, Info}`).
  - `SupervisorConfig`, `Backoff` and `metrics::Config` are
    `#[non_exhaustive]` with builders (`new()` plus one setter per field;
    `Backoff::seed(u64)`).
  - Hidden and sealed surface: `Receiver<M>` is sealed (it gained a
    required method in 0.8; sealing lets the next one be added freely). The
    `reply` module is `#[doc(hidden)]` (its types are at the crate root and
    under `tasks`/`threads`; macros emit root paths). Test hooks
    (`ActorRef::__in_turn`, `PendingReply::__discharge_stopped`,
    `reply_pair`, `PendingReply::detached`) move into
    `#[doc(hidden)] __private`, which spawned-test uses and which carries no
    semver promise. `lifecycle_trace` is public only with the
    `lifecycle-trace` feature; `spawned_rt::time`'s tick functions are
    `#[doc(hidden)]`.
  - Macros: `#[handler]` is removed (use `#[request_handler]` or
    `#[send_handler]`), and a `#[send_handler]` method returning a value is
    a compile error (it used to answer a `request` with it).
  - Duplicates removed: `Context::from_ref` (use `ActorRef::context`) and
    `ChildHandle::shutdown` (use `stop_with_reason(ExitReason::Shutdown)`).
  - `Response<T>` in tasks mode: `unwrap`, `expect`, `map` and the new
    `into_result` on a response that was not awaited wait for the reply
    when called off an async runtime and panic inside one (they always
    panicked); `is_ok`/`is_err` panic on it instead of answering `false`.
  - Docs: an "Experimental in 0.8" list in the GUIDE (job API,
    `PendingReplies`, `exit_pending`, registry, helpers, placement
    experiments, `spawned-rt` re-exports), the stop/exit/kill table on
    `ChildHandle`, and `start_linked`'s unmanaged placement. The
    `ActorError` classifiers (`target_gone`, `is_timeout`, `is_transient`,
    `exit_reason`) are kept, defined per variant and tested for every one.
- Deferred replies and jobs, fixes from the 0.8 pre-release review: a job
  deadline still fires while the result waits for scheduled emits, and the
  job keeps its `MAX_OUTSTANDING` credit until its terminal message; a job
  admitted while its owner exits is refused in one step with the exit
  (`pipe_reply*` returns `OwnerStopping` with the token); in `tasks` mode a
  blocking job's thread (outside a runtime) is created at reservation
  (`SpawnFailed` instead of a silently lost job) and a managed actor's
  blocking job keeps its scope until its body ends (a supervisor does not
  replace the child, and shutdown is not complete, while it runs);
  shortening a token's deadline no longer cancels a `cancel_on_caller_gone`
  blocking job and wakes the waiting caller to re-arm its timer; an
  `emit_at` discarded by a concurrent cancel returns `JobGone`; a refused
  emit returns its credit once; a panicking payload destructor no longer
  stops the process-wide deadline service; a tasks handler that panics
  while building its future is a handler panic (`stopped()` runs).
- **Breaking (0.8):** `DeferError` and `Delivery` are `#[non_exhaustive]`
  (like `JobError` and `PipeError`): match them with a wildcard arm.
- **Breaking (0.8):** the restart budget counts restart actions, as OTP's
  intensity does (ROADMAP rank 5f): a OneForAll or RestForOne recovery round
  takes one `max_restarts` grant when the coordinator selects it, before the
  cohort is stopped, however many children it rebuilds (v0.7 charged every
  rebuilt child, so a six-child OneForAll round needed six grants). A failed
  replacement inside a round requests a new round, charged again (OTP's
  `try_again_restart`); a superseded round keeps its charge; OneForOne and
  retries before the tree is ready still take one grant per child retry. An
  exhausted budget fails the supervisor when the round would be selected,
  naming the round's origin child. `max_restarts` documentation, semantics
  V5, the guide and the migration notes follow.
- The death point is one boundary, however the actor dies (run loop or
  runtime drop): job admission closes, outgoing replies and `ctx.exit`
  signals are fenced, the reason is sealed, and every deferred token and
  queued request is answered `ActorStopped(final reason)` before any queued
  payload or state destructor runs and before death is published. A
  `ReplyTo::reply` once `NoProc` is observable returns
  `Delivery::OwnerExited` (the caller gets the final reason); a signal a
  cloned context sends before its actor's death reaches the target before
  the death notice. Previously a blocking payload destructor, or a runtime
  dropping the actor, could leave callers unanswered while it ran.
- **Breaking (0.8), in progress:** exit signals, startup and runtime-drop
  semantics as in OTP (ROADMAP ranks 5e, 5f, 5g), implementing
  [the design](docs/design/otp-exit-signals.md) (#240, stacked on rank 11).
  Done so far: `ExitReason` is `#[non_exhaustive]` with `Killed` and `NoProc`;
  `ChildHandle::kill()` (untrappable, ends `Killed`, supersedes any reason
  until the death point), `ChildHandle::exit(reason)` and
  `Context::exit(&target, reason)` with `erlang:exit_signal/2` rules
  (`Normal` ignored by a non-trapping actor, other reasons end it without
  `stopped()`, trapping actors get an `Exit`), `ActorId::EXTERNAL`;
  administrative `stop`/`stop_with_reason`/`shutdown` unchanged; one death
  point after `stopped()` where the reason becomes final and the link table
  closes; a late untrapped signal replaces the requested reason unless a kill
  was received (decision 8); a panic in `stopped()` replaces an earlier
  abnormal reason (except after a kill); `stopped()` is skipped on a kill, an
  untrapped signal or link notice and a runtime drop; runtime-dropped actors
  end `Killed` in both modes; `link`/`monitor` to an actor past its death
  point give `NoProc` (a non-trapping caller of `link` exits `NoProc`;
  `Context::try_link` returns `Err(NoProc)`); `link` from an actor whose exit
  began is inert; `unlink`/`demonitor` are fences; a `start_linked` parent's
  exit ends a trapping child through `stopped()` (never `exit_received`);
  signals arriving during `started()` are resolved when it returns;
  supervisor and system shutdown send the parent's `Shutdown` exit signal
  (a non-trapping child ends `Shutdown` without `stopped()`, a trapping one
  finishes its backlog and runs `stopped()`), with the kill at
  `shutdown_timeout` (supervisor, then a second interval before cleanup is
  stuck) or at grace (system); destructor panics after
  the death point are cleanup failures (`ChildHandle::cleanup_failure()`,
  `mailbox_cleanup_failure()` deprecated) that restart a Transient child
  without changing the reason; `spawned-test` `faults::exit` sends an exact
  exit signal; metrics `actors_exited_killed_total`,
  `actors_exited_noproc_total`. Supervisor startup (S1/S2): any failed first
  start of a child (factory error or panic, panic or stop in `started()`,
  `startup_timeout`, placement or group closure) fails `start()` whatever the
  restart type, with no retry, as `SupervisorError::FailedToStartChild
  { child, cause, rollback_failures }` (a stuck cleanup during the initial
  start is `StartupCleanupStuck`); a ready child's exit during the initial
  start is deferred and processed under the strategy once the tree commits
  ready (OneForOne no longer restarts it at once, OneForAll/RestForOne no
  longer fail the start); `start()` returns its error only after the
  rollback (every child terminal, or `Failed` once its cleanup is stuck).
  `Supervision.tla` (no restart before the tree is ready, deferral actions
  `DeferExit`/`Undefer`, traced) and `Cohorts.tla` (round 0 once per child,
  `FirstStartOnce`) follow, with the v0.7 behaviours as negative controls.
  Caller errors (rank 11 x 5e, decision 4): `ActorError::ActorStopped`
  carries the actor's final `ExitReason`, the same one `wait_exit`, `Down`
  and links see (`Killed` after a kill, the signal's reason after an
  untrapped exit signal, `Panic(..)` after a crash); new
  `ActorError::NoProc` for a `send` or `request` after the death point (the
  registry is sealed with the final reason inside the death point's
  exit-table critical section); a send or request made while the actor exits
  is admitted and answered at the death point. Exit errors wait for the
  death point: a request whose handler panicked, a token dropped while the
  owner exits and an undispatched queued request stay open ("exit pending")
  and are answered with the final reason, or by the caller's deadline first.
  `DeferredReplies.tla` models it (`pendA`, invariant `StoppedAtDeath`,
  control `EarlyStopped`; one more reply trace control); metrics
  `requests_noproc_total`.
  Trace validation against `Lifecycle.tla`: with `lifecycle-trace`, the
  runtime records exit signals, stops, links, monitors, sends, dispatch,
  panics, `stopped()` run or skip, the death point, runtime drop,
  publication and notice delivery (schema `lifecycle-v1`,
  `TraceLifecycle.tla`); `tests/lifecycle_trace.rs` records 14 scenarios
  (both modes), all accepted, and eleven edited traces (stopped() after a
  kill, `Kill` for `Killed`, publication before D, real reason instead of
  `NoProc`, notices after unlink/demonitor, ...) are rejected.
  Client feedback (ethrex, ethlambda): `ActorError` classification helpers
  `target_gone()`, `is_timeout()`, `is_transient()` and `exit_reason()`, so
  clients need no exhaustive match on the `#[non_exhaustive]` enum; a
  skipped `stopped()` logs `event = "stopped_skipped"` at `debug` (actor,
  cause `kill`/`exit_signal`/`runtime_drop`, reason);
  `Context::exit_pending()` (both modes) tells a trapping actor that its exit
  began or a parent exit is queued behind the current message, so it can
  refuse late work (advisory, read-only, no per-message cost).
  Concurrency checks: the link table (`src/link/table.rs`) and the monitor
  fence (`src/monitor/fence.rs`) are shared with a Loom model
  (`tests/link_fence_loom.rs`, 9 tests explored exhaustively: one notice per
  edge, none after `unlink`/`demonitor` or the receiver's death, a link or
  monitor racing the death point gets the real reason or `NoProc`, never both
  nor none); `tests/exit_signals_sim.rs` explores six orderings on the
  simulator with PCT and bounded DFS (kill vs `stopped()`, late signal vs the
  death point, link/monitor vs the death point, unlink/demonitor fences,
  supervisor shutdown vs a trapping child's queue and `exit_pending`, a ready
  child's exit vs the tree commit). GUIDE "Links, monitors and exit signals",
  the `exit_signals` capability example, and semantics L3-L6, X1-X6, V1, V3,
  V9 and the OTP differences table.
  Remaining: the heavy TLC configurations and Apalache after the canaries.

- **Breaking (0.8), in progress:** deferred replies and pipe-to-self
  (ROADMAP rank 11), implementing
  [the design](docs/design/deferred-replies.md). Every request now has a reply
  slot: one packed atomic word (state, token, registry and running-job count)
  whose transitions are checked with Loom (`reply_slot_loom`, 16 models), as
  are the owner's turn-phase drop commit (`registry_turn_loom`) and the job
  core (`job_core_loom`: admission rollback, completed-key replacement, emit
  versus terminal notification and cancel, native completion versus cleanup
  join, final destruction after a `Weak` upgrade), each with planted-bug
  controls. The job model found two destructions under the job set's locks
  (key-map clear at owner exit, weak-list compaction), now fixed. With
  `lifecycle-trace`, the job core records `DeferredJobs.tla` actions, and
  `tests/job_trace.rs` traces (seven scenarios) are checked by TLC
  (`TraceJobs.tla`, schema `jobs-v1`), with six edited traces as negative
  controls. Reply slots record `DeferredReplies.tla` actions (send,
  dispatch, the terminal transition inside the slot CAS, exit, death);
  `tests/reply_trace.rs` traces (nine scenarios) are checked against
  `TraceReplies.tla`, with four negative controls. With `metrics`, the
  `spawned_deferred_*` family (tokens, pending, replied/expired/dropped/
  stopped/abandoned, overloaded, sampled defer-to-reply latency) and the
  `spawned_jobs_*` family (started, running, completed/panicked/deadline
  exceeded, cancelled while running, fenced, rejected at a bound, emits,
  unfinished at stop); `tests/metrics_deferred.rs` checks each is counted
  once. Simulator races (`tests/deferred_sim.rs`, PCT and bounded DFS):
  reply vs expiry vs owner death vs caller drop, keyed replacement, emit vs
  terminal and cancel. `Handler::handle` takes a `PendingReply<'_, R>`
  and returns `()`; `#[actor]` generates it, and `#[deferred_handler]` methods
  receive the obligation and may `defer()` it into a storable `ReplyTo<R>`
  (bounded by `Actor::MAX_OUTSTANDING`, `Overloaded` at the bound). Dropped
  obligations answer `ReplyDropped` (in a panicking turn or while exiting
  they are answered `ActorStopped` at death); at death every open token
  answers `ActorStopped`. Request
  deadlines are fixed at send (`Deadline`, `send_request(msg, deadline)`,
  formerly `request_raw`); a per-actor deadline heap and service expire stored
  tokens without a polling caller. `PendingReplies<K, R>` is a bounded token
  table. Pipe-to-self jobs: `ctx.pipe`, `pipe_blocking`, `pipe_reply`,
  `pipe_reply_blocking` (threads mode: the blocking forms on owned native
  threads), with `JobOutcome`/`JobError::{Panicked, DeadlineExceeded}`, keyed
  replacement and cancellation fenced at dispatch, `JobCtx::emit`/
  `emit_blocking` with emit credits and the terminal message last,
  `MAX_NATIVE_JOBS`, owner-exit cancellation and a bounded join after death
  (`unfinished_at_stop`). `ActorError` is `#[non_exhaustive]` with
  `ReplyDropped` and `Overloaded`; `metrics::Snapshot` gains
  `requests_reply_dropped_total` and `requests_overloaded_total`. Example:
  `deferred_replies`. See [migration](docs/MIGRATION.md#unreleased).
- Rank 11 API refinements from porting ethlambda and ethrex to 0.8:
  `#[actor(max_outstanding = N, max_native_jobs = N)]` sets the bounds, and
  `ChildSpec::max_outstanding`/`max_native_jobs` override them per
  supervised child (the design's per-spec override). `tasks`-mode blocking
  jobs (`pipe_blocking*`) run on Tokio's blocking pool instead of a fresh OS
  thread each, still under `MAX_NATIVE_JOBS`, with panics as
  `JobError::Panicked` and cancellation and fencing unchanged (threads mode
  keeps owned threads; a reservation pre-spawns the job's thread).
  Reserve-then-spawn: `ctx.reserve_job(opts)` / `ctx.reserve_blocking_job(opts)`
  return a `JobPermit` / `BlockingJobPermit` whose `pipe*` cannot fail, so a
  refused caller keeps what it would have moved into `work` and `done`.
  `JobCtx::emit_at` / `emit_at_blocking` deliver a fenced, credit-accounted
  message at an instant; the job's terminal message waits for its scheduled
  emits (new Loom model `scheduled_emit_vs_finish_and_cancel`).
  `spawned_concurrency::Instant`
  (also `tasks::`/`threads::Instant`) re-exports the deadline clock.
  `#[send_handler]` on a manual `impl Handler<M>` (for `Result = ()`) drops
  the reply parameter and answers `()` when `handle` returns.
  `spawned_test::tasks::in_actor` / `threads::in_actor` run a closure in a
  live actor's turn. GUIDE: hard job deadlines and the soft-deadline
  pattern, replies carrying a reason through `R = Result<..>`, the
  `send_request` rows.
- Verification: retain bounded weekly Lifecycle checks; run full Lifecycle
  safety/liveness models manually before release, alongside the soak. The
  full configurations and properties are unchanged; incomplete runs block
  release rather than counting as successful CI verification.

- CI: a hot-path performance gate (`perf-gate`). `benchmarks/hotpath.py gate`
  counts instructions per operation with callgrind for async burst, sequential
  and concurrent `request()` and `send` (on a current-thread runtime, so the
  counts are deterministic) and threads-mode `request()`, and fails
  a PR that grows any of them more than 3% over its merge base (it measures
  only when `concurrency/src`, `rt/src` or `macros/src` changed).
  `hotpath.py abba` is the wall-clock A/B on fixed cores, now a release
  checklist step (docs/TESTING.md, "Performance gate").
- Design (0.9, approved for implementation after round 3): a unified start API
  (`docs/design/unified-start.md`, rank 5h). OTP's `start`, `start_link` and
  `start_monitor` wait for `started()`, and `spawn` does not. All four take one
  typed `StartOptions`. `started()` may fail, ignore or continue (`Init`), and a
  failed init never runs `stopped()`, as OTP. Every instance has an immutable
  startup commitment, and readiness is committed by the child, independently
  of the waiter. Errors are answered after publication with the final exit
  reason. One global lock order covers the commit, readiness and scope
  admission. Linked children in the parent's system are owned by its scope,
  with a polite Shutdown at the parent's publication and absolute,
  composing deadlines. It replaces `start_on`, `start_with_backend`,
  `ExecutionGroup::start` and `start_linked`. A new TLA+ model, `Start.tla`,
  has eight configurations and 40 negative controls, and a client mapping
  covers 51 sites.
- TESTING.md: how databases test (FoundationDB, TigerBeetle, SQLite, AWS,
  Jepsen and others), how each technique maps to spawned, and the gaps.
- ROADMAP: smaller releases (0.9 introspection and the attachable console,
  0.10 unified start and the dynamic supervisor, 0.11 the admission package),
  how each feature is implemented against its approved design, and the code
  coverage report widened to the whole crate over the simulated suites.
- Tests: planted-bug follow-ups. New scenario-fuzzer oracles for restart
  decisions, RestForOne cohorts, Transient restarts, required restarts versus
  budget exhaustion, backoff delays in virtual time, linked exit reasons,
  `unlink` and cancelled timers; bounded waits so tests that used to hang under
  a bug fail with a named assertion (`within!`); a Kani layer in the
  planted-bug runner; the planted-bug matrix in `docs/TESTING.md` re-run with
  them. `concurrent_failures_coalesce_into_one_round` no longer flakes on one
  CPU (it read `current()` after the first crash's round had begun).
- ROADMAP: a ranked determinism backlog (D1-D10) under priority tier 3, from
  an audit of what escapes the simulator in Spawned, Ethrex and Ethlambda
  ([docs/research/determinism-audit.md](docs/research/determinism-audit.md)).
  Same-seed reruns found no schedule nondeterminism; traces differ only in
  process-global ids.
- Removed `legacy/` (archived research, benchmark data and reviews, about 58 MB);
  it remains in history at [`efba2434`](https://github.com/lambdaclass/spawned/tree/efba24340a01df82f872ec422b9dbaf48f897b04/legacy),
  and live docs link there.
- ROADMAP: a Path to 1.0 section (0.8, 0.9, 0.10 contents and what 1.0
  requires: stable API rules, a complete semantics contract, production-safe
  defaults, introspection, determinism, release gates, production proof and
  documentation).
- Toolchain pinned to CI's 1.93.0 in `rust-toolchain.toml`, so local, fede
  and agent builds match CI; the fuzz job selects its nightly explicitly.
  `CHANGELOG.md` merges with git's union driver. ROADMAP: the design-review
  rule now requires a test plan, trace validation against real code,
  planted bugs, an introspection hook and a no-cost benchmark for every new
  feature.
- ROADMAP: Erlang/OTP, Elixir and Gleam gaps table (G1-G23, including a remote
  console) with tiers and target versions, and the OTP features Spawned will
- ROADMAP: Erlang/OTP, Elixir and Gleam gaps table (G1-G23, including an attachable
  console in every binary, targeted at 0.9) with tiers and target versions, and the OTP features Spawned will
  not add as non-goals; the comparison is in
  `docs/research/otp-elixir-gleam.md`.
- CI: model-sync checks. A `model-sync` job fails a PR that changes code a TLA+
  model covers (`verification/model-map.toml`) without touching that model,
  unless it carries the `no-model-change` label and a `Model: no change because
  <reason>` line. Every lifecycle trace event kind must map to a trace-spec
  action; every model must be trace-validated or allowlisted as TLC-only with
  a tracking item (Coordinator, Lifecycle, DeferredReplies, DeferredJobs);
  per-model trace coverage has a floor that may only go up.
- Design: introspection v1 for 0.9 (priority tier 2), in review:
  [`docs/design/introspection.md`](docs/design/introspection.md). Per-actor
  `process_info`, `sys:get_state`/`get_status` answered between turns,
  supervision-tree and system snapshots, an opt-in stall watchdog (rank 8) and
  the data model for `spawned-top` (rank 30), an attachable remote console on
  a local Unix socket (`spawned-ctl`, on by default, read-only by default, no
  TCP, no eval), with a cost table per field, consistency guarantees and a
  test plan. TLA+ model `Introspection.tla` (fenced watchdog kill, status
  between turns and at death) with six negative controls. Design only; no
  library code change.

- Design: introspection v1 for 0.9 (priority tier 2), revision 2 after two
  reviews: [`docs/design/introspection.md`](docs/design/introspection.md).
- Design: introspection v1 for 0.9 (priority tier 2), revision 3 after two
  review rounds: [`docs/design/introspection.md`](docs/design/introspection.md).
  `process_info`-style `info()` readable while a handler is wedged (strictly
  free tier 0, richer with the `introspection` feature), `sys:get_state`/
  `get_status` as a turn of their own, supervision-tree and system
  snapshots, structured reports, an opt-in stall watchdog (rank 8) with a
  fenced kill, and an attachable remote console on a local Unix socket
  (`spawned-ctl`; on with `ActorSystem::new`, read-only by default, no TCP,
  no eval). TLA+ model `Introspection.tla` (every activity end, including
  panics in `started()` and `stopped()`, consumes the kill bit; status between
  turns and at D; runtime drop; one action per linearization point, status admission included) with 20
  negative controls; a fork-safe, lock-first socket publication protocol. Design only; no library code change.
- Tests: coverage follow-ups. `restart_modes_distinguish_normal_shutdown_and_panic`
  now sends a real `Normal` exit; a per-PR `MCSupervisionTemporary` TLC
  configuration makes `TemporaryStartsOnce` non-vacuous, with a negative
  control that restarts a Temporary child; threads-mode clock tests for
  `send_after`, `send_interval` and the restart window on a `thread_group` (T4).
- ROADMAP: a Priorities section (soundness, including a smaller stable API and
  soak tests; introspection; determinism; testing; client-driven features;
  performance behind a regression gate), and the runtime direction. Keep Tokio; rank 37 becomes the main
  performance track (generated dispatch, reusable reply slots, a single-wakeup
  mailbox, batched draining), after profiling; rank 22a becomes actor-owned
  I/O (`spawned::net`, Erlang ports) with a simulated network so whole client
  nodes run deterministically; rank 52 (io_uring) reopens only as a
  `spawned::net` backend.
- README: centred on why Spawned exists. A "Why Spawned" section first (guarantees
  verified once and inherited by every application; deterministic, replayable
  tests), both points leading the feature list, and a "Testing your own actors
  deterministically" section showing `#[spawned_macros::sim_test]`.
- ROADMAP: no-cost-on-the-default-path principle, the Erlang gaps table, and new
  ranks 6b (shared-state and abort lints), 8b (simulator check for handlers
  that never yield), 8c (per-group stack size) and 44a (execution groups in a
  separate OS process); rank 18a's tracking allocator is opt-in.
- Tests: semantics-contract coverage audit. `docs/TESTING.md` has a
  guarantee-by-guarantee matrix (checks, modes and placements, whether a
  check fails when the guarantee breaks): 31 of 50 guarantees were fully
  covered, now 46. New `sim_ordering` checks D1, R2, M1, L1, D3/D4/L7 and X7
  under PCT and bounded DFS; `semantics_gaps` checks the stop window, D5,
  M1, M4, M5, L2, L5, R3, T3 and S6 on every placement and V2, V4, V6, V9,
  S1 and S3 on the simulator. No bugs found; several contract citations
  corrected.
- Simulator (rank 8b): under `--cfg spawned_sim`, a test fails when one actor
  turn runs longer than a wall-clock budget inside a single poll (a blocking
  call or a loop with no `.await`), naming the actor and the message type,
  with the usual seed and tape replay lines. Default 250 ms;
  `SPAWNED_SIM_TURN_BUDGET=ms` tunes it and `0` disables it. Compiled only
  under the cfg: no cost in normal builds.
- Testing: planted-bug sensitivity matrix. Hand-written realistic bugs in
  supervision, links, monitors, exit reasons, registry, mailbox, timers,
  shutdown ordering and handle lifetimes are applied one at a time and run
  against the test suite, the simulated suites and the scenario fuzzer
  (`testing/planted/`, `docs/TESTING.md#planted-bug-sensitivity`); new oracles
  close the escapes. Work in progress.

- Testing: planted-bug sensitivity matrix (`docs/TESTING.md#planted-bug-sensitivity`).
  24 hand-written realistic bugs (`verification/planted/patches/`, re-run with
  `verification/planted/run.sh`) against the test suite, simulated suites,
  fuzz harness tests, a 600 s fuzz run and TLA+ trace validation: 23 caught
  (22 by the test suite), one unobservable through the public contract. New
  oracles: a Temporary nested subtree is never rebuilt, fuzzer oracle X8 (a
  stopped actor publishes the reason it was stopped with), and unit tests that
  kill cargo-mutants survivors in `supervision/nested.rs` and `supervision.rs`.
- `#[actor]` lints (ROADMAP rank 6b), compile time only, no runtime cost:
  a warning when the `#[actor]` impl block builds or accepts shared mutable
  state (`Arc<Mutex<_>>`, `Arc<RwLock<_>>`, `Arc<Atomic*>`, `Rc<RefCell<_>>`,
  std, tokio or parking_lot paths), silenced with
  `#[actor(allow_shared_state)]`; and a warning, once per crate, when the
  crate is built with `panic = "abort"`, under which a handler panic ends the
  process instead of one actor, silenced with `#[actor(allow_panic_abort)]`.
  Both use the stable `#[deprecated]` technique, so `-D warnings` turns them
  into errors.
- `spawned-concurrency` re-exports `#[sim_test]`, so downstream crates write
  `#[spawned_concurrency::sim_test]` without a `spawned-macros` dependency
  (they still need `spawned-rt`). New example test
  `examples/capabilities/tests/deterministic_simulation.rs` (per-sender order
  over every seed, an hour-long timer in virtual time), run by the `sim` CI job.
  The README's deterministic-testing section uses the single import.
- Lockfile: `rustix` 1.1.2 -> 1.1.6 for RUSTSEC-2026-0486 (pulled in by the
  examples' `native-tls` dependency; spawned's crates are unaffected).
- CI: a `sim` job runs the simulated test suites (`--cfg spawned_sim`) on
  every pull request: all integration tests of `spawned-concurrency` and
  `spawned-rt` plus the executor unit tests, one test per process with a
  120 s timeout (`scripts/run-sim-tests.py`), decision tapes uploaded on
  failure. `stack_budget` is compiled out under the simulator: it measures
  real thread stacks.
- CI: the core workflows (`ci`, `verification`, `failpoints`) also run on
  `merge_group`, so pull requests go through GitHub's merge queue, which runs
  all 19 required checks (including `sim`) on the merged result before
  anything lands on `main`.
- Design (not implementation) for rank 11, deferred typed replies and
  pipe-to-self, revised over four passes after independent reviews:
  `docs/design/deferred-replies.md`, client mapping for ethrex and ethlambda,
  and the `DeferredReplies` (reply slots) and `DeferredJobs` (pipe jobs) TLA+
  models with 22 negative controls, each checked against its named
  invariant.
- Design (accepted, not implemented): exit signals, supervisor startup and
  runtime-drop semantics as in OTP (ROADMAP ranks 5e, 5f, 5g): `kill`/`Killed`,
  `exit`/`try_link`, `NoProc`, parent exits, `stopped()` skipped on kills and
  untrapped signals, first-start failures abort supervisor startup. Adds the
  base actor lifecycle TLA+ model (`Lifecycle.tla`) with negative controls for
  the v0.7 behaviours and the PR #182/#185 bugs; see
  `docs/design/otp-exit-signals.md`.
- Test: coverage-guided scenario fuzzing on the deterministic simulator
  (`fuzz/`, cargo-fuzz + `arbitrary`): generated operation sequences over
  actors, links, monitors, timers, supervisors and systems, checked against
  the semantics contract, with seed/strategy-fixed inputs, decision tapes,
  `cargo fuzz tmin` -> `repro --emit` regressions, coverage and TLA+ trace
  sample scripts, and a 60 s CI smoke run on pull requests (no scheduled
  runs). A first 3-hour campaign found no framework bug. Adds
  `spawned_rt::tasks::sim::stats()` (simulation builds only).
- CI: keep the per-PR TLA+ checks fast again (about 52 minutes, sometimes
  hitting the 60-minute timeout, down to about 8). The per-PR `MCLifecycle`
  is a smaller instance (7.5M states instead of 43M, fewer reasons, no
  actor-to-actor message, which moved to `MCLifecycleFence`); every
  Lifecycle negative control still fails on it on its named invariant. The
  former instance runs weekly as `MCLifecycleWideDeep`. The TLA+ checks run
  as parallel jobs (`tla-models` legs, `tla-traces`, weekly `tla-deep` with
  one runner per deep configuration) behind a required `tla` job. The
  trace-coverage cache (`coverage-model.json.gz`) is keyed only on the specs
  it is computed from, so unrelated models no longer invalidate it, and is
  regenerated (same keys and state counts). `check.sh` gains `--only` and
  `--exclude`.
- Tests: a long-running soak harness (`concurrency/benches/soak`, run by hand,
  not in CI). It churns actors in both modes and every backend, managed
  groups, supervisors with every strategy, links, monitors, `trap_exit`,
  timers, the registry, timed-out requests, stream listeners, `ActorSystem`
  cycles and mailbox bursts from a seed, samples RSS, live heap, OS threads,
  file descriptors, Tokio tasks, registry size, live actors and the `metrics`
  gauges into a CSV, fails on growth per hour after warm-up or on any count
  not returning to its baseline after quiesce, and has a `--inject-leak`
  negative control. Run it before each release (`docs/TESTING.md`).

- Determinism D1-D3 (priority tier 3). Under `--cfg spawned_sim`:
  wall-clock and OS-entropy reads made inside a simulated run return virtual
  time and seeded bytes, so `Instant::now`, `SystemTime::now`,
  `rand::thread_rng`, `OsRng` and `HashMap` iteration order replay per seed
  with no call-site change (D1; madsim's technique, in the new sim-only crate
  `spawned-sim-libc`, which holds all the unsafe interposers so the other
  crates stay `forbid(unsafe_code)`; Linux/glibc and macOS). Each harness run
  gets a fresh thread, so bodies passed to `sim_test`/`run_once`/`explore`
  must be `Sync`. `SPAWNED_SIM_STRICT=1` (`sim::set_strict`) fails the run on
  such a read, naming the caller. `spawn_blocking` runs inline as one
  simulator step instead of panicking (D2; not concurrent under simulation).
  `spawned_rt::tasks` gains `sleep_until`, `interval`/`interval_at`
  (`Interval`, `MissedTickBehavior`) and `JoinSet` (`AbortHandle`,
  `JoinError`), virtual under the simulator and Tokio's otherwise, and a
  direct `tokio::time` timer fails the run with a message naming the
  replacement (D3). See `docs/TESTING.md` and `docs/MIGRATION.md`.

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
