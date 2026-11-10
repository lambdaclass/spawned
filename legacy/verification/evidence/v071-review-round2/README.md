# PR #209 round-two review fixes

Review candidate only. The PR remains draft; no merge, tag or publication is
authorized. The user owns the final Ethrex RPC debug-stack rerun, #204 rebase,
trace checks, sanitizers, mutation testing and canary.

## Future-layout regression

`future-layout.csv` records Linux x86_64, Rust 1.93.0, unoptimized dev profile
with debuginfo level 1. `layout-probe.rs` is the same standalone probe for all
three source copies: fc35abafe7a850aa27f4b511b60e28dc49b4a393,
35bea650de4a56c0f09a98fbcb28b371a1d0db9f, and this compact-sender correction.
The protocol, messages and nested RPC-shaped functions are identical. Metrics
is toggled on spawned-concurrency; the probe enables Tokio's `rt` and `time`
features. Each source copy was refreshed before compilation to avoid stale shared
target artifacts. These are illustrative future sizes, not Ethrex future sizes.

| Bytes (without / with metrics) | fc35abaf | 35bea650 | Compact sender |
|---|---:|---:|---:|
| ActorRef / Context | 80 / 88 | 88 / 96 | 80 / 88 |
| Borrowed request future | 200 / 224 | 200 / 224 | 200 / 224 |
| Owned send future | 88 / 96 | 96 / 104 | 88 / 96 |
| Owned request future | 368 / 408 | 384 / 424 | 368 / 408 |
| Owned protocol-call future | 192 / 232 | 208 / 248 | 192 / 232 |
| Configured protocol-call future | 288 / 336 | 312 / 360 | 288 / 336 |
| Eight-handle, three-stage dispatch future | 3968 / 4384 | 4360 / 4776 | 3968 / 4384 |

The guard's extra Arc enlarged every handle/context and accumulated in enclosing
owned futures. The sender now holds one Arc to shared raw sender/failure metadata;
the receiver retains only failure metadata, so dropping the final sender still
closes the channel. Receiver-owned panic-safe draining remains in place. There is
no new per-message allocation. A unit regression checks final-sender EOF; the
integration layout regression checks owned send/request/protocol/RPC shapes.
As a negative control, adding that same layout test to 35bea650 fails at the
first handle assertion (96 bytes versus 88 with metrics); it passes on the
compact sender both with and without metrics.

To reproduce, create an external binary crate containing `layout-probe.rs` as
`src/main.rs`, depending on the selected checkout's spawned-concurrency and Tokio
with `rt` and `time` features. Run once with default features and
once with the dependency's `metrics` feature enabled. Use the same compiler,
target and profile for all revisions. The probe uses no internal APIs.

Restored sizes do not establish that Ethrex's actual RPC tests fit a 2 MiB debug
thread stack. Both previously failing client tests and the whole RPC group must
be rerun before release.

## Other review corrections

- Readiness commit and startup-future rollback share the admission lock. A fixed
  two-before/one-supervised/two-after test completes or drops startup after ready
  and admission close, holding the last shutdown turn to check no early cancel.
  Another test fails the tree after readiness but before the caller observes
  startup: success stays historical and `failure()` reports the later failure.
- Lifecycle outcome is captured before explicitly dropping actor state. Both
  execution modes retain the primary handler panic through state and queued
  destructor panics; subprocess tests check observer Exit/Down outcomes.
- Link delivery safely disposes callback panic payloads and contains subscriber
  panics. A unit test makes both panic payload destructors panic too.
- Budget test locks have lexical scopes before await. Migration/guide notes cover
  terminal states, pending reservation concurrency and zero-to-cap jitter.

Validation: workspace tests with metrics + diagnostics, synchronous-only library
and mailbox tests, the layout regression without metrics, and strict Clippy for
both feature profiles passed. The two final startup regressions and strengthened
native destructor assertion also passed. Full `spawned-test` publication dry-run
verification passed with CLI `patch.crates-io` entries for the unpublished local
0.7.1 sibling crates; all 15 package files were included, and no upload occurred.
The README/metadata and publish-wait cleanup remain a separate commit.

The [focused Claude Code review and disposition](claude-review.md) found no
correctness regressions in the four requested areas. It was static review of a
provided packet, without client execution. The [follow-up](claude-followup.md)
found no confirmed correctness defects; its nested-ownership question is resolved
by the pre-registered tracked lifecycle helper and existing nested cancellation
tests. Its smaller suggestions were applied: growth-only size assertions,
preserving even an uncommitted Shutdown through state Drop, and a diagnostic-text
migration note. A new direct cleanup unit checks all primary outcomes without
relying on a committed-cause slot. No client pass is inferred from these checks,
and none of these results authorizes release.
