# Exit signals, startup and runtime-drop semantics as in OTP: design contract

**Status: design in progress, not implemented.** Revised after one round of
independent review; see [Review resolutions](#review-resolutions).

- Covers ROADMAP rank 5e (exit signals), the startup part of rank 5f, rank 5g
  (runtime-dropped actors) and the supervisor `start()` ordering found by the
  quality pass (#223). Counting the restart budget per recovery round (the
  other half of 5f) is a separate change.
- Target: v0.8. Changes L3, L4, L6, L7, X1-X7, V3, V7, V9 and §11 of the
  [semantics contract](semantics.md), and refines the death boundary of
  [deferred-replies.md §5](deferred-replies.md#5-owner-lifecycle) (rank 11,
  designed, not implemented).
- Model: [`Lifecycle.tla`](../../verification/tla/Lifecycle.tla), the "base
  actor lifecycle" model the [verification rule](../../ROADMAP.md#verification-rule)
  calls for. See [What the model establishes](#what-the-model-establishes).
- Illustrative API shapes are not copyable code.

**Project rule.** Match Erlang/OTP. Deviate only where:

- **R:** Rust forces it;
- **T:** Rust types add strictness; or
- **B:** Spawned adds a bound OTP lacks.

## Summary

| # | OTP | Spawned today (v0.7.3) | Proposed (v0.8) |
|---|---|---|---|
| P | a process exists until it is gone; no destructors | exit steps (links, monitors, publication) happen at different points | [phases of an exit](#phases-of-an-exit): one death point D after `stopped()`, publication after destruction |
| E1 | `exit_signal(Pid, kill)` is untrappable; the target ends `killed`, also from inside `terminate`; links receive `killed` (trappable) | `stop_with_reason(Kill)` ends `Kill`; links receive `Kill` | `ChildHandle::kill()` is untrappable and supersedes any reason until D; the target ends `Killed`; links and monitors receive `Killed`. A self-exit with `Kill` stays `Kill` |
| E2 | `exit_signal(Pid, normal)` is ignored by a non-trapping process; every reason except `kill` is trappable | every outside stop ignores `trap_exit` and always stops the actor | new exit-signal API (`ctx.exit`, `ChildHandle::exit`) with exactly OTP's rules; `stop`/`stop_with_reason`/`shutdown` stay administrative stops |
| E3 | a trapping gen_server that gets `'EXIT'` from its parent runs `terminate(Reason)` | supervisor shutdown cancels a trapping child directly; a `start_linked` parent's exit reaches `exit_received` | a signal from the actor's parent is queued and, when dispatched, runs normal termination; it never reaches `exit_received` |
| E4 | `link`/`monitor` on a process that no longer exists give `noproc` | the target's real exit reason | `NoProc` after D |
| E5 | exit reasons are any term | closed four-variant enum | `#[non_exhaustive]`, adds `Killed` and `NoProc` |
| E6 | `unlink`/`demonitor` stop every later effect of the edge | a notice already taken for delivery still arrives | fenced at the receiver; only a message already queued stays |
| E7 | signals between a pair of processes keep their order | death notices follow earlier messages | unchanged for death notices; direct signals are per-sender FIFO with messages |
| S1 | `start_link` aborts on any child start error; exits of started children wait until `init` returns | Permanent/Transient children retry a failed first start; a ready child's exit during startup is handled at once | any first-start failure aborts; ready children's exits are processed after the initial start completes |
| S2 | `start_link` returns its error after the failed child exited and started children were terminated | `start()` can return `Err` before the failed child is `Failed` | `start()` returns `Err` after rollback (or its deadlines) |
| G1 | no counterpart | tasks: `Kill`; threads: `Panic("unexpected framework panic")` | `Killed` in both modes |

## Terms

- **Exit signal.** What `erlang:exit_signal/2` sends (OTP 29; it replaces
  `exit/2` without its quirk for a process signalling itself): a sender, a
  target and a reason. Death notices of links are exit signals too. This
  design follows `exit_signal/2`.
- **Trapping.** `ctx.trap_exit(true)` (OTP `process_flag(trap_exit, true)`).
  Read when the signal reaches the target. An `Exit` already queued stays a
  message if trapping is turned off later.
- **Self-exit.** The actor ends itself: `ctx.stop()`, `ctx.stop_with_reason(r)`,
  a panic in a hook or handler (OTP `exit/1`, `{stop, Reason, State}`, a crash).
  A `Context` clone used from a helper task or stream listener is still the
  actor's own capability and still a self-exit. `ctx.exit(&own_handle, r)` is
  a signal to itself, not a self-exit: a non-trapping actor ignores
  `Normal`, as with `exit_signal/2`.
- **Administrative stop.** `ChildHandle::stop()`, `stop_with_reason(r)`,
  `shutdown()` called by someone else.
- **Parent.** Defined in [E3](#e3-a-parents-exit-signal-ends-a-trapping-actor-normally).
- **Death point (D) and publication.** Defined in
  [Phases of an exit](#phases-of-an-exit).

## Phases of an exit

OTP has two states: a process exists or it does not. Rust adds destructors that
run after the actor's last hook and may hang, so Spawned needs two points
after the last hook: one at which the actor stops existing for its peers, and
one at which its resources are released. Each actor instance goes through:

1. **Running.** Handlers are dispatched.
2. **Exit requested.** A self-exit, an untrapped exit signal, a parent exit, an
   administrative stop or a kill begins exit and records a *requested reason*.
   Dispatch stops after the current handler; queued messages are not
   dispatched (semantics D4). Sends and requests are still admitted (D3) and
   are answered at D. Rank 11 step 1 (registry `Exiting`, jobs cancelled).
   The first requested reason wins, except that a **kill supersedes any
   requested reason until D** (E1). Turning `trap_exit` on or off no longer
   matters.
3. **`stopped()` runs** (unless skipped, open question 2). It may still answer
   deferred tokens and send messages. A stuck `stopped()` keeps the actor in
   this phase: nothing is closed, callers are bounded by their own deadlines
   (rank 11 "cleanup incomplete").
4. **Death point D.** When `stopped()` returns (or the runtime drops the
   actor, G1), one critical section on the actor's exit table:
   - the requested reason becomes the **final reason**;
   - the mailbox refuses admission;
   - the reply registry closes: every open token and every queued request is
     answered with the final reason (rank 11 steps 1-2);
   - the link and monitor table closes, and one notice per link and monitor
     is taken with the final reason.

   After D, `send`, `request`, `link` and `monitor` report `NoProc`; nothing
   can change the final reason.
5. **Destruction.** Queued envelopes (already answered) and the actor state
   are dropped, owned registrations and the deadline heap are released, jobs
   are joined (rank 11 steps 3-4). A destructor panic here does not change the
   final reason; it is recorded as a **cleanup failure**
   (`ChildHandle::cleanup_failure()`, generalising today's
   `mailbox_cleanup_failure()`), and a supervisor treats a cleanup failure like
   an abnormal exit when it decides whether to restart (semantics L6, V1). A
   hanging destructor keeps the actor in this phase: callers are answered,
   newcomers get `NoProc`, but nothing is published, no `Down` or `Exit` is
   delivered and no replacement starts (V3).
6. **Publication.** The final reason is published: `wait_exit`, `join`,
   `exit_reason()` and `is_alive()` observe it, and the notices taken at D are
   delivered (`Exit`, `Down`, link stops). Supervisors observe this point.

So an observer that receives `Down` or `Exit` knows every caller of the
instance was answered and its state was dropped (semantics L7, rank 11).
Between D and publication, `exit_reason()` still returns `None` while requests
already report `NoProc`; code that needs the reason waits on `wait_exit`.

Compared with rank 11 §5: its steps 1-2 (registry close, mailbox disposal) and
the closing of the link and monitor table now form one critical section, and
its step 5 ("the exit reason is published, and monitors and links fire")
becomes publication. The final reason is fixed at D instead of after the state
drop, so callers, `Down` and `wait_exit` always agree.

## Rules

### E1. Kill and Killed

**OTP.** `exit_signal(Pid, kill)`: "the receiver cannot trap the exit signal and
will unconditionally terminate", with "the atom `killed` as exit reason"
([processes, receiving exit signals](https://www.erlang.org/doc/system/ref_man_processes.html#receiving-exit-signals)).
This holds while the process runs `terminate/2`: a supervisor's kill after the
shutdown timeout ends it `killed`. Linked processes receive `killed`, which is
trappable. A process that exits *itself* with reason `kill` propagates `kill`
over its links; "an exit reason of `kill` will *not* be converted to `killed`"
for link signals, and a trapping receiver gets it as a message.

**Today.** `ChildHandle::stop_with_reason(Kill)` and `Context::stop_with_reason(Kill)`
both commit `Kill`, first committed reason wins
([child_handle.rs `stop_with_reason`](../../concurrency/src/child_handle.rs),
[link.rs `stop_with_reason`](../../concurrency/src/link.rs)); `make_signal`
delivers it to peers as `Kill`, trappable (semantics X4).

**Proposed.**

- `ChildHandle::kill()`, `ChildHandle::exit(Kill)` and `ctx.exit(&target, Kill)`
  are untrappable. If the target is running, it begins exit with `Killed`. If
  it is already exiting but before D (current handler, queued behind its
  backlog, inside `stopped()`), `Killed` replaces the requested reason. After
  D a kill does nothing.
- What still runs: the current handler finishes and `stopped()` either runs or
  is skipped per open question 2; if `stopped()` is already running it runs to
  the end (R: Rust cannot interrupt it). A panic in that code does not replace
  `Killed`.
- Links receive `Killed`: a non-trapping peer ends `Killed` and propagates
  `Killed`; a trapping peer gets `Exit { reason: Killed }`. Monitors get
  `Down { reason: Killed }`.
- A self-exit with `Kill` (`ctx.stop_with_reason(Kill)`) stays `Kill`, and
  links receive `Kill` (trappable; a non-trapping peer ends `Kill`), as OTP's
  `exit(kill)`. An administrative `stop_with_reason(Kill)` is open question 5.
- `Killed` is abnormal: Permanent and Transient children restart.

Model: `KilledFromKill`; controls `KillAsKill`, `KillNoSupersede`.

### E2. Exit signals and administrative stops

**OTP.** `exit_signal(Pid, Reason)`: with `normal`, "the signal has no effect if
`P` is not trapping exits"; when trapping, "the exit signal is transformed into
a message `{'EXIT', From, Reason}`"; with any other reason a non-trapping
target terminates with that reason; only `kill` cannot be trapped
([`erlang:exit_signal/2`](https://www.erlang.org/doc/apps/erts/erlang.html#exit_signal/2),
[processes](https://www.erlang.org/doc/system/ref_man_processes.html#receiving-exit-signals)).
Separately, `gen_server:stop/1,3` "orders the generic server ... to exit with
the specified `Reason` and waits for it to terminate"; it calls `terminate/2`
and is not affected by `trap_exit` (it is a system message, not an exit
signal) ([gen_server](https://www.erlang.org/doc/apps/stdlib/gen_server.html#stop/3)).

**Today.** There is no exit-signal API. Every outside stop
(`ChildHandle::stop`, `stop_with_reason(any)`, `shutdown`) commits its reason
and cancels the run loop, ignoring `trap_exit` (semantics X4). Only link
notices consult the flag ([link.rs `make_signal`](../../concurrency/src/link.rs)).

**Proposed.**

1. **Exit signals, exactly as OTP.** New:
   ```rust
   impl<A: Actor> Context<A> {
       pub fn exit(&self, target: &ChildHandle, reason: ExitReason); // from = self
   }
   impl ChildHandle {
       pub fn exit(&self, reason: ExitReason); // from = ActorId::EXTERNAL
       pub fn kill(&self);                     // = exit(ExitReason::Kill)
   }
   ```
   Delivery rule at the target, shared with link notices:

   | Reason | Non-trapping target | Trapping target |
   |---|---|---|
   | `Kill` (a signal, not a link notice) | ends `Killed` (E1) | ends `Killed` (E1) |
   | `Normal` | ignored | `Exit { from, reason: Normal }` queued |
   | anything else | begins exit with that reason after its current handler; queue discarded | `Exit { from, reason }` queued |
   | any, from the target's parent | as above | parent exit queued (E3) |

   Any `ExitReason` may be sent, including `Killed` and `NoProc`; they behave
   as ordinary reasons. Only a `Kill` *signal* produces `Killed`. A non-kill
   signal to an actor that already began exiting does nothing (open question
   9); a signal to an actor past D is dropped, as in OTP. Queuing a trapped
   `Exit` commits no reason, cancels nothing and closes no admission.
2. **Administrative stops stay.** `ChildHandle::stop()`, `stop_with_reason(r)`
   and `shutdown()` keep their v0.7 meaning: an untrappable stop after the
   current handler, `stopped()` runs, queued messages are discarded, the call
   returns at once. They are Spawned's counterpart of `gen_server:stop`,
   without waiting for the backlog and without blocking the caller (join with
   `wait_exit` for that). They are not exit signals. See
   [Differences](#differences-from-otp-that-remain).
3. **Self-exits are unchanged** (`ctx.stop`, `ctx.stop_with_reason`, panics).

Why not turn `ChildHandle::stop()` into `exit(Pid, normal)` (the literal reading
of the roadmap row): it would make `handle.stop()` a silent no-op for every
non-trapping actor, which is most of them, with no compile error. OTP keeps two
primitives (exit signals and `gen_server:stop`); so does this design. See
[Alternatives](#alternatives-considered).

Model: `TrapRespected`, `NormalSignalIgnored`; control `DirectStop`.

### E3. A parent's exit signal ends a trapping actor normally

**OTP.** `gen_server:terminate/2`: under a supervisor, it "is called with
`Reason=shutdown`" when the server traps exits; outside a supervision tree
"this function is called if it receives an `'EXIT'` message from its parent";
"if the `gen_server` process does not trap exits, the `gen_server` process
terminates immediately" ([gen_server terminate](https://www.erlang.org/doc/apps/stdlib/gen_server.html#c:terminate/2)).
In `gen_server.erl` any `{'EXIT', Parent, Reason}` (the `proc_lib` parent
stored at start), including `normal`, is handled by the behaviour, not passed
to `handle_info`. The supervisor's shutdown is `exit(Child, shutdown)`, then
`exit(Child, kill)` after the child's shutdown timeout
([supervisor child spec `shutdown`](https://www.erlang.org/doc/apps/stdlib/supervisor.html#t:shutdown/0)).
The `'EXIT'` message queues behind messages already in the mailbox.

**Today.** Supervisor and system shutdown call `Scope::stop(Shutdown)`
([system.rs `Scope::stop`](../../concurrency/src/system.rs); supervision.rs
`ChildView::stop`, `stop_generation`), which commits `Shutdown`, closes the
scope and cancels the child's token, trapping or not. A `start_linked` child
that traps exits receives its parent's death in `exit_received`.

**Proposed.**

- **Parent identity.** Each actor instance has an immutable parent, fixed at
  spawn before the link is registered and before `started()`:
  - `start_linked(parent_ctx)`: the actor whose `Context` was passed
    (normally the caller);
  - a supervised child: its supervisor (each replacement generation gets the
    same supervisor parent);
  - a standalone actor started on an `ActorSystem` or group: the system;
  - an unmanaged `.start()`: none.

  Supervisors and systems are not actors; their signals use an internal
  entry point on the actor's scope that marks them as parent signals.
  `ChildHandle::exit` called from outside an actor is never a parent signal
  (`from = ActorId::EXTERNAL`). `ctx.exit` from the parent actor is a parent
  signal, as any `'EXIT'` from `Parent` is in OTP. `ctx.link` never changes
  the parent.
- **Delivery.** A parent signal to a non-trapping actor follows E2 (a parent's
  `Normal` is ignored, `Shutdown` begins exit). To a trapping actor it is
  queued as a *parent exit*; when dispatched, exit begins with that reason
  (`stopped()` runs). `exit_received` is not called and cannot ignore it.
  This includes `Normal`: a trapping `start_linked` child ends when its parent
  ends normally, as a gen_server does. Users who want to observe the parent's
  exit instead use `ctx.link`, not `start_linked`.
- **A parent already past D.** `start_linked` with such a parent registers the
  link against a gone actor (E4): the child receives a parent exit with
  `NoProc` (trapping: queued; non-trapping: begins exit with `NoProc`).
- **No early commitment.** Queuing a parent exit commits no reason and cancels
  neither the child's token nor its helpers. Supervisor or system shutdown
  still closes admission for the whole managed tree at once (semantics S1), so
  a trapping child working through its backlog cannot start new helpers,
  timers or registrations. This is B: admission closure bounds the work a
  shutting-down tree can start.
- **Deadlines.** From the parent's Shutdown, the child has `shutdown_timeout`
  to reach publication. At that deadline the parent sends `kill` (E1), which
  supersedes the reason and, per open question 2, skips `stopped()` if it has
  not started. The kill gets a second `shutdown_timeout`; if the child has
  still not published, today's cleanup-stuck rule applies (V3, V9): the slot
  is stuck, the supervisor fails, nothing replaces the child. A supervisor
  `shutdown(deadline)` still signals every remaining sibling at its report
  deadline; escalation per child happens at the earlier of its own
  `shutdown_timeout` and that deadline. "Done" keeps its V9 meaning (no live
  generation, every controller finished).
- **Behaviour change.** A trapping child now finishes the messages queued
  before the shutdown before it stops, as in OTP. Today it stops after its
  current handler.

Model: `ShutdownEndsChild`; control `ParentAsExit`.

### E4. NoProc

**OTP.** `monitor/2`: the `'DOWN'` info is "either the exit reason of the
process, `noproc` (process or port did not exist at the time of monitor
creation), or `noconnection`"; the monitor fires once
([monitor/2](https://www.erlang.org/doc/apps/erts/erlang.html#monitor/2)).
`link/1` to a local process that does not exist "will raise a `noproc` error
exception" when the caller is not trapping exits; the documentation calls the
expected behaviour an exit signal with reason `noproc`, which is what a
trapping caller (and a remote link) gets as `{'EXIT', Pid, noproc}`
([link/1](https://www.erlang.org/doc/apps/erts/erlang.html#link/1),
[processes](https://www.erlang.org/doc/system/ref_man_processes.html#receiving-exit-signals)).
`gen_server:call` to a dead server exits with `noproc`. A process that is
exiting but not yet gone answers link and monitor requests with its real
reason.

**Today.** `register_link` returns the stored `exited` reason and delivers it
under X3 ([link.rs](../../concurrency/src/link.rs)); `monitor` waits on
`wait_exit_async` and delivers the published reason
([tasks/actor.rs `Context::monitor`](../../concurrency/src/tasks/actor.rs)).

**Proposed.** D is the boundary for all four operations:

- `ctx.monitor(&h)` after D: exactly one `Down { reason: NoProc }`. Before D:
  exactly one `Down` with the final reason, delivered at publication.
- `ctx.link(&h)` after D, caller trapping: one `Exit { from: h, reason: NoProc }`,
  no link retained. Caller not trapping: open question 1.
- `send`/`request` after D: `NoProc` (shape per open question 4). Before D they
  are admitted and answered with the final reason at D if not handled.
- Before D, including while `h` runs `stopped()`, links and monitors register
  and fire with the final reason, as OTP's exiting process answers.

Registration takes the target's exit-table lock, which D also holds, so each
registration is linearised either before D (fires at publication) or after it
(NoProc). Model: `NoProcIffLate`, `NoEdgeToDead`, `DownDelivered`; controls
`LateRealReason`, `SplitDeathPoint`.

### E5. ExitReason

```rust
#[non_exhaustive]
pub enum ExitReason {
    Normal,
    Shutdown,
    Panic(String),
    Kill,    // a self-exit with Kill, or a Kill link notice
    Killed,  // killed by a kill signal (E1), or dropped by the runtime (G1)
    NoProc,  // the target no longer existed (E4)
}
```

`is_abnormal()` is true for `Panic`, `Kill`, `Killed` and `NoProc`. `Display`
gives `killed` and `noproc`. Both can also be sent explicitly with `exit` or
`stop_with_reason` and then propagate like any reason. The guarantees are only
that a kill signal produces `Killed` and a registration after D produces
`NoProc`.

### E6. Unlink and demonitor are fences

**OTP.** "Once `unlink(Id)` has returned, it is guaranteed that the link
between the caller and the entity referred to by `Id` has no effect on the
caller in the future (unless the link is setup again)"; an `{'EXIT', Id, _}`
already in the queue may remain. "Once `demonitor(MonitorRef)` has returned, it
is guaranteed that no `{'DOWN', MonitorRef, _, _, _}` message, because of the
monitor, will be placed in the caller message queue in the future"
([unlink/1](https://www.erlang.org/doc/apps/erts/erlang.html#unlink/1),
[demonitor/1](https://www.erlang.org/doc/apps/erts/erlang.html#demonitor/1)).

**Today.** A `Down` is enqueued only if the observer's monitor entry is still
present, so `demonitor` already fences tasks-mode delivery
([tasks/actor.rs `Context::monitor`](../../concurrency/src/tasks/actor.rs)). A
link notice taken by `propagate_exit` is delivered even if the peer unlinks in
between (semantics X5: "`unlink` stops future propagation but does not retract
a signal in flight").

**Proposed.** Both are fences at the receiver. A notice taken at D is applied
(enqueued as `Exit`/`Down`, or a stop for a non-trapping peer) only if the
receiver's edge record still exists, checked under the receiver's lock at
delivery; `unlink` and `demonitor` remove the record under that lock. A link
record carries a generation so a relink after unlink is not mistaken for the
old edge. A message already queued stays. Model: `CancelFenced`; control
`NoFence`.

### E7. Ordering

Trapped `Exit`, parent exits and `Down` are ordinary mailbox messages: they
queue behind messages already there and never interrupt a handler.

- **Death notices** (from a link or monitor) are delivered at publication, so
  they follow every message the source sent earlier to the same observer
  (semantics X7). Model: `ExitAfterMessages`; control `EarlyNotify`.
- **Direct signals** (`ctx.exit` from a live actor) travel in the sender's
  per-sender FIFO with its messages: `message, exit, message` arrives in that
  order. A non-trapping target begins exit after its current handler and
  discards its queue, including earlier messages from the same sender, as a
  terminated process loses its mailbox.

### S1. Initial startup aborts on any first-start error (rank 5f)

**OTP.** If a child's start function fails or returns an error during
`start_link`, the supervisor "first terminates all already started child
processes with reason `shutdown`" and returns `{error, {shutdown, Reason}}`
(`{failed_to_start_child, Id, Error}` in `supervisor.erl` `start_children`)
([supervisor start_link](https://www.erlang.org/doc/apps/stdlib/supervisor.html#start_link/2)).
This does not depend on the restart type. Children start inside the
supervisor's `init`, so the `'EXIT'` of a child that started and then died
waits in the supervisor's queue until `init` returns and is handled then as
an ordinary exit, for every strategy. A start function may return `ignore`:
the spec is kept (unless the child is Temporary) without a process.

**Today.** Before initial readiness, a Permanent or Transient child whose
factory errors, panics or misses `startup_timeout` is retried within the
restart budget; a Transient child stopping `Normal`/`Shutdown` before
readiness and any Temporary failure fail the start
([supervision.rs, the `restart` decision in `run_child`](../../concurrency/src/supervision.rs);
semantics V7). A child that was ready and exits while later siblings start is
restarted at once under OneForOne and fails the start under OneForAll and
RestForOne ("ready child exited during initial tree startup").

**Proposed.** Two boundaries:

- **A child's first start** ends when its first generation becomes ready. If
  it fails for any reason (factory error or panic, panic in `started()`, a
  stop before readiness, startup timeout, placement or group closure), the
  supervisor's start fails, whatever the restart type. Started children stop
  in reverse order (rollback unchanged).
- **The initial start** ends when every child completed its first start (the
  tree commits ready). An exit of a ready child before that point is recorded
  and processed when the initial start completes, under V1 and V5 and the
  configured strategy, as OTP processes the queued `'EXIT'` after `init`. If
  the initial start aborts instead, the recorded exits are part of the
  rollback. A replacement started by that processing is a restart, not a
  first start.

`ignore` is out of scope (a factory cannot return it today). Model work for
the implementation: in `Supervision.tla`, `Retries(m, r, ready, init)` becomes
`init /\ (Restarts(m, r) \/ (~ready /\ m # "Temporary"))`, plus the deferral of
ready-child exits during the initial start, with the v0.7 retry as a negative
control and matching `Cohorts.tla` and trace changes.

### S2. `start()` returns its error after rollback

**OTP.** `proc_lib:start_link`: when the started process fails, "the start
function returns an error tuple when the started process exits"; `init_ack`
"can return before the failing process has exited", which is why OTP 26 added
`init_fail` ([proc_lib](https://www.erlang.org/doc/apps/stdlib/proc_lib.html#init_fail/3)).
The supervisor's own `init` terminates the started children (each through the
shutdown protocol) before it fails, so when `start_link` returns the error the
whole partial tree is gone.

**Today.** `SupervisorBuilder::start()` returns `Err` when the core's readiness
watch resolves false; the failed child's controller publishes `Failed` and
finishes cleanup afterwards (#223: `placement_failure_is_reported_and_fails_the_supervisor`
asserted the child state and failed 5 of 15 runs).

**Proposed.** Awaiting `start()` returns `Err` only after every child is
terminal (`Failed` or `Stopped`) and the rollback finished: every started
generation published, or its E3 deadlines passed (then it is `Failed` with
"cleanup stuck" and the pending cleanup stays owned by the system and appears
in its shutdown report). The error names the child whose first start failed
and its cause, as `failed_to_start_child`, and lists any cleanup failures
separately. Dropping the `start()` future before it resolves still starts the
rollback asynchronously (semantics V7, `StartupGuard`); only awaiting waits.
A nested supervisor (`add_supervisor`) waits for its own rollback inside its
`started()`, so the outer child's `startup_timeout` must cover the inner
rollback; if it fires first, the outer start fails with a startup timeout for
that child.

### G1. Runtime-dropped actors end `Killed` (rank 5g)

**OTP.** No counterpart: the VM does not drop processes.

**Today.** tasks: an actor future dropped before completion (runtime shutdown,
abort, never polled) publishes `Kill` through the cleanup fallback
([tasks/actor.rs `spawn_inner`](../../concurrency/src/tasks/actor.rs)), and
`wait_exit` maps a closed completion channel to `Kill`
([child_handle.rs `poll_exit_reason`, `wait_loop`](../../concurrency/src/child_handle.rs)).
threads: the fallback is `Panic("unexpected framework panic")`
([threads/actor.rs `spawn`](../../concurrency/src/threads/actor.rs)), reached
when the thread closure is dropped without running.

**Proposed.** Both modes report `Killed`: the actor was terminated from outside
without a say, which is what `Killed` means (E1). If a non-`Normal` reason was
already requested it wins (today's rule). The drop goes through D (callers
answered, table closed) and publication; `stopped()` does not run. Links
receive `Killed` (trappable), monitors `Down { Killed }`, supervisors restart
Permanent/Transient children. `wait_exit` on a closed completion channel
returns `Killed`. Tested in both modes. Model: `RuntimeDrop`, `KilledFromKill`.

## Interplay with rank 11

Rank 11 ([deferred-replies.md §5](deferred-replies.md#5-owner-lifecycle))
defines exit begin, `stopped()` that may still answer tokens, and death, and
leaves 5e to "only add the reason". With the phases above:

- **Ignored `Normal` signal:** never begins exit; no token is touched.
- **Trapped exit signal:** a queued `Exit`. If `exit_received` stops the actor,
  that is a self-exit and runs phases 2-6.
- **Parent exit to a trapping actor (E3):** runs phases 2-6 when dispatched,
  so `stopped()` can still answer stored tokens.
- **Kill:** supersedes the requested reason until D. Every token still open
  at D is answered with `Killed`.
- **Callers get the final reason.** A request queued or deferred at D is
  answered with the final reason (OTP `{Reason, {gen_server, call, ..}}`); a
  request sent after D reports `NoProc`; a request sent while the actor is
  exiting is admitted (D3) and answered at D. The error's shape is open
  question 4.
- **Death boundary.** Rank 11's "nothing is closed early" holds: nothing closes
  before `stopped()` returns. Its step order changes as described in
  [Phases](#phases-of-an-exit): admission, registry and exit table close
  together at D, before destruction; delivery of links and monitors happens at
  publication, after destruction.

## API changes

| Today | Proposed |
|---|---|
| `ExitReason` exhaustive, four variants | `#[non_exhaustive]`, plus `Killed`, `NoProc` |
| no exit-signal API | `Context::exit(&self, &ChildHandle, ExitReason)`, `ChildHandle::exit(ExitReason)`, `ChildHandle::kill()`, `ActorId::EXTERNAL` |
| `ChildHandle::stop`/`stop_with_reason`/`shutdown` | unchanged behaviour, documented as administrative stops; `stop_with_reason(Kill)` per open question 5 |
| `Context::link(&ChildHandle)` | unchanged signature or `-> Result<(), NoProc>` per open question 1; `NoProc` after D |
| `Context::monitor` | `Down { reason: NoProc }` after D |
| `Context::unlink`, `demonitor` | fence notices already taken but not yet delivered |
| `Context::trap_exit` docs | "only a kill is untrappable; a parent's exit ends the actor normally" |
| `start_linked` | the parent's exit ends a trapping child through `stopped()` instead of reaching `exit_received` |
| supervisor/system shutdown | parent exit signal (E3), kill at `shutdown_timeout`, stuck after a second `shutdown_timeout` |
| `SupervisorBuilder::start` | `Err` on any first-start failure, returned after rollback; ready children's exits during startup processed afterwards |
| `ChildHandle::mailbox_cleanup_failure()` | generalised to `cleanup_failure()` (state and queued destructors after D) |
| runtime drop | `Killed` in both modes |
| `ActorError` | with rank 11: `#[non_exhaustive]`; reason on stop errors per open question 4 |
| `spawned-test` `faults::exit` | uses `ChildHandle::exit`/`kill` instead of a linked helper and `stop_with_reason(Kill)`, so injected exits are exact (ROADMAP rank 6 asks for this) |
| lifecycle trace | events for exit signals, parent exits, D, publication, fences and NoProc, so `Lifecycle.tla` can replay real runs |

## Migration notes

| Operation | v0.7 | v0.8 |
|---|---|---|
| `match` on `ExitReason` | exhaustive | add a wildcard arm |
| `ChildHandle::stop_with_reason(Kill)` | ends `Kill` | per open question 5 |
| new `ChildHandle::kill()` | | ends `Killed`; links and monitors see `Killed` |
| `ctx.stop_with_reason(Kill)` | `Kill` | unchanged |
| `ChildHandle::stop`, `shutdown` | untrappable stop | unchanged |
| link or monitor to a gone actor | its real reason | `NoProc` (use `ChildHandle::exit_reason()` for the real one) |
| `unlink`/`demonitor` racing a death | a notice in flight may still arrive | it does not; one already queued may |
| supervisor shutdown of a trapping child | stops after the current handler | processes earlier messages first; kill at `shutdown_timeout` |
| trapping `start_linked` child, parent exits | `exit_received` | stops with the parent's reason, `Normal` included |
| supervisor whose child fails its first start | retried (Permanent/Transient) | `start()` fails; make the factory tolerate the condition or retry `start()` |
| a ready child exits during the initial start | OneForOne restarts at once; OneForAll/RestForOne fail the start | processed after the initial start, under the strategy |
| `start()` returning `Err` | may return before the child is `Failed` | returns after rollback; can take up to two `shutdown_timeout`s per started child |
| state destructor panic after a `Normal` exit | exit becomes `Panic` | exit stays `Normal`; `cleanup_failure()` reports it; the supervisor restarts as for an abnormal exit |
| runtime-dropped threads actor | `Panic("unexpected framework panic")` | `Killed` |
| `stopped()` on kill and untrapped signals | always runs | per open question 2 |
| request errors | `ActorStopped` | per open question 4 |
| `Display` | | adds `killed`, `noproc` |

## Client mapping

Searched the `spawned-v0.7` branches (both at the 0.7.3 bump): ethrex
[`0dd4c105`](https://github.com/lambdaclass/ethrex/tree/0dd4c105f65d544b0fb5e92d8634a1699127f826),
ethlambda [`6a12bc75`](https://github.com/lambdaclass/ethlambda/tree/6a12bc753ae18c9d63f3dbf930edc700c4c4704c).
Neither client calls `trap_exit`, defines `exit_received`, links, monitors,
uses `stop_with_reason`, `Kill`, `is_abnormal`, `exit_reason()` or
`ChildHandle`.

| Rule | ethrex | ethlambda |
|---|---|---|
| E1 Killed | none | none |
| E2 exit signals | none, because `Context::stop` stays a self-exit. Two sites call `stop()` on a `Context` clone from outside the handler: [`rlpx/connection/server.rs:1162`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L1162) (a stream listener stopping its own actor) and the test [`shutdown_order_tests.rs:197`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/test/tests/p2p/shutdown_order_tests.rs#L197) (`table.actor_ref().context().stop()` on a mock, then `join`) | none: [`http_server.rs:87`](https://github.com/lambdaclass/ethlambda/blob/6a12bc753ae18c9d63f3dbf930edc700c4c4704c/bin/ethlambda/src/http_server.rs#L87) stops its own actor from a helper so the Permanent supervisor restarts it |
| E3 parent exit | none (no trapping actors; supervised and system-owned actors stop as today) | none |
| E4 NoProc, E6 fences | none | none |
| E5 non_exhaustive | no break ([`discovery/supervised.rs:261`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/discovery/supervised.rs#L261) uses `matches!`); `shutdown.rs:170` logs with `Display` | no break ([`http_server.rs:242`](https://github.com/lambdaclass/ethlambda/blob/6a12bc753ae18c9d63f3dbf930edc700c4c4704c/bin/ethlambda/src/http_server.rs#L242)) |
| S1 startup aborts | **behaviour change**: the Transient discovery child's factory [`DiscoveryServer::build`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/discovery/supervised.rs#L145) can fail at boot (a UDP send error pinging a bootnode); v0.7 retries it, v0.8 fails `start()` and `init_network` panics on `.expect("Network starts")`. Fix: make the bootnode ping best-effort | none: the HTTP child's factory is infallible; a bind failure happens after readiness and is a normal restart |
| S2 start ordering | none | none |
| G1 runtime drop | none (`wait_exit` results are discarded) | none |
| rank 11 reason on stop errors | [`peer_handler.rs:1251`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/peer_handler.rs#L1251) matches `ActorError` exhaustively on purpose, so a new variant or payload is a compile break by design; two tests construct `ActorStopped` | one test mock constructs `ActorStopped` |

`ActorRef::context()` is public, so a `Context` can be used from outside the
actor. This design keeps `Context::stop` a self-exit, which the clients' two
production sites need; the ethrex test that stops a mock through it keeps
working and would read better as `ChildHandle::stop()`.

What the clients gain: nothing they hand-build today, because they use none
of links, monitors or trapping. The value is that these primitives become
safe to adopt with OTP's meaning (for example a connection actor monitoring
its peer table and seeing `NoProc` when it starts after the table died), and
that ethrex's boot path stops hiding a failed discovery start behind retries.

## What the model establishes

[`Lifecycle.tla`](../../verification/tla/Lifecycle.tla) models actors, outside
code (`ChildHandle` holders, a caller) and one supervisor, with the six phases
above (`Die` is D, `Publish` is publication). Configurations:

- `MCLifecycle` (every PR, safety): two actors, `a1` supervised and the
  `start_linked` parent of `a2`; one request, one monitor, one actor-to-actor
  message, one exit signal (from either actor or from outside) or
  administrative stop, one registration after D, runtime drop. Reasons are
  one per behaviour class: self-exits `Normal`, `Panic`, `Kill`; signals
  `Normal`, `Shutdown`, `Kill`; administrative `Shutdown`.
- `MCLifecycleLive` (every PR): the liveness properties at the same bounds
  without the actor-to-actor message.
- `MCLifecycleDeep` (weekly, safety): every reason for every operation (so
  `Killed` and `NoProc` are also sent explicitly), `unlink`, and turning
  `trap_exit` off.

A third actor (transitive propagation through a non-trapping middle actor)
is out of reach for TLC at these bounds; transitive propagation is the same
`Signal` step applied twice and is covered by tests (semantics X3).

Handlers are atomic and nondeterministic (reply, panic, ignore or act on a
trapped exit); a panic in any turn is a self-exit.

Invariants:

| Invariant | Meaning |
|---|---|
| `NoOrphanRequest` | a queued request is in exactly one live mailbox; past D it was answered (PR #182) |
| `NoDispatchAfterStop` | nothing is dispatched after the actor began exiting (PR #185) |
| `DownAtMostOnce` | at most one `Down` per monitor |
| `NoProcIffLate` | `NoProc` exactly for registrations after D; a death notice carries the source's final reason; a caller ends `NoProc` from `link` only with reason `NoProc` |
| `TrapRespected` | a trapping actor ends only by a self-exit, a kill, a runtime drop, a parent exit or an administrative stop |
| `NormalSignalIgnored` | a `Normal` exit signal or link notice never stops a non-trapping actor |
| `KilledFromKill` | a kill ends the target `Killed`, also when it arrives before D during exit; a runtime drop ends `Killed` |
| `NoEdgeToDead` | no live link or monitor points at an actor past D |
| `ExitAfterMessages` | a death notice follows every earlier message from its source |
| `CancelFenced` | no notice takes effect after `unlink`/`demonitor` cancelled it |

Liveness (`MCLifecycleLive`). These are conditional: they assume handlers,
`stopped()` and destruction return (weak fairness on dispatch, `Die`,
`Publish`, delivery and enqueue). They do not establish bounded shutdown when
user code hangs; that is the cleanup-stuck rule, checked by
`Supervision.tla`'s `CleanupStuck` and tests.

| Property | Meaning |
|---|---|
| `RequestsAnswered` | every sent request ends `Ok`, `ActorStopped` or `NoProc` |
| `ExitCompletes` | an exit, once begun, is published |
| `ShutdownEndsChild` | a supervisor's Shutdown ends the child, trapping or not, without the kill |
| `DownDelivered` | a monitor whose target is dead (fired at D, registered late, or still registered) delivers its `Down` unless the observer is gone or demonitored |
| `LinkedPeersStop` | an abnormal death notice stops a non-trapping peer or queues an `Exit`/parent exit for a trapping one, unless unlinked |

Negative controls (`negative_checks.py`; each checks only the named property;
the two liveness controls run on a smaller instance, `LIFECYCLE_SMALL`):

| `Fault` | Reintroduces | Fails |
|---|---|---|
| `DirectStop` | v0.7: exit signals ignore `trap_exit`, `Normal` stops too | `TrapRespected`; `NormalSignalIgnored` |
| `KillAsKill` | v0.7: a kill ends `Kill` | `KilledFromKill` |
| `KillNoSupersede` | a kill during exit keeps the requested reason | `KilledFromKill` |
| `NoFence` | v0.7: `unlink`/`demonitor` do not cancel a notice in flight | `CancelFenced` |
| `LateRealReason` | v0.7: link/monitor to a gone actor report its real reason | `NoProcIffLate` |
| `Retain182` | PR #182: a request published after D is kept without a reply | `NoOrphanRequest` |
| `Dispatch185` | PR #185: work dispatched after stop was observed | `NoDispatchAfterStop` |
| `DropQueued` | D does not answer queued requests | `NoOrphanRequest` |
| `EarlyNotify` | links and monitors fire when exit begins | `ExitAfterMessages` |
| `SplitDeathPoint` | registration checks publication instead of D | `NoEdgeToDead`; `DownDelivered` (a monitor registered between D and publication never fires) |
| `ParentAsExit` | a parent's Shutdown reaches `exit_received`, which may ignore it | `ShutdownEndsChild` |

Not modelled: handler bodies and time; the reply-slot machinery of rank 11
(its own models); supervision policy and the S1/S2 startup rules, which
change `Supervision.tla` (see S1) in the implementation PR; deadlines and the
kill escalation timing (the kill itself is modelled as `SupKill` without
fairness). Trace validation of real runs against `Lifecycle.tla` needs the
lifecycle-trace events in the API table and is part of the implementation, as
the verification rule requires.

## Differences from OTP that remain

| Difference | OTP | Spawned (proposed) | Category and reason |
|---|---|---|---|
| Stop and kill are cooperative | `kill` ends a process at once, also inside `terminate` | the current handler and a running `stopped()` finish; the reason still becomes `Killed` | R: Rust cannot preempt or kill a thread |
| Two points after the last hook (D and publication) | none: a process has no destructors | `NoProc` for requests and registrations can precede `exit_reason()`; a hanging destructor delays publication, not D | R: state destructors run after the last hook and may hang |
| Destructor panics | none | recorded as cleanup failure, not as the exit reason | R: as above |
| Administrative stop | `gen_server:stop` is synchronous and waits for the backlog (system message in order) | `ChildHandle::stop`/`stop_with_reason`/`shutdown` return at once, stop after the current handler and discard the queue; untrappable | B: stop latency is one handler, independent of an unbounded backlog (it does not bound a hanging handler); draining is opt-in (rank 13) |
| `stopped()` on kills and untrapped signals | skipped (`terminate` does not run) | open question 2 | depends on the decision |
| A non-kill signal while exiting | a non-trapping process in `terminate` dies with it | open question 9 | depends on the decision |
| Exit reasons | any term (`{shutdown, Term}`, custom atoms) | a closed, non-exhaustive enum | T: typed reasons; `Panic(String)` carries free text |
| Signal sender from outside an actor | always a pid | `ActorId::EXTERNAL` for `ChildHandle::exit` called outside an actor | R: non-actor Rust code has no identity |
| `link` to a gone local actor when not trapping | raises a `noproc` exception | open question 1 | T if a `Result`; none if an exit signal |
| Shutdown admission | a shutting-down supervisor does not restrict what children start | the whole managed tree closes admission at once | B: bounds work started during shutdown |
| `noconnection` | `Down`/`Exit` reason for a lost node | absent | single process (no distribution) |
| Runtime drop | no counterpart | `Killed` | R: an async runtime can drop a future |

Already aligned: per-sender FIFO, at-most-once delivery, stop discarding the
mailbox, `Normal` link notices ignored by non-trapping peers, transitive
propagation, ordering of death notices, unlink/demonitor fences, the restart
table, first-start abort and deferred startup exits.

## Alternatives considered

1. **Redefine `ChildHandle::stop`/`stop_with_reason`/`shutdown` as exit
   signals** (the literal roadmap reading). Rejected: `stop()` on a
   non-trapping actor would become a silent no-op, and `shutdown()` on a
   trapping actor a request it may ignore. Both would compile unchanged. OTP
   keeps exit signals and `gen_server:stop` separate.
2. **Keep `Kill` for outside kills and only make it trappable over links**
   (today). Rejected: an observer cannot tell "the peer killed itself" from
   "someone killed the peer", which `killed` exists to distinguish.
3. **First requested reason wins even against a kill** (today's L3).
   Rejected: OTP's kill ends a process in `terminate` as `killed`; Rust does
   not force keeping the earlier reason, only finishing the running code.
4. **Death point after destruction** (rank 11's original order). Rejected:
   the reply registry must close before the state that may hold tokens is
   dropped, so callers would see a reason before `Down` does, and a destructor
   panic could make them disagree.
5. **Retry initial startup for some causes (startup timeout).** Rejected for
   OTP alignment; offered as an opt-in only if open question 8 says so.
6. **A new `ExitReason::Dropped` for runtime drops.** Kept as open question 3.
7. **NoProc only for monitors, real reason for links.** Rejected: OTP gives
   `noproc` for both, and one death point serves both.
8. **Deliver a parent's Shutdown to `exit_received` and let the actor decide.**
   Rejected: a trapping actor that ignores it holds supervisor shutdown until
   the kill; gen_server never lets user code see the parent's exit. Control
   `ParentAsExit`.

## Open questions

1. A non-trapping actor calls `link` on an actor past D: should `link` return
   an error (`Result<(), NoProc>`, the counterpart of OTP's local `noproc`
   exception), or should the caller receive a `NoProc` exit signal and exit
   (OTP's documented signal semantics, what a remote link does)? The model
   allows both.
2. Should an actor run `stopped()` when it is killed before `stopped()`
   began, or when a non-trapping actor exits because of an exit signal or link
   notice? OTP skips `terminate` in both cases; v0.7 always runs `stopped()`.
3. Runtime-dropped actors: `Killed`, or a distinct `ExitReason` variant?
4. Error shape for callers: replace `ActorError::ActorStopped` by a variant
   carrying `ExitReason` (with `NoProc` for not admitted), or keep
   `ActorStopped` and add an accessor? Decide with rank 11, which already makes
   `ActorError` non-exhaustive. ethrex's deliberately exhaustive match breaks
   either way.
5. `ChildHandle::stop_with_reason(Kill)`: an alias of `kill()` (ends `Killed`,
   supersedes), or an administrative stop with reason `Kill` as today?
6. Should `ChildHandle::shutdown()` stay an administrative stop, or become
   `exit(Shutdown)` (trappable), the signal a supervisor sends?
7. (Resolved in round 1: folded into S1.)
8. Should there be an opt-in to retry a child's first start (ethrex's
   discovery boot path relies on it implicitly), or is application-level retry
   enough?
9. A non-trapping actor that already began exiting receives a non-kill exit
   signal or abnormal link notice (for example a linked peer panics while it
   runs `stopped()` after a `Normal` stop): keep the first requested reason
   (today's L3, which a regression test pins), or replace it as OTP does for a
   process still in `terminate`?

## Review resolutions

Round 1: Codex no-go (one blocker, eight majors), agy go with changes (six
majors). Both reviewed the design and model without the author's answers to
the open questions.

### Round 1, Codex

| Finding | Resolution |
|---|---|
| 1 (blocker). Death boundary: registry and mailbox close before state drop and publication; requests, links and monitors could disagree; the "callers see `Normal`, `Down` sees `Panic`" split was not R | New [Phases of an exit](#phases-of-an-exit): one death point D after `stopped()` where the reason becomes final and admission, registry and exit table close together; destruction and publication after it. Destructor panics after D are cleanup failures, not reasons, so callers, `Down` and `wait_exit` agree. Rank 11's step order is refined, its "nothing closes early" kept. Model: `gone` and `dead` phases, `Publish`, delivery only after publication; control `SplitDeathPoint` now also fails `DownDelivered`. |
| 2. A requested graceful exit immunises the actor against kill | Kill supersedes any requested reason until D (E1); running code still finishes (R). Model: `Kill` updates the reason while `stopping`; `SupKill` allowed while `stopping`; control `KillNoSupersede`. |
| 3. `unlink`/`demonitor` do not fence notices in flight | Adopted as E6: receiver-side fences with link generations. Model: `Unlink`/`Demonitor` cancel pending notices, `CancelFenced`, `DownDelivered` excludes demonitored monitors, control `NoFence`. |
| 4. Shutdown escalation and stuck deadlines collide; `start()` error contents unspecified | E3 "Deadlines": kill at `shutdown_timeout`, stuck after a second `shutdown_timeout`, V3/V9 meanings kept. S2 says what `start()` waits for and returns (startup cause plus cleanup failures; pending cleanup owned by the system). |
| 5. Parent identity under-specified | E3 "Parent identity": immutable per instance, set before the link and `started()`, internal entry point for supervisors and systems, `EXTERNAL` never a parent, linking never reparents, replacements keep the supervisor, dead `start_linked` parent gives a `NoProc` parent exit. |
| 6. S1 mixes two boundaries; OneForOne also restarts during startup | S1 now separates a child's first start from the initial start; ready children's exits during the initial start are deferred for every strategy, as OTP's queued `'EXIT'`. Open question 7 folded in (both reviewers recommended aligning now). `Supervision.tla` change specified for the implementation. |
| 7. E7 applies death-notice rules to direct signals | E7 split: death notices vs direct signals (per-sender FIFO). Model: items carry an origin (`death`, `late`, `direct`); actors send direct signals (`ExitSignal(s, a, r)` with `s` an actor or outside); `NoProcIffLate` and `ExitAfterMessages` apply to death and late notices only. |
| 8. API accepts every reason but the model restricted them | Model sends any reason from self-exits, signals and administrative stops; `KilledFromKill` now states only "a kill ends `Killed`" (and runtime drop); E5 says `Killed`/`NoProc` can be sent and propagate. |
| 9. Liveness results are conditional; `LinkedPeersStop` too weak; G1 and trap-off not modelled | Assumptions stated beside the liveness table. `LinkedPeersStop` now requires an `Exit`/parent exit from that source or a stop, or a fence. `RuntimeDrop` action added. Turning trapping off in `MCLifecycleDeep`. |
| 10. `ParentAsExit` accepted any temporal violation | `named_temporal` runs only `ShutdownEndsChild` (and `DownDelivered` for `SplitDeathPoint`). |
| 11. `exit_signal/2` vs `exit/2`; `ignore` and Temporary | Terms now follow `exit_signal/2` and define a signal to oneself; S1 qualifies `ignore`. |
| 12. Migration overstated `Killed`; missing operations; `gen_server:stop` is synchronous | Per-operation migration table; administrative stop row says it returns at once and that B covers backlog only. |

### Round 1, agy

| Finding | Resolution |
|---|---|
| 1. `LinkedPeersStop` satisfied by any message from the source | Fixed as Codex 9. |
| 2. Non-trapping late link not covered by `NoProcIffLate`; reasons restricted; no actor-originated signals | `NoProcIffLate` adds `cause = noproc => NoProc`; full reason domain; actor-originated `ExitSignal`. |
| 3. Parent exit queueing vs `Scope::stop`, token cancellation and admission | E3 "No early commitment": queuing commits nothing and cancels no token or helper; tree admission still closes at shutdown (listed as B). E2 says the same for trapped `Exit`. Trap flag read at arrival; queued exits stay messages (Terms). |
| 4. Parent identity; `Normal` parent exit ends a trapping `start_linked` child | As Codex 5; the `Normal` case is called out in E3 and the migration table. |
| 5. Precedence of signals, kill and panic during a turn; kill during `stopped()` | Kill supersedes until D (Codex 2). Other signals during exit: open question 9 (L3 today). Difference row for cooperative kill. |
| 6. S1/S2 vs `Supervision.tla`, `StartupGuard` drop, nested supervisors | `Retries` change specified; dropping `start()` keeps asynchronous rollback; nested supervisor timing in S2. |
| 7. Window between mailbox closure and death point | Removed by D (Codex 1). |
| 8. `spawned-test` `faults::exit` | API table row. |
| 9. `SplitDeathPoint` checked an internal invariant; deep config used `FairSpec` | Also checked against `DownDelivered`; `MCLifecycleDeep` uses `Spec`. |
| 10. Open-question placeholder in the differences table | Question 7 resolved into S1; remaining rows reference decisions explicitly. |
