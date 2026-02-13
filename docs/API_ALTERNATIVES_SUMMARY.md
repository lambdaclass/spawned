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

**Branches:** [`feat/handler-api-v0.5`](../../tree/34bf9a7) (implementation), [`feat/critical-api-issues`](../../tree/1ef33bf) (design doc), [`feat/actor-macro-registry`](../../tree/de651ad) (adds macro + registry)

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

**Branch:** [`feat/145-protocol-trait`](../../tree/b0e5afb) (WIP, committed)

**Status:** WIP. Full implementation + migrated examples committed.

Uses the same `Handler<M>` and `#[actor]` macro as Approach A for #144. Solves #145 differently: instead of `Recipient<M>`, actors communicate across boundaries via explicit user-defined trait objects.

### Full chat room code

<details>
<summary><b>protocols.rs</b> — shared contracts, neither actor type mentioned</summary>

```rust
use spawned_concurrency::error::ActorError;
use std::sync::Arc;

pub trait ChatParticipant: Send + Sync {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError>;
}

pub trait ChatBroadcaster: Send + Sync {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn add_member(&self, name: String, inbox: Arc<dyn ChatParticipant>) -> Result<(), ActorError>;
}
```
</details>

<details>
<summary><b>messages.rs</b> — internal message structs</summary>

```rust
use spawned_concurrency::messages;

messages! {
    Say { from: String, text: String } -> ();
    SayToRoom { text: String } -> ();
    Deliver { from: String, text: String } -> ();
}
```
</details>

<details>
<summary><b>room.rs</b> — actor + bridge impl</summary>

```rust
use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};
use std::sync::Arc;
use crate::messages::Say;
use crate::protocols::{ChatBroadcaster, ChatParticipant};

// Join carries an Arc<dyn ChatParticipant> — manually defined (can't use message macro)
pub struct Join {
    pub name: String,
    pub inbox: Arc<dyn ChatParticipant>,
}
impl Message for Join { type Result = (); }

pub struct ChatRoom {
    members: Vec<(String, Arc<dyn ChatParticipant>)>,
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
                let _ = inbox.deliver(msg.from.clone(), msg.text.clone());
            }
        }
    }
}

// Bridge: ActorRef<ChatRoom> implements the protocol trait
impl ChatBroadcaster for ActorRef<ChatRoom> {
    fn say(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(Say { from, text })
    }
    fn add_member(&self, name: String, inbox: Arc<dyn ChatParticipant>) -> Result<(), ActorError> {
        self.send(Join { name, inbox })
    }
}
```
</details>

<details>
<summary><b>user.rs</b> — actor + bridge impl</summary>

```rust
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};
use std::sync::Arc;
use crate::messages::{Deliver, SayToRoom};
use crate::protocols::{ChatBroadcaster, ChatParticipant};

pub struct User {
    pub name: String,
    pub room: Arc<dyn ChatBroadcaster>,  // protocol trait, not ActorRef<ChatRoom>
}

impl Actor for User {}

impl Handler<SayToRoom> for User {
    async fn handle(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        let _ = self.room.say(self.name.clone(), msg.text);
    }
}

impl Handler<Deliver> for User {
    async fn handle(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }
}

// Bridge: ActorRef<User> implements the protocol trait
impl ChatParticipant for ActorRef<User> {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(Deliver { from, text })
    }
}
```
</details>

<details>
<summary><b>main.rs</b></summary>

```rust
let room = ChatRoom::new().start();
let alice = User { name: "Alice".into(), room: Arc::new(room.clone()) }.start();
let bob = User { name: "Bob".into(), room: Arc::new(room.clone()) }.start();

room.add_member("Alice".into(), Arc::new(alice.clone())).unwrap();
room.add_member("Bob".into(), Arc::new(bob.clone())).unwrap();

alice.send_request(SayToRoom { text: "Hello everyone!".into() }).await?;
```
</details>

### Analysis

