# Alternative Approach Branches for #144 + #145

Create 5 branches from `main`, each implementing a different approach to solve #144 (type safety) and #145 (circular deps). Each branch migrates the same examples (bank, name_server, chat_room, ping_pong) so they can be compared directly.

**Baseline (already done):** `feat/handler-api-v0.5` — Handler<M> + Recipient<M>

---

## Branch 1: `feat/144-typed-wrappers`

**#144 solution:** Typed wrapper methods (user-side pattern, no framework trait changes)
**#145 solution:** Recipient<M> via dual-channel (envelope channel alongside existing enum channel)

### What changes

The enum-based Actor trait stays **unchanged** from main:
```rust
pub trait Actor: Send + Sized {
    type Request; type Message; type Reply; type Error;
    async fn handle_request(&mut self, msg: Self::Request, handle: &ActorRef<Self>) -> RequestResponse<Self>;
    async fn handle_message(&mut self, msg: Self::Message, handle: &ActorRef<Self>) -> MessageResponse;
    // ...
}
```

Each actor adds typed convenience methods that hide enum matching:
```rust
impl Bank {
    pub async fn deposit(handle: &mut ActorRef<Self>, who: String, amount: i32) -> Result<i32, BankError> {
        match handle.request(InMessage::Add { who, amount }).await? {
            Ok(OutMessage::Balance { amount, .. }) => Ok(amount),
            Err(e) => Err(e),
            _ => unreachable!(),
        }
    }
}
// Client: Bank::deposit(&mut bank, "joe".into(), 10).await?
```

For #145, add a **second channel** to ActorRef for envelope-based messages. The actor loop `select!`s on both channels. Actors that want to participate in cross-actor type-erased communication implement `Handler<M>` for specific messages:

```rust
pub struct ActorRef<A: Actor> {
    pub tx: mpsc::Sender<ActorInMsg<A>>,                    // existing enum channel
    envelope_tx: mpsc::Sender<Box<dyn Envelope<A> + Send>>, // NEW: for Recipient<M>
    // ...
}
```

### Key tradeoffs
- **Pro:** Zero breaking changes — existing code works untouched
- **Pro:** Typed wrappers are simple to understand
- **Con:** Dual channel adds complexity (select!, fairness, ordering)
- **Con:** `unreachable!()` still exists inside wrappers, just hidden
- **Con:** Two coexisting dispatch mechanisms is confusing

---

## Branch 2: `feat/144-derive-macro`

**#144 solution:** Proc macro `#[derive(ActorMessages)]` generates typed wrappers + per-message structs from annotated enum
**#145 solution:** Same dual-channel Recipient<M> as Branch 1

### What changes

A new `spawned-derive` proc-macro crate. The macro on the Request enum generates:
- Per-variant message structs implementing `Message`
- A reply enum wrapping per-variant result types
- Typed wrapper methods on `ActorRef<A>`
- `Handler<M>` impls that delegate to `handle_request()`

```rust
#[derive(ActorMessages)]
#[actor(Bank)]
pub enum BankRequest {
    #[reply(Result<(), BankError>)]
    NewAccount { who: String },
    #[reply(Result<u64, BankError>)]
    Deposit { who: String, amount: i32 },
}
// Generates: struct NewAccount, struct Deposit, impl Message, typed methods, etc.
```

### Key tradeoffs
- **Pro:** Zero manual boilerplate for typed wrappers
- **Pro:** Macro enforces variant-struct mapping
- **Con:** Proc macro crate adds compilation cost
- **Con:** Generated code is hard to debug
- **Con:** `unreachable!()` still exists in generated code
- **Con:** Dual channel complexity from Branch 1 still applies

---

## Branch 3: `feat/145-any-actor`

**#144 solution:** Handler<M> per-message trait (same as current PR)
**#145 solution:** `AnyActorRef` — fully type-erased handle via `Box<dyn Any>`

### What changes

Replace `Receiver<M>` / `Recipient<M>` with a single type-erased handle:

```rust
pub trait AnyActor: Send + Sync {
    fn send_any(&self, msg: Box<dyn Any + Send>) -> Result<(), ActorError>;
    fn request_any(&self, msg: Box<dyn Any + Send>) -> Result<oneshot::Receiver<Box<dyn Any + Send>>, ActorError>;
}
pub type AnyActorRef = Arc<dyn AnyActor>;
```

Requires `AnyDispatchable` trait for runtime message type dispatch (macro or manual).

### Chat room example
```rust
// room.rs — stores AnyActorRef, not Recipient<Deliver>
pub struct ChatRoom { members: Vec<(String, AnyActorRef)> }

// user.rs — stores AnyActorRef, not Recipient<Say>
pub struct User { pub name: String, pub room: AnyActorRef }
```

