## What and why

<!-- What changes, and the problem it solves. Link the issue or review finding. -->

## How it was verified

<!-- Commands run, suites added or extended, anything only CI can check. -->

## Checklist

- [ ] Every bug fix includes a test that fails without the fix (say which test, and confirm you ran it against the unfixed code). See docs/TESTING.md, "Reproducing-test rule".
- [ ] Lifecycle, reply or ownership changes follow the verification rule in ROADMAP.md (TLA+ model, trace replay, negative checks), or explain why not.
- [ ] A change to concurrency, the death point, replies, jobs, links/monitors or supervision has had an independent review (someone, or an agent, given the code but not the author's conclusions) before merge; link it. Recent independent reviews found real bugs the author's tests missed (#282, #291).
- [ ] New or changed TLA+ models are tied to the code by trace validation (`model-sync` enforces it; a TLC-only negative control is not enough).
- [ ] No cost on the default path: `perf-gate` passes against the merge base and the last release.
- [ ] `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --workspace` pass.
- [ ] `CHANGELOG.md` (Unreleased) and the relevant docs are updated.
