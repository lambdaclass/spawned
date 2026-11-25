#![forbid(unsafe_code)]
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, spanned::Spanned, Attribute, FnArg, GenericArgument, Ident, ImplItem,
    ImplItemFn, ItemImpl, ItemTrait, Pat, PathArguments, ReturnType, TraitItem, Type, TypePath,
};

mod actor;
mod lints;
mod protocol;
mod send_handler;
mod sim_test;
use lints::*;
use protocol::*;

/// Generates message types and blanket implementations from a protocol trait.
///
/// `#[protocol]` transforms a trait definition into a full message-passing interface.
/// Each trait method becomes a message struct, and any `ActorRef<A>` where `A` handles
/// those messages automatically implements the trait — so you call methods directly on
/// the actor reference.
///
/// # What It Generates
///
/// For a trait `FooProtocol`, the macro generates:
///
/// 1. **Message structs** in a `foo_protocol` submodule — one per method, with public
///    fields matching the method parameters. Each implements `Message`.
///    Unit structs (methods with no parameters beyond `&self`) automatically derive `Clone`.
/// 2. **Type alias** `pub type FooRef = Arc<dyn FooProtocol>` — a type-erased reference.
/// 3. **Converter trait** `ToFooRef` with a `to_foo_ref(&self) -> FooRef` method.
/// 4. **Blanket impls** — `impl FooProtocol for ActorRef<A>` and `impl ToFooRef for ActorRef<A>`
///    for any actor `A` that handles all the generated message types.
///
/// # Type Resolution in Generated Structs
///
/// The generated message module uses `use super::*` to access types from the
/// parent scope. Types used in method parameters are qualified with `super::`
/// in the generated structs — this means any type you reference in a protocol
/// method signature must be in scope where the `#[protocol]` trait is defined.
///
/// Prelude types (`String`, `Vec`, `Option`, `Result`, `Box`, `bool`, `u32`, etc.)
/// and fully qualified paths (`std::`, `core::`, `alloc::`) are used as-is without
/// `super::` prefixing.
///
/// # Return Type Conventions
///
/// The return type on each method determines the message kind:
///
/// | Return type | Kind | Runtime | Caller behavior |
/// |-------------|------|---------|-----------------|
/// | `Response<T>` | Request | Both | `.await.unwrap()` (tasks) / `.unwrap()` (threads) — 5s default timeout |
/// | `Result<(), ActorError>` | Send | Both | Returns send result |
/// | *(none)* / `-> ()` | Send | Both | Fire-and-forget (discards send result) |
///
/// # Naming
///
/// - **Module**: trait name → `snake_case` (e.g., `ChatRoomProtocol` → `chat_room_protocol`)
/// - **Structs**: method name → `PascalCase` (e.g., `send_message` → `SendMessage`)
/// - **XRef alias**: strips `Protocol` suffix → `{Base}Ref` (e.g., `ChatRoomProtocol` → `ChatRoomRef`)
/// - **Converter**: `To{Base}Ref` with `to_{snake_base}_ref()` method
///
/// # Example
///
/// ```ignore
/// use spawned_concurrency::Response;
/// use spawned_concurrency::protocol;
///
/// #[protocol]
/// pub trait CounterProtocol: Send + Sync {
///     fn increment(&self, amount: u64);                     // send (fire-and-forget)
///     fn get_count(&self) -> Response<u64>;                 // request (both modes)
/// }
///
/// // Generated:
/// // - pub mod counter_protocol { pub struct Increment { pub amount: u64 }, pub struct GetCount }
/// // - pub type CounterRef = Arc<dyn CounterProtocol>;
/// // - pub trait ToCounterRef { fn to_counter_ref(&self) -> CounterRef; }
/// // - impl CounterProtocol for ActorRef<A> where A: Handler<Increment> + Handler<GetCount>
/// ```
/// # Request timeouts
///
/// `#[protocol(timeout = Duration::from_secs(30))]` sets a protocol default;
/// `#[request_timeout(Duration::from_secs(2))]` overrides a request method.
/// Without either setting, requests keep their five-second default.
/// `actor.with_request_timeout(limit).method(...)` takes precedence and also
/// supports conversion with the generated `ToXRef` trait. Configure before
/// erasing the concrete handle. These are reply-wait limits, not cancellation
/// or inherited deadlines; accepted work may still execute after timeout.
/// The deadline is fixed when the request is sent (send time plus the
/// timeout), as in OTP's `gen_server:call`. Native protocol calls block
/// immediately; async responses wait until that deadline when polled.
#[proc_macro_attribute]
pub fn protocol(attr: TokenStream, item: TokenStream) -> TokenStream {
    protocol::expand(attr, item)
}

