# Actor scenario tests

Run `cargo test -p spawned-concurrency --test actor_scenarios`.

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

Thread actor completion now uses a watch notification, so async exit waits can
be cancelled without leaving a blocking-pool worker waiting for target exit.
A regression with a one-worker blocking pool checks that property.

## Compatibility and review follow-up

The execution model is unchanged: protocols separate application logic from
message dispatch, each actor handles one message at a time, and both async and
synchronous thread execution remain supported. No restart, queue-capacity, or
message-delivery policies are added here.

Early-stop cleanup panics now produce `ExitReason::Panic` instead of `Normal`;
this intentional observable correction matches the ordinary shutdown path.
Thread completion uses a runtime-independent watch primitive for cancellable
subscriptions, while synchronous waits use a thread waker without entering an
async executor. Blocking waits from inside a futures executor have a regression.

Monitor tables are explicitly closed on owner exit. Cancellation and delivery
claim the monitor entry under the same lock. The documented best-effort rule
still allows a notification already claimed for delivery before demonitor.
Cleanup tests track the waiter's own ChildHandle allocation instead of counting
unrelated internal table owners. Test dependencies remain dev-only; proptest's
unused default features are disabled. The lockfile is generated by Cargo.
