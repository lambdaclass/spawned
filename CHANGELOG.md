# Changelog

## Unreleased

### Added

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

These capabilities are unreleased and require the next minor release. The
separate dedicated-runtime patch remains documented in the 0.6.1 draft; do not
publish these new public APIs as that patch release.

### Fixed

- Bound each dedicated async actor runtime to one async helper worker instead
  of creating a machine-sized Tokio worker pool per actor. The actor remains
  on its dedicated thread; blocking-helper progress, `block_in_place`, timers
  and I/O remain supported. Blocking operations can still create extra threads.


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