/// Generates `impl Actor` and `Handler<M>` implementations from an annotated impl block.
///
/// Place `#[actor]` on an `impl MyStruct` block. The macro extracts annotated methods
/// and generates the boilerplate needed to run the struct as an actor.
///
/// # Protocol Assertion
///
/// Use `protocol = TraitName` to verify at compile time that the actor fully implements
/// a protocol (i.e., that `ActorRef<Self>` implements the protocol trait):
///
/// ```ignore
/// #[actor(protocol = NameServerProtocol)]
/// impl NameServer { /* ... */ }
/// ```
///
/// The runtime (tasks vs threads) is inferred from whether any handler in the block
/// is `async fn`. If all handlers are defined outside the `#[actor]` block, put at
/// least one handler inside so the macro can detect the correct runtime.
///
/// For multiple protocols:
/// ```ignore
/// #[actor(protocol(RoomProtocol, UserProtocol))]
/// impl ChatUser { /* ... */ }
/// ```
///
/// # Deferred-reply bounds
///
/// `max_outstanding = N` sets `Actor::MAX_OUTSTANDING` (deferred credits per
/// instance) and `max_native_jobs = N` sets `Actor::MAX_NATIVE_JOBS` (blocking
/// jobs running at once; the default, `min(4, available_parallelism)`, is
/// easy to outgrow: under load the only symptom is `NativeBudget` refusals).
/// Both take any constant expression and combine with `protocol`; a
/// supervised child can still override them per `ChildSpec`:
///
/// ```ignore
/// #[actor(protocol = P2PProtocol, max_native_jobs = 64, max_outstanding = 4096)]
/// impl P2PServer { /* ... */ }
/// ```
///
/// `inspect` shows the actor's state in `get_status` through its
/// `spawned_concurrency::introspection::Inspect` impl (OTP `format_status`);
/// `inspect = debug` uses its `Debug` form. Without either the state is
/// redacted. Both set `Actor::FORMAT_STATUS`.
///
/// # Lints
///
/// `#[actor]` warns (compile time only) when the impl block builds or accepts
/// shared mutable state (`Arc<Mutex<_>>`, `Arc<RwLock<_>>`, `Arc<Atomic*>`,
/// `Rc<RefCell<_>>`), and once per crate built with `panic = "abort"`. Opt out
/// with `#[actor(allow_shared_state)]` and `#[actor(allow_panic_abort)]`.
///
/// # Handler Attributes
///
/// | Attribute | Use for |
/// |-----------|---------|
/// | `#[request_handler]` | Messages that expect a reply (`Response<T>`) |
/// | `#[send_handler]` | Fire-and-forget messages |
/// | `#[deferred_handler]` | Requests answered through a `PendingReply` (now, or later via `defer()`) |
///
/// Handler signature: `fn name(&mut self, msg: MessageType, ctx: &Context<Self>) -> ReturnType`
///
/// The return type must match the message's `Message::Result`. For request handlers
/// returning `()`, omit the return type. The generated `Handler::handle` calls
/// the method and replies with its result; the method stays callable directly.
///
/// A deferred handler takes the reply obligation instead of returning:
/// `fn name(&mut self, msg: M, reply: PendingReply<'_, R>, ctx: &Context<Self>)`.
///
/// # Lifecycle Hooks
///
/// | Attribute | Called |
/// |-----------|--------|
/// | `#[started]` | After the actor starts, before processing messages |
/// | `#[stopped]` | After the actor stops processing messages |
/// | `#[exit_received]` | A linked exit delivered while trapping exits |
///
/// Both receive `&mut self` and `&Context<Self>`. Use async or sync to match your runtime.
///
/// # Example
///
/// ```ignore
/// use spawned_concurrency::tasks::{Actor, Context, Handler};
/// use spawned_concurrency::actor;
///
/// pub struct MyActor { count: u64 }
///
/// #[actor(protocol = CounterProtocol)]
/// impl MyActor {
///     pub fn new() -> Self { MyActor { count: 0 } }
///
///     #[started]
///     async fn on_start(&mut self, _ctx: &Context<Self>) {
///         tracing::info!("Actor started");
///     }
///
///     #[send_handler]
///     async fn handle_increment(&mut self, msg: Increment, _ctx: &Context<Self>) {
///         self.count += msg.amount;
///     }
///
///     #[request_handler]
///     async fn handle_get_count(&mut self, _msg: GetCount, _ctx: &Context<Self>) -> u64 {
///         self.count
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn actor(attr: TokenStream, item: TokenStream) -> TokenStream {
    actor::expand(attr, item)
}

