# Deferred replies: client mapping

Companion to the [deferred replies design](deferred-replies.md). This note
rewrites four hand-built patterns from ethrex and ethlambda using the proposed
v1 API, which does not exist yet. The rewrites are pseudo-Rust. Line counts
are approximate and count only the logic shown, not tests or logging.

Sources:

- ethrex [`a132ada12`](https://github.com/lambdaclass/ethrex/tree/a132ada127f7949d229391f0db07b13c71eda3f3),
  on spawned 0.5.0; paths are relative to `crates/networking/p2p/`.
- ethlambda [`57a6e13b`](https://github.com/lambdaclass/ethlambda/tree/57a6e13bf31463946838ed9677c29c7eb801a6d3);
  paths are relative to `crates/`.

| Pattern | Before (approx. lines) | After | Bugs and hazards removed |
|---|---|---|---|
| ethrex `outgoing_request` / `current_requests` / `request_timeout` | ~110 | ~45 | leak on caller cancellation; duplicate-id overwrite; opaque `RecvError` on teardown; late-response error spam; unbounded map |
| ethrex `get_best_peer` 10 ms polling (7 sites) | ~7 × 14 + selection | a deferred handler plus ~30 lines in the peer table | about 100 actor calls per second per idle loop; up to 10 ms extra latency; a permit taken before work exists |
| ethlambda gossip verdicts | ~190 | ~70 | a hung check never yields a verdict; a verdict silently lost when the actor is gone; hand-written `catch_unwind`; a detached blocking task |
| ethlambda aggregation session | ~200 | ~70 | 2 s join inside a handler; unfenced `AggregationDone`; a worker panic produces no `Done`; hand fencing of a stale deadline timer |

## 1. ethrex: request/response over a peer connection

### Before

The caller side is `PeerConnection::outgoing_request`
(`rlpx/connection/server.rs:245-272`):

1. It creates a oneshot.
2. It sends the `Arc`-wrapped sender to the connection actor.
3. It awaits under `tokio::time::timeout`.
4. On timeout, it sends `request_timeout(id)` back.

The actor keeps `current_requests: HashMap<u64, (String,
oneshot::Sender<Message>)>` (421):

- inserts in `handle_outgoing_request` (2661-2679);
- removes on eleven response variants (2499-2519);
- removes on timeout (734-753).

```rust
// caller
let (tx, rx) = oneshot::channel();
self.handle.outgoing_request(message, Arc::new(tx))?;
match tokio::time::timeout(timeout, rx).await {
    Ok(Ok(r)) => Ok(r),
    Ok(Err(e)) => Err(RecvError(e.to_string())),
    Err(_) => { self.handle.request_timeout(id)?; Err(Timeout) }
}
// actor
let Some(sender) = Arc::into_inner(msg.sender) else { return };   // drops sender silently
state.current_requests.insert(id, (format!("{message}"), sender)); // overwrites duplicates
send(state, message).await?;                                       // awaited in the handler
```

Hazards:

- **Leak on caller cancellation.** If the caller's future is dropped before
  its timeout, no `request_timeout` is sent. The entry stays until a response
  arrives or teardown, and the map is unbounded.
- **Duplicate ids.** A duplicate id overwrites the earlier entry, and that
  caller gets an opaque `RecvError`.
- **Silent sender drops.** When the connection is not established, or when
  `Arc::into_inner` fails, the sender is dropped without a reason.
- **Late responses.** A response that arrives after the timeout produces a
  logged `SendMessage` error.
- **Teardown.** Teardown never drains the map.

### After

```rust
// protocol: an ordinary request method; nothing says "deferred"
fn request(&self, message: Message) -> Response<Message>;

// caller: no timeout message, no oneshot
conn.with_request_timeout(timeout).request(message).await
// -> Err(RequestTimeout | ActorStopped | Overloaded | ReplyDropped)

// actor
#[deferred_handler]
async fn request(&mut self, msg: Request, reply: PendingReply<'_, Message>, ctx: &Context<Self>) {
    let ConnectionState::Established(state) = &mut self.state else {
        return;                                   // dropped in turn: caller gets ReplyDropped
    };
    let id = msg.message.request_id().expect("request without id");
    let Ok(token) = reply.defer() else { return }; // Overloaded (caller told) or Closed (caller gone)
    match state.current_requests.insert(id, token) {
        Ok(()) => state.outbox.push(msg.message),  // see note on the socket send
        Err(refused) => match refused.into_parts() {
            (InsertErrorKind::Duplicate, token) => drop(token), // the new caller is told; the old one kept
            (_, token) => token.overloaded(), // Full or a later reason: caller gets Err(Overloaded)
        },
    }
}
// response path
match state.current_requests.take(&id) {
    Some(token) => { token.reply(message); }       // CallerGone/Expired are fine
    None => trace!(id, "late or unknown response"), // expired, abandoned or purged: not an error
}
// current_requests: PendingReplies<u64, Message>::with_capacity(MAX_INFLIGHT_PER_PEER)
```

Removed:

- the `RequestTimeout` message and its handler;
- the `Arc<oneshot::Sender>` wrapping and its `into_inner` fallback;
- the caller's timeout bookkeeping.

How the new path bounds itself:

- **One deadline.** The send-time deadline is both the caller's timeout and
  the token's.
- **Caller abandonment.** Dropping the caller's future abandons the slot,
  which returns the permit at once.
- **Bounded map.** `PendingReplies` purges terminal entries on every insert
  and is capped, so ids that time out cannot accumulate.
- **No error spam.** A response for an id that is no longer pending (expired,
  abandoned or purged) is a late response, traced and ignored, the way OTP
  drops a late reply.
- **Teardown.** When the connection actor dies, every stored token is
  answered `ActorStopped`; `stopped()` may answer them first with a better
  error.

Not removed: the 30 s inline socket `send` (`OUTBOUND_SEND_TIMEOUT`, 97-103)
still blocks the mailbox. The fix is a writer job (`ctx.pipe` over a bounded
outbound queue) or a dedicated writer actor. That is a separate change.

## 2. ethrex: `get_best_peer` polling

### Before

Seven call sites ask `get_best_peer` for a peer and, on `None`, sleep 10 ms and
retry:

- `peer_handler.rs:380-393`
- `snap/client.rs:279`, `461`, `1029`
- `sync/healing/state.rs:279-300`
- `sync/healing/storage.rs:396`
- `sync/snap2/download.rs:388`

```rust
loop {
    let Some((peer_id, conn, permit)) = self.peer_table.get_best_peer(caps).await? else {
        tokio::time::sleep(Duration::from_millis(10)).await;
        continue;
    };
    ...
}
```

Each idle loop makes about 100 actor round trips per second. Capacity returns
through `RequestPermit::drop`, which sends a fire-and-forget `dec_requests`
(`peer_table.rs:422-428`); nothing wakes the waiters.

### After

```rust
// caller (each of the 7 sites); protocol method declares a 30 s timeout
let (peer_id, conn, permit) = self.peer_table.wait_for_peer(caps).await?;

// peer table
#[deferred_handler]
async fn wait_for_peer(&mut self, msg: WaitForPeer, reply: PendingReply<'_, Lease>, _: &Context<Self>) {
    if let Some(lease) = self.take_lease(&msg.caps) { reply.reply(lease); return; }
    self.waiters.retain(|(_, t)| !t.is_closed());          // purge before admitting
    if self.waiters.len() >= MAX_WAITERS { reply.overloaded(); return; } // caller gets Err(Overloaded)
    if let Ok(token) = reply.defer() { self.waiters.push_back((msg.caps, token)); } // Closed/NoCaller/Overloaded: nothing to keep
}
fn wake_waiters(&mut self) {                                // from dec_requests and new-peer handlers
    self.waiters.retain(|(_, t)| !t.is_closed());
    while let Some(i) = self.waiters.iter().position(|(caps, _)| self.can_serve(caps)) {
        let (caps, token) = self.waiters.remove(i).unwrap();
        let lease = self.take_lease(&caps).unwrap();
        token.reply(lease);                                 // if the caller left, the lease drops and returns
    }
}
```

Waiters are served first come, first served, filtered by capability. The queue
is capped by `MAX_WAITERS` and purged before each admission. Callers that left
free their permit at once, and their entry on the next purge. A full queue
answers at once instead of spinning. This is rank 17's lease pool with
waiters, at its smallest.

## 3. ethlambda: gossip verdicts (pipe-to-self)

### Before

`spawn_stateful_checks` (`net/p2p/src/beacon/verdict.rs:640-667`):

1. Picks one of four semaphores (`permits_for`, 600-619).
2. Calls `try_acquire_owned`. On failure it reports `Ignore(Overloaded)`.
3. Calls `spawn_blocking`. Inside, `guarded` (a `catch_unwind` that turns a
   panic into `Ignore(Internal)`, 675-688) wraps the check, and the result is
   sent to the actor as `GossipVerdict`. A failed send is only logged, and the
   `JoinHandle` is discarded.
4. `Handler<GossipVerdict>` settles first-valid ordering.

Gaps:

- There is no deadline: a hung check never yields a verdict.
- The verdict is lost if the actor is gone.
- Nothing owns the blocking task.

### After (v1, before the rank 11b pool)

```rust
let opts = JobOptions::new().deadline(VERDICT_DEADLINE);
match ctx.pipe_blocking(opts, move |_| object.stateful_checks(&store).map(|o| (o, object)),
                        move |res| GossipVerdict { id, res }) {
    Ok(_job) => {}
    Err(PipeError::Overloaded | PipeError::NativeBudget) => {
        report(server, id, Ignore(Overloaded)); forward(..);
    }
    Err(e) => { report(server, id, Ignore(Internal)); warn!(?e); }
}
impl Handler<GossipVerdict> for P2PServer {
    async fn handle(&mut self, msg: GossipVerdict, _: PendingReply<'_, ()>, _: &Context<Self>) {
        let outcome = match msg.res {
            Ok((outcome, _object)) => outcome,
            Err(JobError::DeadlineExceeded) => Ignore(Timeout), // new: hung checks still yield
            Err(_) => Ignore(Internal), // a panic
        };
        self.settle(msg.id, outcome, ..);
    }
}
```

Removed:

- `guarded`: a panic becomes `JobError::Panicked`.
- The detached `spawn_blocking`: the job is owned by the actor's job set,
  joined at stop, and reported if it is unfinished.
- The logged send failure.

The deadline notification arrives on time even though the hung thread keeps
running. That thread still holds its native-budget slot, so a hung check
cannot be replaced by new work.

Remaining gap until rank 11b: the four semaphores. They are four admission
classes; v1 offers only the per-actor native budget. Keep them in front of
`pipe_blocking`, or wait for 11b's per-class admission.

## 4. ethlambda: aggregation session (keyed job, deadline, fencing)

### Before

`AggregationSession { session_id, early, cancel, worker: JoinHandle<()> }`
(`blockchain/src/aggregation.rs:129-144`).

`start_aggregation_session` (`blockchain/src/lib.rs:1340-1439`):

1. Cancels the prior session.
2. **Awaits its `JoinHandle` for up to 2 s inside the handler**
   (`PRIOR_WORKER_JOIN_TIMEOUT`).
3. Takes a snapshot.
4. `spawn_blocking`s the worker.
5. Arms `send_after(AggregationDeadline { session_id })`.

Afterwards:

- `AggregateProduced` is fenced by `session_id` (4083-4101).
- `AggregationDeadline` is fenced by session (4177-4188).
- `on_stopped` joins again, with 2 s.

Hazards:

- The join blocks the mailbox for up to 2 s per slot.
- A join timeout detaches the thread.
- `AggregationDone` is not fenced.
- A worker panic sends no `Done`.
- Stale timers rely on the hand fence.

### After

```rust
fn start_aggregation_session(&mut self, slot: u64, ctx: &Context<Self>) -> Result<(), PipeError> {
    let snapshot = self.snapshot_for(slot);
    let opts = JobOptions::new().key(AGGREGATION).deadline(interval_2_boundary(slot));
    // Keyed: cancels the prior session's job and fences its queued messages;
    // no join in the turn (the job set joins at stop, with a deadline).
    ctx.pipe_blocking(opts, move |job: JobCtx<Self>| {
        for proof in snapshot.jobs() {
            if job.is_cancelled() { break }
            job.emit_blocking(AggregateProduced { slot, proof: prove(proof) })?; // fenced per emit
        }
        Ok(())
    }, move |res| AggregationDone { slot, res })?;    // Ok / Panicked / DeadlineExceeded
    self.current = Some(slot);
    Ok(())
}
// Handlers for AggregateProduced and AggregationDone need no session check.
// AggregationDone with DeadlineExceeded replaces the AggregationDeadline timer.
```

Removed:

- the `session_id`, `cancel` and `JoinHandle` struct;
- the 2 s join in the handler;
- the three hand-written session checks;
- the separate deadline timer message: the job deadline delivers
  `DeadlineExceeded` on time even if the proof thread is still running.

Also:

- `AggregationDone` is fenced like the emits. A replaced session's queued
  messages are fenced even if its deadline already fired, and after the
  deadline the old proof thread can no longer emit (`JobGone`).
- A panic arrives as `Err(Panicked)`.
- A thread still running at the stop deadline is reported as unfinished
  work, not silently detached.
- Instance timers that ethlambda keeps (`send_after`) are already fenced by
  the per-instance mailbox, so no new timer API is needed.

## What the clients need from v1

- `#[deferred_handler]`, `PendingReply`, `ReplyTo` and
  `PendingReplies<K, R>` (sections 1 and 2). Expired requests are still
  dispatched (OTP), so handlers check `reply.is_closed()` or rely on
  `defer()` returning `Closed`.
- `pipe_blocking`, `JobOptions::new().key(..).deadline(..)` and
  `JobCtx::emit_blocking` (sections 3 and 4).
- Rank 11b for pool-based admission classes (section 3).
- Ethrex is on spawned 0.5.0. These rewrites assume it first migrates to v0.7
  (`ActorSystem`, managed scopes), as the canary plan already requires.
