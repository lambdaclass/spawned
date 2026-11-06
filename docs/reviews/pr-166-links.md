# Review and hardening of PR166

The original head `0f48d9d5` was reviewed and ported onto main `4e6b4270`, preserving
current monitor ownership, completion channels, message draining and stop-race
fixes. It was not merged unchanged.

Two deterministic regressions fail on the ported original implementation at
`e18ade26` and pass after hardening:

- A real `Context::link` call made after target link-table drain but before its
  completion publication receives no exit signal.
- Dropping the owning runtime before a linked actor is first polled leaves its
  peer running instead of propagating the abnormal exit.

The link table now stores its terminal reason under the registration mutex.
Registration/unlink mutate both ends under a canonical lock order; callbacks
and removed entries are handled after releasing locks. Reverse table references
are weak. Closure is idempotent, self-links are no-ops, and linking a closed peer
creates no retained relation. A new attempt to link a closed peer reports that
peer's exit; existing live links remain idempotent. Unlink cannot retract an exit
already in flight.

Cleanup is constructed before dispatch. A shared lifecycle guard cancels helpers,
releases outgoing monitors, closes links and publishes completion in that order.
Its startup payload is captured as one value, with payload fields before the
cleanup field, so never-polled actor destruction does not depend on disjoint
closure-capture drop order. A regression blocks the actor destructor and verifies
that completion remains unpublished until destruction finishes.

`start_linked` registers before child dispatch/`started()`, with closed-parent
signal handling under the same lifecycle mutex. Local stop commits its normal
cause before cancellation; a later linked failure cannot relabel that stop while
`stopped()` runs. A callback panic is still reported as a panic. `Kill` bypasses
trapping, while handler cancellation remains cooperative.

The normal `#[actor]` interface now supports `#[exit_received]` for both execution
modes. This keeps lifecycle wiring in the framework. It does not implement an
automatic restart policy or make old references point to a replacement actor.

Validation on fede: source `8bf5702` passed 144 unit tests, both public macro tests
and the all-feature workspace suite. The coverage includes pre/post-poll runtime
shutdown, outgoing-monitor waiter reclamation, late monitor registration, normal
stop versus peer failure, startup link visibility in both modes, untrappable
runtime abort, mixed-mode cancellation and a 4096-actor polled link cycle. Clippy's
only failure was a test helper after the test module; it was moved before the
module without changing runtime behavior, then Clippy was rerun on final source.

Independent supplied-code review approved the hardened primitive; final lifecycle
publication changes also received a separate focused approval. Review is not execution
reproduction or a proof of every schedule. Initial reviewer suggestions about
automatic shutdown when all ActorRefs are dropped and nonexistent Shutdown/Aborted
exit variants were rejected: current actors retain their context sender and use
explicit cooperative stop; runtime abort is reported as Kill. No such philosophy
or public delivery change was adopted. All ordinary typed messages and replies
remain addressed and unbounded.


Evidence is archived in [pr-166-evidence](pr-166-evidence/). The bundle preserves
review snapshots, including the intentionally failing regression baseline. It is
an audit artifact, not a release source. With the main repository available:

```sh
git fetch docs/reviews/pr-166-evidence/review-snapshots.bundle HEAD
git worktree add --detach /tmp/spawned-negative e18ade2690de9ec283ae9cb60e51308310655f94
cd /tmp/spawned-negative
cargo test --locked -p spawned-concurrency --all-features --lib link_regression_tests -- --test-threads=1
# Two failures are expected on this baseline; see negative-tests.log.
```

The runtime and tests in the merged source match the passing `8bf5702` snapshot,
except for moving a test helper before its test module to satisfy Clippy. The
final Clippy log is from `19893ba5`, which includes that placement fix and docs.
