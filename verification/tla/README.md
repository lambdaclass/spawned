# TLA+ models

Formal models of Spawned lifecycle contracts, checked with TLC. See the
[verification strategy](../../docs/research/verification-strategy.md) for how
this fits with Loom, Shuttle, Miri, ThreadSanitizer and mutation testing.

| Model | Covers |
|---|---|
| `Supervision.tla` | The implemented OneForOne supervisor: `run_supervisor` and `run_child` in `concurrency/src/supervision.rs`, restart modes, rolling restart budget, ordered startup, reverse shutdown, shutdown deadlines, cleanup-stuck failures. |
| `MCSupervision.tla/.cfg` | Fast instance (CI, every PR): two children (Permanent, Transient), safety, action properties and liveness. |
| `MCSupervisionDeep.tla/.cfg` | Deep instance (weekly, `--deep`): three children, one per restart mode, safety and action properties. |
| `TraceSupervision.tla` | Trace validation: replays a lifecycle trace recorded from real Rust runs as a behavior of `Supervision.tla`. |
| `trace_check.py` | Turns recorded JSON traces into `TraceSupervision` instances and runs TLC on each. |

## Running

TLC is a Java program. The repository flake provides it (with its own JDK), so
nothing needs to be installed globally:

```bash
nix run .#check-tla            # model-check every MC*.cfg
nix develop -c tlc MCSupervision -workers auto   # ad hoc, from verification/tla
```

## Trace validation

Model checking shows the design is safe; trace validation checks that the
implementation behaves like the model. With the `lifecycle-trace` feature
(off by default, compiled out otherwise) `run_supervisor` and `run_child`
record one event per model action, with its parameters, per supervisor
(`concurrency/src/lifecycle_trace.rs`). `concurrency/tests/supervision_trace.rs`
runs real actors through the scenarios below and writes each trace as JSON;
TLC then checks that each trace is a behavior of `Supervision.tla`.

```bash
cargo test -p spawned-concurrency --features lifecycle-trace --test supervision_trace
nix run .#check-tla -- --traces-only target/lifecycle-traces   # or --traces DIR to also model-check
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
- The rolling window is not observable: `WindowExpire` may stutter before a
  `BackoffDone`, and the logged budget count after `Core::retry` pins it.
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

## Abstractions

Time is nondeterministic: backoff, startup and cleanup deadlines and the
rolling window are actions, not clocks. The admission gate and `core.cancel`
close together (`Core::close`), so they are one variable. Execution-group
closure and placement failure are not modelled yet. Each action corresponds to
a point in the Rust code so that recorded traces can be checked against the
model (see Trace validation above).
