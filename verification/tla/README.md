# TLA+ models

Formal models of Spawned lifecycle contracts, checked with TLC. See the
[verification strategy](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/docs/research/verification-strategy.md) for how
this fits with Loom, Shuttle, Miri, ThreadSanitizer and mutation testing.

| Model | Covers |
|---|---|
| `Supervision.tla` | The implemented OneForOne supervisor: `run_supervisor` and `run_child` in `concurrency/src/supervision.rs`, restart modes, rolling restart budget, ordered startup, reverse shutdown, shutdown deadlines, cleanup-stuck failures. |
| `MCSupervision.tla/.cfg` | Fast instance (CI, every PR): two children (Permanent, Transient), safety, action properties and liveness. |
| `MCSupervisionTemporary.tla/.cfg` | Small instance (CI, every PR, `other` leg, seconds): a Temporary and a Permanent child, safety and action properties, so `TemporaryStartsOnce` is not vacuous; `negative_checks.py` restarts the Temporary child and requires a violation. |
| `MCSupervisionDeep.tla/.cfg` | Deep instance (weekly, `--deep`): three children, one per restart mode, safety and action properties. |
| `TraceSupervision.tla` | Trace validation: replays a lifecycle trace recorded from real Rust runs as a behavior of `Supervision.tla`. |
| `Cohorts.tla`, `MCCohorts*.tla/.cfg` | OneForAll/RestForOne range selection, reverse cleanup, whole-cohort join barrier, ordered resume, stale notifications, superseding rounds and retired slots. Three-child mixed-mode and two-round configurations. |
| `Coordinator.tla`, `MCCoordinator.tla/.cfg` | System shutdown coordinator: ordered stop of admitted roots, panic fallback that signals all remaining roots, records failure and retains runtime ownership until joins; ordered-shutdown invariants, cooperative cleanup liveness and four negative mutations. |
| `NestedSupervision.tla`, `MCNested.tla/.cfg` | Dynamic subtree ownership and shutdown delivery; parent scope completion waits for descendant completion. Bounded three-node/two-generation model with fair cleanup. |
| `DeferredReplies.tla`, `MCDeferredReplies*.tla/.cfg` | Rank 11 design model (not implemented), part 1: reply slots, send-time deadlines with shortening, deferral credits shared by a token and its first linked job, the exiting phase with exit-pending drops (unwound or dropped while exiting, answered at death with the final reason), a stuck phase (cleanup deadline passed while `stopped()` runs: nothing closed early, death later or never), death, and downstream request cleanup. Fourteen named negative controls. |
| `DeferredJobs.tla`, `MCDeferredJobs*.tla/.cfg` | Rank 11 design model, part 2: pipe jobs with separate execution, cancellation and notification states; keyed replacement including notified jobs; optional job deadlines; a FIFO mailbox with emit credits and the terminal message last; `ActorId` fencing. `MCDeferredJobsEmits` is the fast configuration with emits; `MCDeferredJobsHeld` the one with delayed emits (`emit_at`) and held results (completion, held notification, scheduled emits and credit ownership as separate steps). Eleven named negative controls. |
| `Lifecycle.tla`, `MCLifecycle{,Live,Fence,Deep,LiveDeep,WideDeep}.tla/.cfg` | Base actor lifecycle as proposed for v0.8 ([design](../../docs/design/otp-exit-signals.md)): links, monitors, `trap_exit`, exit signals (`Killed`, ignored `Normal`, `NoProc`, parent exits), administrative stops, requests around death, `Exit`/`Down` ordering. Nineteen negative-control faults (19 safety checks, 2 liveness checks): the v0.7 exit-signal behaviours (direct stops ignoring `trap_exit`, kills ending `Kill`, unfenced `unlink`/`demonitor`, real reasons for late registrations, `stopped()` after kills and untrapped signals, late signals ignored during exit), the PR #182/#185 bugs, early exit errors, kill and panic precedence, runtime-drop reasons, early notification, a split death point, a parent exit reaching `exit_received`, an exit signal past the sender's death point (`NoSignalFence`) and publication before the queued requests were answered (`PublishBeforeDischarge`). Death is two steps, as in `actor_cleanup.rs`: the death point (`Die`/`RuntimeDrop`: signal fence, seal, table closed) and `Discharge` (queued and exit-pending requests answered), with other actors' steps in between; `Publish` waits for `Discharge`. Per PR: `MCLifecycle` (10.0M states, fewer reasons, where the safety controls run), `MCLifecycleFence`, `MCLifecycleLive`; weekly: `MCLifecycleWideDeep` (the former 43M-state per-PR instance), `MCLifecycleDeep`, `MCLifecycleLiveDeep`. |
| `TraceCohorts.tla`, `TraceNested.tla` | Policy and ownership projections of real cohort/nested Rust executions. |
| `trace_check.py`, `negative_checks.py` | Replay recorded traces; deliberately wrong selections, premature resume/join and broken propagation must be rejected or produce counterexamples. |
| `SupervisionInd.tla`, `CohortsInd.tla`, `NestedSupervisionInd.tla`, `CoordinatorInd.tla`, `MC*Ind.tla`, `apalache.sh` | Inductive invariants (`IndInv`) checked with Apalache; see [Inductive invariants](#inductive-invariants-apalache). |
| `*Coverage.tla`, `Cov*.tla`, `CovTrace*.tla`, `trace_coverage.py`, `coverage-model.json.gz`, `coverage-baseline.json` | Trace coverage: which model actions, projected states and transitions the recorded traces reach; see [Trace coverage](#trace-coverage). |

## Running

TLC is a Java program. The repository flake provides it (with its own JDK), so
nothing needs to be installed globally:

```bash
nix run .#check-tla            # model-check every MC*.cfg except *Deep.cfg
nix run .#check-tla -- --only 'MCLifecycle*'           # a subset (comma-separated globs)
nix run .#check-tla -- --deep --only MCLifecycleDeep   # one deep configuration
nix develop -c tlc MCSupervision -workers auto   # ad hoc, from verification/tla
```

**CI.** On every PR (`.github/workflows/verification.yml`) the per-PR
configurations run as parallel `tla-models` legs (`supervision`, `lifecycle`,
`deferred`, and `other` for every configuration no leg names), next to
`tla-traces` (trace validation, negative controls, trace coverage). Weekly and
on dispatch, `tla-deep` checks each `*Deep.cfg` on its own runner. The
required check `tla` passes only if all of them passed.

## Trace validation

Model checking explores the configured bounded abstraction; trace validation checks that recorded implementation executions conform to it. Neither proves all Rust executions correct. With the `lifecycle-trace` feature
(off by default, compiled out otherwise) `run_supervisor` and `run_child`
record one event per model action, with its parameters, per supervisor
(`concurrency/src/lifecycle_trace.rs`). `concurrency/tests/supervision_trace.rs`
runs real actors through the scenarios below and writes each trace as JSON;
TLC then checks that each trace is a behavior of `Supervision.tla`.

```bash
cargo test -p spawned-concurrency --features lifecycle-trace,failpoints --test supervision_trace --test cohort_trace --test tree_trace --test nested_supervision --test coverage_trace --test job_trace --test reply_trace --test lifecycle_trace
nix run .#check-tla -- --traces-only target/lifecycle-traces --negative-controls   # or --traces DIR to also model-check
nix develop -c python3 verification/tla/trace_check.py target/lifecycle-traces
```

The test writes to `$LIFECYCLE_TRACE_DIR` (default `target/lifecycle-traces`).
`trace_check.py` prints `ACCEPTED` per trace, or `REJECTED` with the longest
matched prefix and the first event no model behavior can take; `--keep DIR`
keeps the generated modules and TLC logs.

Scenarios: normal start and stop of three children with mixed modes;
Permanent and Transient panics and restarts; Transient normal exit (no
restart); Temporary exit; `ChildHandle::shutdown` of running children;
replacement factory error then success; first-start factory error and
first-start timeout (both fail the start, S1); a ready child's exit deferred
during the initial start (`DeferExit`/`Undefer`, S1); non-restartable startup
failure; restart budget exhaustion and disabled retries; shutdown during
backoff; replacement startup timeout;
`Supervisor::shutdown` with a short deadline and a hanging `stopped()`
(`ShutdownDeadline`, `SupChildTimeout`, `CleanupStuck`); and seeded random
crashes, normal exits and external shutdowns racing a stop or shutdown.

How it works:

- `TraceSupervision.tla` extends `Supervision` with a log index `i`;
  `TraceNext` takes the action named by `Trace[i+1]` with its parameters. The
  invariant `TraceIncomplete == i < Len(Trace)` is violated exactly when the
  whole log is consumed, so TLC reporting that violation means ACCEPTED. The
  model's `Safety` invariant is checked along the way.
- Linearization: model actions are atomic, the code's are not. Under the
  feature, every gate-dependent decision (`is_open`/`if_open` and what follows
  from it), the gate flip in `Core::close_admission` and each failure that
  closes the gate record their event inside one process-wide critical section,
  so recorded order is a valid linearization. A `Core::fail` that is part of a
  model action (budget exhausted, cleanup stuck, non-restartable startup
  failure, child cleanup deadline) is recorded as that action, not as `Close`.
- Retry capacity is reserved before backoff (`RetryReserve`) and refunded on
  cancellation (`RetryRelease`). Only a grant enters the rolling time window.
  `WindowExpire` may stutter before reservation or `BackoffDone`; logged total
  capacity pins the abstract count, and pending reservations cannot expire.
- An exit reason not yet known when cleanup gets stuck is logged as
  `unknown` and matches any reason. Paths the model does not cover (placement
  failure, execution-group closure, controller panic) record an `Unmodelled`
  event, which rejects the trace.

Trace validation found one model gap: a ready actor can exit with `Shutdown`
while its supervisor is open (`ChildHandle::shutdown`), which the design's
restart table covers (Permanent restarts, Transient stays stopped), so
`SelfExits` now includes `Shutdown`. A second gap, found through trace
coverage: a generation can also stop with `Shutdown` before readiness while
the supervisor is open (an actor stopping itself, or shut down, in
`started()`), so `StartupFailed` allows it. The same trace exposed a code bug:
a Transient replacement stopping `Normal` or `Shutdown` before readiness
failed the tree; it is now retried (`Retries` in `Decide`, and
`RestartFollowsPolicy` allows a failed restart attempt of a non-Temporary
child), and a trace edited back to failing the tree is a negative control. Hand-edited traces (a `Close` moved
before an `Attempt`, a restart after a Transient normal exit, forward shutdown
order, budget exhaustion with a free retry) are rejected at the edited event.

## Trace coverage

Trace validation says every recorded run is a behavior of the model. Trace
coverage asks the converse: which parts of the model's reachable behavior do
the recorded runs never exercise? `trace_coverage.py` reports it per model with
trace validation (`Supervision`, `Cohorts`, `NestedSupervision`).

```bash
cargo test -p spawned-concurrency --features lifecycle-trace,failpoints --test supervision_trace --test cohort_trace --test tree_trace --test nested_supervision --test coverage_trace --test job_trace --test reply_trace --test lifecycle_trace
nix develop -c python3 verification/tla/trace_coverage.py --traces dedicated=target/lifecycle-traces \
    --model-keys verification/tla/coverage-model.json.gz --baseline verification/tla/coverage-baseline.json
# every test binary: each finished supervisor dumps its traces
LIFECYCLE_TRACE_ALL=/tmp/all cargo test -p spawned-concurrency --features lifecycle-trace,failpoints --no-fail-fast
nix develop -c python3 verification/tla/trace_coverage.py --traces dedicated=target/lifecycle-traces \
    --traces whole-suite=/tmp/all --model-keys verification/tla/coverage-model.json.gz --json coverage.json
```

**Projection.** Recorded traces run with their own constants (3 children,
`max_restarts` 5, unbounded generations) and the model-checking
configurations with others, so concrete states cannot be compared. Both sides
are mapped onto keys defined in TLA+ (`SupervisionCoverage.tla`,
`CohortsCoverage.tla`, `NestedCoverage.tla`):

| Kind | Key | Example |
|---|---|---|
| A | action and outcome | `A\|Decide/restart` |
| S | one per child (node): mode, controller pc, supervisor phase, gate, deadline, failure, budget bucket (0, mid, max), reservation | `S\|Permanent\|backoff\|await\|open\|mid\|reserved` |
| T | action key in its pre-state's context: the acting child's mode, or for supervisor and environment actions each child's mode and pc | `T\|Close\|Permanent\|backoff\|starting\|open\|mid` (close while a child backs off during startup) |
| C | T without mode, budget bucket, failure flag (and cohort position) | `C\|Close\|backoff\|starting\|open` |

Cohort keys use the round phase, the child's position relative to the failing
child and how far it got in the round; nested keys use root/child, subtree,
the node's stage (created, started, closed, cancelled, done) and its parent's.

**Model side.** `Cov<Model>.tla` explores each configuration exactly as the
model does (an extra action label is excluded from the fingerprint with
`VIEW`; the script checks the distinct-state count equals the plain model's)
and an always-true action constraint prints every key of every reachable step
(`TLCGet`/`TLCSet` deduplicate per worker). Configurations: `MCSupervision`
plus a Permanent/Temporary instance at the same bounds (6.1 million states),
the four `MCCohorts*` (174,000) and `MCNested` (47,000). This takes about 5
minutes, so the keys are cached in `coverage-model.json.gz` with a hash of the
files they come from (the modules above, their configurations and what they
extend; other models such as `Lifecycle.tla` do not invalidate it); a stale
cache is recomputed and rewritten (commit the new one, or CI recomputes it on
every run).
`Deliver` in `NestedSupervision` is a fairness device with no trace event, so
its keys are left out.

**Trace side.** `CovTrace<Model>.tla` replays each trace like `trace_check.py`
with a history variable collecting the keys of each step; the accepted
behavior's final value is the trace's coverage. Where an event admits several
model steps (an `unknown` exit reason, an unlogged window expiry) the first
accepted behavior TLC finds is used. Rejected traces are listed and not
counted.

**Report.** Coverage per kind and source (cumulative), the number of trace
keys outside the configured bounds (for example 3-child interleavings or
`max_restarts` 0), and the unreached actions and coarse transitions ranked by
interest: shutdown, deadline or failure in the context, a child mid-restart
(backoff, attempt, construction, cleanup), exhausted budget, stale cohort
notifications, nested restart under a closing parent. `WindowExpire` keys rank
low (timing only). `--json` writes every key set.

**CI.** The `tla` job prints the report for the dedicated trace suites on
every PR. Coverage percentages never fail the job: S and T keys depend on
scheduling (the dedicated suites' state count varies by about 2% between
runs). The job fails only if an action key in `coverage-baseline.json` is no
longer covered. These are the A keys covered in each of three runs of the
dedicated suites; a refactor that silently stops a scenario from reaching its
action (say, a budget-exhaustion test that no longer exhausts the budget)
still passes trace validation but loses its key. Regenerate the baseline with
`--write-baseline` when a test deliberately changes. Weekly, the job runs every
test binary with `LIFECYCLE_TRACE_ALL` and uploads the full report.

### Results

Before and after the tests in `concurrency/tests/coverage_trace.rs`
(cumulative; whole suite = dedicated plus every other test binary, about 250
OneForOne, 100 to 115 cohort and 340 tree traces per run; 13 OneForOne traces
are rejected, see below). "After" ranges are over three runs of the dedicated
suites and two of the whole suite: S and T keys depend on scheduling.

| Model, source | Actions | States | Transitions | Coarse transitions |
|---|---|---|---|---|
| Supervision, dedicated | 33 → 35 / 40 | 195 → 231-234 / 1,878 | 211 → 255-259 / 7,130 | 63 → 86-89 / 544 |
| Supervision, whole suite | 39 → 39-40 / 40 | 263 → 279-294 | 310 → 326-347 | 105 → 106-116 |
| Cohorts, dedicated | 40 / 46 | 166-177 / 1,790 | 133-146 / 2,290 | 71-79 / 619 |
| Cohorts, whole suite | 40 / 46 | 329-334 | 273-280 | 120-124 |
| Nested, dedicated | 11 / 11 | 46 / 82 | 35 → 36 / 207 | |
| Nested, whole suite | 11 / 11 | 53 / 82 | 45 → 46 / 207 | |

After #233 (failed restarts retried like OTP; a `Shutdown` startup failure
allowed while open; one start per cohort child and round) the model side was
regenerated: Supervision has 7,142 transition keys (was 7,130) and 545 coarse
ones (was 544), Cohorts the same key counts over more states. The dedicated
suites, now including #233's traces (30 OneForOne, 8 cohort), cover 36 of 40
Supervision actions: `StartupFailed/Shutdown` (a replacement stopping with
`Shutdown` while the supervisor is open) joined the baseline. Not in the
dedicated suites: `BecomeReady/closed` and `LoopTop/finish` (closure racing a
publication or a loop iteration), `StartupFailed/Panic` (a panic in
`started()` of a supervised child) and `WindowExpire` (an expiring restart
window between retries).

The one whole-suite action that comes and goes is `LoopTop/finish` (a
controller seeing the closed gate at the top of its loop): only a race in some
non-trace test reaches it, so it is a candidate for a deterministic test.

The low state and transition percentages are expected: the model is
nondeterministic about timing (`WindowExpire` anywhere, any child in any
phase when shutdown starts), and many projected combinations are reachable
only through interleavings tests do not force. The ranked list is the useful
output. The new tests reach, each with a real supervisor:

- `A|Attempt/closed`: admission closes between a controller's first loop
  iteration and its admission check (failpoint `system.reserve` delays the
  second child's reservation while the start future is dropped). No other
  test reached it.
- Close while a child backs off during initial startup, then `BackoffDone`
  closed and `SupStartDone` not ready with a child in backoff.
- Shutdown while a restart is under construction (`Close` and the stop turn
  with the restarting child in `constructing`), then the caller's deadline.
- A Transient child stopping `Normal` in its first `started()` after a
  sibling is ready: `StartupFailed/Normal` then `Decide/fail` during startup.
- A ready child crashing while a later sibling is still being constructed.
- A leaf restarting inside a nested supervisor (`TBegin/restart` on a child
  node); the existing nested trace escalates every leaf failure.

Top unreached after these tests:

- Supervision: a child's stop turn timing out (`SupChildTimeout`) or the
  caller's deadline expiring while that child is still restarting (backoff,
  attempt, construction); `CleanupStuck` and startup timeouts after the
  deadline. Each needs a restart held mid-flight while a later child's
  cleanup hangs.
- Cohorts: no stale notification was ever recorded: `CRequest` not accepted
  (a failure reported for an already covered round), `CClean` and `CAck` for
  an older round. The model's supersession handling for these is checked by
  TLC only.
- Nested: a child supervisor restarting a child while its parent is closing
  or cancelled (`TBegin/restart` under `parent:closed`), and an owner join
  (`TJoined/owner`) of a non-root subtree under a closing parent: nested
  escalation during shutdown.

Rejected whole-suite traces (13 OneForOne): eight from
`initial_placement_failure_has_deterministic_failed_slots` and one each from
the coordinator-fault, controller-panic and placement-failure failpoint tests
record `Unmodelled` events (placement failure, controller and coordinator
panics are outside the model). `ready_generation_discarded_before_publication_is_never_current`
is rejected at `StartupFailed` with reason `Shutdown` while the gate is open.
Followed up in #233: the same step happens without failpoints (an actor
stopping itself with `Shutdown`, or shut down, in `started()`), so it is a
model gap, and it exposed a code bug: a Transient replacement stopping
`Normal` or `Shutdown` before readiness failed the whole tree instead of
being retried, unlike OTP. With #233 the trace is accepted.

## What is checked

Invariants (every reachable state):

- `AtMostOneGeneration`: never two live generations of one child slot.
- `PublishedIsLive`: `SupervisedChild::current()` only exposes the latest live generation.
- `BudgetBounded`, `TemporaryStartsOnce`.
- `ReadyMeansAllStarted`: the supervisor never reports a partly ready tree as ready.
- `DoneMeansQuiescent`: done implies no live generation and every controller finished.

Action and temporal properties:

- `NoStartAfterClose`: no generation starts or is published after closure.
- `RestartFollowsPolicy`: restarts follow the Permanent/Transient/Temporary table
  (startup timeouts and failed restart attempts restart unless Temporary).
- `ReverseShutdown`: ready children get their stop turn only after every later
  child finished or timed out, unless the shutdown deadline forced `stop_remaining`.
- `NeverReopens`, and liveness `ClosedLeadsToDone` assuming cleanup completes.

## Inductive invariants (Apalache)

TLC explores the states reachable within a configuration's bounds (MaxGen,
search depth). Apalache 0.62.4 (symbolic, SMT-based) additionally checks that
each model's safety invariants are *inductive*: for a strengthening `IndInv`
(the invariants, a type predicate and a few helper lemmas),

