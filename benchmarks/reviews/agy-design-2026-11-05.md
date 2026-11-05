# Independent agy design review

First review: `24d9fcd`. Corrected gate and one-reactor I/O follow-up: `aae6312`.
These are benchmark-scope reviews, not approval of a production backend.

## Initial review

## Blockers (Must Fix Before Trusting Campaign Data)

1. **`native-ready` Lost-Wakeup & Spin-Reschedule Races ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L195-L206), [`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L420-L447)):**
   - **Pre-send `pending` increment:** [`NativeSender::send`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L197-L201) executes `signal.pending.fetch_add(1, Ordering::AcqRel)` *before* `self.tx.send(probe)`. If a producer is preempted between lines 197 and 200—or blocks on a full 256-slot `sync_channel`—`pending` is positive while the message is not yet in `rx`. A worker draining `rx` hits `try_recv() -> Err(Empty)`, clears `scheduled`, sees `pending > 0` (from the blocked/in-flight producer), and re-enqueues `Some(id)` in a hot spin loop.
   - **StoreLoad reordering between `scheduled` and `pending`:** In the worker loop ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L442-L447)), `actor.scheduled.store(false, Ordering::Release)` followed by `actor.pending.load(Ordering::Acquire)` is **not** sequentially consistent (`Release` store followed by `Acquire` load permits StoreLoad reordering on ARM64/Apple Silicon and x86). A worker can observe `pending == 0` while a concurrent producer observes the stale `scheduled == true`, leaving `scheduled == false` with `pending > 0` and stranding the actor permanently.
   - **Fix:** Move `pending.fetch_add(1, Ordering::SeqCst)` *after* `self.tx.send(probe)` succeeds, use `Ordering::SeqCst` on `scheduled.swap(true, ...)` and `pending.load(Ordering::SeqCst)`, and release `scheduled` with `actor.scheduled.store(false, Ordering::SeqCst)` (or a `SeqCst` fence) before re-checking `pending`.

2. **Undisclosed Queue-Depth & Timestamp Distortion in `burst32` and `overload` ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L670-L695)):**
   - `async`, `threads`, `dedicated-*`, and `plain-tokio` use **unbounded** mailboxes, whereas all `native-*`, `sharded-*`, and `ready-*` variants use **bounded** `sync_channel(256)` mailboxes.
   - In `burst32` (`scenario = "burst"`), `scheduled = Instant::now()` is captured immediately before each `request()` ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L673-L680)). For unbounded variants, all 32 producers enqueue 131,072 messages immediately (`scheduled ≈ t0`), so `completion_p99_ns` and `queue_p99_ns` measure the time to drain the entire 131,072-message backlog. For bounded `native-*` variants, `self.tx.send(probe)` blocks producers once 256 messages are queued; subsequent messages capture a fresh `scheduled = Instant::now()` *after* backpressure unblocks them, artificially capping measured queue wait and completion latency to ~256 messages of backlog instead of 131,072.

3. **Unintended Single-Thread-Per-Connection Architecture in [`io_designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/io_designs.rs#L56-L155):**
   - Each of the `connections` (up to 128) client threads constructs its own isolated `tokio` (`new_current_thread`) or `monoio` runtime driving **one** synchronous-style ping-pong socket alongside 128 server threads (256 threads total on a 4-CPU Linux VM). This does not test event-loop connection multiplexing; it measures single-socket reactor wrapper overhead and 128-instance `io_uring`/epoll setup under severe thread oversubscription.

---

## 1. Runnable-Queue Scheduling, Shutdown & Batching/Fairness Audit

- **Duplicate execution / lost work:** `actor.rx.lock()` ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L420)) prevents concurrent handler execution on the same actor. However, because `actor.scheduled.store(false, ...)` happens at line 442 *even when `pending > 0`*, a producer can flip `scheduled` (`false -> true`) and push `Some(id)` just before the worker's own `pending > 0` check at line 443, causing an extra wakeup handoff.
- **Shutdown safety:** In [`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L795-L799), `ready_shutdown` sends one `None` per worker to `txs[worker % txs.len()]`. Because producers join after receiving all replies before shutdown begins, no work is lost in this benchmark, though a library backend would need drain-before-stop guarantees.
- **`native-sharded` wakeups ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L452-L507)):** Using `wake: sync_channel(1)` with `try_send(())` after `tx.send(probe)` is deadlock-free (if `wake` is full, a wake token is already pending; if the worker already passed `rx[i]` in the current scan, it consumes the token on `wake_rx.recv()` and rescans).
- **Batching & fairness claims:**
  - In `ready-adaptive` ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L421-L436)), `turn.elapsed() >= Duration::from_micros(50)` calls `Instant::now()` **after every single message**. For fast handlers (`iterations = 0`), fixed `ready-16` avoids per-message clock reads entirely, confounding adaptive batching with clock overhead.
  - In `hotspot128` (`iterations = 1000`, ~3 µs/msg), the 50 µs turn budget caps batches at ~16 messages anyway, making `ready-adaptive` nearly indistinguishable from `ready-16` on the hot actor.

---

## 2. Other Benchmark Biases & Apples-to-Oranges Confounds

- **Shard & Producer Alignment Bias in `hotspot128` ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L617-L622), [`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L643)):**
  - With `producers = 128` (even) and `step_by(128)`, the 64 even producers *only* ever generate even `id`s (routing 100% to hot Actor 0), while the 64 odd producers *only* generate odd `id`s (cycling across cold Actors 1..127).
  - Worse, in `sharded-*` and `local-ready-16` (`workers = 2`, partitioned by `actor % 2`), Worker 0 owns Actor 0 (50% of total traffic) **plus** all 63 even cold actors (25% of total traffic) = **75% of total load**, while Worker 1 gets only **25% of total load** (64 odd cold actors). `cold_p99_ns` pools starved Worker-0 cold actors with uncontended Worker-1 cold actors.
