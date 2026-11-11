//! Actor framework for Rust, inspired by Erlang/OTP.
//!
//! `spawned-concurrency` provides an actor model where concurrency logic is
//! separated from business logic. Define message interfaces with `#[protocol]`,
//! implement handlers with `#[actor]`, and call methods directly on actor references.
//!
//! # Quick Start
//!
//! ```ignore
//! use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
//! use spawned_concurrency::{actor, protocol, Response};
//!
//! #[protocol]
//! pub trait GreeterProtocol: Send + Sync {
//!     fn greet(&self, name: String) -> Response<String>;
//! }
//!
//! pub struct Greeter;
//!
//! #[actor(protocol = GreeterProtocol)]
//! impl Greeter {
//!     pub fn new() -> Self { Greeter }
//!
//!     #[request_handler]
//!     async fn handle_greet(&mut self, msg: greeter_protocol::Greet, _ctx: &Context<Self>) -> String {
//!         format!("Hello, {}!", msg.name)
//!     }
//! }
//! ```
//!
//! # Core Concepts
//!
//! **Protocols** — A `#[protocol]` trait defines the message interface. The macro generates:
//! - One message struct per method (in a snake_case submodule)
//! - A type-erased reference type (`XRef = Arc<dyn Protocol>`)
//! - Blanket `impl Protocol for ActorRef<A>` so you can call methods directly
//!
//! Return types determine message kind:
//! - [`Response<T>`] — request, works in both async and sync modes
//! - `Result<(), ActorError>` — fire-and-forget send, returns send result
//! - No return type — fire-and-forget send
//!
//! **Actors** — `#[actor]` on an impl block generates `impl Actor` and `Handler<M>`
//! impls from annotated methods (`#[request_handler]`, `#[send_handler]`, `#[deferred_handler]`).
//! Lifecycle hooks use `#[started]` and `#[stopped]`.
//!
//! **Type-Erased References** — Each protocol generates an `XRef` type alias
//! (e.g., `NameServerRef = Arc<dyn NameServerProtocol>`) and a `ToXRef` converter
//! trait. This lets actors accept protocol references without knowing the concrete
//! actor type — useful for cross-actor communication patterns.
//!
//! # Modules
//!
//! - `tasks` — async actor runtime (default `async` feature; requires Tokio)
//! - [`threads`] — blocking actor runtime (native OS threads)
//! - [`registry`] — global name-based actor registry
//! - [`response`] — `Response<T>` unified wrapper for request-response
//! - [`error`] — `ActorError` and `ExitReason` types
//! - [`message`] — `Message` trait for defining message types
//! - [`child_handle`] — `ChildHandle` and `ActorId` for type-erased actor management
//! - [`monitor`] — `MonitorRef` and `Down` for unidirectional death observation
//! - [`link`] — `Exit` and bidirectional links with `trap_exit` semantics
//!
//! # Choosing `tasks` vs `threads`
//!
//! Both modules provide identical `Actor`, `Handler<M>`, `ActorRef<A>`, and
//! `Context<A>` types. Use `tasks` when you need async I/O or high actor counts.
//! Use `threads` for CPU-bound work or when you want to avoid an async runtime.
//! Switching requires changing imports and adding/removing `async`/`.await`.
//!
//! # Advanced
//!
//! - [`message::Message`] trait for manual message definitions without `#[protocol]`
//! - `Recipient<M>` (`Arc<dyn Receiver<M>>`) for type-erased per-message references
//! - `tasks::Backend` enum for choosing async runtime, blocking pool, or OS thread
#![forbid(unsafe_code)]

#[cfg(test)]
mod test_wait;

#[cfg(test)]
#[path = "../tests/support/drop_subprocess.rs"]
mod drop_subprocess;

// Must precede the modules that place `fail_point!` sites.
#[macro_use]
mod failpoint_macro;
#[cfg(feature = "failpoints")]
pub mod failpoints;

