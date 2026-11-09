# Agy review: Runnable task primitive

Agy reviewed the inline private native-runnable executor for testing on
6 November 2026. First verdict: APPROVE FOR TESTING. That review missed the
async-task destructor-abort compatibility issue found by direct inspection of
pinned v4.7.1 raw.rs. The initial review is not evidence of destructor safety.

The revised source adds PanicSafeFuture, keeps the inner actor future boxed,
catches poll/disposal separately and tests destructor panic recovery against
custom native-root. A second inline review returned APPROVE FOR TESTING with
no blockers, covering unique Runnable ownership, metadata/TLS routing, detach,
retained wakers, shared-I/O/root cooperative budgeting and lease teardown.

These verdicts precede compilation and server testing. Agy's blanket statement
that existing policies are unchanged refers to scheduling semantics: the shared
Work enum changes queue layout/code generation, so new timings need common-source
controls. Its unwind claims apply to the supplied wrapper's separate boundaries,
not arbitrary double panics in Rust code or allocation failures. Agy did not
execute tests or benchmark the design.

Raw review transcripts retained by the controller:
/private/tmp/spawned-runnable-inline-agy.log
/private/tmp/spawned-runnable-drop-agy.log