- **CPU Core Allocation Mismatch (`workers = 2` vs Per-Actor Threads):**
  - `async`, `plain-tokio`, `sharded-*`, `ready-*`, and `local-ready-16` are pinned to `workers = 2` ([`design_experiments.py`](https://github.com/lambdaclass/spawned/blob/24d9fcd/benchmarks/design_experiments.py#L74)), utilizing at most 2 cores.
  - `threads`, `dedicated-*`, and `native-block` spawn 1 OS thread per actor (up to 1,024 threads), utilizing **all 4 cores** in the Linux VM and all host cores on macOS.
- **Driver Tokio Runtime Overhead on Non-Tokio Runs ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs#L361-L366)):**
  - `main()` unconditionally starts a 2-worker multi-thread Tokio runtime for every variant, inflating process RSS and FD counts for `threads` and `native-*`.
- **Envelope & Reply Asymmetry:**
  - Default `async`/`threads`/`dedicated-*` allocate a type-erased `Box<dyn Envelope<A>>` per message plus framework oneshot/mpsc channels, while `native-*` sends unboxed `Probe` structs over `sync_channel` with `reply = bounded`. Additionally, `*-reply-*` variants are omitted from all multi-actor cases ([`design_experiments.py`](https://github.com/lambdaclass/spawned/blob/24d9fcd/benchmarks/design_experiments.py#L43)).

---

## 3. API-Preservation, Caveats & Gaps to a Production Backend

- **Honesty of disclosures:** [`EXPERIMENTS.md`](https://github.com/lambdaclass/spawned/blob/24d9fcd/benchmarks/EXPERIMENTS.md) and [`backend_experiments.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/tests/backend_experiments.rs) accurately characterize `dedicated-current` helper starvation on blocking handlers and `dedicated-shared` failure when the caller runtime drops.
- **Undisclosed experimental hooks hazards:**
  - [`rt/src/threads/mod.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/rt/src/threads/mod.rs#L37-L54) reads `SPAWNED_EXPERIMENT_RUNTIME` inside public [`threads::block_on`](https://github.com/lambdaclass/spawned/blob/24d9fcd/rt/src/threads/mod.rs#L37), affecting any caller of `threads::block_on` process-wide.
  - In [`concurrency/src/tasks/actor.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/src/tasks/actor.rs#L593-L599), `SPAWNED_EXPERIMENT_RUNTIME=shared` panics if [`Backend::Thread`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/src/tasks/actor.rs#L37) is spawned from a thread without an active Tokio context.
- **Gaps before `native-*` / `ready-*` could become a library backend:**
  1. Polling async `Handler<M>::handle` / `started` / `stopped` futures across `.await` points with a valid Tokio reactor context (`Handle::enter`).
  2. Supporting heterogeneous messages via type-erased `Envelope<A>` instead of a fixed `Probe` struct.
  3. Non-blocking `ActorRef::send` / `request_raw` semantics (bounded `sync_channel` blocking would deadlock cyclic actor graphs).
  4. Full lifecycle parity: `CancellationToken` select loop, `catch_unwind` panic isolation, `MonitorTable` cleanup, and `ExitReason` watch notifications.

---

## 4. Conditional Ranking of Designs

1. **Incorporate into production (if data confirms expected resource/latency wins):**
   - **True oneshot / bounded 1-slot reply channel for `threads` mode:** [`rt/src/threads/oneshot.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/rt/src/threads/oneshot.rs#L3) currently aliases unbounded `std::sync::mpsc::channel`. If `threads-reply-bounded` reduces allocation/latency vs `threads-reply-std` in `rpc1`/`alloc1`, replacing `rt::threads::oneshot` is a drop-in internal improvement with zero API impact.
   - **`dedicated-shared` for `Backend::Thread` (with safe fallback):** If `dedicated-shared` eliminates the 1,024-actor macOS FD exhaustion while matching `dedicated-default` throughput and keeping helper progress working, it is the best upgrade for `Backend::Thread`—provided it shares a process-wide fallback runtime when spawned outside a caller runtime.
2. **Candidate for a future `threads` pool backend (requires full lifecycle integration):**
   - **`ready-16` / `local-ready-16` (after fixing the wakeup race):** Only worth building as a pooled `threads::Actor` executor if 512–1,024 actor runs show lower memory/scheduling overhead than 1-thread-per-actor `threads` without severe `hotspot128` cold-tail regressions. (For async actors, Tokio's runtime in `async` is already a work-stealing ready queue.)
3. **Keep strictly as benchmark controls / specialized experiments:**
   - **`native-spin` / `native-hybrid`:** Useful only as lower-bound transport controls; `idle4` and >4 actor limits make them unsuitable as general backends.
   - **`sharded-1` / `sharded-16`:** O(actors) empty-queue scanning does not scale to large idle populations.
   - **`monoio-legacy` / `monoio-uring`:** Keep as isolated I/O controls; incompatible with `Send` futures and Tokio traits in existing `tasks::Actor` handlers.

---

## 5. Optional Follow-ups (High-Value Additions to the Matrix)

1. **Multiplexed Async I/O Control in [`io_designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/io_designs.rs):** Add a variant where 1–2 runtime threads multiplex all 16/128 connections via spawned tasks (`tokio::spawn` / `monoio::spawn_local`) to measure actual event-loop and `io_uring` scaling rather than 1 runtime per socket.
2. **Core-Matched Worker Count (`workers = 4`) & Unbounded/Pre-Sized Mailbox Control in [`designs.rs`](https://github.com/lambdaclass/spawned/blob/24d9fcd/concurrency/benches/designs.rs):** Run `async` and `ready-16` with `workers = 4` (matching the 4-CPU Linux container and multi-core `threads` concurrency) and `capacity >= messages` in `burst32` to separate scheduler efficiency from core-count and backpressure timestamp artifacts.


## Resolution

The atomic pending-count prototype was replaced with a mutex-protected ReadyGate shared with its bounded Loom model. Producer notification occurs after mailbox publication; worker lookahead and ownership release are serialized by the same gate. One buffered lookahead avoids speculative pending counts. The suggested move-after-send count update was not applied because a worker could receive before incrementing and underflow the counter. All earlier exploratory scheduler samples are excluded from final report graphs.
Multiplexed I/O controls were added. Monoio sync wake support is explicitly enabled for the cross-thread start signal; smoke validation caught and corrected its absence. Timing ends before runtime teardown. The one-runtime-per-connection controls remain labeled as such.
Bounded/unbounded burst admission differences, hotspot placement imbalance, worker-count versus total CPU-budget differences, driver runtime overhead, and envelope/lifecycle asymmetry are documented. Separate worker-count and pre-sized queue controls are planned alongside the primary matrix. Shared-pool lifetime is tested; a separate managed process-owned pool is included.
The recommendation to replace a public raw reply-channel alias is not automatically an API-compatible change (raw sender/receiver types can expose different traits and error types). Probe reply experiments are matched benchmark controls, and any eventual internal change requires a wrapper/compatibility review.


## Follow-up review

### Findings

#### 1. [`ReadyGate`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L7-L29) & `native-ready` in [`designs.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L170-L204)

* **Lost Wakeups (Resolved):** In [`NativeSender::send`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L191-L203), the message is published to `self.tx` before [`ReadyGate::notify`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L13-L18) acquires `self.0`. On the worker side ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L434-L442)), [`ReadyGate::finish`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L22-L28) holds that same gate mutex while polling lookahead (`mailbox.rx.try_recv()` into `mailbox.buffered`).
  * If [`notify`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L13-L18) wins the gate lock while a worker is active, `*scheduled` is already `true` and the subsequent [`finish`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L22-L28) is guaranteed to observe the already-sent item via `try_recv()`.
  * If [`finish`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L22-L28) wins the gate lock and sees an empty channel, it sets `*scheduled = false` before releasing the lock, so the waiting [`notify`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L13-L18) observes `*scheduled == false`, transitions it to `true`, and enqueues the actor. No wakeup can be lost.
* **Duplicates & Ownership (Resolved):** A ready token (`Some(id)`) is emitted only when `*scheduled` transitions `false -> true` in [`notify`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L13-L18) or remains `true` on non-empty lookahead in [`finish`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L22-L28). At most one runnable token per actor exists in the ready queue. Even if a newly scheduled token is popped by a second worker before the first worker finishes updating `mailbox.batch_hint` ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L443-L451)), the first worker holds `actor.mailbox` across the entire turn including [`finish`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L414-L451), serializing access to [`ReadyMailbox`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L175-L179) (`rx`, `buffered`, and `batch_hint`).
* **Lock Ordering & Deadlock Freedom (Resolved):**
  * Workers acquire and immediately release `ready_rxs` ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L411)), then lock `actor.mailbox` ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L414)), and inside [`finish`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs#L22-L28) lock `gate.0` and `tx.send`.
  * Producers complete `self.tx.send(probe)` before acquiring `gate.0` and calling `signal.ready.send`, and never lock `actor.mailbox` or `ready_rxs`.
  * Lock acquisition forms a strict DAG (`actor.mailbox` $\rightarrow$ `gate.0` $\rightarrow$ ready channel). Blocking on a full bounded mailbox (`mpsc::sync_channel`) occurs before `gate.0` is taken, so workers can always drain the mailbox.
* **Shutdown (Clean for Benchmark Lifecycle):** Shutdown ([`designs.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L798-L806)) runs only after all warmup and measured replies have been collected. Sending `None` once per `worker` across `txs[worker % txs.len()]` delivers one termination sentinel per worker in both `global` (`groups == 1`, shared `Arc<Mutex<Receiver>>`) and `local` (`groups == config.workers`, one queue per worker) scheduler modes.

#### 2. [`ready_gate_loom.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/tests/ready_gate_loom.rs#L1-L45)

* [`publish_racing_finish_retains_exactly_one_ready_token`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/tests/ready_gate_loom.rs#L6-L44) directly imports [`support/ready_gate.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/ready_gate.rs) and verifies the single-token and no-lost-wakeup invariants under 2-producer / 1-worker races.
* **Minor Modeling Difference (Non-blocker):** The test protects the mock mailbox `messages` with a single `loom::sync::Mutex` held by the worker from pop through [`g.finish`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/tests/ready_gate_loom.rs#L24-L27). In [`designs.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L192-L199), producer `mpsc::SyncSender::send` can interleave between the worker's batch loop and [`actor.gate.finish`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/designs.rs#L434-L442) while `actor.mailbox` is held. Because the test also runs producers before the worker acquires `m` in other schedules, the critical `notify` vs. `finish` gate interleavings are still exercised.

#### 3. [`io_mux.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/io_mux.rs#L1-L172)

* **Cross-Thread Wakeup & Timing Window (Resolved):** Enabling `monoio`'s `sync` feature in [`Cargo.toml`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/Cargo.toml#L10) activates thread-safe cross-thread waker support on Monoio's drivers, so firing the `futures::channel::oneshot` start gates from the main thread ([`io_mux.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/io_mux.rs#L147-L149)) reliably wakes tasks parked on `gate.await` ([`io_mux.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/io_mux.rs#L104)). Recording `finished.send(Instant::now())` inside the runtime block ([`io_mux.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/io_mux.rs#L86), [`io_mux.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/io_mux.rs#L125)) captures completion prior to runtime drop and socket teardown.

#### 4. Remaining Blockers vs. Disclosed Prototype Semantics

* **Real Remaining Blockers:** None for the benchmark and experiment scope.
* **Already-Disclosed Prototype Limitations (Not Blockers):** As documented in [`EXPERIMENTS.md`](https://github.com/lambdaclass/spawned/blob/aae6312/benchmarks/EXPERIMENTS.md#L26-L36), `native-ready` omits full actor lifecycle features (cancellation, panic isolation, timers, dynamic actor registration, async suspension, and cyclic bounded-send deadlock avoidance), `local-ready` uses static modulo placement without work stealing, and [`io_mux.rs`](https://github.com/lambdaclass/spawned/blob/aae6312/concurrency/benches/support/io_mux.rs) pairs a single-thread multiplexed client against a thread-per-connection blocking echo server and does not bridge Monoio's `!Send` futures to `Send` actor handlers.