```text
Init => IndInv                 apalache-mc check --init=Init   --inv=IndInv --length=0
IndInv /\ Next => IndInv'      apalache-mc check --init=IndInv --inv=IndInv --length=1
```

The step check starts from *every* state satisfying `IndInv`, reachable or
not, so it has no depth bound; together the two checks imply the invariants
hold in every reachable state, however long the run. For `Supervision`,
generation numbers are unbounded SMT integers and no `GenBound` constraint is
used; the action properties (`NoStartAfterClose`, `RestartFollowsPolicy`,
`ReverseShutdown`, `NeverReopens`) are checked as one-step action invariants
from `IndInv`. "Unbounded" still means one instance size: the checks hold for
the declared constants, not for all N.

```bash
nix run .#check-apalache                          # default sizes
nix run .#check-apalache -- --negative-controls   # plus guard-removal mutations
nix run .#check-apalache -- --model Supervision --size 10 10
nix run .#check-apalache -- --model Coordinator --targets 32
```

| Model | Constants | Restart modes / strategy | Helper lemmas |
|---|---|---|---|
| `Supervision` | N = 5, MaxRestarts = 4 (CI also 10/10) | every `Mode` table | live generation = latest one while constructing/running/cleanup; `pub` = gen exactly while running; idle until launched; ordered-start prefix is `initial`; a retrying controller holds a restartable exit reason; children after the stop cursor are done or timed out; done only after every controller finished; reservations within budget |
| `Cohorts` | N = 5, MaxGen = 4, MaxRounds = 4 | both strategies, every `Mode` table | stopped ⊆ affected, clean ⊆ stopped, resumed ≠ {} ⇒ clean = affected, no resumes while assigning/stopping; Done ⇒ closed |
| `NestedSupervision` | 6 nodes, 3 slots, MaxGen = 3 | n/a | none: `Safety` is inductive as written |
| `Coordinator` | TargetCount = 8 (CI also 32) | n/a | one: the stop cursor is at target 1 before shutdown begins |

