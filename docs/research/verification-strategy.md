# Research: verification and testing strategy

Date: 2026-11-08. Status: proposal, not a commitment. Inspected revision:
`0ef612dd21e8e8a401422eb06e1f31f5197cd4b2` (v0.6.0). Companion to
[framework ideas](framework-ideas.md); see also [TESTING.md](../TESTING.md).

## Where we are

At this revision the repository uses proptest-generated lifecycle scenarios
(`concurrency/tests/actor_scenarios.rs`), Loom models of cancellation and the
ready gate (`concurrency/tests/ready_gate_loom.rs`), cleanup regressions and
nightly churn tests. CI does not run Miri, ThreadSanitizer, Shuttle or mutation
testing, and there is no formal model of the lifecycle. Generated seeds
reproduce operations, not thread schedules.

Several recent fixes were lifecycle races found after the fact: PR #180 (liveness
after completion closes without an exit reason), PR #182 (requests retained by
mailbox publication racing shutdown) and PR #185 (stop before dispatching newly
received idle work). Since this was drafted, execution groups and OneForOne
supervision have landed on main (`12baea41`), which enlarges that state space
now rather than later: the model should cover the implemented supervision
contract, not only the pre-supervision lifecycle.

## Layers

No single tool covers the problem. Each layer catches a different class of bug.

| Layer | Tool | Catches | Status |
|---|---|---|---|
| Design | **TLA+** (or P) | Holes in the lifecycle rules: link/monitor/stop/exit races; later, restart and handoff logic | New |
| Spec ↔ code | **Trace validation** | The implementation doing something the spec forbids | New; the key missing piece |
| Primitives | **Loom** | Atomic and memory-ordering bugs in mailbox, ready gate, wakeups (exhaustive, small tests) | Exists; expand |
| Whole runtime | **Shuttle** | Scheduling races in multi-actor tests too large for Loom (randomized schedules with probabilistic guarantees) | New |
| User level | **Deterministic simulation** | Bugs in users' actor systems, reproducible from a seed | Planned ([framework ideas](framework-ideas.md) #15) |
| Memory safety | **Miri**, **ThreadSanitizer** | Undefined behavior and data races, especially once unsafe code (e.g. generated envelopes) appears | New; cheap |
| Test quality | **cargo-mutants** | Tests that still pass when the code is broken | New; periodic |

## Formal model

### What to model first

The current lifecycle contract, before supervision:

