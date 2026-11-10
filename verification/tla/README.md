# TLA+ models

Formal models of Spawned lifecycle contracts, checked with TLC. See the
[verification strategy](../../legacy/docs/research/verification-strategy.md) for how
this fits with Loom, Shuttle, Miri, ThreadSanitizer and mutation testing.

| Model | Covers |
|---|---|
| `Supervision.tla` | The implemented OneForOne supervisor: `run_supervisor` and `run_child` in `concurrency/src/supervision.rs`, restart modes, rolling restart budget, ordered startup, reverse shutdown, shutdown deadlines, cleanup-stuck failures. |
| `MCSupervision.tla/.cfg` | Fast instance (CI, every PR): two children (Permanent, Transient), safety, action properties and liveness. |
| `MCSupervisionDeep.tla/.cfg` | Deep instance (weekly, `--deep`): three children, one per restart mode, safety and action properties. |
| `TraceSupervision.tla` | Trace validation: replays a lifecycle trace recorded from real Rust runs as a behavior of `Supervision.tla`. |
| `Cohorts.tla`, `MCCohorts*.tla/.cfg` | OneForAll/RestForOne range selection, reverse cleanup, whole-cohort join barrier, ordered resume, stale notifications, superseding rounds and retired slots. Three-child mixed-mode and two-round configurations. |
| `Coordinator.tla`, `MCCoordinator.tla/.cfg` | System shutdown coordinator: ordered stop of admitted roots, panic fallback that signals all remaining roots, records failure and retains runtime ownership until joins; ordered-shutdown invariants, cooperative cleanup liveness and four negative mutations. |
| `NestedSupervision.tla`, `MCNested.tla/.cfg` | Dynamic subtree ownership and shutdown delivery; parent scope completion waits for descendant completion. Bounded three-node/two-generation model with fair cleanup. |
| `TraceCohorts.tla`, `TraceNested.tla` | Policy and ownership projections of real cohort/nested Rust executions. |
| `trace_check.py`, `negative_checks.py` | Replay recorded traces; deliberately wrong selections, premature resume/join and broken propagation must be rejected or produce counterexamples. |
| `SupervisionInd.tla`, `CohortsInd.tla`, `NestedSupervisionInd.tla`, `CoordinatorInd.tla`, `MC*Ind.tla`, `apalache.sh` | Inductive invariants (`IndInv`) checked with Apalache; see [Inductive invariants](#inductive-invariants-apalache). |

## Running

TLC is a Java program. The repository flake provides it (with its own JDK), so
nothing needs to be installed globally:

```bash
nix run .#check-tla            # model-check every MC*.cfg
nix develop -c tlc MCSupervision -workers auto   # ad hoc, from verification/tla
```

## Trace validation

Model checking explores the configured bounded abstraction; trace validation checks that recorded implementation executions conform to it. Neither proves all Rust executions correct. With the `lifecycle-trace` feature
(off by default, compiled out otherwise) `run_supervisor` and `run_child`
record one event per model action, with its parameters, per supervisor
(`concurrency/src/lifecycle_trace.rs`). `concurrency/tests/supervision_trace.rs`
runs real actors through the scenarios below and writes each trace as JSON;
TLC then checks that each trace is a behavior of `Supervision.tla`.

```bash
cargo test -p spawned-concurrency --features lifecycle-trace --test supervision_trace --test cohort_trace --test tree_trace --test nested_supervision
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
factory error then success; non-restartable startup failure; restart budget
exhaustion and disabled retries; shutdown during backoff; startup timeout;
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
`SelfExits` now includes `Shutdown`. Hand-edited traces (a `Close` moved
before an `Attempt`, a restart after a Transient normal exit, forward shutdown
order, budget exhaustion with a free retry) are rejected at the edited event.

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
  (startup timeouts restart unless Temporary).
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
state). All checks with negative controls take about 2 minutes on a Linux
server (4 JVM cores); `--size 10 10` for `Supervision` about 45 seconds and
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

Non-vacuity: `apalache.sh` requires probe predicates to be violated (IndInv
admits, for example, five running children at generation > 1000 mid-shutdown),
and `--negative-controls` removes one guard at a time and requires induction
to fail: for `Supervision` the retry limit, ordered start, join wait, restart
table, cleanup, unpublish-on-stop, admission gate on `Attempt` and the
reverse-shutdown wait; for `Cohorts` and `NestedSupervision` the existing
`Fault` mutations (`Selection`, `Cleanup`, `Retirement`, `Closed`, `Join`);
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
`queued_drop_preserves_shutdown` retains a committed Shutdown despite a destructor
failure; escalating it to Panic must reject transient retirement.
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
cancellation). Six model mutations must yield counterexamples (selection,
cleanup, retirement, closed admission, parent join and shutdown propagation).
A separate unsafe-final-event control checks `Safety` before the full-log completion
sentinel. The ordinary `--traces-only` interface still accepts older OneForOne
trace collections without requiring the new negative controls.

## Next model

The base actor lifecycle is not modelled yet: exit reasons, monitors and
demonitor, links with `trap_exit` and `start_linked`, stop and cancellation while
a handler runs, request acceptance or rejection around shutdown, and exit
signals relative to queued messages. Keep it small (three or four actors,
bounded mailboxes and message counts). It should check at least:

- an actor linked to an abnormally exiting, non-trapping peer eventually stops;
- every accepted request eventually gets a reply or `ActorStopped`, never neither;
- no message is dispatched after the run loop has observed stop;
- each monitor's `Down` is delivered exactly once.

Acceptance: reintroducing the PR #182 bug (a request whose mailbox publication
races shutdown is retained without a reply) and the PR #185 bug (newly
received idle work dispatched after stop) into the model must each produce a
counterexample, as the five injected supervision bugs do today. Trace validation
should then cover the lifecycle scenarios the same way it covers supervision.
Source: [verification strategy](../../legacy/docs/research/verification-strategy.md#acceptance).

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
