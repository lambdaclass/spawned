# v0.7.1 fix validation

Work in progress; this record does not claim release approval.

## Debug future-layout investigation

**Superseded in part:** this probe did not measure owned protocol calls or nested
RPC dispatch. The [round-two investigation](../v071-review-round2/README.md) found
that the added handle word does enlarge those futures, despite unchanged borrowed
requests. It records the compact sender correction and the remaining client gate.

`future-layout.csv` records Linux x86_64 Rust 1.93.0 unoptimized sizes with dev
debuginfo level 1. Each version used an isolated source copy; sources/manifests
were refreshed before building, avoiding shared-target stale-artifact reuse.
`layout-probe.rs` measures public handles, Response and the actual request future.
For the two root columns, temporary instrumentation printed
`size_of_val(&inner_future)` immediately before `match backend` in
`ActorRef::spawn_inner` (or `spawn` in v0.5.0). The probe starts an empty actor and
an actor owning a 16,384-byte array, requests once, and stops/joins both.

The candidate has the same request-future and Response sizes as v0.7.0 under
matching metrics settings: 184/24 bytes without metrics and 208/48 with metrics.
The receiver guard adds one Arc to handles (+8 bytes); teardown bookkeeping adds
64 bytes to the measured root future. Neither observation reproduces the reported
Ethrex `map_http_requests` 33,728 to 35,168-byte growth. That application figure
requires the same client revision, compiler, target and feature graph to assign a
cause. Relative to v0.5.0, lifecycle features and optional instrumentation do
increase these small public/layout examples, which is a hypothesis to check in
the actual client, not proof of that warning's cause.

A large inline actor state already inflated its root future before this patch.
Moving state behind a Box could reduce that startup frame, at a cold allocation
per actor. No such optimization was shipped without a representative client
profile, and this layout probe makes no throughput or memory-use claim.

## Draft PR regression coverage

- `exit_signal_matrix`: 2 relations (link/monitor) × 2 trap settings × 3 reasons
  (Kill/Panic/Shutdown), in both tasks and threads modes: 24 combinations.
- Private policy tests cover N permitted restarts and N+1 escalation during
  `started()` failure (N=0,1,3), sibling reservation exclusion/refund, sliding
  window expiration while retaining newer grants, and failed startup state.
- Nested supervision exhausts the leaf budget, permits one subtree replacement,
  then escalates the next leaf failure through the parent's budget.
- A deterministic private readiness gate reproduces before=2, supervised=1,
  after=2 on thread placement. Closing admission cannot steal an earlier
  supervisor's stop turn. The fixed integration case covers group placements.
- The metrics-enabled thread-placement property ran exactly 10,000 successful
  cases with seed 20261109; the runner asserts and prints the executed count.
- `spawned-test` tests direct Kill/Shutdown of trapping targets while their
  trapping linked peers remain alive, in both execution modes.
- `cargo publish -p spawned-test --dry-run` passed including package verification
  with CLI `patch.crates-io` entries pointing to the local 0.7.1 sibling sources.
  An unpatched run cannot resolve unpublished sibling version 0.7.1 yet. No crate
  was uploaded. Publishing remains a separate commit and was not executed.

`traces/` contains 32 freshly recorded lifecycle tapes, with checksums and event
counts in `trace-manifest.json`. The commands used were:

```sh
cargo test --locked -p spawned-concurrency --features lifecycle-trace \
  --test supervision_trace --test cohort_trace --test tree_trace --test nested_supervision
PROPTEST_CASES=10000 PROPTEST_RNG_SEED=20261109 cargo test --locked \
  -p spawned-concurrency --features metrics --test cross_backend \
  ordered_system_shutdown_matches_thread_placement -- --exact --nocapture
```

The user owns the final replay, sanitizer/mutation testing, client builds and
canary during review. Earlier bounded runs accepted 32 lifecycle traces and
found no supervisor-model violation across 4,763,429 distinct states; that is not
claimed as replay approval of this freshly recorded bundle.

#204 remains a separate PR for the user's rebase. Its two formerly ignored
coordinator-panic regressions were enabled and passed on an integration checkout.
`enable-pr204-regressions.patch` supplies the test-only follow-up for that rebase;
the failpoint framework itself is deliberately excluded from this fix branch.

## Claude Code review disposition

Initial static reviews requested changes. The release-version/publish changes
are isolated in the second commit, and all matrix/evidence files are tracked.
The reviewer identified a retry grant charged before a superseded generation
was admitted. The charge now commits under the same gate/slot locks as generation
admission; a deterministic RestForOne upstream-supersession regression passes
with exactly three grants for three replacement generations. Existing v0.7
per-child cohort budget accounting is preserved, explicitly documented and
covered by a six-child/five-grant boundary test. The private Rust coordinator
fault tests remain in the library even without #204; the test-only review's
claim that removing #204 removed those tests was not applicable to those units.
A focused Claude follow-up reviews these corrections; none of these records is
permission to merge or release the draft.

The focused Claude follow-up returned **PASS for B1/B2** (documented existing
cohort accounting and atomic grant/admission). Final trace verification remains
with the user as requested; the follow-up is not a global release approval.