Each helper is commented in the `*Ind.tla` module with the reason it holds;
dropping any one of them gives a counterexample to induction (an unreachable
state). All checks with negative controls take about 3 minutes on a Linux
server (4 JVM cores); `--size 10 10` for `Supervision` about 40 seconds and
`--targets 32` for `Coordinator` about 10 seconds.
No counterexample exposed a real bug.

### Coordinator

`Coordinator.tla` abstracts `begin_shutdown` in `concurrency/src/system.rs`.
Targets are numbered in stop order: target 1 is the last-admitted root, since
the Rust loop walks `targets.iter().rev()`. Proven inductive at TargetCount 8
(and 32), from every state satisfying `CoordinatorInd!IndInv`:

- `Safety` (the existing TLC invariant): admission is closed in every phase
  after `Running`; `finished ⊆ signalled ⊆ Targets`; runtime ownership is
  released only in `Done` with every target finished; a coordinator crash is
  always reported.
- `OrderedShutdown` (new, also checked by TLC): in the ordered loop exactly
  the targets before the cursor (plus the cursor once signalled) are
  signalled, and all targets before the cursor have finished.
- `JoinAfterAllSignalled` (new, also checked by TLC): the coordinator joins
  only after every target was signalled, by the ordered loop or by the
  emergency fallback.
