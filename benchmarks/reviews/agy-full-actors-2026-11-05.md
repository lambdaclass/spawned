# Independent agy reviews: full actor backend study

Agy reviewed implementation and aggregated results/metadata read-only. Result
reviews did not independently recompute every raw-row aggregation. Full raw data
and hashes are retained, and report aggregates are separately checked. The initial
large-input Mac review returned reading narration without a verdict; the focused
Mac review below completed. No verdict is inferred from the incomplete attempt.

Implementation findings were corrected before measured source `9b22346`. The
follow-up found the general closed-completion status bug, fixed separately. The
first review describes an uncommitted, superseded candidate; its line references
are historical, not citations to current code.

## Editorial response

- Agreed: keep Tokio default, retain workload/platform-specific sharding research,
  profile causes, then test graph traffic and offered-rate saturation.
- Separate blocking-worker leasing as a resource/API track, not an explanation of
  the scheduler performance differences. The proposed implementation is untested.
- Both compared designs use N actor worker OS threads. Sharded runs have an
  additional observer runtime on the existing main thread, not extra actor worker
  threads; they do have more reactors. Reviewer wording about extra runtime
  threads must not be read as an unequal actor-worker budget.
- The measured changes do not identify which envelope, wakeup, placement or kernel
  component caused a gain/loss. Causal statements in reviews remain hypotheses
  pending profiles. Five run ranges are not confidence intervals.
- An idle4 run contains one latency observation and cannot estimate a tail
  distribution, regardless of the raw field being named p99.


- `actor-sharded-1` is a one-message turn with an explicit yield when backlog
  remains, not a no-batching control. Balanced closed-loop RPC usually has little
  per-actor backlog; policy attribution requires skew/burst/offered-rate evidence.
- The observed sleep duration does not identify timer slack as the exclusive
  cause; VM/OS scheduling and contention remain hypotheses without profiles.

## Initial superseded implementation review

## 1. Interface & Default Runtime Invariants

- **Public API preserved:** [`Backend`], [`Actor`], [`Handler<M>`], [`Context<A>`], [`ActorRef<A>`], and [`ActorStart`] signatures are unchanged.
- **Default runtime path:** Without `--features backend-experiments`, [`ActorRef::spawn`] and [`run_actor`] compile to the original behavior. With `backend-experiments` enabled and env vars unset, `Backend::Async` still dispatches via `rt::spawn(inner_future)` with `turn = None`, though [`ActorRef::spawn`] still queries `std::env::var("SPAWNED_EXPERIMENT_SCHEDULER")` on every spawn.

---

## 2. Actual Blockers & Correctness Bugs (with Concrete Interleavings)

### Blocker 1: `MessageTurn` records `self.began` on park instead of wakeup, breaking batching after any idle $\ge 50\,\mu\text{s}$ and yielding on empty mailboxes
In [`MessageTurn::parked`] and [`run_actor`]:
```rust
let result = rx.poll_recv(cx);
if result.is_pending() {
    if let Some(turn) = &mut turn {
        turn.parked(); // sets self.began = Instant::now() BEFORE sleeping
    }
}
```
When the actor later wakes up and `rx.poll_recv(cx)` returns `Poll::Ready(Some(msg))`, `self.began` is **not** reset.
- **Concrete Interleaving:**
  1. Actor $A$ (configured with fixed batch `SPAWNED_EXPERIMENT_BATCH=16`, `self.limit = 16`) has an empty mailbox at $t = 0$. `rx.poll_recv(cx)` returns `Poll::Pending`; `turn.parked()` sets `self.began = t_0` and `self.completed = 0`.
  2. Actor $A$ remains idle for $100\,\mu\text{s}$. At $t = 100\,\mu\text{s}$, 16 messages arrive in `rx`.
  3. Actor $A$ wakes, dequeues message #1 (`rx.poll_recv` returns `Ready`, so `parked()` does not run), and finishes `handle()` in $1\,\mu\text{s}$.
  4. [`turn.after_message(rx.is_empty()).await`] runs with `self.completed = 1`. Even though `1 < 16`, `self.began.elapsed()` is $101\,\mu\text{s} \ge 50\,\mu\text{s}$ because `self.began` measured the **idle sleep duration** rather than the active turn duration.
  5. Actor $A$ immediately calls `yield_now().await` after just **1 message** instead of batching 16.
  6. Furthermore, when `rx.is_empty()` is `true` (e.g., 1-message ping-pong spaced $>50\,\mu\text{s}$ apart, or adaptive mode where `limit == 1`), `after_message(true)` **still calls `yield_now().await`**, forcing a redundant scheduler round-trip before the loop can call `rx.poll_recv(cx)` to register the mailbox waker and park.

---

### Blocker 2: `stop()` cancellation is bypassed for a queued message when cancelled during `MessageTurn::after_message` yield
In [`run_actor`], the post-handler cancellation check ([line 731]) runs **before** `turn.after_message(rx.is_empty()).await` ([line 736]), and at the top of the next loop iteration `futures::future::select(recv, cancel)` ([line 715]) always polls `recv` **before** `cancel`.
- **Concrete Interleaving:**
  1. Actor $A$ on `spawned-shard-0` has $M_1, M_2$ queued in `rx`.
  2. Actor $A$ completes $M_1$. At [line 731], `cancellation_token.is_cancelled()` is `false`.
  3. At [line 736], `turn.after_message(false).await` calls `yield_now().await` and yields control of `spawned-shard-0`.
  4. Before Actor $A$ is re-polled, another task on `spawned-shard-0` (or another thread) calls `actor_a.child_handle().stop()`, cancelling `cancellation_token`.
  5. Actor $A$ resumes from `yield_now().await` and loops to [line 715]: `future::select(recv, cancel).await`.
  6. `future::select` polls its first argument (`recv`) first. Because $M_2$ is already in `rx`, `rx.poll_recv(cx)` returns `Poll::Ready(Some(M_2))` without polling `cancel`.
  7. Actor $A$ executes $M_2$ after `stop()` was already issued, violating the contract tested by [`stop_finishes_in_flight_handler_and_rejects_queued_work`].

