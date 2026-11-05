> Historical plan, superseded on 2026-11-05 by [the current roadmap](../PLAN.md).
> API status, competitor comparisons, version targets, and priorities below are
> preserved for context and must not be treated as current facts or commitments.

# Spawned Project Roadmap

This document outlines the strategic roadmap for spawned, informed by analysis of established actor frameworks.

## Current Status

**Phase 1: Core Actor Framework** ✅ Complete

The foundation is in place:
- `Actor` trait with `init()`, `handle_request()`, `handle_message()`, `teardown()` lifecycle
- `ActorRef<T>` for communication (`request()` and `send()`)
- Dual execution modes (async tasks / sync threads)
- Timers (`send_after`, `send_interval`)
- Stream processing
- Signal handling via `send_message_on()`

## Framework Comparison

### Feature Matrix: Spawned vs Actix vs Ractor

| Feature | Spawned | Actix | Ractor |
|---------|---------|-------|--------|
| **Handler\<M\> pattern** | Planned (v0.5) | ✅ Yes | ✅ Yes (enum-based) |
| **Recipient/Type erasure** | Planned (v0.5) | ✅ Yes | ❌ Single msg type per actor |
| **Supervision** | Planned (v0.5) | ✅ Yes | ✅ **Best** (Erlang-style) |
| **Distributed actors** | Future (v0.6+) | ❌ No | ✅ `ractor_cluster` |
| **Dual execution modes** | ✅ **Unique** | ❌ No | ❌ No |
| **Native OS threads** | ✅ **Unique** | ❌ No | ❌ No |
| **No runtime required** | ✅ (threads mode) | ❌ Actix runtime | ❌ Tokio required |
| **Signal handling** | ✅ `send_message_on()` | ❌ Manual | ✅ Signal priority channel |
| **Timers** | ✅ Built-in | ✅ Yes | ✅ `time` module |
| **Named registry** | Planned (v0.5) | ✅ Yes | ✅ Erlang-style |
| **Process groups (pg)** | ❌ Not yet | ❌ No | ✅ Erlang-style |
| **Links/Monitors** | Planned (v0.5) | ❌ No | ✅ Yes |
| **RPC** | ❌ Not yet | ❌ No | ✅ Built-in |
| **Async-first** | ✅ Yes | ⚠️ Afterthought | ✅ Yes |
| **Multiple runtimes** | ✅ Tokio + none | ❌ Actix only | ✅ Tokio + async-std |
| **Pure Rust (no unsafe)** | ✅ Yes | ⚠️ Some unsafe | ✅ Yes |

### Supervision Comparison

| Aspect | Spawned (Planned) | Actix | Ractor |
|--------|-------------------|-------|--------|
| **OneForOne** | Planned | ✅ Yes | ✅ Yes |
| **OneForAll** | Planned | ✅ Yes | ✅ Yes |
| **RestForOne** | Planned | ❌ No | ✅ Yes |
| **Meltdown protection** | Not planned | ❌ No | ✅ Yes |
| **Supervision trees** | Planned | ⚠️ Limited | ✅ **Full Erlang-style** |
| **Dynamic supervisors** | Planned | ❌ No | ✅ Yes |

### Erlang Alignment

| Concept | Spawned | Actix | Ractor |
|---------|---------|-------|--------|
| **gen_server model** | ✅ Strong | ⚠️ Diverged | ✅ **Strongest** |
| **call/cast naming** | `request`/`send` | `send`/`do_send` | `call`/`cast` |
| **Supervision trees** | Planned | Limited | ✅ Full OTP-style |
| **Process registry** | Planned | Yes | ✅ Erlang-style |
| **Process groups (pg)** | ❌ No | ❌ No | ✅ Yes |
| **EPMD-style clustering** | Future | ❌ No | ✅ `ractor_cluster` |

### Spawned's Unique Value Propositions

1. **Dual execution modes** - No other framework offers async AND blocking with same API
2. **No runtime lock-in** - threads mode needs zero async runtime
3. **Backend flexibility** - Async, Blocking pool, or dedicated Thread per actor
4. **Simpler mental model** - Less concepts to learn than Actix or Ractor

### What's Missing (vs Ractor)

| Feature | Priority | Rationale |
|---------|----------|-----------|
| **RestForOne strategy** | High | Complete supervision |
| **Meltdown protection** | High | Production safety |
| **Process groups (pg)** | Medium | Erlang compatibility |
| **Priority message channels** | Medium | Better control flow |
| **Distributed actors** | Low | `ractor_cluster` equivalent |

### When to Use Each Framework

| Use Case | Best Choice | Why |
|----------|-------------|-----|
| **Erlang/OTP migration** | **Ractor** | Closest to OTP semantics |
| **Embedded/no-runtime** | **Spawned** | Only one with native OS thread support |
| **Mixed async/sync** | **Spawned** | Dual execution modes |
| **Web applications** | **Actix** | actix-web ecosystem |
| **Distributed systems** | **Ractor** | `ractor_cluster` ready |
| **Raw performance** | **Actix** | Fastest in benchmarks |
| **Simple learning** | **Spawned** | Cleanest API |
| **Production fault-tolerance** | **Ractor** | Most complete supervision |

## Critical API Issues (Pre-Phase 2)

Before building more features, these API design issues should be addressed:

### Issue #145: Circular Dependency with Bidirectional Actors

**Problem:** Current `ActorRef<T>` creates module-level circular dependencies when actors need to communicate bidirectionally.

**Impact:** Blocks real-world actor collaboration patterns.

