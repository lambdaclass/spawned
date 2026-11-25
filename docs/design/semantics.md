# Spawned semantics

Status: describes `main` (v0.8.0 plus merged 0.9 work), 2026-11-23. This is the
contract: every guarantee Spawned gives, what checks it, and what it does not
promise. New designs are reviewed against it ([roadmap](../../ROADMAP.md) rank 5d).
Usage lives in the [guide](../GUIDE.md); the supervision design rationale in
[execution-groups-supervision.md](execution-groups-supervision.md).

How to read it:

- Each guarantee has an id (M1, S3, ...) and a **Checked by** line naming the
  TLA+ invariant (`Model!Invariant`, checked by TLC and proven inductive with
  Apalache unless marked TLC-only), Kani harness, test (`file::test` under
  `concurrency/tests/`, or `src/...` for unit tests) or example that checks it.
- **Not yet checked** means the code provides the behaviour by construction but
  no test, model or proof asserts it. Treat these as weaker than the rest.
- The [coverage matrix](../TESTING.md#semantics-coverage) lists, for each
  guarantee, the modes and placements its checks run on, whether they would
  fail if the guarantee broke, and the clauses still unchecked.
- **tasks** is `spawned_concurrency::tasks` (async, every `Backend` and execution
  group); **threads** is `spawned_concurrency::threads` (one OS thread per
  actor). Unmarked guarantees hold in both.

## 1. Model

**M1. One turn at a time.** An actor's state is owned by its run loop. Hooks
and handlers take `&mut self` and run one at a time; an async handler that
awaits keeps the turn, so no other message of that actor runs until it returns.
Concurrent request histories are therefore linearizable against the actor's
sequential behaviour.
Checked by: [`request_histories`][request_histories] (Porcupine-style checker
on four backends), [`delivery_simulation`][delivery_simulation],
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::hooks_and_turns_never_overlap_under_concurrent_requests_and_stop`
(a read-modify-write across an await, every placement),
[`sim_ordering::turns_and_hooks_never_overlap_{pct,dfs}`][sim_ordering].

**M2. One mailbox.** Each actor has one FIFO mailbox. Sends, requests, `Down`
and trapped `Exit` all go through it; there are no priorities and no selective
receive. tasks: Tokio unbounded mpsc; threads: `std::sync::mpsc` plus a private
stop wakeup that never runs a handler.
Checked by: code (`mailbox.rs`); [`cross_backend`][cross_backend],
[`semantics_gaps`][semantics_gaps]`::{native,tasks}::exit_and_down_follow_earlier_messages_from_the_source`
(`Down` and `Exit` queue behind ordinary messages).

**M3. Placement does not change semantics.** tasks `Async`, `Blocking`,
`Thread`, threads mode, and `async_group`, `thread_group`, `pinned_group`
produce the same reply values, per-sender order, exit reasons (including panic
text), `Down`/`Exit` counts, timer delivery sets, restart generations and
shutdown order. Timing, thread identity, id values and cross-sender
interleaving may differ.
Checked by: [`cross_backend`][cross_backend] (independent oracle per workload).

**M4. Fresh identity.** Every start, including every supervised restart, gets
a new `ActorId`, a new mailbox and new state. Ids are never reused in a process.
Checked by: [`supervision::one_for_one_uses_fresh_state_same_group_and_instance_bound_refs`][supervision],
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::actor_ids_are_never_reused` (every placement).

**M5. Actors are not reference counted.** Dropping every `ActorRef` does not
stop an actor; a stopped actor's state, thread or task, link and monitor
entries and owned registrations are released.
Checked by: [`leaks::unstopped_cycle_keeps_running_until_explicit_stop`][leaks],
`leaks::actor_reference_cycle_released_after_stop_on_every_backend`, LeakSanitizer job,
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::dropping_every_actor_ref_keeps_the_actor_running` (every placement).

## 2. Messages

**D1. Per-sender FIFO.** Messages sent sequentially by one sender (one task or
thread, through any handle of the same actor) are handled in send order.
Checked by: [`actor_scenarios::concurrent_producers_preserve_order_and_reply_identity`][actor_scenarios],
`actor_scenarios::generated_actor_scenarios` (request fences), [`cross_backend`][cross_backend],
[`sim_ordering::per_sender_fifo_and_reply_identity_{pct,dfs}`][sim_ordering] (ActorRef, Recipient and
mixed handles under PCT and bounded DFS).

**D2. At most once.** An accepted message is handled at most once and never
duplicated or retried. While the actor keeps running it is handled exactly once.
Checked by: [`actor_scenarios`][actor_scenarios], [`payload_stress`][payload_stress]
(FIFO delivery or complete destruction), [`request_histories`][request_histories].

**D3. Accepted is not handled.** `send` returns `Err(NoProc)` only once the
actor is past its death point (the mailbox refuses admission); the rejected
message is dropped. `Ok(())` means accepted: between a stop request and the
death point (including during `stopped()`), sends still succeed and the
message is then discarded (a request is answered `ActorStopped` with the
final reason).
Checked by: [`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::stop_window_accepts_then_discards_and_releases_before_join`
(sends after the stop request and from `stopped()` are `Ok` and never handled, every placement),
[`mailbox_admission_loom`][mailbox_admission_loom] (threads send gate: publish before drain or fail),
`src/mailbox.rs::accepted_sends_are_drained_even_when_senders_outlive_receiver`,
[`actor_scenarios`][actor_scenarios] ("accepted sends alone do not
guarantee execution across termination"), `death_boundary` (a request
accepted before the death point is answered at it).

**D4. Stop discards the queue.** The run loop checks for stop after receiving
and before dispatch admission. If that check observes stop, the received item
is retained for death-point cleanup rather than dropped or handled, and no
further message is dispatched, including messages already queued. A message
admitted before a concurrent stop may still run to completion, even if its
handler begins after `stop()` returns: `stop()` requests termination and does
not wait for the current turn (X4). A handler stopping its own actor prevents
the next dispatch. Queued messages are dropped one by one and every
queued request is answered (callers get `ActorStopped` with the final reason)
before the exit is published.
Checked by: `src/tasks/actor/stop_receive_probe.rs` (untraced stop/receive gap,
caller-before-payload ordering and runtime drop), `stop_token_loom` (queue
publication versus post-receive admission),
`src/tasks/actor/idle_stop_regression.rs::stop_while_idle_does_not_dispatch_a_queued_request_on_resume`,
`src/threads/actor/stop_wakeup_regression.rs::repeated_stop_during_a_handler_discards_queued_requests`,
`src/threads/actor/idle_stop_regression.rs::stop_while_receiving_does_not_dispatch_a_later_request`,
[`shutdown_publication`][shutdown_publication], [`mailbox_drop_panic`][mailbox_drop_panic],
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::stop_window_accepts_then_discards_and_releases_before_join`
(payloads dropped and reply senders closed when `join()` returns, every placement),
[`sim_ordering::stop_discards_queue_before_publishing_exit_{pct,dfs}`][sim_ordering].

**D5. Mailboxes are unbounded.** `send` never blocks and never fails for
capacity; nothing bounds queue memory.
Checked by: [`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::unbounded_mailbox_accepts_a_large_backlog_without_blocking`
(100,000 sends behind a blocked handler, every placement). Bounds are planned (rank 17).

## 3. Requests

**R1. One terminal outcome.** Every request has a reply slot
([deferred replies](deferred-replies.md) §1) that is discharged exactly once.
`request`, `request_with_timeout`, protocol calls and
`spawned::tasks::request` end in exactly one of `Ok(reply)`,
`Err(ActorStopped(reason))` (the handler panicked, the request was still
queued at the owner's death, or a deferred token was open at death; always
the owner's final exit reason, the one `wait_exit` and `Down` report),
`Err(NoProc)` (sent after the owner's death point),
`Err(RequestTimeout)` (the deadline fixed at send passed),
`Err(ReplyDropped)` (the handler dropped the reply obligation unanswered) or
`Err(Overloaded)` (`defer()` at `MAX_OUTSTANDING`, or the handler answered
`overloaded()`). A request racing shutdown gets its reply, `ActorStopped` or
`NoProc`; a slow teardown can still produce a timeout first. `ActorStopped` is
reported only at the owner's actual death, never while `stopped()` still
runs (otp-exit-signals.md, "Exit errors wait for D").
Checked by: [`actor_scenarios`][actor_scenarios], `src/tasks/actor/tests.rs::request_timeout`,
[`timeout_outcomes`][ex_timeout_outcomes] example.

**R2. Reply identity.** Each request has its own reply channel; a reply reaches
only the caller that sent that request.
Checked by: [`actor_scenarios::concurrent_producers_preserve_order_and_reply_identity`][actor_scenarios],
[`sim_ordering::per_sender_fifo_and_reply_identity_{pct,dfs}`][sim_ordering].

**R3. Timeouts.** Default 5 s, as `gen_server:call/2`; `send_request(msg,
deadline)` takes its deadline explicitly (`Deadline::Never` waits without
one).
Precedence: `with_request_timeout` handle, `#[request_timeout]` method
attribute, `#[protocol(timeout)]`, 5 s. Send-only methods have no timeout.
The deadline is fixed when the request is sent (send time plus the timeout)
and travels in the reply slot: there is one deadline, not a caller timer plus
a server one. tasks: `request`-family futures enqueue on first poll and
protocol methods enqueue when called; either way the deadline starts at the
enqueue. threads: the call blocks. Expired requests are still dispatched (OTP).
The caller's expiry is the only timeout authority, as in OTP's
`gen_server:call`: a reply reads no clock. A reply that loses the race with
the expiry (the caller's timer, the owner's deadline heap, or a
`[deadline]` check) is discarded (`Delivery::Expired`) and the caller sees
`RequestTimeout`; a reply that lands after the deadline instant but before
the expiry runs is delivered (`Delivery::Delivered`, and the caller gets
the value), as an OTP reply already in the caller's mailbox when its
`after` clause would fire is received. Timer resolution and when the runtime
or deadline service can process expiry determine that window; this is not a
hard real-time bound. The deadline service hands the wakers and payload
destructors an expiry releases to disposal workers, one item at a time, so
while fewer than 64 such callbacks are blocked at once, a blocking
destructor or waker (of another actor, or of another caller of the same
actor) does not delay an expiry, its deferral-credit return or its caller's
wake-up ([`deadline_service`][deadline_service]). What still runs on the
service thread is listed in deferred-replies.md §2.
Which clock expires a waiting caller is part of the contract: `request`,
`request_with_timeout` and `Response::from(rx)` arm the runtime's timer, so
they follow paused Tokio time; a `ReplyReceiver` awaited directly
(`send_request(..)?.await`) puts its deadline in the owner's deadline heap
on its first pending poll instead, serviced on the real clock (the
simulated one under `spawned_sim`), not paused Tokio time. (The slot finds
its owner by a registry key in its word, so it holds no reference to it,
and a request answered before that poll never touches the heap.) The reason is cost: a raw receiver is the building
block for fan-out and needs no runtime timer per request. Tests that pause
Tokio time wait through `request` or `Response::from`. Either way a
shortened deadline (`ReplyTo::shorten_deadline`) wakes the waiting caller.
Checked by: [`protocol_timeouts`][protocol_timeouts],
[`cancellation_safety::protocol_calls_are_cancel_safe_and_send_eagerly`][cancellation_safety],
[`protocol_timeouts`][ex_protocol_timeouts] example (both modes),
[`semantics_gaps`][semantics_gaps]`::tasks_contract::default_request_timeout_is_five_seconds_from_first_poll`
(4.9 s succeeds, 5.1 s times out, no timer before the first poll).

**R4. Giving up never cancels.** A caller timeout or a dropped caller future
or `Response` does not remove the request from the mailbox, cancel its handler
or roll back effects. The caller cannot tell whether it ran. One exception:
a status request (`get_state`, `get_status`) whose caller gave up before its
turn began is not run (I4).
Checked by: [`cancellation_safety::request_family_is_cancel_safe`][cancellation_safety],
[`request_histories`][request_histories] (timed-out operations are "pending").

**R5. Handler panic.** A panic while handling a request drops its reply
obligation in the panicking turn: at the turn's end it becomes exit pending
(a turn that returns without answering gives `ReplyDropped`), the actor exits
and the request is answered at its death point with the final reason,
`ActorStopped(Panic(..))` (or `ActorStopped(Killed)` if a kill arrived
before death), or by the caller's deadline first.
Checked by: [`deferred_replies`][deferred_replies]
`::{tasks_mode::panicking_turn_answers_actor_stopped, threads_mode::panic_and_death_answer_actor_stopped}`,
[`semantics_gaps`][semantics_gaps]`::{native,tasks}::panicking_request_handler_gives_the_caller_actor_stopped`
(exactly `ActorStopped(Panic(_))`, every backend, a managed group,
`Recipient` and raw receivers), [`otp_exit_signals`][otp_exit_signals]`::*::caller_errors`
(answered only at death), [`panic_safety`][panic_safety].

**R6. Deferred replies.** A handler may answer later: `PendingReply::defer()`
takes one of the instance's `MAX_OUTSTANDING` credits (or answers
`Overloaded` at once) and returns a storable `ReplyTo`. The token is answered
from a later turn, a job or `stopped()`; dropping it answers `ReplyDropped`
between turns, is committed at the turn's end inside a turn, and while the
owner exits is held until its death point. At the owner's death every open
token becomes `ActorStopped` with the final reason, wherever it is held; a
reply made once `NoProc` is observable for the owner is refused
(`Delivery::OwnerExited`) and the caller gets that reason. Queued requests
and deferred tokens are answered at the death point before any queued
payload or state destructor runs, also when the runtime drops the actor
(G1). The owner's deadline heap expires
deferred tokens (returning their credits) even when no caller polls. Callers
and protocols cannot tell a deferred reply from an immediate one.
Checked by: `reply_slot_loom` (every concurrent pair of the slot word),
[`deferred_replies`][deferred_replies], `reply::registry::tests`,
`death_boundary` (blocking destructors; both modes; runtime drop),
`failpoints::no_reply_succeeds_after_noproc_is_observable`.

**R7. Pipe-to-self jobs.** `ctx.pipe*` runs work owned by the instance and
delivers `done(outcome)` as one message in a later turn; a panic arrives as
`JobError::Panicked`, a job deadline as `JobError::DeadlineExceeded` while the
work keeps its credit. Messages of cancelled or replaced (keyed) jobs are
fenced at dispatch; a job's emits stay FIFO with the terminal message last.
Owner exit cancels every job; they are joined after death, bounded by the
cleanup deadline, and reported as `unfinished_at_stop`.
`JobCtx::emit_at(at, ..)` with `at` past `JobOptions::deadline` is accepted:
the hard deadline takes the terminal notification when it passes and fences
everything after it, so the emit is dropped at its instant (its credit
returns then). It is not an error at the call: whether the deadline wins is
decided when it passes (the job may finish first, delivering the emit and
then its result), and a job checks one rule, the deadline, rather than two.
An `at` already reached is an immediate emit.
Checked by: [`deferred_jobs`][deferred_jobs].

## 4. Lifecycle

**L1. Hook order.** `started()` runs once before any handler; `stopped()` runs
at most once, after the last handler, and only after a ready startup
commitment: never after a failed or ignored start, including a `ctx.stop*`
called in `started()` (OTP skips `terminate/2` when `init/1` fails; v0.9).
No handler runs after `stopped()` begins.
Checked by: [`sim_ordering::turns_and_hooks_never_overlap_{pct,dfs}`][sim_ordering],
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::hooks_and_turns_never_overlap_under_concurrent_requests_and_stop`
(every placement), [`cross_backend::stop_before_first_poll_follows_managed_and_legacy_contract`][cross_backend],
the fuzz M1/L1/L5 oracle;
[`start_contract`][start_contract]`::{tasks,threads}_mode::init_outcomes_answer_after_publication_without_stopped`,
`src/{tasks,threads}/actor/early_stop_regression.rs::stop_in_started_fails_the_start_without_stopped`.

**L2. Readiness.** Every instance, on every placement and verb, has a
startup result: the startup cell's `Ready`, committed by the child at the end
of `started()` while its admission is open, before any continuation or
handler. It is final: a later death is an ordinary death. `ready()` and the
waiting starts read the same committed value; it is a historical barrier,
not current liveness. If a managed actor's admission closes before
`started()` begins (group or system shutdown), neither hook runs and the
state is dropped. An administrative stop that arrives before or during
`started()` waits for it, on every placement (OTP `gen_server:stop`, a
system message handled after `init/1`): the start commits ready, then the
actor exits with the stop's reason through `stopped()` (v0.9, L9).
Checked by: [`startup_admission`][startup_admission],
[`cross_backend::stop_before_first_poll_follows_managed_and_legacy_contract`][cross_backend],
[`execution_groups::failed_startup_discards_queued_payloads_and_reply_channels_without_dispatch`][execution_groups],
[`semantics_gaps`][semantics_gaps]`::tasks_contract::managed_readiness_gates_handlers_and_stays_historical`
(both group kinds).

**L3. Stop is cooperative.** `ctx.stop()`, `ctx.stop_with_reason(r)`,
`ChildHandle::stop()` and `stop_with_reason(r)` are self-exits
or administrative stops: they ignore `trap_exit`, the running handler
finishes, `stopped()` runs, then the actor exits. Nothing preempts a running
handler or blocking code, a kill included. The first stop's reason wins over
later stops; until the death point D a kill makes it `Killed`, and a panic or
an untrapped exit signal or link notice replaces it unless a kill was received
(OTP: a crash or signal during `terminate` replaces the reason). After D the
reason is final.
Checked by: `Lifecycle!KilledFromKill`, `Lifecycle!LateSignalWins` (TLC);
[`context_exit`][context_exit] (both modes), `src/link.rs::tests::reason_precedence_until_death`,
`src/link_regression_tests.rs::late_link_failure_relabels_an_already_requested_normal_stop`,
[`otp_exit_signals`][otp_exit_signals]`::tasks_mode::{late_signal_replaces_the_requested_reason, kill_during_stopped_supersedes_and_panic_does_not_replace_it}`;
[`exit_signals_sim`][exit_signals_sim]`::{kill_races_stopped, late_signal_races_the_death_point}`;
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_partial::stop_reason_precedence_through_cleanup`
(every requested reason, from `ctx` and from a `ChildHandle`, on every
placement: a second request, a panic in `stopped()`, a kill then a panic in
`stopped()`, a state destructor panic after D).

**L4. Exit reasons.** Every actor publishes exactly one `ExitReason`
(`#[non_exhaustive]`):

| Reason | Arises from |
|---|---|
| `Normal` | `ctx.stop()`, `ChildHandle::stop()`, a parent's `Normal` exit to a trapping child |
| `Shutdown(None)` | `stop_with_reason(Shutdown(None))`, supervisor or system shutdown, an exit signal or link notice with `Shutdown(None)` (non-trapping) |
| `Shutdown(Some(why))` | OTP `{shutdown, Term}` (v0.9, G7): `stop_with_reason(ExitReason::shutdown_with(why))`, an exit signal or link notice with it (non-trapping) |
| `Panic(msg)` | a panic in `started()` (`"panic in started(): .."`), a handler, `exit_received` or `Down` handler (`"panic in handler: .."`), `stopped()` (`"panic in stopped(): .."`); an exit signal or link notice with `Panic` |
| `Kill` | `stop_with_reason(Kill)` (a stop, not a kill), a `Kill` link notice (non-trapping) |
| `Killed` | `ChildHandle::kill()`, `exit(Kill)`, a supervisor's or system's kill escalation, a `Killed` link notice; the runtime dropping the actor unless a non-`Normal` reason was requested |
| `NoProc` | a `link` to an actor past its death point (non-trapping caller), a `NoProc` signal or notice |

`is_abnormal()` is true for `Panic`, `Kill`, `Killed` and `NoProc`.
Destructor panics after the death point do not change the reason (L6).

The `Shutdown` payload (an `Arc<str>`) only explains the stop, as OTP's
`{shutdown, Term}`: every rule that names `Shutdown` in this contract (the
restart table V1, link and signal handling X3, the metrics class, the
trace and model class `Shutdown`) applies to `Shutdown(_)`, with or without a
payload. `Down`, `Exit`, `join()`/`wait_exit`, `ActorStopped` and
`ChildSnapshot::last_exit` carry the full reason; a non-trapping linked actor
ends with the same full reason. A supervisor's or system's own shutdown sends
`Shutdown(None)`.
Checked by: [`exit_signal_matrix`][exit_signal_matrix], [`context_exit`][context_exit],
[`otp_exit_signals`][otp_exit_signals], [`panic_safety`][panic_safety],
`src/link_regression_tests.rs::never_polled_actor_propagates_runtime_shutdown_to_peer`,
[`exit_reason`][ex_exit_reason], [`native_lifecycle`][ex_native_lifecycle] and
[`exit_signals`][ex_exit_signals] examples; the payload form:
[`shutdown_payload`][shutdown_payload] (both modes: `join`, `Down`, trapped
`Exit`, an untrapped linked peer, exit signals, metrics),
`src/error.rs::tests::{shutdown_payload_helpers, exit_reason_is_abnormal_classification, exit_reason_size_is_unchanged}`.

**L5. Panics in hooks.** A panic in `started()` stops the actor without calling
`stopped()` and without handling any message. A panic in a handler loses that
message, then `stopped()` runs. A panic in `stopped()` is caught; `join()`
returns and the reason becomes `Panic`, replacing an earlier requested reason
unless a kill was received (then `Killed` stands). `stopped()` does not run
after a kill, an untrapped exit signal or link notice, or a runtime drop
(see X3, X4); one that already began finishes.
Checked by: `src/{tasks,threads}/actor/tests.rs::panic_in_started_stops_actor`,
`panic_in_handler_stops_actor`, `panic_in_stopped_still_completes`,
`stop_in_started_fails_the_start_without_stopped`; [`panic_safety`][panic_safety],
[`foreign_panics::panicking_subscriber_keeps_handler_reason_and_runs_stopped`][foreign_panics],
[`semantics_gaps`][semantics_gaps]`::tasks_contract::panics_in_hooks_follow_the_contract_on_every_placement`
(reason prefix, handled count and `stopped()` calls);
`Lifecycle!SkipHook` (TLC);
[`otp_exit_signals`][otp_exit_signals]`::tasks_mode::{kill_during_stopped_supersedes_and_panic_does_not_replace_it, stopped_skipped_log::skipped_stopped_is_logged_at_debug}`;
[`exit_signals_sim::kill_races_stopped`][exit_signals_sim]; lifecycle traces
`kill_during_stopped`, `untrapped_signal_skips_stopped` ([`lifecycle_trace`][lifecycle_trace]).

**L6. Destructor panics are secondary.** The actor state and queued
messages are dropped after the death point, so a panicking `Drop` does not
change the exit reason; it is recorded in `ChildHandle::cleanup_failure()`
(`mailbox_cleanup_failure()` is a deprecated alias), and a Transient child
restarts on it. A panic payload whose own destructor panics is forgotten
(`dispose_panic`) to stop the cascade; this holds for every caught panic
(`started()`, handlers, `stopped()`, helpers, factories, native group roots),
so such a payload never skips `stopped()` or changes the reason.
Checked by: [`mailbox_drop_panic`][mailbox_drop_panic] (5 placements x 1/2/256
destructors), [`panic_payload_drop`][panic_payload_drop] (each hook, both
modes, a managed actor), [`panic_safety`][panic_safety], [`native_mailbox_drop`][native_mailbox_drop],
`queued_drop_panic`/`queued_drop_shutdown_restarts` lifecycle traces.

**L7. Exit publication is last.** The exit reason is published after
`stopped()`, after queued messages and reply senders are released, after links
have been signalled and outgoing monitors cancelled. `join()`, `wait_exit()`
and `ChildHandle` waits return only then; `send`/`request` return `NoProc`
from the death point on, which comes before publication.
Checked by: [`shutdown_publication`][shutdown_publication],
`src/link_regression_tests.rs::never_polled_completion_waits_for_actor_payload_destruction`,
[`actor_scenarios`][actor_scenarios] (post-exit requests),
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::stop_window_accepts_then_discards_and_releases_before_join`,
[`sim_ordering::stop_discards_queue_before_publishing_exit_{pct,dfs}`][sim_ordering];
outgoing monitors cancelled before publication:
`src/tasks/actor/monitor_cleanup_regressions.rs::observer_cancels_its_monitors_before_publishing_its_exit`
(the cleanup is shared with threads mode); the fuzz L7 probe also checks that an
exited observer's waiters are released while the target lives on.

**L8. Start (v0.9).** Stage 1 runs in the start call: the options are
validated and the instance prepared privately, then one commit, under the lock
order group admission, the parent's link or monitor table, the parent's exit
request, the registry, applies the group slot, the parent's edge and the name,
all or nothing. A refusal (`Closed`, `Capacity`, `AlreadyStarted`,
`ParentExited`, `InvalidOptions`) is never observable: no hook runs, no edge,
name or slot remains. `spawn` does not wait; `start`, `start_link` and
`start_monitor` answer `Started` at once on a ready commitment, and an
`Ignored` or error answer only after the instance's publication (its state
dropped, its notices disposed of, its name free) with the final reason, or
after its cleanup deadline with `pending: Some(handle)`. Dropping a `Starting`
abandons the wait, nothing else.
Checked by: `Start!AbortInvisible`, `Start!ResultMatchesCommit`,
`Start!ReadyIsFinal`, `Start!FailureAfterPublication`,
`Start!PendingOnlyAfterDeadline`, `Start!CommitImmutable` (TLC, 8
configurations, 40 negative controls); start traces
([`start_trace`][start_trace], `TraceStart.tla`, 15 recorded-trace negative
controls); [`start_contract`][start_contract]`::{tasks,threads}_mode::{init_outcomes_answer_after_publication_without_stopped, names_and_refusals}`,
`tasks_mode::{start_link_through_dead_context_is_refused, ready_then_immediate_crash_with_unpolled_waiter_returns_started_and_delivers_down, stuck_started_reports_pending_after_the_timeout}`;
[`link_fence_loom::start_edge_commit_races_parent_exit_begin`][link_fence_loom].

**L9. Init resolution (v0.9).** When `started()` returns, the first
matching row decides the start and the requested reason, under the child's
exit-request and cell locks: (1) the init timer committed: `Killed`,
`InitTimeout`; (2) a kill arrived: `Killed`; (3) a
held non-`Normal` signal to a non-trapping child, the first in arrival order;
(4) `started()` panicked; (5) `ctx.stop*` in `started()`; (6) `Init::stop(r)`;
(7) `Err(e)`/`Init::error(e)`: `Normal`, `InitFailed`; (8) `Init::ignore()`:
`Normal`, `Ignored`; (9) ready, but admission closed: `Shutdown`; (10) ready.
A non-ready decision fixes the reason for good: a later kill does nothing.
Held signals of a ready child resolve as in phase 0 (E2, E3). No failed row
runs `stopped()`.
Checked by: `Start!KillBeforeDIsKilled`, `Start!IgnoredExitsNormal`, `Start!ReadyReturnCommitsReady` (control `AdminStopFailsInit`; TLC);
start traces `kill_and_admin_stop_during_started`,
`parent_exit_during_child_started`, `link_stop_in_started_ends_nontrapping_parent`;
[`start_contract`][start_contract]`::{tasks,threads}_mode::init_outcomes_answer_after_publication_without_stopped`,
`tasks_mode::trapping_child_with_parent_exit_in_started_commits_ready_then_ends`.

**L10. Continuation (v0.9).** `Init::continue_with(m)` handles `m` as the
first turn after the readiness commit, before every mailbox entry (messages
sent during `started()`, signals queued for a trapping child). It is not a
barrier against termination; a stop observed before it begins discards it.
Checked by: `Start!ContinueFirst` (TLC); start trace
`continuation_precedes_queued_messages` and its negative control;
[`start_contract`][start_contract]`::{tasks_mode::continuation_runs_before_messages_sent_during_started, threads_mode::continuation_runs_first}`.

**L11. A pending exit is observable.** `ctx.exit_pending()` is `true` once
the actor's exit has begun (`ctx.stop()`, an administrative stop, a kill, an
untrapped exit signal or link notice, a panic) or a parent exit is queued
for a trapping actor (V9); a trapped `Exit` does not make it `true`. Once
`true` it stays `true`. In tasks mode `ctx.stopping()` is its awaitable form:
it resolves exactly when `exit_pending()` becomes `true` (never before), is
ready at once when it already is, and stays ready after the death point.
Neither preempts the running handler (L3). An actor that never calls
`stopping()` has no waiter and nothing per message.
Checked by: `src/link.rs::tests::{exit_pending_counts_begun_exits_and_queued_parent_exits, stopping_waiter_wakes_on_a_queued_parent_exit}`,
[`otp_exit_signals`][otp_exit_signals]`::tasks_mode::{exit_pending::queued_work_sees_the_supervisors_pending_shutdown, stopping::wakes_a_waiting_handler_for_every_begun_exit, stopping::a_trapped_exit_does_not_wake_it_a_parent_exit_does, stopping::ready_once_pending_and_after_death, stopping::a_spawned_waiter_wakes_on_stop}`;
[`exit_signals_sim::stopping_wakes_for_a_racing_trigger`][exit_signals_sim]
(a stop, a kill or an untrapped signal racing the waiting turn).

## 5. Links, monitors and trapped exits

**X1. Down exactly once.** `ctx.monitor(&handle)` (requires `Handler<Down>`)
yields exactly one `Down { monitor_ref, actor, reason }` per monitor that is not
cancelled: the target's final reason if registered before its death point D,
`NoProc` (at once) if after it. Each call returns a distinct `MonitorRef`; the
target is unaffected.
Checked by: `Lifecycle!DownAtMostOnce`, `Lifecycle!NoProcIffLate`, `Lifecycle!DownDelivered` (TLC);
[`link_fence_loom`][link_fence_loom]`::monitor_racing_death_gets_exactly_one_down`;
[`exit_signals_sim::link_and_monitor_race_the_death_point`][exit_signals_sim];
[`actor_scenarios::demonitor_before_exit_suppresses_notification_and_races_do_not_duplicate`][actor_scenarios],
[`exit_signal_matrix`][exit_signal_matrix], `src/*/actor/tests.rs::monitor_*`,
`multiple_monitors_each_get_own_ref_and_down`,
[`otp_exit_signals::tasks_mode::registrations_after_death_report_noproc`][otp_exit_signals],
[`cancellation_safety::monitor_and_link_setup_requests_are_cancel_safe`][cancellation_safety].

**X2. Demonitor and late registration.** `demonitor` flushes, as OTP's
`demonitor(Ref, [flush])`: once it returns, in both modes, no `Down` of that
monitor is enqueued or handled, including one already in the mailbox (the
late `NoProc` included); a `Down` whose dispatch began before it returned
was already received and is handled. A `Down` that user code sends itself is
an ordinary message. A monitor
registered by an actor that is already stopping is inert (no waiter, no `Down`).
Likewise `ctx.link`/`ctx.try_link` called through a `Context` whose actor's
exit already began, or which is past its death point (for example from a
helper that outlives the actor), is inert: no record, no notice, no `NoProc`,
and `try_link` returns `Ok(())` (a dead process cannot call `link` in OTP;
design otp-exit-signals.md, "Sender lifetime").
Checked by: `Lifecycle!CancelFenced` (TLC, `MCLifecycleFence`; the
`NoDemonitorFlush` control runs a flushed `Down`);
`src/*/actor/tests.rs::{demonitor_before_target_dies_suppresses_down, demonitor_flushes_a_queued_down}`,
`src/monitor.rs::tests::demonitor_flushes_a_queued_down`;
[`link_fence_loom`][link_fence_loom]`::demonitor_flushes_a_down_in_dispatch`
(a dispatch racing `demonitor` on another thread);
[`failpoints`][failpoints]`::{demonitor_flushes_a_down_in_dispatch, native::demonitor_flushes_a_down_in_dispatch}`
(site `monitor.down_before_dispatch`: `demonitor` returns while the `Down`
is in dispatch; the window is cross-thread, so a failpoint, not the
simulator, reaches it);
`link::tests::link_from_an_exiting_or_dead_caller_is_inert`,
[`otp_exit_signals::tasks_mode::link_from_a_dead_actor_is_inert`][otp_exit_signals],
`closed_monitor_table_rejects_late_registration`, [`leaks::monitor_waiters_released_on_demonitor_owner_exit_and_target_exit`][leaks],
the fuzz X2 (flush) oracle (no `Down` handled after the driver's `demonitor`
returned).

**X3. Exit signals and link propagation.** An exit signal
(`ChildHandle::exit`, `ctx.exit`, `kill`) or a linked actor's death notice is
applied at the receiver by OTP's `exit_signal/2` rules, with the trap flag
read when it arrives:

| Signal or notice | Non-trapping receiver | Trapping receiver (`ctx.trap_exit(true)`) |
|---|---|---|
| `Kill` signal (`kill()`, `exit(Kill)`) | ends `Killed` | ends `Killed` |
| `Normal` | nothing | `exit_received(Exit { from, reason: Normal })` |
| any other reason (a `Kill` notice included) | ends with that reason after its current handler, without `stopped()`, queue discarded; propagates further | `exit_received(Exit { from, reason })` |
| from the receiver's parent (`start_link` caller, supervisor, system) | as above | a parent exit is queued: when dispatched the actor exits with that reason through `stopped()`; `exit_received` never sees it |

Propagation is transitive through non-trapping actors. Links work across
modes. Signals arriving during `started()` take effect when it returns, with
the trap flag read then. `ctx.exit` works until the sender's death point,
also from a cloned context on another thread: the death point waits for a
signal in flight, so a signal sent before it reaches its target before the
sender's death notice (`Lifecycle!ExitAfterMessages`); one sent after it is
inert.
Checked by: `Lifecycle!TrapRespected`, `Lifecycle!NormalSignalIgnored`, `Lifecycle!KilledFromKill`,
`Lifecycle!SkipHook`, `Lifecycle!LinkedPeersStop`, `Lifecycle!ShutdownEndsChild` (TLC);
[`otp_exit_signals`][otp_exit_signals]`::tasks_mode::{exit_signals_follow_exit_signal_2, kill_is_untrappable_and_propagates_killed, ctx_exit_sends_from_the_calling_actor, parent_exit_ends_a_trapping_child_normally, signals_during_started_are_resolved_with_the_flag_set_in_started}`,
`threads_mode::kill_exit_and_noproc_in_threads_mode`;
`failpoints::an_exit_signal_is_not_overtaken_by_its_senders_death_notice`;
`src/link.rs::tests::signal_rules_follow_exit_signal_2`;
`src/tasks/actor/tests.rs::link_propagates_panic_to_non_trapping_peer`,
`link_delivers_exit_to_trapping_peer`, `link_normal_exit_not_propagated_to_non_trapping_peer`,
`chain_propagation_through_non_trapping_middle` (and threads equivalents),
[`exit_signal_matrix`][exit_signal_matrix], [`linked_exit_macro`][linked_exit_macro],
`src/link_regression_tests.rs::linked_runtime_abort_cancels_an_idle_threads_actor` (cross-mode);
lifecycle traces ([`lifecycle_trace`][lifecycle_trace]); [`exit_signals`][ex_exit_signals] example.

**X4. Kills are untrappable; administrative stops are not signals.** Only a
`Kill` *signal* is untrappable and produces `Killed`. A `Kill` or `Killed`
arriving as a link notice is trappable, so one kill cannot take down a graph
of trapping observers (OTP's `kill`/`killed` split). `stop`, `shutdown` and
`stop_with_reason(any)` are administrative stops (as `gen_server:stop`):
untrappable, `stopped()` runs, and `stop_with_reason(Kill)` ends `Kill`.
Checked by: [`kill_links`][kill_links] (both modes),
[`otp_exit_signals`][otp_exit_signals]`::tasks_mode::{kill_is_untrappable_and_propagates_killed, administrative_kill_reason_runs_stopped_and_ends_kill}`;
`Lifecycle!KilledFromKill` (TLC).

**X5. Link registration, NoProc and fences.** A link records both ends in
one transaction. Linking to an actor past its death point D keeps no
relationship and delivers one `NoProc` notice: a trapping caller gets
`Exit { reason: NoProc }`, a non-trapping caller ends `NoProc`;
`ctx.try_link` returns `Err(NoProc)` and delivers nothing. A link racing D
gets exactly one notice, the real reason or `NoProc`. Repeating a link
(including one whose notice is pending) and linking to oneself are no-ops.
`unlink` and `demonitor` are fences: once they return, no notice of that edge
takes effect; an `Exit` already queued stays, a `Down` already queued is
flushed (X2). Each edge has a generation, so a stale
notice never applies to a later link. `start_link` and `start_monitor`
commit their edge before the child's `started()`, on every placement, in the
start's commit; a start through a parent whose exit began is refused
(`ParentExited`, v0.9). A waiting parent whose exit a kill or an untrapped
signal begins abandons its pending start waits (`ParentExited`), so it does
not wait for the child's `started()` (OTP: the caller dies at once;
`parent_killed_while_awaiting_start_link_abandons_the_wait`, both modes,
recorded and validated as a `Start` trace).
Checked by: `Lifecycle!NoProcIffLate`, `Lifecycle!NoEdgeToDead`, `Lifecycle!CancelFenced` (TLC, `MCLifecycleFence`);
[`link_fence_loom`][link_fence_loom] (the production link table and monitor
fence under Loom: `unlink_fences_a_death_notice`, `link_racing_death_gets_exactly_one_notice`,
`relink_after_unlink_ignores_the_stale_generation`, `death_racing_unlink_and_a_new_link`,
`mutual_death_delivers_at_most_one_notice`, `concurrent_links_install_one_edge`,
`demonitor_fences_the_down`, `observer_close_fences_the_down`);
[`exit_signals_sim`][exit_signals_sim]`::{link_and_monitor_race_the_death_point, unlink_and_demonitor_fence_delivery}`;
`src/link.rs::tests::{closure_is_terminal_and_delivers_each_edge_once, try_link_to_a_dead_actor_keeps_no_record, unlink_fences_a_notice_taken_at_death}`;
[`otp_exit_signals::tasks_mode::registrations_after_death_report_noproc`][otp_exit_signals];
`src/tasks/actor/tests.rs::duplicate_link_is_idempotent`, `unlink_prevents_signal_delivery`;
`src/native_link_regression_tests.rs::start_linked_registers_before_threads_started`;
`Start!LinkBeforeStarted`, `Start!EdgeOnlyToRunningParent` (TLC) and the start
trace control "edge committed after started() began";
[`link_fence_loom::start_edge_commit_races_parent_exit_begin`][link_fence_loom];
lifecycle traces `late_registrations_get_noproc`, `unlink_fences_a_notice_taken_at_death`,
`demonitor_fences_a_down`.

**X8. Startup notices (v0.9).** A start edge's notice is disposed of at the
child's publication by its startup commitment and its waiter: after a ready
commitment, or for an abandoned or absent waiter, it is ordinary; while the
waiter waits on a failed or ignored start, a trapping parent's `Exit` and a
monitor's `Down` are diverted to it (its error consumes them), a
non-trapping parent is still ended by a non-`Normal` reason (a `Normal` one
is dropped); a timed-out start's notice is diverted for both trap settings,
so an init timeout never reaches a waiting parent. An abandoned wait hands a
diverted notice on exactly once. No notice is lost or delivered twice.
Checked by: `Start!FailureFlushed`, `Start!NoticeOnce`, `Start!NoLostNotice`,
`Start!TimeoutContained`, `Start!AbandonedNoticeDelivered`,
`Start!FailureEndsNonTrappingParent`, `Start!NormalFailureSparesParent`
(TLC); start traces `monitor_error_consumes_down`,
`link_panic_in_started_trapping_parent`,
`abandon_after_publication_hands_on_exit`, `abandoned_timeout_delivers_killed`,
`init_timeout_waiting` and their negative controls;
[`start_contract`][start_contract]`::{tasks,threads}_mode::failed_start_notices_follow_the_disposition_table`,
`tasks_mode::{abandoned_start_monitor_still_delivers_down, init_timeout_never_reaches_the_parent}`.

**X6. Runtime teardown still notifies.** An actor whose future or thread is
dropped by its runtime, polled or never polled, ends `Killed` in both modes
(unless a non-`Normal` reason was requested), without `stopped()`, and still
signals its links (trappable) and monitors.
Checked by: `Lifecycle!RuntimeDropReason` (TLC);
`src/link_regression_tests.rs::never_polled_actor_propagates_runtime_shutdown_to_peer`,
`polled_actor_propagates_runtime_shutdown_to_peer`, `never_polled_actor_notifies_monitor_on_runtime_shutdown`,
`linked_runtime_abort_is_trappable`; lifecycle trace `runtime_drop_ends_killed`.

**X7. Ordering.** Trapped `Exit` and `Down` are ordinary mailbox messages:
they queue behind messages already in the mailbox and never interrupt a running
handler. A non-trapping actor's linked stop takes effect after its current
handler and discards its queue (D4). `Exit`/`Down` are enqueued only after the
source has exited, so they follow every message the source sent earlier to the
same observer.
Checked by: [`semantics_gaps`][semantics_gaps]`::{native,tasks}::exit_and_down_follow_earlier_messages_from_the_source`
(every reason including a real panic, all three tasks backends),
[`sim_ordering::exit_and_down_follow_the_last_message_{pct,dfs}`][sim_ordering] (a stop racing the
source's sends, noise at the observer, PCT and bounded DFS), the fuzz X7 oracle,
[`exit_signal_matrix`][exit_signal_matrix] (delivery);
the non-trapping linked stop:
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_partial::linked_stop_waits_for_the_running_handler_and_discards_the_queue`
(every non-`Normal` peer reason, on every placement).

## 6. Supervision

**V1. Restart table.** While the supervisor is open:

| Mode | `Normal` / `Shutdown(_)` exit | `Panic` / `Kill` / `Killed` / `NoProc` exit, or a cleanup failure |
|---|---|---|
| Permanent (default) | restart | restart |
| Transient | stay stopped | restart |
| Temporary | stay stopped | stay stopped |

Startup timeouts restart unless Temporary, and so does a failed restart
attempt: a generation that stops before readiness after the child was first
ready is retried whatever its exit reason (OTP retries a failed restart).
Supervisor or system shutdown
suppresses every restart. Stopping a Permanent child from outside restarts it.
Checked by: `Supervision!RestartFollowsPolicy`, `Supervision!TemporaryStartsOnce` (`MCSupervisionTemporary`),
`Cohorts!TemporaryStartsOnce`; [`supervision::restart_modes_distinguish_normal_shutdown_and_panic`][supervision],
[`supervisor_policies`][supervisor_policies] (generated scripts), [`supervision_trace`][supervision_trace];
a `Shutdown` with a payload: [`supervision::restart_modes_distinguish_normal_shutdown_and_panic`][supervision],
[`shutdown_payload`][shutdown_payload]`::tasks_mode::{async_group,thread_group}_restart_table_treats_payload_as_shutdown`.

**V2. Strategies.** OneForOne replaces the failed child; OneForAll the whole
cohort; RestForOne the child and every later-declared sibling. The affected
children stop in reverse declaration order and **all** of them finish cleanup
before any replacement is constructed; replacements start in declaration
order. Temporary collateral children are stopped, not rebuilt. Concurrent
failures coalesce into one round; a new upstream failure supersedes a round.
A replacement that fails before readiness (factory error, panic, startup
timeout or a stop in `started()`) starts a new round, so OneForAll rebuilds
the whole cohort again, as OTP re-runs the strategy on `try_again_restart`.
Checked by: `Cohorts!StrategySelection`, `Cohorts!WholeCohortBarrier`,
`Cohorts!NoRetiredParticipant`, `Cohorts!OneStartPerRound`; [`supervision_strategies`][supervision_strategies],
[`semantics_gaps`][semantics_gaps]`::supervision_contract::concurrent_failures_coalesce_into_one_round`,
[`cohort_trace`][cohort_trace], [`supervision_tree`][ex_supervision_tree] example.

**V3. At most one live generation.** A slot never has two live generations: a
replacement starts only after the previous actor, its registered helpers and
its owned cleanup actually finished. When the supervisor stops a child
(shutdown, a OneForAll/RestForOne round, a startup rollback, or a generation
it gives up on before publishing it: a startup timeout, admission closing
while it constructs, the gate closing between ready and publication), a child
that has not reached its death point `shutdown_timeout` after its signal is
killed
(`Killed`, design E3); if it misses a second `shutdown_timeout` the slot is
stuck. A child that exits on its own is stuck if its cleanup misses
`shutdown_timeout` from its exit (no kill: it would relabel its reason). A
stuck slot fails the supervisor and nothing replaces it. The supervisor's own
wait for a stopped child, in shutdown or a round, is a backstop at three
`shutdown_timeout`s, beyond the kill and the second interval, so it never
gives up on (and fails the tree for) a child before the kill. Failing the
supervisor on a stuck slot is a deliberate deviation from OTP, where `brutal_kill` ends the process at once and the
supervisor carries on (ROADMAP rank 13): Rust cannot preempt a task or thread
stuck in non-cooperative code, so "killed" only takes effect when the code
returns (L3), and a slot whose cleanup never finishes cannot be reused
without overlapping generations. The kill is reported as `shutdown_error`
with action `killed` (I10; OTP reports `shutdown_error` and `brutal_kill`s);
a child the kill does not end within the second interval gets a second
`shutdown_error`, action `stuck`.
Checked by: `Supervision!AtMostOneGeneration`, `Cohorts!AtMostOneGeneration`,
`NestedSupervision!AtMostOneGeneration`; [`supervision::cleanup_deadline_escalates_without_overlapping_a_live_generation`][supervision],
`repeated_crash_producer_and_shutdown_races_never_overlap_generations`,
`stuck_stopped_hook_triggers_cleanup_deadline_before_terminal_publication`;
[`otp_exit_signals::tasks_mode::supervisor_shutdown_is_the_parents_exit_signal`][otp_exit_signals]
(the kill at `shutdown_timeout`); for a supervisor stop turn,
`Supervision!StuckOnlyAfterKill` (`CleanupKill`/`StopExited`, trace-validated
by [`supervision_trace`][supervision_trace]`::trace_shutdown_kills_child_at_shutdown_timeout`
and `trace_shutdown_deadline_with_slow_stop`, with two model faults and two
recorded-trace controls in `negative_checks.py`) and
[`collateral_kill`][collateral_kill]`::shutdown_kills_a_stopped_child_without_failing_the_tree`
(the shutdown loop's backstop; planted bug 36); for a generation stopped
before publication, the same `Supervision!StuckOnlyAfterKill` (`"abandoned"`
and `"startkilled"`, trace-validated by
[`supervision_trace`][supervision_trace]`::trace_unpublished_generation_killed_or_stuck`,
with two model faults and a recorded-trace control) and
[`collateral_kill`][collateral_kill]`::unpublished_generation_is_killed_at_shutdown_timeout`
and `unpublished_generation_is_stuck_only_after_the_kill` (planted bug 40); for a OneForAll/RestForOne round's stop,
`Cohorts!StuckOnlyAfterKill` (`CKill`/`CStuck`, trace-validated by
[`cohort_trace`][cohort_trace]`::trace_collateral_child_killed_or_stuck`, with
a model fault and two recorded-trace controls in `negative_checks.py`) and
[`collateral_kill`][collateral_kill] (`#[sim_test]`: killed at
`shutdown_timeout`, stuck only after a second interval; planted bug 35).

**V4. Generations and publication.** Each construction attempt increments the
slot's generation. `SupervisedChild::current()` returns only the latest live,
ready generation; `wait_for_generation(n)` waits for a ready generation newer
than `n`. Old `ActorRef`s stay bound to the old instance and fail with
`NoProc` (queued requests got `ActorStopped` with its final reason); queued
messages are discarded, never replayed.
Checked by: `Supervision!PublishedIsLive`, `Cohorts!PublishedIsLive`;
[`supervisor_policies`][supervisor_policies] (request fences),
[`semantics_gaps`][semantics_gaps]`::supervision_contract::queued_messages_are_not_replayed_into_a_replacement`,
[`supervision_recovery`][ex_supervision_recovery] example.

**V5. Restart budget.** At most `max_restarts` restart actions per rolling
`restart_window` (default 5 per 30 s), shared by the whole supervisor, as OTP's
intensity and period. Initial construction is free. A OneForOne child retry
(and any retry before the tree is ready) is one action: its capacity is
reserved before backoff, refunded if the backoff is cancelled or superseded,
and stamped as a grant at admission, so a failed factory or `started()` after
admission still counts. A OneForAll/RestForOne recovery round is one action,
however many children it rebuilds: the coordinator takes its grant when it
selects the round, before stopping the cohort (OTP's `add_restart`), and the
round's rebuilds take none. A failed replacement inside a round requests a new
round, charged again (`try_again_restart`); a superseded round keeps its
charge. An exhausted window or `max_restarts = 0` fails at once, without
waiting for backoff. Exhaustion fails the supervisor and stops its children in
reverse order.
Checked by: `Supervision!BudgetBounded`; Kani `restart_window_admits_exactly_within_budget`;
[`supervision::restart_budget_is_shared_between_siblings_and_exhaustion_closes_tree`][supervision],
`disabled_retries_fail_without_waiting_for_a_long_backoff`,
`src/supervision/policy_tests.rs::retry_reservations_exclude_siblings_and_refund_interrupted_backoff`,
`sliding_window_expires_only_old_grants_before_the_next_retry`,
[`supervision_strategies::cohort_reconstruction_consumes_shared_factory_attempt_budget`][supervision_strategies].

**V6. Backoff.** Per child: bound = `initial * 2^failures`, capped at `maximum`
(defaults 10 ms, 5 s). With jitter (default) the delay is drawn uniformly from
zero through the bound, so `initial` is a cap, not a minimum. Without jitter the
delay equals the bound. The exponent resets after the same generation stays
ready for `stable_after` (default 30 s); collateral children add no delay.
Checked by: Kani `backoff_bound_is_capped`, `backoff_jitter_stays_in_range`,
`backoff_bound_is_monotone_*`, `jitter_*`; `src/supervision/policy_tests.rs::capped_backoff_and_seeded_jitter_remain_bounded_after_long_failure_streaks`,
[`supervision::backoff_exponent_resets_only_after_a_stable_generation`][supervision],
[`semantics_gaps`][semantics_gaps]`::supervision_contract::collateral_children_add_no_backoff`.

**V7. Startup.** Children start serially in declaration order. A tree reports
ready only when every child is ready. A child's first start that fails for
any reason (factory error or panic, panic or stop in `started()`, a missed
`startup_timeout`, default 5 s, placement or group closure) fails the start
whatever the restart type, with no retry, as OTP's `start_children` returns
`failed_to_start_child`: the started children roll back in reverse order and
`start()` returns `SupervisorError::FailedToStartChild` only after the
rollback (every child terminal, or `Failed` once its cleanup is stuck). A
ready child's exit while later siblings start is recorded and processed under
the strategy once the tree commits ready, as OTP handles the queued `'EXIT'`
after `init`; if the start aborts, it is part of the rollback. A replacement
that fails before readiness is a failed restart attempt and is retried (V1).
Factories run on the child's group. Cancelling the start future before
readiness rolls the tree back.
Checked by: `Supervision!ReadyMeansAllStarted`, `Supervision!RestartFollowsPolicy` (no restart before the tree is ready), `Cohorts!FirstStartOnce`; [`supervision::failed_partial_startup_rolls_back_without_starting_later_children`][supervision],
`otp_exit_signals::first_start_failure_aborts_start_whatever_the_restart_type`, `ready_child_exit_during_startup_is_processed_afterwards`, `start_returns_its_error_after_rollback`,
[`exit_signals_sim::ready_child_exit_races_the_tree_commit`][exit_signals_sim],
`transient_startup_timeout_retries_after_actual_cleanup`, `normal_pre_ready_exit_records_real_cause_and_fails_transient_startup`,
`transient_replacement_exiting_before_readiness_is_retried`, [`supervision_trace::trace_replacement_exiting_before_readiness_is_retried`][supervision_trace],
`failed_factories_cancel_owned_helpers_before_retry_and_catch_panics`,
[`cancellation_safety::supervisor_start_is_cancel_safe`][cancellation_safety],
[`panic_safety::factory_panics_are_retried`][panic_safety].

**V8. Escalation.** A nested supervisor (`add_supervisor`) that fails escalates
to its parent, which applies its own restart policy to the whole subtree. A
parent's scope finishes only after every descendant finished; late descendant
cleanup failures fail the parent and appear in the system report. A root
supervisor's failure is reported by `Supervisor::failure()` and
`ShutdownReport::supervisor_failures`.
Checked by: `NestedSupervision!NoEarlyParentJoin`, `NestedSupervision!DoneMeansDescendantsJoined`;
[`nested_supervision`][nested_supervision], [`tree_trace`][tree_trace].

**V9. Supervisor shutdown.** Closing a supervisor never reopens it and starts
or publishes nothing afterwards. Ready children get their turn in reverse
declaration order. Each receives its parent's `Shutdown` exit signal (E3): a
non-trapping child ends `Shutdown` without `stopped()`; a trapping child
handles the messages queued before it (`exit_pending()` is `true` there), then
runs `stopped()` and ends `Shutdown`, never seeing it in `exit_received`. Each
turn is bounded by the child's `shutdown_timeout` (default 5 s), then the
child is killed (V3); at the report deadline the remaining siblings are
signalled. `ActorSystem::shutdown(grace)` signals standalone actors the same
way and kills those left at `grace`. "Done" means no live
generation and every controller finished. Supervisors observe children through
internal completion state, not their mailboxes, so a message backlog does not
delay observation.
Checked by: `Supervision!NoStartAfterClose`, `Supervision!ReverseShutdown`,
`Supervision!NeverReopens`, `Supervision!DoneMeansQuiescent`, `Cohorts!NoStartAfterClose`,
`NestedSupervision!NoStartAfterClose`, `Supervision!ClosedLeadsToDone` (TLC-only, assumes cleanup ends);
[`supervision::shutdown_during_backoff_prevents_new_factory_invocation`][supervision],
`supervisor_report_deadline_signals_remaining_siblings`, [`failpoints`][failpoints],
[`semantics_gaps`][semantics_gaps]`::supervision_contract::message_backlog_does_not_delay_restart`;
`Lifecycle!ShutdownEndsChild` (TLC);
[`otp_exit_signals`][otp_exit_signals]`::tasks_mode::{supervisor_shutdown_is_the_parents_exit_signal, exit_pending::queued_work_sees_the_supervisors_pending_shutdown}`;
[`exit_signals_sim::supervisor_shutdown_races_a_trapping_childs_queue`][exit_signals_sim].

## 7. Systems and execution groups

**S1. Admission.** Group construction is all-or-nothing. Actors are admitted
only to open groups (`Closed`); a supervisor accepts only groups of its own
`ActorSystem` (`ForeignGroup`); `thread_group(n)` admits at most `n` live actors (`Capacity`). Once shutdown
begins, actor, helper, timer, monitor and registration admission is closed for
the whole managed tree; infallible APIs return inert work.
Checked by: `Coordinator!Safety` (admission closed after `Running`);
[`execution_groups::groups_validate_names_and_report_their_budget`][execution_groups],
`ordered_shutdown_preserves_ordinary_calls_but_closes_new_helper_admission`,
[`supervision::configuration_rejects_foreign_groups_duplicate_names_and_capacity`][supervision] (foreign groups),
`execution_profiles::persistent_native_slots_bound_admission_and_helpers_progress_during_blocking_turns` (`Capacity`),
[`semantics_gaps`][semantics_gaps]`::supervision_contract::admission_is_closed_for_the_whole_tree_once_shutdown_begins`
(timer, monitor, registration and actor admission from a still-running actor),
[`failpoints`][failpoints] (`system.reserve`, `system.group_start`).

**S2. Ordered shutdown.** `ActorSystem::shutdown(deadline)` stops standalone
actors and supervisor roots together in reverse admission order, one at a time,
each awaited until it finished or the deadline passed. At the deadline every
remaining target is signalled. The coordinator joins only after every target
was signalled.
Checked by: `Coordinator!OrderedShutdown`, `CoordinatorInd!OrderedSignalStep` (Apalache),
`Coordinator!JoinAfterAllSignalled`; [`supervision::system_shutdown_orders_standalone_actors_and_supervisor_trees_together`][supervision],
[`cross_backend::ordered_system_shutdown_matches`][cross_backend],
[`execution_groups::stuck_standalone_cleanup_does_not_leave_earlier_actor_running_forever`][execution_groups].

**S3. Reports and owned cleanup.** `complete` means every owned actor, helper
and group actually finished. An incomplete report names unfinished groups and
actors, keeps admission closed and keeps the runtimes alive until that work
ends; `report.wait()` can be awaited again. Registered helpers are cancelled
(async) or awaited (blocking) before a scope finishes. Dropping an
`ActorSystem` requests shutdown with a 5 s ordering grace without blocking.
Checked by: `Coordinator!Safety` (ownership released only when all finished),
`CoordinatorInd!NeverReopensStep`, `CoordinatorInd!TerminalStep` (Apalache);
[`execution_groups::expired_shutdown_retains_closed_runtime_until_startup_cleanup_finishes`][execution_groups],
`registered_blocking_work_keeps_runtime_owned_until_native_completion`,
`dropping_system_requests_shutdown_without_an_ambient_runtime`, [`leaks`][leaks],
[`semantics_gaps`][semantics_gaps]`::supervision_contract::dropping_a_system_shuts_down_in_order_with_a_five_second_grace`.

**S4. Coordinator faults are reported.** A panic in the system or supervisor
shutdown coordinator is recorded (`coordinator_failures`,
`ShutdownReport::failure`) and followed by an emergency stop of every
remaining target; runtime ownership is still held until all joins.
Checked by: `CoordinatorInd!ReportKeptStep` (Apalache), `Coordinator!Safety`;
`src/system.rs::coordinator_fault_stops_all_targets_and_retains_owned_cleanup`,
[`failpoints`][failpoints] (`system.shutdown_before_target`, `shutdown.before_stop_child`).

**S5. Generation-owned names.** `ctx.register_name` holds the name until that
generation's cleanup finishes; an old generation's cleanup never removes a
replacement's registration.
Checked by: [`execution_groups::old_scope_cleanup_cannot_remove_a_replacement_registration`][execution_groups],
[`supervision::owned_registration_is_released_before_the_replacement_registers`][supervision],
`src/registry.rs::old_guard_preserves_new_registration_even_with_same_actor_identity`.

**S6. Unmanaged work is not owned.** `spawn`/`start` without a placement,
`tokio::spawn` and raw threads, even inside a managed handler, are not joined
by shutdown and can be aborted at group teardown without `stopped()`.
Checked by: [`execution_groups::ambient_actor_started_inside_group_is_unmanaged_and_aborted_at_teardown`][execution_groups],
[`semantics_gaps`][semantics_gaps]`::tasks_contract::unmanaged_tasks_and_threads_are_not_joined_by_shutdown`.

**S7. Placement inheritance (v0.9).** A `start_link` or `start_monitor`
child is placed in its parent's placement (its group and pinned shard, or its
backend) unless the start names one; `spawn` and `start` never inherit
(`placement_of(&ctx)` opts in). A closed or full inherited group fails the
start (`Closed`, `Capacity`), with no fallback. Scope ownership of linked
children (design S8) is not implemented yet.
Checked by: `Start!InheritedPlacement`, `Start!AdmittedOnlyOpen`,
`Start!CapacityRespected` (TLC); start trace
`inherited_thread_group_capacity` and its control "linked child not in its
parent's group"; [`start_contract`][start_contract]`::tasks_mode::{link_and_monitor_inherit_the_parents_group, inherited_start_into_full_thread_group_fails_capacity}`.

**S9. Start-time names (v0.9).** `StartOptions::name(Name<A>)` registers the
name in the start's commit (a clash is `AlreadyStarted` with the holder's
handle, `None` for a non-actor value; nothing is created), holds it until the
instance's publication and releases it before any notice, `wait_exit` or
answer, so a retry after a failed start never overlaps the old instance. A
group slot is released when the instance's scope completes (after
publication and the owned-work join).
Checked by: `Start!NameUnique`, `Start!NameKeptWhileLive`,
`Start!NoOverlapOnReuse`, `Start!NameFreeBeforeObservation`,
`Start!SlotHeldUntilReleased` (TLC); start trace
`named_start_clash_and_release` and its negative controls;
[`start_contract`][start_contract]`::{tasks_mode::names_are_committed_and_released_atomically, threads_mode::names_and_refusals}`.

## 8. Timers and time

**T1. `send_after`.** Sends its message at most once, no earlier than the delay,
as an ordinary message. Cancelling its token or stopping the actor before it
fires prevents the send; a message already enqueued is not retracted. Payloads
are dropped when the actor exits.
Checked by: `src/{tasks,threads}/timer_tests.rs::test_send_after_and_cancellation`,
`src/threads/timer_tests.rs::test_send_after_actor_shutdown`, `src/tasks/timer_tests.rs::test_send_after_gen_server_teardown`,
[`semantics_gaps`][semantics_gaps]`::tasks_contract::managed_timers_run_on_the_group_and_die_with_the_actor`, [`resource_cleanup::timers_release_payloads_on_actor_exit`][resource_cleanup],
[`cross_backend::timers_match`][cross_backend],
[`semantics_gaps`][semantics_gaps]`::tasks::send_after_fires_once_no_earlier_than_its_delay` (paused time).

**T2. `send_interval`.** Repeats until cancelled or the actor stops. Fixed
delay: each tick waits `period` after the previous send, so ticks drift and
missed time is never caught up in a burst.
Checked by: `src/{tasks,threads}/timer_tests.rs::test_send_interval_and_cancellation`,
[`semantics_gaps`][semantics_gaps]`::tasks::send_interval_is_fixed_delay_and_never_catches_up` (paused time,
one jump of 3.5 periods), the fuzz T2 (stop) oracle (no round after the
actor's exit is published). threads timers use real time and are
checked by construction only.

**T3. Timers belong to one instance.** A timer targets the instance whose
`Context` created it and never retargets a supervised replacement. Managed
timers run on the actor's group. `send_message_on` does not send if the actor
stopped first.
Checked by: `src/tasks/actor/tests.rs::send_message_on_cancelled`,
[`semantics_gaps`][semantics_gaps]`::tasks_contract::managed_timers_run_on_the_group_and_die_with_the_actor`
(the timer task runs on the actor's async group),
[`semantics_gaps`][semantics_gaps]`::tasks::timers_are_not_carried_to_a_supervised_replacement` (supervision
is tasks-only).

**T4. Clocks.** Delays use monotonic time (Tokio timers in tasks, so they follow
paused Tokio time in tests; `recv_timeout` in threads). threads: each timer is
one OS thread. Nothing is persisted across process restarts. Request
deadlines are instants on the `spawned_rt` clock: `request`,
`request_with_timeout` and `Response::from` wait on the runtime's timer (paused
Tokio time applies, and a shortened deadline re-arms it); token and job
deadlines, `emit_at` and a raw receiver awaited directly are serviced by the
owner's deadline heap on the real clock (the simulated clock under
`spawned_sim`), not by paused Tokio time.
Checked by: the paused-time tests (`semantics_gaps::tasks::send_after_fires_once_no_earlier_than_its_delay`,
`send_interval_is_fixed_delay_and_never_catches_up`, `default_request_timeout_is_five_seconds_from_first_poll`)
and the simulator's virtual clock for tasks; `src/threads/timer_tests.rs::threads_send_after_uses_the_monotonic_clock_over_long_delays`,
`threads_send_interval_is_fixed_delay_while_the_actor_stalls` and
[`supervision::native_restart_window_expires_on_the_monotonic_clock`][supervision] (`thread_group`) for threads.

**T5. Init timeout (v0.9).** `StartOptions::init_timeout(d)` arms a
framework-owned timer at the start's commit. At expiry it commits `TimedOut`
only if nothing is committed (it loses to a ready commitment) and kills the
instance; the kill is applied when `started()` returns (nothing is
interrupted): a `started()` that never returns keeps its name and slot, and
the waiter answers `InitTimeout { pending: Some(handle) }` at the cleanup
deadline. Default `Infinity`, as OTP.
Checked by: `Start!TimeoutNeverKillsReady`, `Start!CommitImmutable` (TLC);
start traces `init_timeout_waiting`, `abandoned_timeout_delivers_killed` and
the control "init timer wins after a ready commitment";
[`start_contract`][start_contract]`::tasks_mode::{init_timeout_never_reaches_the_parent, stuck_started_reports_pending_after_the_timeout}`.

## 9. Panics

**P1. Where panics are caught.** With unwinding, these are contained and turned
into exit reasons, errors or reports, never into a hung or skipped cleanup:
`started`, handlers (including `exit_received` and `Down`), `stopped`, actor
state and queued-message destructors, abandoned reply destructors, `ChildSpec`
factories (sync and async: a failed attempt), registered helpers
(`HelperError`), supervisor and system coordinators, link-delivery callbacks,
and since 0.7.1 tracing subscribers called from lifecycle diagnostics and
supervision events. Metrics run no user code.
Checked by: [`panic_safety`][panic_safety] (both modes, every backend, both
group kinds), [`foreign_panics`][foreign_panics], [`mailbox_drop_panic`][mailbox_drop_panic],
[`execution_groups::scoped_helpers_report_panics_and_framework_abort_drops_payload`][execution_groups],
[`failpoints`][failpoints].

**P2. `panic = "abort"` disables all of it.** Spawned relies on unwinding. With
`panic = "abort"` any panic ends the process: no `stopped()`, no exit reason, no
`Down`/`Exit`, no restart. Supervision of `Normal`, `Shutdown` and `Kill` exits
still works. A panicking process panic hook, or a second panic while a value is
already unwinding, aborts under either profile.
`#[actor]` warns once per crate built with `panic = "abort"` (opt-out
`allow_panic_abort`); the warning is advice, not a guarantee.
Checked by: the `panic-abort` CI job running [`panic_abort`][ex_panic_abort]
under the `panic-abort` profile (both modes abort on a handler panic with no
`stopped()` and no reply; a Transient child restarts after `Kill` and stays
stopped after `Shutdown`),
[`foreign_panics::panicking_panic_hook_aborts_like_plain_rust`][foreign_panics].

## 10. Not guaranteed

- **Ordering across senders or actors.** Interleaving at one actor is
  arbitrary; A→C and A→B→C may arrive in either order (no causal delivery).
- **Delivery of accepted messages across termination** (D3, D4), durability of
  mailboxes, state or timers, or exactly-once effects. Supervision restores a
  fresh actor, not its state or its queue.
- **Cancellation of accepted work** by timeouts or dropped callers (R4), or
  rollback of effects after an error.
- **Preemption.** A running handler, a blocking call or a non-returning hook is
  never interrupted; stop, shutdown deadlines and cleanup timeouts only signal
  and report. Rust cannot kill a thread.
- **Deadline-service isolation past 64 blocked callbacks, and its inline
  remainder.** Wakers and payload destructors an expiry releases go to a
  pool of at most 64 disposal workers (section 3). With that many callbacks
  blocked at once, later disposals (and so their callers' wake-ups) queue
  behind them; expiry and deferral-credit return do not. A discarded
  scheduled emit's per-job emit credit returns with its message's disposal,
  so it can queue too. A few things still run on the service thread: the
  owner's executor wake, dropping a job's stored wakers, `tracing`
  subscriber calls, a panic payload's destructor, and the work itself if no
  disposal thread can be spawned (deferred-replies.md §2). A payload drop
  decided just before the owner's exit may still run during it (pre-exit
  work; the exit never waits for it). No callback is preempted.
- **Bounded memory.** Mailboxes are unbounded (D5); there are no per-actor
  memory limits (rank 18a).
- **Deadlock detection.** A request cycle, or a handler requesting its own
  actor, waits until `RequestTimeout` (rank 7).
- **Selective receive, priorities or a fair control lane.** Trapped `Exit` and
  `Down` wait behind the backlog (rank 14).
- **Memory isolation.** Actors share an address space. Nothing stops an actor
  from holding state shared with other actors or threads (`Arc<Mutex<_>>`,
  atomics behind `Arc`); a restart then inherits that state, and a panic while
  holding a std lock poisons it for every holder. `#[actor]` warns at compile
  time on the common syntactic patterns in its impl block (opt-out
  `allow_shared_state`), but cannot see sharing built outside that block or
  behind a type alias ([GUIDE](../GUIDE.md#compile-time-lints)).
- **Distribution.** Single process only; `ActorRef`s cannot cross a process.
- **Transparent references.** Nothing rebinds an `ActorRef` to a replacement;
  resolve through `SupervisedChild` or the registry.
- **Erlang/OTP equivalence.** See section 11 for each current difference.
- **Liveness under schedules nobody explored.** Tests run real schedulers;
  models abstract time and handler bodies (see [TESTING](../TESTING.md#how-spawned-is-verified)).
  The base actor lifecycle (signals, links, monitors, requests around the
  death point) is modelled by `Lifecycle.tla` (TLC only), and recorded
  lifecycle traces are checked against it.

## 11. Differences from Erlang/OTP

Principle: match OTP as closely as possible. Deviate only where Rust forces it,
where Rust types add strictness, or where Spawned adds bounds Erlang lacks.

Already the same as OTP: per-sender FIFO (D1), at-most-once delivery (D2),
`gen_server:call`'s 5 s default timeout (R3), a stop dropping the mailbox like a
process exit (D4), exit signals and link notices following `exit_signal/2`
(X3), the `kill`/`killed` split (X4), `noproc` for late links, monitors and
calls and the unlink/demonitor fences (X1, X5, R1), a parent's exit ending a
trapping child through `stopped()` (X3, V9), `stopped()` skipped on kills and
untrapped signals (L5), and the Permanent/Transient/Temporary table (V1). The
remaining differences of the exit-signal design are listed in
[otp-exit-signals.md](otp-exit-signals.md#differences-from-otp-that-remain).

| Today | OTP | Status |
|---|---|---|
| v0.7 had no exit signals: every outside stop ignored `trap_exit` | `exit(Pid, normal)` is ignored by a non-trapping process; every reason except `kill` is trappable | Aligned in v0.8 (rank 5e, #240): `exit`/`kill` follow `exit_signal/2` (X3) |
| `stop`, `shutdown`, `stop_with_reason` return at once and stop after the current handler (X4) | `gen_server:stop` waits for the backlog | Bound: stop latency is one handler; they stay administrative stops, distinct from exit signals |
| v0.7 supervisor shutdown cancelled a trapping child directly | `shutdown` reaches a trapping child as an `'EXIT'` message and it runs normal termination | Aligned in v0.8 (rank 5e, #240): a parent exit (V9) |
| v0.7 propagated a kill over a link as `Kill` | It arrives as `killed` | Aligned in v0.8 (rank 5e, #240): `Killed` (X4) |
| v0.7 linking or monitoring an already-exited actor delivered its real exit reason | `noproc` | Aligned in v0.8 (rank 5e, #240): `NoProc` (X1, X5) |
| A kill or late signal does not interrupt a running `stopped()` (L3) | The process dies at once | Rust-forced: the reason still becomes `Killed` or the signal's reason |
| v0.7 charged the restart budget for every child factory call, collateral ones included | Intensity counts restart actions: one per `one_for_all`/`rest_for_one` round | Aligned in v0.8 (rank 5f, #240): a OneForAll or RestForOne recovery round takes one grant however many children it rebuilds (V5) |
| Initial startup retried a Permanent or Transient child whose factory failed, panicked or timed out (V7), within the budget | `start_link` aborts on any child start error | Aligned in v0.8 (rank 5f, #240): any first-start failure fails `start()` |
| v0.7 reported a runtime-dropped actor as `Kill` (tasks) or `Panic("unexpected framework panic")` (threads) | No counterpart | v0.8 (rank 5g, #240): `Killed` in both modes (X6) |
| Stop is cooperative: a running handler or blocking call is never interrupted (L3) | `kill` ends a process at once | Rust-forced: Rust cannot preempt or kill a thread |
| A child the kill at its shutdown time does not end within a second interval makes its slot stuck and fails the supervisor (V3), reported as `shutdown_error` action `stuck` after the `killed` one | `brutal_kill` ends it and the supervisor continues | Rust-forced: a stuck task or thread cannot be ended from outside; reusing its slot would overlap generations (ROADMAP rank 13) |
| Panics are caught only with unwinding (P2) | Process crashes are always isolated | Rust-forced: `panic = "abort"` and a panicking panic hook end the process |
| No per-actor memory accounting | `max_heap_size` | Rust-forced (no per-actor heap); opt-in size hook proposed, rank 18a |
| One `Handler<M>` per message type; no selective or catch-all receive; `ExitReason` is a closed enum | Any term, selective `receive` | Rust types add strictness; unhandled message types are compile errors |
| `{shutdown, Term}` carries a text payload: `Shutdown(Some(Arc<str>))` | `Term` is any term | Rust types add strictness (v0.9, G7): a typed `Any` payload would lose `Eq` and `Debug` on the reason; the class and every policy match OTP |
| Single process, no distribution | Location-transparent pids | Out of scope until a remote transport (rank 22a and later) |
| Unbounded mailboxes (D5) | Unbounded | Same today; Spawned adds opt-in bounds (rank 17) and byte credits (rank 18) |

## 12. Planned changes to these semantics

- v0.8, ranks 5e, 5f (startup) and 5g, exit signals, startup and runtime
  drop as in OTP ([otp-exit-signals.md](otp-exit-signals.md)): implemented on
  the exit-signals branch (#240, on top of rank 11's #239) and described in
  L3-L6, X1-X6, V1, V3, V5 (the restart budget counts recovery rounds), V7
  and V9 above.

- Rank 11, deferred typed replies, forwarding and pipe-to-self: a reply may
  leave the handler's turn; adds exactly-one completion on panic and timeout.
- Rank 13, bounded draining: an opt-in stop that handles queued work by policy
  instead of discarding it (D4 remains the default).
- Rank 14, fair control path: lifecycle signals not stuck behind a backlog (X7).
- Rank 17, bounded mailboxes: opt-in capacity with reject or wait outcomes (D5).
- Rank 18, byte credits, and rank 18a, per-actor memory limits.
- Rank 19, propagated deadlines: queued work can expire before dispatch (R4).

- 0.9, introspection ([introspection.md](introspection.md)): section 13
  holds I1, I3 (tier 0 part), I4, I5, I7, I10 (crash and supervisor
  reports) and I11 so far; the rest (listings, the fenced watchdog kill, the console)
  join it step by step. L3 and L5 are unchanged (nothing preempts a turn); R4
  has one exception (I4): a status request whose caller gave up before
  dispatch is not run.

See the [ranked backlog](../../ROADMAP.md#ranked-backlog).

## 13. Introspection

Design: [introspection.md](introspection.md). Tier 0 is implemented (0.9
step 1): `info()`, labels, crash and supervisor reports, the status turn
and supervision trees. Listings, the `introspection` feature (turn ids, ages,
mailbox counters), the watchdog and the console are later steps; their
guarantees (I2, I6, I8, I9, I12, I13) are listed in the design and enter this
section with their code. L3, L5, M1, M2, R1 and R3 are unchanged; R4 has one
exception (I4).

**I1. Out of band.** `info()` (on `ActorRef`, `Context` and `ChildHandle`),
`ChildHandle::links()` and `ChildHandle::monitors()` never enqueue, wake, run
user code on the observed actor or wait for a turn. They take short metadata
locks the actor also takes (its exit request, link and monitor tables,
label), so they can contend briefly, never for the length of a turn. A read
during a wedged turn returns and reports `InTurn`. Fields of one `ActorInfo`
are not from one instant.
Checked by: [`introspection`][introspection]`::tasks_mode::info_returns_during_a_wedged_turn`
(every tasks placement: `Async`, `Blocking`, `Thread`, `async_group`,
`thread_group`), `::threads_mode::info_returns_during_a_wedged_turn`.

**I3. Lifecycle and death (tier 0 part).** `lifecycle` reads 0.8 state only:
`Dead` from the death point D (the exit request's `dead`, set in D's critical
section) with the final reason `wait_exit()` returns; `InTurn` while the turn
phase is in a turn; `Exiting` once a reason is requested or the run loop
left; `Starting` until `started()` returns; otherwise `Idle`. `Dead` is
final. Its `published` flag is `false` between D and publication (callers are
answered and sends get `NoProc`, state and payload destructors may still run)
and `true` exactly when `wait_exit()` is ready. A `ChildHandle` that outlives
every `ActorRef` still reads the dead instance. The tier 1 counters of I3
(monotonic turn ids and mailbox counters) come with the `introspection`
feature.
Checked by: [`introspection`][introspection]`::tasks_mode::lifecycle_walks_starting_idle_in_turn_exiting_dead`
(every tasks placement; a state destructor held between D and publication),
`::threads_mode::lifecycle_walks_starting_idle_exiting_dead`,
`::tasks_mode::child_handle_info_after_every_ref_is_dropped`.

**I4. Status is a turn.** `get_state(f)` and `get_status()` (on `ActorRef`,
`get_status` also on `ChildHandle`; OTP `sys:get_state`, `sys:get_status`)
queue a status envelope that runs in a turn of its own: behind the messages
already queued (M2), between handler turns (M1), never inside a suspended
turn. Its reply uses a 0.8 reply slot with the caller's send-time deadline
(R3), answered in the status turn: it never defers and takes no
`MAX_OUTSTANDING` credit. Exactly one outcome: `Ok`, `RequestTimeout`,
`ActorStopped(final reason)` (still queued at the death point, answered
there), `NoProc` (sent after it) or `CallbackFailed` (the panic's message is
logged as a `spawned::status` WARN event with `actor_id`, `actor_type`,
`label` and `error`). The callback
runs under its own `catch_unwind`: a panic is answered `CallbackFailed`, its
payload is disposed of and the turn returns normally, so a caught callback
panic alone does not end the actor; nothing is rolled back (effects through
interior mutability remain; a `ReplyTo` it took and dropped answers
`ReplyDropped`). **Exception to R4:** a status envelope whose slot is already
closed when its turn begins (the caller timed out or dropped the response)
is not run; one that closes later runs to the end and its answer is dropped.
Status envelopes are not messages for `metrics` (no mailbox or request
accounting). `get_status`'s `info` is the actor between turns (`Idle`).
Checked by: [`status`][status]`::tasks_mode::status_runs_between_turns_in_mailbox_order`,
`::status_outcomes_and_abandoned_requests_are_not_run` (fails if the
abandoned check is removed, both modes), `::callback_panic_is_callback_failed_and_actor_continues`,
`::get_state_runs_while_deferred_reply_is_outstanding`,
`::status_during_stopped_and_destruction`, `::status_bypasses_metrics_mailbox_accounting`
(`metrics`), `::threads_mode::*`, `::seeded::get_status_races_stop`
(`#[sim_test]`); `Introspection!StatusBetweenTurns`, `!StatusDischarged`,
`!StatusReasonIsFinal`, `!StatusAtMostOnce`, `!NoCallbackAfterGiveUp`,
`!CallbackPanicContained` (TLC), with recorded runs replayed against the
model (`TraceIntrospection.tla`, `tests/introspection_trace.rs`, both modes,
a runtime drop included) and seven recorded-trace controls rejected
(`negative_checks.py`: an answer after a skip, outside its turn, twice; a
caught panic ending the actor; a dispatch after a kill; `stopped()` after a
kill; a lost kill).

**I5. Redaction.** `get_status` shows type-erased state only for actors that
opt in: `Actor::FORMAT_STATUS` (set by `#[actor(inspect)]` through
`introspection::Inspect`, or `#[actor(inspect = debug)]` through `Debug`);
otherwise `StateView::Redacted`. Output over 64 KiB is cut at a character
boundary (`Truncated`). A formatter panic is `CallbackFailed`.
Checked by: [`status`][status]`::tasks_mode::state_is_redacted_by_default_and_formatted_on_opt_in`,
`::threads_mode::inspect_formats_state`, `src/introspection.rs::tests::state_view_*`.

**I7. Supervision rows.** `Supervisor::which_children()` (OTP
`which_children/1`) returns one row per child in declaration order, each read
under that child's lock (consistent with itself; rows may come from
different instants). `count_children()` (OTP `count_children/1`) and
`tree().counts` are derived from the rows of the same call. `tree()` copies
each level's rows and instance-bound handles, releases every lock, then
expands them (a worker's `info()`, a nested supervisor's `tree()`): a node
never pairs one generation's row with another generation's actor, and no
lock is held across a child's `info()` or a nested `tree()`. A row's `actor`
is its live instance (starting, ready or stopping) or `None`.
Checked by: [`supervision_introspection`][supervision_introspection]`::tree_rows_and_counts_agree`
(OneForOne with a nested OneForAll; rows, counts, expansion, a restart moving
row and node together, after shutdown), `::tree_during_restarts`
(`#[sim_test]`, a OneForAll cohort restarting while `tree()` runs).

**I10. Crash reports.** An actor whose final reason is abnormal and was
produced by the actor itself (a panic in `started()`, a handler or
`stopped()`; a self-exit or parent exit with an abnormal reason) emits
exactly one `spawned::crash` event at `ERROR`, after every caller is answered
and before its state is destroyed, with the fields `actor_id`, `system`,
`actor_type`, `label`, `name`, `reason`, `ancestors`, `generation`, `group`,
`links`. A clean exit and a reason imposed by a kill, an untrapped exit
signal or a runtime drop produce none (OTP: a process killed by a signal runs
no code). For a failed start (unified start, section 2.3 of the design) the
same split holds: the init timer's kill, a kill and a held untrapped signal
produce none; a panic in `started()` and an abnormal `Init::stop(reason)` do.
A panicking subscriber cannot change the reason. Supervisors emit
`spawned::supervisor` reports (OTP's SASL supervisor reports) with the fields
`supervisor`, `system`, `child`, `generation`, `reason`,
`restarts_in_window`, `max_restarts`, `window_ms`, `action`:
`child_terminated` (WARN, an unexpected exit: abnormal or a cleanup failure;
a supervisor-ordered stop or a clean exit is not reported; its `action` is
decided by the restart budget, so it is never `restart` when the intensity
limit stops the supervisor), `child_restarted` (INFO), `start_failed`,
`restart_intensity_reached` and `shutdown_error` (ERROR; a child that missed
its shutdown time when the supervisor stopped it, at shutdown or in a
OneForAll/RestForOne round: action `killed` when it is killed, as OTP
reports before its `brutal_kill`, then a second one with action `stuck` if
the kill does not end it within another interval; a stuck slot fails the
tree, a Rust-forced deviation, see V3). A nested
supervisor instance not ready yet expands to `Unavailable(Starting)`. Sessions come with a later step.
Checked by: [`crash_reports`][crash_reports] (both modes:
`handler_panic_is_reported_with_the_contract_fields`,
`panics_in_started_and_stopped_are_reported`,
`clean_exits_kills_and_signals_are_not_reported`,
`self_exit_with_an_abnormal_reason_is_reported`,
`supervised_report_names_system_group_generation_and_ancestors`,
`threads_mode::handler_panic_is_reported_and_clean_exits_are_not`;
`supervisor_reports::restart_reports_terminated_and_restarted`,
`intensity_and_start_failures_are_errors`,
`a_child_killed_at_its_shutdown_time_is_a_shutdown_error`,
`a_collateral_child_missing_its_shutdown_time_is_a_shutdown_error`,
`a_child_the_kill_does_not_end_is_reported_stuck`;
`supervision_introspection::a_starting_nested_supervisor_reads_starting`).

**I11. No cost on the default path.** Tier 0 adds nothing to send, dispatch
or a turn: its fields are kept by state that already exists or is written at
spawn (type, placement, the instance probe) and by explicit calls
(`set_label`, `monitor` stores its target's id). The crash report runs only
on the exit path.
Checked by: the perf gate (`benchmarks/hotpath.py gate`: instructions per
operation within 3% of the merge base and of the last release).

[start_contract]: ../../concurrency/tests/start_contract.rs
[start_trace]: ../../concurrency/tests/start_trace.rs
[deferred_replies]: ../../concurrency/tests/deferred_replies.rs
[deadline_service]: ../../concurrency/tests/deadline_service.rs
[deferred_jobs]: ../../concurrency/tests/deferred_jobs.rs
[actor_scenarios]: ../../concurrency/tests/actor_scenarios.rs
[cancellation_safety]: ../../concurrency/tests/cancellation_safety.rs
[cohort_trace]: ../../concurrency/tests/cohort_trace.rs
[collateral_kill]: ../../concurrency/tests/collateral_kill.rs
[context_exit]: ../../concurrency/tests/context_exit.rs
[cross_backend]: ../../concurrency/tests/cross_backend.rs
[introspection]: ../../concurrency/tests/introspection.rs
[crash_reports]: ../../concurrency/tests/crash_reports.rs
[status]: ../../concurrency/tests/status.rs
[supervision_introspection]: ../../concurrency/tests/supervision_introspection.rs
[otp_exit_signals]: ../../concurrency/tests/otp_exit_signals.rs
[shutdown_payload]: ../../concurrency/tests/shutdown_payload.rs
[exit_signals_sim]: ../../concurrency/tests/exit_signals_sim.rs
[link_fence_loom]: ../../concurrency/tests/link_fence_loom.rs
[lifecycle_trace]: ../../concurrency/tests/lifecycle_trace.rs
[delivery_simulation]: ../../concurrency/tests/delivery_simulation.rs
[execution_groups]: ../../concurrency/tests/execution_groups.rs
[exit_signal_matrix]: ../../concurrency/tests/exit_signal_matrix.rs
[failpoints]: ../../concurrency/tests/failpoints.rs
[foreign_panics]: ../../concurrency/tests/foreign_panics.rs
[kill_links]: ../../concurrency/tests/kill_links.rs
[leaks]: ../../concurrency/tests/leaks.rs
[linked_exit_macro]: ../../concurrency/tests/linked_exit_macro.rs
[mailbox_admission_loom]: ../../concurrency/tests/mailbox_admission_loom.rs
[mailbox_drop_panic]: ../../concurrency/tests/mailbox_drop_panic.rs
[native_mailbox_drop]: ../../concurrency/tests/native_mailbox_drop.rs
[nested_supervision]: ../../concurrency/tests/nested_supervision.rs
[panic_safety]: ../../concurrency/tests/panic_safety.rs
[panic_payload_drop]: ../../concurrency/tests/panic_payload_drop.rs
[payload_stress]: ../../concurrency/tests/payload_stress.rs
[protocol_timeouts]: ../../concurrency/tests/protocol_timeouts.rs
[request_histories]: ../../concurrency/tests/request_histories.rs
[resource_cleanup]: ../../concurrency/tests/resource_cleanup.rs
[semantics_gaps]: ../../concurrency/tests/semantics_gaps.rs
[sim_ordering]: ../../concurrency/tests/sim_ordering.rs
[shutdown_publication]: ../../concurrency/tests/shutdown_publication.rs
[startup_admission]: ../../concurrency/tests/startup_admission.rs
[supervision]: ../../concurrency/tests/supervision.rs
[supervision_strategies]: ../../concurrency/tests/supervision_strategies.rs
[supervision_trace]: ../../concurrency/tests/supervision_trace.rs
[supervisor_policies]: ../../concurrency/tests/supervisor_policies.rs
[tree_trace]: ../../concurrency/tests/tree_trace.rs
[ex_exit_reason]: ../../examples/capabilities/src/bin/exit_reason.rs
[ex_exit_signals]: ../../examples/capabilities/src/bin/exit_signals.rs
[ex_native_lifecycle]: ../../examples/capabilities/src/bin/native_lifecycle.rs
[ex_panic_abort]: ../../concurrency/examples/panic_abort.rs
[ex_protocol_timeouts]: ../../examples/capabilities/src/bin/protocol_timeouts.rs
[ex_supervision_recovery]: ../../examples/capabilities/src/bin/supervision_recovery.rs
[ex_supervision_tree]: ../../examples/capabilities/src/bin/supervision_tree.rs
[ex_timeout_outcomes]: ../../examples/capabilities/src/bin/timeout_outcomes.rs
