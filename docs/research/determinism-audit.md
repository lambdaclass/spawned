# Determinism audit: what escapes the simulator

Research note, 2026-11-11. Determinism is priority tier 3 in the
[roadmap](../../ROADMAP.md#priorities): every run that can be made
deterministic should be. This note lists what escapes the deterministic
simulator (`--cfg spawned_sim`, see [TESTING.md](../TESTING.md#seeds-and-decision-tapes-under---cfg-spawned_sim))
in Spawned and in its two clients, and backs the
[determinism backlog](../../ROADMAP.md#determinism-backlog-tier-3) (D1-D10).

Audited: Spawned `main` (390e4231) and `feat/otp-exit-signals` (the 0.8
candidate); Ethrex and Ethlambda `spawned-v0.8`, both on Spawned rev 850fd785.
Method: search for direct Tokio spawns and timers, OS threads,
`spawn_blocking`, rayon, wall-clock reads, unseeded randomness, default-hasher
maps, environment reads and real I/O, then classify each hit. Spawned's hits
were classified by hand; client hits by pattern, with spot checks.

## How the simulator treats each escape

| Escape | Under `spawned_sim` today |
|---|---|
| `tokio::spawn`, `tokio::task::spawn_blocking` | Fails the run (detected via the per-poll Tokio runtime's metrics) |
| `tokio::time` | Panics (the per-poll Tokio runtime has no timer driver) |
| `spawned_rt::tasks::spawn_blocking`, `block_in_place` | Panics with a message naming the call |
| OS threads, threads mode, Blocking/Thread placements | Not run; a wakeup from another thread panics |
| `std::time::Instant::now`, `SystemTime::now` | Silently reads the wall clock |
| `rand::thread_rng`, `OsRng`, `HashMap` with `RandomState` | Silently uses OS entropy |
| Sockets, files, environment | Real |

The silent rows are the gap: nothing tells a test author their run is not
deterministic.

## Spawned

The library is clean of direct Tokio, `thread_rng` and `OsRng`. Library time
goes through `spawned_rt::time` (virtual under the cfg), the monitor `HashMap`
walk is ordered under the cfg, and `registry::registered()` is sorted.
Remaining escapes that affect behaviour under simulation:

| Where | What | Status |
|---|---|---|
| Process-global id counters | Actor, monitor and trace ids depend on what ran earlier in the process | Documented; D5 |
| `failpoints.rs:338` | `FailScenario::setup()` without `SPAWNED_FAILPOINTS_SEED` seeds from `SystemTime` | Documented; D5 |
| `tasks/actor.rs:979-1177` | `backend-experiments` env knobs and `std::time::Instant` time slice | Feature-gated, documented |
| `sim.rs` watchdog and turn budget | Wall-clock verdicts: can fail a run on an overloaded machine, never change a schedule | D10 |
| `test_wait::wait_for_async` | `std::time::Instant` deadline around simulated sleeps (unit tests only) | Test helper |
| 0.8: `tasks/jobs.rs` `RunnerPool::Thread` | Native jobs run on an owned OS thread under sim | Documented in [deferred replies](../design/deferred-replies.md) §10; D8 |
| 0.8: `reply/pending.rs:92` | `PendingReplies::drain` walks a `HashMap`, so deferred replies are answered in per-process random order | New; D5 |

## Clients

Production code of the actor subsystems (tests, tooling, EVM, prover
excluded). Counts are pattern occurrences.

| Client | Fails run (`tokio::spawn`, `JoinSet`) | Panics (`tokio::time`) | Threads, `spawn_blocking`, rayon, native backends | Wall clock | Unseeded rand | Real I/O |
|---|---|---|---|---|---|---|
| Ethrex | 34 | 48 | 71 | 162 | 48 | 376 |
| Ethlambda | 8 | 8 | 24 | 109 | 3 | 208 |

Neither client has a simulated test yet. By subsystem:

| Subsystem | Main escapes | To run its actor tests simulated |
|---|---|---|
| Ethrex p2p | Sockets (discv4/v5, RLPx), a Thread-backend initiator, ephemeral keys and peer choice from `OsRng`/`thread_rng`, TTL and revalidation clocks | D1, D3/D4, D8; actor logic (`peer_table`, `tx_broadcaster`) with fake connections; sockets need rank 22a |
| Ethrex sync | 20 direct spawns and `JoinSet`s, 15 Tokio timers, 15 blocking/rayon offloads, `rand::random` request ids | D3/D4 (largest routing job), D2, D1; a fake peer handler until 22a |
| Ethrex blockchain | Pipelined `add_block` threads, merkle and prewarm rayon pools | Block execution is one atomic synchronous step; simulable at the actor boundary only |
| Ethrex rpc, storage | `spawn_blocking` around RocksDB and execution, axum | D2, D4; HTTP with 22a; the database stays synchronous |
| Ethrex l2 | Two Blocking-backend actors, L1 over HTTP, `SystemTime` | D8, D1, D4, a fake L1 client |
| Ethlambda blockchain | 61 `std::time::Instant` reads (most are metric timings; the aggregation deadline is not), `SystemTime` slot clock, a key-manager thread | D1 alone virtualizes its timing; best first candidate (D6) |
| Ethlambda p2p, rpc | libp2p swarm, 20 `spawn_blocking` duty and check computations, `thread_rng` peer choice | D1, D2, D4; libp2p with 22a's custom transport |

Synchronous databases (RocksDB, libmdbx), EVM execution, merkleization and
cryptography are fine: they run as atomic synchronous steps and return the
same result for the same input.

## Same-seed reruns (S2)

Eleven simulated suites (supervision, nested supervision, policies, trees,
ordering, semantics gaps, exits, startup admission, the simulator's own), the
default seed sweep, forced decision tapes and the fuzz corpus (64 seeds and 5
regressions) each ran twice in separate processes on one machine. Test
outcomes, all 21 forced tapes and the fuzz driver's virtual-time trace were
byte-identical. Lifecycle traces were identical except for global ids (and a
proptest left unseeded); with ids masked they matched, also with eight
parallel test threads. **No schedule nondeterminism was found.** Per-world id
counters (D5) would make the comparison byte-exact and cheap enough for CI.

## The key technique: interposing the clock and entropy (D1)

madsim makes code that calls `std` directly deterministic by defining
`clock_gettime`, `gettimeofday`, `getrandom` and `getentropy` (and the macOS
equivalents) as `#[no_mangle]` functions in simulation builds; they return
simulated values and otherwise fall through to the real symbol via
`dlsym(RTLD_NEXT)` ([system_time.rs](https://github.com/madsim-rs/madsim/blob/main/madsim/src/sim/time/system_time.rs),
[rand.rs](https://github.com/madsim-rs/madsim/blob/main/madsim/src/sim/rand.rs)).
For Spawned this covers every wall-clock and entropy read in both clients
without changing a call site, and a strict mode can turn each one into a
failure that names the caller. Limits: `std`'s `HashMap` keys are drawn once
per thread, so maps created before the world starts keep OS keys (madsim
documents the same caveat); the simulator's own watchdog must call the real
symbol; Linux first.

## Sources

- Simulator: `rt/src/tasks/sim.rs`, `rt/src/tasks/sim_test.rs`,
  [TESTING.md](../TESTING.md#seeds-and-decision-tapes-under---cfg-spawned_sim).
- madsim: links above.
- Client branches: `lambdaclass/ethrex@spawned-v0.8` (c1163b7),
  `lambdaclass/ethlambda@spawned-v0.8` (85990aa).