---

### Blocker 3: Pool lease release races with `actor.join()`, worker teardown, and `REGISTRY` re-initialization
In [`ActorRef::spawn`] and [`Pool::spawn`]:
```rust
// ActorRef::spawn
let inner_future = async move {
    let reason = run_actor(...).await;
    let _ = completion_tx.send(Some(reason));
};
// Pool::spawn
self.senders[shard].send(Box::pin(async move {
    let _lease = lease;
    future.await;
}))
```
And in [`Pool::drop`]:
```rust
impl Drop for Pool {
    fn drop(&mut self) {
        self.senders.clear();
        let current = thread::current().id();
        for worker in self.workers.drain(..) {
            if worker.thread().id() != current {
                let _ = worker.join();
            }
        }
    }
}
```
This creates two concrete races when the last sharded actor exits:

1. **Interleaving A (`actor.join()` completes before `_lease` drops — config assertion panic or premature inspection):**
   - Worker `spawned-shard-0` executes `completion_tx.send(Some(reason))` inside `inner_future` ([actor.rs:588]) **before** `inner_future` returns and drops `_lease` ([experimental.rs:72-74]).
   - Observer thread $T_{\text{obs}}$ awaiting `actor.join()` wakes immediately on `completion_tx.send(...)` while `spawned-shard-0` is preempted before dropping `_lease`.
   - $T_{\text{obs}}$ considers the actor system idle and starts a new test/benchmark phase with a different `SPAWNED_EXPERIMENT_WORKERS`. `registry.upgrade()` in [`spawn_sharded`] still succeeds on the old `Pool` and panics at `assert_eq!(pool.senders.len(), count, "use one config per process")`.

2. **Interleaving B (`Weak::upgrade()` becomes `None` before `Pool::drop` runs, overlapping $2N$ shard runtimes and detaching the final worker):**
   - In `Arc::drop`, the strong count is decremented to `0` *before* `Pool::drop` executes. The moment `spawned-shard-0` decrements the strong count of `_lease` to `0`, `REGISTRY`'s `Weak<Pool>::upgrade()` returns `None` (and [`suspended_job_owns_pool_until_it_finishes`] exits its loop), even though:
     1. `spawned-shard-0` has not yet cleared `self.senders` or joined `spawned-shard-1..N-1`; and
     2. `spawned-shard-0` **skips `worker.join()` on itself** (`worker.thread().id() != current`), detaching its own `JoinHandle` while it is still inside `runtime.block_on(...)` and before `runtime.shutdown_background()` has run.
   - If $T_{\text{obs}}$ calls `spawn_sharded` during this window, `registry.upgrade()` is `None`, so `Pool::new(count)` spawns $N$ new shard threads and Tokio reactors **concurrently with the $N$ old shard threads/reactors still shutting down** (temporarily consuming $2N$ reactor FD sets and threads).
   - Also note: `spawned-shard-0` synchronously calls `worker.join()` on $N-1$ other OS threads *from inside an async task poll* on `spawned-shard-0`'s Tokio runtime.

---

### Blocker 4: Helper tasks (`tokio::spawn` / `rt::spawn`), `Context::monitor`, and `send_message_on` do not hold a `Pool` lease and bind to the wrong runtime when called externally
[`Pool::spawn`] only attaches `_lease: Arc<Pool>` to the top-level actor future sent over `self.senders[shard]`.
1. **In-handler helper tasks dropped on last-actor exit:**
   - Inside a sharded actor handler, `tokio::spawn` / `rt::spawn` (including [`Context::monitor`] and [`send_message_on`]) spawns tasks directly onto that shard's `current_thread` runtime **without** cloning `_lease`.
   - **Concrete Interleaving:** The last sharded actor spawns a helper task on `spawned-shard-0` (or a monitor task delivering `Down` to a `Backend::Thread` actor) and then stops. When the actor future finishes, `_lease` drops, `Pool::drop` clears `self.senders`, `rx.recv().await` on `spawned-shard-0` returns `None`, `runtime.block_on` exits immediately (because `block_on` only awaits the `rx.recv()` loop, not tasks spawned via `tokio::spawn`), and `runtime.shutdown_background()` aborts the in-flight helper task.
