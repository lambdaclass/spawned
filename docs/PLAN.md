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

Analysis of how spawned compares to established actor frameworks:

| Framework | Language | Key Features | Spawned Comparison |
|-----------|----------|--------------|-------------------|
| **Erlang/OTP** | Erlang | gen_server, supervisors, hot reload, distributed | Spawned's Actor ≈ gen_server; missing supervision, distribution |
| **Akka** | Scala/Java | Typed actors, clustering, sharding, persistence | More mature; spawned lacks persistence/clustering |
| **Orleans** | C#/.NET | Virtual actors, auto-activation, always-exists | Different model; spawned uses explicit lifecycle |
| **Actix** | Rust | Tokio-based, Arbiter, `Recipient<M>` type erasure | Most similar; Actix more mature, spawned has dual modes |
| **Ractor** | Rust | Erlang-style supervision, distributed actors | Similar goals; Ractor has supervision already |

### What Spawned Has

- Clean `request()`/`send()` separation (like Erlang call/cast)
- Type-safe message passing
- Unique dual async/sync execution modes
- Lightweight, focused API

### What's Missing

- Supervision trees
- Process registry/naming
- Pid/actor identity
- Link/monitor primitives
- Persistence/event sourcing
- Clustering/distribution

## Critical API Issues (Pre-Phase 2)

Before building more features, these API design issues should be addressed:

### Issue #145: Circular Dependency with Bidirectional Actors

**Problem:** Current `ActorRef<T>` creates module-level circular dependencies when actors need to communicate bidirectionally.

**Impact:** Blocks real-world actor collaboration patterns.

**Solution direction:** Type-erased handles (like Actix's `Recipient<M>`) or PID-based addressing.

### Issue #144: Type Safety for Request/Reply

**Problem:** Callers must match the full `Reply` enum even when only a subset of variants are possible for a given request.

**Impact:** Verbose, error-prone code.

**Solution direction:** Per-message response types or derive macro.

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

### Consider Adopting

- **Akka's backoff supervision** - Add `SupervisorStrategy::RestartWithBackoff` in Phase 4
- **Actix's `Recipient<M>`** - Type-erased message recipients for #145

## References

- Issue #124: Framework comparison request
- Issue #138: v0.5 Roadmap
- Issue #144: Type safety for request/reply
- Issue #145: Circular dependency with bidirectional actors
