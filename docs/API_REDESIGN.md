# Plan: API Redesign for v0.5 - Issues #144, #145, and Phase 3

## Decisions Made

| Issue | Decision | Rationale |
|-------|----------|-----------|
| **#145** (Circular deps) | Recipient\<M\> pattern | Type-safe, no exposed Pid |
| **#144** (Type safety) | Per-message types (Handler\<M\>) | Leverage Rust's type system, clean API |
| **Breaking changes** | Accepted | v0.5 is the right time for API improvements |
| **Pattern support** | Per-message only | Clean break, one way to do things |
| **Actor identity** | Internal ID (hidden) | Links/monitors work via traits, no public Pid |
| **Supervision** | Trait-based (`Supervisable`) | Type-safe child management |

## Overview

This is a significant API redesign that:
1. Adds Handler<M> pattern for per-message type safety (#144)
2. Adds Recipient<M> for type-erased message sending (#145)
3. Uses internal identity (not exposed as Pid) for links/monitors
4. Uses traits for supervision (Supervisable, Linkable)

---

## Issue #145: Circular Dependency with Bidirectional Actors

### The Problem

```rust
// actor_a.rs
use crate::actor_b::ActorB;
struct ActorA { peer: ActorRef<ActorB> }  // Needs ActorB type

// actor_b.rs
use crate::actor_a::ActorA;
struct ActorB { peer: ActorRef<ActorA> }  // CIRCULAR!
```

### Solution: Recipient\<M\>

```rust
/// Trait for anything that can receive messages of type M.
/// Object-safe: all methods return concrete types (no async/impl Future).
/// Async waiting happens outside the trait via oneshot channels (Actix pattern).
trait Receiver<M: Send>: Send + Sync {
    fn send(&self, msg: M) -> Result<(), ActorError>;
    fn request(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>;
}

// ActorRef<A> implements Receiver<M> for all M where A: Handler<M>
// Type-erased handle (ergonomic alias)
type Recipient<M> = Arc<dyn Receiver<M>>;

// Ergonomic async wrapper on the concrete Recipient type (not on the trait)
impl<M: Message> Recipient<M> {
    pub async fn send_request(&self, msg: M) -> Result<M::Result, ActorError> {
        let rx = self.request(msg)?;
        rx.await.map_err(|_| ActorError::ActorStopped)
    }
}

// Usage - no circular dependency!
struct ActorA { peer: Recipient<SharedMessage> }
struct ActorB { peer: Recipient<SharedMessage> }
```

---

## Issue #144: Type Safety for Request/Reply

### The Problem

```rust
enum Reply { Name(String), Age(u32), NotFound }

// GetName can only return Name or NotFound, but must match Age too
match actor.request(Request::GetName).await? {
    Reply::Name(n) => println!("{}", n),
    Reply::NotFound => println!("not found"),
    Reply::Age(_) => unreachable!(),  // Required but impossible
}
```

### Solution: Per-Message Types with Handler\<M\>

```rust
struct GetName(String);
impl Message for GetName {
    type Result = Option<String>;
}

impl Handler<GetName> for NameServer {
    fn handle(&mut self, msg: GetName) -> Option<String> { ... }
}

// Clean caller code - exact type!
let name: Option<String> = actor.request(GetName("joe")).await?;
```

---

# Implementation Plan

## Phase 3.1: Receiver\<M\> Trait and Recipient\<M\> Alias

**New file:** `concurrency/src/recipient.rs`

```rust
/// Trait for anything that can receive messages of type M.
///
/// Object-safe by design: all methods return concrete types, no async/impl Future.
/// This follows the Actix pattern where async waiting happens outside the trait
/// boundary via oneshot channels, keeping the trait compatible with `dyn`.
///
/// This is implemented by ActorRef<A> for all message types the actor handles.
/// Use `Recipient<M>` for type-erased storage.
pub trait Receiver<M: Message>: Send + Sync {
    /// Fire-and-forget send (enqueue message, don't wait for reply)
    fn send(&self, msg: M) -> Result<(), ActorError>;

    /// Enqueue message and return a oneshot channel to await the reply.
    /// This is synchronous — it only does channel plumbing.
    /// The async waiting happens on the returned receiver.
    fn request(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>;
}

/// Type-erased handle (ergonomic alias). Object-safe because Receiver is.
pub type Recipient<M> = Arc<dyn Receiver<M>>;

/// Ergonomic async wrapper — lives on the concrete type, not the trait.
impl<M: Message> Recipient<M> {
    pub async fn send_request(&self, msg: M) -> Result<M::Result, ActorError> {
        let rx = Receiver::request(&**self, msg)?;
        rx.await.map_err(|_| ActorError::ActorStopped)
    }
}

// ActorRef<A> implements Receiver<M> for all M where A: Handler<M>
impl<A, M> Receiver<M> for ActorRef<A>
where
    A: Actor + Handler<M>,
    M: Message,
{
    fn send(&self, msg: M) -> Result<(), ActorError> {
        // Pack message into envelope, push to actor's mailbox channel
        ...
    }

    fn request(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError> {
        // Create oneshot channel, pack (msg, tx) into envelope,
        // push to actor's mailbox, return rx
        ...
    }
}

// Convert ActorRef to Recipient
impl<A: Actor> ActorRef<A> {
    pub fn recipient<M>(&self) -> Recipient<M>
    where
        A: Handler<M>,
        M: Message,
    {
        Arc::new(self.clone())
    }
}
```

## Phase 3.2: Internal Identity (Hidden)

**New file:** `concurrency/src/identity.rs`

```rust
/// Internal process identifier (not public API)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ActorId(u64);

/// Exit reason for actors
pub enum ExitReason {
    Normal,
    Shutdown,
    Error(String),
    Linked,  // Linked actor died
}
```

## Phase 3.3: Traits for Supervision and Links

**New file:** `concurrency/src/traits.rs`

```rust
/// Trait for actors that can be supervised.
/// Provides actor_id() for identity comparison with trait objects.
pub trait Supervisable: Send + Sync {
    fn actor_id(&self) -> ActorId;
    fn stop(&self);
    fn is_alive(&self) -> bool;
    fn on_exit(&self, callback: Box<dyn FnOnce(ExitReason) + Send>);
}

/// Trait for actors that can be linked.
/// Uses actor_id() (from Supervisable) to track links internally.
pub trait Linkable: Supervisable {
    fn link(&self, other: &dyn Linkable);
    fn unlink(&self, other: &dyn Linkable);
}

/// Trait for actors that can be monitored
pub trait Monitorable: Supervisable {
    fn monitor(&self) -> MonitorRef;
    fn demonitor(&self, monitor_ref: MonitorRef);
}

// ActorRef<A> implements all these traits
impl<A: Actor> Supervisable for ActorRef<A> { ... }
impl<A: Actor> Linkable for ActorRef<A> { ... }
impl<A: Actor> Monitorable for ActorRef<A> { ... }
```

## Phase 3.4: Registry (Named Actors)

**New file:** `concurrency/src/registry.rs`

```rust
/// Register a recipient under a name
pub fn register<M: Message>(name: &str, recipient: Recipient<M>) -> Result<(), RegistryError>;

/// Look up a recipient by name (must know message type)
pub fn whereis<M: Message>(name: &str) -> Option<Recipient<M>>;

/// Unregister a name
pub fn unregister(name: &str);

/// List all registered names
pub fn registered() -> Vec<String>;
```

## Phase 4: Handler<M> Pattern (#144)

**Redesigned Actor API:**

```rust
/// Marker trait for messages
pub trait Message: Send + 'static {
    type Result: Send;
}

/// Handler for a specific message type.
/// Uses RPITIT (Rust 1.75+) — this is fine since Handler is never used as dyn.
/// &mut self is safe: actors process messages sequentially (one at a time),
/// so there is no concurrent access to self.
pub trait Handler<M: Message>: Actor {
    fn handle(&mut self, msg: M, ctx: &Context<Self>) -> impl Future<Output = M::Result> + Send;
}

/// Actor context (replaces ActorRef in handlers)
pub struct Context<A: Actor> {
    // ... internal fields
}

impl<A: Actor> Context<A> {
    pub fn stop(&self);
    pub fn recipient<M>(&self) -> Recipient<M> where A: Handler<M>;
}

/// Base actor trait (simplified)
pub trait Actor: Send + Sized + 'static {
    fn started(&mut self, ctx: &Context<Self>) -> impl Future<Output = ()> + Send { async {} }
    fn stopped(&mut self, ctx: &Context<Self>) -> impl Future<Output = ()> + Send { async {} }
}
```

**ActorRef changes:**

```rust
/// Typed handle to an actor.
///
/// Internally uses an envelope pattern (like Actix) for the mailbox:
/// messages of different types are packed into `Box<dyn Envelope<A>>` so
/// the actor's single mpsc channel can carry any message type the actor handles.
pub struct ActorRef<A: Actor> {
    id: ActorId,  // Internal identity (not public)
    sender: mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    _marker: PhantomData<A>,
}

/// Type-erased envelope that the actor loop can dispatch.
/// Each concrete envelope captures the message and an optional oneshot sender.
trait Envelope<A: Actor>: Send {
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>);
}

impl<A, M> ActorRef<A>
where
    A: Actor + Handler<M>,
    M: Message,
{
    /// Fire-and-forget send (returns error if actor dead)
    pub fn send(&self, msg: M) -> Result<(), ActorError>;

    /// Enqueue message and return a oneshot receiver for the reply.
    /// Synchronous — only does channel plumbing (Actix pattern).
    pub fn request(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>;

    /// Ergonomic async request: enqueue + await reply.
    pub async fn send_request(&self, msg: M) -> Result<M::Result, ActorError> {
        let rx = self.request(msg)?;
        rx.await.map_err(|_| ActorError::ActorStopped)
    }

    /// Get type-erased Recipient for this message type
    pub fn recipient(&self) -> Recipient<M>;
}

// Implements supervision/linking traits
impl<A: Actor> Supervisable for ActorRef<A> { ... }
impl<A: Actor> Linkable for ActorRef<A> { ... }
impl<A: Actor> Monitorable for ActorRef<A> { ... }
```

---

## Example: Bank Actor (New API)

```rust
// messages.rs
pub struct CreateAccount { pub name: String }
pub struct Deposit { pub account: String, pub amount: u64 }
pub struct GetBalance { pub account: String }

impl Message for CreateAccount { type Result = Result<(), BankError>; }
impl Message for Deposit { type Result = Result<u64, BankError>; }
impl Message for GetBalance { type Result = Result<u64, BankError>; }

// bank.rs
pub struct Bank {
    accounts: HashMap<String, u64>,
}

impl Actor for Bank {
    async fn started(&mut self, ctx: &Context<Self>) {
        tracing::info!("Bank started");
    }
}

impl Handler<CreateAccount> for Bank {
    async fn handle(&mut self, msg: CreateAccount, _ctx: &Context<Self>) -> Result<(), BankError> {
        self.accounts.insert(msg.name, 0);
        Ok(())
    }
}

impl Handler<Deposit> for Bank {
    async fn handle(&mut self, msg: Deposit, _ctx: &Context<Self>) -> Result<u64, BankError> {
        let balance = self.accounts.get_mut(&msg.account).ok_or(BankError::NotFound)?;
        *balance += msg.amount;
        Ok(*balance)
    }
}

impl Handler<GetBalance> for Bank {
    async fn handle(&mut self, msg: GetBalance, _ctx: &Context<Self>) -> Result<u64, BankError> {
        self.accounts.get(&msg.account).copied().ok_or(BankError::NotFound)
    }
}

// main.rs
let bank: ActorRef<Bank> = Bank::new().start();

// Type-safe request (async convenience wrapper: enqueue + await oneshot)
let balance: Result<u64, BankError> = bank.send_request(GetBalance { account: "alice".into() }).await?;

// Fire-and-forget send
bank.send(Deposit { account: "alice".into(), amount: 50 })?;

// Low-level: get oneshot receiver directly (useful for select!, timeouts, etc.)
let rx = bank.request(GetBalance { account: "alice".into() })?;
let balance = tokio::time::timeout(Duration::from_secs(5), rx).await??;

// Get type-erased Recipient for storage/passing to other actors
let recipient: Recipient<GetBalance> = bank.recipient();

// Supervision uses trait objects
let children: Vec<Arc<dyn Supervisable>> = vec![
    Arc::new(bank.clone()),
    Arc::new(logger.clone()),
];
```

---

## Example: Solving #145 (Circular Deps) with Recipient

```rust
// shared_messages.rs - NO circular dependency
pub struct OrderUpdate { pub order_id: u64, pub status: String }
pub struct InventoryReserve { pub item: String, pub quantity: u32, pub reply_to: Recipient<OrderUpdate> }

impl Message for OrderUpdate { type Result = (); }
impl Message for InventoryReserve { type Result = Result<(), InventoryError>; }

// order_service.rs - imports InventoryReserve, NOT InventoryService
use crate::shared_messages::{OrderUpdate, InventoryReserve};

pub struct OrderService {
    inventory: Recipient<InventoryReserve>,  // Type-erased, no circular dep!
}

impl Handler<PlaceOrder> for OrderService {
    async fn handle(&mut self, msg: PlaceOrder, ctx: &Context<Self>) -> Result<(), OrderError> {
        let reply_to: Recipient<OrderUpdate> = ctx.recipient();
        self.inventory.send_request(InventoryReserve {
            item: msg.item,
            quantity: msg.quantity,
            reply_to,
        }).await?;
        Ok(())
    }
}

// inventory_service.rs - imports OrderUpdate, NOT OrderService
use crate::shared_messages::{OrderUpdate, InventoryReserve};

pub struct InventoryService { ... }

impl Handler<InventoryReserve> for InventoryService {
    async fn handle(&mut self, msg: InventoryReserve, _ctx: &Context<Self>) -> Result<(), InventoryError> {
        // Reserve inventory...
        msg.reply_to.send(OrderUpdate { order_id: 123, status: "reserved".into() })?;
        Ok(())
    }
}

// main.rs - wire them together
let inventory: ActorRef<InventoryService> = InventoryService::new().start();
let inventory_recipient: Recipient<InventoryReserve> = inventory.recipient();

let order_service = OrderService::new(inventory_recipient).start();
```

---

## Files to Create/Modify

| File | Action | Description |
|------|--------|-------------|
| `concurrency/src/recipient.rs` | Create | Receiver<M> trait and Recipient<M> alias |
| `concurrency/src/identity.rs` | Create | Internal ActorId (not public) |
| `concurrency/src/traits.rs` | Create | Supervisable, Linkable, Monitorable |
| `concurrency/src/registry.rs` | Create | Named actor registry |
| `concurrency/src/message.rs` | Create | Message and Handler traits |
| `concurrency/src/context.rs` | Create | Context type |
| `concurrency/src/tasks/actor.rs` | Rewrite | New Actor/Handler API |
| `concurrency/src/threads/actor.rs` | Rewrite | Same changes for threads |
| `concurrency/src/lib.rs` | Update | Export new types |
| `examples/*` | Update | Migrate to new API |

---

## Migration Path

1. **Step 1:** Add Message trait and Handler<M> pattern alongside current API
2. **Step 2:** Add Recipient<M> for type-erased sending
3. **Step 3:** Add Supervisable/Linkable/Monitorable traits
4. **Step 4:** Add Registry with Recipient<M>
5. **Deprecation:** Mark old Request/Reply/Message associated types as deprecated
6. **Removal:** Remove old API in subsequent release

---

## v0.6+ Considerations

| Feature | Approach |
|---------|----------|
| **Clustering** | Add `RemoteRecipient<M>` that serializes ActorId + message |
| **State machines** | gen_statem equivalent using Handler<M> pattern |
| **Persistence** | Event sourcing via Handler<PersistEvent> |

---

## Verification

1. `cargo build --workspace` - All crates compile
2. `cargo test --workspace` - All tests pass
3. Update examples to new API
4. Test bidirectional actor communication without circular deps
5. Test Supervisable/Linkable traits work correctly

---

## Final Decisions

| Item | Decision |
|------|----------|
| Method naming | `send()` = fire-forget, `request()` = wait for reply |
| Dead actor handling | Returns `Err(ActorStopped)` (type-safe feedback) |
| Pattern support | Per-message types only (no enum fallback) |
| Type erasure | `Recipient<M>` for message-type-safe erasure |
| Actor identity | Internal `ActorId` (not exposed as Pid) |
| Supervision | `Supervisable` / `Linkable` / `Monitorable` traits |