2. **External `actor.context().monitor(...)` / `send_message_on(actor.context(), ...)` killed when caller runtime drops:**
   - [`Context::from_ref`] is explicitly documented for setting up timers or listeners from outside the actor via `actor_ref.context()`.
   - **Concrete Interleaving:** Caller runtime `rt1` starts a sharded actor $A$, calls `send_message_on(A.context(), sleep(100ms), Msg)` or `A.context().monitor(&target)` (which calls [`rt::spawn`] on **`rt1`**, not on $A$'s shard), and then drops `rt1` as in [`actors_outlive_their_caller_runtime`]. Dropping `rt1` cancels the timer/monitor waiter task on `rt1` while leaving a stale `MonitorRef` entry in $A$'s `MonitorTable`; $A$ survives on its shard, but the timer or `Down` notification is silently lost. Calling `A.context().monitor(...)` or `send_message_on(...)` from a non-runtime OS thread panics outright inside `rt::spawn`.

---

### Blocker 5: Child `Backend::Blocking` (or `Backend::Thread` with `shared`) spawned from inside a sharded actor borrows the shard's `current_thread` runtime without a lease
- **Concrete Interleaving:**
  1. Sharded Actor $A$ on `spawned-shard-0` spawns a child actor $B$ with `Backend::Blocking` ([actor.rs:602-604]: `rt::spawn_blocking(move || rt::block_on(inner_future))`).
  2. $B$ runs on `spawned-shard-0`'s blocking pool and calls `Handle::current().block_on(inner_future)` using `spawned-shard-0`'s `current_thread` runtime handle, relying on `spawned-shard-0` to drive its reactor/timers. $B$ holds no `Arc<Pool>` lease.
  3. Actor $A$ stops and drops the last `Arc<Pool>` lease. `spawned-shard-0` exits `runtime.block_on` and calls `runtime.shutdown_background()`, shutting down the reactor and timer driver underneath the still-running `Backend::Blocking` actor $B$.

---

## 3. Evaluation of Resource Limit Tests ([backend_resource_limits.rs])

The resource tests in [backend_resource_limits.rs] **do not accurately characterize the reported limits**:

1. **`descriptor_limit_child` compares `sharded` (`Backend::Async`) against `Backend::Thread`, not against default `Backend::Async` ([lines 127-131]):**
   - Default `Backend::Async` (standard `tokio::spawn`) also reuses its existing runtime reactor, allocates 0 file descriptors per additional actor, and would pass `count = 128` after `EMFILE` identically to `sharded`.
   - In fact, `sharded` (`SPAWNED_EXPERIMENT_WORKERS=2`) consumes *more* baseline file descriptors than default `Backend::Async` because it initializes 2 extra `current_thread` Tokio runtimes (each with its own `kqueue`/`epoll` + waker FDs) in addition to the test's `current_thread` runtime.
2. **Pre-warming `seed` before exhausting FDs hides `sharded`'s startup `EMFILE` panic ([lines 133-145]):**
   - `seed` is started *before* the `/dev/null` loop drains all remaining file descriptors to `EMFILE` (`errno == 24`).
   - If `spawn_sharded` is first called (or re-initialized after an idle drop) when FDs are exhausted, [`Pool::new(count).expect("initialize experimental actor shards")`] **panics synchronously on the caller thread**, whereas `Backend::Thread` isolates the panic to the spawned thread and reports `ExitReason::Kill` via `wait_exit()`.
   - Note that partial initialization inside [`Pool::new`] *does* clean up already-spawned shard threads $0..k-1$ via `Pool::drop` before `expect(...)` panics.
3. **`SPAWNED_EXPERIMENT_RUNTIME=current` ([line 85]) is a dead configuration value:**
   - [`actor.rs:611-620`] only checks `if mode == "shared"`; `"current"` simply falls into `else { threads::block_on(inner_future) }` (the unmodified `Backend::Thread` path). Meanwhile, the `mode == "shared"` branch is untested here.
4. **Manual `/dev/null` exhaustion does not measure per-actor FD cost:**
   - Draining FDs to 0 with `File::open("/dev/null")` guarantees any operation needing $\ge 1$ FD fails at `count = 1` (including any sharded actor that opens a socket or file), while pure in-memory channel messaging on pre-allocated runtimes succeeds for any `count`.
5. **`persistent_blocking_actors_queue_until_a_worker_is_released` uses an immediate `try_recv()` ([lines 51-57]):**
   - `started.try_recv()` immediately after `start_with_backend(Backend::Blocking)` returns `Err(TryRecvError::Empty)` simply due to OS/thread-pool scheduling latency even when workers are idle; it does not wait a short timeout to prove the 3rd actor is genuinely blocked on pool capacity (`max_blocking_threads(2)`).

---

## 4. Documented Tradeoffs, Deadlock Characteristics & Benchmark Caveats

- **Self-join / cyclic request deadlocks (inherent to sequential actor model):**
  - Calling `ctx.actor_ref().join().await` or `wait_exit().await` inside `started()`, `handle()`, or `stopped()` deadlocks indefinitely because [`completion_tx.send(Some(reason))`] only runs after `run_actor` returns.
  - Self-requests (`ctx.request(...).await`) or cyclic requests ($A \leftrightarrow B$, whether on the same shard or across shards) deadlock until `DEFAULT_REQUEST_TIMEOUT` (5s).
  - Non-cyclic requests between two actors on the *same* shard ($A \to B$) do not deadlock because `A`'s `request` yields `Poll::Pending` to the shard's `current_thread` executor, which then polls `B` (and the shard's `rx.recv()` ingress task).
- **Sync blocking / `rt::block_on` on a shard worker:**
  - Calling [`rt::block_on`] (`Handle::current().block_on`) or `block_in_place` from inside a shard worker panics immediately because each shard uses a `current_thread` runtime.
  - Any synchronous blocking inside a handler stalls every actor, timer, I/O resource, and the new-actor ingress loop (`rx.recv()`) on that shard.
- **Static round-robin placement without work stealing ([`experimental.rs:66`]):**
  - Actors are permanently pinned via `next.fetch_add(1, Relaxed) % count`. Skewed actor lifetimes or skewed message rates across modulo classes can overload one shard while other shards sit idle.
- **Per-spawn env-var and mutex overhead (benchmark note):**
  - Every sharded `Actor::start()` calls `std::env::var` **three times** (`SPAWNED_EXPERIMENT_SCHEDULER`, `SPAWNED_EXPERIMENT_BATCH`, `SPAWNED_EXPERIMENT_WORKERS`) and locks the global `REGISTRY` `Mutex<Weak<Pool>>` ([`actor.rs:573-577`], [`actor.rs:640`], [`experimental.rs:95-107`]). In spawn-heavy benchmarks (or handlers that spawn child actors like `Nested`), this adds non-trivial process-env lock and mutex contention that default `rt::spawn` does not pay.
- **Test gating in [`full_actor_shards.rs`]:**
  - Unless `SPAWNED_EXPERIMENT_SCHEDULER=sharded` is set in the environment, two of the four tests in `full_actor_shards.rs` early-return as no-ops and the other two run against default Tokio.

*(Because you invoked `/plan` alongside the read-only review instructions, please let me know if you would like an implementation plan artifact to address any of the blockers above, or if this read-only review completes your request.)*


## Corrected implementation review (9b22346)

## 1. Resolution of Prior Review Findings

1. **Idle turn clock & empty-mailbox yield (Resolved):**
   - [`MessageTurn`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L648-L698) now initializes `began: None`, clears `began = None` in [`parked()`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L669-L675) when `rx.poll_recv(cx)` returns `Pending`, and starts the turn clock via [`resumed()`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L676-L678) (`self.began.get_or_insert_with(Instant::now)`) only when `rx.poll_recv(cx)` returns `Poll::Ready(Some(_))` ([`actor.rs:732-742`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L732-L742)).
   - [`MessageTurn::after_message`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L679-L697) immediately calls `self.parked()` and returns without calling `yield_now()` when `empty` (`rx.is_empty()`) is `true`. Covered by [`idle_time_is_excluded_and_an_empty_mailbox_needs_no_yield`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L1859-L1874).

2. **Cancellation during batch yield (Resolved):**
   - [`run_actor`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L760-L770) now rechecks `if cancellation_token.is_cancelled() { break; }` immediately after `turn.after_message(rx.is_empty()).await` before the next loop iteration can poll `rx.poll_recv(cx)`. Covered by the deterministic manual-poll regression test [`stop_during_batch_yield_does_not_execute_the_next_queued_message`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L1876-L1924).

3. **Process-owned shard pool, detached helpers, cross-backend monitors, and blocking children (Resolved):**
   - [`registry()`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L153-L156) now holds `OnceLock<Mutex<Option<Arc<Pool>>>>` instead of `Weak<Pool>`, so the last sharded actor stopping no longer drops the pool or calls `runtime.shutdown_background()` underneath detached `tokio::spawn` helpers, monitor waiter tasks, or `Backend::Blocking` children.
   - Non-`Async` backends (`Backend::Blocking` and `Backend::Thread`) acquire a conservative [`RuntimeLease`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L184-L197) via [`lease_if_initialized()`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L188-L197) in [`ActorRef::spawn`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L583-L590) whenever the experimental pool exists.
   - Covered by [`detached_helper_and_blocking_child_outlive_parent_stop`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/tests/full_actor_shards.rs#L349-L373) and [`monitor_helper_survives_the_last_sharded_actor_stopping`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/tests/full_actor_shards.rs#L403-L442).

4. **Resource limit tests & `SPAWNED_EXPERIMENT_RUNTIME=current` (Resolved):**
   - `SPAWNED_EXPERIMENT_RUNTIME=current` is implemented in [`threads::block_on`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/threads/mod.rs#L53-L57), giving `Backend::Thread` in `dedicated` mode the same `current_thread` Tokio runtime flavor as each shard worker.
   - [`descriptor_limit_is_scoped_to_children`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/tests/backend_resource_limits.rs#L82-L124) and [`descriptor_limit_child`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/tests/backend_resource_limits.rs#L128-L201) now test `"async"` (default `Backend::Async` control), `"sharded"` (warmed shard reactor reuse), `"dedicated"` (`Backend::Thread` with `current` failing to allocate a new reactor under `EMFILE`), and `"sharded-cold"` (cold shard initialization failing cleanly under `EMFILE` without poisoning [`registry()`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L153-L156) and recovering once descriptors are released).
   - [`persistent_blocking_actors_queue_until_a_worker_is_released`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/tests/backend_resource_limits.rs#L33-L78) deterministically awaits both initial `started()` hooks and verifies the 3rd actor remains unstarted across a 25ms timeout before releasing a worker.

---

## 2. Inspection of Shutdown, Registration & Lease Races

- **`actor.join()` vs. `shutdown_sharded()` lease release race — Sound:**
  - In [`ActorRef::spawn`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L588-L601), `completion_tx.send(Some(reason))` fires just before `inner_future` returns and drops either [`Pool::spawn`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L105-L110)'s `_lease` (for sharded `Backend::Async`) or `_borrowed_runtime` (for `Backend::Blocking` / `Backend::Thread`).
  - [`Pool::shutdown`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L120-L133) holds `self.state.lock()` and waits on `self.idle.wait_timeout(state, remaining)` while `state.active != 0` *before* calling `state.senders.take()`. Thus, an observer thread that calls `actor.join().await` and immediately invokes `shutdown_sharded()` will wait for the worker to finish polling the wrapper future and drop `Lease` before closing `senders` or shutting down the shard runtimes.
- **`Pool::spawn` vs. `Pool::shutdown` registration serialization — Sound:**
  - [`Pool::spawn`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L95-L104) checks `state.senders.as_ref()` and increments `state.active += 1` under the same `self.state` mutex acquisition.
  - Consequently, `Pool::shutdown` cannot observe `state.active == 0` and take `state.senders` while a `Pool::spawn` call is in the window between releasing `self.state` ([line 104](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L104)) and calling `tx.send(...)` ([line 107](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L107)). If `tx.send(...)` ever failed, dropping the unsent closure drops `lease` outside the lock and decrements `state.active`.
- **Self-join check vs. concurrent `shutdown()` — Sound:**
  - `worker_ids: Vec<thread::ThreadId>` is populated during [`Pool::new`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L85) and is immutable afterward. [`Pool::shutdown`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L114-L118) checks `self.worker_ids.contains(&thread::current().id())` before locking `self.state` or `self.workers`, so a call from a shard worker is rejected immediately even if another thread is concurrently joining workers in `self.workers.lock()`.
  - Two external threads calling `shutdown_sharded()` concurrently both synchronize on `self.workers.lock()` ([line 134](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L134)), so the second caller does not return `Ok(())` until the first caller has finished joining all shard threads.
- **Partial initialization failure in `Pool::new` — Sound:**
  - If thread spawn or `Builder::new_current_thread().enable_all().build()` fails on shard $k > 0$, [`Pool::drop`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L142-L151) takes `senders` and joins shards $0..k-1$, leaving `*registry` as `None` so subsequent initialization can succeed.

---

## 3. Remaining Contract Violations vs. Disclosed Experimental Differences

### Remaining Contract Violation: `exit_reason()`, `ChildHandle::is_alive()`, and `ChildHandle::wait_exit_blocking()` on spawn/startup failure

When [`rt::experimental::spawn_sharded(inner_future)`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L609-L614) returns `Err(error)` (either during cold initialization under FD exhaustion as in `sharded-cold`, or when `Actor::start()` is called after `shutdown_sharded()`), `inner_future` is dropped without ever running:
```rust
if let Err(error) = rt::experimental::spawn_sharded(inner_future) {
    actor_ref.cancellation_token.cancel();
    drop(MonitorCleanup(actor_ref.monitors.clone()));
    tracing::error!("Experimental shard startup failed: {error}");
}
return actor_ref;
```
Because `inner_future` owned `completion_tx` (initialized via `watch::channel(None)` at [`actor.rs:557`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L557)), dropping `inner_future` closes `completion_tx` while the stored value in `completion_rx` is still `None` (the same also happens for `Backend::Thread` when `threads::block_on` panics at `build().unwrap()` before polling `inner_future`, as in `dedicated` under `EMFILE`).

While [`ActorRef::wait_exit`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L501-L511) and [`ChildHandle::wait_exit_async`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/child_handle.rs#L162-L171) treat `rx.changed().await.is_err()` as `ExitReason::Kill`, the synchronous status and blocking-wait methods only inspect `rx.borrow().clone()`:
- [`ActorRef::exit_reason`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/tasks/actor.rs#L496-L498) (`self.completion_rx.borrow().clone()`) returns `None` permanently—even **after** `actor.wait_exit().await` has returned `ExitReason::Kill`.
- [`ChildHandle::exit_reason`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/child_handle.rs#L121-L126) returns `None` permanently, and [`ChildHandle::is_alive`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/child_handle.rs#L116-L118) (`self.exit_reason().is_none()`) returns **`true` permanently** for an actor that failed to start and has already terminated with `ExitReason::Kill`.
- [`ChildHandle::wait_exit_blocking`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/child_handle.rs#L212-L242) checks `if let Some(reason) = rx.borrow().clone() { return reason; }` for its fast path ([line 215](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/child_handle.rs#L215)). Because `rx.borrow()` remains `None`, the fast path is bypassed even after `wait_exit_async().await` has completed:
  - Calling `handle.wait_exit_blocking()` on a `current_thread` runtime (after `handle.wait_exit_async().await` already returned `ExitReason::Kill`) **panics** at [`child_handle.rs:234`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/child_handle.rs#L234) instead of taking the already-exited fast path.
  - Calling `handle.wait_exit_blocking()` from a non-runtime thread while FDs are still exhausted falls through to `spawned_rt::threads::block_on(wait)` ([`child_handle.rs:224`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/src/child_handle.rs#L224)), which attempts `Runtime::new().unwrap()` and **panics with `EMFILE`**.

*(Note: Either sending `Some(ExitReason::Kill)` on `completion_tx` before dropping it on startup failure, or falling back to `rx.has_changed().is_err().then_some(ExitReason::Kill)` in `exit_reason()`, would make `exit_reason()`, `is_alive()`, and `wait_exit_blocking()`'s fast path consistent with `wait_exit()`.)*

---

### Minor Edge Cases in Private `shutdown_sharded()` Lifecycle

These do not affect normal benchmark or test runs when following the documented shutdown sequence ("stop/join all actors, await important helpers, close registration and join workers"), but are worth noting for completeness:
1. **`shutdown_sharded()` before first sharded spawn is a no-op rather than terminal:**
   - [`shutdown_sharded()`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L201-L207) only calls `pool.shutdown()` if `registry()` is already `Some(pool)`. If called when `*registry` is still `None`, it returns `Ok(())` without preventing a later `spawn_sharded()` call from initializing the pool.
2. **Pre-pool `Backend::Thread` actors using a shard monitor waiter:**
   - [`lease_if_initialized()`](https://github.com/lambdaclass/spawned/blob/9b22346/rt/src/tasks/experimental.rs#L188-L197) only tracks non-`Async` actors started *after* `Pool` is initialized. If a `Backend::Thread` actor is started *before* the first sharded actor (so `lease_if_initialized()` returned `None`), and a sharded actor later installs a monitor on its behalf (spawning the monitor waiter onto the shard runtime), `state.active` will return to `0` as soon as the sharded actor stops, even though the pre-pool `Backend::Thread` actor is still alive. Calling `shutdown_sharded()` without also stopping that `Backend::Thread` actor is already outside `shutdown_sharded()`'s documented contract ("stop/join all actors first").

---

### Properly Disclosed Experimental Differences (Non-Blockers)

The following behaviors are accurately disclosed in [full-actor-shards.md](https://github.com/lambdaclass/spawned/blob/9b22346/docs/research/full-actor-shards.md) and code comments, and are **not** blockers for the experiment:
- **Process-owned pool & terminal private shutdown ([`full-actor-shards.md:24-32`](https://github.com/lambdaclass/spawned/blob/9b22346/docs/research/full-actor-shards.md#L24-L32)):** Shard workers live for the process (or until explicit `shutdown_sharded()`), with one fixed configuration per process.
- **External `actor_ref.context()` helpers attach to the caller's runtime ([`full-actor-shards.md:35-37`](https://github.com/lambdaclass/spawned/blob/9b22346/docs/research/full-actor-shards.md#L35-L37)):** Calling `actor_ref.context().monitor(...)` or `send_message_on(actor_ref.context(), ...)` from outside the actor spawns onto `Handle::current()` at the call site rather than relocating to the actor's shard.
- **No `block_in_place` / synchronous blocking stalls the shard ([`full-actor-shards.md:37-40`](https://github.com/lambdaclass/spawned/blob/9b22346/docs/research/full-actor-shards.md#L37-L40)):** Each shard uses a `current_thread` runtime and static round-robin assignment without work stealing.
- **Parallel vs. serial suite behavior ([`full-actor-shards.md:81-91`](https://github.com/lambdaclass/spawned/blob/9b22346/docs/research/full-actor-shards.md#L81-L91)):** Shared 2-worker contention in the parallel unit suite and round-robin separation in `badly_behaved_thread_non_blocking` under serial execution are explicitly documented, with deterministic same-shard and cross-shard tests ([`blocking_handler_stalls_its_shard_while_another_shard_progresses`](https://github.com/lambdaclass/spawned/blob/9b22346/concurrency/tests/full_actor_shards.rs#L247-L310)) added alongside the unchanged original test.


## Completed macOS results review

### 1. Verdict on Interpretations & Overclaim Audit

* **Keep multi-worker Tokio (`async`) as default — Supported, with caveats:**
  Across general multi-actor workloads (`active1024_p256`, `active128_p128`, `hotspot128`), standard multi-worker Tokio (`async` and `actor-tokio-*`) matches or outperforms static sharding (`actor-sharded-*`) without requiring static placement or extra runtime threads. However, `actor-sharded-*` shows ~2× lower p99 latency in `blocking128` (`mac2`: 8.79–10.54 ms vs 18.97–19.31 ms; `mac4`: 4.45–4.54 ms vs 9.77–9.83 ms) and higher single-actor throughput in `mac2` `alloc1`/`rpc1` (though min/max ranges overlap heavily). Keeping Tokio as the default is justified for ergonomics and high-actor-count scaling, provided we do not overclaim universal dominance in blocking or single-actor micro-cases.
* **Native-loop gains fail to reproduce for sharded full actors at 1024 actors — Strongly Supported:**
  In `active1024_p256`, `actor-sharded-{1,16,adaptive}` clearly trail `async` and `actor-tokio-*` in both `mac2` and `mac4` with non-overlapping or near-non-overlapping ranges:
  * `mac2`: sharded medians are **351.9k–373.9k** replies/s (p99 **1.43–1.56 ms**) vs `async` **489.0k** (p99 **0.78 ms**) and `actor-tokio-16` **493.8k** (p99 **0.79 ms**).
  * `mac4`: sharded medians are **508.9k–536.2k** replies/s (p99 **1.06–1.11 ms**) vs `async` **587.2k** (p99 **0.69 ms**) and `actor-tokio-adaptive` **614.1k** (p99 **0.66 ms**).
  Full-actor envelope, unbounded mailbox, reply channel, and cross-runtime wakeup costs erase any raw loop advantage under static placement at 1024 actors.
* **Batching on normal Tokio (`actor-tokio-{16,adaptive}` vs `actor-tokio-1` / `async`) helps some cells but is not universal — Supported:**
  * *Helps throughput:* `hotspot128` (`mac2`: `actor-tokio-16` **339.0k** vs `actor-tokio-1` **220.1k**; `mac4`: **379.2k** vs **217.7k**). However, compare against unyielded `async` (**347.9k** in `mac2`, **402.5k** in `mac4`): `actor-tokio-1` artificially degrades throughput by forcing a yield after every handler, and batching (`16`/`adaptive`) mostly recovers that self-inflicted yield penalty—while inflating `mac2` `hotspot128` p99 to **4.29 ms** (`actor-tokio-16`) vs **0.72 ms** (`actor-tokio-1`) and **2.51 ms** (`async`).
  * *Helps or ties:* `mac2` `active1024_p256` (`actor-tokio-16` **493.8k** vs `actor-tokio-1` **409.5k**, but essentially tied with `async` **489.0k**).
  * *Neutral or noisy:* `active128_p128` (`mac2` `actor-tokio-1` **482.2k** beats `actor-tokio-16` **446.8k**; `mac4` variants cluster around **584.6k–601.6k** with overlapping ranges), `async_wait128`, `blocking128`, and `tcp128`.
* **TCP throughput is similar enough to require caution — Strongly Supported:**
  In `tcp128`, all seven variants cluster within **149.3k–157.6k** replies/s on `mac2` and **142.9k–155.5k** on `mac4`, with wide overlapping min/max spans (e.g., `mac4` `actor-tokio-adaptive` min **119.7k** to max **160.6k**). Note also that `mac4` TCP throughput is slightly *lower* than `mac2` across all variants, pointing to kernel loopback/socket or unpinned producer/echo-server thread contention rather than actor scheduler limits.
* **Challenging Overclaims:**
  * *Process-global CPU & RSS confounds:* `cpu_s` and `rss_bytes` measure the whole process (including startup, multi-thread producers, the native TCP echo server, and the extra current-thread observer runtime in `sharded`). Small RSS differences (~100–200 KB in `idle4`/`blocking128`) or short-run `cpu_s` differences (`alloc1` 0.24–0.43s) cannot be attributed purely to actor runtime overhead.
  * *`idle4` throughput is meaningless:* `idle4` measures a single request after a 1s sleep (`cpu_s` = 0.0); `completed_per_s` (**196–659**/s) is an artifact of dividing 1 request by timer/harness window jitter. Only `completion_p99_ns` (~18.4–23.6 µs across variants) is interpretable.
  * *`blocking128` sleep duration & pool semantics:* Throughput is flat (~15.0k/s on `mac2`, ~29.7k/s on `mac4`) because it is timer-bound by sleep duration and concurrency, while the 2× p99 gap reflects how blocking work is scheduled/queued across N runtimes vs a shared pool.

---

### 2. Three Strongest Methodological Cautions

1. **Closed-Loop Traffic Masks Queueing & Conflates Latency with Throughput:** Producers wait for replies before issuing new work. Under contention (`hotspot128`, `active1024_p256`), closed-loop backpressure caps mailbox depth and hides open-loop tail-latency blowups and producer lag.
2. **Unpinned Multi-Component Contention on macOS ARM64:** With no core pinning (P/E core scheduling on Apple Silicon), producer threads, the native TCP echo server, N actor workers, and the `sharded` observer thread compete for cores within the same process, confounding scheduler comparisons (especially visible in `mac4` `tcp128` regression and wide `alloc1`/`rpc1` min/max spreads at $n=5$).
3. **Whole-Process Short-Run Resource Accounting:** Short non-stationary runs ($n=5$, sub-second to a few seconds of CPU time) mix initialization, allocator warmup, and harness threads into `cpu_s` and peak `rss_bytes`.

---

### 3. Ranked Next Experiments & Decisions Informed

1. **CPU & Wakeup Profiling (`active1024_p256` and `hotspot128`)**
   * *Missing evidence:* Whether `actor-sharded-*` loses at 1024 actors due to cross-thread `kqueue`/unpark syscalls, observer hop overhead, or static load imbalance across the N single-threaded runtimes.
   * *Decision informed:* Whether to abandon sharded current-thread runtimes altogether or fix a specific wakeup/observer bottleneck.
2. **Actor-to-Actor Chain / Fanout Topology**
   * *Missing evidence:* Current benchmarks only test external producer $\rightarrow$ actor $\rightarrow$ producer reply (`driver_policy` hops). They do not measure internal actor-to-actor messaging, where shard-local affinity could avoid cross-thread wakeups or work-stealing could win on dynamic fanout.
   * *Decision informed:* Whether static sharding or locality hints have any architectural value for multi-hop actor pipelines.
3. **Fixed-Rate Open-Loop Saturation & Producer-Lag Tests (paired with Per-Active-Handler Blocking-Worker Leasing)**
   * *Missing evidence:* Open-loop latency-vs-offered-load curves under unbounded mailboxes, and whether leasing blocking workers per active handler eliminates Tokio's 2× `blocking128` p99 penalty without persistent blocking-thread pools.
   * *Decision informed:* Default yield/batching policy under real queue pressure and the blocking-execution model for Spawned actors.

*(Note: All conclusions above are strictly scoped to macOS (`mac2`/`mac4` in [spawned-full-actors-mac-summary.csv](../validation/full-actors-2026-11-05/spawned-full-actors-mac-summary.csv) and [spawned-full-actors-mac-meta.json](../validation/full-actors-2026-11-05/spawned-full-actors-mac-meta.json)); Linux results are still pending and no Linux claims are made.)*


## Completed Linux results and priorities review

## 1. Challenge of Proposed Conclusions

1. **Keep Tokio default; preserve opt-in sharded research; no cross-OS rate comparisons (Supported with qualification):**
   Keeping multi-thread Tokio as default is well-supported. While Linux sharded variants win `active1024_p256`, `tcp128`, and `async_wait128`—unlike macOS 1,024-actor regressions—sharding is **not** a uniform win even on Linux: `async` beats `actor-sharded-16` in `linux2` `hotspot128` (344.7k/s vs. 313.6k/s), `linux4` `active128_p128` (1.744M/s vs. 1.477M/s), and `linux4` `blocking128` (1,736/s vs. 1,403/s). Because runs execute inside a 4-vCPU OrbStack aarch64 VM (`Linux-7.2.12-orbstack`), absolute rates cannot be compared across OS kernels.
2. **`active1024_p256` sharded gains as configuration observations, not general wins (Supported; batching claim needs tightening):**
   The cited `async` vs. `actor-sharded-16` medians in `linux2` (826.9k/s, p99 0.918ms vs. 999.4k/s, p99 0.503ms) and `linux4` (1,029.0k/s, p99 2.106ms vs. 1,179.4k/s, p99 0.542ms) are exact. However, `actor-sharded-1` (no batching) actually achieves the highest median throughput and lowest p99 in both (`linux2`: 1,027.5k/s, p99 0.316ms; `linux4`: 1,429.2k/s, p99 0.407ms), and 5-run min/max intervals overlap (`linux4` `sharded-16` min 979.8k/s < `async` max 1,071.2k/s). Furthermore, adding explicit `yield_now` turns on multi-thread Tokio (`actor-tokio-16`/`adaptive`) degrades `linux4` `active1024_p256` (~850k/s, p99 ~2.5ms) relative to plain `async`.
3. **Hotspot cold tails and lack of fairness guarantees (Supported):**
   In `linux4` `hotspot128`, `actor-sharded-16` improves global completion p99 over `async` (1.053ms vs. 1.465ms) because global p99 is dominated by the hot actor, yet its `cold_p99` is **2.36× worse** (1.305ms vs. 0.553ms; `sharded-1` is 1.285ms). Static round-robin placement traps cold actors behind the hot actor on a single-threaded shard without work stealing. Conversely, in `linux2` `hotspot128`, `async` beats `sharded-16` on throughput (344.7k/s vs. 313.6k/s) and global p99 (1.942ms vs. 2.631ms) while trailing on `cold_p99` (0.631ms vs. 0.487ms). Global p99 never certifies per-actor fairness.
4. **TCP and timer (`async_wait128`) differences and confounds (Supported; mixed tails):**
   `actor-sharded-16` leads `async` in `tcp128` throughput (`linux2`: 349.2k/s vs. 273.8k/s; `linux4`: 377.1k/s vs. 301.8k/s), though `linux4` completion p99 is slightly worse for `sharded-16` (2.050ms vs. 2.002ms) despite lower `handler_p99` (0.818ms vs. 1.488ms), and `actor-sharded-1` is fastest overall (370.2k/s and 395.6k/s). In `async_wait128`, sharded variants lead (~41.2k–41.8k/s, handler p99 ~4.0–4.8ms vs. `async` 31.2k–37.5k/s, ~5.5–5.6ms). However, $N$ single-thread runtimes instantiate $N$ separate epoll/timer wheels, and in `linux4` the 4 actor workers plus observer, producer, and TCP echo server threads oversubscribe the 4-vCPU cgroup limit.
5. **`blocking128` timer slack vs. CPU speed/preemption (Supported):**
   Every variant in `linux2` and `linux4` records `handler_p99` of ~3.69–3.70ms for a requested 100µs sleep due to VM/OS timer slack. Differences in `completion_p99` (~216–225ms sharded vs. ~281–456ms Tokio/async) reflect closed-loop queuing of ~3.7ms sleeps, not CPU speed or preemption.

---

## 2. Strongest Methodological Gaps

* **4-vCPU VM oversubscription & no affinity:** In `linux4`, 4 worker threads + current-thread observer + closed-loop producers + TCP server exceed `cpuset.cpus.effective: 0-3` without core pinning.
* **Closed-loop coordinated omission:** Producers self-throttle when replies slow down, masking saturation queueing and conflating throughput with latency.
* **Short, high-variance runs ($n=5$):** Extreme min/max spreads (e.g., `linux2` `alloc1` `sharded-1` 34.1k–835.8k/s; `rpc1` `sharded-adaptive` 39.4k–740.1k/s; `idle4` one-shot noise) prevent statistical confidence.
* **Single-hop topology:** Zero actor-to-actor traffic hides cross-shard wakeups and cache-line bouncing.
* **No native-loop controls or profiling counters:** Cannot separate Spawned mailbox/envelope costs from Tokio scheduler/reactor behavior.

---

## 3. Assessment of Research Priorities ([spawned-next-experiments.md])

Ranking **per-active-handler blocking-worker leasing** #1 conflates a **deterministic resource-capacity fix** with **empirical performance investigation**. Persistent `Backend::Blocking` starvation at 1,024 idle actors is an architectural contract limit known a priori; fixing it by migrating actor state across blocking pool threads introduces nontrivial TLS, lifecycle, and cancellation hazards without answering why `async` and `sharded` diverge. Treat blocking-worker leasing as a separate correctness/API design track, and reorder the performance experiments as follows:

1. **CPU, Wakeup, and Reactor Profiling on Quiet 4+ Core Hardware (`active1024_p256`, `hotspot128`, `tcp128`):**
   * *Decision informed:* Whether to optimize shared-runtime bottlenecks (mpsc wakeups, work-stealing contention, single-reactor epoll/timer locking, or reply allocations) inside default Tokio, or invest in multi-reactor sharding.
2. **Actor-to-Actor Topologies (Chains/Fan-Out) with Controlled Placement & Hot-Shard Skew:**
   * *Decision informed:* Whether opt-in static sharding remains net-positive once cross-shard hops and hot-shard head-of-line blocking (seen in `hotspot128` `cold_p99`) dominate, or whether sharding must be restricted/paired with placement APIs.
3. **Fixed Offered-Rate (Open-Loop) Load Sweep with Coordinated-Omission Accounting:**
   * *Decision informed:* Whether explicit handler-turn yield policies (`1`/`16`/`adaptive`) provide any real tail-latency protection below or near saturation, or should be removed (given `actor-sharded-1` and plain `async` frequently beat `16`/`adaptive`).


## Completion status fix review

**Verdict:** No remaining blockers or bugs found in the patch.

#### Key Checks
1. **Ordering & Race Safety (`poll_exit_reason`)**:
   - Sampling `let closed = rx.has_changed().is_err();` **before** `rx.borrow()` prevents a TOCTOU race where a sender publishes `Some(Normal | Panic)` and drops between a `None` borrow and a subsequent closure check.
   - If `has_changed().is_err()` observes closure, the sender drop has already occurred, establishing happens-before ordering so `rx.borrow()` is guaranteed to see any final `ExitReason` published prior to drop.
   - If the sender drops after `has_changed()` returns `Ok(_)` without publishing a reason, `poll_exit_reason` returns `None` (linearizable immediately before the drop), and the next poll/wait observes closure.

2. **Preserving Published `Normal` / `Panic`**:
   - `rx.borrow().clone().or_else(|| closed.then_some(ExitReason::Kill))` always prioritizes a published `Some(reason)` over channel closure, falling back to `ExitReason::Kill` only when the value is `None` and the sender is closed.
   - Directly covered by `closure_preserves_the_final_published_reason`.

3. **Blocking Fast Path Without a Runtime / Inside Current-Thread Runtime**:
   - Updating `wait_for_tasks_exit_blocking` to check `poll_exit_reason(&rx)` ensures an already-closed channel with `None` returns `ExitReason::Kill` immediately on the fast path without needing a runtime or panicking inside an active `current_thread` runtime.

4. **API Consistency & Test Coverage**:
   - `ChildHandle::exit_reason` (and transitive `is_alive`), `tasks::ActorRef::exit_reason`, and `threads::ActorRef::exit_reason` all share `poll_exit_reason`.
   - `closed_without_reason_is_dead_and_all_waits_agree` exercises both `ChildHandle::from_tasks` and `ChildHandle::from_threads` across `is_alive`, `exit_reason`, `wait_exit_async`, and `wait_exit_blocking`.