| Dimension | Assessment |
|-----------|-----------|
| **Readability** | `protocols.rs` is an excellent summary of what crosses the actor boundary. But the bridge impls (`impl ChatBroadcaster for ActorRef<ChatRoom>`) are pure boilerplate — each method just wraps `self.send(MessageStruct { ... })`. |
| **API at a glance** | The protocol traits serve as a natural API contract. Looking at `ChatBroadcaster` tells you exactly what a room can do, with named methods and their signatures. This is the strongest "at-a-glance" surface of all approaches. |
| **Boilerplate** | Higher than Approach A. For each cross-actor boundary you need: (1) a protocol trait, (2) a bridge impl, (3) the message structs, and (4) the Handler impls. That's 4 layers of code. With Approach A's `Recipient<M>`, the bridge layer disappears entirely. |
| **main.rs expressivity** | Protocol methods are directly callable: `room.say(...)`, `room.add_member(...)`. But wiring requires `Arc::new()` wrapping: `Arc::new(room.clone())`, `Arc::new(alice.clone())`. |
| **Circular dep solution** | Actors hold `Arc<dyn ProtocolTrait>` instead of `ActorRef<OtherActor>`. Clean in concept but each new message type crossing the boundary requires adding a method to the protocol trait + updating the bridge impl. |
| **Macro compatibility** | The `#[actor]` macro works for the Handler impls, but bridge impls must still be written manually. The protocol trait itself has no macro support. |
| **Testability** | Best of all approaches — you can mock `ChatBroadcaster` or `ChatParticipant` directly in unit tests without running an actor system. |

**Key insight:** Protocol traits excel as documentation and testing contracts. But they duplicate information: the protocol trait method `fn say(&self, from: String, text: String)` mirrors the message struct `Say { from: String, text: String }` and the bridge impl just connects them. In Approach A, `Recipient<Say>` removes this duplication — the message struct *is* the contract.

**Scaling concern:** In a system with N actor types and M message types crossing boundaries, Approach A needs M message structs. Approach B needs M message structs + P protocol traits + P bridge impls, where P grows with the number of distinct actor-to-actor interaction patterns.

---

## Approach C: Typed Wrappers (non-breaking)

**Branch:** Not implemented. Documented in [`docs/ALTERNATIVE_APPROACHES.md`](../../blob/b0e5afb/docs/ALTERNATIVE_APPROACHES.md).

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

**Branch:** Not implemented. Documented in [`docs/ALTERNATIVE_APPROACHES.md`](../../blob/b0e5afb/docs/ALTERNATIVE_APPROACHES.md).

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

**Branch:** Not implemented. Documented in [`docs/ALTERNATIVE_APPROACHES.md`](../../blob/b0e5afb/docs/ALTERNATIVE_APPROACHES.md).

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

**Branch:** Not implemented. Documented in [`docs/ALTERNATIVE_APPROACHES.md`](../../blob/b0e5afb/docs/ALTERNATIVE_APPROACHES.md).

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

- **B** registers per protocol: `registry::register("room", Arc::new(room.clone()) as Arc<dyn ChatBroadcaster>)`. A consumer discovers an `Arc<dyn ChatBroadcaster>` — it can call any method on the protocol (`say`, `add_member`). This is coarser but more natural: one registration covers all the methods in the protocol.

- **E** is trivially simple but useless: `registry::register("room", room.any_ref())`. You get back an `AnyActorRef` that accepts `Box<dyn Any>`. No compile-time knowledge of what messages the actor handles.

- **F** is the most natural fit for a registry. The registry maps `name → Pid`, and PID-based dispatch handles the rest. This mirrors Erlang exactly: `register(room, Pid)`, `whereis(room) → Pid`. The registry is simple; the complexity moves to the PID dispatch table. But the same runtime safety concern applies — sending to a Pid that doesn't handle the message type fails at runtime.

---

## Macro Improvement Potential

Approach A's `actor_api!` macro eliminates extension trait boilerplate by generating a trait + impl from a compact declaration. Could similar macros reduce boilerplate in the other approaches?

### Approach B: Protocol Traits — YES, significant potential

The bridge impls are structurally identical to what `actor_api!` already generates. Each bridge method just wraps `self.send(Msg { fields })`:

```rust
// Current bridge boilerplate (~10 lines per actor)
impl ChatBroadcaster for ActorRef<ChatRoom> {
    fn say(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(Say { from, text })
    }
    fn add_member(&self, name: String, inbox: Arc<dyn ChatParticipant>) -> Result<(), ActorError> {
        self.send(Join { name, inbox })
    }
}
```

A variant of `actor_api!` could generate bridge impls for an existing trait:

