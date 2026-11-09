# Applying backend experiments to ethrex and ethlambda

This is a source audit, not a client performance result. Both clients were fetched
from upstream main on 6 November 2026 and inspected in separate detached worktrees:
ethrex 1dfaa09ec791b36d9f0ea3c613c2f3747c09f788 and
ethlambda 78359e2c90317fd1c3d8980abcac90c2fd8fb14e. Their working checkouts were
left unchanged. No backend was switched and no client benchmark was run.

## The important existing split

**Both clients already isolate important work.** A global async-scheduler switch
would miss those actors and would not automatically shorten their handlers.

| Client / path | What the inspected code does | Implication |
|---|---|---|
| ethlambda chain actor | Starts `BlockChainServer` with `Backend::Thread`, explicitly because long synchronous CPU work starves a shared runtime | Already has dedicated actor execution; async-shard selectors do not move this actor |
| ethlambda networking | P2P actor starts normally; libp2p swarm runs in a separate Tokio task using command/event channels | Isolate/measure the entire network path, not just the actor mailbox |
| ethlambda validation and aggregation | Semaphore-limited stateful gossip checks, epoch precompute and aggregation use `spawn_blocking`, returning results to actors | Worker CPU, queueing and result handling remain costs outside the actor scheduler |
| ethrex block producer | Starts the sequencer `BlockProducer` with `Backend::Blocking` | Async-shard selectors do not change its execution backend |
| ethrex peers / broadcaster | Dynamic per-connection actors and transaction broadcaster use ordinary async startup; some serving work is explicitly offloaded | Tokio remains the first baseline; actor count, data work and helper contention matter |
| ethrex execution / Merkle work | Global Rayon pool plus a dedicated 17-thread Merkle pool | Actor worker count is only part of process CPU capacity; do not reduce the load-bearing Merkle pool to fit an actor experiment |

Pinned source references:

- [ethlambda dedicated chain actor](https://github.com/lambdaclass/ethlambda/blob/78359e2c90317fd1c3d8980abcac90c2fd8fb14e/crates/blockchain/src/lib.rs#L432), [swarm adapter](https://github.com/lambdaclass/ethlambda/blob/78359e2c90317fd1c3d8980abcac90c2fd8fb14e/crates/net/p2p/src/swarm_adapter.rs#L247), [stateful validation worker](https://github.com/lambdaclass/ethlambda/blob/78359e2c90317fd1c3d8980abcac90c2fd8fb14e/crates/net/p2p/src/beacon/verdict.rs#L638).
- [ethrex blocking block producer](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/l2/sequencer/block_producer.rs#L350), [per-connection startup](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/networking/p2p/rlpx/connection/server.rs#L190), [load-bearing 17-thread Merkle pool](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/blockchain/blockchain.rs#L573).

The Merkle pool comment states 16 communicating shard workers plus a watcher must
be resident; making the pool smaller can deadlock. This is an application
invariant, not a proposed tuning experiment. Similarly, moving a long-running
serial handler to another scheduler does not let later messages interrupt it.

## First application experiment: ethlambda deadline and mailbox behavior

The code already separates aggregate processing time from mailbox waiting:
`lean_beacon_aggregate_processing_seconds` and
`lean_beacon_aggregate_mailbox_wait_seconds`. It also records tick intervals,
block building, signature work and fork-choice timings. The mailbox timestamp
crosses the actual P2P-to-chain handoff. Use these metrics to distinguish waiting
behind chain work from slow validation or scheduling.
[Metric definitions](https://github.com/lambdaclass/ethlambda/blob/78359e2c90317fd1c3d8980abcac90c2fd8fb14e/crates/blockchain/src/metrics.rs#L1545),
[handler entry measurement](https://github.com/lambdaclass/ethlambda/blob/78359e2c90317fd1c3d8980abcac90c2fd8fb14e/crates/blockchain/src/lib.rs#L4040).

Replay a fixed block/aggregate arrival trace through actual actors while generating
controlled validation load. Preserve Beacon versus Lean behavior as separate
workloads. Record handoff-to-handler p50/p99, service time, end-to-end verdict/head
latency, late duty/tick counts, validation overload outcomes, CPU and queue depth.
Compare the existing configuration with explicit core placement or a separate
network/validation CPU budget. If handler service dominates, investigate bounded
work per turn, offloading or application batching before another ready queue.
Those changes must preserve chain ordering and validation rules.

Existing offline imports are useful compute controls but do not establish an
actor-scheduler speedup: the import benchmark constructs an unspawned server with
`for_replay` and calls `import_block` directly.
[Replay setup](https://github.com/lambdaclass/ethlambda/blob/78359e2c90317fd1c3d8980abcac90c2fd8fb14e/bin/ethlambda/src/benchmark/import/replay.rs#L123),
[direct import](https://github.com/lambdaclass/ethlambda/blob/78359e2c90317fd1c3d8980abcac90c2fd8fb14e/bin/ethlambda/src/benchmark/import/replay.rs#L210).

## Second application experiment: ethrex peer load and block production

Use a fixed transaction/block corpus and controlled peer traffic. Measure
transaction admission-to-propagation p50/p99, peer serving latency, broadcaster
handler time/queue wait, block-build phase times and total CPU. Test stable peer
populations and reconnect churn separately. Keep execution, Rayon, storage and
blocking-worker settings fixed while comparing actor scheduling.

Start with the current Tokio async peers and existing blocking producer. Trial a
bounded isolated networking/control runtime only if profiling shows scheduling
or cross-runtime wake cost matters. A busy broadcaster can also be limited by
transaction selection/cloning, mempool/storage or work inside a handler; the
code's async-looking outgoing-message wrapper enqueues actor work without waiting
for network transmission. Do not infer slow network waits from the wrapper's
`.await` syntax alone.
[Broadcast loop](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/networking/p2p/tx_broadcaster.rs#L337),
[outgoing-message wrapper](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/networking/p2p/rlpx/connection/server.rs#L226).

## Compatibility and decision rule

Both manifests use registry Spawned 0.5.0. An experiment needs both `spawned-rt`
and `spawned-concurrency` patched to the same recorded research revision, on an
isolated client branch, with selectors off as the same-source baseline. Do not
assume a published 0.5.0 build contains current private experiments. Existing
`Backend::Thread`/`Blocking` choices remain explicit; private async selectors do
not silently override them.

Do not pin an entire client with the small benchmark's outer `taskset` command:
that would also constrain unrelated Rayon/storage/helpers and change the resource
budget. Assign and record pools explicitly. Pinned shard blocking helpers can
inherit their shard's single-core affinity, so placing validation on those shards
can concentrate work rather than isolate it. Measure helper placement and idle
CPU, not just actor workers.

The new [shared-versus-split runtime benchmark](README.md) answers a narrower
mechanism question under synthetic CPU load, including a general-runtime relay
that remains on the critical request path. It is not an ethrex/ethlambda speedup.
Promote a configuration only after a repeatable improvement in the selected
client's end-to-end metric, acceptable background throughput/CPU and unchanged
correctness, lifecycle and protocol behavior. The current evidence justifies
application measurements; it does not justify a global backend replacement.