- exit reasons (`Normal`, `Shutdown`, `Panic`, `Kill`);
- monitors and demonitor;
- bidirectional links, `trap_exit`, `start_linked` registration before dispatch;
- stop, cancellation and completion while a handler is running;
- request acceptance vs. rejection around shutdown (the PR #182 class);
- delivery of exit signals relative to queued messages.

Sources for the rules: [API guide](../API-GUIDE.md#links-and-trapped-exits) and
the [PR 166 lifecycle review](../reviews/pr-166-links.md). Keep the model small:
three to four actors, bounded mailboxes, bounded message counts.

Properties to check, for example:

- an actor linked to an abnormally exiting non-trapping peer eventually stops;
- every accepted request eventually gets a reply or `ActorStopped`, never neither;
- no message is dispatched after stop is observed by the run loop;
- monitor notifications are delivered exactly once per monitor.

Then add the implemented OneForOne contract (restart modes, rolling limits,
ordered startup and reverse shutdown, instance-bound replacement lookup,
discarded queued work) and check it against the existing supervision failure
tests via trace validation. Specify OneForAll and RestForOne in the model before
implementing them. Per-key entity handoff ([framework ideas](framework-ideas.md) #6)
is another good candidate.

### TLA+ or P

- **TLA+**: larger community, mature checkers (TLC, Apalache), well suited to a
  lifecycle protocol.
- **P**: designed for systems of communicating state machines, which is what
  actors are; used at AWS; has runtime trace checking (PObserve).

- **Stateright**: a model checker written in Rust, so the model is ordinary
  Rust code and could share types with the implementation. Smaller community and
  fewer tools than TLA+.

Any of these works. Default to TLA+ unless someone on the team already knows P,
or the team strongly prefers keeping the model in Rust (then Stateright).

Questions the model should answer explicitly, beyond the properties above:

- per-sender ordering between a message an actor sent and its subsequent exit
  or `Down` notification, including messages sent by its helper tasks;
- whether a priority lane for control commands or trapped exits can violate that
  ordering;
- for supervision: whether references are bound to an instance or to a stable
  slot across restarts, and what happens to queued messages and in-flight
  requests on restart.

### Trace validation

A spec checked only by its own model checker drifts from the code. To keep them
tied:

1. Behind a test-only feature, emit structured lifecycle events from the Rust
   code: link, unlink, monitor, exit delivered, stop observed, request accepted,
   request rejected, reply sent.
2. Collect traces from the existing proptest scenarios, nightly churn tests and,
   later, Shuttle runs.
3. Check each trace against the spec (TLC can check a recorded trace as a
   constrained behavior).

Every existing test run then also checks the spec, and the spec becomes the
definition of correct behavior for the implementation.

## Code-level tools

- **Loom.** Keep for small primitives. Add models for mailbox publication versus
  shutdown and for thread-mode explicit wakeup (PR #168).
- **Shuttle (framework internals only).** Route `spawned-rt` synchronization
  through a Shuttle-compatible layer in a test feature, then run multi-actor
  scenarios (link chains, stop during request, monitor storms) under random and
  PCT schedules. Loom cannot scale to these. Shuttle explores every lock and
  atomic inside mailboxes, oneshots and the link and monitor tables. That is right
  for finding races in Spawned itself, and wrong for testing *users'* actor
  systems: the state space explodes before it reorders even a few messages.
- **Actor-level exploration (for users).** Treat one handler turn as one atomic
  step and control delivery at the mailbox boundary. Messages to different
  actors commute, so only deliveries racing at the same actor, or racing a timer
  or exit, branch the schedule (actor-level partial-order reduction, as in
  P#/Coyote). This is simpler and much faster than Shuttle for user tests, and
  failures shrink to readable delivery lists. It is the planned
  `spawned::sim::explore`, not a replacement for Shuttle on internals. (Split
  suggested by agy's review.)
- **Miri.** Run the unit tests that do not need real I/O. Cheap now; essential
  before any unsafe envelope or reply-slot optimization.
- **ThreadSanitizer.** Nightly toolchain run over thread-mode tests and the
  scenario suite.
- **cargo-mutants.** Weekly or before releases, scoped to `concurrency/src`
  lifecycle code; review surviving mutants as missing tests.

## Order

1. Miri and ThreadSanitizer in CI (hours).
2. TLA+ spec of the current lifecycle (before supervision design).
3. Trace validation against existing scenario and churn tests.
4. Shuttle for whole-runtime scenarios.
5. cargo-mutants periodically.
6. Deterministic simulation as a user-facing feature; layers 1–5 are internal
   quality.

## Acceptance

- The spec checks the listed properties at the chosen bounds, and reintroducing
  the PR #182 and PR #185 bugs in the model produces counterexamples.
- Trace validation runs in CI on a sample of scenario seeds and fails on a
  deliberately injected ordering violation.
- Shuttle finds a reintroduced known race within a documented iteration budget.
- Miri and TSAN runs are green or have documented, justified exclusions.

## Sources

- [TLA+](https://lamport.azurewebsites.net/tla/tla.html)
- [P language](https://p-org.github.io/P/)
- [Loom](https://docs.rs/loom/latest/loom/)
- [Shuttle](https://github.com/awslabs/shuttle)
- [Miri](https://github.com/rust-lang/miri)
- [ThreadSanitizer in Rust](https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html)
- [cargo-mutants](https://mutants.rs/)