- Action properties over one step: `OrderedSignalStep` (outside the
  emergency fallback a target is newly signalled only when every earlier
  target in stop order finished), `NeverReopensStep`, `ReportKeptStep`
  (a recorded failure is never dropped) and `TerminalStep` (finished targets
  stay finished; release is permanent).

The liveness property `CleanupCompletes` remains TLC-only (TargetCount 3).

Negative controls (each must break induction, and must also give a
counterexample reachable from `Init` within 4 steps, so none is an
artifact of an unreachable `IndInv` state; all do): the four `Fault`
values `ReleaseEarly`, `ForgetFailure`, `ForgetSignal` and `RollbackReady`,
plus four guard mutations: advance past a target before it finished, stop
targets in reverse of the stop order (checked against `OrderedSignalStep`),
and begin shutdown or crash without closing admission. `RollbackReady` is
the #209 bug: once admission closed, a supervisor startup guard dropped after
readiness rolled the ready tree back, stopping it outside the coordinator's
order. The fixed code rolls back only unready startups, which are not targets
in this model, so the fault is an extra action that is disabled unless
`Fault = "RollbackReady"`; TLC state counts are unchanged (81 distinct
states). `negative_checks.py` runs the same fault under TLC, and now catches
`ForgetSignal` both as a safety violation (`JoinAfterAllSignalled`) and,
with that invariant removed, as the original liveness violation.

