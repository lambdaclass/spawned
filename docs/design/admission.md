# Admission package (design, revision 1, in review)

Status: design and TLA+ model for review (PR #300), not implemented. It
covers the ROADMAP's admission package: bounded mailboxes and admission
(rank 17), weighted credits (rank 18), stale-work skipping and deadlines
(rank 19), and the control lane modelled on OTP 28 priority messages
(rank 14, gap G11). Bounded draining (rank 13) and per-actor memory limits
(rank 18a) are listed with the package but stay out of this design
(section 2). Every guarantee is reviewed against the
[semantics contract](semantics.md); section 9 lists the entries this
design adds. Model: [`Admission.tla`](../../verification/tla/Admission.tla)
and [`AdmissionCycle.tla`](../../verification/tla/AdmissionCycle.tla),
TLC-only until the code exists.

## Contents

- [Summary](#summary)
- [1. The problem](#1-the-problem)
- [2. Scope](#2-scope)
- [3. OTP precedent and deviations](#3-otp-precedent-and-deviations)
- [4. API](#4-api)
- [5. Semantics](#5-semantics)
- [6. Interactions](#6-interactions)
- [7. Cycles and deadlocks](#7-cycles-and-deadlocks)
- [8. Cost on the default path](#8-cost-on-the-default-path)
- [9. Semantics contract changes](#9-semantics-contract-changes)
- [10. Implementation sketch](#10-implementation-sketch)
- [11. The model](#11-the-model)
- [12. Trace events and their mapping](#12-trace-events-and-their-mapping)
- [13. Test plan](#13-test-plan)
- [14. Client impact](#14-client-impact)
- [15. Alternatives considered](#15-alternatives-considered)
- [16. Release: 0.9 or 0.10](#16-release-09-or-010)
- [17. Open questions for the maintainer](#17-open-questions-for-the-maintainer)

## Summary

| # | Decision | Why |
|---|---|---|
| 1 | **Opt-in per actor type, at compile time.** `#[actor(mailbox = N)]` sets `Actor::MAILBOX`; the default stays unbounded (D5). | An unbounded actor compiles to today's send path; the bound is a robustness feature with no default-path cost. |
| 2 | **One mechanism for slots and bytes: a weighted credit pool.** Each message takes `Message::weight()` credits (default 1, so the pool counts slots); a weight that estimates bytes makes it rank 18's byte budget. A pool is private to a mailbox or shared by several (`CreditPool`), which bounds a pipeline. | Rust cannot measure a value's heap size; the user's estimate is the only honest measure. One mechanism keeps one model. |
| 3 | **Calls that cannot wait never wait.** `send`, `try_send`, `send_request`, and every send from synchronous code fail at once with `MailboxFull` when the pool is short. | A synchronous call cannot await, and a handler that blocks also blocks its own mailbox. |
| 4 | **Calls that can wait do so in FIFO order under one deadline.** `request` and protocol calls wait for credits inside the request's own deadline; `send_wait(msg, deadline)` is the waiting send. Freed credits are handed to the oldest waiter in the same step, and the hand-off admits the waiter's message for it. | No barging or starvation, and a registered caller that is never polled cannot hold the queue. R3's single deadline is kept. |
| 5 | **Never admitted is a distinct outcome.** A message that never got credits fails with `MailboxFull` (or `TooLarge` if it exceeds the whole pool), never `RequestTimeout`. | The caller then knows the message did not run, which a timeout cannot tell it (R4). |
| 6 | **Framework items take no credit and are never refused.** These are `Down`, trapped `Exit`, parent exits, job outcomes and emits, and continuations; each is bounded by its own source (one per edge, or by `MAX_OUTSTANDING`). | Supervision, monitors and deferred replies keep working when user traffic fills the mailbox. |
| 7 | **The death point releases everything.** At D, request waiters get `ActorStopped(final reason)`, send waiters get `NoProc` with their message back, and every credit the instance holds returns to its pool. | A crash loop on a shared pool cannot leak capacity. This extends D3, D4 and R1 to waiters. |
| 8 | **Stale work is skipped only on opt-in, and only if nobody can be answered.** `#[request_handler(skip_expired)]` skips a request whose reply slot is already closed (the caller timed out or left), and `send_expiring` skips a send past its deadline. | OTP dispatches expired calls; the deferred-replies design already chose opt-in (rank 19). |
| 9 | **The priority lane is opt-in in two steps, as OTP 28.** The actor type enables the lane (`#[actor(priority_lane)]`); the receiver then marks sources as priority: a priority recipient (OTP's priority alias), a priority monitor and a priority link. A valve dispatches one ordinary item after `burst` priority turns in a row. | Senders cannot impose priority, as in OTP. The valve is the bound ROADMAP rank 14 asks for: priority must not starve user work. |
| 10 | **Inherited deadlines are explicit in v1.** Use `reply.deadline()` with `Deadline::min`; propagating a turn's deadline automatically is left as an open question. | Automatic propagation needs a lookup on every request, which the default path cannot pay for. |

## 1. The problem

The evidence comes from production and from the semantics contract:

- **Memory.** Ethrex lost about 1.5 GB a day to unbounded per-connection
  mailboxes and `spawn_listener` streams
  ([d0b9fecb9](https://github.com/lambdaclass/ethrex/commit/d0b9fecb9);
  ROADMAP, "Client search results"). The workarounds were a bounded channel
  plus a raw writer task, and a 150-connection semaphore whose permit lives
  in the actor's state. In both clients, the inbound `spawn_listener`
  streams are still unbounded. D5 states the gap directly: "nothing bounds
  queue memory", and section 10 lists bounded memory as not guaranteed.
- **No way to say "not now".** In ethrex, a `get_best_peer` request timed
  out under mailbox pressure and the node exited
  ([63167661a](https://github.com/lambdaclass/ethrex/commit/63167661a)).
  The caller cannot tell a slow server from one that never received the
  request, and R4 says it cannot tell whether the request ran. Ethlambda
  has a typed `Ignore(Overloaded)` verdict and five semaphores that give
  it, all built by hand.
- **Stale work.** Expired requests are still dispatched (R3, as in OTP).
  `PendingReply::is_closed()` lets a handler check by hand; the
  deferred-replies design left the opt-in skip to rank 19
  ([deferred-replies.md](deferred-replies.md) §2).
- **Control behind the backlog.** Trapped `Exit` and `Down` wait behind
  every queued message (X7; section 10, "Selective receive, priorities or a
  fair control lane"). An observer flooded with user traffic learns of a
  peer's death late.

## 2. Scope

**In scope:**

- Rank 17, bounded mailboxes and admission: reject and wait semantics,
  cancellation, deadlines, and cyclic sends.
- Rank 18, weighted credits: bytes in flight, pipelines, and oversize
  messages that must not wait forever.
- Rank 19, opt-in expiry before dispatch, distinct outcomes, and explicit
  deadline inheritance.
- Rank 14 and G11, the receiver-opted priority lane.

**Out of scope, with reasons:**

- **Rank 13, bounded draining.** The ROADMAP pairs it with rank 15a
  (cancel scopes): "draining is closing the actor's scope under its
  deadline". This design leaves a hook for it: a pool that can be closed
  while the actor still runs, refusing new admissions with `NoProc`. The
  drain policy, joining and reporting belong to the scope design.
- **Rank 18a, per-actor memory limits.** These bound the actor's own state,
  not messages in flight, through an opt-in size hook checked between
  turns. They touch nothing here except that both report a resource limit.
- **Ranks 36 and 42** (coalescing, CoDel-style shedding) and **rank 33**
  (demand-driven streams) build on credits later. A `CreditPool` is not a
  general-purpose semaphore in v1, so rank 17's "lease pool with waiters"
  for peer leasing (ethrex) stays a deferred-reply pattern (rank 11).

## 3. OTP precedent and deviations

OTP has no mailbox bound. A process whose heap, including an on-heap
message queue, grows past `max_heap_size` is killed; senders never fail or
wait. OTP 28 added priority messages: the receiver opts in per source, with
a priority alias (`alias([priority])`), a priority monitor
(`monitor(.., [priority])`) or a priority link (`link(.., [priority])`).
Priority messages are received before ordinary ones and keep their order
within the class. The sender cannot impose priority.

| Topic | OTP | Spawned | Kind of deviation and its reason |
|---|---|---|---|
| A mailbox bound | None; `max_heap_size` kills a process that grows too large | Opt-in credit pool checked at admission | **A bound Erlang lacks, forced by Rust.** Rust has no per-actor heap to measure or kill (semantics §11, `max_heap_size`), so the only place to bound queued memory is admission. |
| A message's size | The BEAM knows a term's flat size | `Message::weight()`, supplied by the user | **Rust-forced.** Rust has no generic deep size of a value. |
| `gen_server:cast`, `!` | Never fail | `send` fails at once with `MailboxFull` on a full bounded actor | **Rust-forced.** `send` is a synchronous function and cannot await; blocking it would block an async worker, or, from a handler, the sender's own mailbox. `send_wait` is the waiting form. |
| `gen_server:call` | Never fails for capacity | `request` waits for credits inside its deadline; `MailboxFull` if it is never admitted | **Rust types add strictness.** The single deadline (R3) and the waiting behaviour match `call`; the typed outcome tells "never admitted" apart from "unknown" (`RequestTimeout`). |
| Expired calls | Dispatched | Dispatched; the skip is opt-in per handler | No deviation by default. |
| Priority opt-in | Per source, at run time | Per source at run time (recipient, monitor, link), after the actor type enables the lane at compile time | **Rust-forced cost.** The BEAM checks for priority messages in the VM. Spawned's receive loop is monomorphised per actor type, so a compile-time switch keeps the lane check out of every other actor's loop (section 8). |
| Priority starvation | Strict: priority messages can starve ordinary ones | A valve: after `burst` priority turns in a row with ordinary work waiting, one ordinary turn | **A bound Erlang lacks.** ROADMAP rank 14 requires that priority cannot starve user work. `burst = u32::MAX` gives OTP's strict order. |
| `exit/3` with `[priority]` | A sender-side option that targets a priority alias | Not in v1 | It needs exit signals addressed to aliases, which spawned does not have. |
| Deadlines in messages | None | Explicit (`reply.deadline()`, `Deadline::min`) | An addition; OTP has no deadline to inherit. |

## 4. API

The API is the same in both modes. Only waiting differs: tasks mode
returns a future, and threads mode blocks the calling thread.

### 4.1 Choosing a bounded mailbox

```rust
/// Compile-time mailbox kind of an actor type (an associated const, so an
/// unbounded actor's send and receive paths are unchanged code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mailbox {
    Unbounded,
    /// A private pool of `credits`, overridable per start (`StartOptions`,
    /// `ChildSpec`) or replaced by a shared `CreditPool`.
    Bounded { credits: NonZeroU32 },
}

pub trait Actor: Send + Sized + 'static {
    // existing items unchanged
    const MAILBOX: Mailbox = Mailbox::Unbounded;
    const PRIORITY_LANE: Option<PriorityLane> = None;
}

#[actor(protocol = PeerProtocol, mailbox = 1024)]          // Bounded { credits: 1024 }
#[actor(protocol = ChainProtocol, priority_lane(burst = 64, credits = 64))]
```

`#[actor(mailbox = 0)]` is a compile error. These are per-start overrides,
added to the options of unified start
([unified-start.md](unified-start.md) §2.4 reserved `mailbox(..)`):

```rust
impl<A: Actor> StartOptions<A> {
    pub fn mailbox_credits(self, credits: NonZeroU32) -> Self; // private pool of this size
    pub fn credit_pool(self, pool: CreditPool) -> Self;        // a shared pool instead
}
impl ChildSpec { pub fn mailbox_credits(..) -> Self; pub fn credit_pool(..) -> Self; }
```

On an actor type whose `MAILBOX` is `Unbounded`, either option is refused:
`StartError::InvalidOptions("mailbox options on an unbounded actor")`, or
`SupervisorError` when the spec is built. The kind is a property of the
type; only the size and the pool vary per start. A restarted child gets a
fresh private pool of the same size (M4), or rejoins its shared pool.

### 4.2 Weights and shared pools

```rust
pub trait Message: Send + 'static {
    type Result: Send + 'static;
    /// Credits this message takes in a bounded mailbox; read only when the
    /// target is bounded. 0 is treated as 1.
    fn weight(&self) -> u32 { 1 }
}

#[protocol]
pub trait BlockProtocol: Send + Sync {
    #[weight(block.encoded_len() as u32)]      // an expression over the arguments
    fn import(&self, block: Block) -> Response<Result<(), ImportError>>;
}

#[derive(Clone)]
pub struct CreditPool { /* Arc */ }
impl CreditPool {
    pub fn new(credits: NonZeroU32) -> Self;
    pub fn credits(&self) -> u32;
    pub fn in_use(&self) -> u32;     // a snapshot
    pub fn waiters(&self) -> usize;  // a snapshot
}
```

A shared pool bounds the sum of what its mailboxes have queued. Credits
are held from admission until the item is dequeued for dispatch (or
discarded), not across the turn, so a pipeline A → B → C on one pool
bounds its queued payloads, not the item each stage is handling.

### 4.3 Sending

```rust
impl<A: Actor> ActorRef<A> {
    // Never waits. Bounded and short: Err(MailboxFull); weight > pool:
    // Err(TooLarge). The message is dropped, as on NoProc today.
    pub fn send<M>(&self, msg: M) -> Result<(), ActorError> where A: Handler<M>, M: Message;
    // Never waits; gives the message back.
    pub fn try_send<M>(&self, msg: M) -> Result<(), SendError<M>> where ..;
    // Waits for credits until `deadline`, FIFO. Unbounded: ready at once.
    pub fn send_wait<M>(&self, msg: M, deadline: impl Into<Deadline>) -> SendWait<'_, A, M> where ..;
    // Skipped at dispatch once `deadline` passed (rank 19); never waits.
    pub fn send_expiring<M>(&self, msg: M, deadline: impl Into<Deadline>) -> Result<(), ActorError> where ..;
    // Never waits: Err(MailboxFull) at once when short.
    pub fn send_request<M>(&self, msg: M, deadline: impl Into<Deadline>) -> Result<ReplyReceiver<M::Result>, ActorError> where ..;
    // Waits for credits inside the request's own deadline, then for the reply.
    pub fn request<M>(&self, msg: M) -> impl Future<Output = Result<M::Result, ActorError>> + '_ where ..;
}

/// `SendWait` is a future in tasks mode; in threads mode `send_wait`
/// blocks and returns the result.
impl<M> Future for SendWait<'_, A, M> { type Output = Result<(), SendError<M>>; }

#[derive(Debug)]
#[non_exhaustive]
pub enum SendError<M> {
    Full(M),     // never admitted: short at once (try_send) or at the deadline (send_wait)
    TooLarge(M), // weight larger than the whole pool; never waits
    NoProc(M),   // past the death point, or released by it while waiting
}
impl<M> SendError<M> { pub fn into_inner(self) -> M; pub fn kind(&self) -> SendErrorKind; }

#[non_exhaustive]
pub enum ActorError {
    // existing variants unchanged
    /// The message was never admitted: its bounded mailbox had no credits
    /// at once (a call that never waits) or before its deadline.
    MailboxFull,
    /// The message's weight exceeds its pool's credits; it can never fit.
    TooLarge,
}
```

| Variant | `target_gone` | `is_timeout` | `is_transient` |
|---|---|---|---|
| `MailboxFull` | `false` | `false` | `true` |
| `TooLarge` | `false` | `false` | `false` |

`Recipient<M>` has the same four calls (its `send_wait` boxes the future,
since `Receiver<M>` is object-safe). `Context<A>` has them for self-sends:
they count against the actor's own pool and never wait. A protocol method
on a bounded actor registers its waiter when it is called (as it enqueues
when called today), so its FIFO position and deadline start at the call.

### 4.4 Skipping stale work

```rust
#[request_handler(skip_expired)]   // or #[actor(skip_expired)] for every request handler
async fn import(&mut self, msg: Import, ctx: &Context<Self>) -> ImportResult { .. }

impl Deadline {
    pub fn min(self, other: Deadline) -> Deadline;   // the earlier one; Never is the identity
}
// Explicit inheritance from the request being served:
let rx = store.send_request(Load(key), reply.deadline().min(Deadline::after(LOAD_BUDGET)))?;
```

`skip_expired` on a `send_handler` is a compile error: a send has no slot.
Use `send_expiring` instead.

### 4.5 The priority lane

```rust
#[non_exhaustive]
pub struct PriorityLane { pub burst: u32, pub credits: Option<NonZeroU32> }

impl<A: Actor> Context<A> {
    /// OTP `alias([priority])`: a recipient whose messages use the lane.
    /// Err(PriorityLaneDisabled) unless A::PRIORITY_LANE is set.
    pub fn priority_recipient<M>(&self) -> Result<Recipient<M>, PriorityLaneDisabled> where A: Handler<M>, M: Message;
    pub fn monitor_with(&self, target: &ChildHandle, opts: MonitorOptions)
        -> Result<MonitorRef, PriorityLaneDisabled>;
    pub fn link_with(&self, target: &ChildHandle, opts: LinkOptions) -> Result<(), LinkError>;
}
MonitorOptions::new().priority()   // OTP monitor(.., [priority])
LinkOptions::new().priority()      // OTP link(.., [priority]); applies to notices this actor receives
#[non_exhaustive] pub enum LinkError { NoProc, PriorityLaneDisabled }
```

On an actor without the lane, a `.priority()` monitor or link is refused
and registers nothing; without `.priority()` both calls behave as
`monitor` and `try_link`. Priority recipient messages take credits from the
lane's own pool (`credits`; unbounded if the actor's mailbox is unbounded
or `credits` is `None`). Edge notices take none.

### 4.6 Observation

- **Metrics** (feature `metrics`, no cost otherwise):
  - `spawned_mailbox_rejections_total{reason="full"|"too_large"}`;
  - `spawned_admission_wait_seconds` (a histogram);
  - `spawned_admission_waiters` and `spawned_credit_pool_in_use` (gauges, per actor type, or per pool label);
  - `spawned_requests_skipped_total`;
  - `spawned_priority_turns_total` and `spawned_priority_valve_total`.
- **Introspection** ([introspection.md](introspection.md)): `info()` adds
  `mailbox_credits`, `credits_in_use`, `admission_waiters` and
  `priority_depth`. All are out-of-band reads, so they stay readable while
  the mailbox is full.

## 5. Semantics

### 5.1 Admission

The pool has `credits` and a FIFO queue of waiters. Message `m` needs
`w = max(1, m.weight())`.

1. Past the death point the send is refused with `NoProc` (D3, unchanged).
2. `w > credits` (the whole pool, not what is free): `TooLarge` at once,
   for every call, waiting or not.
3. No waiter is queued and `in_use + w <= credits`: admitted
   (`in_use += w`, the item goes into the mailbox).
4. Otherwise, a call that never waits gets `MailboxFull` at once. A waiting
   call queues its message, with its deadline, at the tail.

**Hand-off.** When credits are released (an item is dequeued for dispatch,
skipped or discarded; a shared pool's other mailbox dequeues; a waiter
leaves), the releaser admits head waiters in order while the head fits,
in the same step. It moves each head's message into its mailbox, so the
waiting caller does not have to run. Free credits and a head waiter that
fits never coexist (`NoIdleWithWaiter`), and no waiter is passed over by a
later sender, waiting or not (`NoOvertaking`). Strict FIFO means a heavy
head waiter blocks lighter ones behind it (head-of-line blocking). That is
the price of starvation freedom; first-fit, the alternative, starves heavy
messages, and the model's `FirstFit` control shows it.

**Leaving the queue.** A waiter leaves at exactly one point, decided by
one atomic transition of its node:

- admitted by a hand-off;
- its deadline passed: `MailboxFull` (`SendError::Full`), never
  `RequestTimeout`;
- its future was dropped: not admitted; the message drops with the node,
  on the dropping thread;
- the death point (5.3).

A deadline expiry that loses the race with a hand-off finds the message
admitted, and the call returns `Ok`, or, for a request, goes on waiting
for the reply. It never reports a rejection for an admitted message
(`RejectedNeverAdmitted`).

**One deadline (R3).** A request's deadline is fixed when it starts:
`request` on first poll, a protocol call when it is called. It covers the
wait for credits and the wait for the reply. Before admission, expiry
means `MailboxFull`; after admission it means `RequestTimeout`
(`TimeoutOnlyAdmitted`).

### 5.2 What takes credits

| Item | Takes credits | Refused or waits for capacity | Bounded by |
|---|---|---|---|
| `send`, `try_send`, `send_request`, `send_expiring`, `Recipient` sends | yes | refused at once | the pool |
| `request`, protocol calls, `send_wait` | yes | waits under its deadline | the pool |
| self-sends (`ctx.send`) | yes | refused at once | the pool |
| `send_after`, `send_interval` | yes | waits; the timer task is owned by the actor and dies with it (T1, T3) | the pool |
| `spawn_listener` items | yes | waits; the stream is not polled while it waits | the pool |
| introspection `get_status` | yes (an ordinary request, as OTP's system messages) | waits under its deadline | the pool |
| `ctx.pipe*` outcomes and emits | no | never | `MAX_OUTSTANDING`, emit credits (R6, R7) |
| `Down` | no | never | one per monitor (X1) |
| trapped `Exit`, parent exit, signals held during `started()` | no | never | one per link edge or parent (X3, X5) |
| `handle_continue` (G2) | no | never | one per start |
| priority recipient sends | the lane's credits | refused or waits, as ordinary sends | the lane's pool |

A send interval's fixed delay (T2) starts after the tick is admitted, so
a full mailbox slows ticks rather than piling them up. A `spawn_listener`
that waits pushes back on its stream: the transport's own flow control
then applies.

### 5.3 Stop and the death point

- **Stop window** (between the stop request and D, D3/D4): admission rules
  are unchanged. The actor no longer dispatches, so credits are not
  released, and waiters wait for D.
- **At D**, in the death point's single sequence ([actor_cleanup.rs]
  order: every caller is answered before any user code runs):
  1. the mailbox stops admitting (D3), and a hand-off racing D finds it
     closed and treats the node as released here;
  2. queued requests are answered `ActorStopped(final)` (R1, unchanged);
  3. every waiter of this mailbox is released: a waiting request is
     answered `ActorStopped(final)`, as if it had been admitted and
     discarded, since it was sent while the actor lived (as OTP's `call`
     to a live process gets the real reason); a waiting send gets
     `SendError::NoProc(msg)`;
  4. every credit the instance holds returns to its pool, and the pool
     hands off to other mailboxes' waiters.

  The waiters' messages belong to their senders and are dropped, or handed
  back, on the senders' side after their wake-up, never inside D.
- **After D:** `NoProc` as today (`NoAdmitAfterD`, `NoWaiterAfterD`,
  `CreditsReturnedAtDeath`).

### 5.4 Skipping

At dispatch, before the handler runs, an opted-in request handler checks
its reply slot with the same compare-and-swap `PendingReply::is_closed()`
uses: an expired deadline becomes `TIMED_OUT`, so the expiry wins over any
later reply. A closed slot, whether timed out or abandoned by its caller,
means the request is skipped: the handler does not run, the payload is
dropped, the credits are released, and the caller has already seen, or
will see, `RequestTimeout`. A request whose caller can still be answered
is never skipped (`SkipOnlyClosed`). Without the opt-in, every admitted
request is dispatched (R3 unchanged). `send_expiring` compares its
deadline with the runtime clock (the simulated clock under `spawned_sim`)
at dispatch; that envelope type is the only one that reads the clock.

### 5.5 The priority lane

Between turns, never during one (M1), the receive loop of an actor with a
lane takes from the lane first: priority recipient messages and notices of
priority edges, FIFO within the lane. The exception is the valve: once
`burst` priority turns in a row have run while the ordinary lane is
non-empty, one ordinary item goes next (`BoundedOvertaking`). Stop ends
dispatch from both lanes (D4); D discards both. Ordinary edges keep X7: a
notice follows every message its source sent earlier. A priority edge's
notice may overtake them, exactly as an OTP 28 priority `DOWN` overtakes
the source's earlier messages; the receiver opted in to that. Parent links
are never priority in v1, so V9's trapping-child shutdown order is
unchanged.

## 6. Interactions

**Mailboxes (D1-D5).**
- D1, per-sender FIFO, holds: a sequential sender's next send starts after
  the previous one resolved, and waiters are FIFO.
- D2 and D3 hold for admitted messages. D3's `Ok` means admitted, and a
  bounded mailbox adds the refusals `MailboxFull` and `TooLarge`.
- D4 holds.
- D5 becomes "unbounded by default".

**Deferred replies (R1-R7).**
- `MAX_OUTSTANDING` credits and mailbox credits are independent bounds. One
  limits deferred obligations and jobs, the other queued input. Neither
  waits on the other.
- `defer()` never waits (deferred-replies §8): "Admission at send is
  rank 17" is this design.
- A `pipe_reply` downstream request is an ordinary request to its target.
  It waits for credits within the token's deadline if the job passes it.
- Job outcomes take no mailbox credit, so R7's exactly-one delivery is
  never refused.
- R1 gains two outcomes, `MailboxFull` and `TooLarge`. They are terminal
  and exclusive like the others.

**Stop, exit and links (L, X).**
- The death point gains the waiter release and the credit return (5.3).
  Both happen before publication, so L7 holds.
- Framework notices are never refused (A8). So X1, "Down exactly once",
  and X3, trapped exits, are unaffected by a full mailbox.
- A kill (X4) needs no mailbox space.

**Supervision (V).**
- Supervisors observe children through completion state, not mailboxes
  (V9). A full child mailbox does not delay restart decisions.
- A parent's shutdown of a trapping child is a framework item: never
  refused, still queued behind earlier messages (V9 unchanged).
- Each restart gets a fresh pool (M4), or rejoins a shared one. The old
  instance's credits come back at its D (A7). Without that, a crash loop on
  a shared pool would leak capacity until no instance could receive, which
  is the `LeakCredits` control.
- Old references fail with `NoProc` (V4). Waiters on the old instance are
  released at its D, not moved to the replacement: queued messages are
  never replayed (V4), and neither are waiters.

**Execution groups (S).**
- Waiting is per caller. A tasks caller parks its future. A threads-mode
  or blocking-group caller blocks its thread, as a threads-mode `request`
  already does.
- On a `thread_group`, a handler that waits for another actor's credits
  holds that group slot while it waits. Section 7 bounds this with
  deadlines.
- System shutdown (S1-S3) changes nothing. Each actor's D releases its
  waiters, and a shared pool outlives the actors that use it, since it is
  just an `Arc`.

**Introspection and unified start.**
- `info()` reports occupancy (4.6).
- The watchdog's long-mailbox detector can report a fraction of the
  credits.
- Unified start reserved `mailbox(..)` and a priority opt-in. This design
  uses `mailbox_credits` and `credit_pool`. It needs no run-time priority
  field, because the lane is per type (section 8).

## 7. Cycles and deadlocks

Bounded mailboxes add a new way to deadlock. Two actors, each handling a
message, wait for credits in each other's full mailbox. Neither turn ends,
so neither mailbox drains. The design rules:

- Calls that never wait cannot deadlock; they return `MailboxFull`.
- Every waiting call has a deadline. `request` defaults to 5 s (R3).
  `send_wait` takes its deadline explicitly, as `send_request` does.
- A wait with `Deadline::Never` inside a handler can deadlock. This is the
  same class as a request cycle with no timeout, which section 10 already
  lists ("Deadlock detection ... rank 7"). The stall watchdog's long-turn
  detector reports it, and the rank 7 wait-for graph will name the cycle.
  A debug-build lint warns when a handler awaits `send_wait` with `Never`.

`AdmissionCycle.tla` checks this. With waiting handlers under a finite
deadline, or with never-waiting handlers, every turn ends (`TurnsEnd`).
With `Deadline::Never` (control `NoDeadline`), TLC finds the mutual wait.

## 8. Cost on the default path

The rule ([No performance hit]; ROADMAP "Process"): robustness features
add nothing to the default path.

| Path | Unbounded actor (default) | Bounded or laned actor |
|---|---|---|
| `send`, `request`, `Recipient` send | Unchanged: `if A::MAILBOX` is a constant per monomorphised `A`, folded away | one compare-and-swap on the pool word (credits and a waiters bit); a lock only when waiters exist |
| receive loop | Unchanged: `A::PRIORITY_LANE` and `A::MAILBOX` fold away | one atomic subtract per dequeue; the lane check |
| handler dispatch | Unchanged: `skip_expired` is generated only into opted-in handlers | one slot load (the `is_closed` compare-and-swap) |
| memory per actor | One pointer-sized field in the existing per-mailbox allocation (`DropFailures`/`Admission`), null and never read | the pool (private: inline in that allocation) |
| `Message::weight` | never called | one call per bounded send |
| `ActorError` size | Unchanged: two unit variants | — |

**Showing it with the callgrind gate** (`perf-gate`;
[TESTING.md](../TESTING.md), "Performance gate"):

1. The implementation PR must pass the existing gate on every gated
   benchmark: the `async_ct_*` burst, sequential, 16-task `request()`,
   one- and two-producer `send`, and threads sequential `request()`. That
   means 3% against the merge base and 3% against the last release.
2. Stricter, because the claim is "unchanged code", not "under 3%": the PR
   reports instructions per operation for the deterministic `async_ct_*`
   benchmarks, whose counts repeat within 0.05% on CI. Any change above
   that noise on an unbounded benchmark is investigated and explained in
   the PR, not absorbed by the budget.
3. The driver gains `async_ct_send_bounded`, `async_ct_request_bounded`
   and `threads_request_bounded`, which use a bounded actor that never
   fills. They are report-only in the implementation PR, which has no base
   to compare against, and gated from the next PR on, then against the
   release once a release includes them. Their cost against the unbounded
   twins is stated in the PR.
4. A `size_of` unit test pins `ActorError` and the per-actor shared
   allocation (as `exit_reason_size_is_unchanged` does for
   `ExitReason`).

## 9. Semantics contract changes

**Changed:**
- **D5** becomes "Mailboxes are unbounded by default. `send` never blocks;
  on an unbounded mailbox it never fails for capacity. Bounded mailboxes
  are opt-in (A1-A8)."
- **R1** adds `Err(MailboxFull)` (never admitted) and `Err(TooLarge)`.
- Section 10's bullets on bounded memory and on the fair control lane now
  point to A1 and A10. The priority lane is opt-in, and the unbounded
  default is still not bounded.
- Section 11's last row: "Unbounded by default; opt-in credit pools (A1)".
  Section 11 also gains the deviations of section 3 above.

**New section "Admission".** Each entry lists the check it will have. No
code exists, so only the model checks run today.

**A1. Unbounded by default, unchanged.** An actor type that does not opt in
has the 0.8 send, receive and dispatch code, and D5's behaviour.
Will be checked by: the callgrind gate as in section 8, and
`semantics_gaps::*::unbounded_mailbox_accepts_a_large_backlog_without_blocking`
(kept).

**A2. Capacity.** At every instant, a pool's credits held by its
mailboxes' queued items do not exceed its size. An item holds its weight
from admission until it is dequeued, skipped or discarded.
Will be checked by: `Admission!CapacityRespected` (TLC); the
`credit_pool_loom` harness; `admission::*::capacity_holds_under_concurrent_producers`
(both modes, every placement, private and shared pools); the fuzz A2
oracle.

**A3. Calls that cannot wait never wait.** `send`, `try_send`,
`send_request`, `send_expiring`, `Recipient` sends and self-sends return at
once. When the pool is short they fail with `MailboxFull`, and the message
is never admitted or handled.
Will be checked by: `Admission!RejectedNeverAdmitted`;
`admission::*::never_waiting_calls_fail_at_once`; the fuzz A3 oracle.

**A4. FIFO waits with hand-off.** Waiters are admitted in arrival order.
Freed credits go to the head waiter in the releasing step, and its message
is admitted without it running. No later sender, waiting or not, is
admitted while an earlier waiter waits. A waiter is admitted, expires, is
dropped or is released at D; it is never left waiting while the receiver
drains.
Will be checked by: `Admission!NoIdleWithWaiter`, `Admission!NoOvertaking`,
`Admission!WaiterResolves` (liveness); `credit_pool_loom`;
`admission::*::waiters_are_admitted_in_order`,
`unpolled_protocol_call_is_admitted_by_hand_off`; the simulator's
`admission_sim::hand_off_races_expiry_{pct,dfs}`.

**A5. Oversize never waits.** A message heavier than its whole pool fails
at once with `TooLarge`, whatever the call.
Will be checked by: `Admission!NoOversizeWait` (`MCAdmissionOversize`);
`admission::*::too_large_fails_at_once`.

**A6. One deadline, distinct outcomes.** A request's single deadline (R3)
covers the wait for credits and for the reply. Never admitted by it gives
`MailboxFull`; admitted and then unanswered gives `RequestTimeout`. A
`send_wait` expiry gives `SendError::Full`. An expiry racing a hand-off is
resolved by one transition: the message is either admitted or rejected,
never both.
Will be checked by: `Admission!TimeoutOnlyAdmitted`,
`Admission!RejectedNeverAdmitted`; `credit_pool_loom::expiry_races_hand_off`;
`admission::*::request_deadline_covers_admission_and_reply`.

**A7. The death point releases admission.** At D, before any user
destructor and before publication (L7): queued and waiting requests get
`ActorStopped(final)`, waiting sends get `NoProc` with the message back,
and every credit the instance holds returns to its pool. Nothing is
admitted after D (D3).
Will be checked by: `Admission!NoWaiterAfterD`,
`Admission!CreditsReturnedAtDeath`, `Admission!NoAdmitAfterD`;
`admission::*::death_releases_waiters_and_credits` (also after a panic, a
kill and a supervised restart on a shared pool); the fuzz A7 oracle;
lifecycle traces once instrumented (section 12).

**A8. Framework items are never refused.** `Down`, trapped `Exit`, parent
exits, signals held during `started()`, job outcomes and emits, and
continuations take no credits and are never refused or delayed for
capacity. Each is bounded by its own source.
Will be checked by: `Admission!ControlNeverRefused`;
`admission::*::notices_and_job_outcomes_arrive_through_a_full_mailbox`,
`supervisor_shutdown_reaches_a_trapping_child_with_a_full_mailbox`.

**A9. Opt-in skip only when nobody can be answered.** A `skip_expired`
handler skips exactly the requests whose reply slot is closed at dispatch;
their callers see `RequestTimeout`. A `send_expiring` message is skipped
exactly when its deadline passed at dispatch. Without the opt-in, every
admitted message is dispatched.
Will be checked by: `Admission!SkipOnlyClosed`;
`admission::*::skip_expired_runs_open_requests_and_skips_closed_ones`;
`deferred_replies` (no change for handlers without the opt-in).

**A10. The priority lane.** On an actor with the lane, between turns,
items from priority sources (priority recipient, priority monitor, priority
link) are dispatched before ordinary items, FIFO within the lane. An
ordinary item waiting at the head is overtaken by at most `burst` priority
turns in a row. Nothing preempts a turn (M1). X7 holds for ordinary edges.
Will be checked by: `Admission!BoundedOvertaking`;
`admission::tasks::priority_down_overtakes_the_backlog`,
`priority_valve_admits_one_ordinary_turn`,
`ordinary_down_still_follows_the_source`; the simulator.

**Not guaranteed (section 10 additions):**
- Freedom from deadlock with `Deadline::Never` waits in handlers (section
  7).
- Bounded memory for unbounded actors, framework items beyond their
  sources' bounds, or the item being handled.
- Fairness between pools: a mailbox is fair within its own pool only.

## 10. Implementation sketch

This section is not part of the contract.

- **Storage stays the unbounded queue.** The Tokio unbounded mpsc and the
  threads `std::sync::mpsc` with its `SendGate` stay; admission sits in
  front of them. The death point's close-and-drain protocol
  (`mailbox.rs`) is unchanged.
- **Pool core** (`concurrency/src/credit_pool.rs`, a Loom-checked core
  with a `sync` shim, as `mailbox_admission.rs`):
  - one `AtomicU64` holding `in_use` and a `HAS_WAITERS` bit;
  - the fast path is one compare-and-swap that requires the bit clear and
    room for the weight;
  - waiters are `Arc<Node>` in a `Mutex<VecDeque>`;
  - a node holds the message (type-erased as a ready-to-send envelope),
    the target mailbox, the deadline and a state word `WAITING → ADMITTED
    | EXPIRED | CANCELLED | RELEASED`, with exactly one transition;
  - release does `fetch_sub`, and only if the bit is set takes the lock
    and hands off.
- **Deadlines.** A waiting request's deadline is a timer in the waiter's
  future (tasks) or the `Condvar` wait (threads). Expiry is a
  compare-and-swap on the node state, which the hand-off also uses; the
  loser of the race observes the winner.
- **Death point.** The mailbox closes first. Then, under the pool lock,
  it removes its own nodes (`RELEASED`) and returns its credits. A
  hand-off under the lock checks the target's gate and treats a closed
  target as released. This split is the one place the model's atomic D is
  refined, and Loom checks it (`credit_pool_loom::death_races_hand_off`).
- **Priority lane.** A second unbounded queue allocated at start only for
  `A::PRIORITY_LANE.is_some()`. The receive loop polls it first, with the
  valve counter as a loop local. Monitor and link entries carry a priority
  bit in their existing flags.

## 11. The model

`Admission.tla` is one actor's bounded mailbox on a weighted credit pool
that another mailbox may share (the environment variable `ow`, which also
respects the pool's FIFO). Its parts:

- senders that never wait (`try`) or wait (`wait`), sending and
  requesting, with weights;
- the FIFO waiter queue with atomic hand-off;
- waiter deadlines;
- caller deadlines after admission;
- opt-in `skip_expired`;
- link and monitor edges, ordinary and priority, each delivering at most
  one notice;
- the valve;
- stop (D4) and the death point.

Each action is one linearization point of the planned implementation
(section 12). `AdmissionCycle.tla` is two actors with one-slot mailboxes
whose handlers send to each other.

| Configuration | Run | Instance | States (generated / distinct), depth |
|---|---|---|---|
| `MCAdmission` | per PR | pool of 2 shared with a mailbox holding up to 1; senders `a` (waiting request, weight 1, `skip_expired`), `b` (waiting request, weight 2), `c` (send that never waits, weight 1), two messages each; edges `e1`, `e2` priority and `e3` ordinary; valve 1 | 21,920,345 / 6,256,024, depth 35 (47 s with 4 workers on fede; its own CI leg) |
| `MCAdmissionOversize` | per PR | private pool of 3; waiting requests of weight 1 and 3, a waiting send of weight 4 (`TooLarge`) | 87,994 / 35,730, depth 24 |
| `MCAdmissionLive` | per PR | pool of 2 shared with a mailbox holding up to 1; two waiting requests (weights 1, 2), one priority and one ordinary edge; liveness under weak fairness of dispatch, turn end, D and the other mailbox's release | 16,361 / 5,520, depth 18 |
| `MCAdmissionCycle` | per PR | two actors, one slot each, two outside messages each, handlers that wait under a deadline | 257 / 112, depth 13 |

`AdmissionCycle` with `HandlerSend = "try"` also passes (215 / 112).

**Invariants and properties.** Each is checked by TLC, and each has a
negative control that must violate exactly it. The controls are
`ADMISSION_CONTROLS` and `ADMISSION_TEMPORAL_CONTROLS` in
`negative_checks.py`; each runs `TypeOK` plus the named property, and all
16 fail as required, in 19 s together on fede.

| Guarantee | Invariant or property | Fault (control) | What the fault does |
|---|---|---|---|
| A4 | `NoOvertaking` | `Barge` | the fast path ignores queued waiters |
| A4 | `NoOvertaking` | `FirstFit` | hand-off admits the first waiter that fits, not the head |
| A4 | `NoIdleWithWaiter` | `LazyHandoff` | release does not hand off |
| A4 | `WaiterResolves` (liveness, `MCAdmissionLive`) | `LazyHandoff` | a waiter waits forever while the receiver is idle |
| A2 | `CapacityRespected` | `NoCapacityCheck` | admission without the credit check |
| A3, A6 | `RejectedNeverAdmitted` | `ExpireRacesHandoff` | expiry reports `MailboxFull` for a message the hand-off already admitted |
| A6 | `TimeoutOnlyAdmitted` | `TimeoutWhileWaiting` | a never-admitted request reports `RequestTimeout` |
| A7 | `NoAdmitAfterD` | `LateAdmit` | admission does not check D |
| A7 | `NoWaiterAfterD` | `KeepWaiters` | D does not release waiters |
| A7 | `CreditsReturnedAtDeath` | `LeakCredits` | D keeps the instance's credits |
| A8 | `ControlNeverRefused` | `CountControl` | notices take credits and are refused when full |
| A9 | `SkipOnlyClosed` | `SkipOpen` | skips a request whose caller can still be answered |
| A9 | `SkipOnlyClosed` | `SkipNoOptIn` | skips without the handler's opt-in |
| A10 | `BoundedOvertaking` | `NoValve` | priority always first |
| A5 | `NoOversizeWait` (`MCAdmissionOversize`) | `OversizeWaits` | a message heavier than the pool waits |
| section 7 | `TurnsEnd` (liveness, `MCAdmissionCycle`) | `NoDeadline` | handler waits with `Deadline::Never` |

**What the model leaves out:**
- the queue's storage (Tokio mpsc, `std::sync::mpsc`);
- the split of D into close, then node release (section 10, Loom);
- the lane's own credit pool, which is the same mechanism as the ordinary
  pool;
- `send_expiring`'s clock comparison;
- explicit deadline inheritance, which has no protocol;
- threads versus tasks, which share one model, as `Lifecycle.tla` does;
- cycles longer than two;
- payload destructors, which `Disposal.tla` and `Lifecycle.tla` already
  cover;
- the death point's ordering against user code (L7, `Lifecycle.tla`).

## 12. Trace events and their mapping

The model is `tlc_only` with `design_only = true` in
`verification/model-map.toml`. The PR that adds code under its `code` list
must add `TraceAdmission.tla` and these events, recorded under the
`lifecycle-trace` feature inside the section that performs each step:

| Event | Fields | Model action |
|---|---|---|
| `AdmBegin` | sender, id, weight, mode, `x`: `accepted`, `admitted`, `full`, `toolarge`, `noproc`, `wait` | `Begin` |
| `AdmHandoff` | the admitted ids, in order (recorded under the pool lock) | the hand-off part of `Dispatch`, `Expire`, `OtherRelease` |
| `AdmExpire` | id | `Expire` |
| `Dispatch` (existing) | adds `lane` and `x`: `run`, `skip` | `Dispatch` |
| `AdmTurnEnd` (new: the lifecycle trace has no turn-end event) | id | `TurnEnd` |
| `CtlArrive` | edge, lane | `CtlArrive` |
| `Stop`, `Die` (existing lifecycle events) | `Die` adds the released waiter ids and the returned credits | `Stop`, `Death` |
| `PoolOther` | another mailbox's acquire or release on a shared pool | `OtherAcquire`, `OtherRelease` |
| the reply slot's `TIMED_OUT` transition (existing replies trace) | id | `CallerTimeout` |

The trace spec projects one mailbox and treats other mailboxes on its pool
as the environment, as the model does.

## 13. Test plan

**Integration**, in `concurrency/tests/admission.rs`. Both modes run on
every placement: `Async`, `Blocking`, `Thread`, `async_group`,
`thread_group`, `pinned_group`, and threads mode. Each guarantee's named
tests (section 9) run against private and shared pools. They include:
- hand-off to an unpolled protocol call;
- `TooLarge` at once;
- a request whose deadline covers both waits;
- D releasing waiters after `stop`, a panic, a kill, an untrapped signal
  and a supervised restart on a shared pool, with `in_use` back to the
  survivors';
- `Down`, trapped `Exit`, a supervisor's shutdown of a trapping child, and
  job outcomes arriving through a full mailbox;
- `spawn_listener` and `send_interval` back-pressure (the stream is not
  polled while full; ticks do not pile up);
- `skip_expired`;
- priority order, the valve, and X7 for ordinary edges;
- an unbounded actor's D5 test unchanged.

**Cancellation safety** (`cancellation_safety`). Drop `send_wait`,
`request` and protocol responses at every poll point. The message is then
admitted exactly once or never, and no credit leaks.

**Simulator** (`#[sim_test]`, PCT and bounded DFS, `admission_sim`):
- hand-off racing expiry, drop and D;
- two actors' cyclic waiting sends with deadlines end, mirroring
  `AdmissionCycle`;
- same-seed replays produce identical admission orders.

**Loom** (`credit_pool_loom`, registered under `[loom.CreditPool]` in the
model map):
- the fast path racing a release with the waiters bit set;
- hand-off racing node expiry and cancel, with exactly one transition;
- D racing a hand-off;
- two mailboxes on one shared pool.

**Fuzzer** (`fuzz/fuzz_targets/scenario.rs`). New operations: a bounded
actor, `try_send`, `send_wait` with a deadline, request, a shared pool,
stop and kill. Oracles:
- A2: `in_use <= credits`, sampled;
- A3: per-id tracking, so a rejected id is never handled;
- A4: among waiters that were admitted, admission order equals
  registration order;
- A7: after `join`, the pool's `in_use` excludes the instance;
- A8: every `Down` and `Exit` is delivered;
- A9: a skipped id's caller saw `RequestTimeout`.

**Planted bugs** (`verification/planted/patches/`). Each mirrors a model
control and must be caught by the named test:

| Patch | Caught by |
|---|---|
| `adm-barge` (the fast path ignores the waiters bit) | `waiters_are_admitted_in_order`, fuzz A4 |
| `adm-no-handoff` | `unpolled_protocol_call_is_admitted_by_hand_off` |
| `adm-leak-credits-on-death` | `death_releases_waiters_and_credits` |
| `adm-control-counted` | `notices_and_job_outcomes_arrive_through_a_full_mailbox` |
| `adm-skip-open-slot` | `skip_expired_runs_open_requests_and_skips_closed_ones` |
| `adm-expire-not-atomic` | `credit_pool_loom::expiry_races_hand_off`, cancellation tests |
| `adm-no-valve` | `priority_valve_admits_one_ordinary_turn` |

**Trace validation.** `TraceAdmission.tla` replays the section 12 events
from the integration and simulator suites. Each model control gets a
trace-edit counterpart in `negative_checks.py`, as the existing models
have.

**Compile-time checks** (trybuild): `mailbox = 0`; `skip_expired` on a
`send_handler`; `#[weight]` on a non-protocol item.

**Performance gate**, as in section 8.

**Coverage matrix.** Rows A1-A10 in `TESTING.md`'s semantics coverage.

## 14. Client impact

Nothing changes for either client until it opts in. `ActorError` gains
two variants; it is `#[non_exhaustive]`, so the wildcard arms already in
place keep compiling. `Message::weight` is a provided method.

**Ethrex** (pinned to 0.8.0 in its draft PR):

- **The per-connection actor** (`rlpx/connection/server.rs`) is where the
  1.5 GB/day leak was. It is the first adopter:
  `#[actor(mailbox = N)]`. Its `spawn_listener` over the RLPx stream then
  waits instead of queueing, so a slow connection actor stops reading its
  socket and TCP flow control pushes back on the peer.
  - The broadcast listener (`BroadcastStream`) already drops on lag.
    Bounded, it lags sooner but no longer grows the mailbox. The
    lag-dropping is today's behaviour; it should be counted by a metric.
  - The bounded channel plus raw writer task of d0b9fecb9 is for outbound
    writes, so it stays.
- **The 150-connection `inbound_admission` semaphore** bounds actors, not
  messages. That is the dynamic supervisor's `max_children` (rank 12a),
  not this design.
- **`get_best_peer` under pressure** (63167661a). A bounded peer table
  answers `MailboxFull` at once instead of timing out, and the caller
  retries knowing the request did not run (`is_transient`).
- **Expected effect.** Memory per connection is bounded by the credit
  count times the message size. Measure it in a paired memory run against
  the unbounded build before choosing N.

**Ethlambda** (pinned to 0.8.0 in its draft PR):

- **The P2P actor fed by `spawn_listener(swarm_stream)`** (`net/p2p/src/lib.rs`).
  A bounded mailbox makes the listener stop polling the swarm stream while
  the mailbox is full. Whether pausing the libp2p swarm is acceptable
  must be measured on the canary. The fallback is a listener that uses
  `try_send` and counts what it drops; that variant of `spawn_listener` is
  open question 6.
- **The five validation semaphores and their `Ignore(Overloaded)`
  verdicts** bound compute, not input. They stay until the compute pool
  (rank 11b), which shares this design's "reject at once" rule.
- **Slot-bound gossip work** (attestations past their slot) fits
  `send_expiring` and `skip_expired`.
- **The chain actor on its own thread** can take control messages through
  a priority recipient, so status and shutdown do not wait behind block
  imports.

Neither client needs a code change to upgrade. Adoption is per actor, and
each adoption is a behaviour change to review: senders must handle
`MailboxFull`.

## 15. Alternatives considered

| Alternative | Why not |
|---|---|
| Tokio's bounded `mpsc` as the mailbox | It has slots only, with no weights and no shared pool. Its waiting `send().await` keeps the message in the caller's future, so there is no hand-off on the caller's behalf, and a dropped future loses its place in the queue. Threads mode would need a second, different mechanism (`sync_channel` has no FIFO deadline wait). The death point's close-and-drain protocol is built on the unbounded channel. |
| A run-time `Option<Pool>` branch on every send | One load and branch per send for every actor. The compile-time const costs nothing and loses only a per-start choice of kind, which no client needs; the size and the pool stay per start. |
| `send` waits (blocking) when full | It blocks an async worker, and from a handler it blocks the sender's own mailbox. |
| Drop the newest or oldest message when full (Akka-style dead letters) | A silent loss. Rank 36 requires an explicit error for superseded requests; dropping policies are an opt-in for droppable traffic (rank 42). |
| Count framework items against capacity | `Down`, trapped exits and job outcomes would be refused exactly when the actor is overloaded, which breaks X1, X3 and R7 (`CountControl`). |
| First-fit hand-off | Better throughput for mixed weights, but it starves heavy messages (`FirstFit` violates `NoOvertaking`). |
| Release credits at the end of the turn | It would bound the in-turn item too, but a handler awaiting a reply from an actor that needs to send back to it would hold credits across the await. Release at dequeue matches OTP's message-queue length. |
| Strict OTP priority (no valve) | ROADMAP rank 14 requires no starvation. `burst = u32::MAX` remains available. |
| Ambient deadline inheritance | A task-local or `Context` lookup on every request (open question 3). |
| Skip expired by default | Not OTP; the deferred-replies design decided opt-in. |

## 16. Release: 0.9 or 0.10

**Recommendation: the design and the model land in 0.9 (this PR). The
implementation moves to 0.10.** The reasons:

1. **Process.** The ROADMAP's rule for features that touch admission
   requires two independent reviews (different tools or people), each
   finding resolved, and the maintainer settling the open questions
   before any code. This is revision 1 with no review yet. The 0.8 and
   0.9 designs each took three rounds.
2. **Dependencies in flight.** The per-start options extend unified
   start's `StartOptions` (#291, a draft), and occupancy reporting extends
   introspection's `info()` (step 1 in progress). Implementing on top of
   both while they move would mean rebasing a hot-path change twice.
3. **Size and risk.** It touches the send path in both modes and every
   placement, the death point, the monitor and link tables, the macros,
   and the perf gate. It needs a Loom-checked core, trace validation and
   seven planted bugs. That is the size of the unified start
   implementation, and 0.9 already carries that plus introspection.
4. **Nothing in 0.9 depends on it.** Both clients have workarounds for
   memory. The dynamic supervisor (0.10) shares only the "reject with a
   typed error" pattern.

One piece could ship in 0.9 if the maintainer wants an early win:
`skip_expired` alone (A9). It is a macro attribute plus an existing slot
compare-and-swap, with no hot-path change, no new storage and no
dependency on #291. See open question 1.

Rank 13 (draining) and rank 18a (memory limits) stay in 0.11 on top of
this, as the ROADMAP lists them.

## 17. Open questions for the maintainer

1. **Ship `skip_expired` (A9) in 0.9 on its own?** Recommended only if
   the 0.9 schedule has room; it is independent of the rest.
2. **`MailboxFull` as a new `ActorError` variant, or reuse `Overloaded`?**
   This design adds `MailboxFull` because "never admitted" is a stronger
   statement than "rejected at a bound". `Overloaded` today can also mean
   the handler ran and answered `overloaded()`. Reusing it means one less
   variant, at the cost of that distinction.
3. **Automatic deadline inheritance** (rank 19's "inherited monotonic
   deadlines"). It is explicit in v1. Automatic inheritance (a request
   sent from a turn gets `min(own, turn's deadline)`) needs a lookup on
   every request. Is it acceptable behind a feature flag, given that
   features unify across the dependency graph, or should it wait for the
   cancel-scope design (rank 15a), which owns deadline inheritance?
4. **The valve's default.** `burst = 64` here. OTP is strict
   (`u32::MAX`). The ROADMAP's "cannot starve user work" argues for a
   finite default.
5. **Self-sends on a full own mailbox** fail with `MailboxFull`, so a
   handler cannot grow its own queue past the bound. OTP self-sends never
   fail. Should a turn be allowed one self-send of slack instead?
6. **A `spawn_listener` variant that drops instead of waiting**
   (`spawn_listener_lossy`, with a drop counter), for streams where
   pausing the source is worse than losing items (the ethlambda swarm
   case). Add it in v1 or wait for rank 33's streams?
7. **Priority on the parent link.** It is excluded in v1, which keeps V9.
   OTP 28 allows `link(.., [priority])` on any link. Is a priority parent
   link (fast shutdown past a backlog) wanted?
8. **Per-type compile-time kind.** A type is bounded or not, and only the
   size and pool vary per start. Is any user known to need the same actor
   type bounded in one place and unbounded in another? The unified start
   design reserved a run-time `mailbox(..)`.

[actor_cleanup.rs]: ../../concurrency/src/actor_cleanup.rs
[No performance hit]: ../../ROADMAP.md#non-goals
