# Deadlock and stall detection

Research note, 2026-11-09. Background for ranks 7 (request deadlock
detection), 8 (stall watchdog), 8a (cooperative budgets) and 30 (wait-for
edges in runtime inspection) of the [roadmap](../../ROADMAP.md#ranked-backlog).
Preemption and cooperative budgets themselves are covered in
[concurrency models](concurrency-models.md#3-preemption-reduction-counting-and-cooperative-budgets);
this note covers detection.

## What it is

Two different failures look the same from outside (nothing progresses):

- **Deadlock:** a cycle of waiters, each blocked on another in the cycle. In an
  actor system the classic case is A requesting B inside a handler while B
  requests A. Nothing is running; nothing will ever run.
- **Stall:** something *is* running, or is blocked outside the runtime's view,
  and starves others: a handler blocking a worker thread with synchronous
  work, a handler stuck across an `.await` that never completes, or a dispatcher
  whose threads are all busy.

Detectors differ in when they look (at the moment of waiting, periodically, or
on demand), what they see (lock owners, channel reachability, scheduling
delay) and whether they are exact or heuristic.

## How it works

### Wait-for graphs (classic)

Database lock managers and operating-systems texts model waiting as a directed
graph: an edge T1 → T2 when T1 waits for a resource T2 holds; a cycle is a
deadlock. Detection is either **eager** (check for a cycle when adding an edge,
so the waiter that would close the cycle is refused) or **periodic** (scan the
graph every interval and pick a victim). Eager checks name the cycle at the
moment it forms; periodic scans cost less per wait but report late. Neither
sees waits the graph does not record. (Textbook material; we cite the systems
below rather than a specific text.)

### Go: global deadlock only, then leak profiles

The Go runtime's `checkdead` counts running OS threads; if none is running and
no timer can wake a goroutine, it calls
`fatal("all goroutines are asleep - deadlock!")`. It skips the check in
`c-shared`/`c-archive` builds, where the host program is assumed to be running
([`runtime/proc.go`](https://github.com/golang/go/blob/master/src/runtime/proc.go)).
This catches only *total* deadlock: if any goroutine still runs (a ticker, an
HTTP server), a cycle among the others goes unreported.

Go 1.26 adds partial detection as an experiment: `GOEXPERIMENT=goroutineleakprofile`
enables a `goroutineleak` profile (and `/debug/pprof/goroutineleak`). "If a
goroutine G is blocked on concurrency primitive P, and P is unreachable from any
runnable goroutine or any goroutine that *those* could unblock, then P cannot be
unblocked." It uses the garbage collector, may miss leaks on primitives
reachable from globals or runnable goroutines' locals, and the Go team "aim[s]
to enable goroutine leak profiles by default in Go 1.27"
([Go 1.26 release notes](https://go.dev/doc/go1.26)).

### Erlang/OTP: timeouts and a self-call check

`gen_server:call` waits 5 seconds by default and exits on timeout; it does not
detect cycles. The one cycle it catches eagerly is the trivial one: a process
calling itself exits with `calling_self`
([`gen.erl`](https://github.com/erlang/otp/blob/master/lib/stdlib/src/gen.erl)).
Cycles are found in testing instead, by Concuerror.

### Concuerror (systematic testing for Erlang)

A "stateless model checking tool for Erlang programs" that detects deadlocks
and race-condition errors. It runs processes as if on one scheduler, records
operations whose outcome depends on scheduling (receives, registry operations,
ETS) and reverses racing pairs to reach other schedules: "stateless model
checking with dynamic partial order reduction" ([Concuerror](https://concuerror.com/)).
A process left blocked in `receive` when nothing else can run is reported as a
deadlock with the schedule that led there. The original paper used the actor
model's structure to instrument selectively and bounded preemptions
([Christakis, Gotovos, Sagonas, ICST 2013](https://kar.kent.ac.uk/58947)).
This is the testing-time counterpart to rank 7, and prior art for rank 20.

### Java: on-demand lock-cycle detection

`ThreadMXBean.findDeadlockedThreads()` "finds cycles of platform threads that
are in deadlock waiting to acquire object monitors or ownable synchronizers",
is "designed for troubleshooting use, but not for synchronization control" and
"might be an expensive operation"; cycles that include virtual threads are not
found ([ThreadMXBean, Java 21](https://docs.oracle.com/en/java/javase/21/docs/api/java.management/java/lang/management/ThreadMXBean.html)).
`jstack` and `jcmd Thread.print` print the same result as "Found one
Java-level deadlock" in a thread dump (not verified against current JDK
output). It works because the JVM records lock owners; a thread waiting on a
queue or future has no owner and is not in the graph.

### Akka: dispatcher starvation detector

Akka Diagnostics schedules a trivial task on the dispatcher every
`check-interval` (default 1 s) and measures how late it runs. Above
`max-delay-warning-threshold` (default 100 ms) it logs "Exceedingly long
scheduling" with thread-state counts and a histogram of the dispatcher threads'
stack traces, rate-limited by `warning-interval` and skipped for an
`initial-delay` after startup. It is part of Akka Diagnostics, under the
Business Source License 1.1
([starvation detector](https://doc.akka.io/libraries/akka-diagnostics/current/starvation-detector.html)).
It detects stalls, not deadlocks: it says the pool is starved and shows what
the threads are doing.

### Tokio: console lints and task hooks

tokio-console (with `console-subscriber` and `RUSTFLAGS="--cfg tokio_unstable"`)
lints running tasks: *self-wakes* (a task waking itself in over 50% of wakeups),
*lost-waker* (a task dropped without being woken), *never-yielded*, and
auto-boxed or large futures ([tokio-console](https://github.com/tokio-rs/console)).
Tokio has no deadlock detector. Its runtime `Builder` exposes stable thread
hooks (`on_thread_park`, `on_thread_unpark`) and, under `tokio_unstable`, task
hooks `on_task_spawn`, `on_before_task_poll`, `on_after_task_poll` and
`on_task_terminate`, each receiving a `TaskMeta` with the task id
([Builder, tokio 1.54](https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html)).
Before/after poll hooks are enough to time every poll, which is what a
long-poll watchdog needs; they are runtime-wide and unstable, so a framework
cannot rely on them by default.

### Comparison

| | Finds | When | Exact? | Cost |
|---|---|---|---|---|
| Eager wait-for check | Cycles among recorded waits | At the wait that closes the cycle | Yes, for recorded edges | Per wait |
| Periodic wait-for scan | Same | Next scan | Yes, for recorded edges | Per interval |
| Go `checkdead` | Everything asleep | When the last thread idles | Yes, total only | Free |
| Go leak profile | Blocked on unreachable primitives | On profile request (GC cycle) | Under-reports | One GC |
| Java `findDeadlockedThreads` | Lock-owner cycles | On demand | Yes, locks only | "Expensive" |
| Akka starvation detector | Starved dispatcher | Periodic probe | Heuristic | One task per interval |
| Concuerror | Deadlocks and races in tests | Test time, all schedules up to bounds | Yes, for the test | Exponential, reduced by DPOR |

## Who uses it

- Go's total-deadlock check runs in every Go program (runtime source above).
  The leak profile is experimental in Go 1.26.
- Java's deadlock detection is built into the JDK's management API and thread
  dumps.
- Akka Diagnostics' starvation detector is a Lightbend/Akka commercial
  diagnostic; we did not verify named users.
- Concuerror: we did not verify named industrial users for this note.

## What it means for Spawned

- **Rank 7: choose eager detection at request time.** Spawned already knows
  every edge exactly: an actor awaiting `request` inside its own handler is the
  only wait that can form a request cycle, and the framework mediates it.
  Insert the edge (current actor → target) when the request is sent, check for
  a cycle with a walk over a small per-actor wait set, and fail the request that
  would close it with `ActorError::WouldDeadlock` naming the cycle; remove the
  edge when the reply or timeout arrives. Reasons: (1) the cycle is named when
  it forms, with both message types, rather than as a 5-second timeout
  elsewhere, which is what ethrex's request-inside-handler code would need to
  debug; (2) unlike Java, the edges are complete for the case covered, since
  request is the only blocking wait the framework sees; (3) a periodic scanner
  adds a thread and still reports late. Keep the rank's limits explicit: waits
  on raw channels, locks, `spawn_blocking` or other runtimes create no edges,
  so this is a safety net, not a proof (Go's leak profile has the same
  honesty). Measure the per-request cost before enabling outside debug builds,
  as rank 7 already says. Erlang's `calling_self` is the free first case: a
  request to oneself inside a handler can fail immediately.
- **Rank 8: an Akka-style probe plus per-poll timing.** Today's debug-only
  `WarnOnBlocking` ([`tasks/actor.rs:1365`](../../concurrency/src/tasks/actor.rs))
  times each poll of the whole actor loop over 10 ms and logs the future's type
  name. Two detectors cover rank 8's two cases: per-poll timing inside
  dispatch, with the actor id and message type, for "one poll blocked a
  worker"; and turn age (time since the current message was dequeued) for
  "handler stuck across awaits". Per execution group, an Akka-style probe task
  measures scheduling delay, which detects starvation even when the culprit is
  outside Spawned. Spawned can time its own polls without Tokio's unstable
  hooks; the hooks remain an option for whole-runtime views behind a feature.
  Reports should name the actor, message type and duration, like Akka's
  "Exceedingly long scheduling" with stacks, but without stack sampling, which
  Rust cannot do cheaply in-process.
- **Rank 8a: detection feeds budgets.** The per-poll and per-turn timings above
  are the work measurements rank 8a needs; one instrumentation point should
  serve both.
- **Rank 30: export the wait-for edges.** The edge set rank 7 maintains is the
  "wait-for edges" column of runtime inspection, and `spawned-top` can render
  it, as `jstack` prints lock cycles in a thread dump.
- **Rank 20: Concuerror is the closest prior art for `explore` in an actor
  language.** It explores message-delivery orderings with DPOR and reports
  stuck receives as deadlocks; `explore` should report "all actors idle with a
  request outstanding" the same way.
- **Partial deadlocks (Go's lesson).** A whole-system "everything idle" check
  is cheap in `spawned-test` (every actor idle, a request outstanding, no timer
  pending) and worth adding to the harness's idle check (rank 6 remaining
  work), but in production something always runs, so rank 7's per-request
  check is the useful one.

## Sources

- [Go `runtime/proc.go` (`checkdead`)](https://github.com/golang/go/blob/master/src/runtime/proc.go); [Go 1.26 release notes: goroutine leak profile](https://go.dev/doc/go1.26)
- [OTP `gen.erl`](https://github.com/erlang/otp/blob/master/lib/stdlib/src/gen.erl)
- [Concuerror](https://concuerror.com/); [Christakis, Gotovos, Sagonas, Systematic Testing for Detecting Concurrency Errors in Erlang Programs, ICST 2013](https://kar.kent.ac.uk/58947)
- [Java `ThreadMXBean` (Java 21)](https://docs.oracle.com/en/java/javase/21/docs/api/java.management/java/lang/management/ThreadMXBean.html)
- [Akka Diagnostics starvation detector](https://doc.akka.io/libraries/akka-diagnostics/current/starvation-detector.html)
- [tokio-console](https://github.com/tokio-rs/console); [tokio `runtime::Builder`](https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html)
