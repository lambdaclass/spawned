# Actor test kits: Akka, Elixir, Erlang, Orleans and Rust crates

Research note, 2026-11-09. Informs rank 6 (test kit), rank 20 (ordering
exploration) and the [`spawned-test` crate](../../ROADMAP.md#testing-crate-spawned-test)
of the [roadmap](../../ROADMAP.md#ranked-backlog). Exploration tools for the
framework itself (Loom, Shuttle, Coyote, simulators) are in
[systematic testing](systematic-testing.md); this note covers what users get to
test *their* actors.

## What it is

An actor test kit gives application authors four things: a way to stand in for
a dependency (mock or stub actor), a way to observe what an actor sends (probe),
waits that fail with a report instead of hanging, and control over time. Mature
kits add a fifth: running one actor's handler synchronously, with no runtime, to
unit-test its logic. Erlang adds a sixth that no other ecosystem ships as a
standard tool: systematic exploration of message interleavings (Concuerror).

## How it works

### Akka (classic and typed)

- **Classic TestKit.** A `TestKit` owns a `testActor` that collects messages for
  `expectMsg` ("must be received within the specified time", default 3 s via
  `akka.test.single-expect-default`) and `expectNoMessage`; `within` bounds a
  block by a deadline. A `TestProbe` is an extra observer that can watch actors,
  reply, forward and run an AutoPilot. `TestActorRef` runs an actor on the
  `CallingThreadDispatcher`, but the docs recommend "traditional behavioral
  testing" instead and warn that the dispatcher can deadlock on blocking code
  ([classic testing](https://doc.akka.io/libraries/akka-core/current/testing.html)).
- **Typed ActorTestKit (asynchronous).** Runs a real `ActorSystem`; `TestProbe`
  offers `expectMessage` and `expectMessageType` and can be passed wherever a
  `RecipientRef` is expected. `Behaviors.monitor` wraps a mock behavior and
  forwards each message to a probe. **`ManualTime`** replaces the scheduler:
  `timePasses` advances the clock and `expectNoMessageFor` asserts silence over
  a virtual interval. `LoggingTestKit` asserts log events
  ([async testing](https://doc.akka.io/libraries/akka-core/current/typed/testing-async.html)).
- **Typed BehaviorTestKit (synchronous).** Runs a behavior "synchronously on the
  testing thread": `run(msg)` handles one message, effects such as `Spawned`,
  `Watched` and `Scheduled` are recorded for assertions, and `TestInbox` /
  `childInbox` capture what was sent. Limits stated by the docs: no scheduler,
  no supervision, interactions with other actors must be stubbed, and
  asynchronous callbacks cannot be observed; for those, use the async kit
  ([sync testing](https://doc.akka.io/libraries/akka-core/current/typed/testing-sync.html)).
  Akka's guidance: synchronous for one behavior in isolation, asynchronous for
  interactions ([testing](https://doc.akka.io/libraries/akka-core/current/typed/testing.html)).

### Elixir: ExUnit and Mox

- ExUnit's `assert_receive` waits for a message, with a 100 ms default timeout
  (`assert_receive_timeout`), and `refute_receive` asserts none arrives within a
  window ([assertions](https://ex-unit.hexdocs.pm/ExUnit.Assertions.html)).
  `start_supervised/2` starts a process under a per-test supervisor that "is
  guaranteed to exit before the next test starts"
  ([callbacks](https://ex-unit.hexdocs.pm/ExUnit.Callbacks.html)).
- Mox: "No ad-hoc mocks. You can only create mocks based on behaviours". `expect`
  counts and verifies calls, `stub` allows any number, `verify_on_exit!` checks at
  the end of the test. Expectations are private to the test process by default,
  so tests stay `async: true`; `allow/3` (and `$callers` for Tasks) lets another
  process use them ([Mox](https://mox.hexdocs.pm/Mox.html)).

### Erlang: Common Test, PropEr and Concuerror

- **Common Test** runs suites for white-box testing of OTP code (calling target
  APIs directly) and black-box testing of whole systems, with HTML logs and
  parallel sessions ([introduction](https://www.erlang.org/doc/apps/common_test/introduction.html)).
- **PropEr** `proper_statem` generates command sequences against a model;
  `parallel_commands` runs a sequential prefix, then two command lists in
  parallel, and `run_parallel_commands` checks the results match *some*
  serialization, reporting `no_possible_interleaving` otherwise. The interleaving
  "depends on the Erlang scheduler", so the docs suggest inserting
  `erlang:yield/0` to expose races
  ([proper_statem](https://proper-testing.github.io/apidocs/proper_statem.html)).
- **Concuerror** is a "stateless model checking tool for Erlang programs". It
  runs the test's processes as if on one scheduler, records operations whose
  outcome depends on scheduling (receives, registry and ETS operations), finds
  the pairs that really race, and reverses them to explore new schedules:
  "stateless model checking with dynamic partial order reduction". It reports
  deadlocks and race errors ([concuerror.com](https://concuerror.com/)). Its
  authors published optimal DPOR (POPL 2014) and applied it to Corfu chain repair
  (IFM 2017) ([publications](https://concuerror.com/publications/)). The last
  GitHub release is 0.21.0 (November 2020); the repository was still receiving
  pushes in 2026 (GitHub API, 2026-11-09).

### Orleans

`Microsoft.Orleans.TestingHost` starts silos in-process. In Orleans 9 and 10,
`InProcessTestCluster` is recommended (delegate-based configuration, shared
mock services, silos added, stopped and restarted during a test); `TestCluster`
remains for multi-process runs and Orleans 7/8. Unit tests can instead mock the
runtime (for example `GrainFactory` with Moq), with limits "around scheduling
reentrancy and serialization"; the community OrleansTestKit avoids some of them
([unit testing](https://learn.microsoft.com/en-us/dotnet/orleans/implementation/testing)).

### Rust actor crates

- **actix**: `#[actix::test]` runs an async test inside an actix `System`, and
  `actors::mocker::Mocker` "is able to wrap another actor and accept all the
  messages the wrapped actor can", answering through a closure
  ([actix](https://docs.rs/actix/latest/actix/),
  [mocker](https://docs.rs/actix/latest/actix/actors/mocker/index.html)).
  `actix-actor-expect` is a third-party crate that configures a Mocker
  ([docs](https://docs.rs/actix-actor-expect)).
- **ractor**: we found no public test module. Its own tests use `call_t!` and
  a crate-internal `periodic_check` helper (ractor 0.15.12 sources on docs.rs);
  absence of a user-facing kit is our reading, not a statement by the project.
- **kameo** (0.22.2): no testing module; `tokio-test` is only a dev-dependency
  ([docs.rs](https://docs.rs/kameo/latest/kameo/)).

### Comparison with `spawned-test` phase A (#198)

| Capability | Akka typed | Elixir/Erlang | Orleans | actix | `spawned-test` phase A |
|---|---|---|---|---|---|
| Mock a dependency | `Behaviors.monitor` + probe | Mox (behaviour-based) | Moq on runtime, TestKit | `Mocker` | `Mock`: closure-backed actor for every message type, so `ActorRef<Mock>` satisfies any `#[protocol]`; unmocked messages fail loudly |
| Probe sent messages | `TestProbe` | `assert_receive` | Not built in | No | `Probe<M>` recipient with `expect`, `expect_no_message`, `received` |
| Bounded waits | `within`, default timeouts | `assert_receive` timeout | Test framework | No | `settle`, `exit_within`, `join_within`, `wait_until`, `wait_for_generation` |
| Per-test cleanup | `shutdownTestKit` | `start_supervised` | Cluster dispose | `System` scope | `TestSystem`, `shutdown_system` |
| Fault injection | Stop/kill via kit | `Process.exit/2` | Stop silos | No | `faults::exit` (reason-shaped through a link), `restart_child`, mock delay/discard/crash |
| Manual time | `ManualTime` | No (Mox only) | Not built in | No | Tokio `start_paused` works for async actors (waits use the Tokio clock); no control for thread actors |
| Synchronous handler test | `BehaviorTestKit` | Call callbacks directly | Instantiate grain with mocks | No | No |
| Mock a concrete reference | n/a (typed refs are `ActorRef[T]` of messages) | n/a (pids) | Mock `GrainFactory` | `Mocker` replaces actor type | No: a field typed `ActorRef<B>` cannot hold a `Mock` |
| Interleaving exploration | No | Concuerror, PropEr parallel | No | No | No (rank 20) |

Phase A facts are from `testing/src/lib.rs`, `tasks.rs`, `probe.rs` and
`faults.rs` in `testing/` (PR #198, merged on main 2026-11-09, unreleased).

## Who uses it

- Akka's and Orleans' kits are the documented default for their users; we did
  not verify named production users.
- Concuerror's Corfu chain-repair study (IFM 2017) is the documented industrial
  application we found.
- Ethlambda hand-stubs protocol actors in tests (`RecordingChain` in
  `column_checks.rs`), which is the motivating client evidence for rank 6
  ([roadmap rank 6](../../ROADMAP.md#ranked-backlog)).

## What it means for Spawned

Phase A already matches the core of Akka's async kit and Mox (mocks, probes,
bounded waits, per-test cleanup, fault injection). The gaps, in suggested order:

- **Synchronous behavior test kit** ([rank 6](../../ROADMAP.md#ranked-backlog)
  remaining work). A `BehaviorTestKit`-style harness: build an actor, call
  `started` and a handler with a test `Context`, and return the reply plus
  recorded effects (sends to probes, `send_after` timers, stop requests,
  links/monitors requested) without starting a runtime. It suits pure handler
  logic and fails fast; Akka's stated limits (no supervision, stubbed peers, no
  async callbacks) apply and should be documented the same way. Needs a
  constructible test `Context`, which is a core change.
- **Manual time** (rank 6 remaining work, "controllable time for timers").
  Tokio's paused clock already drives async actors' timers and the kit's waits;
  document it as the supported path for `tasks`. For `threads` and the Thread
  backend, a `ManualTime` equivalent needs timers to read a clock owned by the
  framework, which is the same seam as the simulation backend and `ctx.now()`
  (ranks 20-22). Do not build two clocks.
- **Concrete `ActorRef<B>` mocking** (rank 6 remaining work). actix solves this
  by letting `Mocker<A>` stand in for actor type `A`; Spawned's equivalent is the
  deferred `Envelope` downcast hook. Until then, guide users to hold protocol
  references (`XRef`) or `Recipient<M>`, as Mox insists on behaviours.
- **Concuerror as prior art for `explore`** ([rank 20](../../ROADMAP.md#ranked-backlog)).
  Concuerror is the actor-world precedent for exactly rank 20's design: schedule
  as if on one scheduler, branch only on operations that race (receives and
  registry updates), and use DPOR to skip equivalent orders. Its "modelling
  distributed Erlang within a single node" paper is relevant to rank 22a. Cite it
  next to Coyote.
- **PropEr parallel testing** maps to the `request_histories` linearizability
  oracle and the planned Porcupine-style checker
  ([testing to add](../../ROADMAP.md#testing-to-add)): generate a sequential
  prefix plus two parallel command lists against one actor, then check a
  serialization exists.
- **Log assertions** (Akka `LoggingTestKit`) are cheap with
  `tracing-subscriber` test layers; optional, only if a client asks.

## Sources

- Akka: [classic testing](https://doc.akka.io/libraries/akka-core/current/testing.html), [typed testing](https://doc.akka.io/libraries/akka-core/current/typed/testing.html), [async testing](https://doc.akka.io/libraries/akka-core/current/typed/testing-async.html), [sync testing](https://doc.akka.io/libraries/akka-core/current/typed/testing-sync.html)
- Elixir: [ExUnit.Assertions](https://ex-unit.hexdocs.pm/ExUnit.Assertions.html), [ExUnit.Callbacks](https://ex-unit.hexdocs.pm/ExUnit.Callbacks.html), [Mox](https://mox.hexdocs.pm/Mox.html)
- Erlang: [Common Test introduction](https://www.erlang.org/doc/apps/common_test/introduction.html), [proper_statem](https://proper-testing.github.io/apidocs/proper_statem.html), [Concuerror](https://concuerror.com/), [Concuerror publications](https://concuerror.com/publications/), [Concuerror repository](https://github.com/parapluu/Concuerror)
- [Orleans unit testing (Microsoft Learn)](https://learn.microsoft.com/en-us/dotnet/orleans/implementation/testing)
- Rust: [actix](https://docs.rs/actix/latest/actix/), [actix mocker](https://docs.rs/actix/latest/actix/actors/mocker/index.html), [actix-actor-expect](https://docs.rs/actix-actor-expect), [ractor test sources](https://docs.rs/crate/ractor/latest/source/src/macros/tests.rs), [kameo](https://docs.rs/kameo/latest/kameo/)
- [spawned-test phase A, PR #198](https://github.com/lambdaclass/spawned/pull/198)
