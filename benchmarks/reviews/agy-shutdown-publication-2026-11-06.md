# Agy review: shutdown publication race

Verdict: **APPROVE**, no blockers. Read-only source review of the close/drain fix and allocator-gated regression, before Claude's additional successful-admission assertion was applied. That assertion now ensures a changed allocation hook fails visibly. PR182 is merged; the full-actor branch forwards its turn policy through the same wrapper. Forced cancellation and user destructor panics are outside the awaited ordinary-exit guarantee.

## Transcript

**Verdict: APPROVE**

### Review Summary

The fix in [`actor.rs`](file:///private/tmp/spawned-shutdown-publication/concurrency/src/tasks/actor.rs#L597-L679) and the regression test in [`shutdown_publication.rs`](file:///private/tmp/spawned-shutdown-publication/concurrency/tests/shutdown_publication.rs#L1-L149) are sound, well-ordered, and free of blockers.

### Key Findings

1. **Return-Path Coverage & Lifecycle Semantics ([`run_actor`](file:///private/tmp/spawned-shutdown-publication/concurrency/src/tasks/actor.rs#L597-L612) / [`run_actor_lifecycle`](file:///private/tmp/spawned-shutdown-publication/concurrency/src/tasks/actor.rs#L614-L679))**
   - Extracting [`run_actor_lifecycle`](file:///private/tmp/spawned-shutdown-publication/concurrency/src/tasks/actor.rs#L614-L679) with `rx: &mut mpsc::Receiver<...>` ensures that every ordinary exit path—including a caught panic in `started()` (lines 622–627), pre-loop cancellation and `stopped()` normal/panic return (lines 629–636), and main-loop exit via handler panic, cancellation, or channel closure followed by `stopped()` (lines 640–678)—funnels through `rx.close()` and the drain loop before [`ActorRef::spawn`](file:///private/tmp/spawned-shutdown-publication/concurrency/src/tasks/actor.rs#L574-L577) publishes `Some(reason)` on `completion_tx`.
   - Envelopes drained in `while rx.recv().await.is_some() {}` are dropped immediately without calling `envelope.handle(...)`, preserving the contract that no queued handlers execute after stop while still dropping each [`MessageEnvelope::tx`](file:///private/tmp/spawned-shutdown-publication/concurrency/src/tasks/actor.rs#L84-L87) reply sender before completion is observed.

2. **Cleanup Ordering & Deadlock Freedom**
   - `actor`, `ctx` (which holds the actor's internal `mpsc::Sender`), and `_monitor_cleanup` are owned by [`run_actor_lifecycle`](file:///private/tmp/spawned-shutdown-publication/concurrency/src/tasks/actor.rs#L614-L620) and dropped before `rx.close()` and `rx.recv().await` run in [`run_actor`](file:///private/tmp/spawned-shutdown-publication/concurrency/src/tasks/actor.rs#L609-L610).
   - Because `rx.close()` closes admission on the Tokio `mpsc` channel, `rx.recv().await` does not wait for external [`ActorRef`](file:///private/tmp/spawned-shutdown-publication/concurrency/src/tasks/actor.rs#L391-L397) clones to drop; it only waits for producers that had already claimed a slot prior to `rx.close()` to finish linking/writing their slot, and then returns `None` (EOF). In-flight producers only perform non-blocking memory allocation and pointer writes, so awaiting EOF cannot deadlock ordinary shutdown.

3. **Test & `unsafe` Global Allocator Review ([`shutdown_publication.rs`](file:///private/tmp/spawned-shutdown-publication/concurrency/tests/shutdown_publication.rs#L19-L149))**
   - [`GateAllocator`](file:///private/tmp/spawned-shutdown-publication/concurrency/tests/shutdown_publication.rs#L19-L37) is scoped to its own integration test binary, delegates directly to `System.alloc` / `System.dealloc` with the exact `layout`, uses `const` TLS initialization with `PRODUCER.try_with(Cell::get).unwrap_or(false)` (avoiding allocator recursion or TLS destruction panics), and gates only once via `ARMED.swap(false, Ordering::SeqCst)` on `layout.size() > 256` (targeting Tokio's `mpsc` block allocation rather than the smaller `Box<MessageEnvelope>` / `oneshot` allocations, confirmed by `SENT > 0`).
   - The [`Cleanup`](file:///private/tmp/spawned-shutdown-publication/concurrency/tests/shutdown_publication.rs#L70-L76) drop guard releases `RELEASE` on unwind so the spinning producer thread cannot hang the test runner if an assertion fails early.
