# Actor Framework Comparison

Research compiled during the v0.5 API redesign (Feb 2026). Covers Rust actor frameworks in depth and surveys ideas from 12+ frameworks across languages.

Sources: issue #124, PRs #114, #146.

---

## Rust Frameworks: Spawned vs Actix vs Ractor

### Feature Matrix

| Feature | Spawned | Actix | Ractor |
|---------|---------|-------|--------|
| **Handler\<M\> pattern** | v0.5 | Yes | Yes (enum-based) |
| **Type erasure (Recipient)** | v0.5 | Yes | No (single msg type per actor) |
| **Supervision** | Planned | Yes | **Best** (Erlang-style) |
| **Distributed actors** | Future | No | `ractor_cluster` |
| **Dual execution modes** | **Unique** | No | No |
| **Native OS threads** | **Unique** | No | No |
| **No runtime required** | Yes (threads mode) | No (Actix runtime) | No (Tokio required) |
| **Signal handling** | `send_message_on()` | Manual | Signal priority channel |
| **Timers** | Built-in | Yes | `time` module |
| **Named registry** | v0.5 | Yes | Erlang-style |
| **Process groups (pg)** | Not yet | No | Erlang-style |
| **Links/Monitors** | Planned | No | Yes |
| **RPC** | Not yet | No | Built-in |
| **Multiple runtimes** | Tokio + none | Actix only | Tokio + async-std |
| **Pure Rust (no unsafe)** | Yes | Some unsafe | Yes |

### Supervision Comparison

| Aspect | Spawned (Planned) | Actix | Ractor |
|--------|-------------------|-------|--------|
| OneForOne | Planned | Yes | Yes |
| OneForAll | Planned | Yes | Yes |
| RestForOne | Planned | No | Yes |
| Meltdown protection | Planned | No | Yes |
| Supervision trees | Planned | Limited | **Full Erlang-style** |
| Dynamic supervisors | Planned | No | Yes |

### Erlang Alignment

| Concept | Spawned | Actix | Ractor |
|---------|---------|-------|--------|
| **gen_server model** | Strong | Diverged | **Strongest** |
| **call/cast naming** | `request`/`send` | `send`/`do_send` | `call`/`cast` |
| **Supervision trees** | Planned | Limited | Full OTP-style |
| **Process registry** | v0.5 | Yes | Erlang-style |
| **Process groups (pg)** | No | No | Yes |
| **EPMD-style clustering** | Future | No | `ractor_cluster` |

### When to Use Each

| Use Case | Best Choice | Why |
|----------|-------------|-----|
| Erlang/OTP migration | Ractor | Closest to OTP semantics |
| Embedded/no-runtime | Spawned | Only one with native OS thread support |
| Mixed async/sync workloads | Spawned | Dual execution modes |
| Web applications | Actix | actix-web ecosystem |
| Distributed systems | Ractor | `ractor_cluster` ready |
| Raw performance | Actix | Fastest in benchmarks |
| Simple learning curve | Spawned | Cleanest API |
| Production fault-tolerance | Ractor | Most complete supervision |

### Spawned's Unique Value

1. **Dual execution modes** — async AND blocking with the same API; no other Rust framework offers this
2. **No runtime lock-in** — threads mode needs zero async runtime
3. **Backend flexibility** — Async, Blocking pool, or dedicated Thread per actor
4. **Simpler mental model** — fewer concepts to learn than Actix or Ractor

---

## Cross-Language Framework Survey

### Erlang/OTP & Elixir

The reference model for actor frameworks.

- **gen_server** — The pattern Spawned follows: `init`, `handle_call`, `handle_cast`, `terminate`
- **Supervision trees** — Hierarchical fault tolerance with restart strategies (one_for_one, one_for_all, rest_for_one)
- **Process registry** — Global name-based lookup
- **Links & monitors** — Bidirectional crash propagation (links) and unidirectional notifications (monitors)
- **Task.Supervisor.async_nolink** (Elixir) — Spawning async work from within an actor without blocking; validates our Task/TaskGroup design
- **Anti-pattern** — Don't use actors purely for code organization; only when you need isolated state, message serialization, or supervision

### Gleam OTP

- **Subject-based typed messaging** — Typed channel where only the owning process can receive; validates Spawned's `ActorRef<A>` approach
- **Actor vs Process split** — `gleam_otp/actor` is typed, `gleam_erlang/process` is low-level. Spawned mirrors this: `Actor` trait is typed, `Pid` / registry are lower-level primitives

