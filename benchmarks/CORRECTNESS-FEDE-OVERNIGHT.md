# Correctness findings from the overnight study

Timing success did not establish scheduler correctness. Targeted models,
deterministic regressions and review found four distinct defects. Historical
measurements remain available with their source boundaries.

| Finding | Evidence | Correction and scope |
|---|---|---|
| Coalesced wake did not publish prior writes | A stronger Loom model fails on3478e5f; queue-count-only models had passed | An AcqRel RMW is required even when wake bits do not change. Only old IDLE schedules a new queue entry. Applies to the custom native Job gate, not the default actor scheduler. |
| Global queue idle shutdown could lose notification | Review identified closed-store/notify racing the wait transition; no failing pre-fix execution is claimed | Change closed under the same queue mutex used by Condvar wait, then notify. Serialize concurrent teardown; stress tests cover terminal joins. |
| An idle stopped actor could dispatch newly received work | Deterministic async Pending/resume and synchronous receive-barrier tests fail before the fix and pass after | Check cooperative cancellation before each async receive poll and after synchronous receive; preserve cleanup ordering. Merged in[PR185](https://github.com/lambdaclass/spawned/pull/185). Current handlers still finish; cancellation is non-atomic with dispatch. |
| Oldest local deque work could starve | Strengthened queue-policy regression fails on95b4ca6 with hot local continuations plus sustained outside injection; corrected e633e59 passes | Alternate oldest-local/injection priority; add owner-pop fallback after bounded self-steal Retry. Final2cec0a6 also makes callback hints fall back safely and adds concurrent conservation tests. No hard wall-time fairness guarantee follows. |

The dequeue regression uses unpolled marker tickets to isolate queue policy.
Full actor, timer/TCP/helper, panic, stale-waker, cancellation and shutdown tests
are separate executor gates. Concurrent stress and finite Loom models test their
particular schedules/state machines, not every possible complete runtime state.

## Reproduce the negative controls

Source bundles in the final results archive retain commits that squash merges
would otherwise make inconvenient to find. In a clone containing their listed
base commits, run git bundle verify, then fetch the relevant bundle reference
and create a detached worktree. Use the recorded Rust1.93 toolchain and feature
settings; the commands below are examples inside those source worktrees.

For publication, use the controls-checked bundle. The added stronger model at
3478e5f fails; the corrected5c0eca3 reference passes the four Gate models:

    RUSTFLAGS="--cfg spawned_loom" cargo test --locked -p spawned-rt \
      --features backend-experiments --lib dispatch_gate -- --test-threads=1

For the production idle-stop proofs, the idle-stop-proof bundle includes
a267308 (async regression before correction),0998deb (thread regression before
its correction), and the final c9b9ccc implementation/tests:

    cargo test --locked -p spawned-concurrency --lib idle_stop_regression \
      -- --test-threads=1

The synchronous before-fix revision already contains the async correction; do
not claim both tests fail on every historical checkout.

For deque fairness, the final fairness-source bundle includes the old-policy
negative control95b4ca6 and the final corrected source. The preliminary4b56238
probe omitted hot local continuations and passed; it is a test-design limitation,
not evidence against the later reproduction.

    cargo test --locked -p spawned-rt --features backend-experiments --lib \
      oldest_local_progress_under_sustained_injection -- --test-threads=1

Also run the complete feature-enabled RT and selected actor suites. Source and
binary hashes, gate commands/output, intended failures and successful checks are
archived. A review that could not execute tests is not represented as reproduction.

## Harness and probe failures are separate

The first V3 resource attempt was invalidated by our paused Python pipe reader:
a worker blocked writing expected diagnostic output. A restart inherited nice19
from a nested priority adjustment and was cancelled before a case completed.
Both remain diagnostic artifacts; the valid rerun is soaks-3 at nice10 with a
continuously running reader. Neither is a library resource failure.

An early eight-core smoke rejected more than256 logical producers at a legacy
cap before actor startup. A second preflight exposed missing owned-runtime/test
selectors, creating idle caller workers and skipping two integration guards.
Both were corrected and revalidated before any scored V4 run.

A combined poll/destructor-panic probe aborted the existing Root control through
ordinary Rust double panic before reaching Runnable. Later tests distinguish
single-destructor isolation and sequential wrapper unwind boundaries. No backend
promises recovery from arbitrary double panic or process abort.
