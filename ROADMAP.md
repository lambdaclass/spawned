# Spawned Roadmap: From Actor Framework to Complete Concurrency Platform

## Table of Contents

1. [Executive Summary](#executive-summary)
2. [Design Philosophy](#design-philosophy)
3. [Current Architecture](#current-architecture)
4. [v0.5 Features](#v05-features)
   - [Phase 1: Task/TaskGroup](#phase-1-structured-concurrency-tasktaskgroup)
   - [Phase 2: Deterministic Test Runtime](#phase-2-deterministic-test-runtime-spawned-test-crate)
   - [Phase 3: Observability](#phase-3-observability)
   - [Phase 4: Routers & Pools](#phase-4-routers--pools)
   - [Phase 5: Production Hardening](#phase-5-production-hardening)
5. [v1.0 Features](#v10-features)
   - [Phase 6: Persistence](#phase-6-persistence---event-sourcing)
   - [Phase 7: Distribution](#phase-7-distribution---clustering)
   - [Phase 8: I/O Integration](#phase-8-io-integration)
6. [v1.1+ Features](#v11-features)
   - [Phase 9: Virtual Actors](#phase-9-virtual-actors---orleans-style-convenience)
   - [Phase 10: Durable Reminders](#phase-10-durable-reminders)
   - [Phase 11: Streams/Pub-Sub](#phase-11-streams--pub-sub)
   - [Phase 12: Reliable Delivery](#phase-12-reliable-delivery)
7. [Scheduling & Fairness](#scheduling--fairness)
8. [Insights from Other Frameworks](#insights-from-other-frameworks)
9. [Implementation Details](#implementation-details)
10. [Feature Matrix](#complete-feature-matrix)
11. [References](#references)

---

## Executive Summary

Spawned v0.4.5 has a solid foundation with GenServer, supervision trees, linking/monitoring, and registry.

**v0.5 Target Scope (Production Features):**
1. **Structured concurrency** (Task, TaskGroup) - fills the gap for non-actor concurrent work
2. **Deterministic test runtime** (spawned-test crate) - the killer feature for reliable async testing
3. **Observability** - production-ready systems need visibility
4. **Routers/Pools** - common patterns should be first-class

**Future Vision (v1.0+):**
5. **Persistence** - event sourcing for durable actors
6. **Distribution** - clustering and location-transparent messaging
7. **Virtual Actors** - Orleans/Akka-style automatic lifecycle (opt-in)
8. **Streams** - pub/sub with backpressure

---

## Design Philosophy

### Orleans Power + Erlang Simplicity

| Orleans/Akka | Erlang | Spawned Goal |
|--------------|--------|--------------|
| Virtual actors (automatic lifecycle) | Explicit spawn/stop | **Both**: explicit by default, virtual opt-in |
| Single activation guarantee | Manual enforcement | **Built-in** for virtual actors |
| Durable reminders | In-memory timers | **Both**: in-memory default, durable opt-in |
| Automatic placement | Manual node selection | **Both**: manual default, auto opt-in |
| Hidden complexity | Visible complexity | **Visible**: you understand what happens |

### Core Principle: Layers, Not Replacements

```
+-------------------------------------------------------------+
|  Virtual Actors (EntityManager, ClusterSingleton)           |  <- Orleans-like convenience
|  "Address by ID, framework manages lifecycle"               |     (OPT-IN)
+-------------------------------------------------------------+
|  Cluster Sharding + Distribution                            |  <- Distributed actors
|  "Same API, actors can be anywhere"                         |     (OPT-IN)
+-------------------------------------------------------------+
|  Persistence (PersistentGenServer)                          |  <- Durable state
|  "Events persisted, state survives restarts"                |     (OPT-IN)
+-------------------------------------------------------------+
|  GenServer + Supervisor + Registry                          |  <- Erlang foundation
|  "Explicit lifecycle, message passing, let it crash"        |     (ALWAYS AVAILABLE)
+-------------------------------------------------------------+
```

**You can always drop down a layer**. Virtual actors are GenServers under the hood. You're never locked into magic.

---

## Current Architecture

### GenServer (`concurrency/src/tasks/gen_server.rs`)
- Uses `mpsc::channel` for user messages (Call/Cast)
- Uses separate `mpsc::channel` for system messages (Down/Exit/Timeout)
- `select_biased!` prioritizes system messages over user messages
- `CancellationToken` for graceful shutdown
- Registered in global `ProcessTable` on start, unregistered on exit

### Runtime (`rt/src/tasks/`)
- Clean re-exports of tokio types: `spawn`, `sleep`, `timeout`, `mpsc`, `oneshot`
- Already has parallel `threads/` implementation using `std::thread`
- Comments mention deterministic runtime as planned feature

### ProcessTable (`concurrency/src/process_table.rs`)
- Global singleton with `RwLock<ProcessTableInner>`
- Tracks: processes (HashMap), links (bidirectional), monitors (unidirectional)
- `notify_exit()` handles exit propagation to linked/monitoring processes

### Pid (`concurrency/src/pid.rs`)
- Simple `u64` with atomic counter generation
- No generation tracking (stale reference detection not implemented)
- Display format: `<0.{id}>` mimicking Erlang

---

## v0.5 Features

### Phase 1: Structured Concurrency (Task/TaskGroup)

#### Goal
Provide Task and TaskGroup abstractions that complement GenServer for short-lived concurrent work.

#### Why First?
- GenServer is for long-lived stateful actors
- Many use cases need short-lived parallel work (parallel fetches, batch processing)
- Currently requires raw `tokio::spawn` or `JoinSet`, breaking the abstraction

#### Design Decision: Full ProcessTable Registration

**Tasks ARE registered in ProcessTable for full observability.**

Even short-lived tasks should be visible - Erlang tracks ALL processes, not just long-lived ones. This enables:
- Complete system visibility via inspection API
- Starvation detection knows about all running work
- Metrics can track task counts, lifetimes, etc.
- Debugging can see what's actually running

#### Task Primitive

```rust
// concurrency/src/task.rs
use crate::process_table;
use crate::pid::Pid;
use rt::{spawn, spawn_blocking, JoinHandle, CancellationToken};

pub struct Task<T> {
    pid: Pid,
    handle: JoinHandle<T>,
    token: CancellationToken,
}

impl<T: Send + 'static> Task<T> {
    pub fn spawn<F>(future: F) -> Self
    where F: Future<Output = T> + Send + 'static {
        let pid = Pid::new();
        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Register in ProcessTable for visibility
        let system_sender = Arc::new(TaskSystemSender {
            cancellation_token: token.clone(),
        });
        process_table::register(pid, system_sender);

        let pid_clone = pid;
        let handle = spawn(async move {
            let result = tokio::select! {
                result = future => Some(result),
                _ = token_clone.cancelled() => None,
            };

            // Unregister on completion
            process_table::unregister(pid_clone, ExitReason::Normal);

            result.expect("Task cancelled")
        });

        Self { pid, handle, token }
    }

    pub fn pid(&self) -> Pid { self.pid }

    pub async fn join(self) -> Result<T, JoinError> {
        self.handle.await
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn detach(self) {
        // Task continues running but we don't wait
        // Still registered in ProcessTable until completion
        std::mem::forget(self.handle);
    }
}

/// Minimal SystemMessageSender for Tasks (no message handling)
struct TaskSystemSender {
    cancellation_token: CancellationToken,
}

impl SystemMessageSender for TaskSystemSender {
    fn send_down(&self, _: Pid, _: MonitorRef, _: ExitReason) {
        // Tasks don't handle system messages
    }
    fn send_exit(&self, _: Pid, _: ExitReason) {
        // Tasks don't handle exit signals - just cancel
        self.cancellation_token.cancel();
    }
    fn kill(&self, _: ExitReason) {
        self.cancellation_token.cancel();
    }
    fn is_alive(&self) -> bool {
        !self.cancellation_token.is_cancelled()
    }
}
```

#### TaskGroup (JoinSet Replacement)

```rust
// concurrency/src/task_group.rs
pub struct TaskGroup<T> {
    pid: Pid,  // The group itself has a Pid
    tasks: Vec<Task<T>>,
    token: CancellationToken,
}

impl<T: Send + 'static> TaskGroup<T> {
    pub fn new() -> Self {
        let pid = Pid::new();
        let token = CancellationToken::new();

        // Register group in ProcessTable
        let system_sender = Arc::new(TaskGroupSystemSender {
            cancellation_token: token.clone(),
        });
        process_table::register(pid, system_sender);

        Self { pid, tasks: Vec::new(), token }
    }

    pub fn pid(&self) -> Pid { self.pid }

    /// Spawn a task in this group. Task is registered in ProcessTable.
    pub fn spawn<F>(&mut self, future: F)
    where F: Future<Output = T> + Send + 'static {
        let task = Task::spawn_with_parent_token(future, self.token.clone());
        self.tasks.push(task);
    }

    /// All task Pids in this group
    pub fn task_pids(&self) -> Vec<Pid> {
        self.tasks.iter().map(|t| t.pid()).collect()
    }

    pub async fn join_all(mut self) -> Vec<Result<T, JoinError>> {
        let mut results = Vec::new();
        for task in self.tasks.drain(..) {
            results.push(task.join().await);
        }
        process_table::unregister(self.pid, ExitReason::Normal);
        results
    }

    pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
        if self.tasks.is_empty() { return None; }
        let task = self.tasks.remove(0);
        Some(task.join().await)
    }

    /// First success wins, cancel rest
    pub async fn race_ok(self) -> Result<T, Vec<JoinError>>;

    pub fn cancel_all(&self) { self.token.cancel(); }
    pub fn len(&self) -> usize { self.tasks.len() }
    pub fn is_empty(&self) -> bool { self.tasks.is_empty() }
}

impl<T> Drop for TaskGroup<T> {
    fn drop(&mut self) {
        self.token.cancel();
        process_table::unregister(self.pid, ExitReason::Normal);
    }
}
```

#### Scoped Tasks (Optional, Lower Priority)

```rust
// Ensures tasks don't outlive scope - enables borrowing
pub async fn scope<'a, F, R>(f: F) -> R
where
    F: FnOnce(&Scope<'a>) -> BoxFuture<'a, R>;
```

#### Files to Create/Modify
- `concurrency/src/task.rs` (new)
- `concurrency/src/task_group.rs` (new)
- `concurrency/src/lib.rs` (export new modules)
- `concurrency/src/tasks/mod.rs` (integrate with tasks runtime)

#### Tests
- Task spawn and join
- Task cancellation
- TaskGroup join_all
- TaskGroup join_next ordering
- TaskGroup race_ok cancellation behavior
- Integration with GenServer (spawn tasks from within actor)

---

### Phase 2: Deterministic Test Runtime (`spawned-test` crate)

#### Goal
Enable fully deterministic async testing with controlled time advancement.

#### Why Critical?
- This is a **killer feature** - nothing else in Rust does this well
- Makes testing async code reliable and fast
- Enables property-based testing of concurrent systems
- Differentiates spawned from "just another actor library"

#### Crate Structure
**New crate: `spawned-test`** (separate from spawned-rt)
- Clean separation of concerns
- Optional dev-dependency
- Similar to `tokio-test` pattern

#### TestRuntime

```rust
// spawned-test/src/lib.rs

pub struct TestRuntime {
    /// Virtual time
    current_time: Instant,
    /// Pending timers
    timers: BinaryHeap<Timer>,
    /// Random seed for reproducibility
    seed: u64,
    /// Task scheduler (deterministic)
    scheduler: DeterministicScheduler,
}

impl TestRuntime {
    /// Create with seed for reproducibility
    pub fn new_with_seed(seed: u64) -> Self;

    /// Run async code deterministically
    pub fn run<F, Fut>(&mut self, f: F) -> Fut::Output
    where
        F: FnOnce() -> Fut,
        Fut: Future;

    /// Advance virtual time instantly
    pub fn advance(&mut self, duration: Duration);

    /// Advance to next scheduled event
    pub fn advance_to_next(&mut self);

    /// Run until all tasks blocked or complete
    pub fn run_until_idle(&mut self);

    /// Inject process failure
    pub fn inject_failure(&mut self, pid: Pid, reason: ExitReason);

    /// Get current virtual time
    pub fn now(&self) -> Instant;
}
```

#### Full Runtime Replacement (Decision Made)

TestRuntime will **fully replace tokio** - spawn, time, channels all deterministic.

```rust
// spawned-test/src/runtime.rs

pub trait Runtime: Send + Sync + 'static {
    type JoinHandle<T: Send>: Future<Output = Result<T, JoinError>> + Send;
    type Sender<T: Send>: Clone + Send + Sync;
    type Receiver<T: Send>: Send;
    type Sleep: Future<Output = ()> + Send;

    fn spawn<F, T>(future: F) -> Self::JoinHandle<T>
    where F: Future<Output = T> + Send + 'static, T: Send + 'static;

    fn sleep(duration: Duration) -> Self::Sleep;
    fn now() -> Instant;
    fn channel<T: Send>(buffer: usize) -> (Self::Sender<T>, Self::Receiver<T>);
}

// Production runtime (wraps tokio)
pub struct TokioRuntime;

impl Runtime for TokioRuntime {
    type JoinHandle<T: Send> = tokio::task::JoinHandle<T>;
    type Sender<T: Send> = tokio::sync::mpsc::Sender<T>;
    type Receiver<T: Send> = tokio::sync::mpsc::Receiver<T>;
    type Sleep = tokio::time::Sleep;

    fn spawn<F, T>(future: F) -> Self::JoinHandle<T> { tokio::spawn(future) }
    fn sleep(duration: Duration) -> Self::Sleep { tokio::time::sleep(duration) }
    fn now() -> Instant { Instant::now() }
    // ...
}

// Test runtime (fully deterministic)
pub struct DeterministicRuntime {
    scheduler: DeterministicScheduler,
    time: VirtualTime,
    rng: StdRng,  // Seeded for reproducibility
}
```

#### Implementation Approach
1. **Runtime trait**: Define trait in spawned-rt that both TokioRuntime and DeterministicRuntime implement
2. **Compile-time switching**: Use generics with default type parameter (not feature flags - allows both in same binary)
3. **Scheduler**: Custom scheduler that executes tasks in deterministic order based on seed
4. **Time**: Virtual time that only advances when explicitly told
5. **Channels**: Deterministic MPSC/oneshot that integrate with scheduler

#### GenServer Generic Over Runtime

```rust
// Option A: Default type parameter (backwards compatible) - RECOMMENDED
pub struct GenServerHandle<G: GenServer, R: Runtime = TokioRuntime> { ... }

// Tests can use:
GenServerHandle<MyServer, TestRuntime>

// Production uses (unchanged):
GenServerHandle<MyServer>
```

#### Files to Create/Modify
- Create `rt/src/runtime.rs` (Runtime trait)
- Create `spawned-test/` crate with Cargo.toml
- Create `spawned-test/src/lib.rs`, `scheduler.rs`, `time.rs`, `channel.rs`
- Modify `concurrency/src/tasks/gen_server.rs` to be generic over Runtime

#### Tests
- Deterministic execution with same seed
- Time advancement without real delays
- Failure injection
- Supervisor restart behavior under test
- Timer firing at correct virtual times

---

### Phase 3: Observability

#### Goal
Built-in metrics, tracing, and introspection for production systems.

#### Backend Decision
**Use `metrics` crate** - standard Rust metrics facade with pluggable backends (Prometheus, StatsD, etc.)

#### Inspection API

```rust
// concurrency/src/inspect.rs

pub mod inspect {
    /// List all processes
    pub fn processes() -> Vec<ProcessSummary>;

    /// Get detailed info about a process
    pub fn process_info(pid: Pid) -> Option<ProcessInfo>;

    /// Get supervision tree structure
    pub fn supervision_tree() -> SupervisionTree;

    /// Get system-wide metrics
    pub fn system_metrics() -> SystemMetrics;

    /// Get link graph
    pub fn link_graph() -> HashMap<Pid, Vec<Pid>>;
}

pub struct ProcessInfo {
    pub pid: Pid,
    pub name: Option<String>,
    pub status: ProcessStatus,
    pub mailbox_len: usize,
    pub links: Vec<Pid>,
    pub monitors: Vec<MonitorRef>,
    pub uptime: Duration,
    pub supervisor: Option<Pid>,
    pub trap_exit: bool,
    pub is_alive: bool,
}
```

#### Process Metrics (Automatic)

```rust
use metrics::{counter, gauge, histogram};

// Auto-instrumentation in GenServer receive() function:

// Before processing:
gauge!("spawned.mailbox.size", rx.len() as f64, "pid" => pid.to_string());

// After processing:
histogram!("spawned.message.latency_ms", elapsed.as_millis() as f64, "pid" => pid.to_string());
counter!("spawned.messages.processed", 1, "pid" => pid.to_string(), "type" => msg_type);
```

#### Automatic Tracing Integration

```rust
// In handle_call:
let span = tracing::info_span!(
    "genserver.call",
    pid = %self.pid,
    msg_type = std::any::type_name::<G::CallMsg>()
);
let _guard = span.enter();
```

#### Files to Create/Modify
- `concurrency/src/inspect.rs` (new)
- `concurrency/src/metrics.rs` (new)
- `concurrency/src/tasks/gen_server.rs` (add instrumentation)
- `concurrency/src/process_table.rs` (track additional metadata)
- Add `metrics` dependency to Cargo.toml

---

### Phase 4: Routers & Pools

#### Goal
First-class support for work distribution patterns.

#### Router Strategies

```rust
pub enum RouterStrategy {
    RoundRobin,
    Random,
    SmallestMailbox,
    ConsistentHash { hash_fn: fn(&[u8]) -> u64 },
    Broadcast,
}
```

#### Router Implementation

```rust
// concurrency/src/router.rs
pub struct Router<G: GenServer> {
    handles: Vec<GenServerHandle<G>>,
    strategy: RouterStrategy,
    next_index: AtomicUsize,  // For RoundRobin
}

impl<G: GenServer> Router<G> {
    pub fn new(workers: Vec<G>, strategy: RouterStrategy) -> Self;

    fn select(&self, msg: Option<&[u8]>) -> &GenServerHandle<G> {
        match &self.strategy {
            RouterStrategy::RoundRobin => {
                let idx = self.next_index.fetch_add(1, Ordering::Relaxed);
                &self.handles[idx % self.handles.len()]
            }
            RouterStrategy::Random => {
                let idx = rand::thread_rng().gen_range(0..self.handles.len());
                &self.handles[idx]
            }
            RouterStrategy::SmallestMailbox => {
                self.handles.iter()
                    .min_by_key(|h| h.mailbox_len())
                    .unwrap()
            }
            RouterStrategy::ConsistentHash { hash_fn } => {
                let hash = hash_fn(msg.unwrap_or(&[]));
                &self.handles[(hash as usize) % self.handles.len()]
            }
        }
    }

    /// Route a call
    pub async fn call(&self, msg: G::CallMsg) -> G::OutMsg;

    /// Route a cast
    pub fn cast(&self, msg: G::CastMsg);

    /// Add worker dynamically
    pub fn add_worker(&mut self, worker: G);

    /// Remove worker
    pub fn remove_worker(&mut self, pid: Pid);
}
```

**Note**: `SmallestMailbox` needs `mailbox_len()` exposed on GenServerHandle via `Arc<AtomicUsize>` counter.

#### Pool with Auto-Scaling

```rust
// concurrency/src/pool.rs
pub struct Pool<G: GenServer> {
    supervisor: GenServerHandle<DynamicSupervisor>,
    router: Router<G>,
    config: PoolConfig,
    scaling_task: JoinHandle<()>,
}

pub struct PoolConfig {
    pub min_workers: usize,
    pub max_workers: usize,
    pub scale_up_threshold: usize,  // mailbox size
    pub scale_down_threshold: usize,
    pub cooldown: Duration,
}

impl<G: GenServer + Clone> Pool<G> {
    pub async fn new(factory: impl Fn() -> G, config: PoolConfig) -> Self {
        let supervisor = DynamicSupervisor::start(DynamicSupervisorSpec::default()).await;

        // Start min_workers
        let mut handles = Vec::new();
        for _ in 0..config.min_workers {
            let child = supervisor.start_child(factory()).await;
            handles.push(child);
        }

        let router = Router::new(handles, RouterStrategy::SmallestMailbox);

        // Spawn scaling task that monitors mailbox sizes
        let scaling_task = spawn(async move { ... });

        Self { supervisor, router, config, scaling_task }
    }
}
```

#### Files to Create/Modify
- `concurrency/src/router.rs` (new)
- `concurrency/src/pool.rs` (new)
- Modify `GenServerHandle` to expose `mailbox_len()`

---

### Phase 5: Production Hardening

#### Pid Generation Tracking

Add generation counter to detect stale references:

```rust
// concurrency/src/pid.rs
pub struct Pid {
    id: u64,
    generation: u32,  // Incremented when process terminates
}

// In process_table.rs:
struct ProcessTableInner {
    // ...
    generations: HashMap<u64, u32>,  // id -> current generation
}
```

**Alternative** (simpler): Don't track generations, but provide `is_alive(pid)` function that checks ProcessTable. Current `SystemMessageSender::is_alive()` already does this.

#### Graceful Shutdown Improvements

```rust
// In supervisor.rs shutdown_child:
async fn shutdown_child(&self, child: &ChildInfo, timeout: Duration) {
    // Send shutdown signal
    child.handle.cast(ShutdownSignal);

    // Wait for graceful exit with timeout
    match tokio::time::timeout(timeout, child.handle.wait()).await {
        Ok(_) => { /* Clean exit */ }
        Err(_) => {
            // Force kill after timeout
            child.handle.stop();
        }
    }
}
```

Features:
- Async wait for child termination in supervisors
- Shutdown ordering (supervisors last)
- Drain period for in-flight requests

#### Health Checks

```rust
pub trait GenServer: Send + Sized {
    // ... existing ...

    fn health_check(&self) -> Health {
        Health::Healthy  // Default implementation
    }
}

pub enum Health {
    Healthy,
    Degraded(String),
    Unhealthy(String),
}

// In GenServerHandle:
pub async fn health(&self) -> Health {
    // Implemented as internal call type to access actor state
}
```

**Implementation**: Add internal `__health_check` call type that calls `health_check()` inside the actor.

---

## v1.0 Features

### Phase 6: Persistence - Event Sourcing

#### Goal
Enable durable actors that persist state across restarts via event sourcing.

#### Why Event Sourcing?
- Complete audit trail of all state changes
- Can rebuild state at any point in time
- Natural fit for actors (messages -> events)
- Enables temporal queries and debugging

#### PersistentGenServer Trait

```rust
// spawned-persist/src/lib.rs

pub trait PersistentGenServer: GenServer {
    /// Event type - what gets persisted
    type Event: Serialize + DeserializeOwned + Send;

    /// Snapshot type - for faster recovery
    type Snapshot: Serialize + DeserializeOwned + Send;

    /// Unique ID for this actor's event stream
    fn persistence_id(&self) -> &str;

    /// Apply event to state (used during replay and normal operation)
    fn apply_event(&mut self, event: &Self::Event);

    /// Create snapshot of current state
    fn snapshot(&self) -> Self::Snapshot;

    /// Restore from snapshot
    fn restore(&mut self, snapshot: Self::Snapshot);

    /// Persist event before processing
    async fn persist<F>(
        &mut self,
        event: Self::Event,
        then: F,
        ctx: &Context<Self>,
    ) -> Reply<Self>
    where
        F: FnOnce(&mut Self) -> Reply<Self>;

    /// Persist multiple events atomically
    async fn persist_all<F>(
        &mut self,
        events: Vec<Self::Event>,
        then: F,
        ctx: &Context<Self>,
    ) -> Reply<Self>
    where
        F: FnOnce(&mut Self) -> Reply<Self>;
}
```

#### Event Storage Abstraction

```rust
pub trait EventStore: Send + Sync {
    async fn append(
        &self,
        persistence_id: &str,
        events: Vec<StoredEvent>,
        expected_version: Option<u64>,
    ) -> Result<u64, StoreError>;

    async fn read(
        &self,
        persistence_id: &str,
        from_version: u64,
    ) -> Result<Vec<StoredEvent>, StoreError>;

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        snapshot: &[u8],
        at_version: u64,
    ) -> Result<(), StoreError>;

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(Vec<u8>, u64)>, StoreError>;
}

pub struct StoredEvent {
    pub sequence_nr: u64,
    pub timestamp: DateTime<Utc>,
    pub payload: Vec<u8>,
    pub metadata: HashMap<String, String>,
}
```

#### Built-in Storage Backends

```rust
pub struct InMemoryEventStore;      // For testing
pub struct FileEventStore;          // Simple persistence
pub struct PostgresEventStore;      // Production
pub struct SqliteEventStore;        // Embedded
```

#### Recovery Flow

```
Actor starts
     |
     v
+-------------------------+
| Load latest snapshot    |
| (if any)                |
+-----------+-------------+
            |
            v
+-------------------------+
| Replay events since     |
| snapshot                |
+-----------+-------------+
            |
            v
+-------------------------+
| Actor ready to process  |
| new messages            |
+-------------------------+
```

#### Files to Create
- New crate: `spawned-persist/`
- `spawned-persist/src/lib.rs` - PersistentGenServer trait
- `spawned-persist/src/store.rs` - EventStore trait
- `spawned-persist/src/stores/memory.rs`
- `spawned-persist/src/stores/file.rs`
- `spawned-persist/src/stores/postgres.rs`
- `spawned-persist/src/stores/sqlite.rs`

---

### Phase 7: Distribution - Clustering

#### Goal
Enable actors to communicate across nodes transparently.

#### Design Principles
- **Location transparency**: Same API for local and remote actors
- **Failure detection**: Know when nodes/actors become unavailable
- **Partition tolerance**: System continues during network splits

#### Cluster Formation

```rust
// spawned-cluster/src/lib.rs

pub struct ClusterConfig {
    pub node_name: String,
    pub discovery: DiscoveryMethod,
    pub transport: TransportConfig,
    pub failure_detector: FailureDetectorConfig,
}

pub enum DiscoveryMethod {
    Static(Vec<NodeAddress>),
    Dns { hostname: String, port: u16 },
    Kubernetes { namespace: String, label_selector: String },
    Gossip { seed_nodes: Vec<NodeAddress>, gossip_interval: Duration },
}
```

#### Node Identity

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId {
    id: u64,
    generation: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RemotePid {
    node: NodeId,
    pid: Pid,
}
```

#### Cluster Sharding

```rust
pub struct ClusterSharding<G: GenServer> {
    config: ShardingConfig<G>,
}

pub struct ShardingConfig<G: GenServer> {
    pub number_of_shards: u16,
    pub key_extractor: fn(&G::CallMsg) -> String,
    pub factory: fn(shard_id: u16) -> G,
}

impl<G: GenServer> ClusterSharding<G> {
    pub async fn entity(&self, entity_id: &str) -> EntityHandle<G>;
    pub async fn tell(&self, entity_id: &str, msg: G::CastMsg);
    pub async fn ask(&self, entity_id: &str, msg: G::CallMsg) -> G::OutMsg;
}
```

#### Cluster Events

```rust
pub enum ClusterEvent {
    NodeUp { node: NodeId, address: SocketAddr },
    NodeDown { node: NodeId },
    NodeUnreachable { node: NodeId },
    NodeReachable { node: NodeId },
    MembershipChanged { members: Vec<NodeId> },
}
```

#### Files to Create
- New crate: `spawned-cluster/`
- `spawned-cluster/src/lib.rs` - Cluster API
- `spawned-cluster/src/node.rs` - NodeId, RemotePid
- `spawned-cluster/src/discovery/` - Discovery implementations
- `spawned-cluster/src/transport.rs` - Network transport
- `spawned-cluster/src/sharding.rs` - Cluster sharding
- `spawned-cluster/src/failure_detector.rs` - Phi accrual detector

---

### Phase 8: I/O Integration

#### Goal
Actor-friendly I/O that integrates naturally with the actor model.

#### TCP Integration

```rust
// spawned-io/src/tcp.rs

pub struct TcpListener {
    inner: tokio::net::TcpListener,
}

impl TcpListener {
    pub async fn bind(addr: impl ToSocketAddrs) -> io::Result<Self>;

    pub async fn serve<G, F>(
        &self,
        supervisor: Option<Pid>,
        factory: F,
    ) where
        G: GenServer,
        F: Fn(TcpStream, SocketAddr) -> G;

    pub fn incoming(&self) -> impl Stream<Item = io::Result<(TcpStream, SocketAddr)>>;
}

pub struct TcpStream {
    inner: tokio::net::TcpStream,
}

impl TcpStream {
    pub fn attach<G: GenServer, D: Decoder>(
        self,
        handle: &GenServerHandle<G>,
        decoder: D,
        mapper: impl Fn(D::Item) -> G::CastMsg,
    );

    pub async fn write(&self, data: &[u8]) -> io::Result<usize>;
    pub async fn close(self) -> io::Result<()>;
}
```

#### Framing/Codecs

```rust
pub trait Decoder: Send {
    type Item: Send;
    type Error: std::error::Error;
    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error>;
}

pub trait Encoder<Item>: Send {
    type Error: std::error::Error;
    fn encode(&mut self, item: Item, dst: &mut BytesMut) -> Result<(), Self::Error>;
}

// Built-in codecs
pub struct LinesCodec;
pub struct LengthDelimitedCodec;
pub struct JsonCodec<T>;
```

---

## v1.1+ Features

### Phase 9: Virtual Actors - Orleans-Style Convenience

#### Goal
Provide Orleans/Akka-style "virtual actors" as an **opt-in layer** on top of GenServer.

#### What Virtual Actors Add

| Feature | Regular GenServer | Virtual Actor |
|---------|------------------|---------------|
| Lifecycle | You spawn, you manage | Framework manages |
| Addressing | By Pid | By type + ID (e.g., `User/123`) |
| Activation | Explicit `start()` | On-demand (first message) |
| Deactivation | Explicit `stop()` | Automatic after idle timeout |
| Location | You track where it lives | Framework routes for you |
| Single instance | You enforce | Framework guarantees |

#### Entity Trait

```rust
pub trait Entity: GenServer {
    type Id: EntityId;

    fn activate(id: Self::Id) -> Self;
    async fn passivate(&mut self) { }
    fn idle_timeout() -> Duration { Duration::from_secs(300) }
}

pub trait EntityId: Clone + Eq + Hash + Send + Sync + Display + FromStr {
    fn to_string(&self) -> String;
}

// String and numeric IDs work out of the box
impl EntityId for String { ... }
impl EntityId for u64 { ... }
impl EntityId for Uuid { ... }
```

#### EntityManager

```rust
pub struct EntityManager<E: Entity> {
    active: HashMap<E::Id, ActiveEntity<E>>,
    passivation_scheduler: PassivationScheduler,
}

impl<E: Entity> EntityManager<E> {
    pub async fn get(&self, id: E::Id) -> EntityHandle<E>;
    pub async fn tell(&self, id: E::Id, msg: E::CastMsg);
    pub async fn ask(&self, id: E::Id, msg: E::CallMsg) -> E::OutMsg;
    pub async fn passivate(&self, id: E::Id);
    pub fn active_count(&self) -> usize;
}
```

#### The Erlang Difference

**Orleans**: Virtual actors are the primary model, you can't easily escape
**Spawned**: Virtual actors are **one option**. You can:
- Use plain GenServer when you want control
- Use EntityManager when you want convenience
- Mix both in the same app
- Inspect the underlying GenServer/Pid anytime

```rust
// You can always get the raw handle
let entity_handle = entity_manager.get("user-123").await;
let raw_pid = entity_handle.pid();  // It's just a GenServer!
let raw_handle = entity_handle.inner();  // Full GenServerHandle access
```

---

### Phase 10: Durable Reminders

#### Goal
Timers that survive process restarts. Required for reliable delayed operations.

```rust
pub trait Reminders: PersistentGenServer {
    type ReminderMsg: Serialize + DeserializeOwned + Send;

    async fn handle_reminder(
        &mut self,
        name: &str,
        msg: Self::ReminderMsg,
        ctx: &Context<Self>,
    ) -> Noreply<Self>;
}

impl<G: Reminders> Context<G> {
    pub async fn set_reminder(&self, name: &str, due_in: Duration, msg: G::ReminderMsg);
    pub async fn set_reminder_recurring(&self, name: &str, due_in: Duration, interval: Duration, msg: G::ReminderMsg);
    pub async fn cancel_reminder(&self, name: &str);
    pub async fn list_reminders(&self) -> Vec<ReminderInfo>;
}
```

---

### Phase 11: Streams / Pub-Sub

#### Goal
Built-in pub-sub with backpressure, similar to Akka Streams or Orleans Streams.

```rust
pub struct StreamRef<T> {
    name: String,
    _marker: PhantomData<T>,
}

impl<T: Message + Clone> StreamRef<T> {
    pub async fn publish(&self, msg: T);
    pub fn subscribe<G: GenServer>(
        &self,
        handle: &GenServerHandle<G>,
        mapper: impl Fn(T) -> G::CastMsg,
    ) -> Subscription;
}

pub struct PersistentStream<T> {
    name: String,
    store: Arc<dyn StreamStore>,
}

impl<T: Serialize + DeserializeOwned> PersistentStream<T> {
    pub fn subscribe_persistent<G: GenServer>(
        &self,
        consumer_id: &str,
        handle: &GenServerHandle<G>,
        mapper: impl Fn(T) -> G::CastMsg,
    ) -> PersistentSubscription;
}
```

---

### Phase 12: Reliable Delivery

#### Goal
At-least-once delivery guarantees for critical messages.

```rust
impl<G: GenServer> GenServerHandle<G> {
    pub async fn tell_reliable(
        &self,
        msg: G::CastMsg,
        config: DeliveryConfig,
    ) -> DeliveryId;
}

pub struct DeliveryConfig {
    pub retry_interval: Duration,
    pub max_retries: Option<u32>,
    pub timeout: Duration,
}

impl<G: GenServer> Context<G> {
    pub fn ack(&self, delivery_id: DeliveryId);
    pub fn nack(&self, delivery_id: DeliveryId);
}
```

---

## Scheduling & Fairness

This section outlines the design for adding cooperative scheduling controls, fairness mechanisms, and starvation detection to Spawned. The design draws inspiration from **Orleans** (Microsoft's virtual actor framework) and **Akka** (Lightbend's actor toolkit).

### Motivation

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

### Scheduling Configuration

```rust
pub struct SchedulingConfig {
    /// Maximum messages processed before yielding to other tasks.
    /// Lower = fairer but higher overhead. Default: 100
    pub throughput: u32,

    /// Maximum time for a single handler execution.
    pub handler_timeout: Option<Duration>,

    /// What to do when handler times out
    pub timeout_action: TimeoutAction,

    /// Reentrancy behavior for this GenServer
    pub reentrancy: Reentrancy,
}

pub enum TimeoutAction {
    Warn,    // Log warning, let handler continue
    Cancel,  // Cancel the handler future, skip message
    Stop,    // Stop the GenServer (supervisor will restart)
}
```

### Reentrancy Control (Orleans-style)

```rust
pub enum Reentrancy {
    /// Default: Process each message to completion before next.
    None,

    /// Allow interleaving at any await point within handlers.
    Full,

    /// Only allow reentrancy for callbacks in same call chain.
    /// Prevents A->B->A deadlocks without full interleaving.
    CallChain,
}
```

### Call Chain Tracking

```rust
#[derive(Clone, Debug)]
pub struct CallChain {
    chain: Vec<Pid>,
    chain_id: u64,
}

impl CallChain {
    pub fn contains(&self, pid: Pid) -> bool { self.chain.contains(&pid) }
    pub fn extend(&self, caller: Pid) -> Self { ... }
}
```

### Throughput-Based Fairness (Akka-style)

```rust
struct ThroughputCounter {
    processed: u32,
    limit: u32,
}

impl ThroughputCounter {
    fn tick(&mut self) -> bool {
        self.processed += 1;
        if self.processed >= self.limit {
            self.processed = 0;
            true  // Should yield
        } else {
            false
        }
    }
}
```

Tuning guidance:

| `throughput` | Behavior | Use Case |
|--------------|----------|----------|
| 1 | Yield after every message | Maximum fairness, latency-sensitive |
| 10-100 | Balanced | General purpose |
| 1000+ | Batch processing | High throughput, less fairness |

### Starvation Detection (Akka-style)

```rust
pub struct StarvationConfig {
    pub check_interval: Duration,      // Default: 1s
    pub warning_threshold: Duration,   // Default: 100ms
    pub critical_threshold: Duration,  // Default: 1s
    pub capture_stack_traces: bool,
}

pub struct StarvationDetector {
    config: StarvationConfig,
}

impl StarvationDetector {
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
                        tracing::error!(delay = ?delay, "CRITICAL: Executor starvation detected");
                    } else if delay > config.warning_threshold {
                        tracing::warn!(delay = ?delay, "Executor starvation detected");
                    }
                }
            }
        })
    }
}
```

### GenServer Trait Extensions

```rust
pub trait GenServer: Send + Sized {
    // ... existing ...

    fn scheduling_config(&self) -> SchedulingConfig {
        SchedulingConfig::default()
    }

    fn on_slow_handler(&mut self, handler: HandlerType, duration: Duration, message_type: &str) {
        tracing::warn!(handler = ?handler, duration = ?duration, message = message_type, "Handler exceeded timeout threshold");
    }

    fn on_starvation(&mut self, event: StarvationEvent) {
        tracing::warn!(event = ?event, "Executor starvation detected");
    }
}
```

### ReadOnly Handlers (Orleans-style)

```rust
pub enum CallResponse<G: GenServer> {
    Reply(G::OutMsg),
    ReplyReadOnly(G::OutMsg),  // Can run concurrently with other ReadOnly
    Unused,
    Stop(G::OutMsg),
}
```

### Yield Helper Utility

```rust
pub struct YieldBudget {
    count: u32,
    limit: u32,
}

impl YieldBudget {
    pub fn new(limit: u32) -> Self { Self { count: 0, limit } }

    pub async fn tick(&mut self) {
        self.count += 1;
        if self.count >= self.limit {
            self.count = 0;
            tokio::task::yield_now().await;
        }
    }
}

// Usage in handler:
async fn handle_call(&mut self, msg: ProcessAll, _: &Handle) -> CallResponse {
    let mut budget = YieldBudget::new(100);
    for item in self.items.iter() {
        budget.tick().await;
        process(item);
    }
    CallResponse::Reply(Done)
}
```

### What This Doesn't Solve

1. **True Preemption**: CPU-bound code between `.await` points cannot be interrupted
2. **Blocking FFI Calls**: C library calls block the thread entirely
3. **Guaranteed Latency**: Without true preemption, latency guarantees cannot be made
4. **Infinite Loops**: Will hang the GenServer (detection but not prevention)
5. **Memory Exhaustion**: Not addressed by scheduling controls

### Design Principles

1. **Backward Compatible**: Default configuration matches current behavior
2. **Opt-in Complexity**: Simple use cases stay simple
3. **Detection Over Prevention**: We can't prevent blocking, but we can detect and recover
4. **Let It Crash**: Integration with supervisors for automatic recovery
5. **Observable**: Metrics and callbacks for production monitoring

---

## Insights from Other Frameworks

### Gleam OTP

**Subject-based typed messaging**: Gleam uses a "Subject" type which is essentially a typed channel that only the owning process can receive from. This is similar to what spawned already does with `GenServerHandle<G>` where message types are tied to the GenServer trait.

**Actor vs GenServer split**: Gleam distinguishes between:
- `gleam_otp/actor` - Fully typed, Gleam-native actors
- `gleam_erlang/process` - Low-level Erlang compatibility

Spawned already follows this pattern - `GenServer` is the typed layer, while `Pid` and `ProcessTable` are the lower-level primitives.

### Elixir Patterns

**Task.Supervisor.async_nolink**: Elixir recommends this for spawning async work from within a GenServer without blocking. This validates the Task/TaskGroup design - actors should be able to spawn Tasks for concurrent work.

**Key anti-pattern to avoid**: Using GenServer purely for code organization when no runtime concurrency benefit exists. GenServers should model entities that need:
- Isolated state
- Message serialization
- Supervision

### Lessons Applied

1. **Full visibility**: Like Erlang, track ALL processes (including short-lived Tasks) in ProcessTable
2. **Typed channels**: GenServerHandle provides type-safe message passing like Gleam's Subject
3. **Task for async work**: Task/TaskGroup fills the gap for async work within actors (like Elixir's Task.Supervisor)
4. **Layers not magic**: Keep explicit lifecycle as default, virtual actors as opt-in layer

---

## Implementation Details

### Crate Structure for v0.5

```
spawned/
├── Cargo.toml (workspace)
├── concurrency/               # spawned-concurrency (existing)
│   └── src/
│       ├── task.rs           # NEW: Task primitive
│       ├── task_group.rs     # NEW: TaskGroup
│       ├── router.rs         # NEW: Routers
│       ├── pool.rs           # NEW: Pools
│       ├── inspect.rs        # NEW: Observability
│       └── ...
├── rt/                        # spawned-rt (existing)
│   └── src/
│       └── runtime.rs        # NEW: Runtime trait
├── test/                      # NEW: spawned-test crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs            # TestRuntime
│       ├── scheduler.rs      # Deterministic scheduler
│       └── time.rs           # Virtual time
└── examples/
```

### Implementation Order (Dependency Graph)

```
Phase 1: Task/TaskGroup
    | (no dependencies)

Phase 2: TestRuntime <---------------------+
    |                                      |
    +-- Runtime trait                      |
    +-- TokioRuntime impl                  |
    +-- spawned-test crate                 |
    +-- GenServer<R: Runtime>              |
                                           |
Phase 3: Observability                     |
    |                                      |
    +-- inspect.rs (uses ProcessTable)     |
    +-- metrics (in gen_server.rs)         |
    +-- tracing (in gen_server.rs)         |
                                           |
Phase 4: Routers/Pools                     |
    |                                      |
    +-- Router (uses GenServerHandle)      |
    +-- Pool (uses Router + Supervisor)    |
    +-- mailbox_len() exposure             |
                                           |
Phase 5: Production Hardening              |
    |                                      |
    +-- Pid generation (optional)          |
    +-- Graceful shutdown                  |
    +-- Health checks ---------------------+
        (can use TestRuntime to verify)
```

### Suggested PR Order

#### Phase 1 PRs (Structured Concurrency)
1. **PR #1**: Task primitive (`concurrency/src/task.rs`)
2. **PR #2**: TaskGroup (`concurrency/src/task_group.rs`)
3. **PR #3**: Integration examples

#### Phase 2 PRs (Test Runtime)
1. **PR #4**: New `spawned-test` crate skeleton
2. **PR #5**: Virtual time and deterministic scheduler
3. **PR #6**: TestRuntime core implementation
4. **PR #7**: Failure injection and advanced features
5. **PR #8**: Documentation and examples

#### Phase 3 PRs (Observability)
1. **PR #9**: Inspection API (`inspect.rs`)
2. **PR #10**: Process metrics integration
3. **PR #11**: Automatic tracing spans

#### Phase 4 PRs (Routers/Pools)
1. **PR #12**: Router strategies (`router.rs`)
2. **PR #13**: Pool with auto-scaling (`pool.rs`)

#### Phase 5 PRs (Production Hardening)
1. **PR #14**: Pid generation tracking
2. **PR #15**: Graceful shutdown improvements
3. **PR #16**: Health checks

### Critical Files to Modify

| File | Changes |
|------|---------|
| `concurrency/src/lib.rs` | Export new modules |
| `concurrency/src/pid.rs` | Add generation field |
| `concurrency/src/tasks/gen_server.rs` | Add health_check, tracing, metrics |
| `concurrency/src/supervisor.rs` | Async shutdown wait |
| `Cargo.toml` | Add spawned-test to workspace |

### New Files

| File | Purpose |
|------|---------|
| `concurrency/src/task.rs` | Task primitive |
| `concurrency/src/task_group.rs` | TaskGroup |
| `concurrency/src/router.rs` | Router strategies |
| `concurrency/src/pool.rs` | Pool with scaling |
| `concurrency/src/inspect.rs` | Observability API |
| `rt/src/runtime.rs` | Runtime trait |
| `test/src/lib.rs` | TestRuntime |
| `test/src/scheduler.rs` | Deterministic scheduler |
| `test/src/time.rs` | Virtual time |

---

## Complete Feature Matrix

### Spawned vs Orleans vs Akka vs Erlang

| Feature | Erlang/OTP | Akka | Orleans | Spawned (Planned) |
|---------|------------|------|---------|-------------------|
| **Core Model** |
| Lightweight processes | Yes | Yes | Yes | Yes |
| Message passing | Yes | Yes | Yes | Yes |
| Supervision trees | Yes | Yes | No | Yes |
| Location transparency | Yes | Yes | Yes | Yes (v1.0) |
| **Lifecycle** |
| Explicit lifecycle | Yes | Yes | No | Yes (default) |
| Virtual actors | No | No* | Yes | Yes (opt-in v1.0) |
| Automatic passivation | No | No | Yes | Yes (opt-in v1.0) |
| **Persistence** |
| Event sourcing | No | Yes | Yes | Yes (v1.0) |
| Durable reminders | No | No | Yes | Yes (v1.0) |
| Snapshots | No | Yes | Yes | Yes (v1.0) |
| **Distribution** |
| Clustering | Yes | Yes | Yes | Yes (v1.0) |
| Cluster sharding | No | Yes | Yes | Yes (v1.0) |
| Single activation | No | Yes | Yes | Yes (v1.0) |
| **Messaging** |
| At-most-once | Yes | Yes | Yes | Yes |
| At-least-once | No | Yes | Yes | Yes (v1.0) |
| Streams/pub-sub | No | Yes | Yes | Yes (v1.0) |
| **Testing** |
| Deterministic testing | No | No | No | Yes (v0.5) |
| Time control | No | Yes | No | Yes (v0.5) |
| **Observability** |
| Built-in metrics | No | Yes | Yes | Yes (v0.5) |
| Process inspection | Yes | No | No | Yes (v0.5) |

*Akka has "sharded entities" which are similar but require cluster

### Implementation Priority Matrix

#### v0.5 (Production Features)

| Phase | Feature | Effort | Impact |
|-------|---------|--------|--------|
| 1 | Task/TaskGroup | Medium | High |
| 2 | spawned-test (deterministic runtime) | High | Very High |
| 3 | Observability | Medium | High |
| 4 | Routers/Pools | Medium | Medium |
| 5 | Production Hardening | Medium | Medium |

#### v1.0 (Persistence + Distribution)

| Phase | Feature | Effort | Impact |
|-------|---------|--------|--------|
| 6 | Persistence (spawned-persist) | High | High |
| 7 | Distribution (spawned-cluster) | Very High | Very High |
| 8 | I/O Integration (spawned-io) | Medium | Medium |

#### v1.1+ (Orleans/Akka Feature Parity)

| Phase | Feature | Effort | Dependencies |
|-------|---------|--------|--------------|
| 9 | Virtual Actors | High | Phase 7 (cluster) |
| 10 | Durable Reminders | Medium | Phase 6 (persistence) |
| 11 | Streams / Pub-Sub | High | Phase 7 (distributed) |
| 12 | Reliable Delivery | Medium | Phase 6 (persistence) |

### Estimated Lines of Code

| Version | Features | New Lines | Cumulative |
|---------|----------|-----------|------------|
| v0.5 | Task, TestRuntime, Observability, Routers | ~5,000 | ~9,000 |
| v1.0 | Persistence, Distribution, I/O | ~12,000 | ~21,000 |
| v1.1 | Virtual Actors, Reminders, Streams, Reliable | ~6,000 | ~27,000 |

For reference: Akka is ~500k lines, Orleans is ~300k lines. Spawned would be ~30k for full feature parity - much smaller due to Rust's expressiveness and building on tokio.

---

## Decisions Made

1. **Tasks in ProcessTable**: All Tasks (even short-lived) are registered in ProcessTable for full observability
2. **Metrics backend**: Use `metrics` crate (standard Rust metrics facade with pluggable backends)
3. **TestRuntime scope**: Full runtime replacement - replace tokio entirely (spawn, time, channels all deterministic)
4. **spawned-test**: Separate crate (like tokio-test pattern)
5. **Runtime trait**: Use generics with default type parameter (not feature flags) to allow both runtimes in same binary

## Open Questions

1. **Router supervision**: Should routers automatically supervise their workers?
2. **API stability**: What level of API stability to promise for v0.5?

## Success Criteria for v0.5

- [ ] Task and TaskGroup fully implemented with tests
- [ ] spawned-test crate enables deterministic testing
- [ ] Inspection API provides visibility into running system
- [ ] Router supports at least 3 strategies (RoundRobin, Random, SmallestMailbox)
- [ ] Pool supports min/max sizing
- [ ] All existing tests pass
- [ ] Documentation updated
- [ ] At least 2 examples demonstrating new features

---

## References

### Actor Frameworks

- [Akka Dispatchers Documentation](https://doc.akka.io/docs/akka/current/typed/dispatchers.html)
- [Akka Starvation Detector](https://doc.akka.io/docs/akka-diagnostics/current/starvation-detector.html)
- [Orleans Request Scheduling](https://learn.microsoft.com/en-us/dotnet/orleans/grains/request-scheduling)
- [Orleans Reentrancy](https://dotnet.github.io/orleans/docs/grains/reentrancy.html)
- [Gleam OTP](https://hexdocs.pm/gleam_otp/)

### Runtime & Scheduling

- [Erlang Scheduling](https://www.erlang.org/doc/system/system_principles.html)
- [Go Preemption Design Doc](https://github.com/golang/proposal/blob/master/design/24543-non-cooperative-preemption.md)
- [Tokio Cooperative Scheduling](https://tokio.rs/blog/2020-04-preemption)
- [Lunatic (WASM-based preemption)](https://github.com/lunatic-solutions/lunatic)

### Further Reading

- [Actors with Tokio](https://ryhl.io/blog/actors-with-tokio/)
- [Joe Armstrong's Thesis](https://erlang.org/download/armstrong_thesis_2003.pdf) - "Making reliable distributed systems in the presence of software errors"
