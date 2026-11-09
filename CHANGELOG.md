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

These capabilities are unreleased and require the next minor release; do not
publish these new public APIs as a patch release. The dedicated-runtime fix
below was drafted as a separate 0.6.1 patch (draft notes archived under
`legacy/docs/releases/`); if it ships separately, prepare a patch-only lineage
rather than tagging current main.

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
  now plans v0.7 with a per-method request timeout, a migration guide and an
  example for every way of using the library, followed by deferred replies.
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
