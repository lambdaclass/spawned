# Design documents

Active design contracts. Usage is documented in the [guide](../GUIDE.md).

- **[semantics.md](semantics.md)**: the semantics contract. Every guarantee
  Spawned gives, each linked to the TLA+ invariant, test or example that checks
  it, plus what is not guaranteed. New designs are reviewed against it.
- **[execution-groups-supervision.md](execution-groups-supervision.md)**: the
  contract for `ActorSystem`, execution groups and OneForOne supervision
  (available in v0.7.0). The TLA+ model in
  [`verification/tla`](../../verification/tla/README.md) follows its restart table.
- **[deferred-replies.md](deferred-replies.md)** (rank 11, design in
  progress, not implemented): reply slots and send-time deadlines, deferred
  tokens with an owner registry, bounded deferral, pipe-to-self jobs with
  `ActorId` and cancellation fencing, with the rank 11a decisions. Models:
  `verification/tla/DeferredReplies.tla` and `DeferredJobs.tla`. Client
  rewrites: [deferred-replies-client-mapping.md](deferred-replies-client-mapping.md).
- **[otp-exit-signals.md](otp-exit-signals.md)**: proposed v0.8 contract (not
  implemented) for exit signals, `Killed`/`NoProc`, supervisor startup and
  runtime-dropped actors as in OTP (ROADMAP ranks 5e, 5f, 5g), with the base
  actor lifecycle model `Lifecycle.tla`.
- **[dynamic-supervisor.md](dynamic-supervisor.md)** (rank 12a, design in
  review, not implemented): OTP's `simple_one_for_one` / Elixir's
  `DynamicSupervisor`: runtime `start_child` from a typed template,
  `terminate_child`, `max_children` counting restarting children, a
  supervisor-wide budget and concurrent shutdown. Model:
  `verification/tla/DynamicSupervisor.tla`. Client mapping (ethrex's
  per-peer connections): [dynamic-supervisor-client-mapping.md](dynamic-supervisor-client-mapping.md).

Earlier designs (the v0.5 API redesign alternatives, the framework comparison
and superseded roadmap plans) are archived in
[`legacy/docs/design`](https://github.com/lambdaclass/spawned/tree/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/docs/design).
