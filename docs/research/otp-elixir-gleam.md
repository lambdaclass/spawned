# Erlang/OTP, Elixir and Gleam compared with Spawned: gaps, changes and non-goals

Research note, 2026-11-11. Research only: nothing in Spawned was changed.

**What was compared.**

- **Spawned:** `origin/main` at `390e4231`: ROADMAP (Priorities, Non-goals, the "Erlang gaps" table and the ranked backlog), `docs/design/semantics.md` §10-12, GUIDE, `docs/research/*`, and the accepted designs `otp-exit-signals.md` and `deferred-replies.md`.
- **In-flight designs:**
  - `design/dynamic-supervisor` (PR #245, revision 2): read in full.
  - `design/introspection` (`51f6efca`): still a three-line stub ("Draft in progress").
  - No `design/unified-start` branch exists on the remote. The section 3 advice on start options is therefore input for a design that has not been written yet.
- **Upstream versions:**
  - Erlang/OTP 28 and 29: OTP 29.0 was released 2026-05-13 ([announcement](https://erlang.org/pipermail/erlang-announce/2026-May/000353.html)).
  - Elixir v1.20.5 (hexdocs, current).
  - gleam_otp v1.3.0 and gleam_erlang v1.3.0 (hexdocs, current).
- **How claims are marked:**
  - **(doc)**: read from the official docs for this note.
  - **(mem)**: from memory. Where a claim is about an older version, it says so.
  - **(spawned)**: read from Spawned's repo.

**How each gap is ranked.** Gaps are ordered by the project's tiers (soundness, introspection, determinism, testing, features, performance), then by OTP-fidelity value and client evidence. Each gap names a target from the plan:

- **0.8:** deferred replies and exit signals. This is in flight, so nothing new is proposed for it.
- **0.9:** unified start, dynamic supervisor and introspection v1.
- **0.10:** admission, draining and the control lane.
- **1.0:** stability.
- **1.x layer:** built on top of the core later.
- **never.**

---

## 1. Feature matrix

Legend:

- **Y:** has it.
- **P:** partial.
- **—:** absent.
- **Planned:** the ROADMAP rank or release that covers it.

### 1.1 Behaviours

| Feature | Erlang/OTP | Elixir | Gleam (gleam_otp 1.3) | Spawned today | Spawned planned |
|---|---|---|---|---|---|
| Server behaviour | `gen_server` | `GenServer` | `actor` (builder: `new`, `on_message`, `start`) (doc) | `Actor` + `Handler<M>`, `#[actor]`/`#[protocol]` | — |
| Typed request/reply | — (untyped `From`) | — | `call(subject, waiting:, sending: fn(Subject(reply)) -> msg)`: the reply subject is carried in the message (doc) | `Message::Result`, `Response<T>` | 0.8 `ReplyTo<R>` tokens |
| Deferred reply (`noreply` + `reply/2`) | Y | Y | P: the reply `Subject` can be stored and sent later (doc) | — (hand-built oneshot in ethrex) | 0.8, rank 11 |
| Init with result/failure | `{ok,S}`, `{stop,R}`, `{error,R}` (OTP 26), `ignore` (doc) | same | `new_with_initialiser(timeout, fn(subject) -> Result(Initialised, String))`; `StartError = InitTimeout \| InitFailed \| InitExited` (doc) | P: fallible only for supervised children (factory `Result`, `startup_timeout`); unsupervised `started()` cannot fail (spawned ROADMAP "Implemented") | 0.8 S1/S2 for supervisors; unified start (0.9) |
| Init returns data to the starter | `proc_lib:init_ack/1` (doc) | `{:ok, pid}` only | `returning(initialised, value)` → `Started(pid, data)` (doc) | — (`start` returns `ActorRef<A>`) | — |
| `handle_continue` (work right after init, before any other message) | `{continue, C}` (OTP 21) (doc) | Y | P: the initialiser runs before any message | — (a self-send from `started()` can queue behind external messages) | — (the dynsup design moves slow work "into the first message") |
| Idle timeout in a return | `{noreply,S,Timeout}`, `{timeout,T,Msg}` (doc) | Y | — | — | rank 38 (timer policies) |
| Hibernate / `hibernate_after` | Y (doc) | Y | — | — | — |
| `format_status` (redacted state for logs and `sys`) | `format_status/1` (OTP 25) (doc) | Y | — | — | — |
| Async request plus collections | `send_request/2,4`, `receive_response`, `reqids_*` (OTP 25) (doc) | `Task.await_many`, `yield_many` (doc) | — | P: `request_raw` returns a oneshot receiver; futures combinators in tasks mode | — |
| `gen_statem` (state enter, postpone, event, state and generic timeouts, inserted events) | Y (mem) | `:gen_statem` (no Elixir wrapper) | — | — | rank 32 (needs rank 16) |
| `gen_event` | Y (mem) | `GenEvent` deprecated (mem) | — | — | — |
| `Agent` (closure-state server) | — | Y (mem) | — | — | — |
| Task with await/timeout | — (proc_lib) | `Task.async/await` (linked, 5 s default) (doc) | `gleam/otp/task` in pre-1.0 gleam_otp (mem; not checked for 1.3) | P: `spawn_helper`; `send_message_on` | 0.8 `ctx.pipe`; rank 11b |
| Bounded parallel map | — | `Task.async_stream` (`max_concurrency`, `ordered`, `timeout`, `on_timeout: :kill_task`, `zip_input_on_exit`) (doc) | — | — | rank 11b (compute pool) |

### 1.2 Supervision

| Feature | Erlang/OTP | Elixir | Gleam | Spawned today | Planned |
|---|---|---|---|---|---|
| one_for_one, one_for_all, rest_for_one | Y | Y | `static_supervisor` (doc) | Y (v0.7) | — |
| Restart types permanent, transient, temporary | Y | Y | Y (`supervision.restart`, default Permanent) (doc) | Y | — |
| Intensity and period defaults | 1 / 5 s | 3 / 5 s | 2 / 5 s (static and factory) (doc) | 5 / 30 s, plus per-child backoff | dynsup: 3 / 5 s (Elixir) |
| Budget counts recovery rounds | Y | Y | Y (mem) | — | 0.8, rank 5f |
| Start aborts on child start failure | Y | Y | Y ("already-started children are shut down") (doc) | — (retries) | 0.8, S1 |
| Dynamic children | `simple_one_for_one` | `DynamicSupervisor` (`max_children`, `extra_arguments`) | `factory_supervisor` (`start_child(sup, arg)`, default restart **Transient**, no `max_children`, no `terminate_child`) (doc) | — | 0.9, rank 12a (PR #245) |
| `terminate_child` / `restart_child` / `delete_child` on static | Y | Y | — | — (a Permanent child stopped individually restarts) | dynsup Q6 "follow-up" |
| `which_children` / `count_children` | Y | Y | `count_children` (factory only) (doc) | P: `snapshots()` on `Supervisor` | dynsup has both |
| Significant children / `auto_shutdown` | Y (OTP 24) | Y | **Y** (`supervision.significant`, `AutoShutdown`) (doc) | — | deferred ("wait for a user") |
| Shutdown `brutal_kill`, timeout, `infinity` | Y | Y | worker timeout, supervisor unlimited (doc) | timeout, then cooperative kill, then "stuck" (Rust-forced) | — |
| `{shutdown, Term}` reasons (a clean exit with a payload) | Y (mem) | Y | P: `stop_abnormal(String)` only | — (closed enum: `Normal`, `Shutdown`, `Panic(String)`, `Kill`) | 0.8 adds `Killed`, `NoProc` only |
| `PartitionSupervisor` | — | Y (1.14) | — | — | rank 27 |
| Task.Supervisor | — | Y | — | — | ranks 11, 11b, 15 |

### 1.3 Signals and messaging

| Feature | Erlang/OTP | Elixir | Gleam | Spawned today | Planned |
|---|---|---|---|---|---|
| Links, `trap_exit`, untrappable kill | Y | Y | Y (`link`, `trap_exits`, `kill`, `send_abnormal_exit`) (doc) | P: OTP deviations listed in semantics §11 | 0.8, rank 5e |
| Monitors with `Down` exactly once, `noproc` | Y | Y | Y (`monitor`, `ProcessDown`; already-dead gives `noproc`) (doc) | P: real reason, not `NoProc` | 0.8 |
| Monitor tags, `{alias, …}` monitors | Y (OTP 24+) (doc) | `Process.monitor/2` (1.15) (doc) | — | n/a: typed `Down` with `MonitorRef` | — |
| Process aliases (revocable address, drops late replies) | Y (OTP 24; `gen_server:call` uses them) (doc) | `Process.alias` (1.15) (doc) | Subjects are per-owner channels (doc) | n/a: a oneshot per request already drops late replies | 0.8 abandonment |
| Priority messages | OTP 28: receiver opts in per alias, monitor (`priority`) or link; exit signals via `exit/3` (doc) | via `:erlang` | — | — (trapped `Exit`/`Down` wait behind the backlog, semantics §10) | rank 14, 0.10 |
| `exit_signal/2` | OTP 29 per Spawned's design (not verified here) | — | `send_exit`, `send_abnormal_exit` | — | 0.8 `ctx.exit`, `ChildHandle::exit` |
| Selective receive | Y | Y | Selectors over owned subjects; `with_selector` changes what is received next (doc) | — (deliberate) | rank 16 bounded stash |
| Per-sender FIFO | Y | Y | Y (doc) | Y (D1) | — |
| `message_queue_data` (off-heap queue) | Y (mem) | via opts | — | n/a (queues are already off-heap) | — |
| Process priority levels (`low`, `normal`, `high`, `max`) | Y (mem) | via opts | — | P: execution groups as bulkheads | rank 39 |
| Mailbox bound | — (none in OTP) | — | — | — | rank 17, 0.10 |

### 1.4 Placement, limits and naming

| Feature | Erlang/OTP | Elixir | Gleam | Spawned today | Planned |
|---|---|---|---|---|---|
| Start variants | `start`, `start_link`, `start_monitor` (OTP 23) (doc) | `start`, `start_link` | `actor.start` (links, mem); `supervised()` child specs (doc) | `start`, `start_with_backend`, `start_on(group)`, `start_linked(ctx)` (Async backend only) (spawned `tasks/actor.rs:1373`) | unified start (0.9) |
| `spawn_opt` options | `link`, `monitor`, `priority`, `min_heap_size`, `max_heap_size`, `message_queue_data`, `fullsweep_after` (mem) | `GenServer` `:spawn_opt` | — | `Backend`, `ExecutionGroup` | 8c stack size, 17, 18a |
| `max_heap_size` | Y (mem) | Y | — | — | rank 18a |
| Local names | `register`, `{via, M, K}` (mem) | `name:` plus `{:via, Registry, …}` (doc) | **typed** `Name(message)`; `named_subject` keeps working across restarts (doc) | global string registry with `Any` downcast (GUIDE §Registry); `ctx.register_name` per generation | rank 12 (scoped, keyed, generations) |
| Process groups | `pg` (OTP 23), `monitor_scope` (mem) | `Registry` duplicate keys, `dispatch` (doc) | — | — | rank 27 |
| Registry as PubSub with partitions | — | Y (`partitions`, `{:duplicate, :key}`, `listeners`, `meta`) (doc) | — | — | ranks 12, 27 |
| `global`, distribution | Y | Y | Y (BEAM) | — | rank 44 (separate layer) |
| ETS, `persistent_term`, counters/atomics | Y (mem) | Y | via FFI | n/a (Rust `Arc`, atomics) | rank 10 snapshots |

### 1.5 Observability and operations

| Feature | Erlang/OTP | Elixir | Gleam | Spawned today | Planned |
|---|---|---|---|---|---|
| `process_info` (queue length, current function, links, monitors, status) | Y (mem) | `Process.info` | via FFI | P: `mailbox_depth()` (metrics feature), `exit_reason()`, supervisor `snapshots()` | introspection v1 (0.9), rank 30 |
| Process labels | `proc_lib:set_label` (OTP 27) (mem) | `Process.set_label` (1.17) (doc) | — | — (type name only) | — |
| `sys:get_state`, `replace_state`, `get_status` | Y (mem) | `:sys` | — | — | 0.9 |
| `sys:suspend`/`resume` | Y | Y | — | — (spawned-test can delay mocks) | — |
| `sys:statistics`, `sys:trace`, `sys:log` | Y | Y | — | P: global metrics | rank 9 |
| Trace sessions | `trace:session_create/3` (OTP 27): isolated, concurrent tracers (doc) | via `:trace` | — | `lifecycle-trace` (test-only recorder) | ranks 5c, 22 |
| Crash reports with context (initial call, ancestors, queue length, links) | proc_lib and SASL (mem) | Logger metadata | — | P: panic message via `tracing` | — |
| Logger metadata per process | Y (OTP 21) (mem) | `Logger.metadata` (mem) | — | — | rank 28 (causal) |
| `observer`, `recon`, `crashdump` | Y (mem) | Y | — | — | rank 30 (`spawned-top`), rank 43 |
| `:telemetry` event conventions | — | `[:lib, :x, :start \| :stop \| :exception]` spans (mem) | — | `metrics` feature (global counters and histograms) | rank 29 |
| `heart` | Y (mem) | Y | — | — | — |
| Hot code upgrade | `change_code`, appup (mem) | Y | — | — | — |

### 1.6 Testing

| Feature | Erlang/OTP | Elixir | Gleam | Spawned today | Planned |
|---|---|---|---|---|---|
| Mocks | meck (mem) | Mox (explicit contracts) (mem) | — | `spawned-test` `Mock`, `Probe` | rank 6 remaining |
| `start_supervised` (owned by the test, torn down in reverse order) | Common Test (mem) | ExUnit (mem) | — | — | — |
| Parallel tests with isolated names | per-node | `async: true` plus per-test Registry (mem) | — | — (global registry) | rank 12 ("scope the registry to an ActorSystem") |
| Systematic exploration | Concuerror (mem) | — | — | simulator, PCT/DFS (#218) | ranks 20-22 |

---

## 2. Ranked gap list

Each entry gives:

- **What:** the gap.
- **Why:** the evidence for it.
- **Tier:** the priority tier.
- **Target:** the release.
- **Cost:** whether it can follow the no-default-cost rule.
- **Overlap:** the ROADMAP rank it overlaps, or "new".

### G1. Fallible, value-returning init on every start path

- **What:** OTP's `init/1` returns `{ok,S}`, `{stop,R}`, `{error,R}` or `ignore`, and `proc_lib` adds `init_ack`/`init_fail`. Gleam's `new_with_initialiser(timeout, f)` returns a `Result`, can choose the selector, and `returning(value)` hands arbitrary data back to the starter (`Started(pid, data)`) (doc).
  - Spawned only has fallible startup for supervised children.
  - `start()` returns `ActorRef<A>` even when `started()` will panic. The caller learns only from `join`/`exit_reason`.
  - There is no init timeout outside supervision.
- **Why:**
  - Soundness: "start succeeded" should mean the actor is ready. OTP's `start_link` is synchronous on `init` for exactly this reason.
  - The client sweep found "the ethrex L2 sequencer starts actors one by one and only logs startup failures".
  - The 0.8 S1/S2 rules make supervisors abort on start failure, but plain `start` stays fire-and-forget.
  - Gleam's typed `returning` is the cleanest prior art for handing typed sub-handles (for example a `Recipient<M>` or a protocol ref) back to the starter.
- **Tier:** soundness / features. **Target:** 0.9, in unified start.
- **Cost:** yes. The await is on the start path, not on send or dispatch; a non-awaiting `spawn` stays available.
- **Overlap:**
  - The "Fallible startup / readiness" row of Implemented (supervised only).
  - The archived `result_on_start` branch.
  - Otherwise new for the unsupervised path.

### G2. Post-init continuation (`handle_continue`)

- **What:** `{ok, S, {continue, C}}` runs `handle_continue` before any other message.
  - In Spawned, a self-send from `started()` is enqueued behind anything that arrived during `started()`.
  - The ordering guarantee is missing.
- **Why:**
  - The dynamic-supervisor design's decision Q1 (serialised construction) explicitly moves ethrex's RLPx handshake "out of `started()` into the first message, OTP's `handle_continue` idiom". Without a real continue, that first message can be overtaken by a peer message or a `terminate_child`, so the actor must defend against a half-initialised state.
  - This is the most common OTP idiom for slow initialisation (mem).
- **Tier:** soundness. **Target:** 0.9, in unified start, because the dynamic supervisor depends on it.
- **Cost:** yes. One branch after `started()`; the dispatch loop is untouched.
- **Overlap:** new. The dynsup design assumes it informally.

### G3. `sys`-style state and status inspection

- **What:**
  - `sys:get_state/1,2` and `sys:get_status`, with `format_status/1` (OTP 25) redaction.
  - `process_info`: status, message queue length, current function, links, monitors, `monitored_by`, `trap_exit`.
- **Why:**
  - This is priority tier 2's own definition ("which actor is stuck, on what message, with how much queued").
  - Ethrex polls its peer table with an ordinary request for health; ethlambda hand-stamps `Instant`s through messages (client sweep).
  - OTP's design point that matters here: system messages go through the mailbox in order and are handled by the behaviour loop between callbacks (mem). That gives `get_state` without a priority lane and without reading `&mut A` during a turn, which matches Spawned's "Debug only between turns" rule (rank 30).
- **Tier:** introspection. **Target:** 0.9 (introspection v1).
- **Cost:** yes, if:
  - status fields reuse the counters the `metrics` feature already keeps;
  - state inspection is an opt-in trait (`Inspect`, or a `#[actor(inspect)]` attribute) that only types which implement it answer;
  - the system envelope is one more variant in the existing envelope type, not a new branch per message.
- **Overlap:** ranks 30, 8 and 9. The `sys` framing and `format_status` redaction are new.

### G4. Process enumeration, labels and tree snapshots

- **What:**
  - `erlang:processes()` plus `proc_lib:set_label` (OTP 27) and `Process.set_label` (Elixir 1.17).
  - `which_children`/`count_children` on every supervisor, and a whole-system snapshot (observer's application tree).
- **Why:**
  - Labels are what makes 10,000 peer actors readable: "PeerConnection(enode…)" rather than the type name.
  - recon's `proc_count`/`proc_window` rank processes by queue length or reductions over a window (mem; already cited in Spawned's observability note).
  - `spawned-top` needs an enumeration source.
- **Tier:** introspection. **Target:** 0.9.
- **Cost:** a live-actor table costs an insert and remove at spawn and exit, not per message. Put it behind an `introspection` feature or on `ActorSystem`-managed actors only. Labels are set once.
- **Overlap:** rank 30. `which_children`/`count_children` exist in the dynsup design; static supervisors lack them (dynsup Q6).

### G5. Structured crash and supervisor reports

- **What:**
  - proc_lib crash reports carry the initial call, ancestors, message queue length, links and the last message (gen_server's `terminate` report includes the last message) (mem).
  - SASL supervisor reports (`child_terminated`, `reached_max_restart_intensity`) carry the child spec and reason (mem).
- **Why:**
  - Spawned logs a panic string and records `failure()` text. Operators cannot tell which message killed a peer actor.
  - Ethrex: "a swarm adapter cannot tell an actor panic from a stop" (client sweep).
- **Tier:** introspection. **Target:** 0.9.
- **Cost:** yes. Built only on the exit path; the "last message type" is a `&'static str` already known at dispatch (`type_name`) and stored only under the feature.
- **Overlap:** rank 9 (flight recorder) covers history. The report schema is new.

### G6. Child management on static supervisors

- **What:** `terminate_child`, `restart_child`, `delete_child`, `which_children` and `count_children` on the existing `Supervisor`.
- **Why:**
  - OTP alignment: today "stopping a Permanent child individually restarts it" (supervision-models), so there is no way to stop one child for maintenance.
  - The dynamic supervisor will have `terminate_child`, so the two supervisor types would diverge at 0.9.
- **Tier:** features, with OTP alignment. **Target:** 0.9, next to the dynamic supervisor (dynsup Q6 "separate follow-up").
- **Cost:** yes. Control path only.
- **Overlap:** dynsup Q6 and the supervision-models gap. The TLA+ models `Supervision` and `Cohorts` need the new transitions.

### G7. Clean exits that carry a payload (`{shutdown, Term}`)

- **What:**
  - OTP treats `{shutdown, Term}` like `shutdown`: no crash report, and a transient child is not restarted (mem: supervisor docs, "normal, shutdown or {shutdown,Term}").
  - Spawned's `ExitReason` has `Shutdown` with no payload, and `Panic(String)` is the only reason with data.
  - Applications (ethrex `ActorStopped` → `exit(2)`) must encode why they stopped elsewhere.
- **Why:**
  - Without it, a clean "peer disconnected: TooManyPeers" exit must be `Shutdown` (losing the cause) or abnormal (restarting a Transient child and logging a crash).
  - The 0.8 design already makes `ExitReason` `#[non_exhaustive]`, so the variant can be added later without breaking callers.
- **Tier:** soundness / OTP fidelity. **Target:** 0.9.
- **Cost:** yes.
- **Overlap:** new. `otp-exit-signals.md` E5 lists "any term (`{shutdown, Term}`)" as a T-difference but does not add it.

### G8. `start_monitor` and one options struct (`spawn_opt` analog)

- **What:**
  - OTP: `start_monitor` (OTP 23) returns `{Pid, MonRef}` atomically (doc). `gen_server` start options are `name`, `timeout`, `hibernate_after`, `spawn_opt`, `debug` (doc).
  - Spawned has four start functions:
    - `start_linked` works only with `Backend::Async`;
    - `start_on(group)` has no linked or monitored variant;
    - there is no atomic monitor-at-start.
- **Why:**
  - Atomic monitor-at-start closes the race in which the child dies before `ctx.monitor`. Today that race reports the real reason, and `NoProc` after 0.8.
  - The dynamic supervisor and ethrex's per-peer owners both need "start and watch".
- **Tier:** soundness / features. **Target:** 0.9 (unified start).
- **Cost:** yes.
- **Overlap:** the unified start plan item. It is new as a ROADMAP rank.

### G9. Runtime-toggled, rate-limited per-actor tracing (`sys:trace`, trace sessions, `recon_trace`)

- **What:**
  - OTP 27's `trace` module allows isolated concurrent sessions: "Several sessions can exist at the same time without interfering with each other" (doc).
  - recon_trace adds hard rate limits so a trace cannot take down production (mem).
  - Spawned's `lifecycle-trace` is a compile-time test recorder with a global section and an unbounded buffer (ROADMAP rank 5c).
- **Why:**
  - Turning on "log every message in and out of actor X for 100 messages or 10 seconds" in a live node is the standard BEAM incident tool.
  - It is also the capture side of rank 22 (record in production, replay in simulation).
  - Sessions matter because rank 5c's monitors, `spawned-top` and an operator's ad-hoc trace would otherwise fight over one sink.
- **Tier:** introspection, then determinism. **Target:** 0.10. Its design should share the production trace sink that rank 5c already requires.
- **Cost:** feature-gated, with one relaxed atomic flag load per dispatch when compiled in, and nothing otherwise.
- **Overlap:** ranks 5c, 9 and 22. The session and rate-limit model is new.

### G10. Per-turn logging context (Logger metadata) and a stable event-name contract (`:telemetry`)

- **What:**
  - Elixir and Erlang attach process metadata (pid, registered name, mfa) to every log line from that process (mem).
  - `:telemetry` gives libraries a naming convention, `[:lib, :obj, :start | :stop | :exception]` with `duration` measurements (mem), that Phoenix, Ecto and Oban all follow (mem).
- **Why:**
  - Handlers that call `tracing::info!` today emit lines with no actor id or label.
  - A per-turn span (actor id, label, message type) gives Logger-metadata behaviour for free in `tracing`.
  - Documenting span and event names as a contract is the telemetry lesson: exporters can then rely on them across releases.
- **Tier:** introspection. **Target:**
  - 0.9: the span, behind a feature.
  - 1.0: the names, part of the stability promise.
- **Cost:** feature-gated. With the feature off nothing is compiled; with it on, `tracing`'s callsite interest cache makes a disabled span cheap but not free, so it stays opt-in.
- **Overlap:** rank 28 (causal tracing across mailboxes) builds on it; rank 29.

### G11. Control lane modelled on OTP 28 priority messages

- **What:** OTP 28 lets a receiver opt in per source: a priority alias, `monitor(..., [priority])`, `link(..., [priority])`. Only those signals jump the queue (doc). It is not a message-type priority, and the sender cannot impose it.
- **Why:**
  - Semantics §10 says trapped `Exit` and `Down` "wait behind the backlog"; rank 14 is the fix.
  - OTP's design answers rank 14's open questions:
    - priority is a receiver-side capability, so a peer cannot starve an actor by flooding it with priority messages;
    - ordering is kept within the priority class.
- **Tier:** soundness / features. **Target:** 0.10 (control lane, as planned).
- **Cost:** yes, if the second queue exists only for actors that opted in (lazily allocated) and the receive loop checks it only when a flag is set.
- **Overlap:** rank 14. The opt-in-per-source shape is new input.

### G12. Named, replace-on-set timers and idle timeouts

- **What:**
  - gen_server `Timeout` in returns (idle timeout).
  - gen_statem `state_timeout` (cancelled on state change), `{timeout, Name}` generic timeouts (setting one with the same name replaces it), and event timeouts (cancelled by any event) (mem).
- **Why:**
  - Ethrex has about 14 actors that reschedule with `send_after`, and a committer that uses a timer token as a pause flag; ethlambda aligns ticks to slots by hand (client sweep).
  - Named, replace-on-set timers remove the stale-tick bugs this pattern causes. An idle timeout is the natural trigger for idle-stop of per-peer actors and later rank 26 deactivation.
- **Tier:** features. **Target:** 0.10 (with rank 38).
- **Cost:**
  - Named timers: yes (a per-actor map, used only by actors that call it).
  - Idle timeout: it must re-arm on each message, so it is opt-in per actor and stays off the default path.
- **Overlap:** rank 38. The named and idle forms are new detail.

### G13. `start_supervised` and per-test isolation in `spawned-test`

- **What:** ExUnit's `start_supervised/2` starts a process under a per-test supervisor that is torn down in reverse order before the next test, and surfaces start errors (mem). `async: true` tests rely on per-test names (mem).
- **Why:**
  - It is the most-used ExUnit OTP helper (mem).
  - Spawned's global registry makes parallel tests share names (rank 12 already notes this).
- **Tier:** testing. **Target:** 0.9.
- **Cost:** zero (dev-dependency crate).
- **Overlap:** ranks 6 and 12. New as an API.

### G14. Typed, scoped, restart-surviving names

- **What:**
  - Gleam's `Name(message)` is a typed name made once at startup. `actor.named(name)` registers it, and `named_subject(name)` keeps addressing whichever process holds it, including after a supervisor restart (doc).
  - Elixir's `{:via, Registry, {reg, key}}` does the same for keys (doc).
  - Spawned's registry is global, keyed by `String`, downcast with `Any` (a wrong type gives `None` silently), and references are instance-bound.
- **Why:**
  - Rank 12 exists for ethrex's per-peer map (#7435).
  - The type-safety half is cheap and fits Spawned's typed philosophy: `Name<P>` where `P` is a protocol ref type, so lookups cannot mistype.
  - Freezing a stringly-typed global registry in 1.0 would be hard to undo.
- **Tier:** soundness (API) / features. **Target:**
  - 1.0 at the latest for the typed, system-scoped API shape;
  - keyed generations follow as rank 12.
- **Cost:** yes. Lookup path only.
- **Overlap:** rank 12. The typed `Name<T>` is new input.

### G15. Significant children and `auto_shutdown`

- **What:** OTP 24. Gleam implements it in v1 (`supervision.significant`, `AutoShutdown.AnySignificant | AllSignificant`) (doc).
- **Why:**
  - It is the standard shape for "run these workers to completion, then end the subtree": a sync job, or a batch import.
  - It is also the trigger for draining (rank 13), per supervision-models.
  - There is no client evidence yet.
- **Tier:** features. **Target:** 1.x layer. Make sure the 0.9 child-spec type leaves room for a `significant` flag (see section 3).
- **Cost:** yes.
- **Overlap:** rank 12a "wait for a user".

### G16. `gen_statem`-style behaviour

- **What:** state functions, state enter calls, postpone (re-delivered on state change), state, event and generic timeouts, and `next_event` inserted events (mem).
- **Why:**
  - OTP's own protocol stacks use it (ssl and ssh connection processes; mem).
  - Ethrex's RLPx handshake and ethlambda's sync states are state machines today written as flags (inference).
- **Tier:** features. **Target:** 1.x layer.
- **Cost:** yes. It is a separate trait or macro over `Handler`, and postpone builds on bounded stashing (rank 16).
- **Overlap:** ranks 32 and 16. G12's named timers are the part worth taking early.

### G17. Process groups, Registry-style PubSub, `PartitionSupervisor`

- **What:**
  - `pg` (OTP 23): scopes, join and leave, `monitor_scope`.
  - Elixir `Registry` duplicate keys with `dispatch`, `partitions`, `{:duplicate, :key}` and `listeners`.
  - `PartitionSupervisor` (doc).
- **Why:**
  - Ethrex's broadcaster re-requests all peers per broadcast: mild evidence (ROADMAP client search).
  - Registry's documented caveat, "the change may not propagate immediately", so some calls can return dead processes (doc), is the partial-failure semantics rank 27 must specify. Elixir's answer is that `send` to a dead pid is harmless; Spawned's equivalent is `send` → `Err(ActorStopped)`, which must be counted, not panicked on.
- **Tier:** features. **Target:** 1.x layer.
- **Cost:** yes.
- **Overlap:** ranks 27 and 12.

### G18. Bounded parallel map à la `Task.async_stream`

- **What:** `max_concurrency` (default `schedulers_online`), `ordered`, per-item `timeout`, `on_timeout: :kill_task` and `zip_input_on_exit` (doc).
- **Why:**
  - Ethlambda's gossip checks run behind semaphores with `catch_unwind`; ethrex has four rayon pools and about 30 parallel call sites (concurrency-models note).
  - The option set is a tested vocabulary for rank 11b's API.
- **Tier:** features. **Target:** 1.x (rank 11b).
- **Cost:** yes.
- **Overlap:** ranks 11b and 15.

### G19. Request collections

- **What:** `send_request/4` plus a `reqids` collection, then `receive_response(Collection, Timeout, Delete)` (OTP 25); `Task.await_many`/`yield_many` (doc).
- **Why:**
  - Fan-out to N actors with one deadline is common (ethrex's broadcaster).
  - In tasks mode, `futures::join_all` over `request()` already does this. Threads mode has no equivalent, because `Response::unwrap` blocks one at a time.
- **Tier:** features. **Target:** 1.0 (a threads helper and a GUIDE recipe).
- **Cost:** yes.
- **Overlap:** new (small).

### G20. Per-actor memory limit (`max_heap_size`)

- **What:** kill a process past a heap limit (mem).
- **Why:** ethrex's 1.5 GB/day growth.
- **Tier:** features. **Target:** 1.x.
- **Cost:** opt-in only.
- **Overlap:** rank 18a, already in the Erlang gaps table.

### G21. Demand-driven pipelines (GenStage, Broadway)

- **What:** demand-driven producer/consumer stages with batching.
- **Why:** both clients have unbounded `spawn_listener` inputs.
- **Target:** 1.x.
- **Overlap:** ranks 33, 18 and 35.

### G22. Node watchdog (`heart`)

- **What:** `heart` restarts a hung VM (mem).
- **Why:**
  - The Rust equivalent is the systemd watchdog (`sd_notify WATCHDOG=1`). It should be fed from the stall watchdog (rank 8) so that a wedged runtime stops petting it.
  - A GUIDE recipe is enough; there is no core work.
- **Target:** 1.0 (documentation).
- **Overlap:** rank 8.

---

## 3. Changes the in-flight 0.9 designs should make

### 3.1 Unified start (no branch yet): start from OTP's start options and Gleam's builder

1. **One builder, three verbs.**
   - Shape: `A::new(..).spawn_opts(opts).start() | .start_link(&ctx) | .start_monitor(&ctx)`.
   - `opts` covers:
     - placement: `group` or `backend`;
     - `label` (OTP 27 `set_label`);
     - `name` (a typed name, G14);
     - `init_timeout` (OTP `timeout`, default 5 s, as `gen_server:start`);
     - `trap_exit` at start (avoids the window before `started()` sets it);
     - `stack_size` (rank 8c).
   - Reserve the fields that 0.10 will need, so 0.10 adds variants rather than a new API:
     - `mailbox: Unbounded | Bounded(n)` (rank 17);
     - `max_memory` (rank 18a);
     - `priority` opt-in (G11).
   - Make the options struct `#[non_exhaustive]` with a builder.
   - Linking and monitoring must work for **every** backend and execution group. Today `start_linked` is Async-only and `start_on` has neither.
2. **Make start synchronous on init.**
   - `start*` resolves after `started()` returns, as `gen_server:start_link` and Gleam's `actor.start`. The unsupervised path then matches supervised children (0.8 S1/S2).
   - Keep a non-awaiting `spawn` for the rare fire-and-forget case.
   - The error type should mirror Gleam's `StartError { InitTimeout, InitFailed(E), InitExited(ExitReason) }`. It should not reuse `ActorError`.
3. **Fallible `started` returning typed data.**
   - Let `started` return `Result<Init<R>, E>`, where `Init` can carry:
     - a value handed back to the starter: Gleam's `returning`, `proc_lib:init_ack`;
     - an optional continuation (G2): `Init::new().continue_with(msg)`;
     - `Init::ignore()` (OTP `ignore`), which the dynamic supervisor treats as "nothing started". The dynsup design currently excludes `ignore` "as the static supervisor"; unified start is the place to add it to both.
   - Keep `()` as the default so existing actors compile: a default associated type, or a separate `#[init]` hook.
4. **A child spec derived from the actor.**
   - Elixir's `use GenServer` generates `child_spec/1`, and Gleam's `supervised()` returns a `ChildSpecification` (doc).
   - `#[actor]` could derive a `ChildSpec` constructor from the same start options, so "start under a supervisor" and "start directly" share one description.
   - Leave room for `significant` (G15) and `shutdown_timeout` in that spec.
5. **What not to copy from `spawn_opt`:** GC tuning (`fullsweep_after`, `min_heap_size`), `message_queue_data` and process priority levels. Groups are the Rust bulkhead. See section 5.

### 3.2 Dynamic supervisor (PR #245) against Elixir and Gleam

The design is already closer to OTP and Elixir than Gleam's `factory_supervisor`. Gleam's version has no `max_children`, no `terminate_child`, and defaults to **Transient** with a 2/5 budget (doc). Suggestions:

1. **Rely on a real `continue` (G2) instead of "the first message".**
   - Decision Q1 moves the RLPx handshake out of `started()`. With unified start's continuation in the same release, the guarantee "the handshake runs before any message from the peer table or a `terminate_child`" becomes framework-enforced.
   - Write the design and the client mapping against that, and note the dependency.
2. **Make the default restart type a visible choice.**
   - `DynamicChildSpec::new` defaults to Permanent (OTP's default). Elixir's `DynamicSupervisor` children take the child's own spec default, and Gleam's factory defaults to Transient.
   - For per-connection children, which is the motivating ethrex case, Temporary is correct: a dropped peer must not be redialled by the supervisor.
   - Keep Permanent for OTP fidelity, but make the ethrex mapping and the example use `temporary()`, and say in the doc why.
3. **Accept `ignore` from the factory** once unified start defines it (see 3.1.3). OTP's `simple_one_for_one` returns `{ok, undefined}`, which is a valid "nothing to supervise" outcome for a duplicate peer.
4. **Return init data, as Gleam's `start_child`.**
   - Gleam returns `Started(child_data)`, where the template chooses the data (doc).
   - `Started { actor, child }` is fine. If unified start adds `Init<R>` data, `Started` should carry `R` as well, so a child can hand back a sub-protocol ref without a second request.
5. **Report through the introspection types.** `DynamicChildInfo` and `ChildCounts` should be the same types introspection v1 exposes for static supervisors (G4, G6), so `spawned-top` walks one tree shape. Agree the type names now, across PR #245 and the introspection design.
6. **Decide open question 1 the OTP way, and log it.** Holding the loop during `terminate_child` is faithful. Instead of changing semantics, have the stall watchdog and introspection show "supervisor blocked in terminate_child(id) for T".

### 3.3 Introspection v1 (stub) against `sys`, `process_info` and trace sessions

1. **Do `get_state` as OTP does: an in-band system message.** It is handled between turns, in mailbox order, behind the actor's backlog. That needs no control lane (rank 14 stays in 0.10) and never touches `&mut A` mid-turn. Pair it with:
   - an opt-in `Inspect` trait (or `#[actor(inspect)]`), so actors without it answer `NotInspectable`;
   - a `format_status`-style redaction hook, defaulting to "no state". Peer keys and secrets must not leak into dumps by default; OTP added `format_status/1` in OTP 25 for this reason.
2. **Split cheap status from in-band state.**
   - `ActorInfo` (`process_info`) must be readable while a handler is wedged, which is the ethrex case. It comes from shared atomics, not from a message: lifecycle state, mailbox length, current message type and turn start, restarts, label, group, parent, `trap_exit`, link and monitor counts.
   - `get_state` is the in-band part and can time out on a stuck actor. Recon draws the same line (mem).
3. **Enumeration and labels:** `system.actors()` / `processes()` with labels (G4), and a supervision tree snapshot that includes dynamic supervisors (3.2.5).
4. **Crash and supervisor reports (G5)** as structured `tracing` events with stable field names (G10). This is cheap and gives the most value for incidents.
5. **Defer trace sessions (G9) to 0.10, but reserve the sink.**
   - The production trace sink rank 5c requires (bounded, per-entity ordering, counted gaps) should be designed so that several isolated consumers can attach, as OTP 27's sessions do.
   - Make that a requirement in the introspection design, even if v1 ships only the snapshot APIs.
6. **No `sys:replace_state`, `suspend` or `change_code` in v1.**
   - `suspend`/`resume` belongs in `spawned-test` fault injection if anywhere.
   - `replace_state` breaks the invariants the TLA+ models assume.
   - `change_code` is not applicable to Rust.

---

## 4. What Spawned does better or differently on purpose

Keep these; do not "fix" them toward Erlang.

| Spawned | Erlang/Elixir | Gleam | Why keep |
|---|---|---|---|
| Reply type bound to the message type (`Message::Result`); unhandled message types are compile errors | untyped terms; the `call` reply is whatever the server sends | typed, but the request carries a `Subject(reply)` built by the caller (`sending: fn(Subject(reply)) -> msg`), so the server can reply on the wrong subject type only if the types allow it | stronger than both: one handler per type, and the reply type cannot drift |
| `Handler<M>` per message type, no selector plumbing | one `handle_call` with pattern matching | one message type per subject; `select_map` adapts other subjects (doc) | Rust dispatches on type; `Recipient<M>` gives Gleam-like per-message capabilities without selectors |
| `ReplyTo<R>` (0.8): affine, `#[must_use]`, a dropped token answers at once | an unanswered `From` waits until the caller's timeout | a stored reply subject has the same problem | the "must answer" guarantee comes from types, and exactly-one discharge is modelled in TLA+ |
| A request returns `Err(RequestTimeout / ActorStopped / NoProc)` | the caller **exits** on timeout or server death (doc) | `call` **panics** the caller on timeout or callee death (doc) | Rust's error model; panicking callers would defeat `catch_unwind` containment. Keep `Result`, and keep OTP's 5 s default and late-reply dropping |
| Bounded deferral, jobs and emit credits (0.8); `max_children` counts restarting children | unbounded | unbounded; the factory has no `max_children` | a bound Erlang lacks (B); production evidence of an ethrex memory leak |
| Per-child capped exponential backoff with jitter | none (immediate restart) | none | spreads restart storms. Keep it opt-in in the dynsup (decision Q3) and on by default in static supervisors, as documented |
| "Stuck" cleanup is reported, never overlapped by a replacement | `brutal_kill` always works | same as BEAM | Rust cannot kill a thread. Never run two generations at once |
| Instance-bound `ActorRef` plus explicit `current()` | pid plus names | `named_subject` follows restarts | explicit beats transparent rebinding. Typed names (G14) give Gleam's convenience without hiding generations |
| Abandonment frees the credit; no aliases needed | OTP 24 needed aliases to drop late replies (doc) | — | the per-request oneshot already makes late replies harmless |
| Deterministic simulator, PCT/DFS, TLA+ trace validation | Concuerror (mem) | — | a differentiator: tier 3 and 4 work no BEAM framework ships in the box |

---

## 5. Do not do

| Item | Reason |
|---|---|
| Unrestricted selective receive | It needs a scannable mailbox (O(n) per receive) and breaks per-type dispatch. Bounded stash (rank 16) and postpone (rank 32) cover the uses. Semantics §11 already lists it as a type-strictness difference |
| `gen_event` | Elixir deprecated `GenEvent` (mem). Handlers run inside the manager, so one crash affects all of them. Use topics or groups (rank 27) with per-subscriber actors |
| Hot code upgrade (`change_code`, appup/relup) | Not applicable to compiled Rust; restart a node or a supervisor instead |
| `hibernate`, `fullsweep_after`, `min_heap_size`, `message_queue_data` | BEAM GC and heap knobs with no Rust counterpart. An idle Spawned actor holds no heap the framework could compact. If idle memory ever matters, measure first; a `shrink` hook would be rank 18a territory |
| Process priority levels (`spawn_opt` `priority`) | A cooperative runtime cannot honour per-actor CPU priority. Execution groups (bulkheads) and rank 39 service classes are the honest form. Do not confuse this with OTP 28 *message* priority (G11), which is worth taking |
| `sys:replace_state` | It mutates state outside a turn, which breaks invariants and the "state only between turns" rule. Tests can rebuild the actor instead |
| Calls that crash the caller on timeout (OTP and Gleam) | See section 4: keep `Result` |
| ETS and `persistent_term` clones in core | Rust has `Arc`, `DashMap`, `ArcSwap` and atomics. Published snapshots (rank 10) are the actor-safe form. A framework table would invite exactly the shared mutable state rank 6b lints against |
| `global` and `multi_call`/`abcast` in core | Distribution is a separate layer (rank 44), never core semantics |
| `brutal_kill` emulation (thread abort or detaching the task) | It would leave state and destructors half-run. The stuck policy is the correct Rust answer |
| Akka-style Resume | Already rejected (supervision-models); recorded here so OTP comparisons do not reopen it |
| Copying Gleam's factory defaults (Transient, 2/5) or OTP's 1/5 blindly | The dynsup design chose Elixir's 3/5 with an explicit reason; keep defaults justified per supervisor kind |
| `Agent` as a core type | A closure-state server is a few lines on `Actor`. It encourages "state behind a request" for reads that rank 10 snapshots serve better. An example at most |

---

## Sources

- Erlang/OTP:
  - [gen_server](https://www.erlang.org/doc/apps/stdlib/gen_server.html): `send_request/4` and `reqids_*` OTP 25; `start_monitor` OTP 23; `format_status/1` OTP 25; `init` returns `{error,R}` OTP 26; `handle_continue` OTP 21; "Starting with OTP 24, gen_server:call uses process aliases".
  - [Processes reference](https://www.erlang.org/doc/system/ref_man_processes.html): priority messages "As of OTP 28.0"; the `priority` option on `alias/1`, `monitor/3`, `link/2` and `exit/3`.
  - [trace module](https://www.erlang.org/doc/apps/kernel/trace.html): "introduced in OTP 27.0"; sessions "without interfering with each other".
  - [supervisor](https://www.erlang.org/doc/apps/stdlib/supervisor.html): cited via Spawned's supervision-models note.
  - [OTP 29.0 announcement](https://erlang.org/pipermail/erlang-announce/2026-May/000353.html).
- Elixir v1.20.5:
  - [Registry](https://elixir.hexdocs.pm/Registry.html): partitions, `{:duplicate, :key}`, listeners, delayed unregistration.
  - [Task](https://elixir.hexdocs.pm/Task.html): `async_stream` options, `await_many`, `yield_many`, `async_nolink`.
  - [Process](https://elixir.hexdocs.pm/Process.html): `alias` 1.15, `set_label` 1.17, `monitor/2` 1.15.
  - [DynamicSupervisor](https://elixir.hexdocs.pm/DynamicSupervisor.html) and [PartitionSupervisor](https://elixir.hexdocs.pm/PartitionSupervisor.html): via the supervision-models note.
- Gleam:
  - [gleam_otp actor](https://gleam-otp.hexdocs.pm/gleam/otp/actor.html): `new_with_initialiser`, `returning`, `StartError`, `call` crashing the caller on timeout.
  - [static_supervisor](https://gleam-otp.hexdocs.pm/gleam/otp/static_supervisor.html): defaults 2/5, `auto_shutdown`.
  - [factory_supervisor](https://gleam-otp.hexdocs.pm/gleam/otp/factory_supervisor.html): Transient default; no `max_children` or `terminate_child`.
  - [supervision](https://gleam-otp.hexdocs.pm/gleam/otp/supervision.html): `significant`, `worker`, `supervisor`, timeouts.
  - [gleam_erlang process](https://gleam-erlang.hexdocs.pm/gleam/erlang/process.html): owned subjects, typed `Name`, selectors, monitors, `call` panics.
- From memory (mem), not re-read for this note:
  - gen_statem details;
  - `{shutdown, Term}` treated as normal by supervisors;
  - proc_lib crash-report and SASL report contents;
  - recon (`proc_window`, `recon_trace` limits);
  - `heart`;
  - `max_heap_size` and other `spawn_opt` options;
  - `pg` (OTP 23);
  - `:telemetry` naming;
  - Logger metadata;
  - the GenEvent deprecation;
  - ExUnit `start_supervised`;
  - Mox;
  - Phoenix, Ecto, Oban and Broadway usage;
  - gleam_otp `task` (from the pre-1.0 API; not checked for 1.3).
- Spawned (`origin/main` `390e4231`):
  - ROADMAP (Priorities, Non-goals, Erlang gaps, ranks 5e-54, client sweep);
  - `docs/design/semantics.md` §10-12;
  - `otp-exit-signals.md`;
  - `deferred-replies.md` ("Differences from OTP");
  - GUIDE;
  - `docs/research/supervision-models.md` and `observability.md`;
  - `concurrency/src/tasks/actor.rs` (`ActorStart`);
  - `origin/design/dynamic-supervisor:docs/design/dynamic-supervisor.md` (revision 2);
  - `origin/design/introspection:docs/design/introspection.md` (stub).
