# Full actor sharding: experiment and research questions

This follows the [design study](../../benchmarks/DESIGN-FINDINGS.md). The previous
native loops omitted async handlers and lifecycle overhead. This experiment runs
Spawned's actual actor future, boxed envelopes, unbounded addressed mailbox,
reply type, cancellation, monitors, panic handling and stopped callback.
Existing public Actor/Handler/Backend signatures and production defaults remain
unchanged. The experimental hooks are feature-gated and hidden from stable docs.

## Designs being compared

| Variant | Actor execution | Message turn |
|---|---|---|
| async | Existing caller-owned multi-thread Tokio runtime, N workers | Existing Tokio cooperative policy |
| actor-tokio-1 / 16 / adaptive | Same runtime as async | Explicit actor yield after backlog batches of 1 / 16 / adaptive 1–64 |
| actor-sharded-1 / 16 / adaptive | N independent current-thread Tokio runtimes on N OS threads; static round-robin actor assignment | Same explicit policies as batched Tokio |

Explicit turns also check a 50 µs elapsed budget between handlers. Idle mailbox
time is excluded. No handler is preempted, and `yield_now` is a scheduling hint,
not a guarantee that another task or I/O event runs next. Empty mailboxes park
without an extra explicit yield. The actor rechecks cancellation after a yield
so a stop during the scheduling gap cannot dispatch another queued message.

The pool is process-owned. It deliberately does not disappear when the last
actor stops: detached helpers, cross-backend monitors and blocking children may
still require its reactors. A private, terminal `shutdown_sharded()` hook lets
the experiment stop/join all actors, await important helpers, close registration
and join workers. It refuses calls from shard workers and waits for registered
actor leases to drain. Other actor backends are conservatively tracked while the
pool exists. Remaining detached async helpers are cancelled at explicit pool
shutdown. Already-running blocking helpers can outlive background shutdown.
Do not present this private lifecycle as a finished production runtime API.

All selected actors share this process pool across caller runtimes. Actor loops
outlive a dropped caller runtime. Helpers created externally from an ActorRef's
Context still attach to the runtime at their call site, as existing helper APIs
do; this experiment does not transparently relocate external tasks. Spawning a
current-thread-incompatible operation such as Tokio `block_in_place` inside a
shard is unsupported. Synchronous blocking stalls that entire shard, including
its timers and sockets. No work stealing, CPU affinity or !Send handler support
is added. Runtime configuration is fixed for the process; shutdown is terminal after pool
initialization. Calling the private shutdown hook before initialization is a no-op.
Its registration count cannot discover every external helper, so callers must
still follow the stop-all/await-important-helpers contract.

## Equal worker budgets and workload coverage

Sharded samples use a current-thread observer runtime and N actor workers;
ordinary/batched Tokio samples use N workers total for actors. Producers and the
common native echo server still use additional threads, shared by all variants.
Each process is fresh, warmed and randomly ordered. Do not compare these new
full-handler numbers directly with the older native-loop dataset/source revision.

The CPU scaling/hotspot cases retain 128–1,024 actors and up to 256 producers.
New cases use 128 actors and 128 closed-loop producers:

- `async_wait128`: 1 ms Tokio timer per handler; 16,384 requests, 128 warmups.
- `blocking128`: 100 µs OS sleep per handler; 8,192 requests, 128 warmups. This
  deliberately characterizes damage from blocking inside an async backend.
- `tcp128`: one persistent TCP_NODELAY socket per actor, 64-byte validated echo
  frames; 32,768 requests, 128 warmups. A common native server has one blocking
  thread per connection. This is loopback I/O, not a production network service.

All seven compared variants use the same message/reply layout and lifecycle,
with unbounded mailboxes. This removes the previous bounded-admission confound
from the scheduler comparison. Allocation counting remains a separate case;
queue time and handler service time include their proper async/I/O boundaries.

## Correctness and resource tests

- Two blocking workers and three persistent actors: the third waits until an
  occupied worker is released; no machine-wide resource changes.
- Child-only FD limit: default async and preinitialized shards both reuse reactors
  after descriptor exhaustion. Dedicated-current cannot create another reactor.
  Cold shard initialization under exhaustion reports termination, and retries
  successfully after descriptors are released. This characterizes reuse/startup;
  it does not measure descriptors per actor or make I/O work without descriptors.
- Same-shard async suspension, same-shard blocking and independent-shard progress.
- Nested actor spawning, detached helpers, timers, real TCP, blocking children,
  caller-runtime drop, stop with queued work, and cross-backend monitor delivery.
- Direct regression for stop during a batch yield, and idle-time budget accounting.
- Existing generated lifecycle/race/FIFO/monitor tests on the selected backend.

An all-feature workspace run with selectors unset exercises the unchanged
default. Selecting a two-worker process pool for the entire parallel unit suite
changes its resource isolation: formerly independent test runtimes now contend
for that pool, and three existing timing demonstrations failed. Serial selection
removes that contention; one legacy test still expects the blocking actor and a
counter to interfere, whereas round-robin placement separates them. Its original
assertion is kept unchanged, and the serial characterization excludes that one
scheduler-layout timing demonstration explicitly. New gated tests check actual
same-shard blocking and cross-shard progress deterministically. Report these
limitations; do not call the selected parallel suite a clean pass or promote the
backend to default from these measurements.

## What the research tells us to test next

1. **Separate policy from placement first.** Tokio already has cooperative
   scheduling and work stealing. Compare explicit batches on existing Tokio with
   the identical batches on shards before attributing an improvement to either.
   [Tokio runtime behavior](https://docs.rs/tokio/1.48.0/tokio/runtime/index.html),
   [yield scheduling non-guarantees](https://docs.rs/tokio/latest/tokio/task/fn.yield_now.html).
2. **Measure skew and actor-to-actor routing next.** Seastar's locality argument
   depends on ownership and communication patterns. A producer-only RPC test
   misses intra-shard versus cross-shard chains, fan-out/fan-in, hot-shard
   concentration and actor placement. Those should precede a custom work-stealing
   executor, because an existing Tokio runtime already provides work stealing.
   [Seastar shared-nothing design](https://seastar.io/shared-nothing/).
3. **Treat fairness as an I/O concern too.** Long CPU turns can delay timers and
   readiness polling. Sweep batch size/time budgets and measure cold-actor p99,
   reactor delay and throughput at fixed offered rates. Seastar checks quotas at
   cooperative points; this does not imply arbitrary Rust handlers can be forcibly
   preempted. [Seastar task quotas](https://docs.seastar.io/master/tutorial.html#preemption-and-task-quota).
4. **Use lifecycle correctness as a gate.** Runtime ownership, shutdown and
   detached work need explicit contracts before chasing another few percent.
   The first actor-count-owned pool was rejected because its last actor could
   destroy reactors still needed by helpers and blocking children.
5. **Delay another kernel backend until the workload motivates it.** The earlier
   io_uring study found a throughput/tail tradeoff. Full-actor network/storage
   workloads, latency under saturation and a target application should determine
   whether owned-buffer/kernel integration earns its compatibility cost.

Actix Arbiters provide a useful historical comparison: one thread/event loop,
fixed actor placement, addressed communication across loops, and separate handling
for synchronous work. This is an architectural comparison, not a framework speed
claim or adoption recommendation; its actor book is now archived.
[Actix Arbiter documentation](https://actix.rs/docs/actix/arbiter).

Do not add affinity, stealing, spin loops or another transport simultaneously.
Choose the next change from this experiment's counterexamples and then repeat a
matched comparison. A result where ordinary Tokio wins is useful evidence.
