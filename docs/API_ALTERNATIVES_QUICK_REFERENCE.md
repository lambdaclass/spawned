# API Redesign: Quick Reference

Condensed version of [API_ALTERNATIVES_SUMMARY.md](./API_ALTERNATIVES_SUMMARY.md). Same structure, same code examples, shorter analysis.

## Table of Contents

- [The Three Problems](#the-three-problems)
- [The Chat Room Example](#the-chat-room-example)
- [Baseline: The Old API](#baseline-the-old-api)
- [Approach A: Handler\<M\> + Recipient\<M\>](#approach-a-handlerm--recipientm)
- [Approach B: Protocol Traits](#approach-b-protocol-traits)
- [Approach C: Typed Wrappers](#approach-c-typed-wrappers)
- [Approach D: Derive Macro](#approach-d-derive-macro)
- [Approach E: AnyActorRef](#approach-e-anyactorref)
- [Approach F: PID Addressing](#approach-f-pid-addressing)
- [Registry & Service Discovery](#registry--service-discovery)
- [Comparison Matrix](#comparison-matrix)
- [Recommendation](#recommendation)
- [Branch Reference](#branch-reference)

---

## The Three Problems

**#144 — No per-message type safety.** The old API uses one enum for all requests and another for all replies. Callers must match impossible variants:

```rust
match actor.request(Request::GetName).await? {
    Reply::Name(n) => println!("{}", n),
    Reply::Age(_) => unreachable!(), // impossible but required
}
```

**#145 — Circular dependencies.** When two actors communicate bidirectionally, storing `ActorRef<A>` and `ActorRef<B>` creates a module cycle.

**Service discovery.** Real systems don't wire actors in `main.rs` — they discover each other at runtime by name. The registry API is the same across approaches, but **what you store** (and what the discoverer gets back) varies dramatically.

---

## The Chat Room Example

Every approach implements: **ChatRoom** ↔ **User** (bidirectional), `Members` request-reply, and a **late joiner (Charlie)** who discovers the room via the registry. This exercises #144, #145, and service discovery.

---

## Baseline: The Old API

Single-enum `Actor` trait with associated types for Request/Message/Reply. **Cannot build the chat room** as separate modules (no type-erasure → circular imports). Even in one file, callers must match impossible enum variants.

---

## Approach A: Handler\<M\> + Recipient\<M\>

**Status:** Implemented on `feat/approach-a`. 34 tests passing.

Each message is its own struct with `type Result`. Actors implement `Handler<M>` per message. Type erasure via `Recipient<M> = Arc<dyn Receiver<M>>`. Messages live in the module that handles them (room.rs defines Room's messages, user.rs defines User's messages).

### Without macro (manual `impl Handler<M>`)

<details>
<summary><b>room.rs</b> — defines Room's messages, imports Deliver from user</summary>

```rust
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};
use crate::user::Deliver;

// -- Messages (Room handles these) --

pub struct Say { pub from: String, pub text: String }
impl Message for Say { type Result = (); }

pub struct Join { pub name: String, pub inbox: Recipient<Deliver> }
impl Message for Join { type Result = (); }

pub struct Members;
impl Message for Members { type Result = Vec<String>; }

// -- Actor --

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

impl Handler<Members> for ChatRoom {
    async fn handle(&mut self, _msg: Members, _ctx: &Context<Self>) -> Vec<String> {
        self.members.iter().map(|(name, _)| name.clone()).collect()
    }
}
```
</details>

<details>
<summary><b>user.rs</b> — defines User's messages (including Deliver), imports Say from room</summary>

```rust
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};
use crate::room::Say;

// -- Messages (User handles these) --

pub struct Deliver { pub from: String, pub text: String }
impl Message for Deliver { type Result = (); }

pub struct SayToRoom { pub text: String }
impl Message for SayToRoom { type Result = (); }

// -- Actor --

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

let members: Vec<String> = room.send_request(Members).await?;

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
use crate::user::Deliver;

// -- Messages --

send_messages! {
    Say { from: String, text: String };
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
<summary><b>user.rs</b> — defines Deliver (User's inbox message) + macro version</summary>

```rust
use spawned_concurrency::actor_api;
use spawned_concurrency::send_messages;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};
use spawned_macros::actor;
use crate::room::{ChatRoom, ChatRoomApi};

// -- Messages --

send_messages! {
    Deliver { from: String, text: String };
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

// Register the room by name
registry::register("general", room.clone()).unwrap();

alice.join_room(room.clone()).unwrap();
bob.join_room(room.clone()).unwrap();

let members = room.members().await.unwrap();

alice.say("Hello everyone!".into()).unwrap();
bob.say("Hi Alice!".into()).unwrap();

// Late joiner discovers the room — must know the concrete type
let charlie = User::new("Charlie".into()).start();
let discovered: ActorRef<ChatRoom> = registry::whereis("general").unwrap();
charlie.join_room(discovered).unwrap();
```
</details>

### Analysis

Type erasure via `Recipient<M>` — per-message granularity. Messages live in the actor that handles them (bidirectional module imports: room↔user). `actor_api!` provides method-call syntax. Proven Actix pattern.

**Cross-crate limitation:** The bidirectional module imports work within a crate but become circular crate dependencies if Room and User are in separate crates. You'd need to extract shared types into a third crate — effectively recreating Approach B's `protocols.rs`.

**Registry trade-off:** Register `ActorRef<ChatRoom>` (full API, but discoverer must know the concrete type) or `Recipient<M>` per message (decoupled but fragmented).

---

## Approach B: Protocol Traits

**Status:** Implemented on `feat/approach-b`. 34 tests passing.

Same `Handler<M>` core as A. Solves #145 differently: each actor defines a **protocol trait** (its complete public API, like Erlang module exports). Actors communicate through `Arc<dyn Protocol>`, never `ActorRef<OtherActor>`. All types live in a shared `protocols.rs` — no bidirectional imports.

`#[protocol]` generates message structs, converter traits, and blanket impls from the trait definition. Return types determine runtime mode: `Result<(), ActorError>` → send, `Response<T>` → async request, `Result<T, ActorError>` → sync request. `Response<T>` keeps traits object-safe (no RPITIT, no BoxFuture).

### Without macro (expanded reference)

<details>
<summary><b>protocols.rs</b> — traits + message structs + blanket impls (all manual)</summary>

```rust
use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, ActorRef, Handler, Response};
use std::sync::Arc;

// --- Type aliases (manually declared for cross-protocol references) ---

pub type RoomRef = Arc<dyn RoomProtocol>;
pub type UserRef = Arc<dyn UserProtocol>;

// --- RoomProtocol ---

pub trait RoomProtocol: Send + Sync {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn add_member(&self, name: String, user: UserRef) -> Result<(), ActorError>;
    fn members(&self) -> Response<Vec<String>>;
}

pub mod room_protocol {
    use super::*;

    pub struct Say { pub from: String, pub text: String }
    impl Message for Say { type Result = (); }

    pub struct AddMember { pub name: String, pub user: UserRef }
    impl Message for AddMember { type Result = (); }

    pub struct Members;
    impl Message for Members { type Result = Vec<String>; }
}

pub trait ToRoomRef {
    fn to_room_ref(&self) -> RoomRef;
}

impl ToRoomRef for RoomRef {
    fn to_room_ref(&self) -> RoomRef {
        Arc::clone(self)
    }
}

impl<A: Actor + Handler<room_protocol::Say> + Handler<room_protocol::AddMember> + Handler<room_protocol::Members>>
    RoomProtocol for ActorRef<A>
{
    fn say(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(room_protocol::Say { from, text })
    }

    fn add_member(&self, name: String, user: UserRef) -> Result<(), ActorError> {
        self.send(room_protocol::AddMember { name, user })
    }

    fn members(&self) -> Response<Vec<String>> {
        Response::from(self.request_raw(room_protocol::Members))
    }
}

impl<A: Actor + Handler<room_protocol::Say> + Handler<room_protocol::AddMember> + Handler<room_protocol::Members>>
    ToRoomRef for ActorRef<A>
{
    fn to_room_ref(&self) -> RoomRef {
        Arc::new(self.clone())
    }
}

// --- UserProtocol ---

pub trait UserProtocol: Send + Sync {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError>;
    fn say(&self, text: String) -> Result<(), ActorError>;
    fn join_room(&self, room: RoomRef) -> Result<(), ActorError>;
}

pub mod user_protocol {
    use super::*;

    pub struct Deliver { pub from: String, pub text: String }
    impl Message for Deliver { type Result = (); }

    pub struct Say { pub text: String }
    impl Message for Say { type Result = (); }

    pub struct JoinRoom { pub room: RoomRef }
    impl Message for JoinRoom { type Result = (); }
}

pub trait ToUserRef {
    fn to_user_ref(&self) -> UserRef;
}

impl ToUserRef for UserRef {
    fn to_user_ref(&self) -> UserRef {
        Arc::clone(self)
    }
}

impl<A: Actor + Handler<user_protocol::Deliver> + Handler<user_protocol::Say> + Handler<user_protocol::JoinRoom>>
    UserProtocol for ActorRef<A>
{
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(user_protocol::Deliver { from, text })
    }

    fn say(&self, text: String) -> Result<(), ActorError> {
        self.send(user_protocol::Say { text })
    }

    fn join_room(&self, room: RoomRef) -> Result<(), ActorError> {
        self.send(user_protocol::JoinRoom { room })
    }
}

impl<A: Actor + Handler<user_protocol::Deliver> + Handler<user_protocol::Say> + Handler<user_protocol::JoinRoom>>
    ToUserRef for ActorRef<A>
{
    fn to_user_ref(&self) -> UserRef {
        Arc::new(self.clone())
    }
}
```
</details>

<details>
<summary><b>room.rs</b> — manual Actor + Handler impls + protocol assertion</summary>

```rust
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};

use crate::protocols::room_protocol::{AddMember, Members, Say};
use crate::protocols::{RoomProtocol, UserRef};

pub struct ChatRoom {
    members: Vec<(String, UserRef)>,
}

impl ChatRoom {
    pub fn new() -> Self {
        Self { members: Vec::new() }
    }
}

impl Actor for ChatRoom {}

impl Handler<Say> for ChatRoom {
    async fn handle(&mut self, msg: Say, _ctx: &Context<Self>) {
        tracing::info!("[room] {} says: {}", msg.from, msg.text);
        for (name, user) in &self.members {
            if *name != msg.from {
                let _ = user.deliver(msg.from.clone(), msg.text.clone());
            }
        }
    }
}

impl Handler<AddMember> for ChatRoom {
    async fn handle(&mut self, msg: AddMember, _ctx: &Context<Self>) {
        tracing::info!("[room] {} joined", msg.name);
        self.members.push((msg.name, msg.user));
    }
}

impl Handler<Members> for ChatRoom {
    async fn handle(&mut self, _msg: Members, _ctx: &Context<Self>) -> Vec<String> {
        self.members.iter().map(|(name, _)| name.clone()).collect()
    }
}

// Compile-time assertion: ActorRef<ChatRoom> must satisfy RoomProtocol
const _: () = {
    fn _assert<T: RoomProtocol>() {}
    fn _check() { _assert::<ActorRef<ChatRoom>>(); }
};
```
</details>

<details>
<summary><b>user.rs</b> — same pattern, manual Actor + Handler impls</summary>

```rust
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};

use crate::protocols::user_protocol::{Deliver, JoinRoom, Say};
use crate::protocols::{RoomRef, ToUserRef, UserProtocol};

pub struct User {
    name: String,
    room: Option<RoomRef>,
}

impl User {
    pub fn new(name: String) -> Self {
        Self { name, room: None }
    }
}

impl Actor for User {}

impl Handler<Deliver> for User {
    async fn handle(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }
}

impl Handler<Say> for User {
    async fn handle(&mut self, msg: Say, _ctx: &Context<Self>) {
        if let Some(ref room) = self.room {
            let _ = room.say(self.name.clone(), msg.text);
        }
    }
}

impl Handler<JoinRoom> for User {
    async fn handle(&mut self, msg: JoinRoom, ctx: &Context<Self>) {
        let _ = msg.room.add_member(self.name.clone(), ctx.actor_ref().to_user_ref());
        self.room = Some(msg.room);
    }
}

// Compile-time assertion: ActorRef<User> must satisfy UserProtocol
const _: () = {
    fn _assert<T: UserProtocol>() {}
    fn _check() { _assert::<ActorRef<User>>(); }
};
```
</details>

<details>
<summary><b>main.rs</b> — identical to macro version (no macros used here)</summary>

```rust
let room = ChatRoom::new().start();
let alice = User::new("Alice".into()).start();
let bob = User::new("Bob".into()).start();

// Register the room's protocol — not the concrete type
registry::register("general", room.to_room_ref()).unwrap();

alice.join_room(room.to_room_ref()).unwrap();
bob.join_room(room.to_room_ref()).unwrap();

let members = room.members().await.unwrap();

alice.say("Hello everyone!".into()).unwrap();
bob.say("Hey Alice!".into()).unwrap();

// Late joiner discovers the room — only needs the protocol, not the concrete type
let charlie = User::new("Charlie".into()).start();
let discovered: RoomRef = registry::whereis("general").unwrap();
charlie.join_room(discovered).unwrap();
```

Protocol methods (`say`, `join_room`, `members`) are called directly on `ActorRef` via blanket impls. The `.to_room_ref()` conversion makes the protocol boundary explicit.
</details>

### With `#[protocol]` + `#[actor]` macros

<details>
<summary><b>protocols.rs</b> — one protocol per actor, <code>#[protocol]</code> generates everything</summary>

```rust
use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::Response;
use spawned_macros::protocol;
use std::sync::Arc;

// Manual type aliases for circular references between protocols
pub type RoomRef = Arc<dyn RoomProtocol>;
pub type UserRef = Arc<dyn UserProtocol>;

// #[protocol] generates for each trait:
//   Conversion trait: pub trait ToRoomRef { fn to_room_ref(&self) -> RoomRef; }
//   Identity impl:    impl ToRoomRef for RoomRef { ... }
//   Message structs:  room_protocol::{Say, AddMember, Members} with Message impls
//   Blanket impl:     impl<A: Actor + Handler<Say> + ...> RoomProtocol for ActorRef<A>
//   Blanket impl:     impl<A: Actor + Handler<Say> + ...> ToRoomRef for ActorRef<A>

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
</details>

<details>
<summary><b>room.rs</b> — <code>#[actor(protocol = RoomProtocol)]</code> generates Actor impl, Handler impls, and assertion</summary>

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
</details>

<details>
<summary><b>user.rs</b> — <code>#[actor(protocol = UserProtocol)]</code>, symmetric with room.rs</summary>

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
<summary><b>main.rs</b> — protocol refs for wiring and discovery</summary>

```rust
let room = ChatRoom::new().start();
let alice = User::new("Alice".into()).start();
let bob = User::new("Bob".into()).start();

// Register the room's protocol — not the concrete type
registry::register("general", room.to_room_ref()).unwrap();

alice.join_room(room.to_room_ref()).unwrap();
bob.join_room(room.to_room_ref()).unwrap();

let members = room.members().await.unwrap();

alice.say("Hello everyone!".into()).unwrap();
bob.say("Hey Alice!".into()).unwrap();

// Late joiner discovers the room — only needs the protocol, not the concrete type
let charlie = User::new("Charlie".into()).start();
let discovered: RoomRef = registry::whereis("general").unwrap();
charlie.join_room(discovered).unwrap();
```
</details>

### Analysis

Protocol traits ARE the API — `RoomProtocol` tells you everything a room can do. All types in shared `protocols.rs` — zero cross-actor dependencies, crate-ready from day one. Best testability (mock protocol traits directly). Lowest boilerplate with macros: `#[protocol]` trait + `#[actor(protocol = X)]` handlers.

**Registry:** One registration per protocol, full API, discoverer only needs the protocol trait (not the concrete type). This is the cleanest discovery story.

---

## Approach C: Typed Wrappers

**Status:** Design only.

Keeps the old enum `Actor` trait. Adds typed convenience methods hiding enum matching. For #145, adds a second envelope-based channel alongside the enum channel.

**Fatal flaw:** The old `Message` requires `Clone`, but `Recipient<M>` (Arc-based) can't always satisfy it. Cross-boundary messages are forced onto the second channel, splitting actor logic across two dispatch systems. More confusion than a clean break.

---

## Approach D: Derive Macro

**Status:** Design only.

`#[derive(ActorMessages)]` on an enum auto-generates per-variant message structs, `Message` impls, typed wrappers, and `Handler<M>` delegation.

Compact definition but generated code is invisible without `cargo expand`. Largest blast radius of any macro approach — must handle `Recipient<M>` in fields, mixed send/request variants, `Clone` bounds. Trades visibility for conciseness.

---

## Approach E: AnyActorRef

**Status:** Design only.

Fully type-erased `AnyActorRef = Arc<dyn AnyActor>` using `Box<dyn Any>`. Callers use `send_any(Box::new(...))` and must `downcast()` replies. Solves #145 by erasing all types, but also erases compile-time safety. Wrong message types → runtime panics.

---

## Approach F: PID Addressing

**Status:** Design only.

Every actor gets a `Pid(u64)`. Global registry maps `(Pid, TypeId)` → sender. Most Erlang-faithful, best for clustering (location-transparent). But: unregistered message type or dead PID → runtime error. Requires explicit `room.register::<Say>()` per message type.

---

## Registry & Service Discovery

Same API everywhere (`register`, `whereis`, `unregister`). The key difference is what you store and retrieve:

```rust
// Approach A — discoverer must know the concrete type
registry::register("general", room.clone()).unwrap();
let discovered: ActorRef<ChatRoom> = registry::whereis("general").unwrap();

// Approach B — discoverer only needs the protocol
registry::register("general", room.to_room_ref()).unwrap();
let discovered: RoomRef = registry::whereis("general").unwrap();
```

| Approach | Stored value | Type safety | Granularity |
|----------|-------------|-------------|-------------|
| **A: Recipient** | `ActorRef<A>` or `Recipient<M>` | Compile-time (but coupled or fragmented) | Per actor or per message |
| **B: Protocol** | `Arc<dyn Protocol>` | Compile-time, decoupled | Per protocol (one reg, full API) |
| **E: AnyActorRef** | `AnyActorRef` | Runtime only | Per actor, no type info |
| **F: PID** | `Pid` | Runtime only | Per actor |

---

## Comparison Matrix

| Dimension | A: Recipient | B: Protocol | C: Typed Wrappers | D: Derive | E: AnyActorRef | F: PID |
|-----------|:-----------:|:-----------:|:-----------------:|:---------:|:--------------:|:------:|
| **Implemented** | Yes | Yes | No | No | No | No |
| **Breaking change** | Yes | Yes | No | No | Yes | Yes |
| **#144 type safety** | Full | Full | Hidden unreachable! | Hidden unreachable! | Full | Full |
| **#145 type safety** | Compile-time | Compile-time | Compile-time | Compile-time | Runtime | Runtime |
| **API at a glance** | `actor_api!` block | Protocol trait (best) | Wrapper fns | Annotated enum | Opaque | Opaque |
| **Boilerplate per msg** | struct + `actor_api!` line + handler | protocol method + handler | enum variant + wrapper + match | enum variant + annotation | struct | struct + registration |
| **main.rs ergonomics** | `alice.say("Hi")` | `room.say(...)`, `room.to_room_ref()` | `ChatRoom::say(&room, ...)` | `room.say(...)` | `send_any(Box::new(...))` | `send(pid, ...)` |
| **Registry discovery** | Must know concrete type or per-msg handles | Only needs protocol trait | Depends | Same as A | No type info | No type info |
| **Testability** | Good (mock Recipient) | Best (mock traits) | Good | Good | Fair | Hard |
| **Cross-crate ready** | Needs restructuring (bidirectional imports) | Yes (`protocols.rs` → shared crate) | N/A | N/A | N/A | N/A |
| **Dual-mode (async+threads)** | Works | Auto-detected | Complex | Complex | Works | Works |
| **Clustering readiness** | Needs RemoteRecipient | Needs remote bridge | Hard | Hard | Possible | Excellent |
| **Erlang alignment** | Actix-like | Most Erlang (protocol = module exports) | Actix-like | Actix-like | Erlang-ish | Erlang PIDs |

---

## Recommendation

**Approach A** is the most mature and balanced:
- Proven Actix pattern, 34 tests, full macro support
- `actor_api!` provides clean method-call syntax
- Registry trade-off: full API requires knowing concrete type, or per-message handles are fragmented
- Cross-crate: bidirectional imports need restructuring into a shared types crate

**Approach B** is the strongest alternative with the most Erlang-like architecture:
- One protocol per actor = Erlang gen_server module exports
- `#[protocol]` + `#[actor(protocol = X)]` — lowest boilerplate, no separate API macros needed
- Best registry story: one registration, full API, no concrete type
- Best testability: mock protocol traits directly
- Crate-ready from day one: `protocols.rs` maps directly to a shared crate

**C/D** add complexity trying to preserve the old enum API. **E/F** sacrifice compile-time safety (F may matter later for clustering).

---

## Branch Reference

| Branch | Description |
|--------|-------------|
| `main` | Old enum-based API (baseline) |
| [`feat/approach-a`](https://github.com/lambdaclass/spawned/tree/feat/approach-a) | **Approach A** — Recipient\<M\> + actor_api! |
| [`feat/approach-b`](https://github.com/lambdaclass/spawned/tree/feat/approach-b) | **Approach B** — `#[protocol]` + `#[actor(protocol = X)]` |
| [`feat/actor-macro-registry`](https://github.com/lambdaclass/spawned/tree/de651ad21e2dd39babf534cb74174ae0fe3b399c) | `#[actor]` macro + named registry |
| `docs/api-comparison` | This document + full detailed comparison |

See [API_ALTERNATIVES_SUMMARY.md](./API_ALTERNATIVES_SUMMARY.md) for the full detailed comparison with expanded analysis tables.
