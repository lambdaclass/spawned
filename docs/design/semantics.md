# Spawned semantics

Status: describes `main` (v0.7.1 plus merged work), 2026-11-10. This is the
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

**D3. Accepted is not handled.** `send` returns `Err(ActorStopped)` only once
the mailbox is closed; the rejected message is dropped. `Ok(())` means
accepted: between a stop request and mailbox closure (including during
`stopped()`), sends still succeed and the message is then discarded.
Checked by: [`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::stop_window_accepts_then_discards_and_releases_before_join`
(sends after the stop request and from `stopped()` are `Ok` and never handled, every placement),
[`mailbox_admission_loom`][mailbox_admission_loom] (threads send gate: publish before drain or fail),
`src/mailbox.rs::accepted_sends_are_drained_even_when_senders_outlive_receiver`.

**D4. Stop discards the queue.** The run loop checks for stop before every
receive. Once a stop is observed no further message is dispatched, including
messages already queued. Queued messages are dropped one by one and every
queued reply sender is released (callers get `ActorStopped`) before the exit is
published.
Checked by: `src/tasks/actor.rs::stop_while_idle_does_not_dispatch_a_queued_request_on_resume`,
`src/threads/actor.rs::repeated_stop_during_a_handler_discards_queued_requests`,
`src/threads/actor.rs::stop_while_receiving_does_not_dispatch_a_later_request`,
[`shutdown_publication`][shutdown_publication], [`mailbox_drop_panic`][mailbox_drop_panic],
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::stop_window_accepts_then_discards_and_releases_before_join`
(payloads dropped and reply senders closed when `join()` returns, every placement),
[`sim_ordering::stop_discards_queue_before_publishing_exit_{pct,dfs}`][sim_ordering].

**D5. Mailboxes are unbounded.** `send` never blocks and never fails for
capacity; nothing bounds queue memory.
Checked by: [`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::unbounded_mailbox_accepts_a_large_backlog_without_blocking`
(100,000 sends behind a blocked handler, every placement). Bounds are planned (rank 17).

## 3. Requests

**R1. Three outcomes.** `request`, `request_with_timeout`, protocol calls and
`spawned::tasks::request` end in exactly one of `Ok(reply)`,
`Err(ActorStopped)` (not accepted, or the reply sender was dropped: the actor
stopped before replying, the handler panicked, or the request was still queued
at stop) or `Err(RequestTimeout)`. A request racing shutdown gets its reply or
`ActorStopped`; a slow teardown can still produce a timeout first.
Checked by: [`actor_scenarios`][actor_scenarios], `src/tasks/actor.rs::request_timeout`,
[`timeout_outcomes`][ex_timeout_outcomes] example.

**R2. Reply identity.** Each request has its own reply channel; a reply reaches
only the caller that sent that request.
Checked by: [`actor_scenarios::concurrent_producers_preserve_order_and_reply_identity`][actor_scenarios],
[`sim_ordering::per_sender_fifo_and_reply_identity_{pct,dfs}`][sim_ordering].

**R3. Timeouts.** Default 5 s. Precedence: `with_request_timeout` handle,
`#[request_timeout]` method attribute, `#[protocol(timeout)]`, 5 s. Send-only
methods have no timeout. tasks: `request`-family futures enqueue on first poll
and protocol methods enqueue when called; the timer starts at first poll.
threads: the call blocks and the timer starts immediately.
Checked by: [`protocol_timeouts`][protocol_timeouts],
[`cancellation_safety::protocol_calls_are_cancel_safe_and_send_eagerly`][cancellation_safety],
[`protocol_timeouts`][ex_protocol_timeouts] example (both modes),
[`semantics_gaps`][semantics_gaps]`::tasks_contract::default_request_timeout_is_five_seconds_from_first_poll`
(4.9 s succeeds, 5.1 s times out, no timer before the first poll).

**R4. Giving up never cancels.** A caller timeout or a dropped caller future
or `Response` does not remove the request from the mailbox, cancel its handler
or roll back effects. The caller cannot tell whether it ran.
Checked by: [`cancellation_safety::request_family_is_cancel_safe`][cancellation_safety],
[`request_histories`][request_histories] (timed-out operations are "pending").

**R5. Handler panic.** A panic while handling a request drops its reply sender:
the caller gets an error, not a reply, and the actor exits with `Panic`.
Checked by: [`semantics_gaps`][semantics_gaps]`::{native,tasks}::panicking_request_handler_gives_the_caller_actor_stopped`
(exactly `ActorStopped`, every backend, a managed group, `Recipient` and raw
receivers), [`panic_safety`][panic_safety].

## 4. Lifecycle

**L1. Hook order.** `started()` runs once before any handler; `stopped()` runs
at most once, after the last handler. No handler runs after `stopped()` begins.
Checked by: [`sim_ordering::turns_and_hooks_never_overlap_{pct,dfs}`][sim_ordering],
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::hooks_and_turns_never_overlap_under_concurrent_requests_and_stop`
(every placement), [`cross_backend::stop_before_first_poll_follows_managed_and_legacy_contract`][cross_backend],
the fuzz M1/L1/L5 oracle.

**L2. Managed readiness.** For actors admitted with `start_on` or by a
supervisor, readiness commits after `started()` returns while admission is
open; ordinary handlers run only after it. `ready()` reports that historical
barrier, not current liveness. If admission closes before `started()` begins,
neither hook runs and the state is dropped (its `Drop` runs). Unmanaged
`.start()` and threads actors stopped that early run both hooks.
Checked by: [`startup_admission`][startup_admission],
[`cross_backend::stop_before_first_poll_follows_managed_and_legacy_contract`][cross_backend],
[`execution_groups::failed_startup_discards_queued_payloads_and_reply_channels_without_dispatch`][execution_groups],
[`semantics_gaps`][semantics_gaps]`::tasks_contract::managed_readiness_gates_handlers_and_stays_historical`
(both group kinds). (`ready_gate_loom` checks the benchmark scheduler's gate, not this barrier.)

**L3. Stop is cooperative.** `ctx.stop()`, `ctx.stop_with_reason(r)`,
`ChildHandle::stop()`, `stop_with_reason(r)` and `shutdown()` request a stop:
the running handler finishes, `stopped()` runs, then the actor exits. Nothing
preempts a running handler or blocking code. The first committed reason wins; a
later stop or linked failure does not relabel it, but an actual panic in
cleanup still makes the exit abnormal.
Checked by: [`context_exit`][context_exit] (both modes),
`src/link_regression_tests.rs::late_link_failure_cannot_relabel_an_already_requested_normal_stop`.

**L4. Exit reasons.** Every actor publishes exactly one `ExitReason`:

| Reason | Arises from |
|---|---|
| `Normal` | `ctx.stop()`, `ChildHandle::stop()` |
| `Shutdown` | `ChildHandle::shutdown()`, `stop_with_reason(Shutdown)`, supervisor or system shutdown, a linked peer's `Shutdown` (non-trapping) |
| `Panic(msg)` | a panic in `started()` (`"panic in started(): .."`), a handler, `exit_received` or `Down` handler (`"panic in handler: .."`), `stopped()` (`"panic in stopped(): .."`, only if not already abnormal); a linked peer's `Panic`; a destructor panic on an otherwise `Normal` exit (`"actor state destructor: .."`, `"queued message destructor: .."`) |
| `Kill` | `stop_with_reason(Kill)`, a linked peer's `Kill`; tasks only: the actor future dropped without completing (runtime shutdown, abort, never polled), unless a non-`Normal` cause was already committed |

`is_abnormal()` is true for `Panic` and `Kill`.
Checked by: [`exit_signal_matrix`][exit_signal_matrix], [`context_exit`][context_exit],
[`panic_safety`][panic_safety], `src/link_regression_tests.rs::never_polled_actor_propagates_runtime_shutdown_to_peer`,
`src/actor_cleanup.rs::destructor_failure_only_escalates_normal_even_without_a_committed_cause`,
[`exit_reason`][ex_exit_reason] and [`native_lifecycle`][ex_native_lifecycle] examples.

**L5. Panics in hooks.** A panic in `started()` stops the actor without calling
`stopped()` and without handling any message. A panic in a handler loses that
message, then `stopped()` runs. A panic in `stopped()` is caught; `join()`
returns and the reason becomes `Panic` unless it was already abnormal.
Checked by: `src/{tasks,threads}/actor.rs::panic_in_started_stops_actor`,
`panic_in_handler_stops_actor`, `panic_in_stopped_still_completes`,
`panic_during_early_stop_is_reported`; [`panic_safety`][panic_safety],
[`foreign_panics::panicking_subscriber_keeps_handler_reason_and_runs_stopped`][foreign_panics],
[`semantics_gaps`][semantics_gaps]`::tasks_contract::panics_in_hooks_follow_the_contract_on_every_placement`
(reason prefix, handled count and `stopped()` calls).

**L6. Destructor panics are secondary.** A panicking `Drop` of the actor state
or of a queued message turns only an otherwise `Normal` exit into `Panic`; a
committed `Shutdown`, `Kill` or panic keeps its reason and the queued failure
is kept in `ChildHandle::mailbox_cleanup_failure()`. A panic payload whose own
destructor panics is forgotten (`dispose_panic`) to stop the cascade.
Checked by: [`mailbox_drop_panic`][mailbox_drop_panic] (5 placements x 1/2/256
destructors), [`panic_safety`][panic_safety], [`native_mailbox_drop`][native_mailbox_drop],
`queued_drop_panic`/`queued_drop_preserves_shutdown` lifecycle traces.

**L7. Exit publication is last.** The exit reason is published after
`stopped()`, after queued messages and reply senders are released, after links
have been signalled and outgoing monitors cancelled. `join()`, `wait_exit()`
and `ChildHandle` waits return only then; afterwards `send`/`request` return
`ActorStopped`.
Checked by: [`shutdown_publication`][shutdown_publication],
`src/link_regression_tests.rs::never_polled_completion_waits_for_actor_payload_destruction`,
[`actor_scenarios`][actor_scenarios] (post-exit requests),
[`semantics_gaps`][semantics_gaps]`::{native,tasks}_contract::stop_window_accepts_then_discards_and_releases_before_join`,
[`sim_ordering::stop_discards_queue_before_publishing_exit_{pct,dfs}`][sim_ordering]. Not yet checked:
outgoing monitors cancelled before publication.

## 5. Links, monitors and trapped exits

**X1. Down exactly once.** `ctx.monitor(&handle)` (requires `Handler<Down>`)
yields exactly one `Down { monitor_ref, reason }` per monitor that is not
cancelled, carrying the target's published reason. An already-exited target
produces `Down` immediately. Each call returns a distinct `MonitorRef`; the
target is unaffected.
Checked by: [`actor_scenarios::demonitor_before_exit_suppresses_notification_and_races_do_not_duplicate`][actor_scenarios],
[`exit_signal_matrix`][exit_signal_matrix], `src/*/actor.rs::monitor_*`,
`multiple_monitors_each_get_own_ref_and_down`,
[`cancellation_safety::monitor_and_link_setup_requests_are_cancel_safe`][cancellation_safety].

**X2. Demonitor and late registration.** `demonitor` before the target exits
suppresses `Down`; a `Down` already queued may still arrive. A monitor
registered by an actor that is already stopping is inert (no waiter, no `Down`).
Checked by: `src/*/actor.rs::demonitor_before_target_dies_suppresses_down`,
`closed_monitor_table_rejects_late_registration`, [`leaks::monitor_waiters_released_on_demonitor_owner_exit_and_target_exit`][leaks].

**X3. Link propagation.** When a linked actor exits:

| Peer exits with | Non-trapping peer | Trapping peer (`ctx.trap_exit(true)`) |
|---|---|---|
| `Normal` | nothing | `exit_received(Exit { from, reason: Normal })` |
| `Shutdown`, `Panic`, `Kill` | stopped cooperatively, exits with the same reason (unless it already committed one); propagates further | `exit_received(Exit { from, reason })` |

Propagation is transitive through non-trapping actors. Links work across modes.
Checked by: `src/tasks/actor.rs::link_propagates_panic_to_non_trapping_peer`,
`link_delivers_exit_to_trapping_peer`, `link_normal_exit_not_propagated_to_non_trapping_peer`,
`chain_propagation_through_non_trapping_middle` (and threads equivalents),
[`exit_signal_matrix`][exit_signal_matrix], [`linked_exit_macro`][linked_exit_macro],
`src/link_regression_tests.rs::linked_runtime_abort_cancels_an_idle_threads_actor` (cross-mode).

**X4. Direct stops are untrappable, propagated Kill is trappable.** Every
direct stop (`stop`, `shutdown`, `stop_with_reason(any)`) ignores `trap_exit`.
A `Kill` arriving over a link is trappable, so one kill cannot take down a
graph of trapping observers. This follows Erlang's `kill`/`killed` split, except the
propagated value is still `Kill` (section 11).
Checked by: [`kill_links`][kill_links] (both modes).

**X5. Link registration.** Linking to an already-exited peer delivers that
peer's actual reason once, under X3, and retains no link (Erlang reports
`noproc` instead). Repeating a live link and linking to oneself are no-ops.
`unlink` stops future propagation but does not retract a signal in flight.
`start_linked` registers the link before the child's `started()`. A link
installed while the peer is between draining its links and publishing its exit
still receives the exit. See section 11 for the planned `NoProc`.
Checked by: `src/tasks/actor.rs::link_to_already_dead_actor_delivers_signal`,
`link_to_already_dead_delivers_exactly_once_to_trapping_peer`, `duplicate_link_is_idempotent`,
`unlink_prevents_signal_delivery`; `src/link_regression_tests.rs::linking_in_post_drain_pre_completion_window_delivers_exit`,
`start_linked_registers_before_async_started`; `src/native_link_regression_tests.rs::start_linked_registers_before_threads_started`.

**X6. Runtime teardown still notifies.** A tasks actor whose future is dropped
by its runtime, polled or never polled, still signals its links and monitors
with `Kill`, and the notice is trappable.
Checked by: `src/link_regression_tests.rs::never_polled_actor_propagates_runtime_shutdown_to_peer`,
`polled_actor_propagates_runtime_shutdown_to_peer`, `never_polled_actor_notifies_monitor_on_runtime_shutdown`,
`linked_runtime_abort_is_trappable`.

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
[`exit_signal_matrix`][exit_signal_matrix] (delivery).

## 6. Supervision

**V1. Restart table.** While the supervisor is open:

| Mode | `Normal` / `Shutdown` exit | `Panic` / `Kill` exit |
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
[`supervisor_policies`][supervisor_policies] (generated scripts), [`supervision_trace`][supervision_trace].

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
its owned cleanup actually finished. If cleanup misses its `shutdown_timeout`,
the slot is stuck, the supervisor fails and nothing replaces it.
Checked by: `Supervision!AtMostOneGeneration`, `Cohorts!AtMostOneGeneration`,
`NestedSupervision!AtMostOneGeneration`; [`supervision::cleanup_deadline_escalates_without_overlapping_a_live_generation`][supervision],
`repeated_crash_producer_and_shutdown_races_never_overlap_generations`.

**V4. Generations and publication.** Each construction attempt increments the
slot's generation. `SupervisedChild::current()` returns only the latest live,
ready generation; `wait_for_generation(n)` waits for a ready generation newer
than `n`. Old `ActorRef`s stay bound to the old instance and fail with
`ActorStopped`; queued messages are discarded, never replayed.
Checked by: `Supervision!PublishedIsLive`, `Cohorts!PublishedIsLive`;
[`supervisor_policies`][supervisor_policies] (request fences),
[`semantics_gaps`][semantics_gaps]`::supervision_contract::queued_messages_are_not_replayed_into_a_replacement`,
[`supervision_recovery`][ex_supervision_recovery] example.

**V5. Restart budget.** At most `max_restarts` retry grants per rolling
`restart_window` (default 5 per 30 s), shared by the whole supervisor. Initial
construction is free; every later factory call counts, including failed
factories and collateral OneForAll/RestForOne children (unlike OTP, which counts
rounds). Capacity is reserved before backoff, refunded if the backoff is
cancelled or superseded, and stamped as a grant at admission. An exhausted
window or `max_restarts = 0` fails at once, without waiting for backoff.
Exhaustion fails the supervisor and stops its children in reverse order.
Checked by: `Supervision!BudgetBounded`; Kani `restart_window_admits_exactly_within_budget`;
[`supervision::restart_budget_is_shared_between_siblings_and_exhaustion_closes_tree`][supervision],
`disabled_retries_fail_without_waiting_for_a_long_backoff`,
`src/supervision.rs::retry_reservations_exclude_siblings_and_refund_interrupted_backoff`,
`sliding_window_expires_only_old_grants_before_the_next_retry`,
[`supervision_strategies::cohort_reconstruction_consumes_shared_factory_attempt_budget`][supervision_strategies].

**V6. Backoff.** Per child: bound = `initial * 2^failures`, capped at `maximum`
(defaults 10 ms, 5 s). With jitter (default) the delay is drawn uniformly from
zero through the bound, so `initial` is a cap, not a minimum. Without jitter the
delay equals the bound. The exponent resets after the same generation stays
ready for `stable_after` (default 30 s); collateral children add no delay.
Checked by: Kani `backoff_bound_is_capped`, `backoff_jitter_stays_in_range`,
`backoff_bound_is_monotone_*`, `jitter_*`; `src/supervision.rs::capped_backoff_and_seeded_jitter_remain_bounded_after_long_failure_streaks`,
[`supervision::backoff_exponent_resets_only_after_a_stable_generation`][supervision],
[`semantics_gaps`][semantics_gaps]`::supervision_contract::collateral_children_add_no_backoff`.

**V7. Startup.** Children start serially in declaration order. A tree reports
ready only when every child is ready; a non-restartable failure (or exhausted
retries) rolls back the started children in reverse order. Each attempt has a
`startup_timeout` (default 5 s) that counts as a failure. A child that stops
`Normal` (or `Shutdown`) before its initial readiness is a startup failure and,
if Transient, fails the start, as a failed child `init` fails an OTP
supervisor's start. A replacement that stops before readiness is a failed
restart attempt and is retried (V1). Factories run on the child's
group; a factory error or panic is a failed attempt. Cancelling the start future
before readiness rolls the tree back.
Checked by: `Supervision!ReadyMeansAllStarted`; [`supervision::failed_partial_startup_rolls_back_without_starting_later_children`][supervision],
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
or publishes nothing afterwards. Ready children get their stop turn in reverse
declaration order, each bounded by its `shutdown_timeout` (default 5 s); at the
report deadline the remaining siblings are signalled. "Done" means no live
generation and every controller finished. Supervisors observe children through
internal completion state, not their mailboxes, so a message backlog does not
delay observation.
Checked by: `Supervision!NoStartAfterClose`, `Supervision!ReverseShutdown`,
`Supervision!NeverReopens`, `Supervision!DoneMeansQuiescent`, `Cohorts!NoStartAfterClose`,
`NestedSupervision!NoStartAfterClose`, `Supervision!ClosedLeadsToDone` (TLC-only, assumes cleanup ends);
[`supervision::shutdown_during_backoff_prevents_new_factory_invocation`][supervision],
`supervisor_report_deadline_signals_remaining_siblings`, [`failpoints`][failpoints],
[`semantics_gaps`][semantics_gaps]`::supervision_contract::message_backlog_does_not_delay_restart`.

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

**S6. Unmanaged work is not owned.** `.start()`, `tokio::spawn` and raw
threads, even inside a managed handler, are not joined by shutdown and can be
aborted at group teardown without `stopped()`.
Checked by: [`execution_groups::ambient_actor_started_inside_group_is_unmanaged_and_aborted_at_teardown`][execution_groups],
[`semantics_gaps`][semantics_gaps]`::tasks_contract::unmanaged_tasks_and_threads_are_not_joined_by_shutdown`.

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
one jump of 3.5 periods). threads timers use real time and are
checked by construction only.

**T3. Timers belong to one instance.** A timer targets the instance whose
`Context` created it and never retargets a supervised replacement. Managed
timers run on the actor's group. `send_message_on` does not send if the actor
stopped first.
Checked by: `src/tasks/actor.rs::send_message_on_cancelled`,
[`semantics_gaps`][semantics_gaps]`::tasks_contract::managed_timers_run_on_the_group_and_die_with_the_actor`
(the timer task runs on the actor's async group),
[`semantics_gaps`][semantics_gaps]`::tasks::timers_are_not_carried_to_a_supervised_replacement` (supervision
is tasks-only).

**T4. Clocks.** Delays use monotonic time (Tokio timers in tasks, so they follow
paused Tokio time in tests; `recv_timeout` in threads). threads: each timer is
one OS thread. Nothing is persisted across process restarts.
Checked by: the paused-time tests (`semantics_gaps::tasks::send_after_fires_once_no_earlier_than_its_delay`,
`send_interval_is_fixed_delay_and_never_catches_up`, `default_request_timeout_is_five_seconds_from_first_poll`)
and the simulator's virtual clock for tasks; `src/threads/timer_tests.rs::threads_send_after_uses_the_monotonic_clock_over_long_delays`,
`threads_send_interval_is_fixed_delay_while_the_actor_stalls` and
[`supervision::native_restart_window_expires_on_the_monotonic_clock`][supervision] (`thread_group`) for threads.

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
  The base actor lifecycle (links, monitors, requests around shutdown) has no
  TLA+ model of the v0.7 behaviour; `Lifecycle.tla` models the proposed v0.8
  contract ([next model](../../verification/tla/README.md#next-model)).

## 11. Differences from Erlang/OTP

Principle: match OTP as closely as possible. Deviate only where Rust forces it,
where Rust types add strictness, or where Spawned adds bounds Erlang lacks.

Already the same as OTP: per-sender FIFO (D1), at-most-once delivery (D2),
`gen_server:call`'s 5 s default timeout (R3), a stop dropping the mailbox like a
process exit (D4), linked exits propagating the same reason and `Normal` being
ignored by non-trapping peers (X3), and the Permanent/Transient/Temporary table (V1).

| Today | OTP | Status |
|---|---|---|
| Every direct stop (`stop`, `shutdown`, `stop_with_reason(any)`) ignores `trap_exit` (X4) | `exit(Pid, normal)` is ignored by a non-trapping process; every reason except `kill` is trappable | Planned v0.8, rank 5e: a `Normal` exit signal is ignored by a non-trapping actor, all reasons except `Kill` (including `Shutdown`) are trappable, only `Kill` is untrappable |
| Supervisor shutdown cancels a trapping child directly | `shutdown` reaches a trapping child as an `'EXIT'` message and it runs normal termination | Planned v0.8, rank 5e |
| A kill propagated over a link stays `Kill` (X4) | It arrives as `killed` | Planned v0.8, rank 5e: a distinct trappable `Killed` |
| Linking or monitoring an already-exited actor delivers its real exit reason (X1, X5) | `noproc` | Planned v0.8, rank 5e: `NoProc` |
| The restart budget counts every child factory call, collateral ones included (V5) | Intensity counts recovery rounds | Planned v0.8, rank 5f |
| Initial startup retries a Permanent or Transient child whose factory fails, panics or times out (V7), within the budget | `start_link` aborts on any child start error | Planned v0.8, rank 5f |
| An actor dropped by its runtime exits `Kill` in tasks; the threads fallback is `Panic("unexpected framework panic")` (L4) | No counterpart | Planned v0.8, rank 5g: one reason in both modes |
| Stop is cooperative: a running handler or blocking call is never interrupted (L3) | `kill` ends a process at once | Rust-forced: Rust cannot preempt or kill a thread |
| Panics are caught only with unwinding (P2) | Process crashes are always isolated | Rust-forced: `panic = "abort"` and a panicking panic hook end the process |
| No per-actor memory accounting | `max_heap_size` | Rust-forced (no per-actor heap); opt-in size hook proposed, rank 18a |
| One `Handler<M>` per message type; no selective or catch-all receive; `ExitReason` is a closed enum | Any term, selective `receive` | Rust types add strictness; unhandled message types are compile errors |
| Single process, no distribution | Location-transparent pids | Out of scope until a remote transport (rank 22a and later) |
| Unbounded mailboxes (D5) | Unbounded | Same today; Spawned adds opt-in bounds (rank 17) and byte credits (rank 18) |

## 12. Planned changes to these semantics

- v0.8, rank 5e, exit signals as in OTP: trappable
  `Shutdown`, `Normal` ignored by non-trapping actors, `Killed` for propagated
  kills, `NoProc` for links and monitors to exited actors, normal termination
  for a trapping child on supervisor shutdown (changes X1, X3, X4, X5, L3, L4).
- v0.8, rank 5f: the restart budget counts recovery rounds (changes V5).
- v0.8, rank 5g: one exit reason for runtime-dropped actors in both modes (changes L4, X6).
- Design for 5e, the 5f startup rule and 5g, accepted (not implemented):
  [otp-exit-signals.md](otp-exit-signals.md). Reason-bearing `ActorStopped`
  and `NoProc` for requests wait for rank 11 (#239).

- Rank 11, deferred typed replies, forwarding and pipe-to-self: a reply may
  leave the handler's turn; adds exactly-one completion on panic and timeout.
- Rank 12a, dynamic supervisor (design in review,
  [dynamic-supervisor.md](dynamic-supervisor.md)): adds V10-V13 (dynamic
  admission, a cap that counts restarting children, terminated means
  removed, concurrent shutdown of dynamic children) and V14 (a child stuck
  while retiring fails only itself); touches V1, V3-V5, V8, V9, S1, S2.
- Rank 13, bounded draining: an opt-in stop that handles queued work by policy
  instead of discarding it (D4 remains the default).
- Rank 14, fair control path: lifecycle signals not stuck behind a backlog (X7).
- Rank 17, bounded mailboxes: opt-in capacity with reject or wait outcomes (D5).
- Rank 18, byte credits, and rank 18a, per-actor memory limits.
- Rank 19, propagated deadlines: queued work can expire before dispatch (R4).

See the [ranked backlog](../../ROADMAP.md#ranked-backlog).

[actor_scenarios]: ../../concurrency/tests/actor_scenarios.rs
[cancellation_safety]: ../../concurrency/tests/cancellation_safety.rs
[cohort_trace]: ../../concurrency/tests/cohort_trace.rs
[context_exit]: ../../concurrency/tests/context_exit.rs
[cross_backend]: ../../concurrency/tests/cross_backend.rs
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
[ex_exit_reason]: ../../examples/exit_reason
[ex_native_lifecycle]: ../../examples/capabilities/src/bin/native_lifecycle.rs
[ex_panic_abort]: ../../concurrency/examples/panic_abort.rs
[ex_protocol_timeouts]: ../../examples/capabilities/src/bin/protocol_timeouts.rs
[ex_supervision_recovery]: ../../examples/capabilities/src/bin/supervision_recovery.rs
[ex_supervision_tree]: ../../examples/capabilities/src/bin/supervision_tree.rs
[ex_timeout_outcomes]: ../../examples/capabilities/src/bin/timeout_outcomes.rs
