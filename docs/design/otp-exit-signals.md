# Exit signals, startup and runtime-drop semantics as in OTP: design contract

**Status: design in progress, not implemented.**

- Covers ROADMAP rank 5e (exit signals), the startup part of rank 5f, rank 5g
  (runtime-dropped actors) and the supervisor `start()` ordering found by the
  quality pass (#223). Counting the restart budget per recovery round (the
  other half of 5f) is a separate change.
- Target: v0.8. Changes sections L3, L4, X1-X7, V7 and §11 of the
  [semantics contract](semantics.md).
- Model: [`Lifecycle.tla`](../../verification/tla/Lifecycle.tla), the "base
  actor lifecycle" model the [verification rule](../../ROADMAP.md#verification-rule)
  calls for. See [What the model establishes](#what-the-model-establishes).
- Interplay with rank 11 (deferred replies, PR #221, still open): [§8](#8-interplay-with-rank-11).
- Illustrative API shapes are not copyable code.

**Project rule.** Match Erlang/OTP. Deviate only where:

- **R:** Rust forces it;
- **T:** Rust types add strictness; or
- **B:** Spawned adds a bound OTP lacks.

## Summary

| # | OTP | Spawned today (v0.7.3) | Proposed (v0.8) |
|---|---|---|---|
| E1 | `exit(Pid, kill)` is untrappable; the target exits `killed`, and links receive `killed`, which is trappable | `stop_with_reason(Kill)` exits `Kill`; links receive `Kill` (trappable) | `ChildHandle::kill()` is untrappable; the target exits `Killed`; links and monitors receive `Killed`. A self-exit with `Kill` stays `Kill` (OTP `exit(kill)`) |
| E2 | `exit(Pid, normal)` is ignored by a non-trapping process; every other reason except `kill` is trappable | every outside stop ignores `trap_exit` and always stops the actor, `Normal` included | new exit-signal API (`ctx.exit`, `ChildHandle::exit`) with exactly OTP's rules; `stop`/`stop_with_reason`/`shutdown` stay administrative stops (see [E2](#e2-exit-signals-and-administrative-stops)) |
| E3 | a trapping gen_server that gets `'EXIT'` from its parent runs `terminate(Reason)` | supervisor shutdown cancels a trapping child directly; a `start_linked` parent's exit reaches `exit_received` | an exit signal from the actor's parent (its supervisor, its system, its `start_linked` parent) is queued and, when dispatched, runs normal termination with that reason; it never reaches `exit_received` |
| E4 | `link/1` or `monitor/2` on a process that no longer exists gives `noproc` | the target's real exit reason | `NoProc` |
| E5 | exit reasons are any term | closed four-variant enum | `#[non_exhaustive]`, adds `Killed` and `NoProc` |
| E6 | one death point per process | links close before monitors fire and before the exit is published | one death point: registration of links and monitors is atomic with it |
| S1 | `start_link` aborts on any child start error | Permanent/Transient children retry a failed factory, panic or startup timeout during the first start | any initial start failure aborts the start and rolls back |
| S2 | `start_link` returns its error after the failed child exited and started children were terminated | `start()` can return `Err` before the failed child's state is `Failed` | `start()` returns `Err` only after rollback finished (or each child's shutdown deadline passed) |
| G1 | no counterpart | tasks: `Kill`; threads: `Panic("unexpected framework panic")` | `Killed` in both modes |

## Terms

- **Exit signal.** What `erlang:exit/2` sends (renamed `exit_signal/2` in
  OTP 29): a sender, a target and a reason. Link notices (an actor died) are
  exit signals too.
- **Trapping.** `ctx.trap_exit(true)` (OTP `process_flag(trap_exit, true)`).
  Read when the signal is delivered to the target, as OTP checks it when the
  signal arrives.
- **Self-exit.** The actor ends itself: `ctx.stop()`, `ctx.stop_with_reason(r)`,
  a panic in a hook or handler (OTP `exit/1`, `{stop, Reason, State}`, a crash).
  Untrappable by definition. A `Context` clone used from a helper task or
  stream listener is still the actor's own capability and still a self-exit.
- **Administrative stop.** `ChildHandle::stop()`, `stop_with_reason(r)`,
  `shutdown()` called by someone else.
- **Parent.** The supervisor of a supervised child, the `ActorSystem` for a
  standalone managed actor, the actor that called `start_linked` for its
  child. OTP's `proc_lib` parent.
- **Death point (D).** The instant at which the actor stops existing for
  registration and observers. Rank 11 §5 step 3 places it after `stopped()`
  returned: the reply registry closes, the mailbox is disposed, the state is
  dropped, then "the exit reason is published, and monitors and links fire".
  This design makes that last step one atomic transition (E6).

## Rules

### E1. Kill and Killed

**OTP.** `exit(Pid, kill)`: "the receiver cannot trap the exit signal and will
unconditionally terminate", and it terminates "with the atom `killed` as exit
reason" ([processes, receiving exit signals](https://www.erlang.org/doc/system/ref_man_processes.html#receiving-exit-signals)).
Linked processes therefore receive `killed`, which is an ordinary trappable
reason. A process that exits *itself* with reason `kill` (`exit(kill)`)
propagates `kill` over its links; "an exit reason of `kill` will *not* be
converted to `killed`" for link signals, and a trapping receiver gets it as a
message, while a non-trapping receiver exits with `kill` (same section).

**Today.** `ChildHandle::stop_with_reason(Kill)` and `Context::stop_with_reason(Kill)`
both commit `Kill` ([child_handle.rs `stop_with_reason`](../../concurrency/src/child_handle.rs),
[link.rs `stop_with_reason`](../../concurrency/src/link.rs)); `make_signal`
delivers it to peers as `Kill`, trappable (semantics X4).

**Proposed.**

- `ChildHandle::kill()` (and `ctx.exit(&target, ExitReason::Kill)`, E2) is
  untrappable. The target commits `Killed` unless it already committed a
  reason, stops after its current handler and discards its queue.
- Its links receive `Killed`: a non-trapping peer exits `Killed` and
  propagates `Killed`; a trapping peer gets `Exit { reason: Killed }`.
  Monitors get `Down { reason: Killed }`.
- A self-exit with `Kill` (`ctx.stop_with_reason(Kill)`) stays `Kill`, and
  links receive `Kill` (trappable; a non-trapping peer exits `Kill`), as OTP's
  `exit(kill)`.
- `Killed` is abnormal: Permanent and Transient children restart.

Rust cannot preempt: "untrappable" means the actor does not get a say, not that
the current handler is interrupted (semantics L3, R).

### E2. Exit signals and administrative stops

**OTP.** `exit(Pid, Reason)`: with `normal`, "the signal has no effect if `P`
is not trapping exits"; when trapping, "the exit signal is transformed into a
message `{'EXIT', From, Reason}`"; with any other reason a non-trapping target
terminates with that reason; only `kill` cannot be trapped
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
   Delivery rule at the target (the same function link notices use):

   | Reason | Non-trapping target | Trapping target |
   |---|---|---|
   | `Kill` (direct, not a link notice) | exits `Killed` | exits `Killed` |
   | `Normal` | ignored | `Exit { from, reason: Normal }` queued |
   | anything else | exits with that reason after its current handler; queue discarded | `Exit { from, reason }` queued |
   | any, from the target's parent | as above | parent exit queued (E3) |

   A signal to an actor that already began exiting does nothing (the first
   committed reason wins, semantics L3). A signal to a dead actor is dropped,
   as in OTP.
2. **Administrative stops stay.** `ChildHandle::stop()`, `stop_with_reason(r)`
   and `shutdown()` keep their v0.7 meaning: a cooperative, untrappable stop
   after the current handler, `stopped()` runs, queued messages are discarded.
   They are documented as Spawned's administrative stop, the counterpart of
   `gen_server:stop` without waiting for the backlog. They are not exit
   signals, so `trap_exit` does not apply. See [Differences](#differences-from-otp-that-remain).
3. **Self-exits are unchanged** (`ctx.stop`, `ctx.stop_with_reason`, panics).

Why not turn `ChildHandle::stop()` into `exit(Pid, normal)` (the literal reading
of the roadmap row): it would make `handle.stop()` a silent no-op for every
non-trapping actor, which is most of them, with no compile error. OTP itself
keeps two primitives (exit signals and `gen_server:stop`); so does this
design. See [Alternatives](#alternatives-considered).

### E3. A parent's exit signal ends a trapping actor normally

**OTP.** `gen_server:terminate/2`: under a supervisor, it "is called with
`Reason=shutdown`" when the server traps exits and the supervisor's shutdown
strategy is a timeout; outside a supervision tree "this function is called if
it receives an `'EXIT'` message from its parent"; "if the `gen_server` process
does not trap exits, the `gen_server` process terminates immediately"
([gen_server terminate](https://www.erlang.org/doc/apps/stdlib/gen_server.html#c:terminate/2)).
In `gen_server.erl`, `{'EXIT', Parent, Reason}` from the `proc_lib` parent is
handled by the behaviour, not passed to `handle_info`. The supervisor's shutdown
is `exit(Child, shutdown)`, then `exit(Child, kill)` after the child's shutdown
timeout ([supervisor child spec `shutdown`](https://www.erlang.org/doc/apps/stdlib/supervisor.html#t:shutdown/0)).
The `'EXIT'` message queues behind messages already in the mailbox.

**Today.** Supervisor and system shutdown call `Scope::stop(Shutdown)`
([system.rs `Scope::stop`](../../concurrency/src/system.rs); supervision.rs
`ChildView::stop`, `stop_generation`), which commits `Shutdown` and cancels the
child directly, trapping or not. A `start_linked` child that traps exits
receives its parent's death in `exit_received`.

**Proposed.**

- The supervisor, the system coordinator and a `start_linked` parent send an
  exit signal marked as coming from the parent.
- Non-trapping target: E2's table (Shutdown stops it after its current handler;
  a parent's `Normal` exit is ignored).
- Trapping target: a *parent exit* is queued like any `Exit`; when dispatched,
  the run loop begins exit with that reason (`stopped()` runs, exit reason as
  sent). `exit_received` is not called and cannot ignore it.
- Shutdown timeout: if the child has not finished when `shutdown_timeout`
  expires, the supervisor sends `kill` (OTP's escalation). The kill is
  cooperative (R): it helps when the child is still behind its backlog, not
  when a handler or `stopped()` is stuck. If the child is still alive at the
  report deadline, today's cleanup-stuck rule applies unchanged.
- Behaviour change: a trapping child now finishes the messages queued before
  the shutdown before it stops, as in OTP. Today it stops after its current
  handler.

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
A process that is exiting but not yet gone answers link and monitor requests
with its real reason.

**Today.** `register_link` returns the stored `exited` reason and delivers it
under X3 ([link.rs](../../concurrency/src/link.rs)); `monitor` waits on
`wait_exit_async` and delivers the published reason
([tasks/actor.rs `Context::monitor`](../../concurrency/src/tasks/actor.rs)).

**Proposed.**

- `ctx.monitor(&h)` after `h`'s death point: exactly one
  `Down { reason: NoProc }`. Before it: exactly one `Down` with the real
  reason at death.
- `ctx.link(&h)` after `h`'s death point, caller trapping: one
  `Exit { from: h, reason: NoProc }`, no link retained. Caller not trapping:
  see open question 1.
- Before the death point (including while `h` runs `stopped()`), the link or
  monitor is registered and fires with the real reason, as OTP's exiting
  process does.
- A request or send to an actor after its death point reports `NoProc`
  (with rank 11, [§8](#8-interplay-with-rank-11)).

### E5. ExitReason

```rust
#[non_exhaustive]
pub enum ExitReason {
    Normal,
    Shutdown,
    Panic(String),
    Kill,    // a self-exit with Kill, or a Kill link notice
    Killed,  // killed from outside (E1), or the runtime dropped it (G1)
    NoProc,  // link/monitor/request against an actor that no longer exists (E4)
}
```

`is_abnormal()` is true for `Panic`, `Kill`, `Killed` and `NoProc`. `Display`
gives `killed` and `noproc`. `NoProc` is never an actor's own exit reason
except, depending on open question 1, a non-trapping caller of `link` that
exits because of it (as OTP's caller does).

### E6. One death point

**Today.** Death is spread over several steps: `propagate_exit` closes the link
table and signals links; `MonitorCleanup` cancels outgoing monitors; incoming
monitors fire when the completion channel publishes the reason, which happens
later (semantics L7). A link registered in between gets the real reason; a
monitor registered in between waits for publication. Both are correct today,
but with NoProc the boundary must be one instant, or a registration could
fall between "links closed" and "published" and get neither the real reason
nor NoProc.

**Proposed.** One per-actor *exit table* (today's `LinkState`, with monitor
watchers moved into it) guarded by one mutex. At the death point the actor
sets `exited = Some(reason)` and takes every link and watcher under that lock;
`link` and `monitor` register under the same lock and observe either "alive"
(edge stored, fires at death) or "dead" (NoProc). The exit reason is published
to `wait_exit`/`exit_reason` in the same critical section, so an observer that
sees the reason also sees the table closed. Delivery happens after the lock is
released, as today.

This is the rank 11 step 3.5 ("the exit reason is published, and monitors and
links fire") made atomic. Model: `NoEdgeToDead`, control `SplitDeathPoint`.

### E7. Ordering (unchanged, now modelled)

Trapped `Exit`, parent exits and `Down` are ordinary mailbox messages: they
queue behind messages already there and are enqueued only after the source's
death point, so they follow every message the source sent earlier to the same
observer (semantics X7; OTP's per-pair signal order). A non-trapping actor's
exit after a signal takes effect after its current handler and discards its
queue (D4). Model: `ExitAfterMessages`, control `EarlyNotify`.

### S1. Initial startup aborts on any child start error (rank 5f)

**OTP.** If a child's start function fails or returns an error during
`start_link`, the supervisor "first terminates all already started child
processes with reason `shutdown`" and returns `{error, {shutdown, Reason}}`
(`{failed_to_start_child, Id, Error}` in `supervisor.erl` `start_children`)
([supervisor start_link](https://www.erlang.org/doc/apps/stdlib/supervisor.html#start_link/2)).
This does not depend on the restart type. A start function may return
`ignore`, which keeps the child spec without a process.

**Today.** Before initial readiness, a Permanent or Transient child whose
factory errors, panics or misses `startup_timeout` is retried within the
restart budget; a Transient child stopping `Normal`/`Shutdown` before
readiness and any Temporary failure fail the start
([supervision.rs, the `restart` decision in `run_child`](../../concurrency/src/supervision.rs);
semantics V7).

**Proposed.** During the initial start (before the tree commits ready), any
failure of a child to reach readiness fails the start, whatever its restart
type and cause: factory error, factory panic, a panic in `started()`, a stop
before readiness, startup timeout, placement or group closure. Rollback is
unchanged: started children stop in reverse order. After initial readiness,
the restart table and the failed-restart rule (semantics V1, V7) are
unchanged. `ignore` is out of scope (a factory cannot return it today).

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

**Proposed.** `start()` returns `Err` only after every child is terminal
(`Failed` or `Stopped`) and the tree's rollback finished: every started
generation joined, or its `shutdown_timeout` passed (then the child is
`Failed` with "cleanup stuck", the existing rule). The returned error names the
failing child and its cause, as `failed_to_start_child`.

### G1. Runtime-dropped actors exit `Killed` (rank 5g)

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
without a say, which is what `Killed` means (E1). A non-`Normal` reason already
committed still wins (today's rule). Links receive `Killed` (trappable),
monitors `Down { Killed }`, supervisors restart Permanent/Transient children.
Tested in both modes.

## 8. Interplay with rank 11

Rank 11 ([PR #221](https://github.com/lambdaclass/spawned/pull/221),
`deferred-replies.md` §5) defines the death boundary: exit begins (registry
`Exiting`, dispatch stops, jobs cancelled), `stopped()` runs and may still
answer tokens, then death closes the registry, disposes the mailbox, drops the
state and publishes the exit. It leaves 5e to "only add the reason".

- **Ignored `Normal` signal:** never begins exit; no token is touched.
- **Trapped exit signal:** a queued `Exit`. If `exit_received` stops the actor,
  that is a self-exit and runs rank 11 steps 1-4.
- **Parent exit to a trapping actor (E3):** runs steps 1-4 when dispatched, so
  `stopped()` can still answer stored tokens.
- **Kill (`Killed`):** begins exit after the current turn; `stopped()` runs or
  not according to open question 2. Either way the death step answers every
  open token `ActorStopped` with reason `Killed`.
- **Callers get the reason.** A request whose slot is closed at death carries
  the reason committed when exit began (OTP `{Reason, {gen_server, call, ..}}`).
  A request sent after the death point reports `NoProc`. A request sent while
  the actor is exiting is accepted (semantics D3) and then answered with the
  committed reason at death. The shape of the error is open question 4.
- **Committed versus published reason.** The registry closes (step 1) before
  the state is dropped (step 3); a destructor panic can still turn a `Normal`
  exit into `Panic` (semantics L6). Callers then see `Normal`, `Down` and
  `wait_exit` see `Panic`. This is accepted and documented (R: the slots must
  close before the state that may hold tokens is dropped).
- **Death point.** E6's atomic step is rank 11's step 3.5; `NoProc` for
  requests uses the same point.

## API changes

| Today | Proposed |
|---|---|
| `ExitReason` exhaustive, four variants | `#[non_exhaustive]`, plus `Killed`, `NoProc` |
| no exit-signal API | `Context::exit(&self, &ChildHandle, ExitReason)`, `ChildHandle::exit(ExitReason)`, `ChildHandle::kill()`, `ActorId::EXTERNAL` (sender of an outside signal) |
| `ChildHandle::stop`/`stop_with_reason`/`shutdown` | unchanged behaviour, documented as administrative stops; `stop_with_reason(Kill)` per open question 5 |
| `Context::link(&ChildHandle)` | unchanged signature or `-> Result<(), NoProc>` per open question 1; NoProc instead of the real reason after the death point |
| `Context::monitor` | `Down { reason: NoProc }` after the death point |
| `Context::trap_exit` docs | "only `Kill` is untrappable; a parent's exit ends the actor normally" |
| `start_linked` | the parent's exit ends a trapping child through `stopped()` instead of reaching `exit_received` |
| supervisor/system shutdown | exit signal from the parent (E3), `kill` at `shutdown_timeout` |
| `SupervisorBuilder::start` | `Err` on any initial child failure, returned after rollback |
| runtime drop | `Killed` in both modes; `wait_exit` on a closed channel returns `Killed` |
| `ActorError` | with rank 11: `#[non_exhaustive]`; reason on stop errors per open question 4 |
| lifecycle trace | new events for exit signals, parent exits and NoProc, so `Lifecycle.tla` can replay real runs |

## Migration notes

- **Exhaustive matches on `ExitReason`** need a wildcard arm. Neither client
  has one (both only use `matches!(.., Some(ExitReason::Panic(_)))`).
- **Code matching `Kill` from a link or monitor** of an actor stopped from
  outside now sees `Killed`. Code that self-exits with `Kill` is unchanged.
- **Code relying on a link or monitor to a dead actor reporting its real
  reason** now gets `NoProc`; use `ChildHandle::exit_reason()` for the real
  one.
- **Trapping actors under a supervisor** no longer stop mid-backlog on
  supervisor shutdown; they process earlier messages first, bounded by
  `shutdown_timeout` and the kill that follows.
- **Trapping `start_linked` children** no longer see their parent's exit in
  `exit_received`; they stop. Use `ctx.link` instead of `start_linked` to keep
  observing it.
- **Supervisors whose children may fail to start at boot** (waiting for a
  network, a port, a file) now fail `start()`. Make the factory tolerate the
  condition (start, then retry inside the actor), or retry `start()` in the
  application.
- **Display output** gains `killed` and `noproc`.

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
| E2 exit signals | none if `Context::stop` stays a self-exit. Two sites call `stop()` on a `Context` clone from outside the handler: [`rlpx/connection/server.rs:1162`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/rlpx/connection/server.rs#L1162) (a stream listener stopping its own actor) and the test [`shutdown_order_tests.rs:197`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/test/tests/p2p/shutdown_order_tests.rs#L197) (`table.actor_ref().context().stop()` on a mock, then `join`) | none if `Context::stop` stays a self-exit: [`http_server.rs:87`](https://github.com/lambdaclass/ethlambda/blob/6a12bc753ae18c9d63f3dbf930edc700c4c4704c/bin/ethlambda/src/http_server.rs#L87) stops its own actor from a helper so the Permanent supervisor restarts it |
| E3 parent exit | none (no trapping actors; supervised and system-owned actors stop as today) | none |
| E4 NoProc | none | none |
| E5 non_exhaustive | no break ([`discovery/supervised.rs:261`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/discovery/supervised.rs#L261) uses `matches!`); `shutdown.rs:170` logs with `Display` | no break ([`http_server.rs:242`](https://github.com/lambdaclass/ethlambda/blob/6a12bc753ae18c9d63f3dbf930edc700c4c4704c/bin/ethlambda/src/http_server.rs#L242)) |
| S1 startup aborts | **behaviour change**: the Transient discovery child's factory [`DiscoveryServer::build`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/discovery/supervised.rs#L145) can fail at boot (a UDP send error pinging a bootnode); v0.7 retries it, v0.8 fails `start()` and `init_network` panics on `.expect("Network starts")`. Fix: make the bootnode ping best-effort | none: the HTTP child's factory is infallible; a bind failure happens after readiness and is a normal restart |
| S2 start ordering | none | none |
| G1 runtime drop | none (`wait_exit` results are discarded) | none |
| rank 11 reason on `ActorStopped` | [`peer_handler.rs:1251`](https://github.com/lambdaclass/ethrex/blob/0dd4c105f65d544b0fb5e92d8634a1699127f826/crates/networking/p2p/peer_handler.rs#L1251) matches `ActorError` exhaustively on purpose, so a new variant or payload is a compile break by design; two tests construct `ActorStopped` | one test mock constructs `ActorStopped` |

`ActorRef::context()` is public, so a `Context` can be used from outside the
actor. This design keeps `Context::stop` a self-exit (the clients' two
production sites need that); the ethrex test that stops a mock through it keeps
working, and would read better as `ChildHandle::stop()`.

What the clients gain: nothing they hand-build today, because they use none
of links, monitors or trapping. The value is that these primitives become
safe to adopt with OTP's meaning (for example a connection actor monitoring
its peer table and seeing `NoProc` when it starts after the table died), and
that ethrex's boot path stops hiding a failed discovery start behind retries.

## What the model establishes

[`Lifecycle.tla`](../../verification/tla/Lifecycle.tla) models three actors
(one `start_linked` pair, one supervised child), one outside caller, one
supervisor, one request, one monitor, bounded sends, signals and late
registrations, with `trap_exit` toggling at any turn. Handlers are atomic and
nondeterministic (reply, panic, ignore or act on a trapped exit). Death is one
atomic step (E6). It models the proposed contract; the v0.7 behaviours are
negative controls.

Invariants (TLC, `MCLifecycle.cfg`):

| Invariant | Meaning |
|---|---|
| `NoOrphanRequest` | a queued request is never left in a dead actor's mailbox (PR #182) |
| `NoDispatchAfterStop` | nothing is dispatched after the actor began exiting (PR #185) |
| `DownAtMostOnce` | at most one `Down` per monitor |
| `NoProcIffLate` | `NoProc` exactly for registrations after the death point; every other notice carries the source's committed reason |
| `TrapRespected` | a trapping actor ends only by a self-exit, a kill, a parent exit or an administrative stop |
| `NormalSignalIgnored` | a `Normal` exit signal or link notice never stops a non-trapping actor |
| `KilledFromKill` | an outside kill exits `Killed`; `Killed` arises only from kills and their propagation |
| `NoEdgeToDead` | no live link or monitor points at a dead actor (E6) |
| `ExitAfterMessages` | `Exit`/`Down` follow every earlier message from the same source |

Liveness (weak fairness on dispatch, death, delivery and enqueue):
`RequestsAnswered` (every sent request ends `Ok`, `ActorStopped` or `NoProc`),
`ExitCompletes`, `ShutdownEndsChild` (a supervisor's Shutdown ends the child
without needing the kill), `DownDelivered` (a fired or late monitor delivers
its `Down` unless the observer died) and `LinkedPeersStop`.

Negative controls (`negative_checks.py`, each must produce a counterexample):

| `Fault` | Reintroduces | Fails |
|---|---|---|
| `DirectStop` | v0.7: exit signals ignore `trap_exit`, `Normal` stops too | `TrapRespected`, `NormalSignalIgnored` |
| `KillAsKill` | v0.7: an outside kill exits and propagates `Kill` | `KilledFromKill` |
| `LateRealReason` | v0.7: link/monitor to a dead actor reports its real reason | `NoProcIffLate` |
| `ParentAsExit` | a parent's Shutdown reaches `exit_received`, which may ignore it | `ShutdownEndsChild` (liveness) |
| `Retain182` | PR #182: a request published after death is kept without a reply | `NoOrphanRequest` |
| `Dispatch185` | PR #185: work dispatched after stop was observed | `NoDispatchAfterStop` |
| `DropQueued` | death does not answer queued requests | `NoOrphanRequest` |
| `EarlyNotify` | links and monitors fire when exit begins, before `stopped()` returns | `ExitAfterMessages` |
| `SplitDeathPoint` | registration checks a later "published" flag (E6 not atomic) | `NoEdgeToDead` |

Limits. The model abstracts handler bodies, time, the reply-slot machinery of
rank 11 (its own models cover it), supervision policy (covered by
`Supervision.tla`/`Cohorts.tla`) and the startup rules S1/S2, which change the
restart decision and the `start()` wait; those extend `Supervision.tla`'s
`Decide` and `SupStartDone` when implemented (see open question 7). Requests
sent in the window between mailbox closure and publication are not
distinguished from requests after death. Trace validation of real runs
against `Lifecycle.tla` needs the new lifecycle-trace events listed in the API
table; it is part of the implementation, as the verification rule requires.

## Differences from OTP that remain

| Difference | OTP | Spawned (proposed) | Category and reason |
|---|---|---|---|
| Stop and kill are cooperative | `kill` ends a process at once | a running handler, `stopped()` or blocking code is never interrupted | R: Rust cannot preempt or kill a thread |
| Administrative stop | `gen_server:stop` waits for the backlog (system message in order) | `ChildHandle::stop`/`stop_with_reason`/`shutdown` stop after the current handler and discard the queue; untrappable | B: stop latency is one handler, independent of an unbounded backlog; draining is opt-in (rank 13) |
| `stopped()` on untrapped exits | a non-trapping process killed by a signal, or any process killed by `kill`, skips `terminate` | open question 2 | depends on the decision |
| Exit reasons | any term (`{shutdown, Term}`, custom atoms) | a closed, non-exhaustive enum | T: typed reasons; `Panic(String)` carries free text |
| Signal sender from outside an actor | always a pid | `ActorId::EXTERNAL` for `ChildHandle::exit` called outside an actor | R: non-actor Rust code has no identity |
| `link` to a dead local process when not trapping | raises a `noproc` exception | open question 1 | T if a `Result`; none if an exit signal |
| `noconnection` | `Down`/`Exit` reason for a lost node | absent | single process (no distribution) |
| Committed versus published reason | identical | callers can see `Normal` while `Down` sees a destructor `Panic` | R: reply slots close before state that may hold tokens is dropped (rank 11) |
| Ready child crashing during the initial tree start (OneForAll/RestForOne) | its `'EXIT'` waits until `init` returns, then is a normal restart | the start fails | open question 7 |
| Runtime drop | no counterpart | `Killed` | R: an async runtime can drop a future |

Unchanged and already aligned: per-sender FIFO, at-most-once delivery, stop
discarding the mailbox, `Normal` link notices ignored by non-trapping peers,
transitive propagation, `Exit`/`Down` ordering, the restart table.

## Alternatives considered

1. **Redefine `ChildHandle::stop`/`stop_with_reason`/`shutdown` as exit
   signals** (the literal roadmap reading). Rejected: `stop()` on a
   non-trapping actor would become a silent no-op, and `shutdown()` on a
   trapping actor would become a request the actor may ignore. Both would
   compile unchanged. OTP keeps exit signals and `gen_server:stop` separate.
2. **Keep `Kill` for outside kills and only make it trappable over links**
   (today). Rejected: an observer cannot tell "the peer killed itself" from
   "someone killed the peer", which `killed` exists to distinguish.
3. **Retry initial startup only for some causes (startup timeout)**. Rejected
   for OTP alignment; offered as an opt-in only if open question 8 says so.
4. **A new `ExitReason::Dropped` for runtime drops.** Kept as open question 3.
5. **NoProc only for monitors, real reason for links.** Rejected: OTP gives
   `noproc` for both, and one death point serves both.
6. **Deliver a parent's Shutdown to `exit_received` and let the actor decide.**
   Rejected: a trapping actor that ignores it holds supervisor shutdown until
   the kill; gen_server never lets user code see the parent's exit. Control
   `ParentAsExit`.

## Open questions

1. A non-trapping actor calls `link` on an actor past its death point: should
   `link` return an error (`Result<(), NoProc>`, the counterpart of OTP's local
   `noproc` exception), or should the caller receive a `NoProc` exit signal and
   exit (OTP's documented signal semantics, what a remote link does)? The
   model allows both.
2. Should an actor run `stopped()` when it is killed, or when a non-trapping
   actor exits because of an exit signal or link notice? OTP skips
   `terminate` in both cases; v0.7 always runs `stopped()`.
3. Runtime-dropped actors: `Killed`, or a distinct `ExitReason` variant?
4. Error shape for callers: replace `ActorError::ActorStopped` by a variant
   carrying `ExitReason` (with `NoProc` for not-alive-at-send), or keep
   `ActorStopped` and add an accessor? Decide with rank 11, which already makes
   `ActorError` non-exhaustive. ethrex's deliberately exhaustive match breaks
   either way.
5. `ChildHandle::stop_with_reason(Kill)`: an alias of `kill()` (exits
   `Killed`), or an administrative stop with reason `Kill` as today?
6. Should `ChildHandle::shutdown()` stay an administrative stop, or become
   `exit(Shutdown)` (trappable), the signal a supervisor sends?
7. A child that became ready and then fails while later siblings are still
   starting (OneForAll/RestForOne): OTP handles its exit after `init` returns,
   as a normal restart; v0.7 fails the start. Align now or later?
8. Should there be an opt-in to retry a child's initial start (ethrex's
   discovery boot path relies on it implicitly), or is application-level retry
   enough?

## Review resolutions

Pending.
