# API Redesign: Alternatives Summary

This document summarizes the different approaches explored for solving two critical API issues in spawned's actor framework. Each approach is illustrated with the **same example** — a chat room with bidirectional communication — so the trade-offs in expressivity, readability, and ease of use can be compared directly.

## Table of Contents

- [The Two Problems](#the-two-problems)
- [The Chat Room Example](#the-chat-room-example)
- [Baseline: The Old API](#baseline-the-old-api-whats-on-main-today)
- [Approach A: Handler\<M\> + Recipient\<M\>](#approach-a-handlerm--recipientm-actix-style)
- [Approach B: Protocol Traits](#approach-b-protocol-traits-user-defined-contracts)
- [Approach C: Typed Wrappers](#approach-c-typed-wrappers-non-breaking)
- [Approach D: Derive Macro](#approach-d-derive-macro)
- [Approach E: AnyActorRef](#approach-e-anyactorref-fully-type-erased)
- [Approach F: PID Addressing](#approach-f-pid-addressing-erlang-style)
- [Registry & Service Discovery](#registry--service-discovery)
- [Macro Improvement Potential](#macro-improvement-potential)
- [Comparison Matrix](#comparison-matrix)
- [Recommendation](#recommendation)
- [Branch Reference](#branch-reference)

---

## The Two Problems

### #144: No per-message type safety

The original API uses a single enum for all request types and another for all reply types. Every `match` must handle variants that are structurally impossible for the message sent:

```rust
// Old API — every request returns the full Reply enum
match actor.request(Request::GetName).await? {
    Reply::Name(n) => println!("{}", n),
    Reply::NotFound => println!("not found"),
    Reply::Age(_) => unreachable!(), // impossible, but the compiler demands it
}
```

### #145: Circular dependencies between actors

When two actors need bidirectional communication, storing `ActorRef<A>` and `ActorRef<B>` creates a circular module dependency:

```rust
// room.rs — needs to send Deliver to Users
struct ChatRoom { members: Vec<ActorRef<User>> }  // imports User

// user.rs — needs to send Say to the Room
struct User { room: ActorRef<ChatRoom> }           // imports ChatRoom → circular!
```

---

## The Chat Room Example

Every approach below implements the same scenario:

- **ChatRoom** actor holds a list of members and broadcasts messages
- **User** actor receives messages and can speak to the room
- The room sends `Deliver` to users; users send `Say` to the room → **bidirectional**
- `Members` is a request-reply message that returns the current member list

This exercises both #144 (typed request-reply) and #145 (circular dependency breaking).

---

## Baseline: The Old API (what's on `main` today)

Single-enum approach inspired by Erlang's gen_server callbacks:

```rust
trait Actor: Send + Sized + 'static {
    type Request: Clone + Send;   // single enum for all call messages
    type Message: Clone + Send;   // single enum for all cast messages
    type Reply: Send;             // single enum for all responses
    type Error: Debug + Send;

    async fn handle_request(&mut self, msg: Self::Request, ...) -> RequestResponse<Self>;
    async fn handle_message(&mut self, msg: Self::Message, ...) -> MessageResponse;
}
```

**The chat room cannot be built** with the old API as separate modules. There's no type-erasure mechanism, so `ChatRoom` must store `ActorRef<User>` (imports User) while `User` must store `ActorRef<ChatRoom>` (imports ChatRoom) — circular. You'd have to put everything in a single file or use raw channels.

Even ignoring #145, the #144 problem means this:

```rust
// room.rs — all messages in one enum, all replies in another
#[derive(Clone)]
enum RoomRequest { Say { from: String, text: String }, Members }

#[derive(Clone)]
enum RoomReply { Ack, MemberList(Vec<String>) }

impl Actor for ChatRoom {
    type Request = RoomRequest;
    type Reply = RoomReply;
    // ...

    async fn handle_request(&mut self, msg: RoomRequest, handle: &ActorRef<Self>) -> RequestResponse<Self> {
        match msg {
            RoomRequest::Say { from, text } => { /* broadcast */ RequestResponse::Reply(RoomReply::Ack) }
            RoomRequest::Members => RequestResponse::Reply(RoomReply::MemberList(self.member_names())),
        }
    }
}

// Caller — must match impossible variants
match room.request(RoomRequest::Members).await? {
    RoomReply::MemberList(names) => println!("{:?}", names),
    RoomReply::Ack => unreachable!(), // ← impossible but required
}
```

**Readability:** The trait signature is self-contained but the enum matching is noisy. New team members must mentally map which reply variants are valid for each request variant — the compiler won't help.

---

## Approach A: Handler\<M\> + Recipient\<M\> (Actix-style)

**Branches:** [`feat/handler-api-v0.5`](https://github.com/lambdaclass/spawned/tree/34bf9a759cda72e5311efda8f1fc8a5ae515129a) (implementation), [`feat/critical-api-issues`](https://github.com/lambdaclass/spawned/tree/1ef33bf0c463543dca379463c554ccc5914c86ff) (design doc), [`feat/actor-macro-registry`](https://github.com/lambdaclass/spawned/tree/de651ad21e2dd39babf534cb74174ae0fe3b399c) (adds macro + registry)

**Status:** Fully implemented and working. 34 tests passing. Multiple examples ported.

Each message is its own struct with an associated `Result` type. Actors implement `Handler<M>` per message. Type erasure uses `Recipient<M> = Arc<dyn Receiver<M>>`.

### Without macro (manual `impl Handler<M>`)

<details>
<summary><b>messages.rs</b> — shared types, no actor types mentioned</summary>

```rust
use spawned_concurrency::message::Message;
use spawned_concurrency::messages;
use spawned_concurrency::tasks::Recipient;

pub struct Join {
    pub name: String,
    pub inbox: Recipient<Deliver>,
}
impl Message for Join { type Result = (); }

messages! {
    Say { from: String, text: String } -> ();
    SayToRoom { text: String } -> ();
    Deliver { from: String, text: String } -> ();
}
```
</details>

<details>
<summary><b>room.rs</b> — knows messages, not User</summary>

```rust
use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};
use crate::messages::{Deliver, Join, Say};

pub struct ChatRoom {
    members: Vec<(String, Recipient<Deliver>)>,
}

impl Actor for ChatRoom {}

impl Handler<Join> for ChatRoom {
    async fn handle(&mut self, msg: Join, _ctx: &Context<Self>) {
        self.members.push((msg.name, msg.inbox));
    }
}

impl Handler<Say> for ChatRoom {
    async fn handle(&mut self, msg: Say, _ctx: &Context<Self>) {
        for (name, inbox) in &self.members {
            if *name != msg.from {
                let _ = inbox.send(Deliver { from: msg.from.clone(), text: msg.text.clone() });
            }
        }
    }
}
```
</details>

<details>
<summary><b>user.rs</b> — knows messages, not ChatRoom</summary>

```rust
use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};
use crate::messages::{Deliver, Say, SayToRoom};

pub struct User {
    pub name: String,
    pub room: Recipient<Say>,
}

impl Actor for User {}

impl Handler<SayToRoom> for User {
    async fn handle(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        let _ = self.room.send(Say { from: self.name.clone(), text: msg.text });
    }
}

impl Handler<Deliver> for User {
    async fn handle(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }
}
```
</details>

<details>
<summary><b>main.rs</b></summary>

```rust
let room = ChatRoom::new().start();
let alice = User { name: "Alice".into(), room: room.recipient() }.start();
let bob = User { name: "Bob".into(), room: room.recipient() }.start();

room.send_request(Join { name: "Alice".into(), inbox: alice.recipient::<Deliver>() }).await?;
room.send_request(Join { name: "Bob".into(), inbox: bob.recipient::<Deliver>() }).await?;

alice.send_request(SayToRoom { text: "Hello everyone!".into() }).await?;
```
</details>

### With `#[actor]` macro + `actor_api!`

<details>
<summary><b>room.rs</b> — macros eliminate both Handler and extension trait boilerplate</summary>

```rust
use spawned_concurrency::actor_api;
use spawned_concurrency::send_messages;
use spawned_concurrency::request_messages;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler, Recipient};
use spawned_macros::actor;

// -- Messages --

send_messages! {
    Say { from: String, text: String };
    Deliver { from: String, text: String };
    Join { name: String, inbox: Recipient<Deliver> }
}

request_messages! {
    Members -> Vec<String>
}

// -- API --

actor_api! {
    pub ChatRoomApi for ActorRef<ChatRoom> {
        send fn say(from: String, text: String) => Say;
        send fn add_member(name: String, inbox: Recipient<Deliver>) => Join;
        request async fn members() -> Vec<String> => Members;
    }
}

// -- Actor --

pub struct ChatRoom {
    members: Vec<(String, Recipient<Deliver>)>,
}

impl Actor for ChatRoom {}

#[actor]
impl ChatRoom {
    pub fn new() -> Self {
        Self { members: Vec::new() }
    }

    #[send_handler]
    async fn handle_say(&mut self, msg: Say, _ctx: &Context<Self>) {
        for (name, inbox) in &self.members {
            if *name != msg.from {
                let _ = inbox.send(Deliver { from: msg.from.clone(), text: msg.text.clone() });
            }
        }
    }

    #[send_handler]
    async fn handle_join(&mut self, msg: Join, _ctx: &Context<Self>) {
        self.members.push((msg.name, msg.inbox));
    }

    #[request_handler]
    async fn handle_members(&mut self, _msg: Members, _ctx: &Context<Self>) -> Vec<String> {
        self.members.iter().map(|(name, _)| name.clone()).collect()
    }
}
```
</details>

<details>
<summary><b>user.rs</b> — macro version</summary>

```rust
use spawned_concurrency::actor_api;
use spawned_concurrency::send_messages;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};
use spawned_macros::actor;
use crate::room::{ChatRoom, ChatRoomApi, Deliver};

// -- Messages --

send_messages! {
    SayToRoom { text: String };
    JoinRoom { room: ActorRef<ChatRoom> }
}

// -- API --

actor_api! {
    pub UserApi for ActorRef<User> {
        send fn say(text: String) => SayToRoom;
        send fn join_room(room: ActorRef<ChatRoom>) => JoinRoom;
    }
}

// -- Actor --

pub struct User {
    pub name: String,
    room: Option<ActorRef<ChatRoom>>,
}

impl Actor for User {}

#[actor]
impl User {
    pub fn new(name: String) -> Self {
        Self { name, room: None }
    }

    #[send_handler]
    async fn handle_say_to_room(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        if let Some(ref room) = self.room {
            let _ = room.say(self.name.clone(), msg.text);
        }
    }

    #[send_handler]
    async fn handle_join_room(&mut self, msg: JoinRoom, ctx: &Context<Self>) {
        let _ = msg.room.add_member(self.name.clone(), ctx.recipient::<Deliver>());
        self.room = Some(msg.room);
    }

    #[send_handler]
    async fn handle_deliver(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }
}
```
</details>

<details>
<summary><b>main.rs</b> — extension traits make it read like plain method calls</summary>

```rust
let room = ChatRoom::new().start();
let alice = User::new("Alice".into()).start();
let bob = User::new("Bob".into()).start();

alice.join_room(room.clone()).unwrap();
bob.join_room(room.clone()).unwrap();

let members = room.members().await.unwrap();

alice.say("Hello everyone!".into()).unwrap();
bob.say("Hi Alice!".into()).unwrap();
```
</details>

### Analysis

| Dimension | Non-macro | With `#[actor]` macro + `actor_api!` |
|-----------|-----------|--------------------------------------|
| **Readability** | Each `impl Handler<M>` block is self-contained. You see the message type and return type in the trait bound. But many small impl blocks can feel scattered. | `#[send_handler]`/`#[request_handler]` attributes inside a single `#[actor] impl` block group all handlers together. `actor_api!` declares the caller-facing API in a compact block. Files read top-to-bottom: Messages → API → Actor. |
| **API at a glance** | Must scan all `impl Handler<M>` blocks to know what messages an actor handles. | The `actor_api!` block is the "at-a-glance" API surface — each line declares a method, its params, and the underlying message. |
| **Boilerplate** | One `impl Handler<M>` block per message × per actor. Message structs need manual `impl Message`. | `send_messages!`/`request_messages!` macros eliminate `Message` impls. `#[actor]` eliminates `Handler` impls. `actor_api!` reduces the extension trait + impl (~15 lines) to ~5 lines. |
| **main.rs expressivity** | Raw message structs: `room.send_request(Join { ... })` — explicit but verbose. | Extension traits: `alice.join_room(room.clone())` — reads like natural API calls. |
| **Circular dep solution** | `Recipient<M>` — room stores `Recipient<Deliver>`, user stores `Recipient<Say>`. Neither knows the other's concrete type. | Same mechanism. The macros don't change how type erasure works. |
| **Discoverability** | Standard Rust patterns. Any Rust developer can read `impl Handler<M>`. | `#[actor]` and `actor_api!` are custom — new developers need to learn what they do, but the patterns are common (Actix uses the same approach). |

**Key insight:** The non-macro version is already concise for handler code. The `#[actor]` macro eliminates the `impl Handler<M>` delegation wrapper per handler. The `actor_api!` macro eliminates the extension trait boilerplate (trait definition + impl block) that provides ergonomic method-call syntax on `ActorRef`. Together, they reduce an actor definition to three declarative blocks: messages, API, and handlers.

---

## Approach B: Protocol Traits (user-defined contracts)

**Branch:** [`feat/approach-b`](https://github.com/lambdaclass/spawned/tree/feat/approach-b)

**Status:** Fully implemented and working. All examples ported. Two proc macros: `#[protocol]` and `#[actor]`.

Users define protocol traits with `#[protocol]` — the macro generates message structs, converter traits, and blanket impls. Actors declare their protocol with `#[actor(protocol = X)]`, which auto-generates `impl Actor` and a compile-time assertion that the actor satisfies the protocol.

### How `#[protocol]` works

Given a trait definition, the `#[protocol]` macro generates:

1. **Message structs** in a submodule named after the trait (e.g., `room_protocol::Say`, `room_protocol::Members`)
2. **Converter trait** with identity impl (e.g., `ToRoomRef` / `to_room_ref()`) for ergonomic `Arc<dyn Protocol>` creation
3. **Blanket impl**: `impl<A: Actor + Handler<Msg1> + Handler<Msg2> + ...> Protocol for ActorRef<A>` — any actor handling all required messages automatically satisfies the protocol

Method return types determine the runtime mode:
- `Result<(), ActorError>` → **send** (fire-and-forget)
- `Response<T>` → **async request** (tasks runtime only)
- `Result<T, ActorError>` where T ≠ () → **sync request** (threads runtime only)
- Send-only protocols generate blanket impls for both runtimes

### How `#[actor]` works

The `#[actor]` macro:

1. **Auto-generates `impl Actor for T {}`** — no more redundant manual impl
2. **Generates `impl Handler<M>`** from `#[send_handler]` and `#[request_handler]` annotated methods
3. **Named parameters** for future extensibility: `#[actor(protocol = RoomProtocol)]`
4. **Compile-time assertion** that `ActorRef<T>` satisfies the declared protocol — if the actor is missing a handler, you get a clear compile error
5. **Lifecycle callbacks** via `#[started]` and `#[stopped]` annotations on methods (optional)

### Response\<T\>: Envelope's counterpart on the receive side

The existing codebase uses the **Envelope pattern** to type-erase messages on the send side: `Box<dyn Envelope<A>>` wraps a message + a oneshot sender, allowing the actor's mailbox to hold heterogeneous messages. `Response<T>` is the structural mirror on the receive side — it wraps a oneshot receiver and implements `Future<Output = Result<T, ActorError>>`:

```rust
// Envelope (existing): type-erases on the SEND side
//   Box<dyn Envelope<A>> holds msg + response sender

// Response<T> (new): concrete awaitable on the RECEIVE side
//   wraps oneshot::Receiver<T>, implements Future
pub struct Response<T>(oneshot::Receiver<T>);

impl<T> Future for Response<T> {
    type Output = Result<T, ActorError>;
    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        // delegates to inner receiver
    }
}
```

This keeps protocol traits **object-safe** — `fn members(&self) -> Response<Vec<String>>` returns a concrete type, not `impl Future` (which would require RPITIT and break `dyn Trait`). No `BoxFuture` boxing needed either.

### Full chat room code

<details>
<summary><b>protocols.rs</b> — shared contracts, just traits + type aliases</summary>

```rust
use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::Response;
use spawned_macros::protocol;
use std::sync::Arc;

// Manual type aliases for circular references between protocols
pub type RoomRef = Arc<dyn RoomProtocol>;
pub type UserRef = Arc<dyn UserProtocol>;

#[protocol]
pub trait RoomProtocol: Send + Sync {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn add_member(&self, name: String, user: UserRef) -> Result<(), ActorError>;
    fn members(&self) -> Response<Vec<String>>;
}

#[protocol]
pub trait UserProtocol: Send + Sync {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError>;
    fn say(&self, text: String) -> Result<(), ActorError>;
    fn join_room(&self, room: RoomRef) -> Result<(), ActorError>;
}
```

`#[protocol]` generates for each trait: a submodule with message structs (`room_protocol::Say`, `room_protocol::AddMember`, `room_protocol::Members`), a converter trait (`ToRoomRef` with `to_room_ref()`), and blanket `impl RoomProtocol for ActorRef<A>` where `A` handles all required messages.
</details>

<details>
<summary><b>room.rs</b> — just the actor struct + handlers, no bridge boilerplate</summary>

```rust
use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_macros::actor;

use crate::protocols::room_protocol::{AddMember, Members, Say};
use crate::protocols::{RoomProtocol, UserRef};

pub struct ChatRoom {
    members: Vec<(String, UserRef)>,
}

#[actor(protocol = RoomProtocol)]
impl ChatRoom {
    pub fn new() -> Self {
        Self { members: Vec::new() }
    }

    #[send_handler]
    async fn handle_say(&mut self, msg: Say, _ctx: &Context<Self>) {
        tracing::info!("[room] {} says: {}", msg.from, msg.text);
        for (name, user) in &self.members {
            if *name != msg.from {
                let _ = user.deliver(msg.from.clone(), msg.text.clone());
            }
        }
    }

    #[send_handler]
    async fn handle_add_member(&mut self, msg: AddMember, _ctx: &Context<Self>) {
        tracing::info!("[room] {} joined", msg.name);
        self.members.push((msg.name, msg.user));
    }

    #[request_handler]
    async fn handle_members(&mut self, _msg: Members, _ctx: &Context<Self>) -> Vec<String> {
        self.members.iter().map(|(name, _)| name.clone()).collect()
    }
}
```

No manual `impl Actor`, no message structs, no bridge impl, no conversion helper. `#[actor(protocol = RoomProtocol)]` generates `impl Actor for ChatRoom {}`, the `Handler<M>` impls, and a compile-time check that `ActorRef<ChatRoom>: RoomProtocol`.
</details>

<details>
<summary><b>user.rs</b> — same pattern, uses protocol types from room</summary>

```rust
use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_macros::actor;

use crate::protocols::user_protocol::{Deliver, JoinRoom, Say};
use crate::protocols::{RoomRef, ToUserRef, UserProtocol};

pub struct User {
    name: String,
    room: Option<RoomRef>,
}

#[actor(protocol = UserProtocol)]
impl User {
    pub fn new(name: String) -> Self {
        Self { name, room: None }
    }

    #[send_handler]
    async fn handle_deliver(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }

    #[send_handler]
    async fn handle_say(&mut self, msg: Say, _ctx: &Context<Self>) {
        if let Some(ref room) = self.room {
            let _ = room.say(self.name.clone(), msg.text);
        }
    }

    #[send_handler]
    async fn handle_join_room(&mut self, msg: JoinRoom, ctx: &Context<Self>) {
        let _ = msg.room.add_member(self.name.clone(), ctx.actor_ref().to_user_ref());
        self.room = Some(msg.room);
    }
}
```
</details>

<details>
<summary><b>main.rs</b> — protocol traits used directly as method calls</summary>

```rust
let room = ChatRoom::new().start();
let alice = User::new("Alice".into()).start();
let bob = User::new("Bob".into()).start();

alice.join_room(room.to_room_ref()).unwrap();
bob.join_room(room.to_room_ref()).unwrap();

let members = room.members().await.unwrap();

alice.say("Hello everyone!".into()).unwrap();
bob.say("Hey Alice!".into()).unwrap();
```

Protocol methods (`say`, `join_room`, `members`) are called directly on `ActorRef` thanks to the blanket impls. No extension traits, no `actor_api!`, no manual wrappers.
</details>

### Analysis

| Dimension | Assessment |
|-----------|-----------|
| **Readability** | `protocols.rs` is the single source of truth for cross-actor contracts. Actor files contain only the struct + handlers — no message definitions, no bridge code. `#[actor(protocol = RoomProtocol)]` makes the actor-to-protocol relationship explicit. |
| **API at a glance** | Protocol traits are the "at-a-glance" API surface: `RoomProtocol` tells you exactly what a room can do, `UserProtocol` tells you what a user can do. The strongest summary of all approaches. |
| **Boilerplate** | Minimal. Per actor: one `#[actor(protocol = X)]` line + handler methods. Per protocol: one `#[protocol]` trait. The `#[protocol]` macro generates message structs, converters, and blanket impls. No manual bridge code. |
| **main.rs expressivity** | Protocol methods called directly on `ActorRef`: `room.members().await`, `alice.say("Hi")`, `room.to_room_ref()`. No wrappers needed. |
| **Request-response** | `Response<T>` keeps protocol traits object-safe while supporting async request-response. Structural mirror of the Envelope pattern — no RPITIT, no `BoxFuture` boxing. |
| **Circular dep solution** | Actors hold `RoomRef` / `UserRef` (`Arc<dyn Protocol>`) instead of `ActorRef<OtherActor>`. Protocol definitions live in a shared `protocols.rs` that neither actor module depends on cyclically. |
| **Compile-time safety** | `#[actor(protocol = X)]` emits a static assertion. If `ChatRoom` is missing a handler for any `RoomProtocol` method, the build fails with a clear error pointing at the actor. |
| **Testability** | Best of all approaches — you can mock `RoomProtocol` or `UserProtocol` directly in unit tests without running an actor system. |

**Key insight:** The combination of `#[protocol]` + `#[actor(protocol = X)]` eliminates all the boilerplate that made the original Approach B more verbose than A (manual message structs, bridge impls, conversion helpers, `actor_api!`), while preserving B's unique advantages: protocol-level contracts, best-in-class testability, and actor-level registry granularity. The protocol trait IS the API — no separate API layer needed.

**Scaling trade-off:** In a system with N actor types and M cross-boundary message types, Approach A needs M message structs (manual or via `messages!`). Approach B needs P protocol traits (where P is the number of distinct actor interfaces). The message structs are auto-generated by `#[protocol]`, so the user only writes the trait methods. The cost scales with *actor interfaces*, not messages.

---

## Approach C: Typed Wrappers (non-breaking)

**Branch:** Not implemented. Documented in [`docs/ALTERNATIVE_APPROACHES.md`](https://github.com/lambdaclass/spawned/blob/b0e5afb2c69e1f5b6ab8ee82b59582348877c819/docs/ALTERNATIVE_APPROACHES.md).

Keeps the old enum-based `Actor` trait unchanged. Adds typed convenience methods that hide the enum matching. For #145, adds a second envelope-based channel to `ActorRef` alongside the existing enum channel.

### What the chat room would look like

<details>
<summary><b>room.rs</b> — enum Actor + typed wrappers + dual channel</summary>

```rust
// Old-style enum messages (unchanged from baseline)
#[derive(Clone)]
pub enum RoomMessage {
    Say { from: String, text: String },
    Join { name: String },
}

#[derive(Clone)]
pub enum RoomRequest { Members }

#[derive(Clone)]
pub enum RoomReply { Ack, MemberList(Vec<String>) }

pub struct ChatRoom {
    members: Vec<(String, Recipient<Deliver>)>,  // Recipient comes from new dual-channel
}

impl Actor for ChatRoom {
    type Request = RoomRequest;
    type Message = RoomMessage;
    type Reply = RoomReply;
    type Error = std::fmt::Error;

    async fn handle_message(&mut self, msg: RoomMessage, handle: &ActorRef<Self>) -> MessageResponse {
        match msg {
            RoomMessage::Say { from, text } => {
                for (name, inbox) in &self.members {
                    if *name != from {
                        let _ = inbox.send(Deliver { from: from.clone(), text: text.clone() });
                    }
                }
                MessageResponse::NoReply
            }
            RoomMessage::Join { name } => {
                // But wait — where does the Recipient<Deliver> come from?
                // The enum variant can't carry it (Clone bound on Message).
                // This is a fundamental limitation.
                MessageResponse::NoReply
            }
        }
    }

    async fn handle_request(&mut self, msg: RoomRequest, _: &ActorRef<Self>) -> RequestResponse<Self> {
        match msg {
            RoomRequest::Members => {
                let names = self.members.iter().map(|(n, _)| n.clone()).collect();
                RequestResponse::Reply(RoomReply::MemberList(names))
            }
        }
    }
}

// Typed wrappers hide the enum matching from callers
impl ChatRoom {
    pub fn say(handle: &ActorRef<Self>, from: String, text: String) -> Result<(), ActorError> {
        handle.send(RoomMessage::Say { from, text })
    }
    pub async fn members(handle: &ActorRef<Self>) -> Result<Vec<String>, ActorError> {
        match handle.request(RoomRequest::Members).await? {
            RoomReply::MemberList(names) => Ok(names),
            _ => unreachable!(),  // still exists, just hidden inside the wrapper
        }
    }
}

// For #145: Handler<M> impl on the SECOND channel (envelope-based)
// The actor loop select!s on both the enum channel and the envelope channel
impl Handler<Deliver> for ChatRoom { /* ... */ }
```
</details>

### Analysis

| Dimension | Assessment |
|-----------|-----------|
| **Readability** | Poor. Two dispatch mechanisms coexist: the old `match msg { ... }` for enum messages and `Handler<M>` impls on the envelope channel. A reader must understand both systems and how they interact. |
| **API at a glance** | The typed wrappers (`ChatRoom::say(...)`, `ChatRoom::members(...)`) provide a clean caller API. But the implementation behind them is messy. |
| **Boilerplate** | High. Every message needs: enum variant + typed wrapper + match arm. And `unreachable!()` branches still exist inside the wrappers. Cross-boundary messages also need `Handler<M>` impls. |
| **main.rs expressivity** | `ChatRoom::say(&room, from, text)` — associated functions, not method syntax on ActorRef. Less ergonomic than extension traits. |
| **Fundamental problem** | The old `Message` type requires `Clone`, but `Recipient<Deliver>` is `Arc<dyn ...>` which doesn't implement `Clone` in all contexts. The `Join` message can't carry a Recipient through the enum channel. This forces cross-boundary messages onto the second channel, splitting the actor's logic across two systems. |

**Key insight:** This approach tries to preserve backward compatibility, but the dual-channel architecture creates more confusion than a clean break would. The `Clone` bound on the old `Message` associated type is fundamentally incompatible with carrying type-erased handles, making the split between channels unavoidable and arbitrary.

---

## Approach D: Derive Macro

**Branch:** Not implemented. Documented in [`docs/ALTERNATIVE_APPROACHES.md`](https://github.com/lambdaclass/spawned/blob/b0e5afb2c69e1f5b6ab8ee82b59582348877c819/docs/ALTERNATIVE_APPROACHES.md).

A proc macro `#[derive(ActorMessages)]` auto-generates per-variant message structs, `Message` impls, typed wrappers, and `Handler<M>` delegation from an annotated enum.

### What the chat room would look like

<details>
<summary><b>room.rs</b> — derive macro generates everything from the enum</summary>

```rust
use spawned_derive::ActorMessages;

// The macro generates: struct Say, struct Join, struct Members,
// impl Message for each, typed wrapper methods, and Handler<M> delegation
#[derive(ActorMessages)]
#[actor(ChatRoom)]
pub enum RoomMessages {
    #[send]
    Say { from: String, text: String },

    #[send]
    Join { name: String, inbox: Recipient<Deliver> },

    #[request(Vec<String>)]
    Members,
}

pub struct ChatRoom {
    members: Vec<(String, Recipient<Deliver>)>,
}

impl Actor for ChatRoom {}

// You still write the old-style handle_request/handle_message,
// but the macro routes per-struct Handler<M> calls into it.
// OR: the macro generates Handler<M> impls that call per-variant methods:
impl ChatRoom {
    fn on_say(&mut self, msg: Say, ctx: &Context<Self>) { /* ... */ }
    fn on_join(&mut self, msg: Join, ctx: &Context<Self>) { /* ... */ }
    fn on_members(&mut self, msg: Members, ctx: &Context<Self>) -> Vec<String> { /* ... */ }
}
```
</details>

<details>
<summary><b>main.rs</b> — generated wrapper methods</summary>

```rust
let room = ChatRoom::new().start();
// Generated methods (associated functions on ActorRef<ChatRoom>):
room.say("Alice".into(), "Hello!".into()).unwrap();
let members = room.members().await.unwrap();
```
</details>

### Analysis

| Dimension | Assessment |
|-----------|-----------|
| **Readability** | The enum definition is compact, but what the macro generates is invisible. Reading `room.rs` tells you the message *names*, but you can't see the generated Handler impls, wrapper methods, or error handling without running `cargo expand`. |
| **API at a glance** | The annotated enum is a good summary of all messages. `#[send]` vs `#[request(ReturnType)]` makes the distinction clear. |
| **Boilerplate** | Lowest of all approaches for defining messages — one enum covers everything. But debugging generated code is costly when things go wrong (compile errors point to generated code). |
| **main.rs expressivity** | Generated wrappers would provide method-call syntax. Comparable to Approach A's extension traits, but with less control over the API shape. |
| **Complexity** | A new proc macro crate (compilation cost). The macro must handle edge cases: messages carrying `Recipient<M>`, mixed send/request variants, `Clone` bounds for the enum vs non-Clone fields. This is the most complex approach to implement correctly. |
| **Macro compatibility** | This IS the macro — it replaces both `send_messages!`/`request_messages!` and `#[actor]`. Larger blast radius means more things that can break. |

**Key insight:** The derive macro trades visibility for conciseness. Approach A's `#[actor]` macro is lighter — it only generates `impl Handler<M>` delegation from visibly-written handler methods. The derive macro tries to generate the handler methods too, making the actor's behavior harder to trace.

---

## Approach E: AnyActorRef (fully type-erased)

**Branch:** Not implemented. Documented in [`docs/ALTERNATIVE_APPROACHES.md`](https://github.com/lambdaclass/spawned/blob/b0e5afb2c69e1f5b6ab8ee82b59582348877c819/docs/ALTERNATIVE_APPROACHES.md).

Replaces `Recipient<M>` with a single fully type-erased handle `AnyActorRef = Arc<dyn AnyActor>` using `Box<dyn Any>`.

### What the chat room would look like

<details>
<summary><b>room.rs</b></summary>

```rust
pub struct ChatRoom {
    members: Vec<(String, AnyActorRef)>,  // no type parameter — stores anything
}

impl Handler<Say> for ChatRoom {
    async fn handle(&mut self, msg: Say, _ctx: &Context<Self>) {
        for (name, inbox) in &self.members {
            if *name != msg.from {
                // Runtime type dispatch — if inbox can't handle Deliver, it's a silent error
                let _ = inbox.send_any(Box::new(Deliver {
                    from: msg.from.clone(),
                    text: msg.text.clone(),
                }));
            }
        }
    }
}

impl Handler<Join> for ChatRoom {
    async fn handle(&mut self, msg: Join, _ctx: &Context<Self>) {
        self.members.push((msg.name, msg.inbox));  // just stores AnyActorRef
    }
}
```
</details>

<details>
<summary><b>user.rs</b></summary>

```rust
pub struct User {
    pub name: String,
    pub room: AnyActorRef,  // no type safety — could be any actor
}

impl Handler<SayToRoom> for User {
    async fn handle(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        // Must Box the message and hope the room can handle it
        let _ = self.room.send_any(Box::new(Say {
            from: self.name.clone(),
            text: msg.text,
        }));
    }
}
```
</details>

<details>
<summary><b>main.rs</b></summary>

```rust
let room = ChatRoom::new().start();
let alice = User { name: "Alice".into(), room: room.any_ref() }.start();

// Joining — also type-erased
room.send(Join { name: "Alice".into(), inbox: alice.any_ref() }).unwrap();

// Requesting members — must downcast the reply
let reply: Box<dyn Any> = room.request_any(Box::new(Members)).await?;
let members: Vec<String> = *reply.downcast::<Vec<String>>().expect("wrong reply type");
```
</details>

### Analysis

| Dimension | Assessment |
|-----------|-----------|
| **Readability** | The actor code is cluttered with `Box::new()`, `send_any()`, and `downcast()`. The type information that was available at compile time is now lost, making the code harder to reason about. |
| **API at a glance** | `AnyActorRef` tells you nothing about what messages an actor can receive. You must read the `Handler<M>` impls to know, and even then the caller has no compile-time enforcement. |
| **Boilerplate** | Low for cross-boundary wiring (just `AnyActorRef` everywhere). But higher for callers who must box/downcast. |
| **main.rs expressivity** | Poor. `room.request_any(Box::new(Members))` followed by `.downcast::<Vec<String>>()` is verbose and error-prone. Compare to Approach A's `room.request(Members).await` → `Vec<String>`. |
| **Safety** | Sending the wrong message type is a **runtime** error (or silently ignored). This defeats Rust's core value proposition. |

**Key insight:** AnyActorRef is essentially what you get in dynamically-typed languages. It solves #145 by erasing all type information, but in doing so also erases the compile-time safety that Rust provides. Wrong message types become runtime panics instead of compile errors.

---

## Approach F: PID Addressing (Erlang-style)

**Branch:** Not implemented. Documented in [`docs/ALTERNATIVE_APPROACHES.md`](https://github.com/lambdaclass/spawned/blob/b0e5afb2c69e1f5b6ab8ee82b59582348877c819/docs/ALTERNATIVE_APPROACHES.md).

Every actor gets a `Pid(u64)`. A global registry maps `(Pid, TypeId)` → message sender. Messages are sent by PID with explicit registration per message type.

### What the chat room would look like

<details>
<summary><b>room.rs</b></summary>

```rust
pub struct ChatRoom {
    members: Vec<(String, Pid)>,  // lightweight copyable identifier
}

impl Handler<Say> for ChatRoom {
    async fn handle(&mut self, msg: Say, _ctx: &Context<Self>) {
        for (name, pid) in &self.members {
            if *name != msg.from {
                // Typed send — but resolved at runtime via global registry
                let _ = spawned::send(*pid, Deliver {
                    from: msg.from.clone(),
                    text: msg.text.clone(),
                });
            }
        }
    }
}

impl Handler<Join> for ChatRoom {
    async fn handle(&mut self, msg: Join, _ctx: &Context<Self>) {
        self.members.push((msg.name, msg.pid));
    }
}
```
</details>

<details>
<summary><b>user.rs</b></summary>

```rust
pub struct User {
    pub name: String,
    pub room_pid: Pid,  // just a u64
}

impl Handler<SayToRoom> for User {
    async fn handle(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        let _ = spawned::send(self.room_pid, Say {
            from: self.name.clone(),
            text: msg.text,
        });
    }
}
```
</details>

<details>
<summary><b>main.rs</b> — requires explicit registration</summary>

```rust
let room = ChatRoom::new().start();
let alice = User { name: "Alice".into(), room_pid: room.pid() }.start();

// Must register each message type the actor can receive via PID
room.register::<Say>();
room.register::<Join>();
room.register::<Members>();
alice.register::<SayToRoom>();
alice.register::<Deliver>();

room.send(Join { name: "Alice".into(), pid: alice.pid() }).unwrap();

// Typed request — but only works if Members was registered
let members: Vec<String> = spawned::request(room.pid(), Members).await?;
```
</details>

### Analysis

| Dimension | Assessment |
|-----------|-----------|
| **Readability** | Actor code is clean — `spawned::send(pid, msg)` is simple and Erlang-familiar. But the registration boilerplate in `main.rs` is noisy and easy to forget. |
| **API at a glance** | `Pid` tells you nothing about what messages an actor accepts. You know less than with `ActorRef<ChatRoom>` (which at least tells you the actor type) or `Recipient<Say>` (which tells you the message type). |
| **Boilerplate** | Per-actor registration of every message type: `room.register::<Say>()`, `room.register::<Join>()`, etc. Forgetting a registration → runtime error. |
| **main.rs expressivity** | `spawned::send(pid, msg)` is concise. But registration lines are pure ceremony with no business logic value. |
| **Safety** | Sending to a dead PID or unregistered message type → **runtime** error. The compile-time guarantee "this actor handles this message" is lost. |
| **Clustering** | Best positioned for distributed systems — `Pid` is a location-transparent identifier that naturally extends to remote nodes. |

**Key insight:** PID addressing is the most Erlang-faithful approach, and shines for clustering/distribution. But it trades Rust's compile-time type safety for runtime resolution, which is a cultural mismatch. Erlang's runtime was designed around "let it crash" — Rust's philosophy is "don't let it compile if it's wrong."

---

## Registry & Service Discovery

The current registry is a global `Any`-based name store (Approach A):

```rust
// Register: store a Recipient<M> by name
registry::register("service_registry", svc.recipient::<Lookup>()).unwrap();

// Discover: retrieve without knowing the concrete actor type
let recipient: Recipient<Lookup> = registry::whereis("service_registry").unwrap();

// Use: typed request through the recipient
let addr = request(&*recipient, Lookup { name: "web".into() }, timeout).await?;
```

The registry API (`register`, `whereis`, `unregister`, `registered`) stays the same across approaches — it's just `HashMap<String, Box<dyn Any>>` with `RwLock`. What changes is **what you store and what you get back**.

### How it differs per approach

| Approach | Stored value | Retrieved as | Type safety | Discovery granularity |
|----------|-------------|-------------|-------------|----------------------|
| **Baseline** | `ActorRef<A>` | `ActorRef<A>` | Compile-time, but requires knowing actor type | Per actor — defeats the point of discovery |
| **A: Recipient** | `Recipient<M>` | `Recipient<M>` | Compile-time per message type | Per message type — fine-grained |
| **B: Protocol Traits** | `Arc<dyn Protocol>` | `Arc<dyn Protocol>` | Compile-time per protocol | Per protocol — coarser-grained |
| **C: Typed Wrappers** | `ActorRef<A>` or `Recipient<M>` | Mixed | Depends on channel | Unclear — dual-channel split |
| **D: Derive Macro** | `Recipient<M>` | `Recipient<M>` | Same as A | Same as A |
| **E: AnyActorRef** | `AnyActorRef` | `AnyActorRef` | None — runtime only | Per actor, but no type info |
| **F: PID** | `Pid` | `Pid` | None — runtime only | Per actor (Erlang-style `whereis`) |

**Key differences:**

- **A and D** register per message type: `registry::register("room_lookup", room.recipient::<Lookup>())`. A consumer discovers a `Recipient<Lookup>` — it can only send `Lookup` messages, nothing else. If the room handles 5 message types, you can register it under 5 names (or one name per message type you want to expose). This is the most granular.

- **B** registers per protocol: `registry::register("room", room.as_broadcaster())`. A consumer discovers a `BroadcasterRef` (`Arc<dyn ChatBroadcaster>`) — it can call any method on the protocol (`say`, `add_member`, `members`). This is coarser but more natural: one registration covers all the methods in the protocol.

- **E** is trivially simple but useless: `registry::register("room", room.any_ref())`. You get back an `AnyActorRef` that accepts `Box<dyn Any>`. No compile-time knowledge of what messages the actor handles.

- **F** is the most natural fit for a registry. The registry maps `name → Pid`, and PID-based dispatch handles the rest. This mirrors Erlang exactly: `register(room, Pid)`, `whereis(room) → Pid`. The registry is simple; the complexity moves to the PID dispatch table. But the same runtime safety concern applies — sending to a Pid that doesn't handle the message type fails at runtime.

---

## Macro Improvement Potential

Approach A's `actor_api!` macro eliminates extension trait boilerplate by generating a trait + impl from a compact declaration. Could similar macros reduce boilerplate in the other approaches?

### Approach B: Protocol Traits — DONE

The `#[protocol]` and `#[actor]` macros on [`feat/approach-b`](https://github.com/lambdaclass/spawned/tree/feat/approach-b) fully eliminate all boilerplate that was previously identified as "macro improvement potential":

| Previously manual | Now auto-generated by |
|---|---|
| Message structs + `impl Message` | `#[protocol]` (submodule with structs) |
| Bridge impls (`impl Protocol for ActorRef<T>`) | `#[protocol]` (blanket impls) |
| Conversion helpers (`to_room_ref()`) | `#[protocol]` (converter trait + impls) |
| `impl Actor for T {}` | `#[actor]` (auto-generated) |
| `impl Handler<M> for T` | `#[actor]` (`#[send_handler]`/`#[request_handler]`) |
| Protocol assertion | `#[actor(protocol = X)]` (compile-time check) |
| `actor_api!` for caller API | No longer needed — protocol methods work directly on `ActorRef` |

The user writes: (1) a protocol trait with `#[protocol]`, (2) an actor struct, (3) `#[actor(protocol = X)] impl T { handlers }`. Everything else is generated.

### Approach C: Typed Wrappers — NO

The fundamental problem is the dual-channel architecture, not boilerplate. The `Clone` bound incompatibility between enum messages and `Recipient<M>` creates a structural split that macros can't paper over. Typed wrappers still hide `unreachable!()` branches internally.

### Approach D: Derive Macro — N/A

This approach IS a macro. The `#[derive(ActorMessages)]` would generate message structs, `Message` impls, API wrappers, and `Handler<M>` delegation — subsuming what `actor_api!`, `send_messages!`, and `#[actor]` do separately. Adding `actor_api!` on top would be redundant.

### Approach E: AnyActorRef — NO

You could wrap `send_any(Box::new(...))` in typed helper methods, but this provides false safety — the runtime dispatch can still fail. The whole point of AnyActorRef is erasing types; adding typed wrappers on top contradicts that.

### Approach F: PID — PARTIAL

The registration boilerplate could be automated:

```rust
// Current: manual registration per message type
room.register::<Say>();
room.register::<Join>();
room.register::<Members>();

// Potential: derive-style auto-registration
#[actor(register(Say, Join, Members))]
impl ChatRoom { ... }
```

And `spawned::send(pid, Msg { ... })` could get ergonomic wrappers similar to `actor_api!`. But since `Pid` carries no type information, these wrappers can only provide ergonomics, not safety — a wrong Pid still causes a runtime error.

### Summary

| Approach | Macro potential | Status | Worth implementing? |
|----------|----------------|--------|---------------------|
| **B: Protocol Traits** | High | **Done** — `#[protocol]` + `#[actor]` | Implemented on `feat/approach-b` |
| **C: Typed Wrappers** | None | N/A — structural problem | No |
| **D: Derive Macro** | N/A | Already a macro | N/A |
| **E: AnyActorRef** | None | Would add false safety | No |
| **F: PID** | Low-Medium | Registration ceremony | Maybe — ergonomics only |

**Takeaway:** Approach B's macro potential has been fully realized. The `#[protocol]` macro generates message structs, blanket impls, and converter traits from just the protocol trait definition. Combined with `#[actor(protocol = X)]`, the total code for a protocol-based actor is now *less* than Approach A while retaining B's testability, Erlang-like actor-level granularity, and explicit protocol contracts.

---

## Comparison Matrix

### Functional Dimensions

| Dimension | A: Recipient | B: Protocol Traits | C: Typed Wrappers | D: Derive Macro | E: AnyActorRef | F: PID |
|-----------|-------------|-------------------|-------------------|-----------------|---------------|--------|
| **Status** | Implemented | Implemented | Design only | Design only | Design only | Design only |
| **Breaking** | Yes | Yes | No | No | Yes | Yes |
| **#144 type safety** | Full | Full | Hidden `unreachable!` | Hidden `unreachable!` | Full | Full |
| **#145 type safety** | Compile-time | Compile-time | Compile-time | Compile-time | Runtime only | Runtime only |
| **Macro support** | `#[actor]` + `actor_api!` + message macros | `#[protocol]` + `#[actor(protocol = X)]` | N/A (enum-based) | Derive macro | `#[actor]` | `#[actor]` |
| **Dual-mode (async+threads)** | Works | Works (auto-detected) | Complex (dual channel) | Complex | Works | Works |
| **Registry stores** | `Recipient<M>` | `RoomRef` / `Arc<dyn Protocol>` | Mixed | `Recipient<M>` | `AnyActorRef` | `Pid` |
| **Registry type safety** | Compile-time | Compile-time | Depends | Compile-time | Runtime | Runtime |

### Code Quality Dimensions

| Dimension | A: Recipient | B: Protocol Traits | C: Typed Wrappers | D: Derive Macro | E: AnyActorRef | F: PID |
|-----------|-------------|-------------------|-------------------|-----------------|---------------|--------|
| **Handler readability** | Clear: one `impl Handler<M>` or `#[send_handler]` per message | Same as A for handlers. Actor files contain only struct + handlers — no message defs, no bridge code. | Noisy: enum `match` arms + wrapper fns | Opaque: generated from enum annotations | Same as A, but callers use `Box::new` | Same as A, but callers use global `send(pid, msg)` |
| **API at a glance** | `actor_api!` block or scan Handler impls | Protocol traits (best) — `RoomProtocol` IS the API surface, `#[actor(protocol = X)]` declares the link | Typed wrapper functions | Annotated enum (good summary) | Nothing — `AnyActorRef` is opaque | Nothing — `Pid` is opaque |
| **main.rs expressivity** | `alice.say("Hi")` with `actor_api!`; `alice.send(SayToRoom{...})` without | `room.say(...)`, `room.members().await?`, `room.to_room_ref()` — protocol methods directly on ActorRef | `ChatRoom::say(&room, ...)` assoc fn | Generated methods: `room.say(...)` | `room.send_any(Box::new(...))` | `spawned::send(pid, ...)` + registration |
| **Boilerplate per message** | Struct + `actor_api!` line | Just a method signature in the `#[protocol]` trait — struct auto-generated | Enum variant + wrapper + match arm | Enum variant + annotation | Struct | Struct + registration |
| **Debugging** | Standard Rust — all code visible | Protocol traits visible; blanket impls via `cargo expand` if needed | Standard Rust | Requires `cargo expand` | Runtime errors (downcast failures) | Runtime errors (unregistered types) |
| **Testability** | Good (mock via Recipient) | Best (mock protocol trait) | Good | Good | Fair (Any-based) | Hard (global state) |

### Strategic Dimensions

| Dimension | A: Recipient | B: Protocol Traits | C: Typed Wrappers | D: Derive Macro | E: AnyActorRef | F: PID |
|-----------|-------------|-------------------|-------------------|-----------------|---------------|--------|
| **Framework complexity** | Medium | Low (2 proc macros) | High (dual channel) | Very high (proc macro) | High (dispatch) | Medium (registry) |
| **Maintenance burden** | Low — proven Actix pattern | Low — macros are thin code generators | High — two dispatch systems | High — complex macro | Medium | Medium |
| **Clustering readiness** | Needs `RemoteRecipient` | Needs remote bridge impls | Hard | Hard | Possible (serialize Any) | Excellent (Pid is location-transparent) |
| **Learning curve** | Moderate (Handler\<M\> pattern) | Low (`#[protocol]` + `#[actor]` — write traits, add annotations) | Low (old API preserved) | Low (write enum, macro does rest) | Low concept, high debugging | Low concept, high registration overhead |
| **Erlang alignment** | Actix-like | Actor-level granularity (Erlang behaviours) | Actix-like | Actix-like | Erlang-ish | Most Erlang |
| **Macro improvement potential** | Already done (`actor_api!`) | Done (`#[protocol]` + `#[actor]`) | None (structural) | N/A (is a macro) | None (false safety) | Low (ergonomics only) |

---

## Recommendation

**Approach B (Protocol Traits)** is now the recommended option:
- Fully implemented with `#[protocol]` and `#[actor(protocol = X)]` macros, all examples ported, 34 tests passing
- Lowest boilerplate of all approaches — the user writes a protocol trait and annotated handler methods; everything else (message structs, bridge impls, Actor impl, Handler impls, converters) is generated
- Protocol traits are the strongest "API at a glance" surface — `RoomProtocol` tells you exactly what a room can do
- Best testability — mock protocol traits directly without running an actor system
- `#[actor(protocol = X)]` makes the actor-to-protocol link explicit and compile-time checked
- `Response<T>` keeps protocol traits object-safe for async request-response (no RPITIT, no `BoxFuture`)
- Named parameter syntax (`protocol = X`) is future-proof for additional macro parameters
- Auto-detects async (tasks) vs sync (threads) runtime from method signatures
- Can coexist with `Recipient<M>` — use protocol traits where you want explicit contracts, `Recipient<M>` where you want per-message granularity

**Approach A (Handler\<M\> + Recipient\<M\>)** remains a solid foundation:
- Both A and B share the same core: `Handler<M>`, `#[actor]`, `#[send_handler]`/`#[request_handler]`
- A's `Recipient<M>` and `actor_api!` are still available for cases where per-message type erasure is preferred over protocol-level erasure
- The `#[protocol]` macro is additive — it builds on A's infrastructure

**Approaches C and D** try to preserve the old enum-based API but introduce significant complexity (dual-channel, or heavy code generation) to work around its limitations.

**Approaches E and F** sacrifice Rust's compile-time type safety for runtime flexibility. F (PID) may become relevant later for clustering, but is premature as the default API today.

---

## Branch Reference

| Branch | Base | Description |
|--------|------|-------------|
| `main` | — | Old enum-based API (baseline) |
| [`feat/critical-api-issues`](https://github.com/lambdaclass/spawned/tree/1ef33bf0c463543dca379463c554ccc5914c86ff) | main | Design doc for Handler\<M\> + Recipient\<M\> ([`docs/API_REDESIGN.md`](https://github.com/lambdaclass/spawned/blob/1ef33bf0c463543dca379463c554ccc5914c86ff/docs/API_REDESIGN.md)) |
| [`feat/handler-api-v0.5`](https://github.com/lambdaclass/spawned/tree/34bf9a759cda72e5311efda8f1fc8a5ae515129a) | main | Handler\<M\> + Recipient\<M\> implementation |
| [`feat/actor-macro-registry`](https://github.com/lambdaclass/spawned/tree/de651ad21e2dd39babf534cb74174ae0fe3b399c) | main | Adds `#[actor]` macro + named registry on top of Handler\<M\> |
| [`feat/145-protocol-trait`](https://github.com/lambdaclass/spawned/tree/b0e5afb2c69e1f5b6ab8ee82b59582348877c819) | main | Early protocol traits approach (manual bridge impls) |
| [`feat/approach-b`](https://github.com/lambdaclass/spawned/tree/feat/approach-b) | main | **Recommended.** `#[protocol]` + `#[actor(protocol = X)]` macros with blanket impls |
| [`docs/add-project-roadmap`](https://github.com/lambdaclass/spawned/tree/426c1a9952b3ad440686c318882d570f2032666f) | main | Framework comparison with Actix and Ractor |

---

## Detailed Design Documents

- **[`docs/API_REDESIGN.md`](https://github.com/lambdaclass/spawned/blob/1ef33bf0c463543dca379463c554ccc5914c86ff/docs/API_REDESIGN.md)** (on `feat/critical-api-issues`) — Full design rationale for Handler\<M\>, Receiver\<M\>, Envelope pattern, RPITIT decision, and planned supervision traits.
- **[`docs/ALTERNATIVE_APPROACHES.md`](https://github.com/lambdaclass/spawned/blob/b0e5afb2c69e1f5b6ab8ee82b59582348877c819/docs/ALTERNATIVE_APPROACHES.md)** (on `feat/145-protocol-trait`) — Original comparison of all 5 alternative branches with execution order plan.