```rust
// Potential: impl-only mode for existing protocol traits
actor_api! {
    impl ChatBroadcaster for ActorRef<ChatRoom> {
        send fn say(from: String, text: String) => Say;
        send fn add_member(name: String, inbox: Arc<dyn ChatParticipant>) => Join;
    }
}
```

This would use the same syntax but `impl Trait for Type` (no `pub`, no new trait) signals that we're implementing an existing trait. The protocol trait itself remains user-defined — it IS the contract, so it should stay hand-written.

**Impact:** Bridge boilerplate per actor drops from ~10 lines to ~4 lines. The protocol trait definition stays manual (by design). Combined with `#[actor]`, the total code for a protocol-based actor would be competitive with Approach A.

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

| Approach | Macro potential | What it would eliminate | Worth implementing? |
|----------|----------------|----------------------|---------------------|
| **B: Protocol Traits** | High | Bridge impl boilerplate | Yes — `actor_api!` impl-only mode |
| **C: Typed Wrappers** | None | N/A — structural problem | No |
| **D: Derive Macro** | N/A | Already a macro | N/A |
| **E: AnyActorRef** | None | Would add false safety | No |
| **F: PID** | Low-Medium | Registration ceremony | Maybe — ergonomics only |

**Takeaway:** Approach B is the only unimplemented approach that would meaningfully benefit from `actor_api!`-style macros. And the required change is small — adding an impl-only mode to the existing macro. This would make Approach B more competitive with Approach A on boilerplate, while retaining its testability advantage.

---

## Comparison Matrix

### Functional Dimensions

| Dimension | A: Recipient | B: Protocol Traits | C: Typed Wrappers | D: Derive Macro | E: AnyActorRef | F: PID |
|-----------|-------------|-------------------|-------------------|-----------------|---------------|--------|
| **Status** | Implemented | WIP | Design only | Design only | Design only | Design only |
| **Breaking** | Yes | Yes | No | No | Yes | Yes |
| **#144 type safety** | Full | Full | Hidden `unreachable!` | Hidden `unreachable!` | Full | Full |
| **#145 type safety** | Compile-time | Compile-time | Compile-time | Compile-time | Runtime only | Runtime only |
| **Macro support** | `#[actor]` + `actor_api!` + message macros | `#[actor]` (no bridge macro) | N/A (enum-based) | Derive macro | `#[actor]` | `#[actor]` |
| **Dual-mode (async+threads)** | Works | Works | Complex (dual channel) | Complex | Works | Works |
| **Registry stores** | `Recipient<M>` | `Arc<dyn Protocol>` | Mixed | `Recipient<M>` | `AnyActorRef` | `Pid` |
| **Registry type safety** | Compile-time | Compile-time | Depends | Compile-time | Runtime | Runtime |

### Code Quality Dimensions

| Dimension | A: Recipient | B: Protocol Traits | C: Typed Wrappers | D: Derive Macro | E: AnyActorRef | F: PID |
|-----------|-------------|-------------------|-------------------|-----------------|---------------|--------|
| **Handler readability** | Clear: one `impl Handler<M>` or `#[send_handler]` per message | Same as A + bridge impls | Noisy: enum `match` arms + wrapper fns | Opaque: generated from enum annotations | Same as A, but callers use `Box::new` | Same as A, but callers use global `send(pid, msg)` |
| **API at a glance** | `actor_api!` block or scan Handler impls | Protocol traits (best) | Typed wrapper functions | Annotated enum (good summary) | Nothing — `AnyActorRef` is opaque | Nothing — `Pid` is opaque |
| **main.rs expressivity** | `alice.say("Hi")` with `actor_api!`; `alice.send(SayToRoom{...})` without | `room.say("Alice", "Hi")` via protocol | `ChatRoom::say(&room, ...)` assoc fn | Generated methods: `room.say(...)` | `room.send_any(Box::new(...))` | `spawned::send(pid, ...)` + registration |
| **Boilerplate per message** | Struct + `actor_api!` line | Struct + trait method + bridge impl | Enum variant + wrapper + match arm | Enum variant + annotation | Struct | Struct + registration |
| **Debugging** | Standard Rust — all code visible | Standard Rust — bridge impls visible | Standard Rust | Requires `cargo expand` | Runtime errors (downcast failures) | Runtime errors (unregistered types) |
| **Testability** | Good (mock via Recipient) | Best (mock protocol trait) | Good | Good | Fair (Any-based) | Hard (global state) |