pub mod child_handle;
pub mod error;
// Test-only: public (and unstable) with the `lifecycle-trace` feature.
#[cfg(feature = "lifecycle-trace")]
pub mod lifecycle_trace;
#[cfg(not(feature = "lifecycle-trace"))]
#[allow(dead_code)]
pub(crate) mod lifecycle_trace;
pub mod link;
pub mod message;
pub mod monitor;
pub mod registry;
// Reached through the crate root, `tasks` and `threads`; the module path
// is not part of the API.
#[doc(hidden)]
pub mod reply;
pub mod response;
#[cfg(feature = "async")]
pub mod supervision;
#[cfg(feature = "async")]
pub mod system;
#[cfg(feature = "async")]
pub mod tasks;
pub mod threads;

pub use child_handle::{ActorId, ChildHandle};
pub use error::{ActorError, ExitReason, NoProc};
pub use link::Exit;
pub use monitor::{Down, MonitorRef};
pub use reply::{
    Deadline, DeferError, Delivery, InsertError, PendingReplies, PendingReply, ReplyReceiver,
    ReplyTo,
};
pub use response::{Response, DEFAULT_REQUEST_TIMEOUT};
pub use spawned_macros::{actor, protocol, send_handler};
/// The instant type of reply and job deadlines (`Deadline::At`,
/// `JobOptions::deadline`, `ReplyTo::deadline`, `JobCtx::emit_at`):
/// `std::time::Instant`, or the simulated clock's instant under
/// `--cfg spawned_sim`. Re-exported so callers need not depend on
/// `spawned-rt`.
pub use spawned_rt::time::Instant;

#[cfg(all(kani, feature = "async"))]
mod kani_proofs;
#[cfg(all(test, feature = "async"))]
mod link_regression_tests;
#[cfg(test)]
mod native_link_regression_tests;

mod actor_cleanup;
mod mailbox;

/// Hooks for `spawned-test` and macro-generated code. Not part of the public
/// API: no semver promise, may change in any release.
#[doc(hidden)]
pub mod __private {
    use crate::reply::{Deadline, PendingReply, ReplyReceiver};
    use crate::ActorError;

    /// Seals `Receiver` (tasks and threads): implemented by
    /// `ActorRef`, `Context` and spawned-test's probe only.
    pub trait Sealed {}

    /// A `PendingReply` not tied to an actor turn, and its receiver
    /// (`spawned_test::reply_pair`).
    pub fn reply_pair<R: Send + 'static>(
        deadline: Deadline,
    ) -> (PendingReply<'static, R>, ReplyReceiver<R>) {
        crate::reply::reply_pair(deadline)
    }

    /// The `PendingReply` of a sent message (`spawned_test::detached_reply`).
    pub fn detached_reply<R: Send + 'static>() -> PendingReply<'static, R> {
        PendingReply::detached()
    }

    /// Answer `ActorStopped` (a test double standing in for an absent actor).
    pub fn discharge_stopped<R: Send + 'static>(reply: PendingReply<'_, R>) {
        reply.discharge_stopped()
    }

    /// Run `f` in one of the actor's turns (`spawned_test::tasks::in_actor`).
    #[cfg(feature = "async")]
    pub fn in_turn<A, R, F>(
        actor: &crate::tasks::ActorRef<A>,
        f: F,
    ) -> Result<ReplyReceiver<R>, ActorError>
    where
        A: crate::tasks::Actor,
        R: Send + 'static,
        F: FnOnce(&mut A, &crate::tasks::Context<A>) -> R + Send + 'static,
    {
        actor.in_turn(f)
    }

    /// Run `f` in one of the actor's turns (`spawned_test::threads::in_actor`).
    pub fn in_turn_threads<A, R, F>(
        actor: &crate::threads::ActorRef<A>,
        f: F,
    ) -> Result<ReplyReceiver<R>, ActorError>
    where
        A: crate::threads::Actor,
        R: Send + 'static,
        F: FnOnce(&mut A, &crate::threads::Context<A>) -> R + Send + 'static,
    {
        actor.in_turn(f)
    }
}

mod instrumentation;
#[cfg(feature = "metrics")]
pub mod metrics;

// Resolve feature gating in the defining crate, not in a protocol consumer's
// unrelated feature namespace. Proc macros use this for their async blanket impl.
#[doc(hidden)]
#[macro_export]
#[cfg(feature = "async")]
macro_rules! __spawned_async_items { ($($item:item)*) => { $($item)* }; }
#[doc(hidden)]
#[macro_export]
#[cfg(not(feature = "async"))]
macro_rules! __spawned_async_items {
    ($($item:item)*) => {};
}
