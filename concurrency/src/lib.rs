//! Actor framework for Rust, inspired by Erlang/OTP.
//!
//! `spawned-concurrency` provides an actor model where concurrency logic is
//! separated from business logic. Define message interfaces with `#[protocol]`,
//! implement handlers with `#[actor]`, and call methods directly on actor references.
//!
//! # Quick Start
//!
//! ```ignore
//! use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler, Response};
//! use spawned_concurrency::{actor, protocol};
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
//! - [`tasks::Response<T>`] — async request, caller awaits the reply
//! - `Result<T, ActorError>` — sync request (threads mode)
//! - No return type — fire-and-forget send
//!
//! **Actors** — `#[actor]` on an impl block generates `impl Actor` and `Handler<M>`
//! impls from annotated methods (`#[request_handler]`, `#[send_handler]`, `#[handler]`).
//! Lifecycle hooks use `#[started]` and `#[stopped]`.
//!
//! **Type-Erased References** — Each protocol generates an `XRef` type alias
//! (e.g., `NameServerRef = Arc<dyn NameServerProtocol>`) and a `ToXRef` converter
//! trait. This lets actors accept protocol references without knowing the concrete
//! actor type — useful for cross-actor communication patterns.
//!
//! # Modules
//!
//! - [`tasks`] — async actor runtime (requires tokio)
//! - [`threads`] — blocking actor runtime (native OS threads)
//! - [`registry`] — global name-based actor registry
//! - [`error`] — `ActorError` type
//! - [`message`] — `Message` trait and declarative macros (`messages!`, `send_messages!`, `request_messages!`)
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
//! - [`message::Message`] trait and `messages!` macro for manual message definitions
//! - `Recipient<M>` (`Arc<dyn Receiver<M>>`) for type-erased per-message references
//! - [`tasks::Backend`] enum for choosing async runtime, blocking pool, or OS thread

pub mod error;
pub mod message;
pub mod registry;
pub mod tasks;
pub mod threads;

pub use spawned_macros::{actor, protocol};