### Strategic Dimensions

| Dimension | A: Recipient | B: Protocol Traits | C: Typed Wrappers | D: Derive Macro | E: AnyActorRef | F: PID |
|-----------|-------------|-------------------|-------------------|-----------------|---------------|--------|
| **Framework complexity** | Medium | None (user-space) | High (dual channel) | Very high (proc macro) | High (dispatch) | Medium (registry) |
| **Maintenance burden** | Low — proven Actix pattern | Low — user-maintained | High — two dispatch systems | High — complex macro | Medium | Medium |
| **Clustering readiness** | Needs `RemoteRecipient` | Needs remote bridge impls | Hard | Hard | Possible (serialize Any) | Excellent (Pid is location-transparent) |
| **Learning curve** | Moderate (Handler<M> pattern) | Moderate + bridge pattern | Low (old API preserved) | Low (write enum, macro does rest) | Low concept, high debugging | Low concept, high registration overhead |
| **Erlang alignment** | Actix-like | Least Erlang | Actix-like | Actix-like | Erlang-ish | Most Erlang |
| **Macro improvement potential** | Already done (`actor_api!`) | High (bridge impls) | None (structural) | N/A (is a macro) | None (false safety) | Low (ergonomics only) |

---

## Recommendation

**Approach A (Handler\<M\> + Recipient\<M\>)** is the most mature and balanced option:
- Fully implemented with 34 passing tests, multiple examples, proc macro, registry, and dual-mode support
- Compile-time type safety for both #144 and #145
- The `#[actor]` macro + `actor_api!` macro provide good expressivity without hiding too much
- `actor_api!` reduces extension trait boilerplate from ~15 lines to ~5 lines per actor
- Proven pattern (Actix uses the same architecture)
- Non-macro version is already clean — the macros are additive, not essential

**Approach B (Protocol Traits)** is valuable as a **complementary** pattern:
- Can coexist with Recipient\<M\> — use protocol traits where you want explicit contracts and testability, Recipient\<M\> where you want less boilerplate
- No framework changes needed — it's purely a user-space convention
- Best option for high-testability boundaries, but the bridge boilerplate cost is real

**Approaches C and D** try to preserve the old enum-based API but introduce significant complexity (dual-channel, or heavy code generation) to work around its limitations.

**Approaches E and F** sacrifice Rust's compile-time type safety for runtime flexibility. F (PID) may become relevant later for clustering, but is premature as the default API today.

---

## Branch Reference

| Branch | Base | Description |
|--------|------|-------------|
| `main` | — | Old enum-based API (baseline) |
| [`feat/critical-api-issues`](../../tree/1ef33bf) | main | Design doc for Handler\<M\> + Recipient\<M\> ([`docs/API_REDESIGN.md`](../../blob/1ef33bf/docs/API_REDESIGN.md)) |
| [`feat/handler-api-v0.5`](../../tree/34bf9a7) | main | Handler\<M\> + Recipient\<M\> implementation |
| [`feat/actor-macro-registry`](../../tree/de651ad) | main | Adds `#[actor]` macro + named registry on top of Handler\<M\> |
| [`feat/145-protocol-trait`](../../tree/b0e5afb) | main | Protocol traits approach + [`docs/ALTERNATIVE_APPROACHES.md`](../../blob/b0e5afb/docs/ALTERNATIVE_APPROACHES.md) |
| [`docs/add-project-roadmap`](../../tree/426c1a9) | main | Framework comparison with Actix and Ractor |

---

## Detailed Design Documents

- **[`docs/API_REDESIGN.md`](../../blob/1ef33bf/docs/API_REDESIGN.md)** (on `feat/critical-api-issues`) — Full design rationale for Handler\<M\>, Receiver\<M\>, Envelope pattern, RPITIT decision, and planned supervision traits.
- **[`docs/ALTERNATIVE_APPROACHES.md`](../../blob/b0e5afb/docs/ALTERNATIVE_APPROACHES.md)** (on `feat/145-protocol-trait`) — Original comparison of all 5 alternative branches with execution order plan.
