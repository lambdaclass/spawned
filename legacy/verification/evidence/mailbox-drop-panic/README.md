# Queued destructor panic regression — 2026-11-09

Baseline: main 342b707b. Reports/repros from PR #206 at cd35299b248879236d9ff496367207695a474bdb.
The source manifest identifies this patch; both AGY review passes returned PASS.

Before the fix, the unchanged single-drop repro timed out with
`QueuedMessageDrop Down and Exit, reason Kill; recorded []`. The two-drop
subprocess failed with Unix wait status 6 (SIGABRT). Both repro tests returned
failure. After the fix, both original ignored repros and their subprocess helper
passed unchanged.

Final Linux research-server validation (four CPUs, one build job, sampled 2 GiB
RSS guard):

- Rust 1.93 workspace tests with metrics, diagnostics and lifecycle-trace: 290 passed.
- Enabled subprocess regression: 30 cases, five task placements, 1/2/16 queued
  messages and ordinary/prior-handler-panic shutdown. It verifies one Exit and
  one Down, no extra notifications, live trapping observer, discarded handlers,
  all payload drops, reply-channel release and quiescent metrics.
- Same focused tests with default features: passed.
- Deterministic managed/legacy pre-first-poll startup tests: passed.
- All 31 real lifecycle traces accepted. Erasing the drain Panic or Context Kill
  was rejected, as were all existing trace and model mutation controls.
- Strict Clippy 1.98.1, all targets; final subprocess test rechecked: passed.
- PR #206's callback/supervision/factory suite, using an ephemeral copy with the
  ninth queued-drop fault included in TASK_FAULTS: four suite tests passed. That
  test file is not merged by this patch; the permanent regressions are separate.

Only ordinary task stop/drain is changed. Runtime-abort fallback Kill and managed
startup admission semantics are retained. Unwinding cannot contain panic=abort,
abort in a panic hook, or multiple panicking field destructors in one unwind.
No new library release, client checkout edit or deployment is performed here.
