# AGY review: execution groups plus supervision

Reviewed 2026-11-08 at the user's request. The
[MVP proposal](../../../docs/design/execution-groups-supervision.md) and
[roadmap](../../../ROADMAP.md) make runtime ownership/placement and automatic recovery
one combined milestone, delivered as 1a then 1b.

**Final AGY verdict: no remaining blocking design/API-contract gaps.** This is
a static documentation review, not approval of implementation code, test
coverage, a new release or a production rollout. The APIs were unimplemented at
the time of this design review. The subsequent implementation has a
[separate review and validation record](execution-groups-supervision-implementation.md).

The original reviewed documentation is preserved in commit `1288031`; the
recorded hashes refer to that design snapshot. This page now links the later
implementation record.

## Revisions from review

| Review concern | Contract now chosen |
|---|---|
| Deadline expires while cleanup still runs | Return an incomplete ShutdownReport with owned pending-work state. Keep affected groups closed, support later joining and prevent overlapping replacement generations. |
| Shutdown reason / control observation | Commit an uncommitted Shutdown cause before existing cancellation; preserve prior causes and real cleanup panics. Observe generation state directly rather than relying on FIFO user-mailbox Down/exit handlers. |
| Factory and helper execution context | Invoke/poll factories in the target group. Gated group capabilities route Context helpers correctly from foreign runtimes/plain threads. |
| Failed startup and readiness | Async factory Result supplies fallible construction. Existing started/stopped hooks remain unchanged; explicit readiness/stop rules cover construction, panic, cancellation and self-messages. |
| Restart accounting | One supervisor-wide rolling retry budget; per-child backoff/jitter/stability reset, starting only after that generation is ready. |
| Runtime ownership / ambient work | Stale public handles are weak/gated and do not retain completed runtimes. Plain `.start()` remains ambient/unmanaged; managed scope has explicit admission. |
| Registry/timer generations and replies | Compare-and-remove owned registrations; join generation-scoped helpers/timers; queued request timeouts can win during stopped-hook/mailbox cleanup. |

The review's initial description of current `normal_stop` as an unconditional
overwrite was checked against source and corrected: it writes only when the
cause slot is empty. Likewise, the native stop sentinel does not require
processing the preceding user backlog; the cancellation flag is checked before
dispatch. These were documentation clarifications, not newly established
production defects. The final AGY pass explicitly verified those facts.

## Evidence and limits

The first CLI attempt ended after a denied shell-read command and supplied no
verdict. A self-contained source/diff packet then produced substantive review
comments, but the client reported a stream interruption. Those comments were
used to improve the design and were not treated as approval. The final compact,
self-contained review returned **SUCCESS** and a completed verdict.
An additional successful check verified the README clarification separating
Tokio-free thread builds from replacing Tokio for async applications; the
design-only verdict remained unchanged.

[The review record](execution-supervision-evidence/review-record.json) identifies
the exact reviewed file hashes and input-packet hash.
[Final review output](execution-supervision-evidence/revised-final.json),
[README clarification check](execution-supervision-evidence/readme-clarification.json),
[earlier comments](execution-supervision-evidence/comments-stream-error.json)
and [the initial denied read](execution-supervision-evidence/initial-read-denied.json)
retain the distinction between substantive feedback and successful verification.

No builds or tests were run by AGY for this documentation review. The separate
[runtime worker-budget tests](dedicated-runtime.md) and
[Engine-client experiments](../../research/client-observations/2026-11-06-ethlambda-ab/server-probe/one-worker/README.md)
do not establish correctness of future supervisor code.

Implementation must still test group ownership, startup barriers, cleanup
deadlines, restart generations/control fairness and supported execution modes.
A one-worker async pool is not the same execution model as a dedicated actor
root with one helper worker: long CPU/blocking handlers need appropriate
placement and a workload check. The proposal deliberately defers specialized
profiles rather than inferring their semantics from a synthetic latency win.
