# Changelog

## Unreleased

- Release monitor waiters when a monitor is cancelled or its owner exits.
  Monitor registration after owner shutdown starts returns an inert reference
  and creates no waiter or notification, including calls from `stopped()`.
- Preserve a cleanup panic as `ExitReason::Panic` when shutdown starts during
  `started()`, matching the ordinary shutdown path.
- Stop thread listeners before invoking another iterator `next()` after
  cancellation. An already-running blocking iterator call remains cooperative.
- Make async waits for thread-actor exit cancellable without retaining a
  blocking-pool worker. Synchronous waits still block the caller and work
  without an async runtime, including inside a futures executor.
- Add actor scenario, cleanup, concurrent producer, macro diagnostic, Loom
  cancellation, and sustained-churn tests.
