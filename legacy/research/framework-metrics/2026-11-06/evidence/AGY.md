**Verdict: No blockers found.**

### Verification by Focus Area

1. **Counters & Gauges (Underflow, Panic, and Abort Safety)**
   - **Mailbox gauge (`mailbox_depth` & per-actor `Mailbox`)**: Incremented in [`Mailbox::enqueued`](../../../../../concurrency/src/metrics.rs#L179-L188) and decremented at most once via [`MessageObservation::release`](../../../../../concurrency/src/metrics.rs#L207-L211), which clears `self.queued = false`. Neither [`MessageObservation::begin`](../../../../../concurrency/src/metrics.rs#L212-L225) nor [`MessageObservation::reject`](../../../../../concurrency/src/metrics.rs#L226-L228) can double-decrement on drop.
   - **Handler gauge & outcomes (`handlers_active`, completed/panicked/aborted)**: [`MessageObservation::begin`](../../../../../concurrency/src/metrics.rs#L212-L225) transitions ownership to [`HandlerObservation`](../../../../../concurrency/src/metrics.rs#L241-L265) before handler execution. Panics in both tasks (`catch_unwind().await`) and threads (`catch_unwind`) are caught and passed to `observation.finish(result.is_err())`. If an actor task is aborted mid-handler by the runtime across `.await`, [`HandlerObservation::drop`](../../../../../concurrency/src/metrics.rs#L255-L264) runs with `completed == false`, decrementing `handlers_active`, incrementing `handlers_aborted_total`, and recording `HANDLER_DURATION`.
   - **Actor lifecycle (`actors_active` & exit totals)**: [`ActorCleanup`](../../../../../concurrency/src/actor_cleanup.rs#L12-L65) constructs [`ActorObservation`](../../../../../concurrency/src/metrics.rs#L266-L296) before dispatch and calls `self.metrics.finish(&reason)` in `Drop`. The idempotent `finished` guard in [`ActorObservation::finish`](../../../../../concurrency/src/metrics.rs#L275-L288) prevents double-decrement when [`ActorObservation::drop`](../../../../../concurrency/src/metrics.rs#L290-L296) subsequently runs, while still guaranteeing fallback `ExitReason::Kill` accounting if dropped standalone.

2. **Cancellation and Rejected / Discarded Sends**
   - **Rejected sends**: In both [`tasks::actor::enqueue`](../../../../../concurrency/src/tasks/actor.rs#L150-L180) and [`threads::actor::enqueue`](../../../../../concurrency/src/threads/actor.rs#L103-L135), channel `SendError` extracts the envelope and calls `.reject()`, taking the `MessageObservation`, releasing the in-flight gauge reservation, and incrementing `messages_rejected_total` without incrementing `messages_enqueued_total`.
   - **Discarded messages**: Queued envelopes dropped without calling `begin()` or `reject()` (including channel teardown or `discarded = Some(envelope)` on post-receive cancellation in [`threads::actor::run_actor_receiving`](../../../../../concurrency/src/threads/actor.rs#L857-L860)) release the gauge reservation in [`MessageObservation::drop`](../../../../../concurrency/src/metrics.rs#L230-L240) and increment `messages_discarded_total`.

3. **Request Accounting & `Response<T>` Guard Correctness**
   - **Direct `request` functions/methods**: In both [`tasks::actor`](../../../../../concurrency/src/tasks/actor.rs#L320-L330) and [`threads::actor`](../../../../../concurrency/src/threads/actor.rs#L281-L291), `RequestObservation` is created before `request_raw` and `request_raw` errors are matched rather than early-returned via `?`, ensuring rejected requests record `requests_actor_stopped_total` and decrement `requests_active`. Dropping a pending async `request` future at `.await` drops `RequestObservation` with `finished == false`, decrementing `requests_active` and incrementing `requests_cancelled_total`.
   - **`Response<T>` state machine**:
     - [`Response::from_with_timeout`](../../../../../concurrency/src/response.rs#L129-L163) and [`Response::from`](../../../../../concurrency/src/response.rs#L166-L185) finish the observation immediately on `Err` (storing `None`) and store `Some(observation)` on `Ok(rx)`.
     - [`Response::poll`](../../../../../concurrency/src/response.rs#L188-L227) consumes the observation via `this.1.take()` on `Poll::Ready`, recording the terminal outcome once.
     - Dropping an uncompleted `Response<T>` (including on panic if `unwrap`/`expect`/`map` is called on a pending response) drops `Some(observation)`, decrementing `requests_active` and recording `requests_cancelled_total`.
     - [`Response::ready`](../../../../../concurrency/src/response.rs#L57-L63) stores `None`, avoiding double-counting when wrapping pre-observed blocking results in threads mode.

4. **Sampling & Prometheus Rendering**
   - [`sample`](../../../../../concurrency/src/metrics.rs#L31-L34) guards `every != 0` before calling `fetch_add(1, Relaxed).is_multiple_of(every)`, avoiding division/modulo by zero and skipping `Instant::now()` when disabled.
   - [`render_prometheus`](../../../../../concurrency/src/metrics.rs#L139-L176) derives cumulative bucket counts and `_count` from the same bucket snapshot array, preventing `+Inf` / `_count` skew under concurrent updates.

### Uncertainties / Uninspected Boundaries

- **`crate::instrumentation` shim & `Cargo.toml` (not in supplied code)**: `actor_cleanup.rs`, `tasks/actor.rs`, and `threads/actor.rs` reference `crate::instrumentation::{ActorObservation, Mailbox, RequestObservation}` outside `#[cfg(feature = "metrics")]`. This assumes `crate::instrumentation` provides no-op equivalents when `metrics` is disabled and that `metrics` is not in `default` features in `Cargo.toml`.
- **Threads-mode protocol codegen callers of `Response::ready` (not in supplied code)**: Since `Response::ready` attaches `None` for `RequestObservation`, threads-mode protocol blanket implementations must call `ActorRef::request` / `Context::request` / `threads::actor::request` (rather than `request_raw`) if those protocol requests are expected to be counted in `requests_*`.
- **MSRV**: [`u64::is_multiple_of`](../../../../../concurrency/src/metrics.rs#L33) requires Rust $\ge 1.87.0$.


Maintainer scope checks: the disabled shim is zero-sized/no-op, `metrics` is not
in any default feature set, synchronous protocol codegen calls `self.request`
(which is observed once), and the CI/compiler floor is Rust 1.93. The first broad
CLI attempt ended with inspection preambles, so it is not counted as approval.
This focused supplied-code review did not execute tests. Passing test/Clippy logs
are independent fede execution evidence.