What the model does not cover: per-unit admission (`register_unit` checks
`closing` under the same lock `begin_shutdown` sets it, but no admission
step is modelled, so "nothing admitted after close" is only `open` staying
false); readiness itself (all targets are ready roots); execution groups
(`group.done`); and the exact grace-expiry path. At the deadline Rust keeps
walking the remaining targets in order with a zero timeout, calling
`stop_remaining`, then re-signals every scope; the model collapses this into
`GraceExpired` followed by `Emergency`, which signals all targets at once. A
crash before `Begin` is also allowed by the model but cannot happen in Rust,
where the coordinator task exists only after `begin_shutdown`.
`record_coordinator_failure` dedupes and keeps at most 16 messages; the
boolean `reported` stands for "the first failure is recorded", which holds
because the coordinator panic is the first record of the single
`begin_shutdown` run.

Non-vacuity: `apalache.sh` requires probe predicates to be violated (IndInv
admits, for example, five running children at generation > 1000 mid-shutdown),
and `--negative-controls` removes one guard at a time and requires induction
to fail: for `Supervision` the retry limit, ordered start, join wait, restart
table, cleanup, unpublish-on-stop, admission gate on `Attempt` and the
reverse-shutdown wait; for `Cohorts` and `NestedSupervision` the existing
`Fault` mutations (`Selection`, `Cleanup`, `Retirement`, `Closed`, `InPlaceRetry`, `FirstStartRetry`, `PerChildCharge`, `Join`);
for `Coordinator` see below.

The type annotations are `\* @type:` comments, ignored by TLC. Apalache also
needs constant range bounds, so `r \in 1..s.epoch`-style guards in `Cohorts`
and `NestedSupervision` are written as comparisons (the same predicate); TLC
state counts are unchanged.

## Abstractions

Time is nondeterministic: backoff, startup and cleanup deadlines and the
rolling window are actions, not clocks. The admission gate and `core.cancel`
close together (`Core::close`), so they are one variable. Execution-group
closure and placement failure are not modelled yet. Each action corresponds to
a point in the Rust code so that recorded traces can be checked against the
model (see Trace validation above).

## Scope of the capability extension

