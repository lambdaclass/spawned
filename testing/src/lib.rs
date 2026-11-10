//! Test kit for [Spawned](https://github.com/lambdaclass/spawned) actors.
//!
//! Use it as a dev-dependency, the way `tokio-test` is used with Tokio. The
//! crate is unreleased: depend on it by path (or from the repository) until
//! the next release.
//!
//! ```toml
//! [dev-dependencies]
//! spawned-test = { path = "../testing" } # published with the next release
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
//! - **Fault injection**: [`faults::exit`] stops any actor with a chosen
//!   [`ExitReason`](spawned_concurrency::ExitReason) (`Panic` travels
//!   through a link, while `Kill` is direct; an injected panic surfaces as
//!   `Panic("panic in handler: ..")` and the default panic hook prints it);
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
