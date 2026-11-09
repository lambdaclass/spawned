# Security and lifecycle review of PR168

Reviewed external head `4b5a04ec0bd144d2c553db15eba4eeafd5902905` against
its merge base `579533cb`, then integrated the useful change onto hardened PR166
(main `2f066d19`). The original patch was not merged unchanged.

The original net diff changes three Rust files: `child_handle.rs`,
`tasks/actor.rs`, and `threads/actor.rs`. It adds no dependency, build script,
workflow, unsafe code, filesystem access or network operation. The useful change
replaces the synchronous thread actor's 100 ms mailbox poll with blocking receive
and an explicit private wakeup. This is not a change to the async task scheduler
or the task mode's `Backend::Thread`.

## Findings addressed before integration

1. **Missed cancellation wakeup after integrating links.** The original patch
   sends Shutdown only from `Context::stop` and the child-handle stop closure.
   Linked exits and framework cancellation also cancel tokens. With polling
   removed, those routes can leave an idle thread blocked forever. The integrated
   code registers one `CancellationToken::on_cancel` callback at mailbox creation,
   before links or dispatch, so all cancellation routes use the same wakeup.
2. **Repeated stop calls grow the control queue.** The original patch adds one
   Shutdown item per stop call. A busy handler allows those items to accumulate
   without bound. The registered callback is `FnOnce`, drained under the token's
   callback mutex: the framework sends at most one Shutdown per actor, even with
   concurrent or repeated stops. Ordinary user messages remain unbounded by design.

These are availability/resource-exhaustion concerns. No evidence of malicious
code or additional security blocker was found in the reviewed changes. Static
review and tests do not prove the absence of every possible defect or schedule.

## Preserved contract

- Shutdown is a module-private enum variant, never an ordinary typed message;
  users cannot forge it through `send` or `request`.
- Stop still requires an actor lifecycle capability. A running handler finishes
  cooperatively; blocking Rust code is not forcibly interrupted.
- A stop observed after receive still wins over dispatch. No queue drain before
  `stopped()` or premature completion publication was introduced.
- Existing stopped-hook, outgoing-monitor cleanup, undelivered-message destruction
  and completion ordering stays covered by the prior regressions.
- The callback invokes no business handler and holds no cancellation/link mutex
  while sending. It releases its captured sender when cancellation fires.
- Contexts already own senders; this does not add a promise of automatic shutdown
  when the last external `ActorRef` is dropped. No public API changes are needed.

## Validation

Source `21446fd0` passed on fede. Its runtime and tests are identical to the
merged [main source `b4c3632f`](https://github.com/lambdaclass/spawned/commit/b4c3632f6bf8854d7ad5409c8fb96cb301187c2e);
subsequent changes only add the review and logs:

```sh
cargo test --locked -p spawned-concurrency --all-features --lib -- --test-threads=1
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
```

There are 148 passing library tests. New coverage checks a real blocking receive
boundary; cancellation before/after callback registration; 16 concurrent callers
cancelling 256 times each with exactly one control item; Context/ChildHandle/raw
cancellation/linked-panic routes; and 256 queued requests behind an active handler
with 4096 repeated stop calls. Existing mixed-mode runtime-abort, untrappable Kill,
startup, panic, monitor, macro and message-drop regressions also pass. Logs are in
[pr-168-evidence](pr-168-evidence/).

An independent Agy static review of the original diff, final integration and
supporting lifecycle/token source found no blockers. That review did not execute
the test suite; the execution evidence above is from fede.
