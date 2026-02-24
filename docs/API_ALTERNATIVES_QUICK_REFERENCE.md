# API Redesign: Quick Reference

Condensed version of [API_ALTERNATIVES_SUMMARY.md](./API_ALTERNATIVES_SUMMARY.md). Same structure, same code examples, shorter analysis.

## Table of Contents

- [The Two Problems](#the-two-problems)
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

## The Two Problems

**#144 — No per-message type safety.** The old API uses one enum for all requests and another for all replies. Callers must match impossible variants:

```rust
match actor.request(Request::GetName).await? {
    Reply::Name(n) => println!("{}", n),
    Reply::Age(_) => unreachable!(), // impossible but required
}
```

**#145 — Circular dependencies.** When two actors communicate bidirectionally, storing `ActorRef<A>` and `ActorRef<B>` creates a module cycle.

---

## The Chat Room Example

Every approach implements: **ChatRoom** (holds members, broadcasts) ↔ **User** (receives messages, speaks to room). This exercises both typed request-reply (#144) and circular dependency breaking (#145).

---

## Baseline: The Old API

Single-enum `Actor` trait with associated types for Request/Message/Reply. **Cannot build the chat room** as separate modules (no type-erasure mechanism → circular imports). Even in one file, callers must match impossible enum variants.

---

## Approach A: Handler\<M\> + Recipient\<M\>

**Status:** Implemented. 34 tests passing.

Each message is its own struct with `type Result`. Actors implement `Handler<M>` per message. Type erasure via `Recipient<M> = Arc<dyn Receiver<M>>`.

### Without macro (manual `impl Handler<M>`)

<details>
<summary><b>messages.rs</b> — shared types, no actor types mentioned</summary>

```rust
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::Recipient;

pub struct Say { pub from: String, pub text: String }
impl Message for Say { type Result = (); }

pub struct SayToRoom { pub text: String }
impl Message for SayToRoom { type Result = (); }

pub struct Deliver { pub from: String, pub text: String }
impl Message for Deliver { type Result = (); }

pub struct Join { pub name: String, pub inbox: Recipient<Deliver> }
impl Message for Join { type Result = (); }

pub struct Members;
impl Message for Members { type Result = Vec<String>; }
```
</details>

<details>
<summary><b>room.rs</b> — knows messages, not User</summary>

```rust
use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};
use crate::messages::{Deliver, Join, Members, Say};

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

Type erasure via `Recipient<M>` — fine-grained (per message type). `actor_api!` provides ergonomic method-call syntax. Proven pattern (Actix). Each `impl Handler<M>` is self-contained but can feel scattered across many small blocks.

---

## Approach B: Protocol Traits

**Status:** Implemented. All examples ported. Two proc macros: `#[protocol]` and `#[actor]`.

Users define protocol traits with `#[protocol]` — the macro generates message structs, converter traits, and blanket impls. `#[actor(protocol = X)]` auto-generates `impl Actor`, `Handler<M>` impls, and a compile-time protocol assertion.

**Return type → runtime mode:** `Result<(), ActorError>` → send | `Response<T>` → async request (tasks) | `Result<T, ActorError>` → sync request (threads). Send-only protocols get blanket impls for both runtimes.

**`Response<T>`** wraps a oneshot receiver, keeps protocol traits object-safe (no RPITIT, no `BoxFuture`). Structural mirror of the Envelope pattern on the receive side.

### Without macro (expanded reference)

<details>
<summary><b>protocols.rs</b> — traits + message structs + blanket impls (all manual)</summary>

```rust
use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{ActorRef, Actor, Handler, Response};
use std::sync::Arc;

// --- Type aliases (same as macro version) ---

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
    fn _assert_bridge<T: RoomProtocol>() {}
    fn _check() { _assert_bridge::<ActorRef<ChatRoom>>(); }
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
    fn _assert_bridge<T: UserProtocol>() {}
    fn _check() { _assert_bridge::<ActorRef<User>>(); }
};
```
</details>

<details>
<summary><b>main.rs</b> — identical to macro version (no macros used here)</summary>

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

### With `#[protocol]` + `#[actor]` macros

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

Protocol traits ARE the API surface — `RoomProtocol` tells you everything a room can do. Best testability (mock protocol traits directly). Lowest boilerplate with macros: one `#[protocol]` trait + `#[actor(protocol = X)]` handlers. The non-macro expansion reveals all machinery (blanket impls, converters) in standard Rust.

---

## Approach C: Typed Wrappers

**Status:** Design only.

Keeps the old enum `Actor` trait. Adds typed convenience methods hiding enum matching. For #145, adds a second envelope-based channel alongside the enum channel.

**Fatal flaw:** The old `Message` requires `Clone`, but `Recipient<M>` (Arc-based) can't always satisfy it. Cross-boundary messages are forced onto the second channel, splitting actor logic across two dispatch systems. More confusion than a clean break.

---

## Approach D: Derive Macro

**Status:** Design only.

`#[derive(ActorMessages)]` on an enum auto-generates per-variant message structs, `Message` impls, typed wrappers, and `Handler<M>` delegation.

Compact definition (one annotated enum covers everything), but generated code is invisible without `cargo expand`. Largest blast radius of any macro approach. Trades visibility for conciseness.

---

## Approach E: AnyActorRef

**Status:** Design only.

Fully type-erased handle `AnyActorRef = Arc<dyn AnyActor>` using `Box<dyn Any>`. Callers use `send_any(Box::new(...))` and must `downcast()` replies.

Solves #145 by erasing all type information, but also erases compile-time safety. Wrong message types become runtime panics. Defeats Rust's core value proposition.

---

## Approach F: PID Addressing

**Status:** Design only.

Every actor gets a `Pid(u64)`. Global registry maps `(Pid, TypeId)` → message sender. Most Erlang-faithful. Best positioned for clustering (location-transparent identifiers).

But: sending to a dead PID or unregistered message type is a runtime error. Requires explicit `room.register::<Say>()` per message type — ceremony with no business logic value.

---

## Registry & Service Discovery

All approaches use the same registry API (`register`, `whereis`, `unregister`). What differs is what you store and retrieve:

| Approach | Stored value | Type safety | Granularity |
|----------|-------------|-------------|-------------|
| **A: Recipient** | `Recipient<M>` | Compile-time | Per message type |
| **B: Protocol** | `Arc<dyn Protocol>` | Compile-time | Per protocol |
| **E: AnyActorRef** | `AnyActorRef` | Runtime only | Per actor |
| **F: PID** | `Pid` | Runtime only | Per actor |

A/D register per message type (most granular). B registers per protocol (one registration covers all methods). E/F store opaque handles with no compile-time knowledge.

---

## Comparison Matrix

| Dimension | A: Recipient | B: Protocol | C: Typed Wrappers | D: Derive | E: AnyActorRef | F: PID |
|-----------|:-----------:|:-----------:|:-----------------:|:---------:|:--------------:|:------:|
| **Implemented** | Yes | Yes | No | No | No | No |
| **Breaking change** | Yes | Yes | No | No | Yes | Yes |
| **#144 type safety** | Full | Full | Hidden unreachable! | Hidden unreachable! | Full | Full |
| **#145 type safety** | Compile-time | Compile-time | Compile-time | Compile-time | Runtime | Runtime |
| **API at a glance** | `actor_api!` block | Protocol trait (best) | Wrapper fns | Annotated enum | Opaque | Opaque |
| **Boilerplate** | Medium | Lowest (with macros) | High | Low (but opaque) | Low | Low + registration |
| **main.rs ergonomics** | `alice.say("Hi")` | `room.say(...)` | `ChatRoom::say(&room, ...)` | `room.say(...)` | `send_any(Box::new(...))` | `send(pid, ...)` |
| **Testability** | Good | Best (mock traits) | Good | Good | Fair | Hard |
| **Debugging** | Standard Rust | Traits visible; `cargo expand` for impls | Standard Rust | Needs `cargo expand` | Runtime errors | Runtime errors |
| **Dual-mode (async+threads)** | Works | Auto-detected | Complex | Complex | Works | Works |
| **Clustering readiness** | Needs RemoteRecipient | Needs remote bridge | Hard | Hard | Possible | Excellent |
| **Erlang alignment** | Actix-like | Behaviours-like | Actix-like | Actix-like | Erlang-ish | Most Erlang |

---

## Recommendation

**Approach B (Protocol Traits)** is recommended:
- Lowest boilerplate: write a `#[protocol]` trait + `#[actor(protocol = X)]` handlers; everything else is generated
- Protocol traits are the clearest API surface — `RoomProtocol` IS the contract
- Best testability — mock protocol traits directly
- Compile-time protocol checking via `#[actor(protocol = X)]`
- `Response<T>` keeps traits object-safe (no RPITIT, no BoxFuture)
- Auto-detects async vs sync from method signatures
- Can coexist with A's `Recipient<M>` for per-message granularity when needed

**Approach A** remains a solid foundation — both share `Handler<M>`, `#[actor]`, `#[send_handler]`/`#[request_handler]`. B's `#[protocol]` is additive.

**C/D** add significant complexity trying to preserve the old enum API. **E/F** sacrifice compile-time safety for runtime flexibility (F may be relevant later for clustering).

---

## Branch Reference

| Branch | Description |
|--------|-------------|
| `main` | Old enum-based API (baseline) |
| `feat/handler-api-v0.5` | Handler\<M\> + Recipient\<M\> implementation |
| `feat/actor-macro-registry` | `#[actor]` macro + named registry |
| **`feat/approach-b`** | **Recommended.** `#[protocol]` + `#[actor(protocol = X)]` macros |
| `docs/api-alternatives-summary` | This document + full detailed comparison |

See [API_ALTERNATIVES_SUMMARY.md](./API_ALTERNATIVES_SUMMARY.md) for the full detailed comparison with expanded analysis tables.
