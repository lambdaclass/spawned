# Ethlambda v0.7 canary fixes — 2026-11-09

Code baseline: Spawned 7499d51f; rebased over docs-only 56498d0d.
Feedback from Ethlambda 047860fa.
The source hash manifest identifies the reviewed patch; AGY returned SUCCESS/PASS.

Validation on the Linux research server (four CPUs, one build job, sampled 2 GiB
RSS guard):

- Rust 1.93 workspace tests with metrics, diagnostics and lifecycle-trace: 286 passed.
- Default and native-only explicit-exit/canary regressions: passed.
- All 12 required native integration test names present; isolated synchronous
  normal/build dependency graph contains no Tokio.
- All 30 real lifecycle traces accepted by the existing TLA+ models.
- Negative controls rejected the erased abnormal Context exit, premature cohort
  resume, wrong selection, early parent join, false propagation and final action;
  existing model mutations produced their required counterexamples.
- Strict Clippy (1.98.1) and Rustdoc (1.93): passed.
- Prometheus Python client 0.21.1 parsed all exported histogram/max gauge families;
  removing the max declarations failed the regression check.
- The shutdown_diagnostics example ran and identified its held handler before
  cleanup was released and joined.

The old Prometheus output parses maxima as unknown/untyped families with this
parser. No general scrape outage is claimed. Active-turn diagnostics are opt-in;
this patch does not benchmark their overhead. Standalone helper ownership and a
shutdown dependency DAG remain unimplemented and are documented in the roadmap.
No canary node was deployed, and this evidence is not application performance data.
