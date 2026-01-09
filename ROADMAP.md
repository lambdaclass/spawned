# Spawned Roadmap: Scheduling & Fairness

This document outlines the design for adding cooperative scheduling controls, fairness mechanisms, and starvation detection to Spawned. The design draws inspiration from **Orleans** (Microsoft's virtual actor framework) and **Akka** (Lightbend's actor toolkit).

---

## Table of Contents

1. [Motivation](#motivation)
2. [Technical Background](#technical-background)
   - [How Erlang Does It](#how-erlang-does-it)
   - [Why Rust Can't Do This](#why-rust-cant-do-this)
   - [How Go Does It (And Why We Can't Copy It)](#how-go-does-it-and-why-we-cant-copy-it)
   - [What Orleans Does](#what-orleans-does)
   - [What Akka Does](#what-akka-does)
3. [Alternatives Considered](#alternatives-considered)
   - [Signal-Based Preemption](#1-signal-based-preemption-go-style)
   - [Panic-Based "Preemption"](#2-panic-based-preemption)
   - [Thread Cancellation](#3-thread-cancellation-nuclear-option)
   - [WASM-Based True Preemption](#4-wasm-based-true-preemption-lunatic-style)
   - [Cooperative Budget](#5-cooperative-budget-tokios-coop)
   - [Manual Yield Points](#6-manual-yield-points)
4. [Current State](#current-state)
5. [Proposed Design](#proposed-design)
   - [Scheduling Configuration](#1-scheduling-configuration)
   - [Reentrancy Control](#2-reentrancy-control-orleans-style)
   - [Call Chain Tracking](#3-call-chain-tracking)
   - [Throughput-Based Fairness](#4-throughput-based-fairness-akka-style)
   - [Handler Timeouts](#5-handler-timeouts)
   - [Starvation Detection](#6-starvation-detection-akka-style)
   - [GenServer Trait Extensions](#7-genserver-trait-extensions)
   - [ReadOnly Handlers](#8-readonly-handlers-orleans-style)
   - [Backend Integration](#9-backend-integration)
   - [Metrics & Observability](#10-metrics--observability)
   - [Yield Helper Utility](#11-yield-helper-utility)
6. [Implementation Phases](#implementation-phases)
7. [Example Usage](#example-usage)
8. [What This Doesn't Solve](#what-this-doesnt-solve)
9. [Frequently Asked Questions](#frequently-asked-questions)
10. [Design Principles](#design-principles)
11. [Conclusion](#conclusion)
12. [References](#references)

---

## Motivation

### The Problem

Erlang's BEAM VM provides true preemptive scheduling through "reduction counting" - each process gets a budget of ~2000-4000 operations before being forcibly yielded. This is possible because BEAM controls the entire execution environment.

Rust's async model is fundamentally **cooperative** - tasks only yield at `.await` points. Between await points, there's nothing the runtime can do to interrupt a misbehaving task. This means:

- A CPU-bound loop blocks the executor thread
- One slow GenServer can starve others on the same thread pool
- No automatic fairness guarantees

### What Other Frameworks Do

| Framework | Preemption | Fairness Mechanism |
|-----------|------------|-------------------|
| **Erlang/BEAM** | True (reductions) | Automatic |
| **Go** | Semi (signals) | Runtime-injected safe points |
| **Orleans (.NET)** | None | Await-point interleaving, reentrancy control, timeouts |
| **Akka (JVM)** | None | Throughput limits, dedicated dispatchers, starvation detection |

**Key insight**: No mainstream actor framework except Erlang has true preemption. They all use cooperative scheduling with various fairness and isolation mechanisms.

### Our Approach

Accept cooperative scheduling as a constraint and provide maximum control within that model:

1. **Detection**: Know when things go wrong (starvation detection, slow handler callbacks)
2. **Fairness**: Throughput limits to prevent one GenServer from monopolizing the executor
3. **Reentrancy**: Orleans-style control over message interleaving
4. **Timeouts**: Handler-level timeouts with configurable actions
5. **Let-it-crash**: Integration with supervisors for recovery

---

## Technical Background

This section explains *why* true preemption is difficult in Rust and what alternatives exist.

### How Erlang Does It

Erlang's BEAM VM achieves true preemption through **reduction counting**:

```
┌─────────────────────────────────────────────────────────────┐
│  BEAM VM Scheduler                                          │
│                                                             │
│  1. Process starts executing                                │
│  2. Every function call decrements reduction counter        │
│  3. When counter hits 0 (~2000-4000 reductions):            │
│     - Save process state                                    │
│     - Switch to next process in run queue                   │
│  4. Process resumes later with fresh reduction budget       │
└─────────────────────────────────────────────────────────────┘
```

This works because:

| BEAM Advantage | Why It Works |
|----------------|--------------|
| **Bytecode interpreter** | VM executes every instruction, can check counters |
| **Controlled execution** | All code runs through the VM, no escape |
| **Known process state** | VM can save/restore process state at any point |
| **No native code** | Even NIFs have special handling |

### Why Rust Can't Do This

Rust compiles to native machine code. Once a function is running, there's nothing between the code and the CPU:

```
┌─────────────────────────────────────────────────────────────┐
│  Rust Execution Model                                       │
│                                                             │
│  Native Code ──────────────────────────────────► CPU        │
│       │                                                     │
│       └── No interpreter, no VM, no reduction counting      │
│                                                             │
│  Async/Await adds yield points, but only where you write    │
│  `.await` - between those points, code runs uninterrupted   │
└─────────────────────────────────────────────────────────────┘
```

| Challenge | Why It's Hard |
|-----------|---------------|
| **No VM control** | Can't inject yield points into compiled native code |
| **Cooperative async** | Tasks must voluntarily yield at `.await` points |
| **Opaque futures** | Runtime can't inspect or manipulate future internals |
| **No reduction counting** | No mechanism to count operations |

### How Go Does It (And Why We Can't Copy It)

Go evolved from cooperative (pre-1.14) to **signal-based asynchronous preemption** (1.14+):

```
┌─────────────────────────────────────────────────────────────┐
│  Go's Preemption Model (1.14+)                              │
│                                                             │
│  sysmon goroutine (monitoring thread)                       │
│    │                                                        │
│    ├─► Detects goroutine running >10ms                      │
│    │                                                        │
│    └─► Sends SIGURG to the goroutine's thread               │
│              │                                              │
│              ▼                                              │
│    Signal handler sets preemption flag on goroutine stack   │
│              │                                              │
│              ▼                                              │
│    At next "safe point", goroutine checks flag and yields   │
└─────────────────────────────────────────────────────────────┘
```

**Why this works for Go but not Rust:**

| Go Advantage | Rust Reality |
|--------------|--------------|
| **Compiler inserts safe points** | No compiler support - would need to modify rustc |
| **Runtime knows goroutine state** | Future state is opaque, can't be inspected |
| **Unified goroutine abstraction** | Futures can be any shape, no standard layout |
| **Controlled stack management** | Rust uses native stacks, can't inject checks |
| **Single runtime** | Multiple async runtimes (tokio, async-std, etc.) |

Even if we sent signals to threads:
- **For `Backend::Async`**: Tokio tasks share worker threads. A signal hits the thread, not the specific task
- **For `Backend::Thread`**: We could send signals, but there's nowhere safe to yield without compiler support

### What Orleans Does

Orleans uses **turn-based, single-threaded execution** with configurable interleaving:

```
┌─────────────────────────────────────────────────────────────┐
│  Orleans Grain Execution                                    │
│                                                             │
│  Turn 1: Process message A                                  │
│    │                                                        │
│    └─► await SomeIO()  ◄─── Interleaving point              │
│              │                                              │
│  Turn 2: Process message B (if [Reentrant])                 │
│    │                                                        │
│    └─► await OtherIO()                                      │
│              │                                              │
│  Turn 3: Continue message A                                 │
└─────────────────────────────────────────────────────────────┘
```

Key features:
- **`[Reentrant]` attribute**: Allows interleaving at await points
- **`[ReadOnly]` methods**: Can run concurrently (no state mutation)
- **Call chain reentrancy**: Prevents A→B→A deadlocks
- **Timeouts**: Calls that take too long throw exceptions

### What Akka Does

Akka uses **dispatcher-based scheduling** with fairness controls:

```
┌─────────────────────────────────────────────────────────────┐
│  Akka Dispatcher Model                                      │
│                                                             │
│  Dispatcher (thread pool)                                   │
│    │                                                        │
│    ├─► Actor A: process up to N messages (throughput)       │
│    │     │                                                  │
│    │     └─► After N messages, return thread to pool        │
│    │                                                        │
│    ├─► Actor B: process up to N messages                    │
│    │                                                        │
│    └─► Starvation detector: monitor scheduling latency      │
└─────────────────────────────────────────────────────────────┘
```

Key features:
- **Throughput setting**: Max messages per actor before yielding (1 = fairest)
- **Dedicated dispatchers**: Isolate blocking actors to separate thread pools
- **Starvation detector**: Monitors scheduling latency, logs warnings with stack traces
- **Internal dispatcher**: Protects Akka internals from user code starvation

---

## Alternatives Considered

We evaluated several approaches before settling on the Orleans/Akka-inspired design:

### 1. Signal-Based Preemption (Go-style)

**Idea**: Send OS signals (SIGURG) to threads running slow handlers.

```rust
// Pseudocode
fn monitor_handler(thread_id: ThreadId, timeout: Duration) {
    sleep(timeout);
    if handler_still_running() {
        pthread_kill(thread_id, SIGURG);
    }
}

extern "C" fn signal_handler(_: i32) {
    PREEMPT_FLAG.store(true, Ordering::SeqCst);
}
```

**Why we rejected it:**

| Problem | Impact |
|---------|--------|
| No safe points | Signal sets a flag, but code must check it - no compiler support |
| Shared threads | For `Backend::Async`, signal hits all tasks on the thread |
| Platform-specific | Different signals/behavior on Linux vs macOS vs Windows |
| Unsafe interactions | Signal handlers have severe restrictions on what they can do |

**Verdict**: Too fragile, doesn't provide true preemption without compiler support.

### 2. Panic-Based "Preemption"

**Idea**: Signal handler triggers a panic, which Rust unwinds.

```rust
extern "C" fn preemption_handler(_: i32) {
    panic!("Handler exceeded time limit");  // UNDEFINED BEHAVIOR
}
```

The existing `catch_unwind` in the message loop would catch this.

**Why we rejected it:**

| Problem | Impact |
|---------|--------|
| **Undefined behavior** | Panicking from signal handler is UB in many cases |
| **State corruption** | Panic might occur in the middle of a data structure update |
| **Not a yield** | Destroys the handler, doesn't pause it |

**Verdict**: Dangerous and doesn't achieve the goal.

### 3. Thread Cancellation (Nuclear Option)

**Idea**: For `Backend::Thread`, forcibly kill the thread after timeout.

```rust
// After timeout
pthread_cancel(thread_id);
// or
thread_handle.abort();
```

**Why we rejected it as primary mechanism:**

| Problem | Impact |
|---------|--------|
| Resource leaks | Thread may hold locks, file handles, etc. |
| No cleanup | Destructors may not run |
| Abrupt termination | No graceful shutdown |

**Verdict**: Could be a last-resort option combined with supervisors, but not a primary mechanism.

### 4. WASM-Based True Preemption (Lunatic-style)

**Idea**: Compile GenServer handlers to WebAssembly and run in Wasmtime with fuel metering.

```
┌─────────────────────────────────────────────────────────────┐
│  WASM Preemption Model                                      │
│                                                             │
│  Rust handler code                                          │
│       │                                                     │
│       ▼                                                     │
│  Compile to .wasm                                           │
│       │                                                     │
│       ▼                                                     │
│  Run in Wasmtime with fuel metering                         │
│       │                                                     │
│       └─► Fuel exhausted → yield to scheduler               │
└─────────────────────────────────────────────────────────────┘
```

The [Lunatic](https://github.com/lunatic-solutions/lunatic) project does exactly this.

**Why we deferred it:**

| Consideration | Assessment |
|---------------|------------|
| **Achieves true preemption** | Yes - this actually works |
| **Architectural change** | Massive - requires WASM compilation pipeline |
| **Performance overhead** | WASM is slower than native code |
| **Ecosystem compatibility** | Can't use arbitrary Rust crates in WASM |
| **Complexity** | Significant learning curve |

**Verdict**: Viable for a future major version, but too large a change for incremental improvement. The Orleans/Akka approach provides 80% of the benefit with 20% of the effort.

### 5. Cooperative Budget (Tokio's `coop`)

**Idea**: Track "work units" and yield when budget exhausted.

Tokio already has internal budget tracking in its `coop` module. Tasks that do too many operations get forced to yield at their next `.await`.

**Assessment:**

| Pro | Con |
|-----|-----|
| Already exists in Tokio | Still requires `.await` points |
| No unsafe code | Can't help CPU-bound code |
| Proven in production | Not exposed for user configuration |

**Verdict**: Good foundation, but doesn't solve the core problem. We build on this concept with throughput limits.

### 6. Manual Yield Points

**Idea**: Provide helpers for users to insert yield points in long-running code.

```rust
pub async fn yield_every(counter: &mut u32, budget: u32) {
    *counter += 1;
    if *counter >= budget {
        *counter = 0;
        tokio::task::yield_now().await;
    }
}

// Usage
async fn handle_call(&mut self, msg: Msg, _: &Handle) -> CallResponse {
    let mut i = 0;
    for item in self.huge_list.iter() {
        yield_every(&mut i, 100).await;
        process(item);
    }
    CallResponse::Reply(result)
}
```

**Assessment:**

| Pro | Con |
|-----|-----|
| Simple to implement | Requires developer discipline |
| No magic | Must remember to use it |
| Works anywhere | Doesn't help existing code |

**Verdict**: Include as a utility, but not a complete solution.

### Summary: Why Orleans/Akka Approach

After evaluating alternatives, we chose the Orleans/Akka approach because:

1. **Proven at scale**: Both frameworks handle massive production workloads
2. **No unsafe code**: Pure Rust implementation, no signal handling
3. **Incremental adoption**: Users can opt-in to features as needed
4. **Composable with supervisors**: "Let it crash" philosophy for recovery
5. **Observable**: Starvation detection tells you when things go wrong
6. **Pragmatic**: Accepts cooperative scheduling as a constraint

The philosophy is: **Don't try to preempt; detect and recover.**

---

## Current State

Spawned already has some relevant infrastructure:

- **Three backends**: `Backend::Async`, `Backend::Blocking`, `Backend::Thread` provide isolation options
- **`WarnOnBlocking`**: Debug-mode wrapper that detects when polls take >10ms (detection only)
- **Supervisors**: `Supervisor` and `DynamicSupervisor` for crash recovery
- **Call timeouts**: `call_with_timeout()` for individual calls

What's missing:

- Per-GenServer scheduling configuration
- Throughput-based fairness
- Reentrancy control
- Global starvation detection
- Handler-level timeouts (not just call timeouts)
- Metrics/observability hooks

---

## Proposed Design

### 1. Scheduling Configuration

Per-GenServer configuration for scheduling behavior:

```rust
/// Per-GenServer scheduling configuration
pub struct SchedulingConfig {
    /// Maximum messages processed before yielding to other tasks.
    /// Lower = fairer but higher overhead. Default: 100
    /// Set to 1 for maximum fairness (Akka-style throughput)
    pub throughput: u32,

    /// Maximum time for a single handler execution.
    /// None = no limit (current behavior)
    pub handler_timeout: Option<Duration>,

    /// What to do when handler times out
    pub timeout_action: TimeoutAction,

    /// Reentrancy behavior for this GenServer
    pub reentrancy: Reentrancy,
}

pub enum TimeoutAction {
    /// Log warning, let handler continue (detection only)
    Warn,
    /// Cancel the handler future, skip message, continue GenServer
    Cancel,
    /// Stop the GenServer (supervisor will restart)
    Stop,
}

impl Default for SchedulingConfig {
    fn default() -> Self {
        Self {
            throughput: 100,
            handler_timeout: None,
            timeout_action: TimeoutAction::Warn,
            reentrancy: Reentrancy::None,
        }
    }
}
```

### 2. Reentrancy Control (Orleans-style)

Control how messages interleave during async operations:

```rust
/// Orleans-style reentrancy control
pub enum Reentrancy {
    /// Default: Process each message to completion before next.
    /// Simplest mental model, no interleaving surprises.
    None,

    /// Allow interleaving at any await point within handlers.
    /// Higher throughput but requires careful state management.
    Full,

    /// Only allow reentrancy for callbacks in same call chain.
    /// Prevents A→B→A deadlocks without full interleaving.
    CallChain,
}
```

**Why this matters:**

```
GenServer A calls GenServer B
GenServer B calls back to GenServer A
Without reentrancy: DEADLOCK (A is waiting for B, B is waiting for A)
With CallChain reentrancy: B's callback is allowed through
```

### 3. Call Chain Tracking

To support `Reentrancy::CallChain`, messages carry call chain information:

```rust
/// Tracks the call chain for reentrancy decisions
#[derive(Clone, Debug)]
pub struct CallChain {
    /// Stack of Pids in the current call chain
    chain: Vec<Pid>,
    /// Unique identifier for this call chain
    chain_id: u64,
}

impl CallChain {
    /// Check if a Pid is already in the call chain
    pub fn contains(&self, pid: Pid) -> bool {
        self.chain.contains(&pid)
    }

    /// Extend chain when making an outgoing call
    pub fn extend(&self, caller: Pid) -> Self {
        let mut new_chain = self.chain.clone();
        new_chain.push(caller);
        Self {
            chain: new_chain,
            chain_id: self.chain_id,
        }
    }
}

/// Messages carry call chain for reentrancy decisions
pub enum GenServerInMsg<G: GenServer> {
    Call {
        sender: oneshot::Sender<Result<G::OutMsg, GenServerError>>,
        message: G::CallMsg,
        call_chain: Option<CallChain>,  // For reentrancy
    },
    Cast {
        message: G::CastMsg,
    },
}
```

### 4. Throughput-Based Fairness (Akka-style)

Prevent one GenServer from monopolizing the executor:

```rust
/// Internal counter for throughput-based fairness
struct ThroughputCounter {
    processed: u32,
    limit: u32,
}

impl ThroughputCounter {
    fn new(limit: u32) -> Self {
        Self { processed: 0, limit }
    }

    /// Called after each message. Returns true if should yield.
    fn tick(&mut self) -> bool {
        self.processed += 1;
        if self.processed >= self.limit {
            self.processed = 0;
            true
        } else {
            false
        }
    }
}
```

In the main loop:

```rust
async fn main_loop(...) {
    let config = self.scheduling_config();
    let mut throughput = ThroughputCounter::new(config.throughput);

    loop {
        if !self.receive(handle, rx, system_rx).await {
            break;
        }

        // Yield after throughput limit reached
        if throughput.tick() {
            tokio::task::yield_now().await;
        }
    }
}
```

**Tuning guidance:**

| `throughput` | Behavior | Use Case |
|--------------|----------|----------|
| 1 | Yield after every message | Maximum fairness, latency-sensitive |
| 10-100 | Balanced | General purpose |
| 1000+ | Batch processing | High throughput, less fairness |

### 5. Handler Timeouts

Wrap handler execution with configurable timeouts:

```rust
/// Wraps handler execution with timeout and metrics
async fn execute_with_timeout<F, T>(
    future: F,
    config: &SchedulingConfig,
    handler_type: HandlerType,
    on_timeout: impl FnOnce(Duration),
) -> Result<T, HandlerTimeoutError>
where
    F: Future<Output = T>,
{
    let start = Instant::now();

    match config.handler_timeout {
        Some(timeout_duration) => {
            match timeout(timeout_duration, future).await {
                Ok(result) => Ok(result),
                Err(_) => {
                    on_timeout(start.elapsed());
                    match config.timeout_action {
                        TimeoutAction::Warn => {
                            // Future was cancelled, can't get result
                            Err(HandlerTimeoutError::Cancelled)
                        }
                        TimeoutAction::Cancel => {
                            Err(HandlerTimeoutError::Cancelled)
                        }
                        TimeoutAction::Stop => {
                            Err(HandlerTimeoutError::Stop)
                        }
                    }
                }
            }
        }
        None => Ok(future.await),
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HandlerType {
    Init,
    HandleCall,
    HandleCast,
    HandleInfo,
    Teardown,
}
```

**Important limitation**: Timeouts only work at `.await` points. A CPU-bound loop without awaits will not be interrupted by the timeout - it will only trigger after the next await.

### 6. Starvation Detection (Akka-style)

Global monitoring for executor health:

```rust
pub struct StarvationConfig {
    /// How often to check for starvation. Default: 1s
    pub check_interval: Duration,

    /// Scheduling delay threshold to trigger warning. Default: 100ms
    pub warning_threshold: Duration,

    /// Scheduling delay threshold to trigger critical alert. Default: 1s
    pub critical_threshold: Duration,

    /// Whether to capture stack traces on starvation (expensive)
    pub capture_stack_traces: bool,
}

pub struct StarvationDetector {
    config: StarvationConfig,
}

impl StarvationDetector {
    /// Spawns background task that periodically checks scheduling latency
    pub fn start(config: StarvationConfig) -> StarvationDetectorHandle {
        spawn(async move {
            let mut interval = tokio::time::interval(config.check_interval);

            loop {
                interval.tick().await;
                let check_start = Instant::now();

                // Schedule a probe task and measure latency
                let (tx, rx) = oneshot::channel();
                spawn(async move { let _ = tx.send(Instant::now()); });

                if let Ok(scheduled_at) = rx.await {
                    let delay = scheduled_at - check_start;

                    if delay > config.critical_threshold {
                        tracing::error!(
                            delay = ?delay,
                            "CRITICAL: Executor starvation detected. \
                             Tasks are not being scheduled promptly. \
                             This may indicate blocking code or CPU overload."
                        );
                    } else if delay > config.warning_threshold {
                        tracing::warn!(
                            delay = ?delay,
                            "Executor starvation detected"
                        );
                    }
                }
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct StarvationEvent {
    /// How long the probe task waited to be scheduled
    pub scheduling_delay: Duration,
    /// Severity level
    pub severity: StarvationSeverity,
    /// Thread stack traces (if capture enabled)
    pub stack_traces: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum StarvationSeverity {
    Warning,
    Critical,
}
```

### 7. GenServer Trait Extensions

Add hooks for scheduling events:

```rust
pub trait GenServer: Send + Sized {
    // ... existing associated types and methods ...

    /// Override to provide custom scheduling configuration.
    fn scheduling_config(&self) -> SchedulingConfig {
        SchedulingConfig::default()
    }

    /// Called when a handler exceeds the timeout threshold.
    /// Default implementation logs a warning.
    /// Override to emit metrics, trigger alerts, etc.
    fn on_slow_handler(
        &mut self,
        handler: HandlerType,
        duration: Duration,
        message_type: &str,
    ) {
        tracing::warn!(
            handler = ?handler,
            duration = ?duration,
            message = message_type,
            "Handler exceeded timeout threshold"
        );
    }

    /// Called when starvation is detected affecting this GenServer.
    /// Default implementation logs a warning.
    fn on_starvation(&mut self, event: StarvationEvent) {
        tracing::warn!(event = ?event, "Executor starvation detected");
    }
}
```

### 8. ReadOnly Handlers (Orleans-style)

Allow concurrent execution of read-only operations:

```rust
pub enum CallResponse<G: GenServer> {
    /// Normal reply, exclusive access assumed
    Reply(G::OutMsg),

    /// Reply from a read-only operation.
    /// Multiple ReadOnly responses can be processed concurrently.
    ReplyReadOnly(G::OutMsg),

    /// Message type not handled
    Unused,

    /// Reply and stop the GenServer
    Stop(G::OutMsg),
}
```

When a handler returns `ReplyReadOnly`, the GenServer knows the operation didn't modify state and can potentially process other ReadOnly messages concurrently (when `Reentrancy::Full` is enabled).

### 9. Backend Integration

Extend the Backend enum to accept scheduling configuration:

```rust
pub enum Backend {
    /// Tokio async task with default scheduling
    Async,

    /// Tokio async task with custom scheduling
    AsyncWithConfig(SchedulingConfig),

    /// Tokio blocking pool with default scheduling
    Blocking,

    /// Tokio blocking pool with custom scheduling
    BlockingWithConfig(SchedulingConfig),

    /// Dedicated OS thread with default scheduling
    Thread,

    /// Dedicated OS thread with custom scheduling
    ThreadWithConfig(SchedulingConfig),
}
```

Or alternatively, keep Backend simple and add a separate method:

```rust
impl<G: GenServer> G {
    fn start(self, backend: Backend) -> GenServerHandle<Self>;

    fn start_with_config(
        self,
        backend: Backend,
        config: SchedulingConfig,
    ) -> GenServerHandle<Self>;
}
```

### 10. Metrics & Observability

Define metrics that the scheduling system emits:

```rust
/// Metrics emitted by the scheduling system
pub trait SchedulingMetrics: Send + Sync {
    /// Record handler execution duration
    fn record_handler_duration(&self, pid: Pid, handler: HandlerType, duration: Duration);

    /// Record a handler timeout event
    fn record_timeout(&self, pid: Pid, handler: HandlerType);

    /// Record current mailbox size
    fn record_mailbox_size(&self, pid: Pid, size: usize);

    /// Record scheduling delay (starvation indicator)
    fn record_scheduling_delay(&self, delay: Duration);

    /// Record a throughput yield event
    fn record_throughput_yield(&self, pid: Pid);
}

/// No-op implementation for when metrics are disabled
pub struct NoOpMetrics;

impl SchedulingMetrics for NoOpMetrics {
    fn record_handler_duration(&self, _: Pid, _: HandlerType, _: Duration) {}
    fn record_timeout(&self, _: Pid, _: HandlerType) {}
    fn record_mailbox_size(&self, _: Pid, _: usize) {}
    fn record_scheduling_delay(&self, _: Duration) {}
    fn record_throughput_yield(&self, _: Pid) {}
}
```

Users can implement this trait to integrate with their metrics backend (Prometheus, StatsD, etc.).

### 11. Yield Helper Utility

For users who want to make their handlers cooperative:

```rust
/// Helper for inserting yield points in long-running handlers.
///
/// Call this in loops that process many items to ensure fair scheduling.
///
/// # Example
///
/// ```rust
/// async fn handle_call(&mut self, msg: ProcessAll, _: &Handle) -> CallResponse {
///     let mut budget = YieldBudget::new(100);  // Yield every 100 iterations
///
///     for item in self.items.iter() {
///         budget.tick().await;
///         process(item);
///     }
///
///     CallResponse::Reply(Done)
/// }
/// ```
pub struct YieldBudget {
    count: u32,
    limit: u32,
}

impl YieldBudget {
    pub fn new(limit: u32) -> Self {
        Self { count: 0, limit }
    }

    /// Increment counter and yield if budget exhausted.
    pub async fn tick(&mut self) {
        self.count += 1;
        if self.count >= self.limit {
            self.count = 0;
            tokio::task::yield_now().await;
        }
    }

    /// Check without yielding (for conditional logic)
    pub fn should_yield(&self) -> bool {
        self.count >= self.limit
    }
}

/// Convenience function for simple cases
pub async fn yield_every(counter: &mut u32, budget: u32) {
    *counter += 1;
    if *counter >= budget {
        *counter = 0;
        tokio::task::yield_now().await;
    }
}
```

This is a **voluntary** mechanism - it requires developer discipline but provides explicit control.

---

## Implementation Phases

### Phase 1: Foundation (Core Infrastructure)

**Priority: High**

1. **`SchedulingConfig` struct** and defaults
2. **`scheduling_config()` trait method** on GenServer
3. **Throughput counter** and yield-after-N-messages logic
4. **Handler timeout wrapper** with `TimeoutAction` enum

**Deliverables:**
- GenServers can be configured with throughput limits
- Handlers can have timeouts with Warn/Cancel/Stop actions
- Backward compatible - default config matches current behavior

### Phase 2: Detection & Observability

**Priority: High**

1. **Enhanced `WarnOnBlocking`** - make thresholds configurable
2. **`on_slow_handler()` callback** - let users hook into slow handler events
3. **`StarvationDetector`** - global executor health monitoring
4. **`SchedulingMetrics` trait** - integration point for metrics backends

**Deliverables:**
- Users can detect scheduling problems in production
- Metrics can be exported to monitoring systems
- Starvation is detected and logged before it causes cascading failures

### Phase 3: Reentrancy Control

**Priority: Medium**

1. **`Reentrancy` enum** - None, Full, CallChain
2. **`CallChain` struct** - tracking for deadlock prevention
3. **Modified message types** - carry call chain information
4. **Reentrancy logic in `receive()`** - queue/process decisions

**Deliverables:**
- Users can enable interleaving for high-throughput scenarios
- A→B→A deadlocks are prevented with CallChain reentrancy
- Clear mental model for each reentrancy mode

### Phase 4: Advanced Features

**Priority: Low**

1. **`ReplyReadOnly` response** - concurrent read operations
2. **Per-handler timeout configuration** - different timeouts for call vs cast
3. **Mailbox size limits** - backpressure when mailbox grows too large
4. **Priority messages** - some messages skip the queue

**Deliverables:**
- Read-heavy workloads can scale better
- Fine-grained control over timeouts
- Backpressure prevents memory exhaustion

---

## Example Usage

### Basic: Default Configuration (Current Behavior)

```rust
// No changes needed - defaults match current behavior
let handle = MyServer::new().start(Backend::Async);
```

### Fair Scheduling

```rust
impl GenServer for LatencySensitiveServer {
    fn scheduling_config(&self) -> SchedulingConfig {
        SchedulingConfig {
            throughput: 1,  // Yield after every message
            ..Default::default()
        }
    }
}
```

### Handler Timeouts with Supervisor Recovery

```rust
impl GenServer for UnreliableWorker {
    fn scheduling_config(&self) -> SchedulingConfig {
        SchedulingConfig {
            handler_timeout: Some(Duration::from_secs(30)),
            timeout_action: TimeoutAction::Stop,  // Crash and restart
            ..Default::default()
        }
    }
}

// Supervisor will restart the worker when it times out
let spec = SupervisorSpec::new()
    .with_child(ChildSpec::new("worker", || {
        UnreliableWorker::new().start(Backend::Async)
    }));
```

### Preventing Deadlocks

```rust
impl GenServer for ServiceA {
    fn scheduling_config(&self) -> SchedulingConfig {
        SchedulingConfig {
            reentrancy: Reentrancy::CallChain,  // Allow callbacks
            ..Default::default()
        }
    }

    async fn handle_call(&mut self, msg: Msg, handle: &Handle) -> CallResponse {
        // This can call ServiceB, which can call back to us
        // CallChain reentrancy prevents deadlock
        let result = self.service_b.call(NeedInfo).await?;
        CallResponse::Reply(result)
    }
}
```

### Global Starvation Detection

```rust
fn main() {
    spawned_rt::tasks::run(async {
        // Start starvation detection
        StarvationDetector::start(StarvationConfig {
            check_interval: Duration::from_secs(1),
            warning_threshold: Duration::from_millis(100),
            critical_threshold: Duration::from_secs(1),
            capture_stack_traces: cfg!(debug_assertions),
        });

        // Start your application
        start_application().await;
    });
}
```

### Custom Metrics Integration

```rust
struct PrometheusMetrics {
    handler_duration: Histogram,
    timeouts: Counter,
}

impl SchedulingMetrics for PrometheusMetrics {
    fn record_handler_duration(&self, pid: Pid, handler: HandlerType, duration: Duration) {
        self.handler_duration
            .with_label_values(&[&pid.to_string(), &format!("{:?}", handler)])
            .observe(duration.as_secs_f64());
    }

    fn record_timeout(&self, pid: Pid, handler: HandlerType) {
        self.timeouts
            .with_label_values(&[&pid.to_string(), &format!("{:?}", handler)])
            .inc();
    }

    // ... other methods
}
```

---

## What This Doesn't Solve

This section is an honest assessment of limitations. Understanding what we *can't* do is as important as knowing what we can.

### True Preemption

This design does not provide Erlang-style preemption. CPU-bound code between `.await` points cannot be interrupted.

**Example of what we CAN'T help:**

```rust
async fn handle_call(&mut self, msg: Msg, _: &Handle) -> CallResponse {
    // This loop has no await points - it WILL block the executor
    // No amount of configuration can interrupt it
    for i in 0..1_000_000_000 {
        self.data.push(expensive_computation(i));
    }
    CallResponse::Reply(self.data.len())
}
```

**What the user must do:**

```rust
async fn handle_call(&mut self, msg: Msg, _: &Handle) -> CallResponse {
    let mut budget = YieldBudget::new(1000);
    for i in 0..1_000_000_000 {
        budget.tick().await;  // User must add this
        self.data.push(expensive_computation(i));
    }
    CallResponse::Reply(self.data.len())
}
```

**The only ways to achieve true preemption in Rust:**

| Approach | Feasibility | Trade-offs |
|----------|-------------|------------|
| WASM compilation (Lunatic) | Works | Performance overhead, ecosystem limits |
| Compiler modifications | Would require rustc changes | Not happening |
| Signal-based | Partially works | Unsafe, platform-specific, no safe points |

None of these are in scope for this roadmap.

### Blocking FFI Calls

Calls to blocking C libraries or system calls cannot be interrupted by any mechanism:

```rust
async fn handle_call(&mut self, msg: Msg, _: &Handle) -> CallResponse {
    // This blocks the thread entirely - even Backend::Thread can't help mid-call
    let result = unsafe { some_c_library_blocking_call() };
    CallResponse::Reply(result)
}
```

**Mitigations:**
- Use `Backend::Thread` to isolate the GenServer to its own OS thread
- Use `Backend::Blocking` to run on Tokio's blocking pool
- Wrap in `spawn_blocking` if only part of the handler blocks

### Guaranteed Latency

Without true preemption, **latency guarantees cannot be made**. A misbehaving GenServer can still cause latency spikes.

```
┌─────────────────────────────────────────────────────────────┐
│  What can happen even with all our features:                │
│                                                             │
│  GenServer A: starts processing message                     │
│       │                                                     │
│       └─► Enters CPU-bound section (no awaits)              │
│             │                                               │
│             │   Meanwhile, GenServer B is waiting...        │
│             │   Starvation detector fires warning           │
│             │   But A keeps running until it hits an await  │
│             │                                               │
│       ◄─────┘                                               │
│       │                                                     │
│       └─► Finally awaits, B gets to run                     │
└─────────────────────────────────────────────────────────────┘
```

This design provides **detection and recovery**, not **prevention**.

### Infinite Loops

An infinite loop without await points will hang the GenServer (and potentially the executor thread):

```rust
async fn handle_call(&mut self, msg: Msg, _: &Handle) -> CallResponse {
    loop {
        // Infinite loop, no escape
        self.counter += 1;
    }
    // Never reached
}
```

**Detection**: `WarnOnBlocking` and `StarvationDetector` will fire warnings.
**Recovery**: `TimeoutAction::Stop` will eventually mark the GenServer for termination, but the actual termination only happens if the code reaches an await point (which it won't in an infinite loop).

**For `Backend::Thread`**: The thread is effectively lost until the process exits.

### Memory Exhaustion

This design doesn't address memory issues:
- A GenServer that allocates unboundedly will OOM
- Mailbox size limits (Phase 4) can help with message backlog, but not internal state growth

### Multi-GenServer Deadlocks

While `Reentrancy::CallChain` prevents A→B→A deadlocks, it doesn't prevent:
- A→B→C→A deadlocks (would require full distributed deadlock detection)
- Resource deadlocks (two GenServers waiting for the same external resource)

---

## Frequently Asked Questions

### Why not just use Erlang/Elixir?

Erlang is excellent for the problems it solves. Use it if:
- Your problem fits Erlang's sweet spot (telecom, messaging, soft real-time)
- You can accept Erlang's performance characteristics
- Your team knows Erlang

Use Spawned if:
- You need Rust's performance, safety, or ecosystem
- You're already in a Rust codebase
- You want actor-model benefits without leaving Rust

### Why not contribute preemption to Tokio?

Tokio has considered and rejected adding preemption. The fundamental issue is that async Rust is cooperative by design - changing this would require:
1. Language/compiler changes (yield points in loops)
2. Breaking changes to the Future trait
3. Runtime overhead that goes against Tokio's goals

The Tokio team's position is that blocking is a user bug that should be fixed in user code.

### Should I use `Backend::Thread` for everything?

No. `Backend::Thread` has costs:
- ~2MB stack per thread
- OS thread limits
- Context switch overhead

Use `Backend::Thread` for:
- GenServers that do long-running blocking work
- GenServers that call blocking FFI
- Isolation of unreliable/untrusted code

Use `Backend::Async` for:
- The majority of GenServers
- I/O-bound work
- Well-behaved async code

### How does this compare to the `async-std` runtime?

Both Tokio and async-std are cooperative. Neither has preemption. The design in this roadmap would work with either runtime (with some adaptation).

### Will this make my code "as good as" Erlang?

No. Erlang's preemption is a fundamental property of the BEAM VM that cannot be replicated in native code without significant trade-offs.

This design makes your code:
- **More observable**: You'll know when things go wrong
- **More recoverable**: Supervisors can restart stuck GenServers
- **More fair**: Throughput limits improve scheduling fairness
- **Safer from deadlocks**: Reentrancy prevents common deadlock patterns

But it does not make your code preemptive.

---

## Design Principles

1. **Backward Compatible**: Default configuration matches current behavior exactly
2. **Opt-in Complexity**: Simple use cases stay simple; advanced features are opt-in
3. **Detection Over Prevention**: We can't prevent blocking, but we can detect and recover
4. **Let It Crash**: Integration with supervisors for automatic recovery
5. **Observable**: Metrics and callbacks for production monitoring
6. **Configurable**: Different workloads need different tradeoffs

---

## Conclusion

This roadmap acknowledges a fundamental truth: **true preemption is not achievable in Rust without extraordinary measures** (WASM compilation, compiler changes, or unsafe signal handling). Rather than fighting this constraint, we embrace it and focus on what we *can* do:

1. **Make problems visible** through starvation detection and slow handler callbacks
2. **Provide fairness controls** through throughput limits and reentrancy configuration
3. **Enable recovery** through handler timeouts and supervisor integration
4. **Offer escape hatches** through `Backend::Thread` isolation and manual yield helpers

The result is a system that:
- Handles well-behaved async code with optimal performance
- Detects misbehaving code quickly
- Recovers from stuck handlers automatically
- Gives users the tools to fix problems when they occur

This is the same approach taken by Orleans and Akka - two battle-tested actor frameworks that handle massive production workloads despite lacking true preemption.

**The philosophy**: Don't try to preempt; detect, recover, and give users the tools to write better code.

---

## References

### Actor Frameworks

- [Akka Dispatchers Documentation](https://doc.akka.io/docs/akka/current/typed/dispatchers.html) - Akka's approach to fairness and thread pool management
- [Akka Starvation Detector](https://doc.akka.io/docs/akka-diagnostics/current/starvation-detector.html) - How Akka detects executor starvation
- [Orleans Request Scheduling](https://learn.microsoft.com/en-us/dotnet/orleans/grains/request-scheduling) - Orleans' turn-based execution model
- [Orleans Reentrancy](https://dotnet.github.io/orleans/docs/grains/reentrancy.html) - How Orleans handles message interleaving

### Runtime & Scheduling

- [Erlang Scheduling](https://www.erlang.org/doc/system/system_principles.html) - How BEAM achieves true preemption
- [Go Preemption Design Doc](https://github.com/golang/proposal/blob/master/design/24543-non-cooperative-preemption.md) - Go's signal-based preemption
- [Tokio Cooperative Scheduling](https://tokio.rs/blog/2020-04-preemption) - Why Tokio chose cooperative scheduling
- [Lunatic (WASM-based preemption)](https://github.com/lunatic-solutions/lunatic) - Erlang-like preemption via WASM

### Further Reading

- [Actors with Tokio](https://ryhl.io/blog/actors-with-tokio/) - Patterns for actors in async Rust
- [Joe Armstrong's Thesis](https://erlang.org/download/armstrong_thesis_2003.pdf) - "Making reliable distributed systems in the presence of software errors" - the philosophical foundation of Erlang's approach
