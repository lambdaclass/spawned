# Bounded dedicated-actor Tokio workers

The previous `tasks::Backend::Thread` drove its actor on one native root thread
and created `Runtime::new()` per actor. Each isolated runtime inherited Tokio's
machine-sized/environment-configured worker count. Both Spawned 0.5 and 0.6 have
this behavior; it is not a newly introduced 0.6 regression.

The default now builds an isolated **multithread runtime with one async worker**.
This preserves an independent place for async helpers to progress while the
actor's root thread blocks, and retains `block_in_place`, async I/O and timers.
No actor/protocol type or startup call changes. The explicit worker count also
applies to the public `spawned_rt::threads::block_on` bridge, independently of
`TOKIO_WORKER_THREADS`. Private experimental selectors remain separate.

A direct switch to `current_thread` was rejected for this fix: synchronous waits
for local async helpers could deadlock, and `block_in_place` would panic. That
mode remains an explicit experiment. A shared helper pool was not promoted: its
ownership/isolation/shutdown contracts and workload results need further work.

This does not promise exactly two total process threads or unlimited helper
parallelism. Blocking/DNS/filesystem work can spawn further threads. One blocked
async worker can stall other helpers; they must cooperate or use `spawn_blocking`.
Per-actor reactors remain a descriptor cost. Actor completion does not join
arbitrary detached helpers or guarantee the runtime has finished destruction;
a nonterminating blocking helper can retain the native thread during teardown.

## Validation

On fede, logical CPUs 2–5, nice 10, Rust 1.93.0, build jobs 2:

- Four new focused integration tests pass with default and all features. They
  check the worker bound, runtime flavor, stable root identity, started/handler/
  stopped helper timers, synchronous helper waits, `block_in_place`,
  `spawn_blocking`, real loopback TCP, nested Async actors, startup without an
  ambient runtime and survival after the caller runtime is dropped.
- The worker-bound regression **fails against the old implementation** with
  `TOKIO_WORKER_THREADS=4` (observed 4, required 1), then passes against the fix.
  The old source was a temporary single-file overlay, restored in `finally`;
  the test source itself was unchanged. See [negative control](dedicated-runtime-evidence/negative-control/result.json).
- Workspace/default/all-feature tests, all-target Clippy/default/all-features,
  extended histories, controlled progress simulation, payload stress, scenarios,
  churn and both Loom models pass. Clippy uses the installed matching 1.98.1
  Rust/Clippy pair; ordinary tests use 1.93.0. Exact commands and sampled resource
  peaks are in [check records](dedicated-runtime-evidence/checks/runs.json).
- The [dedicated-thread example](../../../examples/capabilities/src/bin/dedicated_thread.rs)
  runs successfully and demonstrates independent helper progress. CI runs it
  alongside the other capability examples.

A first all-feature compilation with four build jobs exceeded the campaign's
2GiB sampled aggregate RSS ceiling and was killed by its owned-group watchdog.
The successful validation used two build jobs, without raising that ceiling.
This was a compiler resource-limit preflight, not an actor test failure. No
latency/throughput improvement is claimed from the worker-bound regression.
