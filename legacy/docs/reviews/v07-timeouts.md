# v0.7 timeout configuration and examples

Unreleased work following the [supervision review](v07-supervision.md).
The five-second default, ordinary handler signatures and ActorRef layout are
unchanged. Configuration is additive:

- `#[protocol(timeout = Duration expression)]` sets one protocol's default.
- `#[request_timeout(Duration expression)]` overrides a request method.
- `actor.with_request_timeout(limit).method(...)` overrides both through a cloned
  `RequestOptions` handle. It can also use the generated protocol-ref converter.

Manual protocol fakes acquire no new required methods. Configure concrete handles
before type erasure. A native call blocks immediately; an async call enqueues
immediately and starts its response timer on first poll, preserving existing
semantics. Timeout/drop does not cancel accepted work or roll back effects.

Timeout expressions are evaluated before message argument moves. A hygienic local
binding avoids capturing identically named user parameters. Wrapper accessors are
associated functions rather than methods, so protocols can still call methods
named `timeout` and `actor_ref`. These cases have explicit regressions.

Validation on fede used CPU mask 2,3,4,5, nice 10, one build job and a sampled
2 GiB RSS guard. Rust 1.93.0 workspace, metrics and native test suites passed;
strict default/native documentation and Clippy passed. All-feature target checking
also passed. Seven timeout regressions cover default/method/handle precedence,
virtual time beyond five seconds, source-compatible fakes, type erasure, cfg,
owned arguments and method-name collisions. Existing active cfg cases now also
exercise configured handles; three new trybuild fixtures reject invalid attributes.
The native CI inventory requires ten specific tests to prevent silent gating.

Five new asserted examples run in CI: timeout configuration/outcomes,
streams/recipients, native lifecycle signals, registrations/fakes and coordinated
shutdown. The isolated synchronous fixture exercises the new configuration and
still has no Tokio package in its normal/build dependency graph. The
[client migration guide](../../../docs/MIGRATION.md#preparing-for-07) covers dependency order,
stale references, snapshot/persistence before closure and report deadlines.

AGY returned PASS for the API review and for the associated-function follow-up.
[Evidence](../../verification/evidence/v07-timeouts/) retains the review packets,
results, test logs and source hashes compared with the server.

## Client compatibility and remaining release gate

An isolated Ethlambda checkout at `c3f5f566cb3a61e3d388c66ce21794c18a6618ce`
passed `cargo check --locked --offline --release -p ethlambda` against the candidate
with its pinned Rust 1.97.1 compiler. Application source was unchanged; the
workspace selected the local Spawned candidate. Offline resolution changed ten
off-platform lock entries; none is in the Linux normal/build dependency graph.
The exact resolution changes and lock hash are retained in the evidence.
This was a compile check, not a running-node or performance result.

The client's import replay bypasses mailboxes and its era replay targets state
transition, so those commands alone cannot establish the actor-runtime release
gate. A real node canary or actor-backed replay remains required. The canary's
SSH/deployment target has been requested; no production service was changed,
release was published, or version/tag was created. Deferred replies/pipe-to-self
and Tokio-free execution groups/supervision remain outside this milestone.