### Key tradeoffs
- **Pro:** Single type for all type-erased refs — simpler storage
- **Con:** No compile-time safety at actor boundary — wrong message type is runtime error
- **Con:** Needs dispatch macro or manual boilerplate
- **Con:** Extra allocation (Box<dyn Any>) and downcast overhead
- **Con:** Caller must downcast request-reply results

---

## Branch 4: `feat/145-pid-addressing`

**#144 solution:** Handler<M> per-message trait (same as current PR)
**#145 solution:** Global PID registry — actors get a `Pid(u64)`, messages sent by PID

### What changes

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pid(u64);

// Global registry maps (Pid, TypeId) -> Arc<dyn Receiver<M>> (stored as Arc<dyn Any>)
static REGISTRY: OnceLock<RwLock<ProcessRegistry>> = OnceLock::new();

// Typed send/request — message type known, actor type erased
pub fn send<M: Message>(pid: Pid, msg: M) -> Result<(), ActorError>;
pub fn request<M: Message>(pid: Pid, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>;

// Named processes (Erlang register/1)
pub fn register_name(name: &str, pid: Pid);
pub fn whereis(name: &str) -> Option<Pid>;
```

### Chat room example
```rust
// room.rs — stores Pid, not Recipient<Deliver>
pub struct ChatRoom { members: Vec<(String, Pid)> }

// user.rs — stores Pid
pub struct User { pub name: String, pub room_pid: Pid }

// main.rs — explicit registration
room.register::<Say>();
room.register::<Join>();
alice.register::<Deliver>();
```

### Key tradeoffs
- **Pro:** Most Erlang-faithful — `Pid` is a lightweight copyable u64
- **Pro:** Natural fit for named processes and clustering
- **Con:** Global mutable state — synchronization cost, harder to test
- **Con:** Registration boilerplate per message type
- **Con:** Runtime errors for unregistered message types or dead PIDs

---

## Branch 5: `feat/145-protocol-trait`

**#144 solution:** Handler<M> per-message trait (same as current PR)
**#145 solution:** User-defined protocol traits as cross-actor contracts

### What changes

**No framework changes.** The cross-actor boundary is defined by explicit traits:

```rust
// protocols.rs — shared contract, neither actor type mentioned
pub trait ChatParticipant: Send + Sync {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError>;
}
pub trait ChatBroadcaster: Send + Sync {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn join(&self, name: String, inbox: Arc<dyn ChatParticipant>) -> Result<(), ActorError>;
}

// Bridge impls connect ActorRef to protocol traits
impl ChatBroadcaster for ActorRef<ChatRoom> {
    fn say(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(RoomSay { from, text })
    }
}
impl ChatParticipant for ActorRef<User> {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(UserDeliver { from, text })
    }
}
```

### Key tradeoffs
- **Pro:** Zero framework changes for #145 — purely a user-space pattern
- **Pro:** Strongest contracts — protocol trait is self-documenting
- **Pro:** Best testability — mock the protocol trait directly
- **Con:** More boilerplate — define trait + bridge impl per cross-actor boundary
- **Con:** Doesn't scale well for many-to-many actor topologies

---

## Comparison Matrix

| Dimension | Baseline (Recipient) | 1: Typed Wrappers | 2: Derive Macro | 3: AnyActor | 4: Pid | 5: Protocol Trait |
|-----------|---------------------|-------------------|-----------------|-------------|--------|------------------|
| Breaking changes | Yes | No | No | Yes | Yes | Yes |
| #144 type safety | Full | Hidden unreachable | Hidden unreachable | Full | Full | Full |
| #145 compile safety | Per-message | Per-message | Per-message | None | Runtime resolve | Per-protocol |
| Framework complexity | Medium | High (dual channel) | Very high (macro) | High (dispatch) | Medium (registry) | None |
| User boilerplate | Low | Medium (wrappers) | Low (macro) | Low | Medium (register) | High (traits) |
| Erlang alignment | Actix-like | Actix-like | Actix-like | Erlang-ish | Most Erlang | Least Erlang |
| Testability | Good | Good | Good | Fair | Hard (global) | Best |
| Clustering readiness | Hard | Hard | Hard | Medium | Excellent | Medium |

## Execution Order

1. **Branch 5** (`feat/145-protocol-trait`) — simplest, no new framework types for #145
2. **Branch 4** (`feat/145-pid-addressing`) — new framework feature (Pid registry)
3. **Branch 3** (`feat/145-any-actor`) — new framework feature (AnyActor)
4. **Branch 1** (`feat/144-typed-wrappers`) — keeps enum Actor, adds dual-channel
5. **Branch 2** (`feat/144-derive-macro`) — most complex (proc macro crate + dual-channel)