/// Send-only form of a manual `Handler` impl: write `handle` without the
/// reply parameter, and the impl answers `()` once `handle` returns.
///
/// ```ignore
/// #[send_handler]
/// impl Handler<Tick> for Poller {
///     async fn handle(&mut self, _msg: Tick, ctx: &Context<Self>) {
///         self.poll().await;
///     }
/// }
/// ```
///
/// expands to the full `handle(&mut self, msg, reply: PendingReply<'_, ()>,
/// ctx)` whose body runs yours and then calls `reply.reply(())`, also after
/// an early `return`. The automatic `()` is a real reply, as with
/// `#[send_handler]` methods in an `#[actor]` block: a `request` gets
/// `Ok(())`, not `ReplyDropped`, and a panic in the body answers
/// `ActorStopped` as for any handler. The message's `Result` must be `()`.
/// Works for `tasks` (`async fn`) and `threads` (`fn`) handlers.
///
/// The reply type is named through the `Handler` path when it is qualified
/// (`impl my_spawned::tasks::Handler<M> for A` uses
/// `my_spawned::tasks::PendingReply`), so a renamed dependency works.
/// With a bare `Handler`, it is `spawned_concurrency::PendingReply`;
/// a crate that renames `spawned-concurrency` names it:
/// `#[send_handler(crate = my_spawned)]`.
#[proc_macro_attribute]
pub fn send_handler(attr: TokenStream, item: TokenStream) -> TokenStream {
    send_handler::expand(attr, item)
}

/// Seeded async test: on Tokio in normal builds, over a seed range on the
/// deterministic simulator under `--cfg spawned_sim` (see
/// `spawned_rt::tasks::SimTest`). Accepts `#[tokio::test]`'s `flavor` and
/// `worker_threads` (used only on Tokio), `seeds = N` (simulation seeds
/// `0..N`), `explore = "pct:d=3"` (simulation scheduling strategy: `random`,
/// `pct[:d=D][,k=K]` or `dfs[:bound=B][,depth=N]`) and `iterations = N` (runs
/// for that strategy). `sim_only` compiles the test only under
/// `--cfg spawned_sim` (the cfg is evaluated by `spawned-concurrency`, so the
/// caller needs no `#[cfg]` or `unexpected_cfgs` entry). The expansion goes
/// through `spawned_concurrency::__private`; `crate = path` names a renamed
/// `spawned-concurrency`. The function may take one `u64` argument, the seed.
///
/// ```ignore
/// #[spawned_concurrency::sim_test(flavor = "multi_thread", worker_threads = 2)]
/// async fn restarts_child(seed: u64) { /* ... */ }
///
/// #[spawned_concurrency::sim_test(sim_only, explore = "pct:d=3", iterations = 1000)]
/// async fn no_lost_update() { /* ... */ }
/// ```
#[proc_macro_attribute]
pub fn sim_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    sim_test::expand(attr, item)
}