### Akka (Scala/Java)

- **Typed actors** — Akka evolved from untyped to typed actors, validating the move toward per-message type safety
- **Persistence / event sourcing** — Durable actors that replay events on restart
- **Backoff strategies** — Built into supervision; prevents restart storms
- **Clustering** — Location-transparent actor references across nodes

### Orleans (C# / .NET)

- **Virtual actors (grains)** — Actors activated on-demand, deactivated when idle; no explicit lifecycle management
- **Single activation guarantee** — Only one instance of a grain exists at a time across the cluster
- **Durable reminders** — Persisted timers that survive restarts (vs in-memory timers)

**Design decision:** Spawned keeps explicit lifecycle by default (better for Rust's ownership model). Virtual actors could be an opt-in layer in the future.

### Pony

- **Reference capabilities** — 6 capability types (`iso`, `val`, `ref`, `box`, `trn`, `tag`) guarantee data-race freedom at compile time
- **`iso` (isolated)** — Single ownership; the only way to send mutable data between actors
- **Causal messaging** — If actor A sends M1 then M2 to actor B, B receives them in that order (stronger than FIFO per-pair)
- **Per-actor GC (ORCA)** — Each actor has its own heap; no stop-the-world pauses

**Adopted:** Typed message guarantees (via Rust's Send/Sync). **Skipped:** Full reference capability system (would need language-level support).

### Lunatic (Rust/Wasm)

- **WASM process isolation** — Each process in its own WASM sandbox; crashes can't corrupt other processes
- **Serialization-based messaging** — All messages serialized for true isolation (enables distribution)
- **Resource limits per process** — Memory, CPU, and file handle limits per process
- **Selective receive** — Pattern matching on message types in mailbox

**Adopted:** Resource limits concept (mailbox size caps). **Skipped:** WASM isolation (too heavyweight).

### Bastion (Rust)

- **Supervision trees** — Hierarchical, configurable restart strategies per level
- **Restart strategies** — OneForOne, OneForAll, RestForOne + ExponentialBackOff
- **Children group redundancy** — Minimum number of children that must be alive for health
- **Runtime-agnostic** — Works with tokio, async-std, or any executor

**Adopted:** Exponential backoff, children group redundancy concepts. **Skipped:** Runtime-agnostic design (committed to tokio).

### Proto.Actor (Go / C#)

- **Virtual actors** — Orleans-style on-demand activation with identity-based addressing
- **Cross-language messaging** — Protobuf-based; Go and C# actors communicate seamlessly
- **Behavior stack** — `become(behavior)` / `unbecome()` to swap message handlers (useful for state machines)
- **Persistence providers** — Pluggable event stores (PostgreSQL, MongoDB) with snapshots

**Adopted:** Behavior stack concept for future state machine support. **Skipped:** Protobuf requirement.

### Trio (Python)

- **Structured concurrency pioneer** — Introduced "nurseries": a scope that owns spawned tasks and ensures they all complete before the scope exits
- **Level-triggered cancellation** — Cancellation is a persistent state, not an edge event. Code checks "am I cancelled?" rather than catching exceptions. More reliable.
- **Shield scopes** — Temporarily protect critical sections from cancellation (essential for cleanup)
- **Checkpoints** — Explicit points where cancellation can occur

**Adopted:** Level-triggered cancellation model. **Skipped:** Checkpoints (Rust's `.await` points serve this purpose).

### Project Loom (Java)

- **StructuredTaskScope** — Parent scope owns child virtual threads
- **Joiner policies** — `ShutdownOnSuccess` (first wins), `ShutdownOnFailure` (fail-fast), custom joiners for quorum/timeout
- **ScopedValue** — Inherited thread-local-like values, immutable in child tasks

**Adopted:** Joiner policies for TaskGroup. **Skipped:** ScopedValue (Rust ownership handles this better).

### Ray (Python)

- **Task + Actor unification** — Stateless tasks and stateful actors share scheduling infrastructure
- **Distributed object store** — Large objects stored once, referenced by ID; avoids copying
- **Placement groups** — Co-locate related actors for communication efficiency
- **Actor pools** — Built-in load balancing across multiple instances

**Adopted:** Placement hints concept for future distributed mode. **Skipped:** Distributed object store (different problem domain).

### Swift Distributed Actors

- **`distributed` keyword** — Language-level support; methods marked `distributed` can be called remotely
- **Explicit distribution surface** — Only `distributed` methods callable across nodes; local-only methods clearly separated
- **SerializationRequirement** — Compiler enforces that distributed actor parameters are `Codable`

**Adopted:** Explicit marker for remotely-callable methods (`#[distributed]` attribute, future). **Skipped:** Language-level integration (Rust macros suffice).

### CAF (C++ Actor Framework)

- **Typed actor interfaces** — Actors declare message types as a type signature; compile-time verification
- **Behavior composition** — Combine multiple behaviors with `or_else`
- **Response promises** — Explicit promise objects for async responses; can delegate response to another actor

**Adopted:** Typed message interfaces via traits (already implemented). **Skipped:** C++ template complexity.

### Dapr (Sidecar Architecture)

- **Timers vs Reminders** — In-memory timers (lost on restart) vs persisted reminders (survive restarts)
- **Reentrancy tracking** — Tracks request chains to allow A→B→A calls without deadlock while preventing true concurrent access
- **Turn-based concurrency** — One message handler at a time per actor; reentrancy creates nested turns

**Adopted:** Timer vs Reminder distinction (future). **Adopted:** Reentrancy tracking concept for `request()` chains.

### Go CSP Patterns

- **Channels** — First-class typed communication; blocking send/receive by default
- **Select** — Wait on multiple channel operations, proceed with first ready
- **Context cancellation** — `context.Context` carries deadlines and cancellation through call chains
- **errgroup** — Structured concurrency for goroutines

**Adopted:** Context propagation for cancellation/deadlines. **Skipped:** Raw channels (actors already provide this abstraction).

### Clojure core.async

- **Buffer strategies** — Fixed (backpressure), Dropping (drop newest), Sliding (drop oldest)
- **Pub/sub with topics** — Publish to topics, subscribers filter by predicate
- **Mult/Mix combinators** — `mult` (one→many broadcast), `mix` (many→one merge)
- **Transducers on channels** — Apply transformations to channel data without intermediate collections

**Adopted:** Buffer strategies for mailboxes. **Skipped:** Transducers (Rust iterators serve this purpose).

---

## Synthesis: Lessons Applied

1. **Full visibility** — Like Erlang, track all processes (including short-lived Tasks)
2. **Typed channels** — `ActorRef` provides type-safe message passing like Gleam's Subject
3. **Task for async work** — Task/TaskGroup fills the gap for non-actor concurrent work (Elixir's Task.Supervisor)
4. **Layers not magic** — Explicit lifecycle by default, virtual actors as opt-in layer (Orleans)
5. **Anti-patterns in docs** — Document when NOT to use actors
6. **Buffer strategies** — Mailboxes should support fixed/dropping/sliding (core.async)
7. **Level-triggered cancellation** — Cancellation as persistent state (Trio)
8. **Joiner policies** — TaskGroup needs configurable completion policies (Loom)
9. **Timer vs Reminder** — Ephemeral vs durable scheduled work (Dapr)
10. **Typed message interfaces** — Compile-time verification (CAF, Gleam)
11. **Context propagation** — Cancellation/deadlines flow through call chains (Go)
12. **Reentrancy tracking** — Prevent deadlock in A→B→A call chains (Dapr)

### What Spawned Adopted for v0.5

| Feature | Source | Status |
|---------|--------|--------|
| Handler\<M\> pattern | Actix | Done |
| Recipient\<M\> type erasure | Actix | Done |
| `#[protocol]` + `#[actor]` macros | Original design | Done |
| Named registry | Erlang, Ractor | Done |
| Dual execution modes | Original design | Done |

### What's Planned

| Feature | Source | Priority |
|---------|--------|----------|
| Supervision trees | Erlang, Ractor, Bastion | High (required for 1.0) |
| Meltdown protection / backoff | Bastion, Ractor | High |
| Buffer strategies | core.async | Medium |
| Links & monitors | Erlang | Medium |
| Level-triggered cancellation | Trio | Medium |

### What's Deferred

| Feature | Source | Rationale |
|---------|--------|-----------|
| Distributed actors | Ractor, Proto.Actor | Significant complexity |
| Virtual actors | Orleans, Proto.Actor | Opt-in layer for future |
| Persistence / event sourcing | Akka | Different problem domain |
| WASM isolation | Lunatic | Too heavyweight |
| Process groups (pg) | Erlang, Ractor | After supervision |
