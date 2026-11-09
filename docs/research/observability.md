# Observability: observer, recon, tokio-console, OpenTelemetry, Akka and Orleans

Research note, 2026-11-09. Informs ranks 9, 28, 29 and 30 of the
[roadmap](../../ROADMAP.md#ranked-backlog), and the stall watchdog (rank 8)
where it overlaps. The earlier idea list (live introspection, flight recorder,
causal tracing) is in
[framework ideas](../../legacy/docs/research/framework-ideas.md#d-observability-and-debugging);
this note adds the prior art behind it.

## What it is

Actor runtimes are observed at three levels, and the mature ones offer all
three:

1. **Live inspection**: attach to a running node and see every actor, its
   parent, mailbox depth and current work (Erlang observer, recon, tokio-console,
   kameo's console).
2. **Aggregate metrics**: counters, gauges and histograms exported to
   Prometheus or OpenTelemetry (Akka Insights, Orleans meters, Spawned's
   `metrics` feature).
3. **Causal tracing**: one request followed across actor hops, with context
   carried inside each message (OpenTelemetry, Orleans activity propagation).

## How it works

### Erlang observer

Observer is a GUI bundled with OTP. The guide recommends running it "from a
standalone node to minimize the impact of the system being observed", connected
to the target over distribution. Tabs: *System*, *Load Charts* (scheduler
utilization, memory by category, I/O), *Memory Allocators*, *Applications* (each
application's supervision tree; right-click a process for info or tracing),
*Processes* (Pid, description, reductions, memory, message-queue length; per
process: messages, dictionary, stack trace, state, log; kill a process), *Ports*
(with links and monitors), *Table Viewer* (ETS) and *Trace Overview* (messages,
calls, scheduling and GC events for selected processes)
([observer user's guide](https://www.erlang.org/doc/apps/observer/observer_ug.html)).
The "State" view relies on OTP behaviours answering `sys:get_state`, which only
works between callbacks: a process busy in a handler cannot answer.

### recon

recon (Fred Hebert) is "a set of tools usable in production to diagnose Erlang
problems or inspect production environment safely"
([README](https://github.com/ferd/recon)). Its functions encode lessons about
what is unsafe in production:

- `proc_count/2` returns the top N processes by an attribute (memory,
  reductions, queue length); `proc_window/3` does the same over a sliding window,
  which finds processes that are busy *now* rather than old ones with large
  totals.
- `bin_leak/1` forces GC and reports the processes that freed the most
  reference-counted binaries.
- `info/1` is a safer `process_info`: it excludes the mailbox, which "may have a
  tendency to grow and be unsafe when called in production systems".
- `get_state/1,2` fetches an OTP process's state with a timeout
  ([recon](https://ferd.github.io/recon/recon.html)).
- `recon_trace` traces calls with an absolute count or a rate limit such as
  `{10, 100}` (10 messages per 100 ms); the tracer "never block[s] while
  building up a large mailbox"
  ([recon_trace](https://ferd.github.io/recon/recon_trace.html)).

### tokio-console and console-subscriber

`console-subscriber` is a `tracing-subscriber` layer that aggregates the
runtime's own `tracing` instrumentation and serves it over gRPC to the
`tokio-console` TUI. It needs three things: Tokio's `tracing` feature, the
`tokio_unstable` cfg (switching it causes full rebuilds), and the `tokio` and
`runtime` targets at TRACE level. It collects tasks with waker activity, time
resources (`sleep`, `Interval`) and sync resources (mutexes, semaphores,
channels); completed tasks are retained for a configurable period
([console-subscriber README](https://github.com/tokio-rs/console/blob/main/console-subscriber/README.md)).
The console flags tasks with built-in lints: `self-wakes` (a task waking itself
more than 50% of the time), `lost-waker` (dropped without being woken) and
`never-yielded` (never returned `Pending`; about one second threshold)
([warnings.rs](https://docs.rs/crate/tokio-console/latest/source/src/warnings.rs)).
The lints have known false positives (a completed task reported as having lost
its waker, [tokio-rs/console#345](https://github.com/tokio-rs/console/issues/345)).

It sees *tasks*, not actors: a Spawned async actor is one Tokio task whose name,
mailbox and current message the console does not know (our reading; naming a
task needs `tokio::task::Builder`, itself behind `tokio_unstable`). Thread and
Blocking backends are invisible to it.

### kameo console

kameo (a Rust actor crate) ships a `console` feature: per-actor monitors record
counters, status and mailbox depth in a global registry, and `serve` exposes
the snapshots over TCP to a console client
([kameo::console](https://docs.rs/kameo/latest/kameo/console/index.html)).
This is the closest Rust precedent for `spawned-top`. We did not run it.

### OpenTelemetry context propagation

"Propagation is the mechanism that moves context between services and
processes": the sender injects context into a carrier and the receiver extracts
it; the default propagator uses W3C TraceContext (`traceparent`) and Baggage
carries arbitrary key-value pairs
([context propagation](https://opentelemetry.io/docs/concepts/context-propagation/)).
For messaging, "a producer SHOULD attach a message creation context to each
message", and *span links*, not parent-child, are the default way to connect a
consumer's span to the producer's, because a span has one parent and a batch has
many producers. Making the creation context the parent of a "Process" span is
"NOT RECOMMENDED" by default. These messaging conventions are still marked
*Development*
([messaging spans](https://opentelemetry.io/docs/specs/semconv/messaging/messaging-spans/)).

In Rust, `tracing` spans do not cross a channel by themselves: a span entered by
the sender is not current inside the receiver's handler unless the message
carries it. That is the gap rank 28 names (our reading of how `tracing` works;
no Spawned measurement).

### Akka Insights (Lightbend Telemetry, "Cinnamon")

Commercial instrumentation for Akka, now documented as Akka Insights. Actor
metrics include mailbox size, stash size, mailbox time and processing time, with
thresholds that raise events when a queue grows too large or a message waits or
runs too long; mailbox and stash sizes are sampled "at most once every 1 second
by default"
([actor instrumentation](https://doc.akka.io/libraries/akka-insights/current/instrumentations/akka/actors.html)).
Actors are selected by path, so instrumentation is configured per subtree rather
than for every actor. Use requires an Akka subscription and credentials, and from
Cinnamon 2.18 each release supports a single Akka release (from the docs'
introduction pages; we did not find an end-of-life notice).

### Orleans

Orleans 7+ publishes `System.Diagnostics.Metrics` instruments under the
`Microsoft.Orleans` meter, exported through OpenTelemetry: activations
(`orleans-catalog-activations`, created, destroyed, failed), messaging
(dispatcher received/processed, sent/dropped/expired/rejected), app request
latency and timeouts, `orleans-scheduler-long-running-turns`, watchdog health
checks, storage latency, reminder tardiness and stream queue ages. Distributed
tracing is enabled with `AddActivityPropagation`, which carries .NET `Activity`
context across grain calls
([Orleans observability](https://learn.microsoft.com/en-us/dotnet/orleans/host/monitoring/)).
The community OrleansDashboard (activations, per-grain method rates, errors and
latency, silos, reminders) is no longer maintained in OrleansContrib; it points
to the dashboard in the official Orleans packages
([OrleansDashboard](https://github.com/OrleansContrib/OrleansDashboard)).

### Comparison

| | Erlang observer + recon | tokio-console | kameo console | Akka Insights | Orleans | Spawned today |
|---|---|---|---|---|---|---|
| Unit observed | Process | Task, resource | Actor | Actor (by path) | Grain, silo | Process-wide aggregate |
| Live attach | Yes (distribution) | gRPC | TCP | Via backends | Via OTel backends | No |
| Mailbox depth | Per process | No | Per actor | Sampled per actor | Per queue (streams) | Sum, plus per-ref `mailbox_depth()` |
| Long-turn signal | Reductions | `never-yielded` lint | Not checked | Processing-time threshold | `long-running-turns` | Debug-only `WarnOnBlocking` |
| Cross-actor tracing | Trace BIFs | No | Not checked | Yes (commercial) | `Activity` propagation | No |
| Production-safety limits | recon rate limits | Retention period | Not checked | Sampling | OTel sampling | Sampled timing (`timing_sample_every`) |

## Who uses it

- Observer and recon are the standard BEAM tools; Hebert's *Erlang in Anger*
  is built around recon (not re-checked for this note).
- Orleans' meters are documented by Microsoft for its own runtime; no named
  production users are claimed here.
- **Ethlambda** serves Spawned's metrics on `/metrics` (spawned 0.6.0, open PR
  [lambdaclass/ethlambda#782](https://github.com/lambdaclass/ethlambda/pull/782)):
  a 3-node devnet read showed handler p99 around 800 ms from the block-building
  tick and tens-of-ms queue-wait p99 behind it, with the mailbox draining to zero
  between ticks.

### The `_max` exposition bug

The Ethlambda v0.7 upgrade branch listed a "Prometheus `_max` exposition bug" as
spawned feedback ([ethlambda#783](https://github.com/lambdaclass/ethlambda/pull/783),
"Known follow-ups"). `render_prometheus` wrote `# TYPE <name> histogram` and then,
inside the same family, an undeclared `<name>_max` sample after `_bucket`,
`_count` and `_sum`. The Prometheus text format defines a histogram's samples as
`x_bucket`, `x_sum` and `x_count`, and requires all lines of one metric to form
one group with its `TYPE` line first
([exposition formats](https://prometheus.io/docs/instrumenting/exposition_formats/)).
The exact failure seen in Ethlambda (rejected scrape, dropped series or a lint
warning) is **unverified**: the PR text names the bug without details.

**Fixed on main** (commit 342b707b, "address Ethlambda v0.7 canary lifecycle and
metrics feedback"): each maximum is now declared as its own gauge family with the
series name preserved, and CI pipes the metrics example through
`scripts/check-metrics-exposition.py` (the Prometheus Python client parser),
keeping the old undeclared-max output as a negative control
([CHANGELOG](../../CHANGELOG.md), [testing](../TESTING.md)). The lesson for rank 29:
every new metric family ships with a strict-parser check.

## What it means for Spawned

- **Keep the exposition parser check** ([rank 29](../../ROADMAP.md#ranked-backlog)):
  the `_max` bug is fixed; every new metric family (cohort metrics, turn
  counters) should pass `scripts/check-metrics-exposition.py` with a negative
  control, as the fix did.
- **`spawned-top` ([rank 30](../../ROADMAP.md#ranked-backlog)) should copy
  observer's columns and recon's safety rules.** Columns: id, type name, parent
  and links, mailbox depth, status, current message type and turn age, work
  counters (rank 8a). Rules from recon: never copy a mailbox to inspect it, rank
  by a sliding window (`proc_window`) rather than totals, rate-limit any trace,
  and read actor state only between turns (as observer's `sys:get_state`).
  kameo's console (a registry of per-actor monitors served over TCP) is the
  closest Rust design to compare against.
- **tokio-console integration is cheap but partial.** Naming each actor's task
  with its type (behind `tokio_unstable`) and emitting a `tracing` span per turn
  would make actors legible in tokio-console and its `never-yielded` lint a free
  stall signal for async actors. It cannot see Thread or Blocking actors or
  mailboxes, so it complements `spawned-top`, not replaces it. Record as a note on
  rank 30; measure the overhead before enabling by default.
- **Causal tracing ([rank 28](../../ROADMAP.md#ranked-backlog)) should follow the
  OpenTelemetry messaging model**: capture the sender's span context at
  enqueue, carry it in the envelope (as metrics already carry an observation),
  and open the handler's span *linked* to it; use parent-child only for
  `request`, where one caller awaits one reply. Keep it out of the message type
  so handlers stay ordinary Rust, and bound cost with sampling at the ingress.
- **Flight recorder ([rank 9](../../ROADMAP.md#ranked-backlog)).** Observer's
  per-process message view and Akka's mailbox-time thresholds both point to the
  same minimal record: message type, enqueue and start time, outcome. Keep it
  per actor and bounded; `spawned-top` can read it.
- **Metrics ([rank 29](../../ROADMAP.md#ranked-backlog)).** Orleans' meters
  suggest what is missing: a long-running-turn counter (rank 8 can increment
  it), activation created/destroyed/failed counts per role, and oldest-message
  age. Akka's per-path selection is the model for bounded per-role labels.

## Sources

- [Erlang observer user's guide](https://www.erlang.org/doc/apps/observer/observer_ug.html)
- [recon README](https://github.com/ferd/recon), [recon module](https://ferd.github.io/recon/recon.html), [recon_trace](https://ferd.github.io/recon/recon_trace.html)
- [console-subscriber README](https://github.com/tokio-rs/console/blob/main/console-subscriber/README.md), [tokio-console warnings.rs](https://docs.rs/crate/tokio-console/latest/source/src/warnings.rs), [tokio-rs/console#345](https://github.com/tokio-rs/console/issues/345)
- [kameo console module](https://docs.rs/kameo/latest/kameo/console/index.html)
- [OpenTelemetry context propagation](https://opentelemetry.io/docs/concepts/context-propagation/), [messaging spans semantic conventions](https://opentelemetry.io/docs/specs/semconv/messaging/messaging-spans/)
- [Akka Insights actor instrumentation](https://doc.akka.io/libraries/akka-insights/current/instrumentations/akka/actors.html), [Akka Insights introduction](https://doc.akka.io/libraries/akka-insights/2.20.x/introduction/introduction.html)
- [Orleans observability (Microsoft Learn)](https://learn.microsoft.com/en-us/dotnet/orleans/host/monitoring/), [OrleansDashboard](https://github.com/OrleansContrib/OrleansDashboard)
- [Prometheus exposition formats](https://prometheus.io/docs/instrumenting/exposition_formats/)
- [lambdaclass/ethlambda#782](https://github.com/lambdaclass/ethlambda/pull/782), [lambdaclass/ethlambda#783](https://github.com/lambdaclass/ethlambda/pull/783)
