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

Earlier designs (the v0.5 API redesign alternatives, the framework comparison
and superseded roadmap plans) are archived in
[`legacy/docs/design`](../../legacy/docs/design).
