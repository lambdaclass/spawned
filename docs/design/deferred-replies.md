# Deferred replies and pipe-to-self: design contract

**Status: implementation in review (PR #239); see [Implementation notes](#implementation-notes).**

- Covers ROADMAP rank 11 and the rank 11a decisions. The rank 11b compute pool
  is deferred to 11b.
- Revised in three passes after two rounds of independent review; see
  [Review resolutions](#review-resolutions).
- Illustrative API shapes are not copyable code.
- Models: [`DeferredReplies.tla`](../../verification/tla/DeferredReplies.tla)
  for reply slots, tokens, linked jobs and owner exit;
  [`DeferredJobs.tla`](../../verification/tla/DeferredJobs.tla) for job
  fencing, keyed replacement and emits.
- Client rewrites: [client mapping](deferred-replies-client-mapping.md).

**Goal.** A handler can accept a request, return to its mailbox at once, and
answer later from another turn or from a background job. Callers and
protocols do not change. Every accepted request gets exactly one terminal
discharge, including on owner exit or panic, and no message from an earlier
instance or a cancelled job reaches a handler.

**Project rule.** Match Erlang/OTP. Deviate only where:

- Rust forces it;
- Rust types add strictness; or
- Spawned adds a bound OTP lacks.

See [Differences from OTP](#differences-from-otp).

| OTP | Spawned (proposed) |
|---|---|
| `handle_call` returns `{noreply, State}` and keeps `From` | the handler receives a `PendingReply<'_, R>`; `defer()` gives a storable `ReplyTo<R>` |
| `gen_server:reply(From, R)` | `reply_to.reply(value)` |
| `handle_cast` | the same handler, reached by `send`; the `PendingReply` is detached |
| delegating with `From` | `ctx.pipe_reply(opts, token, \|_\| other.request(msg), ..)`; the obligation stays here |
| `Task.Supervisor.async_nolink` + `handle_info` | `ctx.pipe(opts, work, done)`; the result or a caught panic arrives as one message |
| `gen_server:call/3` timeout (default 5000, or `infinity`) | one deadline fixed at send (default 5 s, or `Deadline::Never`) |
| a call times out; the server still handles it; the late reply is dropped | the same: expired requests are dispatched, and a late reply is a no-op |
| call monitors the server and fails when it dies | the owner's death discharges every token it holds |
| `terminate/2` may still reply | `stopped()` may still reply |

## Goals and non-goals

Goals:

1. **No awaiting downstream work inside a handler.** Awaiting blocks the
   whole mailbox. That is behind ethrex's wedged connection handlers,
   d958dcba4 and 535647e45.
2. **Exactly one terminal discharge per request.** This holds on every path:
   in-place reply, later reply, end-of-turn drop, held-token drop, deadline,
   caller give-up, deferral at the bound, owner stop or panic, and tokens
   held by jobs.
3. **Fenced late messages.** Messages from earlier instances, cancelled jobs
   and replaced keyed jobs are fenced.
4. **Bounded resources.** Deferred credits, native jobs and emits in flight
   are bounded per actor, and so is deadline storage.
5. **Identical semantics in `tasks` and `threads` modes.** Every timer and
   spawn goes through `spawned_rt`.
6. **One completion contract.** Ranks 11b, 13, 15, 15a and 19 reuse it.

Non-goals:

- general reentrancy;
- `ReplyTo::forward` and `map` in v1;
- promise pipelining and transactional turns (rank 11a, decided below);
- durable replies;
- preempting blocking code.

## 1. Reply transport (breaking)

Today an envelope holds `oneshot::Sender<M::Result>`, which can only express
`ActorStopped`. Every request now gets a **reply slot**, shared by the
envelope, then the token, and the caller's receiver.

### Slot synchronization protocol

**One synchronization domain: a packed slot word.** Slot state, the token's
credit, the running linked-job count and registry membership live in one
atomic word. Every operation that changes any of them is a single
compare-and-swap on that word, so "install a credit only if `OPEN`" and
"terminalize and release the credit" cannot interleave.

```text
word: AtomicU64 = state:4 | TOKEN:1 | REG:1 | JOBS:16 | (spare)
  state: OPEN | WRITING | REPLIED | DROPPED | OVERLOADED | TIMED_OUT | STOPPED | ABANDONED
  TOKEN: a deferred token holds this request's credit (set by defer())
  REG:   listed in the owner registry (set together with TOKEN)
  JOBS:  running linked jobs (pipe_reply); MAX_OUTSTANDING is limited to
         65535 at configuration, so the field cannot overflow
deadline: AtomicU64   rt clock ticks; u64::MAX = never; only decreases
value:    written only by the thread that moved the word to WRITING
waker:    AtomicWaker (tasks) / Mutex+Condvar (threads)
```

**Credits held by a slot:**

```text
credits(word) = max(JOBS, TOKEN and state is OPEN ? 1 : 0)
```

The owner's credit counter changes only by `credits(new) - credits(old)` of a
successful CAS:

- a positive delta is reserved from the counter before the CAS, and rolled
  back if the CAS fails;
- a negative delta is returned after it.

**State machine.** Each row is one CAS on the word. `[deadline]` means the
operation first reads `now`, and if `now >= deadline` it performs
`OPEN -> TIMED_OUT` instead.

| Operation | From | To | Credit delta |
|---|---|---|---|
| `defer()` `[deadline]` | `OPEN`, `TOKEN=0` | `OPEN`, `TOKEN=1`, `REG=1` | +1 if `JOBS=0` |
| `pipe_reply` attach `[deadline]` | `OPEN`, `TOKEN=1` | `OPEN`, `JOBS+1` | +1 if `JOBS>=1` (the first job shares the token's credit) |
| linked job ends | any, `JOBS>=1` | same state, `JOBS-1` | -1 if this lowers `credits` |
| reply | `OPEN` | `WRITING`, then `REPLIED` or `TIMED_OUT` | -1 if `TOKEN` and `JOBS=0` |
| drop, overloaded, expiry, abandonment, registry close | `OPEN` | `DROPPED` / `OVERLOADED` / `TIMED_OUT` / `ABANDONED` / `STOPPED` | -1 if `TOKEN` and `JOBS=0` |
| `is_closed()` `[deadline]` | `OPEN`, past deadline | `TIMED_OUT` | as expiry |

Rules:

1. **Reply.** The reply is `CAS OPEN -> WRITING`, which keeps the
   `TOKEN`/`REG`/`JOBS` bits. It then reads `deadline` and `now`:
   - at or past the deadline: CAS the state field `WRITING -> TIMED_OUT`, and
     return the value as `Expired`;
   - otherwise: write the value, then CAS the state field
     `WRITING -> REPLIED`.

   These CASes change only the state field and retry on concurrent `JOBS`
   changes; a whole-word store would lose a job-end decrement.

   Only `REPLIED` publishes the value; a receiver that sees `WRITING` waits.
   A registry close that finds `WRITING` waits for it to resolve. The window
   is a value write; no user code runs in it.
2. **Shortening.** `shorten_deadline(at)` is `fetch_min(deadline, at)`, then the
   `[deadline]` check. The linearization point is the reply's `WRITING` CAS:
   - a shortening before it is seen by the reply;
   - a shortening after it finds the word not `OPEN` and does nothing.
3. **Registry.** The registry is a per-instance list of weak slot references,
   appended by `defer()` after its CAS and pruned lazily; the `REG` bit in the
   word is authoritative. Closing the registry CASes every listed slot that is
   still `OPEN` to `STOPPED`.

   `defer()` and attach run only in the owner's turns, which never overlap
   the close, so a token cannot be registered after the close.
4. **After a winning transition, in this order:**
   1. return the counter delta;
   2. wake the receiver.
5. **No user code under framework locks.** Payloads, losing values, `done`
   closures and fenced envelopes are dropped after locks are released.
   Fenced and discarded envelopes are dropped through the existing
   `DropFailures` path.

Exact memory orderings and waker registration are implementation details, to
be checked with Loom.

Caller view:

| Slot state | Caller gets |
|---|---|
| `REPLIED` | `Ok(v)` |
| `DROPPED` | `Err(ReplyDropped)` |
| `OVERLOADED` | `Err(Overloaded)` |
| `TIMED_OUT` | `Err(RequestTimeout)` |
| `STOPPED` | `Err(ActorStopped)` |

There is one timeout reason, as in OTP. `DeadlineExceeded` exists only on
`JobError`.

### API changes

| Today | Proposed |
|---|---|
| `request_raw(msg) -> Result<oneshot::Receiver<R>, _>`, no timeout | `send_request(msg, impl Into<Deadline>) -> Result<ReplyReceiver<R>, _>` (OTP's `gen_server:send_request/2` with an explicit deadline): a `Duration`, an `Instant` or `Deadline::Never` |
| `Receiver<M>::request_raw` | `send_request(msg, Deadline)` (required) |
| erased `request(recipient, msg, timeout)` | computes the deadline before enqueueing, then `send_request` |
| `Response::from(Result<oneshot::Receiver<T>, _>)`, `Response::from_with_timeout` | `Response::from(Result<ReplyReceiver<T>, _>)`; `from_with_timeout` removed (the deadline is already in the slot) |
| `#[protocol]` tasks code: `from_with_timeout(target.request_raw(m), t)` | the timeout is chosen first (per-call override, method, protocol, default), then `Response::from(target.send_request(m, Deadline::after(t)))` |
| `#[protocol]` threads code: `Response::ready(target.request_with_timeout(m, t))` | unchanged: threads `Response` stays eager and blocks in the call |
| `ActorError`, exhaustive | `#[non_exhaustive]`, plus `ReplyDropped` and `Overloaded` |
| `metrics::Snapshot` request counters | add `requests_reply_dropped_total` and `requests_overloaded_total` (`RequestObservation::finish`) |
| `spawned-test` `Mock`, `Probe<M>` | `Mock` implements the new `Handler` (below); `Probe` implements `send_request` and discharges unexpected requests as `Stopped`, keeping its contract |

`ReplyReceiver<R>` is a future in `tasks` mode and has `recv()` in `threads`
mode. Dropping it is `CAS OPEN -> ABANDONED`, which returns the token's credit
at once.

## 2. Timing contract and deadline service

**One deadline, fixed at send.** The deadline is chosen before enqueueing:
`send instant + timeout`, or never. The caller's receiver has no separate
timeout duration. While it is polled, it sleeps until the slot's deadline and
then expires the slot; there is one deadline, not two timers.

The behaviour changes in two places, both in the migration notes:

- a `#[protocol]` request's timeout used to start at first poll;
- `request_raw` used to have no timeout (`send_request` takes one explicitly).

We use send time because the deadline is a property of the request, the server
cannot learn when the caller polls, propagated deadlines (rank 19) need an
absolute instant, and OTP's `call` starts waiting when it sends.

**Who expires a slot.**

- **A polling receiver.** `request`, `request_with_timeout` and
  `Response::from` sleep on the runtime's timer until the slot's deadline
  and then `CAS OPEN -> TIMED_OUT` (so they follow paused Tokio clocks). A
  raw receiver awaited directly (`send_request(..)?.await`) instead puts its
  deadline once in the owner's heap, which expires it on the service's clock
  (real time, or the simulated clock under `spawned_sim`), saving a timer
  per request. A shortened deadline (`ReplyTo::shorten_deadline`) wakes the
  awaiting or parked receiver, which re-arms for it.
- **`defer()`, the `pipe_reply` attach and `is_closed()`.** They check the
  deadline first, so an unpolled, expired request reports `Closed`.
- **The owner's deadline heap,** for deferred tokens and for job deadlines.
  It returns credits and delivers job deadline notifications even when no
  caller is polling.
- **A reply itself:** at or past the deadline it expires the slot (rule 2).

A queued or in-turn request that nobody polls stays open until it is polled,
replied (and expired), deferred (`defer()` on an expired slot returns
`Closed`), or the owner dies. It holds no credit, so no bound depends on it.

**Expired requests are still dispatched.** This is OTP behaviour: the server
handles a call even after the caller timed out, and the late reply is dropped.

- `PendingReply::is_closed()` lets a handler skip work.
- Skipping stale work before dispatch is an opt-in policy, deferred to rank 19
  (for example `#[deferred_handler(skip_expired)]`).

**Deadline storage.** Each actor's registry owns a min-heap of
`(deadline, entry)`, where an entry is a token slot or a job state.

- **Stale entries.** An entry is stale when its slot or job is terminal, or
  when its deadline is later than the current one (superseded by
  `shorten_deadline`).
- **Compaction.** The heap is rebuilt when stale entries exceed live ones,
  per actor. So storage is at most about 2 × (live deferred tokens + live
  jobs with deadlines + awaited raw receivers); the first two are bounded by
  `MAX_OUTSTANDING` + job count, the last by the requests callers are
  awaiting (each also holds a queued envelope or an open slot).
- **Ownership.** Entries hold `Weak` references, so the heap owns no slot and
  no job. The heap is dropped at the owner's death, after the registry close.
- **Service.** A per-system service holds one entry per actor: its earliest
  deadline. Shortening or a new earliest entry re-registers and wakes the
  service. The service is a task on the `ActorSystem`'s control runtime and
  stops after all of the system's actors have died.
- **Unmanaged actors.** These use one process-wide service thread with its
  own timer, not a task on whichever Tokio runtime touched it first, so
  per-test runtimes do not break it.
- **Simulation.** Under `spawned_sim` the service is a task of the simulated
  runtime, one per simulation instance, so clock domains never mix.

## 3. Handlers (explicit trait migration)

Deferral is an implementation detail of the actor; `#[protocol]` and callers
are unchanged. The breaking release replaces the dispatch trait:

```rust
pub trait Handler<M: Message>: Actor {
    /// Envelope dispatch for both send and request.
    fn handle(&mut self, msg: M, reply: PendingReply<'_, M::Result>, ctx: &Context<Self>)
        -> impl Future<Output = ()> + Send;
}
```

- **`#[actor]` users write the same methods as today.**
  - A `#[request_handler] fn f(&mut self, msg, ctx) -> R` gets a generated
    `handle` that calls `f` and replies.
  - A `#[deferred_handler] fn g(&mut self, msg, reply: PendingReply<'_, R>,
    ctx)` gets a `handle` that passes the reply through.
  - `#[send_handler]` is unchanged in shape.
  - The inherent methods stay callable, so direct test calls keep working:
    `actor.f(msg, &ctx)` returns `R`, as now.
- **Manual impls migrate:** `async fn handle(&mut self, msg, reply, ctx) {
  reply.reply(...) }`. There is no default bridge from a `handle -> R`
  signature, so there is no blanket impl and no E0119 conflict with `Mock`.
- **`send(M)`** dispatches with a detached `PendingReply`:
  - `reply()` drops the value (outside locks);
  - `defer()` returns `Err(DeferError::NoCaller)`;
  - dropping it does nothing.

  Nothing ever awaits a local slot.
- **Direct calls of manual impls in tests** use `spawned_test::reply_pair()`,
  which returns `(PendingReply<'static, R>, ReplyReceiver<R>)`. The test
  awaits the receiver only after driving any turns the reply depends on.
- **`Mock`** keeps `impl<M: Message> Handler<M> for Mock` with the new
  signature. A reply closure replies in place, and `Mock::defer` stores the
  token so a test can reply later, drop it, or never reply.

Example:

```rust
#[actor(protocol = PeerTableProtocol)]
impl PeerTable {
    #[deferred_handler]
    async fn wait_for_peer(&mut self, msg: WaitForPeer, reply: PendingReply<'_, Lease>,
                           ctx: &Context<Self>) {
        if let Some(lease) = self.pick(&msg.caps) { reply.reply(lease); return; }
        match reply.defer() {
            Ok(token) => self.waiters.push(msg.caps, token),
            Err(DeferError::Overloaded) => {}   // caller already answered Overloaded
            Err(DeferError::Closed) => {}       // caller already timed out or left
            Err(DeferError::NoCaller) => {}     // reached by send
            Err(_) => {}                        // #[non_exhaustive]
        }
    }
}
```

## 4. Tokens

```rust
/// Turn-scoped; cannot be stored.
pub struct PendingReply<'t, R> { .. }
impl<R> PendingReply<'_, R> {
    pub fn reply(self, value: R) -> Delivery;          // Delivered | CallerGone | Expired
    pub fn defer(self) -> Result<ReplyTo<R>, DeferError>;  // Overloaded | Closed | NoCaller
    pub fn deadline(&self) -> Deadline;                // Never for sends
    pub fn is_closed(&self) -> bool;                   // checks the deadline first
    pub fn overloaded(self);                            // answer Err(Overloaded)
}

#[must_use = "dropping a ReplyTo answers ReplyDropped (or ActorStopped while the owner exits)"]
pub struct ReplyTo<R> { .. }                           // Send + 'static, not Clone
impl<R: Send + 'static> ReplyTo<R> {
    pub fn reply(self, value: R) -> Delivery;          // .. | OwnerExited
    pub fn deadline(&self) -> Deadline;
    pub fn shorten_deadline(&self, d: impl Into<Deadline>); // shorten only
    pub fn is_closed(&self) -> bool;
    pub async fn closed(&self);                        // tasks mode
    pub fn owner(&self) -> ActorId;
    pub fn overloaded(self);                            // answer Err(Overloaded), e.g. on InsertError::Full
}
```

`defer()` checks the deadline, then sets `TOKEN` and `REG` in the one CAS that
requires `OPEN` (see the state machine). A slot that went terminal
concurrently therefore never acquires a credit.

**Drop classification.** The registry has a phase: `Running`, `InTurn(seq)`,
`Exiting` or `Dead`. Each token records the turn that created it.

| When the token is dropped | Outcome |
|---|---|
| owner is `InTurn` (in-turn drops include unwinding) | recorded on the turn, then committed when the turn ends: `Dropped` if the handler returned, `Stopped` if it panicked |
| owner is `Running` (between turns: a stored token, or a job-held token) | `Dropped` at once |
| owner is `Exiting` (`stopped()` or later) | `Stopped` at once |
| owner is `Dead` | no-op; the slot is already terminal |

No rule uses `thread::panicking()`. A drop that lands on the turn list from
another task while a turn runs is committed at that turn's end. The delay is
at most one turn, and the outcome only depends on whether that turn panicked.

**Refined by rank 5e** ([otp-exit-signals.md](otp-exit-signals.md#required-changes-to-rank-11-221-design-239-implementation),
implemented in #240): a `Stopped` outcome is no longer answered at once. The
slot becomes *exit pending*: it stays open in the registry and the death
point answers it `ActorStopped` with the owner's final exit reason (or the
caller's deadline comes first). An undispatched queued request is held the
same way, and a send or request after the death point reports `NoProc`.

## 5. Owner lifecycle

Each actor instance has a reply registry (its open deferred tokens and the
current turn's slot) and a job set. The order is:

1. **Exit begins.** Triggers: a panic caught by the run loop, `ctx.stop()`, a
   kill, or a linked exit.
   - the registry phase becomes `Exiting`;
   - the turn's dropped tokens commit as `Stopped`;
   - dispatch stops;
   - every running job is cancelled (flag set, blocked emitters woken);
   - `pipe*` now returns `OwnerStopping`.

   The run loop arms the cleanup deadline for every actor, standalone
   `group.start` and `.start()` actors included:
   - the supervisor's `shutdown_timeout`;
   - the system grace;
   - otherwise `DEFAULT_STOP_DEADLINE` (5 s).
2. **`stopped()` runs** (OTP `terminate`). Stored tokens can still be
   answered.
3. **Death.** This happens only when `stopped()` has returned. Rust cannot
   preempt `stopped(&mut self)` or a hanging destructor, and it cannot
   destroy state that is still borrowed. In this order, whether the run
   loop or the runtime dropping the actor drives it:
   1. job admission closes and every job is cancelled; outgoing replies of
      deferred tokens and exit signals sent through the actor's contexts
      are fenced (those in flight finish, later ones are refused: a reply
      returns `OwnerExited`, a signal is inert);
   2. the final reason is fixed, the link table closes and admission is
      sealed (`NoProc`);
   3. the registry closes, and every open token it lists becomes `STOPPED`,
      wherever the token is (state, job, helper, leaked);
   4. the mailbox closes and every queued request is answered `STOPPED`;
   5. only then are payloads destroyed: queued envelopes (emit credits
      return), then actor state;
   6. the deadline heap is dropped;
   7. the exit reason is published, and monitors and links fire.

   Steps 1 to 4 run no user code: steps 3 and 4 only make terminal
   transitions, and every slot they meet stays referenced (an unread
   reply's value is user code when destroyed) until step 5. After the last
   answer, and before step 5, run in this order: the parked blocking
   receivers are notified (no user code, so none waits behind a waker), the
   callers' wakers (executor or user code), a runtime drop's "stopped()
   skipped" log, and the callbacks the jobs' cancellation owes (`cancelled()`
   waiters, blocked emitters). Cancellation in step 1 only sets the jobs'
   flag: the payloads it releases (held results, scheduled emits) are
   destroyed in step 5. Steps 3 and 4 follow the seal instead of being one atomic step
   with it, but nothing between them depends on the requests they answer,
   so the models' atomic death is a sound abstraction of them (they commute
   left to the seal).

   `ActorStopped` keeps one meaning, "the actor is gone". An observer that
   sees `Down` knows every caller of the instance has been answered.
   `wait_exit()` and `exit_reason()` observe death.
4. **Job cleanup.** The cancelled jobs are joined, bounded by what remains
   of the cleanup deadline. A job still running at that point does not undo
   death: the actor is dead and its **job cleanup is incomplete**.
   - Unfinished jobs are reported: a tracing event, `unfinished_at_stop`,
     and a count in the exit report or `ShutdownReport`.
   - Their native threads may outlive the report.
   - In `threads` mode, only threads that have signalled completion through
     the job set's condvar are joined.

**Actor cleanup incomplete (stuck).** If the cleanup deadline passes while
`stopped()` or a state destructor is still running, the actor is **still
alive**, not dead. This matches the
[execution-group contract](execution-groups-supervision.md#lifecycle-control-and-shutdown)
for incomplete shutdown:

- **Nothing is closed early.** The registry stays open, `stopped()` may
  still reply, and no caller gets `ActorStopped` before the actor is gone.
- **Callers.** Token holders and queued callers are bounded by their own
  deadlines (`RequestTimeout`). A `Deadline::Never` caller waits until death,
  as an OTP caller with `infinity` waits for the server.
- **Death, exit reason, monitors and links.** All wait for the real end.
- **Report.** The exit report or `ShutdownReport` marks the instance cleanup
  incomplete, with an owned pending-work handle that can be awaited again.
- **No replacement.** A supervised child is not replaced while its old
  instance is alive; the existing cleanup-stuck rule fails the tree.
- **Joins.** `join()` returns when cleanup completes, which may be never.
  Bounded observation is the report, or a caller-side timeout around `join()`.

This is a Rust-forced deviation (category R). OTP's supervisor kills the
process at the shutdown timeout, which makes death and the callers'
`noproc`/exit coincide. Rust cannot kill a thread, so death, and with it the
close, waits for the code to return. The cleanup deadline only reports.

**`JobHandle` and ownership.** Dropping a `JobHandle` leaves the job owned by
the actor's job set; it is not detached.

**Exit reasons (ROADMAP rank 5e, v0.8).** Rank 5e brings `Killed`, ignored
`Normal` exit signals, trappable `Shutdown` and `NoProc`:

- an ignored `Normal` signal never begins exit;
- a trapped `Shutdown` runs steps 1-4;
- `ActorStopped` should carry the reason (OTP `{Reason, {gen_server, call,
  ..}}`), for example `ActorError::Exited(ExitReason)` on the now
  non-exhaustive enum;
- a request to an actor that has already died reports `NoProc`.

Both are implemented in #240 as `ActorError::ActorStopped(ExitReason)` and
`ActorError::NoProc`, with the death boundary of
[otp-exit-signals.md](otp-exit-signals.md#phases-of-an-exit): the registry is
sealed with the final reason at the death point, and exit errors wait for
it.

The slot's terminal kinds are defined so that rank 5e only adds the reason.

## 6. Delegation

There is no `ReplyTo::forward` in v1.

```rust
let token = reply.defer()?;
ctx.pipe_reply(JobOptions::new(), token, |_| other.request(msg), |res, token| Delegated { res, token })?;
```

The downstream request is an ordinary request with the destination's own
admission and deadline; it should pass `token.deadline()` on
(`other.send_request(msg, token.deadline())`), which rank 19
automates. The obligation and credit stay in this actor's registry. When the
linked job ends or is cancelled, its future, and with it the downstream
`ReplyReceiver`, is dropped. That abandons the downstream slot and returns
the destination's credit.

## 7. `PendingReplies`

```rust
impl<K: Hash + Eq, R> PendingReplies<K, R> {
    pub fn with_capacity(cap: usize) -> Self;
    /// Purges terminal tokens, then inserts; a duplicate key or a full table
    /// hands the new token back.
    pub fn insert(&mut self, key: K, token: ReplyTo<R>) -> Result<(), InsertError<R>>;
    pub fn take(&mut self, key: &K) -> Option<ReplyTo<R>>;   // skips (and removes) terminal entries
    pub fn len(&self) -> usize;                               // live entries (scan)
    pub fn is_empty(&self) -> bool;
    pub fn drain(&mut self) -> impl Iterator<Item = (K, ReplyTo<R>)>; // live tokens, oldest first
}
#[non_exhaustive]
pub enum InsertError<R> { Duplicate(ReplyTo<R>), Full(ReplyTo<R>) }   // into_inner()
```

Retained entries never exceed `cap`, however fast tokens expire. The handler
decides what to answer on `InsertError` (typically `Overloaded` or a domain
error) and still owns the token. Entries keep insertion order, so `drain`
answers callers in the same order in every process: the order never comes
from a per-process hash seed, and a same-seed simulator run reproduces it
(#261; `reply::pending::tests::drain_follows_insertion_order`,
`tests/sim.rs::pending_replies_drain_is_reproducible`). The table is a scan
bounded by `cap`: `insert` already purges it whole, and `take` is linear.

## 8. Bounds

**Credits per actor instance: `MAX_OUTSTANDING`** (an `Actor` constant,
default 1024; `#[actor(max_outstanding = N)]`, overridable per `ChildSpec`
with `ChildSpec::max_outstanding`). This is a count limit, not a
memory budget; byte credits are rank 18.

- An in-place reply needs no credit.
- A request holds `credits(word) = max(running linked jobs, 1 if its token
  is live)`, computed from the packed slot word.
- The token and its first linked job share one credit. Each further running
  linked job of that request adds one, as does any job still running after
  the token closed.
- Unlinked jobs (`ctx.pipe*`) hold one credit each, in their `JobState`.

**Attaching a linked job (`pipe_reply`).** The attach is one CAS on the slot
word that requires `OPEN` and `TOKEN` and increments `JOBS`:

- **Slot not open or past its deadline:** no job is spawned, and the token
  comes back in `PipeReplyError::Closed`.
- **A linked job already runs:** for example, a job deadline returned the
  token through `done`. The CAS raises `credits`, so one credit is reserved
  first, and `Overloaded` is returned at the bound. One credit never funds
  two running jobs.

Because the count is per request rather than per credit identity, it does not
matter which job finishes first: the request holds
`max(running jobs, live token)` credits.

**Native execution budget: `MAX_NATIVE_JOBS`** (an `Actor` constant,
`#[actor(max_native_jobs = N)]`, overridable per `ChildSpec` with
`ChildSpec::max_native_jobs`; provisional default
`min(4, available_parallelism)`). It covers `threads`-mode jobs and
`pipe_blocking*`.

**Emit credits: `JobOptions::emit_credits`** (provisional default 16).

**At any bound: reject at that call, never wait.** `defer()`, `pipe*` and
`PendingReplies::insert` fail at once. For `defer()` the caller is answered
`Overloaded` in the same step, so the handler never holds an already-answered
token. Waiting in a turn would block the mailbox. Admission at send is rank 17
(an unbounded mailbox cannot reject at send).

**Owner constraint.** `pipe_reply` requires `token.owner() == ctx.id()`;
otherwise `PipeReplyError::WrongOwner` hands the token back.

## 9. Pipe-to-self jobs

```rust
#[non_exhaustive]
pub struct JobOptions { /* private */ }     // OTP-style option list, passed first
impl JobOptions {
    pub fn new() -> Self;                                   // = default()
    pub fn key(self, key: impl Into<JobKey>) -> Self;       // cancel-previous
    pub fn deadline(self, d: impl Into<Deadline>) -> Self;  // Duration, Instant or Deadline
    pub fn emit_credits(self, n: NonZeroU16) -> Self;       // default 16
    pub fn cancel_on_caller_gone(self) -> Self;             // pipe_reply* only; off by default (OTP)
}
// One call per shape; spawn at once. `work` receives the job's `JobCtx`.
ctx.pipe(opts, |job| async { .. }, done)                     -> Result<JobHandle, PipeError>
ctx.pipe_blocking(opts, |job| .., done)                      -> Result<JobHandle, PipeError>
ctx.pipe_reply(opts, token, |job| async { .. }, done)        -> Result<JobHandle, PipeReplyError<R>>
ctx.pipe_reply_blocking(opts, token, |job| .., done)         -> Result<JobHandle, PipeReplyError<R>>
// Reserve-then-spawn: the admission checks (owner running, credit, native
// slot) before `work`/`done` exist; spawning from the permit cannot fail.
ctx.reserve_job(opts)          -> Result<JobPermit<A>, PipeError>         // .pipe(work, done)
ctx.reserve_blocking_job(opts) -> Result<BlockingJobPermit<A>, PipeError> // .pipe_blocking(work, done)

#[non_exhaustive]
pub enum PipeError { Overloaded, NativeBudget, SpawnFailed(io::Error), OwnerStopping }
#[non_exhaustive]
pub enum PipeReplyError<R> {
    Failed { token: ReplyTo<R>, error: PipeError },
    Closed(ReplyTo<R>),
    WrongOwner(ReplyTo<R>),
}
pub type JobOutcome<T> = Result<T, JobError>;
#[non_exhaustive]
pub enum JobError { Panicked(String), DeadlineExceeded }
impl<A> JobCtx<A> {
    pub fn is_cancelled(&self) -> bool;
    pub async fn cancelled(&self);
    pub async fn emit<M>(&self, msg: M) -> Result<(), JobGone> where A: Handler<M>;
    pub fn emit_blocking<M>(&self, msg: M) -> Result<(), JobGone> where A: Handler<M>;
    // Delayed emit: credit taken now, step 2 at `at` (see the emit protocol).
    pub async fn emit_at<M>(&self, at: Instant, msg: M) -> Result<(), JobGone> where A: Handler<M>;
    pub fn emit_at_blocking<M>(&self, at: Instant, msg: M) -> Result<(), JobGone> where A: Handler<M>;
}
```

**Failure disposition.** On every error no job is spawned, and any credit
reserved for it is released before returning.

- `pipe_reply*` hands the token back in the error, and the handler decides
  the answer.
- `work`, `done` and their captures are dropped after the error is built,
  outside locks. A token captured inside `done`, instead of passed as
  `token`, drops per the drop rules.

**Job state.** Each job has one shared `JobState`, held by the job, its
handle, the key map and every message it produces. It has three parts:

- **Execution** (running or finished). This is physical. Cooperative
  cancellation means cancelled or timed-out work may keep running, and it
  keeps its credit (or native slot) until it ends.
- **Cancellation** (a flag). It is set by:
  - `JobHandle::cancel`;
  - a newer job with the same key (see Fencing);
  - owner exit;
  - for `pipe_reply` with `cancel_on_caller_gone`, the linked slot becoming
    `ABANDONED` or `TIMED_OUT`.

  Setting it wakes a blocked emitter with `JobGone`. Cancellation suppresses
  later results; it never undoes effects.
- **Terminal notification** (pending, notified or suppressed). This is a
  one-shot under the job's state lock. The completed value, a caught panic, or
  the job deadline (when `JobOptions::deadline` is set, serviced by the
  owner's deadline heap) takes it and enqueues the terminal message
  `(done, outcome)`. Cancellation suppresses it. After a deadline
  notification the work keeps running; its later result is dropped outside
  locks.

**Emit protocol.**

1. **Acquire.** `emit` takes an emit credit (waiting, or blocking in
   `emit_blocking`, while all are in flight).
2. **Check and enqueue.** Under the job's state lock: if the job is cancelled
   or its notification is not pending, fail with `JobGone` and return the
   credit. Otherwise enqueue the message.

   The terminal enqueue takes the same lock and needs no credit. So no emit
   can follow a job's terminal message, and a job's messages stay FIFO with
   the terminal one last.
3. **Return.** A credit comes back when its message is dispatched, fenced, or
   dropped with the mailbox at death.
4. **Wake.** Cancellation, notification and owner exit wake blocked emitters,
   which return `JobGone`. A blocked emitter therefore never holds up the
   stop-time join.

**Delayed emits (`emit_at`).** Step 1 runs at the call. A scheduled emit is
then registered under the job's lock (refused, `JobGone`, if the job is no
longer accepting emits) and serviced by the owner's deadline heap; at its
instant, step 2 runs as for `emit`. So it is fenced and credit-accounted like
any emit. To keep the terminal message last, the work's result is **held**
while scheduled emits remain: the last one to fire enqueues it. Cancellation
drops the scheduled emits and a held result; a job deadline takes the
notification as usual (deadlines are hard), and later scheduled emits are
then dropped at their instant. Modelled in `DeferredJobs.tla`
(`MCDeferredJobsHeld`: scheduled emits, the held result, the deadline taking
it and the job's credit kept until the terminal message; controls
`EarlyCredit` and `HeldDeadlineLost`); Loom model
`scheduled_emit_vs_finish_and_cancel`.

**Fencing at dispatch.** Every job message carries the job's `JobState` and
the owner's `ActorId`. The run loop drops it before the handler runs if:

- its `ActorId` is not this instance's (instance identity is the global
  `ActorId`, not a per-slot supervisor generation); or
- the job is cancelled.

**Keyed replacement.** `JobOptions::key` cancels the previous
job for the key **whatever its notification state**: pending, notified (its
deadline fired) or finished with a queued terminal message. So none of its
queued messages reach the handler. The key map holds only the current job.

There is no per-actor table of cancelled jobs, so nothing accumulates. Untagged
`ctx.send` and `send_after` messages get no job fence; the per-instance mailbox
already keeps them from newer instances.

**`done` runs in the owner's turn.** The terminal message carries `(done,
outcome)`. The envelope calls `done(outcome)` and dispatches the produced
message to its handler in the same turn. A panic in `done` is a handler
panic, so no consumed `FnOnce` has to be retried.

**Executors.**

- **`tasks` mode:** async jobs on the owner's runtime; blocking jobs on
  Tokio's blocking pool (`spawn_blocking` on the managed actor's group
  runtime, or the current one), under the native budget. Outside a Tokio
  runtime, and under the simulator, an owned thread.
- **`threads` mode:** one owned native thread per admitted job under the
  native budget. Spawning is fallible (`SpawnFailed`); the thread is spawned
  at reservation and parked until its job starts, so a permit's spawn is not.
  A managed compute pool comes in rank 11b.
- **Unmanaged actors** own their job set like managed ones; managed actors
  also register theirs with the scope.
- **Jobs must not wait on their own owner.** Once the owner is exiting it no
  longer dispatches, so such a job would hold up cleanup until the deadline
  and then be reported.

## 10. Ordering, supervision, metrics and simulation

**Ordering.**

- Replies may complete in a different order from their requests.
- One sender's messages stay FIFO.
- A job's messages are FIFO, with the terminal message last.
- Handlers of one actor never run concurrently.
- Deferral creates no deadlock wait-for edge (rank 7).

**Supervision.** Restart policy and budgets are unchanged.

- Callers see death as `ActorStopped`, with its reason after rank 5e.
- Tokens never carry over to a new instance.
- Draining (rank 13) is answering tokens and finishing jobs inside
  `stopped()` under the drain deadline; rank 13 adds admission close and the
  unfinished report.

**Metrics** (`metrics` feature, process-global families, plus
`ctx.deferred_outstanding()`):

- `spawned_deferred_outstanding`
- `spawned_deferred_reply_seconds`
- `spawned_deferred_outcomes_total{outcome}`, with outcome one of
  `replied`, `dropped`, `timed_out`, `abandoned`, `stopped`, `overloaded`
- `spawned_jobs_inflight`
- `spawned_jobs_total{outcome}`, with outcome one of `completed`, `panicked`,
  `deadline`, `cancelled`, `fenced`, `rejected`, `unfinished_at_stop`
- `spawned_deadline_heap_entries`

**Simulation.** Slot clocks, the deadline service, receiver sleeps, job
spawns and emits go through `spawned_rt`. Native jobs are outside sim control
and are reported as such.

**Trace validation.** `lifecycle-trace` records one event per model action
for trace validation against both models.

## Differences from OTP

Categories:

- **R:** Rust forces it.
- **T:** Rust types add strictness.
- **B:** a bound OTP lacks.

Everything else follows OTP.

| Difference | OTP | Spawned | Category and reason |
|---|---|---|---|
| Typed token | `From` is untyped | `ReplyTo<R>`; `#[must_use]` | T |
| Turn-scoped token and `defer()` | `From` is always storable | `PendingReply<'_, R>` cannot outlive the turn; `defer()` takes a credit | T (storing a token without a credit does not compile) and B |
| A dropped token answers at once | an unanswered `From` waits for the caller's timeout | `ReplyDropped` (or `ActorStopped` while exiting) | T (`Drop` on an affine token) |
| Deferral, native and emit bounds | none | `MAX_OUTSTANDING`, `MAX_NATIVE_JOBS`, emit credits; reject at once | B |
| Server-side token and job deadline servicing | the server never learns of the caller's timeout | the owner's heap expires deferred tokens (returning credits) and delivers job deadlines | B (credits return on time; the caller still sees OTP's single `timeout`) |
| Abandonment frees the credit | OTP 24+ aliases drop the late reply | `ABANDONED` drops it and returns the token credit | B (the credit); dropping the late reply is OTP |
| Job panic is a message | `async_nolink` crashes the task; the owner gets `:DOWN` | caught at the job boundary; `done(Err(Panicked))` is the one terminal message | R (no process isolation: `catch_unwind` keeps the owner alive); otherwise `async_nolink` |
| Owner exit cancels its jobs | `async_nolink` tasks outlive the owner; linked tasks die only on an abnormal exit | owner exit always cancels and joins its job set, bounded by the cleanup deadline | B (jobs hold instance credits and native slots; bounded shutdown requires an owner); a chosen lifecycle policy, not `Task.async` behaviour |
| Cleanup deadline does not kill | the supervisor kills the process at the shutdown timeout, so death and the callers' failure coincide | the cleanup deadline only reports "cleanup incomplete"; death, registry close and `ActorStopped` wait for `stopped()` and destructors to really end; callers are bounded by their own deadlines; no replacement meanwhile | R (Rust cannot kill a thread or destroy state borrowed by `stopped(&mut self)`) |
| One handler for send and request | `handle_cast` and `handle_call` are separate callbacks | `send(M)` and `request(M)` reach the same `Handler<M>::handle`; sends get a detached `PendingReply` (`defer()` returns `NoCaller`) | T (Rust dispatches on the message type, not the call verb) |
| Deadlines notify but do not kill | `Task.shutdown` / `brutal_kill` stop the task | `DeadlineExceeded` arrives on time; the work runs on, holding its credit | R (no preemption of Rust code) |
| Fencing at dispatch | `demonitor(Ref, [flush])`; a new pid makes old messages undeliverable | messages of cancelled or replaced jobs, or other `ActorId`s, are dropped before dispatch | R (a FIFO channel without selective receive cannot remove queued messages); OTP's flush semantics |
| Fenced emits | task messages are ordinary; users tag them with a ref | `JobCtx::emit` messages carry their job; terminal last | T (bound to the job by type; automates the ref check) |

Aligned with OTP, so not differences:

- **Callers and protocols** are unaware of deferral.
- **Timeouts.** There is one timeout reason, the timeout starts at send, the
  default is 5 s, and `Never` is allowed.
- **Expired requests are still dispatched** and late replies dropped.
  Skipping them is opt-in (rank 19).
- **Caller give-up does not cancel server work.** `cancel_on_caller_gone` is
  an opt-in coupling, like a server monitoring the caller's alias.
- **`stopped()` may reply**, and callers learn at death.
- **Replies are immediate.**
- **Completions are turns** (`handle_info`).
- **A keyed job (`JobOptions::key`) is shorthand** for cancel then start.

## Decisions on rank 11a

1. **Promise pipelining: not now.** In-process there is no round trip to
   save, and no client needs it. Error propagation is already provided:
   piped tokens always get a terminal discharge. Revisit with rank 44
   (distribution).
2. **Replies are sent immediately, not held until the turn ends.** This is
   OTP. A panicking instance is discarded, and external effects cannot roll
   back. The slot model covers a reply followed by a panic in the same turn.
3. **Per-turn snapshots** are decided by ranks 21 and 30.

## Reuse by later ranks

| Rank | Reuses |
|---|---|
| 11b | `JobState`, the emit protocol, fencing, `done`, credits, `PipeError` |
| 13 | `stopped()` replies, the registry outstanding count, the unfinished report |
| 15 | the per-instance job set, keyed cancel-previous, the bounded join |
| 15a | a slot is a request scope; `JobOptions::deadline` is never later than its parent's |
| 16 | stash keeps the envelope's slot |
| 17-19 | `Overloaded`; the send-time deadline; opt-in stale skipping at dispatch |
| 12, 26 | `ActorId` fencing for durable addresses |

## Migration notes

**Errors.**

- `ActorError` is `#[non_exhaustive]` and gains `ReplyDropped` and
  `Overloaded`; add a wildcard arm to exhaustive matches.

**Handlers.**

- Manual `impl Handler<M>` takes `(msg, reply: PendingReply, ctx)` and calls
  `reply.reply(v)`. `#[actor]` code is unchanged.
- Direct calls of manual impls in tests use `spawned_test::reply_pair()`.

**Timeouts.**

- The request timeout starts at send, not at first poll.
- `request_raw(msg)` becomes `send_request(msg, deadline)`; 0.7's
  unbounded wait is `send_request(msg, Deadline::Never)`.

**Raw API.**

- `send_request` returns `ReplyReceiver<R>`, which yields
  `Result<R, ActorError>`.
- `Response::from_with_timeout` is removed.
- `Receiver<M>` implementors (including `Probe`) implement
  `send_request`.

**Lifecycle.**

- `join()` waits for cleanup completion: the job join is bounded by the cleanup
  deadline, but a stuck `stopped()` or destructor keeps it waiting (the report
  marks cleanup incomplete). `wait_exit()` observes death.
- Recipes for the guide:
  - await in a handler becomes `defer` plus `pipe_reply`;
  - a `HashMap<Id, oneshot::Sender>` becomes `PendingReplies`;
  - a `oneshot::Sender` inside a message becomes an ordinary request with a
    `#[deferred_handler]`;
  - a polling loop becomes a deferred request answered when capacity frees.

## Alternatives considered

| Alternative | Why not |
|---|---|
| Await in the handler | Blocks the mailbox. |
| Futures stored in state | Hidden interleaving with `&mut self`; no ownership on restart. |
| Blanket `Handler<M>` from a `DeferredHandler<M>` | E0119 against `Mock`; cannot implement `handle -> R`. |
| Keep `handle -> R` and synthesize it for deferring handlers | Needs `R: From<ActorError>` or awaits a local slot (deadlocks on deferral). |
| `#[deferred]` on `#[protocol]` | Leaks server strategy into callers. |
| Envelope admission before every handler | Rejects fast in-place answers while slow ones are parked. |
| Already-answered token at the bound | Effects run after the caller was told `Overloaded`. |
| First-poll caller timer plus a server timer | Two timeout reasons. |
| Skipping expired requests by default | Not OTP; no credit depends on it. Opt-in at rank 19. |
| Cancelling linked jobs on caller give-up by default | Not OTP; the token credit already returns. Opt-in. |
| Cancelled-job table | Grows without bound or races queued emits. |
| `forward` and `map` in v1 | Cross-actor credit and deadline hand-off races; no client needs them. |
| Waiting at the bound | Blocks the turn; rank 17. |
| Pipelining, transactional turns | See the rank 11a decisions. |

## What the models establish

The models are bounded TLA+ abstractions. They check the protocol's
**ordering and accounting decisions**: which transitions exist, who may
discharge or fence what, and when credits are held. They do not check Rust
memory orderings, the slot's `WRITING` publication, wakeups, the
deadline-heap data structure, API shapes or actual Rust schedules. Those need
Loom/Shuttle tests and trace validation once implemented. Measurements and
the negative controls (each checked against its named invariant) are in
[`verification/tla/README.md`](../../verification/tla/README.md#deferred-replies-rank-11-design).

**`DeferredReplies.tla`** has a discrete clock, two callers, owner A with
turns, a linked-job count per request, and downstream actor B.

It establishes, within its bounds:

- **Discharge.** At most one discharge per request (`AtMostOneDischarge`), and
  no open slot without a live holder, including job-held tokens at death
  (`NoOrphan`).
- **Deadlines.** No success at or after the deadline
  (`NoSuccessAfterDeadline`); deadlines are serviced while a turn is stuck
  (`TimelyExpiry`); shortening never extends past the caller's deadline
  (`DeadlineWithinCaller`).
- **Credits.** Combined token and linked-job accounting with attach-on-open
  and no credit reuse across two running jobs (`BoundedA`), and B's own bound
  (`BoundedB`). The model's `CreditOf` is the design's per-request
  `credits(word) = max(running linked jobs, live token)`, so both use the same
  count, not separate credit identities. The model computes it from state; it
  does not check the incremental counter deltas of the packed CAS, which is
  Loom territory.
- **Exit.** Unwind-local drops answer `Stopped` (`PanicStopped`). A stuck
  owner (cleanup deadline passed while `stopped()` runs) closes nothing
  early: no token is answered `Stopped` before death (`NoEarlyClose`).
  `stopped()` may still reply while stuck, and death may never follow.
- **Death boundary.** Death is the three steps of `actor_cleanup.rs`: the
  death point fences the reply gate and seals the registry, then the batch
  answers every held token and queued request one at a time (callers and the
  deadline service interleave), then the collected wake-ups run. No reply is
  admitted after the death point (`NoReplyAfterDeath`, control
  `NoReplyFence`), and a caller woken with `Stopped` finds the whole death
  answered (`WokenAfterDischarge`, control `CallbackBeforeDischarge`).
- **Downstream cleanup.** A downstream request ends with its last job
  (`DownstreamOwned`).
- **Liveness without handler fairness:** `EveryAcceptedDischarged` and
  `CallerNeverHangs`.

Abstractions:

- expiry of polled queued slots is fair, standing for a polling caller;
- B is reduced to its outcome and bound;
- a job count stands in for job identities;
- `done` panics are handler panics (`Panic`).

**`DeferredJobs.tla`** has restarts with new `ActorId`s, unlinked and keyed
jobs, optional deadlines, a FIFO mailbox and in-flight emit credits.

It establishes:

- **Fencing.** `NoLateResult`, `NoStaleAfterCancel`, and `NoStaleKeyed`
  (including replacement after notification).
- **Terminal messages.** `AtMostOneTerminal` and `TerminalLast`.
- **Bounds.** `PermitsBounded` (running work keeps its permit), `OneLivePerKey`
  and `EmitCreditsBounded`.
- **Held results.** A held result keeps its credit until its terminal
  message (`HeldCounted`) and its deadline stays armed
  (`DeadlineArmedWhileHeld`).
- **Owner exit.** Admission and `cancel_all` are serialized, and the exit
  cancels running work, scheduled emits and held results alike
  (`ExitCancelsJobs`, controls `NoAdmissionFence` and `ExitKeepsHeld`).
- **Liveness.** `JobReported` for jobs with a deadline.

Abstraction: credits return at dispatch (fenced or not); mailbox disposal at
death is not modelled separately because messages stay addressed to the slot.

**Not modelled:**

Requests and timing:

- unpolled or paused receivers of non-deferred requests (the model treats
  "waiting" as polling);
- `Deadline::Never`;
- real deadline latency, and the cleanup deadline's duration;
- per-job downstream receivers and their deadlines (B is per request).

Tokens and synchronization:

- token drops by other tasks during an unrelated turn;
- `WRITING` versus registry close (the reply permit is modelled as the
  reply gate closing at the death point; the CAS race is Loom territory);
- the packed-word counter deltas;
- memory orderings and wakeups.

Jobs:

- blocked-emitter wakeups (`CanEmit` is a precondition, not a blocked
  caller woken by cancellation);
- one key class and one job bound (no `MAX_NATIVE_JOBS`);
- cross-model coupling: a fenced completion's `done` dropping its token is
  approximated by `JobCancel` in `DeferredReplies.tla`, and the models are
  not composed.

Lifecycle:

- destructor hangs;
- managed incomplete-cleanup ownership (pending-work handle, no replacement
  while stuck), which the supervision and execution-group models own;
- the join barrier and the unfinished report.

API and helpers:

- `PendingReplies`;
- the `pipe_reply` owner check;
- API dispatch (detached sends, `Mock`);
- native spawn failure;
- heap compaction;
- deadline-before-admission at equality: the model's `Defer` and
  `PipeReply` check `OPEN`, not `now >= deadline`, so at the exact deadline
  instant they may admit before expiry (the design checks the deadline
  first);
- cleanup-close liveness: `CleanupDeadline` has no clock guard or fairness,
  so the model does not establish that the deadline triggers the stuck
  report, only what holds if it does.

Note on `NoCancelFence` under `MCDeferredJobsEmits`: it can fail through a
queued terminal message, so it is not an emit-specific witness. Emit fencing
is checked by `NoLateResult` and `NoStaleAfterCancel` on the emits
configuration, where emits are reachable. `TerminalLast` checks terminal
ordering and `EmitCreditsBounded` checks in-flight emit credits.

## Acceptance (from the roadmap)

**Example:**

- routes a slow request while accepting another;
- shows `Overloaded` at the bound;
- stops the owner with stored and job-held tokens: callers get
  `ActorStopped` at death, and `stopped()` can answer first;
- hangs `stopped()` past the cleanup deadline: the instance reports cleanup
  incomplete, no replacement starts, no caller gets `ActorStopped` early, and
  callers are answered by their own deadlines;
- shows a late completion and a replaced keyed job's queued messages being
  fenced.

**Tests in both modes and under `spawned_sim`:**

- every discharge path;
- a leaked token answered by its deadline;
- a hung blocking job notified at its deadline while it keeps its credit;
- a panicking `done`;
- a blocked emitter released by cancellation;
- Loom tests of the slot protocol (reply versus shortening versus expiry
  versus abandonment).

**Trace validation:** against both models.

## Implementation notes

Mechanism choices made by the implementation (PR #239). None changes the
semantics above.

- **Value cell.** The crates are `#![forbid(unsafe_code)]`, so the slot's value
  and the caller's waker live under one uncontended `Mutex` instead of an
  `UnsafeCell` plus `AtomicWaker`. The packed word, its CAS rules and every
  credit delta are as specified; the reply reads the deadline under the same
  lock that a shortening takes, which makes a shortening after the `WRITING`
  CAS a no-op (Loom: `reply_vs_shortening`).
- **Receiver and timer.** `ReplyReceiver<R>` is one pointer and implements
  `IntoFuture`; the deadline timer lives in the awaiting `ReplyFuture`. On
  the `request` and `Response` paths, when the runtime's timer fires the
  receiver expires the slot, so paused Tokio clocks keep working. A raw
  receiver awaited directly registers its deadline in the owner's heap
  instead (no runtime timer per request), so it follows the deadline
  service's clock, not a paused Tokio clock.
- **Deadline service.** Unmanaged and managed actors share one process-wide
  service thread (not a task on the `ActorSystem` control runtime); a dead
  owner is forgotten at its registry close. Under `spawned_sim` each owner's
  heap is served by a task of the simulated runtime, so clock domains never
  mix.
- **Cleanup deadline.** Armed when exit begins: the supervisor's
  `shutdown_timeout` for a supervised child, the remaining system grace
  during `ActorSystem::shutdown`, otherwise `DEFAULT_STOP_DEADLINE` (5 s).
  Jobs still running then appear in `ShutdownReport::unfinished_jobs` and as
  an `unfinished_at_stop` event.
- **Async emits.** Every `pipe*` hands the work its `JobCtx` (`emit`,
  `cancelled`): `ctx.pipe(opts, |job| async move { .. }, done)`; `|_| fut`
  when the work needs neither.
- **Destruction.** If an owner's lifecycle future is dropped (its runtime
  destroyed), exit begins before its state is dropped and the registry
  closes, so open tokens answer `ActorStopped`.

## Open questions

1. Exact names (`PendingReply`, `DeferError`, `PipeReplyError`) and whether
   `#[request_handler]` stays the default attribute in `#[actor]`.
2. The provisional defaults: 16 emit credits, `min(4, available_parallelism)`
   native jobs and a 5 s stop deadline. All are configurable; benchmarks will
   set them.

## Review resolutions

Round 1 (Codex: rework; agy: approve with changes) is mapped in PR #221. The
round-2 reviews of the revised design follow (Codex: rework; agy: ready with
changes).

### Round 2, Codex

| Finding | Resolution |
|---|---|
| 1. Slot synchronization protocol | §1 "Slot synchronization protocol": the `WRITING` state serializes reply and deadline read; shortening is `fetch_min` plus a re-check, with the `WRITING` CAS as linearization point; the value is published before `REPLIED`; then the order credit release, registry removal, wake. |
| 2. Panic drop of a local `ReplyTo` | §4 drop classification: drops during `InTurn` commit at turn end (`Stopped` on panic). Model: `PanicStopped` / `UnwindDropped`. |
| 3. Linked credit attach and reuse; failure disposition; `WrongOwner` shape | §8 attach CAS (`OPEN` and `JOB` clear), fresh credit when a job still runs, `Closed` hands the token back; §9 failure disposition and `PipeReplyError<R>`. Model: `BoundedA` with `ShareRunning` and `AttachClosed`. |
| 4. Keyed replacement after notification | §9 keyed replacement cancels whatever the notification state. Model: `ToReplace`, `NoStaleKeyed`, control `ReplacePendingOnly` (the review's trace). |
| 5. Emit terminal boundary, credit lifecycle, blocked emitters | §9 emit protocol: check and enqueue under the job lock, no emits after the terminal message, credit return at dispatch, fence or death, wake on cancel. Model: FIFO mailbox, `TerminalLast`, `EmitCreditsBounded`, controls `EmitAfterTerminal` and `NoEmitCredit` on the emits configuration. |
| 6. Old entry point requires error-capable `R`; `send` dispatch | §3 explicit trait migration: `handle(msg, PendingReply, ctx)`; `#[actor]` generates it; detached `PendingReply` for sends; inherent methods and `reply_pair()` for direct calls; `Mock` adapted. |
| 7. Deadline service bounds and lifetime; job deadlines | §2 per-actor heaps (stale includes superseded), per-actor compaction, `Weak` entries, earlier-deadline re-registration, system and unmanaged service lifetimes, sim isolation, job deadlines in the same heap. |
| 8. `Response::from` / `from_with_timeout`; timeout before enqueue; threads ready; death ordering; acceptance wording | §1 API table; §5 lifecycle steps 1-4 (registry close, then mailbox, state, heap, then exit publication, monitors and links, then join); acceptance now says "at death". |
| `ExpectedRequestId` leftover in the client mapping | Unknown ids are treated as late responses and ignored at trace level. |
| Model gaps table | Models extended (combined credits, exiting phase with unwind drops, downstream cleanup, optional job deadlines, FIFO and emit credits, notified replacement). Remaining gaps are listed under "Not modelled". |
| Controls check aggregate `Safety`; no emit coverage | `negative_checks.py` `named_fault` checks `TypeOK` plus the named invariant only; emit controls run on `MCDeferredJobsEmits`. |
| OTP: caller-give-up cancellation; skipping expired requests; `Task.async` analogy | Opt-in cancellation; expired requests dispatched (opt-in skip, rank 19); job-cancellation policy re-categorized as B and stated as a chosen policy. |
| Open questions 1-3 | Adopted: explicit trait migration; bounded join keeping ownership; configurable provisional defaults after the credit protocol (§8, §9). |

### Round 2, agy

| Finding | Resolution |
|---|---|
| 2.1.1 Read before publish | `WRITING` state; `REPLIED` stored after the value (§1). |
| 2.1.2 Permit install racing a terminal slot | `TOKEN` bit set by a CAS that requires `OPEN` (§4). |
| 2.1.3 Undispatched envelope drop | Mailbox disposal discharges `STOPPED` (§5 step 3.2); only a turn's `PendingReply` drop gives `Dropped`. |
| 2.2.1 `request_raw` gains a timeout | `send_request(msg, deadline)` takes it explicitly; `Deadline::Never` keeps the unbounded wait (§1, migration). |
| 2.2.2 Stale doc comments in code | Implementation task: `macros/src/lib.rs` "timers start on poll" and `tasks/actor.rs` request docs. |
| 2.3.1 Unpolled expired requests dispatched | Dispatched by design (OTP); `defer()` on an expired slot returns `Closed`; skipping is opt-in (rank 19). |
| 2.3.2 Process-wide service on the first runtime | Dedicated service thread for unmanaged actors; per-sim-instance under sim (§2). |
| 2.3.3 Superseded entries in compaction | Counted as stale (§2). |
| 2.4.1 Drop while exiting should answer at once | `Exiting` drops answer `Stopped` at once (§4). |
| 2.4.2 Registry close before the job join | Death (close, mailbox, state, heap) precedes the job join (§5). |
| 2.4.3 Standalone actors have no cleanup deadline | The run loop arms one for every actor (§5 step 1). |
| 2.5.1 Attach after expiry | Attach CAS requires `OPEN`; otherwise `Closed` with the token (§8). |
| 2.5.2 `TimedOut` does not trigger cancellation | The opt-in coupling triggers on `ABANDONED` and `TIMED_OUT` (§9). |
| 2.5.3 `WrongOwner` shape; token lost on error | `PipeReplyError<R>` returns the token in every variant (§9). |
| 2.5.4 No `pipe_reply_with` | `pipe_reply` and `pipe_reply_blocking` take `JobOptions` first (§9). |
| 2.6.1 Keyed replacement after a deadline | Fixed (Codex 4). |
| 2.6.2 Fenced payload drop panics | `DropFailures` path (§1 rule 5). |
| 2.7.1 Emit credit deadlock on stop | Cancel and exit wake blocked emitters with `JobGone`; credits also return at mailbox disposal (§9). |
| 2.7.2 Emits after the deadline | Emit check and terminal enqueue are serialized under the job lock (§9). |
| 2.8 `R: From<ActorError>`, local-slot deadlock | Removed; explicit trait migration (§3). |
| 2.9.1 Threads `Response` stays eager | Stated (§1 API table). |
| 2.9.2 `Probe<M>` | Implements `send_request`; unexpected requests answer `Stopped` (§1). |
| 2.9.3 `PendingReplies::len(&self)` | Replaced by non-mutating `retained` and `live`; purging is in `&mut` methods (§7). |
| 2.9.4 Metrics type name | `metrics::Snapshot` and `RequestObservation::finish` (§1). |
| 3.A model divergences | `HeldA` replaced by `Credits` with two holders; expiry abstraction stated; `pipe_reply` notification is a turn (`JobNotify`); cancellation is opt-in (`JobCancel` any closed slot). |
| 3.B unchecked behaviours | Exiting phase, `stopped()` replies, unwind drops, linked credit coupling, FIFO and emit credits are now modelled; the rest are listed under "Not modelled". |
| 3.C controls | Named-invariant checks; `NoCancelFence` also runs on the emits configuration. `NoAtomicExpiry` remains an abstract double-discharge mutation; the real race is for Loom (see "What the models establish"). |
| 4.1 Caller-give-up default | Opt-in (user decision). |
| 4.2 Skipping expired requests | Not the default; opt-in at rank 19 (user decision, OTP). |
| 5.1-5.3 open questions | Explicit trait migration with inherent methods and `reply_pair()`, without panicking stubs; bounded join with condvar-signalled thread joins; configurable provisional defaults. |

### Round 3, Codex (no-go: two blockers)

| Finding | Resolution |
|---|---|
| Blocker 1: a cleanup deadline is not death | Superseded in round 4: the close now happens only at death. Round 3 separated a reply close (at the earlier of `stopped()` returning and the cleanup deadline) from death (only after `stopped()` returns). **Cleanup incomplete (stuck)** follows the execution-group contract: incomplete report with a pending-work handle, no replacement while alive, `join()` unbounded if stuck. This is listed as a Rust-forced difference from OTP. Model: `CleanupDeadline`, `StuckClosed`, control `CloseOnlyAtDeath`. |
| Blocker 2: one synchronization domain | §1 packed slot word: state, `TOKEN`, `REG` and `JOBS` in one atomic. The state machine table puts every credit, attach, registry and terminal operation in a single CAS; counter deltas come from `credits(new) - credits(old)`. Orderings and wakers are left to Loom. |
| Round 2, item 3: linked accounting | The per-request count `max(JOBS, live token)` replaces the two-holder identity; it matches the model's `CreditOf`, as disclosed in the models section. |
| Credit identity coverage | The design now uses the same per-request count as the model (`credits(word)`), so there is no identity gap. Incremental counter deltas are listed as not modelled. |
| Uncomposed models | Listed under "Not modelled" (cross-model coupling). |
| "Not modelled" incomplete | Every listed item added. |
| Emits-config `NoCancelFence` can fail via a terminal message | Noted. |
| W:90 `InsertError::Full`; W:216 handler signature; D:154 vs D:167 receiver timer | Mapping fixed (`token.overloaded()`, new `handle` signature); receiver wording unified (one deadline, polled sleep). |

### Round 3, agy (go with small doc fixes)

| Fix | Resolution |
|---|---|
| 1. `overloaded()` on `PendingReply` and `ReplyTo` | Added (§4); the mapping uses `token.overloaded()` on `InsertError::Full` and a full waiter queue. |
| 2. Manual handler signature in the mapping | Updated to `handle(msg, PendingReply, ctx)`. |
| 3. `defer()`, attach and `is_closed()` check the deadline first | Stated (§1 state machine `[deadline]`, §2, §4). |
| 4. Cleanup deadline while `stopped()` runs | Registry close at the deadline; death steps wait for `stopped()` to return (§5, the same resolution as Codex blocker 1). |
| 5. Packed word and running-job count; "Not modelled" items | Packed word with `JOBS` count (§1, §8); items added. |
| OTP table: `send` through the same handler | Added as category T. |

### Round 4 (agy: go; Codex: no-go on the early close)

| Finding | Resolution |
|---|---|
| Codex: registry close at the cleanup deadline contradicts the death boundary | User decision: the close happens only at actual death; `ActorStopped` means "gone". A stuck `stopped()` reports cleanup incomplete, may still reply, blocks replacement, and callers are bounded by their own deadlines (§5). OTP difference row updated (R: OTP's kill makes death and close coincide). Model: `CleanupDeadline` no longer closes; invariant `NoEarlyClose`; control `EarlyClose` (the mutation that closes early) replaces `CloseOnlyAtDeath`. |
| Codex: terminal "store" wording | State-field CAS preserving concurrent `JOBS` changes (§1 rule 1). |
| Codex: 8-bit `JOBS` | 16 bits; `MAX_OUTSTANDING` is capped at 65535 at configuration. |
| Codex: dead-with-incomplete-job-cleanup vs still alive | Separate cases (§5 step 4 versus "Actor cleanup incomplete"). |
| Codex: duplicate `wait_exit()` line; agy: duplicate Credits bullet | Removed. |
| Codex: deadline-before-admission at equality; cleanup-close liveness | Listed under "Not modelled". |
| Codex: emit-fencing control labels | The note now names `NoLateResult` and `NoStaleAfterCancel`; `TerminalLast` and `EmitCreditsBounded` are described separately. |

