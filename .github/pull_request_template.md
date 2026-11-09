## What and why

<!-- What changes, and the problem it solves. Link the issue or review finding. -->

## How it was verified

<!-- Commands run, suites added or extended, anything only CI can check. -->

## Checklist

- [ ] Every bug fix includes a test that fails without the fix (say which test, and confirm you ran it against the unfixed code). See docs/TESTING.md, "Reproducing-test rule".
- [ ] Lifecycle, reply or ownership changes follow the verification rule in ROADMAP.md (TLA+ model, trace replay, negative checks), or explain why not.
- [ ] `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --workspace` pass.
- [ ] `CHANGELOG.md` (Unreleased) and the relevant docs are updated.