**Solution direction:** Type-erased handles (like Actix's `Recipient<M>`) or PID-based addressing.

### Issue #144: Type Safety for Request/Reply

**Problem:** Callers must match the full `Reply` enum even when only a subset of variants are possible for a given request.

```rust
// Current problem: GetBalance can only return Balance or NotFound,
// but caller must handle ALL Reply variants
match bank.request(Request::GetBalance { account }).await? {
    Reply::Balance(b) => Ok(b),
    Reply::AccountNotFound => Err(BankError::NotFound),
    Reply::AccountCreated => unreachable!(),  // Annoying!
    Reply::Deposited { .. } => unreachable!(), // Annoying!
}
```

**Impact:** Verbose, error-prone code.

#### How Ractor Solves This: `RpcReplyPort<T>`

Ractor embeds a typed reply channel in each message variant:

```rust
// Ractor approach: Each call variant has its OWN reply type
enum BankMessage {
    // Fire-and-forget (cast) - no reply
    PrintStatement,

    // RPC calls - each specifies its reply type via RpcReplyPort<T>
    CreateAccount(String, RpcReplyPort<Result<(), BankError>>),
    Deposit(String, u64, RpcReplyPort<Result<u64, BankError>>),
    GetBalance(String, RpcReplyPort<Result<u64, BankError>>),
}

// Caller gets exact type - no unreachable!()
let balance: Result<u64, BankError> = call_t!(
    bank_actor,
    BankMessage::GetBalance,
    100,  // timeout ms
    "alice".to_string()
).expect("RPC failed");
```

#### Comparison of Solutions

| Approach | Reply Type | Message Definition | Multiple Handlers |
|----------|------------|-------------------|-------------------|
| **Spawned current** | Single enum | Clean | N/A |
| **Ractor** | Per-variant via `RpcReplyPort<T>` | Port embedded in message | ❌ Single enum |
| **Actix** | Per-message via `Message::Result` | Separate structs | ✅ Multiple `Handler<M>` |
| **Spawned planned** | Per-message via `Message::Result` | Separate structs | ✅ Multiple `Handler<M>` |

#### Our Planned Solution: Handler\<M\> Pattern

We chose the Actix-style `Handler<M>` pattern over Ractor's `RpcReplyPort<T>` because:

1. **Cleaner messages** - No infrastructure (reply port) in message definition
2. **Multiple message types** - Actor can implement `Handler<M>` for multiple `M`
3. **Proven pattern** - Actix has validated this approach at scale

```rust
// Spawned planned approach
struct GetBalance { account: String }
impl Message for GetBalance { type Result = Result<u64, BankError>; }

impl Handler<GetBalance> for Bank {
    async fn handle(&mut self, msg: GetBalance, ctx: &Context<Self>) -> Result<u64, BankError> {
        self.accounts.get(&msg.account).copied().ok_or(BankError::NotFound)
    }
}

// Caller gets exact type
let balance: Result<u64, BankError> = bank.request(GetBalance { account: "alice".into() }).await?;
```

## Phase Priorities

| Priority | Phase | Description | Status |
|----------|-------|-------------|--------|
| **P0** | API Design | Issues #144, #145 | 🔴 Not started |
| **P1** | Phase 2 | Error Handling | 🔴 Not started |
| **P2** | Phase 3 | Process Primitives (Pid, Registry, Links) | 🔴 Not started |
| **P3** | Phase 4 | Supervision Trees | 🔴 Not started |
| **P4** | Phase 5 | Documentation & Examples | 🔴 Not started |

### Rationale for Ordering

1. **API Design first** - Issues #144 and #145 affect the core API. Fixing them later would be breaking changes. Better to address before building supervision on top.

2. **Error Handling before Supervision** - Clean error propagation is foundational for supervision strategies.

3. **Process Primitives before Supervision** - Pid, Registry, and Links/Monitors are the building blocks supervisors need.

4. **Documentation last** - After API stabilizes to avoid rewriting docs.

## v0.6+ Considerations

Features to consider for future versions:

| Feature | Priority | Notes |
|---------|----------|-------|
| State machines (`gen_statem`) | Medium | Useful for protocol implementations |
| Backoff strategies | Medium | Akka has this built into supervision |
| Persistence/event sourcing | Medium | Akka Persistence pattern |
| Actor naming beyond Registry | Low | Like Erlang's `{global, Name}` |
| Clustering/distribution | Low | Significant complexity |
| Virtual actors (Orleans) | Low | Different paradigm |
| Hot code reload | Low | Rust doesn't support well |

## Design Decisions

### Keep Current Approach

- **Explicit actor lifecycle** - Better for Rust's ownership model than Orleans's implicit activation
- **Type-safe messages** - More safety than Erlang's untyped approach
- **Dual execution modes** - Unique value proposition among Rust frameworks
- **`request()`/`send()` naming** - Clearer than Actix's `send`/`do_send`, familiar to Erlang users

### Adopt from Actix

- **`Handler<M>` pattern** - Per-message type safety (#144)
- **`Recipient<M>`** - Type-erased message recipients (#145)
- **`Message::Result`** - Associated type for reply instead of separate enum

### Adopt from Ractor

- **RestForOne supervision strategy** - Complete supervision options
- **Meltdown protection** - Prevent restart loops in production
- **Supervision trees** - Full hierarchical fault tolerance

### Consider for Future (from Ractor)

- **Process groups (pg)** - Erlang-style actor grouping
- **Priority message channels** - Signal > Stop > Supervision > Message
- **Distributed actors** - `ractor_cluster` equivalent

### Not Adopting

- **Ractor's `RpcReplyPort<T>` in messages** - Clutters message definition; Handler<M> is cleaner
- **Ractor's single message type per actor** - Less flexible than multiple Handler<M> impls
- **Actix runtime requirement** - Keep our no-runtime threads mode

## References

- Issue #124: Framework comparison request
- Issue #138: v0.5 Roadmap
- Issue #144: Type safety for request/reply
- Issue #145: Circular dependency with bidirectional actors
