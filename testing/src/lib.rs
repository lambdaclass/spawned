//! Test kit for [Spawned](https://github.com/lambdaclass/spawned) actors.
//!
//! Use it as a dev-dependency, the way `tokio-test` is used with Tokio.
//! First published with Spawned v0.7.1.
//!
//! ```toml
//! [dev-dependencies]
//! spawned-test = "0.7.1"
//! ```
//!
//! - **Mocks** ([`tasks::Mock`], [`threads::Mock`]): closure-backed actors that
//!   handle every message type, so `ActorRef<Mock>` implements every
//!   `#[protocol]` and converts to any protocol reference (`XRef`). Unmocked
//!   messages fail loudly with the type and the rule to add.
//! - **Probes** ([`tasks::Probe`], [`threads::Probe`]): a `Recipient<M>` that
//!   records what it is sent, with `expect`, `expect_no_message` and `received`.
//! - **Harness helpers**: [`tasks::settle`], [`tasks::exit_within`],
//!   [`tasks::wait_for_generation`], [`tasks::shutdown_system`] and
//!   [`tasks::TestSystem`] turn hangs into test failures with a report.
//! - **Fault injection**: [`faults::exit`] sends an exact exit signal with a
//!   chosen [`ExitReason`](spawned_concurrency::ExitReason)
//!   (`ChildHandle::exit`; `Kill` ends the target `Killed`, `Normal` is
//!   ignored by a non-trapping target, a trapping target gets an `Exit`);
//!   [`tasks::restart_child`] crashes a supervised child and returns its
//!   replacement; mocks delay, discard or crash on chosen messages.
//!
//! - **Linearizability** ([`linearizability`]): record concurrent request
//!   histories and check them against a sequential model with a
//!   Porcupine-style checker (memoised search, per-key partitions, pending
//!   requests, failure explanations and an HTML timeline).
//!
//! Everything here uses the public `spawned-concurrency` API. Mocks stand in
//! for a dependency reached through a protocol reference or a `Recipient<M>`;
//! a field typed as a concrete `ActorRef<B>` cannot hold a mock (see the guide).
//!
//! The `tasks` module needs the default
//! `async` feature; with `default-features = false` the crate is Tokio-free and
//! provides the `threads` helpers.
#![forbid(unsafe_code)]
// The docs point at `tasks` items, which a Tokio-free build does not compile.
#![cfg_attr(not(feature = "async"), allow(rustdoc::broken_intra_doc_links))]

#[macro_use]
mod shared;

pub mod faults;
mod inbox;
pub mod linearizability;
mod probe;
mod rules;
#[cfg(feature = "async")]
pub mod tasks;
pub mod threads;

pub use inbox::{Outcome, Received};

/// A `PendingReply` and its receiver for calling a manual
/// `Handler::handle` directly in a test, without an actor:
///
/// ```ignore
/// let (reply, rx) = spawned_test::reply_pair::<u64>();
/// actor.handle(GetCount, reply, &ctx).await;
/// assert_eq!(rx.await.unwrap(), 3);
/// ```
///
/// The reply has no deadline. Await (or `recv`) the receiver only after
/// driving every turn the reply depends on: a deferred reply answered by a
/// later message is never answered while the test waits for it first.
pub fn reply_pair<R: Send + 'static>() -> (
    spawned_concurrency::PendingReply<'static, R>,
    spawned_concurrency::ReplyReceiver<R>,
) {
    spawned_concurrency::__private::reply_pair(spawned_concurrency::Deadline::Never)
}

/// The `PendingReply` a handler receives for a sent message (no caller),
/// for calling a manual `Handler::handle` directly as a send:
///
/// ```ignore
/// actor.handle(Tick, spawned_test::detached_reply(), &ctx).await;
/// ```
pub fn detached_reply<R: Send + 'static>() -> spawned_concurrency::PendingReply<'static, R> {
    spawned_concurrency::__private::detached_reply()
}
