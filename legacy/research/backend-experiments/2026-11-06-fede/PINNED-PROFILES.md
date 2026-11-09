# Fixed-core actor pipeline experiment

The Silver-inspired profile uses one process-owned current-thread Tokio runtime
on each selected physical core. Shards stay on their cores; actors are assigned
round-robin at startup. A parked owner waits for new work. A spinning owner stays
runnable and yields to Tokio tasks, timers and I/O between checks. Both use
ordinary Spawned actor methods, owned messages, private addressed mailboxes and
end-to-end replies. Neither implements Flux's Copy-only broadcast rings,
shared-memory payload cache or overwrite-on-lapping behavior.

These selectors are private research controls behind backend-experiments. The
public Actor/Handler/Backend signatures and Tokio default remain unchanged.
Affinity currently requires Linux and taskset. A failed pin fails worker startup;
placement/wait options are read once and frozen when the pool starts. Set them
before creating any actor; subsequent environment changes do not reconfigure it. Stop and await actors before
terminal shutdown_sharded; shutdown joins workers. Blocking a handler still
blocks its assigned worker. Pinning gives placement, not CPU reservation or
preemption. Blocking helpers can inherit the creating shard's single-core
affinity and compete with the spinning owner. Already-running blocking helpers
may outlive shutdown_background; this profile does not reserve separate helper
cores or promise bounded blocking latency.

## Reproduce the measured comparison

Use the V9 source bundle and recorded compiler/build environment from the results
archive. Its measured source is a8471f74b2b2d8c6550690d27679216a7341c774.
The controller pinned9.py and shell/gate logs are archived as evidence. Run only
on a machine where these CPU IDs are available, on different physical cores;
substitute and record masks for other machines. The fede masks are specific to
its topology, including asymmetric L3 domains at eight actor cores.

An illustrative four-core spinning case, after building the benchmark:

    SPAWNED_EXPERIMENT_SCHEDULER=sharded \
    SPAWNED_EXPERIMENT_WORKERS=4 \
    SPAWNED_EXPERIMENT_BATCH=16 \
    SPAWNED_EXPERIMENT_SLICE_US=50 \
    SPAWNED_EXPERIMENT_PIN_CORES=2,3,4,5 \
    SPAWNED_EXPERIMENT_IDLE_SPIN=1 \
    taskset -c 2,3,4,5 <pinned_pipeline-executable> \
      --workers=4 --groups=1 --messages=1048576 --producers=256 \
      --idle-ms=1000 --actor-cpus=2,3,4,5 --driver-cpus=10,11,12,13 \
      --audit=/tmp/pinned-pipeline-audit.csv

Spin 0 uses the same pinned profile with idle parking. The Tokio reference removes
all experimental selectors and uses the same workers, arguments and outer mask;
its workers share that mask without individual pinning. Build with
cargo bench --locked -p spawned-concurrency --features backend-experiments
--bench pinned_pipeline --no-run. The controller records the executable hash and
actual clean source, rather than relying on the filename.

## What is compared

The matrix has three profiles × four stage/core counts (1,2,4,8) × two actor
populations (one or 16 actors/core) × three fresh processes = 72 full cases, plus
24 separate smoke cases. Every root performs exactly 800 LCG iterations, divided
among N sequential stages, and returns one end-to-end reply. Each hop is an owned
one-way actor message. Groups1 gives one N-stage pipeline; groups16 gives 16 such
pipelines. There are 256 logical producers on four fixed driver runtime threads,
with one outstanding root per producer. All actor placements are audited before
warmup; replies, computation, message handling and actor lifecycle are checked.

Throughput and p99 cover the active phase. Whole-process CPU is measured separately
for one second idle after warmup and for active work; driver/runtime threads are
included. Tick resolution limits tiny parked CPU values. Equal total computation
does not mean equal communication: N also changes hop count. This is a new
streaming fixture, not the earlier recursive eight-hop request/reply graph.
Sharding, batching and placement differ together from the default reference;
park versus spin is the narrower comparison. No throughput result alone proves
affinity caused a change, or establishes electrical power consumption.

## Choosing what to investigate

Keep Tokio as the supported default for dynamic populations and general async
applications. Investigate pinned parked shards for fixed pipelines needing
predictable placement and acceptable measured throughput/latency tradeoffs.
Consider spinning only with dedicated CPU capacity and a demonstrated latency
need that outweighs its idle CPU use. More actors per core can share a worker;
that does not create a thread per actor. Fixed placement can also strand capacity
behind one busy stage. Use a representative client workload and its latency/CPU
budget before supporting a new configuration. The report contains measured
tradeoffs; no private profile is promoted by this study.

Post-measurement source `20260cd` freezes optional pin/spin parsing at startup,
makes the benchmark actor mask self-contained and supplies a non-Linux stub.
It passes its own full gates. Reported timings remain on a8471f7, whose controller
already applied the same actor mask externally. No timings are attributed to the
post-measurement source. The measured bundle retains a8471f7; the retained bundle
also includes 20260cd.
