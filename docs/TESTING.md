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
exhaustive scheduler or coverage-guided fuzzer. It does not yet cover demonitor
races, timer/listener cleanup, bounded mailboxes, or supervisor restart policies.
Add focused Loom models for monitor/completion synchronization separately.
Do not interpret a passing campaign as proof of race freedom.