The original `Supervision` / `TraceSupervision` pair checks OneForOne only.
Separate cohort and nested ownership projections now cover the strategy and
subtree extension below. These checks do not cover CPU placement or native
resource teardown, and do not establish correctness of every Rust schedule.
`context_exit_reasons` records explicit Context Shutdown/Kill exits against the
existing reason model. A negative control erases Kill and must reject the
resulting transient restart; the entry point adds no new model transition.
`queued_drop_panic` checks a queued destructor fault during ordinary stop and
the resulting transient restart. Erasing its Panic exit must reject the tape.
`queued_drop_shutdown_restarts` keeps the raw Shutdown reason but records the
destructor failure after the death point as a cleanup failure, which the trace
projects to the abnormal class (v0.8, otp-exit-signals.md "Phases of an
exit"): the Transient child restarts. Projecting the raw Shutdown instead must
reject the restart.
All library crates continue to forbid unsafe Rust; native synchronization and
Linux affinity use safe interfaces. No unsafe exemption was introduced.

## Cohort and nested coverage

`Cohorts` records selection, per-slot assignment, stop, actual scope join,
cleanup acknowledgement, ordered resume and readiness acknowledgement as separate
steps. A Temporary child is stopped but not reconstructed. Already terminal
Transient/Temporary slots are skipped; assignment and terminal publication race
under the slot lock. A round that wins assignment before retirement can still
include that controller. Delayed notifications carry their previous round and
must not trigger duplicate reconstruction after newer coverage was published.

The real trace scenarios include both strategies with a retired Temporary tail,
an upstream failure superseding a round while a later factory is pending,
three-level mixed-strategy subtree failure/replacement and root shutdown, and a
native leaf retaining a blocking helper beyond the shutdown report deadline.
Every cohort controller must record exactly one `Finish`, including early
retirement. `tree-v1` uses a separate globally ordered ownership tape, follows
actual parent/generation attachments and checks descendant `TDone` before parent
`TJoined`. Independent tests' trees are not merged. A child already failing may
close itself before observing parent cancellation; a propagation event is only
required when that path actually runs.

These are complementary projections. Cohorts abstracts handler bodies, time,
retry-window accounting and local exit classification; the original OneForOne
model checks those local rules. The tree model abstracts local restart selection
and stop-turn scheduling; cohort/OneForOne models and Rust tests cover those.
`ClosedLeadsToDone` assumes eventual cancellation delivery and resource cleanup;
a blocking helper that never returns cannot be preempted. Node, generation and
round bounds are explicit in each configuration. Links, monitors, trapped-exit
ordering and arbitrary Tokio/OS schedules remain outside these models.

`--negative-controls` requires all three trace suites above. Four edited real
traces must reject (wrong selection, early resume, early parent join, false
cancellation). Seven model mutations must yield counterexamples (selection,
cleanup, retirement, closed admission, in-round retry of one child, parent join
and shutdown propagation).
A separate unsafe-final-event control checks `Safety` before the full-log completion
sentinel. The ordinary `--traces-only` interface still accepts older OneForOne
trace collections without requiring the new negative controls.

## Next model

The base actor lifecycle model is `Lifecycle.tla` (proposed v0.8 contract,
[design](../../docs/design/otp-exit-signals.md#what-the-model-establishes)).
It checks that every sent request ends `Ok`, `ActorStopped` or `NoProc`; no
message is dispatched after stop; each monitor's `Down` is delivered at most
once and, unless its observer died, exactly once; linked non-trapping peers of
an abnormal exit stop; and the exit-signal rules of rank 5e. Reintroducing the
PR #182 bug (`Fault = "Retain182"`) and the PR #185 bug (`"Dispatch185"`) each
produce a counterexample (`negative_checks.py`).

**Lifecycle trace validation.** With `lifecycle-trace`, the runtime records
one `LifeEvent` per `Lifecycle.tla` action for the actors spawned while
`lifecycle_trace::record_lifecycle()` is on (both `tasks` and `threads`):
`Trap`, `Send` (a request from `Ext`, admitted or `NoProc`), `Dispatch` (with
the mailbox item kind), `Panic`, `Stop`, `Signal` (also a supervisor's
Shutdown or kill), `Link`/`Unlink`/`Monitor`/`Demonitor`, `Deliver` (a link
notice or `Down` applied by its receiver), `Hook` (`stopped()` runs or is
skipped), `Die` (D, final reason), `Drop` (runtime drop) and `Publish`. Each
is recorded in the critical section of the change it describes: the
dispatch with the run loop's stop check, the stop with its cancel, the
admission check with the push, D with the exit-table close. A
recorded actor's sections take the process-wide trace section first, so the
log is a linearization. `TraceLifecycle.tla` replays it with no unlogged
steps; its header lists the mapping (actors `a1`, `a2` in spawn order,
`start_linked` parents as `LinkParent`, every send a request from `Ext`,
folded request panics, the late `NoProc` notice of a non-trapping caller,
and `Hook` "skip" accepted after a kill that arrived before `stopped()`
began, which the model's `Kill` leaves out). Phase 0 (signals held during
`started()`; not in `Lifecycle.tla` either: actors start in `run`), managed
scopes' own stops and actor-to-actor sends are not mapped: they reject. `concurrency/tests/lifecycle_trace.rs` records 14
scenarios (`lifecycle_*.json`, schema `lifecycle-v1`): kill with a queued
request answered `Killed` and a later `NoProc`, kill during `stopped()`,
untrapped and `Normal` signals, trapped exits (also from an actor, then a
kill), late link/try_link/monitor (`NoProc`), a late link ending a
non-trapping caller, the unlink and demonitor fences between D and
publication, a monitor's `Down`, a `start_linked` parent exit to a trapping
child, a link notice ending a non-trapping peer, runtime drop (`Killed`), and
one threads-mode run. Controls (`lifecycle_trace_controls`): `stopped()`
after a kill or after an untrapped signal, `Kill` instead of `Killed`,
publication before D, a `Normal` signal stopping an actor, a late `Down` or
link notice with the real reason, a notice after unlink, a `Down` after
demonitor, a parent exit dispatched as an `Exit`, and a runtime drop ending
`Normal`: all eleven are rejected. `trace_coverage.py` skips this schema.
Source: [verification strategy](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/docs/research/verification-strategy.md#acceptance).

`Coordinator.tla` checks coordinator failure ownership independently of the actor
policy models: a panic closes admission, records failure, signals remaining
roots, and retains runtime ownership until all roots finish. Liveness assumes
signalled work eventually completes; uncooperative native code remains a host
escalation decision. Negative controls remove failure reporting, omit emergency
signals, release runtime ownership early, or stop a ready root out of order
through its startup rollback (#209). The same safety invariants are proven
inductive with Apalache ([Coordinator](#coordinator)). This is an abstract fault model,
not a replay of Rust failpoint traces. Private Rust tests separately inject
supervisor-launch and system-shutdown panics with real actor/helper cleanup.

## Deferred replies (rank 11 design)

The two models back the [deferred replies design](../../docs/design/deferred-replies.md).
They were written before the implementation and revised over three review
passes. They check the design's ordering and accounting decisions, not Rust
memory orderings or schedules. The design's *What the models establish*
section lists what is and is not covered.

**`DeferredReplies.tla`** has a discrete clock, two callers, owner A with
turns, a running-linked-job count per request, and downstream actor B.

- **Callers and deadlines.** Every request owns a slot with one atomic
  terminal transition. The deadline is fixed at send and may only shrink. A
  reply at or after the deadline expires the slot instead of delivering.
- **Dispatch.** Expired requests are still dispatched (OTP).
- **Turns.** A turn replies in place, or defers and then stores, pipes or
  drops the token. A turn may never end, and a panic drops the turn's tokens
  while unwinding.
- **Credits.** A linked job shares its token's credit only while the token is
  open and no other job of that request runs.
- **Job notifications** hand the token back to a turn; after a job deadline
  the work keeps running.
- **Exit.** While exiting, stored tokens can still be answered. A token
  dropped while exiting, or unwound by a panicking turn, is *exit pending*
  (`pendA`): it stays open and death answers it `Stopped` with the final
  reason (rank 5e, [otp-exit-signals.md](../../docs/design/otp-exit-signals.md#required-changes-to-rank-11-221-design-239-implementation)),
  unless the caller's deadline or abandonment comes first. At the cleanup deadline, a still-running `stopped()`
  makes A stuck: nothing is closed early, `stopped()` may still reply, and
  callers are bounded by their own deadlines. Only death closes the registry
  and discards the mailbox.
- **Downstream.** A job may call B, which has its own bound, and the
  downstream request is abandoned when the request's last job ends.

Invariants:

- `AtMostOneDischarge`, `NoOrphan`, `NoSuccessAfterDeadline`, `TimelyExpiry`,
  `DeadlineWithinCaller`;
- `BoundedA` (combined credits) and `BoundedB`;
- `PanicStopped`, `StoppedAtDeath` (a `Stopped` discharge happens only in
  the death step, where the reason is final; the model has no reasons, so
  this is how it shows callers get the final one), `NoEarlyClose`,
  `DownstreamOwned`, `CallerConsistent`.

Liveness is `EveryAcceptedDischarged` and `CallerNeverHangs`, with fairness
only on the clock and the deadline service.

**`DeferredJobs.tla`** has restarts with new `ActorId`s, unlinked and keyed
jobs with optional deadlines, a FIFO mailbox, in-flight emit credits, emits
only while the notification is pending, and a terminal message that takes no
credit.

- A new keyed job cancels every earlier uncancelled job for the key,
  including notified and finished ones.
- Dispatch fences on `ActorId` and on the job's cancellation flag.

Invariants:

- `NoLateResult`, `NoStaleAfterCancel`, `NoStaleKeyed`;
- `AtMostOneTerminal`, `TerminalLast`;
- `PermitsBounded`, `OneLivePerKey`, `EmitCreditsBounded`.

Liveness is `JobReported`, for jobs with a deadline.

**Negative controls.** `negative_checks.py` runs `named_fault`, which checks
only `TypeOK` and the named invariant (safety only), so each control must break
exactly the property it targets:

| Model (configuration) | Mutation | Breaks |
|---|---|---|
| `Supervision` (`MCSupervision`, a rewrite of `Supervision.tla`: no `Fault` constant) | `FirstStartRetry` (v0.7 retried a failed first start) | `RestartFollowsPolicy` |
| `Supervision` (`MCSupervision`, rewrite) | `NoDeferral` (v0.7 decided on a ready child's exit during the initial start) | `RestartFollowsPolicy` |
| `Cohorts` (`MCCohortsAll`) | `FirstStartRetry` | `FirstStartOnce` (in `Safety`) |
| `Cohorts` (`MCCohortsAll`) | `PerChildCharge` (v0.7 charged the restart budget per rebuilt child, not per round) | `RoundCharged` |
| `DeferredReplies` | `NoAutoReply` | `NoOrphan` |
| `DeferredReplies` | `NoAtomicExpiry` (abstract double discharge; the real race needs Loom) | `AtMostOneDischarge` |
| `DeferredReplies` | `LateSuccess` | `NoSuccessAfterDeadline` |
| `DeferredReplies` | `LoopDeadline` | `TimelyExpiry` |
| `DeferredReplies` | `Lengthen` | `DeadlineWithinCaller` |
| `DeferredReplies` | `Unbounded` | `BoundedA` |
| `DeferredReplies` | `NoDestAdmission` | `BoundedB` |
| `DeferredReplies` | `ExitStateOnly` | `NoOrphan` |
| `DeferredReplies` | `UnwindDropped` | `PanicStopped` |
| `DeferredReplies` | `ShareRunning` | `BoundedA` |
| `DeferredReplies` | `AttachClosed` | `BoundedA` |
| `DeferredReplies` | `LeakDownstream` | `DownstreamOwned` |
| `DeferredReplies` | `EarlyClose` (closes the registry at the cleanup deadline) | `NoEarlyClose` |
| `DeferredReplies` | `EarlyStopped` (rank 11 before 5e: a token dropped while exiting or unwound answers `Stopped` at once) | `StoppedAtDeath` |
| `DeferredReplies` | `NoReplyFence` (a stored token's reply is admitted after the death point) | `NoReplyAfterDeath` |
| `DeferredReplies` | `CallbackBeforeDischarge` (an answer's wake-up runs before the last answer of the death batch) | `WokenAfterDischarge` |
| `DeferredJobs` | `NoInstanceCheck` | `NoLateResult` |
| `DeferredJobs` | `NoCancelFence` | `NoStaleAfterCancel` |
| `DeferredJobs` | `DoubleTerminal` | `AtMostOneTerminal` |
| `DeferredJobs` | `ReleaseOnTimeout` | `PermitsBounded` |
| `DeferredJobs` | `KeyNoCancel` | `OneLivePerKey` |
| `DeferredJobs` | `ReplacePendingOnly` (the round-2 trace: Start 1, Finish 1, Start 2, dispatch of job 1's terminal message) | `NoStaleKeyed` |
| `DeferredJobs` (`MCDeferredJobsEmits`) | `NoCancelFence` | `NoStaleAfterCancel` |
| `DeferredJobs` (`MCDeferredJobsEmits`) | `EmitAfterTerminal` | `TerminalLast` |
| `DeferredJobs` (`MCDeferredJobsEmits`) | `NoEmitCredit` | `EmitCreditsBounded` |
| `DeferredJobs` (`MCDeferredJobsHeld`) | `EarlyCredit` (a held result returns its credit when the work ends) | `HeldCounted` |
| `DeferredJobs` (`MCDeferredJobsHeld`) | `HeldDeadlineLost` (the deadline cannot take a held result) | `DeadlineArmedWhileHeld` |
| `DeferredJobs` (`MCDeferredJobsHeld`) | `ExitKeepsHeld` (owner exit cancels running work only, not held results or scheduled emits) | `ExitCancelsJobs` |
| `DeferredJobs` | `NoAdmissionFence` (a job admitted after the exit cancelled the job set) | `ExitCancelsJobs` |

Measured 2026-11-10 (TLC 2.19, fede; the two `MCDeferredReplies` rows on
2026-11-11, after the exit-pending change):

| Configuration | Distinct states | Generated | Depth | Time |
|---|---|---|---|---|
| `MCDeferredReplies` (fast: 2 requests, `BoundA` 2, `BoundB` 1, clock 2; safety and liveness; death split into D and the answer batch, 2026-11-11) | 4,864,810 | 25,029,343 | 34 | 4 min 22 s, 4 workers (fede shared) |
| `MCDeferredRepliesDeep` (clock 3; safety) | 11,016,668 | 58,096,711 | 34 | 55 s, 4 workers |
| `MCDeferredJobs` (fast: 3 jobs, 2 keyed, 2 permits, 2 instances, no emits; safety and liveness) | 229,070 | 487,283 | 17 | 5 s, 8 workers |
| `MCDeferredJobsEmits` (fast: 2 keyed jobs, 1 instance, 2 emits each, 1 credit; safety and liveness) | 24,842 | 50,944 | 18 | 1 s, 2 workers |
| `MCDeferredJobsHeld` (fast: 3 unkeyed jobs, 2 permits, 1 instance, 1 emit or scheduled emit each, 1 credit; safety and liveness) | 1,946,047 | 6,230,609 | 22 | 1 min 30 s, 4 workers (with `ExitCancelsJobs`, 2026-11-11) |
| `MCDeferredJobsDeep` (3 jobs, 2 instances, 1 emit or scheduled emit each; safety) | 7,980,942 | 19,676,218 | 23 | 82 s, 2 workers |

All 24 named controls find their counterexample, each in a few seconds.

### Job trace validation

With `lifecycle-trace`, for owners a test opts in with
`lifecycle_trace::watch_deferred(id)`, the job core (`concurrency/src/reply/jobs.rs`) records
one `DeferredJobs.tla` action per owner instance: `Start` (keyed, with a job
deadline), `Emit`, `Finish` (whether it enqueued the terminal message),
`JobDeadline`, `Cancel`, `Exit` and `Dispatch` (kind and fence verdict). Each
is recorded in the trace section together with its effect (the flag change
under the job lock, the enqueue, the fence read), so the log is a
linearization. `concurrency/tests/job_trace.rs` records seven scenarios (plain
result, a finished keyed job replaced and fenced, emits with one credit,
handle cancel, job deadline with the work still running, owner exit with a
running job, a blocking job emitting); `trace_check.py` checks them (schema
`jobs-v1`) against `TraceJobs.tla` with one owner instance and one key. The
permit bound is not checked (`JobBound = J`): the code returns a job's credit
just before `Finish`, and rejected admissions are not model actions.

```bash
cargo test -p spawned-concurrency --features lifecycle-trace --test job_trace
python3 verification/tla/trace_check.py target/lifecycle-traces
```

`negative_checks.py` requires six edited job traces to be rejected: the
replaced job's terminal message delivered, an emit after the terminal
message, a second emit with its credit in flight, a second terminal message
after the deadline's, a quiet finish without a cancel, and a dispatch after
the owner's exit.

### Reply trace validation

The reply transport records `DeferredReplies.tla` actions per owner
instance: `Send` (slot created), `Dispatch` (its turn starts), `Discharge`
(the slot's one `OPEN` -> terminal transition, recorded in the trace section
together with the CAS in `Slot::cas`; a reply counts at `WRITING`), `Exit`
(after the registry commits the turn's drops) and `Death` (the closes that
death and mailbox disposal perform are that one model step and are not
logged). `TraceReplies.tla` takes the turn-internal steps that leave no trace
silently (a `Defer` that takes a credit, `Store`, `DropInTurn`, a turn end
that answers nothing, a drop while exiting that makes the slot exit pending,
the clock); a `Discharge` must be the model action that
discharges exactly that request with that kind. Linked jobs and the
downstream actor are not traced (their notifications and job ends are not
logged). `concurrency/tests/reply_trace.rs` records nine scenarios (reply in
the turn, stored then answered or dropped from a later non-request turn, an
unused obligation, `Overloaded` at the bound, deadline expiry by the
service, the caller giving up, death with a stored token, a panicking turn);
`negative_checks.py` requires five edited traces to be rejected (`Overloaded`
below the bound, a reply after death, a second discharge, a drop before the
turn, and a `Stopped` discharge of the panicking turn's token before its
`Exit`, the rank 11 behaviour before 5e).
