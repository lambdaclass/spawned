# v0.7 API and generated-policy gates

Candidate based on `f407d7ba9cd515c55f033e19c19ba740c0f095f5`.
Baseline: released v0.6.0, `4a648d4da7da520ce3aa8cbd4fdc068246486b21`.
The [evidence](../../verification/evidence/v07-release-gates/) retains logs,
commands, source hashes and AGY's review. These checks do not publish a release.

## Compatibility finding and correction

The initial cargo-semver-checks scan rejected the added `RegistryError::OwnerClosed`
variant: v0.6 callers can exhaustively match the original single-variant enum.
The global registry now retains that original enum. Only the unreleased managed
`Context::register_name` API uses the new non-exhaustive
`ManagedRegistrationError`, wrapping collisions and distinguishing closed owners.
No admission, cleanup or registration-lifetime behavior changed. A deliberately
exhaustive match and managed collision/closed-owner assertions retain regressions.

Both libraries now pass the stricter patch-compatibility policy against v0.6.0:
229 checks pass and 31 skip per library. This default-feature scan does not prove
behavioral or macro compatibility. Existing trybuild and runtime tests remain.

## Generated policy scripts and build gates

The new `supervisor_policies` suite uses a generation/retirement oracle separate
from the implementation's policy helpers. Generated scripts cover up to four
children, all strategies and restart modes, normal stops, shutdowns, panics and
early tree closure. Ready boundaries and request fences identify the instance
actually serving work. A fixed case verifies retired Transient/Temporary slots
stay retired during later recoveries. It is not a deterministic scheduler.

| Check on fede | Result |
|---|---|
| Generated policies, default 32 scripts | Pass |
| Generated policies, 512 scripts, seed 20261109 | Pass with metrics off and on |
| Full workspace tests | Pass |
| Native-only threads/metrics tests | Pass |
| Strict default documentation | Pass |
| Strict workspace/metrics and native Clippy | Pass |
| Each library feature plus default/no-default/all, no dev dependencies | 15 checks pass |
| Default API comparison with released v0.6.0 | Both libraries pass |

Runtime tests used Rust 1.93.0; strict Clippy used 1.98.1. The feature matrix
uses Rust 1.93.0 and cargo-hack 0.6.46. cargo-semver-checks 0.51.0 is pinned
together with Rust 1.99.1 through Nix, matching its rustdoc JSON producer.
Jobs shared CPUs 2–5 with one build job, nice 10, a sampled 2 GiB RSS guard and
30-minute outer timeout. Test/Clippy peak sampled RSS was 1,213,068 KiB.

CI now requires feature/API gates, runs policy scripts in normal tests and under
ThreadSanitizer (eight generated inputs), and runs 1,000 scripts in the extended
workflow. Disposable source copies keep cargo-hack's manifest/lockfile rewrites
away from the real checkout; before/after manifest hashes are retained.

AGY returned PASS. Its initial low-severity note missed the extended workflow's
existing job-level `PROPTEST_CASES=1000`; the follow-up includes that context.
No claim of exhaustive schedule exploration or absence of all security bugs is
made by this review.

## Remaining release gate

A running Ethlambda canary or actor-backed replay with fault injection is still
required. The prior isolated client compile check is not that gate. The target
and deployment configuration are pending; no production service, release tag or
published version changed. See the [migration guide](../MIGRATION.md#validation-before-rollout).
