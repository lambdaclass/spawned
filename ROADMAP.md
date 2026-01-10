# Spawned Roadmap: From Actor Framework to Complete Concurrency Platform

## Table of Contents

1. [Executive Summary](#executive-summary)
2. [Design Philosophy](#design-philosophy)
3. [Current Architecture](#current-architecture)
4. [v0.5 Features](#v05-features)
5. [v1.0 Features](#v10-features)
6. [v1.1+ Features](#v11-features)
7. [Structured Concurrency Tradeoffs](#structured-concurrency-tradeoffs)
8. [Scheduling & Fairness](#scheduling--fairness)
9. [Insights from Other Frameworks](#insights-from-other-frameworks)
10. [Prioritized Features from Research](#prioritized-features-from-research)
11. [Implementation Details](#implementation-details)
12. [Feature Matrix](#complete-feature-matrix)

---

## Executive Summary

### v0.5 Target Scope (Production Features)
1. **Structured concurrency** (Task, TaskGroup) - fills the gap for non-actor concurrent work
2. **Buffer strategies** - Fixed/Dropping/Sliding mailboxes for backpressure
3. **Joiner policies** - TaskGroup completion strategies from Loom
4. **Deterministic test runtime** (spawned-test crate) - killer feature for reliable async testing
5. **Observability** - production-ready systems need visibility
6. **Routers/Pools** - common patterns should be first-class

### Future Vision (v1.0+)
- **Persistence** - event sourcing for durable actors
- **Distribution** - clustering and location-transparent messaging
- **Virtual Actors** - Orleans/Akka-style automatic lifecycle (opt-in)
- **Streams** - pub/sub with backpressure

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
|  Virtual Actors (EntityManager, ClusterSingleton)           |  <- Orleans-like (OPT-IN)
+-------------------------------------------------------------+
|  Cluster Sharding + Distribution                            |  <- Distributed (OPT-IN)
+-------------------------------------------------------------+
|  Persistence (PersistentActor)                              |  <- Durable state (OPT-IN)
+-------------------------------------------------------------+
|  Actor + Supervisor + Registry                              |  <- Foundation (ALWAYS)
+-------------------------------------------------------------+
```

**You can always drop down a layer.** Virtual actors are Actors under the hood.

---

## Current Architecture

### Actor (`concurrency/src/tasks/actor.rs`)
- Uses `mpsc::channel` for user messages (Request/Message)
- Uses separate `mpsc::channel` for system messages (Down/Exit/Timeout)
- `select_biased!` prioritizes system messages
- `CancellationToken` for graceful shutdown
- Registered in global `ActorTable` on start, unregistered on exit

### ActorTable (`concurrency/src/actor_table.rs`)
- Global singleton with `RwLock<ActorTableInner>`
- Tracks: actors (HashMap), links (bidirectional), monitors (unidirectional)
- `notify_exit()` handles exit propagation

### Pid (`concurrency/src/pid.rs`)
- Simple `u64` with atomic counter
- Display format: `<0.{id}>` mimicking Erlang

---

## v0.5 Features

### Phase 1: Structured Concurrency (Task/TaskGroup)

**Design Decision: Full ActorTable Registration**

All Tasks are registered in ActorTable for full observability - Erlang tracks ALL processes.

```rust
// concurrency/src/task.rs
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

        // Register in ActorTable for visibility
        actor_table::register(pid, TaskSystemSender::new(token.clone()));

        let handle = spawn(async move {
            let result = select! {
                result = future => Some(result),
                _ = token.cancelled() => None,
            };
            actor_table::unregister(pid, ExitReason::Normal);
            result.expect("Task cancelled")
        });

        Self { pid, handle, token }
    }

    pub fn pid(&self) -> Pid { self.pid }
    pub async fn join(self) -> Result<T, JoinError> { self.handle.await }
    pub fn cancel(&self) { self.token.cancel(); }
    pub fn detach(self) { std::mem::forget(self.handle); }
}

/// Escape hatch for fire-and-forget (explicit, visible in code review)
pub fn spawn_detached<F, T>(future: F) -> Pid
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let task = Task::spawn(future);
    let pid = task.pid();
    task.detach();
    pid
}
```

```rust
// concurrency/src/task_group.rs
pub struct TaskGroup<T> {
    pid: Pid,
    tasks: Vec<Task<T>>,
    token: CancellationToken,
    joiner: Joiner,
}

/// Joiner policies (inspired by Java Project Loom)
pub enum Joiner {
    /// Wait for all tasks to complete, return all results
    AllCompleted,
    /// Cancel remaining tasks when first succeeds
    FirstSuccess,
    /// Cancel remaining tasks when first fails
    FirstFailure,
    /// Wait for N successful completions, then cancel rest
    Quorum(usize),
    /// Custom policy
    Custom(Box<dyn JoinerPolicy>),
}

pub trait JoinerPolicy: Send {
    fn should_short_circuit(&self, completed: usize, succeeded: usize, failed: usize, total: usize) -> bool;
}

impl<T: Send + 'static> TaskGroup<T> {
    pub fn new() -> Self;
    pub fn with_joiner(joiner: Joiner) -> Self;
    pub fn spawn<F>(&mut self, future: F);
    pub fn task_pids(&self) -> Vec<Pid>;
    pub async fn join(self) -> JoinResult<T>;  // Uses joiner policy
    pub async fn join_next(&mut self) -> Option<Result<T, JoinError>>;
    pub fn cancel_all(&self);
}

pub enum JoinResult<T> {
    AllCompleted(Vec<Result<T, JoinError>>),
    FirstSuccess(T),
    FirstFailure(JoinError),
    Quorum(Vec<T>),
}
```

#### Level-triggered Cancellation (from Trio)

```rust
// Cancellation is a persistent state, not an edge event
pub struct CancellationScope {
    token: CancellationToken,
    shielded: AtomicBool,
}

impl CancellationScope {
    /// Check if cancelled (level-triggered - always returns true once cancelled)
    pub fn is_cancelled(&self) -> bool;

    /// Shield scope from cancellation temporarily (for cleanup)
    pub fn shield<F, Fut>(&self, f: F) -> ShieldGuard<Fut>
    where F: FnOnce() -> Fut;

    /// Checkpoint - explicit cancellation check point
    pub async fn checkpoint(&self) -> Result<(), Cancelled>;
}

// Usage
async fn do_work(scope: &CancellationScope) -> Result<(), Error> {
    loop {
        scope.checkpoint().await?;  // Check for cancellation

        let data = fetch_data().await;

        // Shield cleanup from cancellation
        scope.shield(|| async {
            save_partial_progress(&data).await;
        }).await;
    }
}
```

### Phase 2: Deterministic Test Runtime (`spawned-test`)

**Full runtime replacement** - spawn, time, channels all deterministic.

```rust
// spawned-test/src/lib.rs
pub struct TestRuntime {
    current_time: Instant,
    timers: BinaryHeap<Timer>,
    seed: u64,
    scheduler: DeterministicScheduler,
}

impl TestRuntime {
    pub fn new_with_seed(seed: u64) -> Self;
    pub fn run<F, Fut>(&mut self, f: F) -> Fut::Output;
    pub fn advance(&mut self, duration: Duration);
    pub fn advance_to_next(&mut self);
    pub fn run_until_idle(&mut self);
    pub fn inject_failure(&mut self, pid: Pid, reason: ExitReason);
    pub fn now(&self) -> Instant;
}
```

```rust
// Runtime trait for abstraction
pub trait Runtime: Send + Sync + 'static {
    type JoinHandle<T: Send>: Future<Output = Result<T, JoinError>> + Send;
    type Sender<T: Send>: Clone + Send + Sync;
    type Receiver<T: Send>: Send;

    fn spawn<F, T>(future: F) -> Self::JoinHandle<T>;
    fn sleep(duration: Duration) -> impl Future<Output = ()>;
    fn now() -> Instant;
    fn channel<T: Send>(buffer: usize) -> (Self::Sender<T>, Self::Receiver<T>);
}

pub struct TokioRuntime;        // Production
pub struct DeterministicRuntime; // Testing
```

### Phase 3: Observability

```rust
// concurrency/src/inspect.rs
pub mod inspect {
    pub fn actors() -> Vec<ActorSummary>;
    pub fn actor_info(pid: Pid) -> Option<ActorInfo>;
    pub fn supervision_tree() -> SupervisionTree;
    pub fn link_graph() -> HashMap<Pid, Vec<Pid>>;
}

pub struct ActorInfo {
    pub pid: Pid,
    pub name: Option<String>,
    pub mailbox_len: usize,
    pub links: Vec<Pid>,
    pub monitors: Vec<MonitorRef>,
    pub uptime: Duration,
    pub is_alive: bool,
}
```

Auto-instrumentation with `metrics` crate:
```rust
gauge!("spawned.mailbox.size", mailbox_len, "pid" => pid);
histogram!("spawned.message.latency_ms", elapsed, "pid" => pid);
counter!("spawned.messages.processed", 1, "pid" => pid);
```

### Phase 4: Routers & Pools

```rust
pub enum RouterStrategy {
    RoundRobin,
    Random,
    SmallestMailbox,
    ConsistentHash { hash_fn: fn(&[u8]) -> u64 },
    Broadcast,
}

pub struct Router<A: Actor> {
    actors: Vec<ActorRef<A>>,
    strategy: RouterStrategy,
}

impl<A: Actor> Router<A> {
    pub async fn call(&self, req: A::Request) -> A::Reply;
    pub fn cast(&self, msg: A::Message);
    pub fn add(&mut self, actor: A);
    pub fn remove(&mut self, pid: Pid);
}

pub struct Pool<A: Actor> {
    supervisor: ActorRef<DynamicSupervisor>,
    router: Router<A>,
    config: PoolConfig,
}

pub struct PoolConfig {
    pub min_workers: usize,
    pub max_workers: usize,
    pub scale_up_threshold: usize,
    pub scale_down_threshold: usize,
    pub cooldown: Duration,
}
```

### Phase 4.5: Buffer Strategies (from core.async)

Mailbox buffer strategies prevent both deadlock (unbounded growth) and memory exhaustion:

```rust
// concurrency/src/buffer.rs
pub enum BufferStrategy {
    /// Block sender when buffer is full (backpressure)
    Fixed(usize),
    /// Drop newest messages when full (never blocks, lossy)
    Dropping(usize),
    /// Drop oldest messages when full (keep most recent)
    Sliding(usize),
    /// Unbounded (current default - can cause OOM)
    Unbounded,
}

impl BufferStrategy {
    pub fn fixed(capacity: usize) -> Self { Self::Fixed(capacity) }
    pub fn dropping(capacity: usize) -> Self { Self::Dropping(capacity) }
    pub fn sliding(capacity: usize) -> Self { Self::Sliding(capacity) }
}

// Actor configuration
pub struct ActorConfig {
    pub mailbox: BufferStrategy,  // Default: Fixed(1000)
    pub system_mailbox: BufferStrategy,  // Default: Fixed(100)
}

impl Default for ActorConfig {
    fn default() -> Self {
        Self {
            mailbox: BufferStrategy::Fixed(1000),
            system_mailbox: BufferStrategy::Fixed(100),
        }
    }
}

// Usage
let actor = MyActor::new()
    .with_config(ActorConfig {
        mailbox: BufferStrategy::dropping(500),  // Lossy but safe
        ..Default::default()
    })
    .start();

// Metrics for monitoring buffer behavior
gauge!("spawned.mailbox.capacity", capacity, "pid" => pid);
gauge!("spawned.mailbox.size", current_size, "pid" => pid);
counter!("spawned.mailbox.dropped", 1, "pid" => pid);  // For dropping/sliding
```

**When to use each:**
- `Fixed`: Most cases. Provides backpressure, prevents OOM.
- `Dropping`: High-throughput telemetry/metrics. OK to lose some messages.
- `Sliding`: Status updates where only latest matters. E.g., position tracking.
- `Unbounded`: Testing only. Never in production.

### Phase 5: Production Hardening

**Pid Generation Tracking:**
```rust
pub struct Pid {
    id: u64,
    generation: u32,  // Detect stale references
}
```

**Graceful Shutdown:**
```rust
async fn shutdown_child(&self, child: &ChildInfo, timeout: Duration) {
    child.actor_ref.cast(ShutdownSignal);
    match timeout(timeout, child.actor_ref.wait()).await {
        Ok(_) => { /* Clean */ }
        Err(_) => child.actor_ref.stop(),  // Force
    }
}
```

**Health Checks:**
```rust
pub trait Actor {
    fn health_check(&self) -> Health { Health::Healthy }
}

pub enum Health {
    Healthy,
    Degraded(String),
    Unhealthy(String),
}
```

---

## v1.0 Features

### Phase 6: Persistence - Event Sourcing

```rust
pub trait PersistentActor: Actor {
    type Event: Serialize + DeserializeOwned + Send;
    type Snapshot: Serialize + DeserializeOwned + Send;

    fn persistence_id(&self) -> &str;
    fn apply_event(&mut self, event: &Self::Event);
    fn snapshot(&self) -> Self::Snapshot;
    fn restore(&mut self, snapshot: Self::Snapshot);

    async fn persist(&mut self, event: Self::Event, ctx: &Context<Self>);
    async fn persist_all(&mut self, events: Vec<Self::Event>, ctx: &Context<Self>);
}

pub trait EventStore: Send + Sync {
    async fn append(&self, id: &str, events: Vec<StoredEvent>, version: Option<u64>) -> Result<u64>;
    async fn read(&self, id: &str, from: u64) -> Result<Vec<StoredEvent>>;
    async fn save_snapshot(&self, id: &str, snapshot: &[u8], version: u64) -> Result<()>;
    async fn load_snapshot(&self, id: &str) -> Result<Option<(Vec<u8>, u64)>>;
}

// Backends
pub struct InMemoryEventStore;   // Testing
pub struct PostgresEventStore;   // Production
pub struct SqliteEventStore;     // Embedded
```

### Phase 7: Distribution - Clustering

```rust
pub struct ClusterConfig {
    pub node_name: String,
    pub discovery: DiscoveryMethod,
    pub transport: TransportConfig,
}

pub enum DiscoveryMethod {
    Static(Vec<NodeAddress>),
    Dns { hostname: String, port: u16 },
    Kubernetes { namespace: String, label_selector: String },
    Gossip { seed_nodes: Vec<NodeAddress> },
}

pub struct RemotePid {
    node: NodeId,
    pid: Pid,
}

pub struct ClusterSharding<A: Actor> {
    config: ShardingConfig<A>,
}

impl<A: Actor> ClusterSharding<A> {
    pub async fn entity(&self, id: &str) -> EntityRef<A>;
    pub async fn tell(&self, id: &str, msg: A::Message);
    pub async fn ask(&self, id: &str, req: A::Request) -> A::Reply;
}
```

### Phase 8: I/O Integration

```rust
pub struct TcpListener { ... }

impl TcpListener {
    pub async fn bind(addr: impl ToSocketAddrs) -> io::Result<Self>;
    pub async fn serve<A, F>(&self, supervisor: Option<Pid>, factory: F)
    where A: Actor, F: Fn(TcpStream, SocketAddr) -> A;
}

pub struct TcpStream { ... }

impl TcpStream {
    pub fn attach<A: Actor, D: Decoder>(
        self,
        actor_ref: &ActorRef<A>,
        decoder: D,
        mapper: impl Fn(D::Item) -> A::Message,
    );
}

// Built-in codecs
pub struct LinesCodec;
pub struct LengthDelimitedCodec;
pub struct JsonCodec<T>;
```

---

## v1.1+ Features

### Phase 9: Virtual Actors (Orleans-style)

```rust
pub trait Entity: Actor {
    type Id: EntityId;
    fn activate(id: Self::Id) -> Self;
    async fn passivate(&mut self) { }
    fn idle_timeout() -> Duration { Duration::from_secs(300) }
}

pub struct EntityManager<E: Entity> {
    active: HashMap<E::Id, ActiveEntity<E>>,
}

impl<E: Entity> EntityManager<E> {
    pub async fn get(&self, id: E::Id) -> EntityRef<E>;
    pub async fn tell(&self, id: E::Id, msg: E::Message);
    pub async fn ask(&self, id: E::Id, req: E::Request) -> E::Reply;
    pub async fn passivate(&self, id: E::Id);
}

// You can always get the raw ref
let entity_ref = entity_manager.get("user-123").await;
let pid = entity_ref.pid();           // It's just an Actor!
let raw = entity_ref.inner();         // Full ActorRef access
```

### Phase 10: Durable Reminders

```rust
pub trait Reminders: PersistentActor {
    type ReminderMsg: Serialize + DeserializeOwned + Send;
    async fn handle_reminder(&mut self, name: &str, msg: Self::ReminderMsg, ctx: &Context<Self>);
}

impl<A: Reminders> Context<A> {
    pub async fn set_reminder(&self, name: &str, due_in: Duration, msg: A::ReminderMsg);
    pub async fn set_reminder_recurring(&self, name: &str, due_in: Duration, interval: Duration, msg: A::ReminderMsg);
    pub async fn cancel_reminder(&self, name: &str);
}
```

### Phase 11: Streams / Pub-Sub

```rust
pub struct StreamRef<T> { ... }

impl<T: Clone + Send> StreamRef<T> {
    pub async fn publish(&self, msg: T);
    pub fn subscribe<A: Actor>(&self, actor_ref: &ActorRef<A>, mapper: impl Fn(T) -> A::Message) -> Subscription;
}

pub struct PersistentStream<T> { ... }

impl<T: Serialize + DeserializeOwned> PersistentStream<T> {
    pub fn subscribe_persistent<A: Actor>(&self, consumer_id: &str, actor_ref: &ActorRef<A>, mapper: impl Fn(T) -> A::Message);
}
```

### Phase 12: Reliable Delivery

```rust
impl<A: Actor> ActorRef<A> {
    pub async fn cast_reliable(&self, msg: A::Message, config: DeliveryConfig) -> DeliveryId;
}

pub struct DeliveryConfig {
    pub retry_interval: Duration,
    pub max_retries: Option<u32>,
    pub timeout: Duration,
}

impl<A: Actor> Context<A> {
    pub fn ack(&self, delivery_id: DeliveryId);
    pub fn nack(&self, delivery_id: DeliveryId);
}
```

---

## Structured Concurrency Tradeoffs

### What You Lose: Fire-and-Forget Convenience

Structured concurrency removes implicit fire-and-forget. Some patterns become harder:

#### 1. Background Daemons

```rust
// TODAY: Easy - spawn and forget
fn init() {
    spawn(metrics_reporter());  // Runs forever in background
    spawn(health_checker());    // Parent doesn't wait
}

// STRUCTURED: Parent must wait or explicitly detach
async fn init() {
    let group = TaskGroup::new();
    group.spawn(metrics_reporter());
    group.spawn(health_checker());
    group.join_all().await;  // BLOCKS HERE FOREVER - not what you want!
}

// SOLUTION: Explicit detach (escape hatch)
fn init() {
    spawn_detached(metrics_reporter());  // Explicit "I know what I'm doing"
    spawn_detached(health_checker());
}

// OR: Move to top-level scope
async fn main() {
    let group = TaskGroup::new();
    group.spawn(metrics_reporter());  // Lives for entire program
    group.spawn(health_checker());
    group.spawn(actual_app());        // Main logic
    group.join_all().await;
}
```

#### 2. Request Handler Spawning Background Work

```rust
// TODAY: Handler returns fast, work continues
async fn handle_request(req: Request) -> Response {
    spawn(send_analytics(req.clone()));  // Fire and forget
    spawn(update_cache(req.clone()));    // Fire and forget
    Response::ok()  // Return immediately
}

// STRUCTURED: Can't outlive the handler
async fn handle_request(req: Request) -> Response {
    let group = TaskGroup::new();
    group.spawn(send_analytics(req.clone()));
    group.spawn(update_cache(req.clone()));
    group.join_all().await;  // WAITS - slower response!
    Response::ok()
}

// SOLUTION A: Background task pool (owned by server, not request)
async fn handle_request(req: Request, bg: &BackgroundScope) -> Response {
    bg.spawn(send_analytics(req.clone()));  // Outlives request
    Response::ok()
}

// SOLUTION B: Accept the latency (often fine)
// Analytics/cache are usually fast anyway
```

#### 3. Self-Perpetuating Tasks

```rust
// TODAY: Task spawns its replacement
async fn worker() {
    loop {
        let job = get_job().await;
        if job.is_big() {
            spawn(worker());  // Spawn helper, keep going
        }
        process(job).await;
    }
}

// STRUCTURED: Need group passed in
async fn worker(group: &TaskGroup) {
    loop {
        let job = get_job().await;
        if job.is_big() {
            group.spawn(worker(group));  // Must have access to scope
        }
        process(job).await;
    }
}
```

### The Tradeoff Summary

| Lost | Gained |
|------|--------|
| Fire-and-forget convenience | No orphaned tasks, ever |
| Implicit background work | Explicit lifetime ownership |
| Spawn anywhere | Clear task hierarchy |

### How Other Systems Handle This

| System | Approach |
|--------|----------|
| **Trio (Python)** | Strict. Must pass nurseries explicitly. Detach discouraged. |
| **Go** | Has `go` statement (fire-and-forget) but `errgroup` for structured. Mix both. |
| **Kotlin** | Coroutine scopes. `GlobalScope` is the escape hatch. |

### Recommendation for Spawned

```rust
// Structured by default (safe)
group.spawn(task);

// Explicit escape hatch (visible in code review)
spawn_detached(task);  // Requires explicit acknowledgment
```

This way:
- **95% of code** uses structured concurrency (safe)
- **5% that needs fire-and-forget** is visible and intentional

---

## Scheduling & Fairness

This section outlines the design for adding cooperative scheduling controls, fairness mechanisms, and starvation detection to Spawned. The design draws inspiration from **Orleans** (Microsoft's virtual actor framework) and **Akka** (Lightbend's actor toolkit).

### Motivation

#### The Problem

Erlang's BEAM VM provides true preemptive scheduling through "reduction counting" - each process gets a budget of ~2000-4000 operations before being forcibly yielded. This is possible because BEAM controls the entire execution environment.

Rust's async model is fundamentally **cooperative** - tasks only yield at `.await` points. Between await points, there's nothing the runtime can do to interrupt a misbehaving task. This means:

- A CPU-bound loop blocks the executor thread
- One slow Actor can starve others on the same thread pool
- No automatic fairness guarantees

#### What Other Frameworks Do

| Framework | Preemption | Fairness Mechanism |
|-----------|------------|-------------------|
| **Erlang/BEAM** | True (reductions) | Automatic |
| **Go** | Semi (signals) | Runtime-injected safe points |
| **Orleans (.NET)** | None | Await-point interleaving, reentrancy control, timeouts |
| **Akka (JVM)** | None | Throughput limits, dedicated dispatchers, starvation detection |

**Key insight**: No mainstream actor framework except Erlang has true preemption. They all use cooperative scheduling with various fairness and isolation mechanisms.

#### Our Approach

Accept cooperative scheduling as a constraint and provide maximum control within that model:

1. **Detection**: Know when things go wrong (starvation detection, slow handler callbacks)
2. **Fairness**: Throughput limits to prevent one Actor from monopolizing the executor
3. **Reentrancy**: Orleans-style control over message interleaving
4. **Timeouts**: Handler-level timeouts with configurable actions
5. **Let-it-crash**: Integration with supervisors for recovery

### Technical Background

This section explains *why* true preemption is difficult in Rust and what alternatives exist.

#### How Erlang Does It

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

#### Why Rust Can't Do This

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

#### How Go Does It (And Why We Can't Copy It)

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

#### What Orleans Does

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

#### What Akka Does

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

### Alternatives Considered

We evaluated several approaches before settling on the Orleans/Akka-inspired design:

#### 1. Signal-Based Preemption (Go-style)

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

#### 2. Panic-Based "Preemption"

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

#### 3. Thread Cancellation (Nuclear Option)

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

#### 4. WASM-Based True Preemption (Lunatic-style)

**Idea**: Compile Actor handlers to WebAssembly and run in Wasmtime with fuel metering.

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

#### 5. Cooperative Budget (Tokio's `coop`)

**Idea**: Track "work units" and yield when budget exhausted.

Tokio already has internal budget tracking in its `coop` module. Tasks that do too many operations get forced to yield at their next `.await`.

**Assessment:**

| Pro | Con |
|-----|-----|
| Already exists in Tokio | Still requires `.await` points |
| No unsafe code | Can't help CPU-bound code |
| Proven in production | Not exposed for user configuration |

**Verdict**: Good foundation, but doesn't solve the core problem. We build on this concept with throughput limits.

#### 6. Manual Yield Points

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

#### Summary: Why Orleans/Akka Approach

After evaluating alternatives, we chose the Orleans/Akka approach because:

1. **Proven at scale**: Both frameworks handle massive production workloads
2. **No unsafe code**: Pure Rust implementation, no signal handling
3. **Incremental adoption**: Users can opt-in to features as needed
4. **Composable with supervisors**: "Let it crash" philosophy for recovery
5. **Observable**: Starvation detection tells you when things go wrong
6. **Pragmatic**: Accepts cooperative scheduling as a constraint

The philosophy is: **Don't try to preempt; detect and recover.**

### Current State

Spawned already has some relevant infrastructure:

- **Three backends**: `Backend::Async`, `Backend::Blocking`, `Backend::Thread` provide isolation options
- **`WarnOnBlocking`**: Debug-mode wrapper that detects when polls take >10ms (detection only)
- **Supervisors**: `Supervisor` and `DynamicSupervisor` for crash recovery
- **Call timeouts**: `call_with_timeout()` for individual calls

What's missing:

- Per-Actor scheduling configuration
- Throughput-based fairness
- Reentrancy control
- Global starvation detection
- Handler-level timeouts (not just call timeouts)
- Metrics/observability hooks

### Proposed Design

#### 1. Scheduling Configuration

Per-Actor configuration for scheduling behavior:

```rust
/// Per-Actor scheduling configuration
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

    /// Reentrancy behavior for this Actor
    pub reentrancy: Reentrancy,
}

pub enum TimeoutAction {
    /// Log warning, let handler continue (detection only)
    Warn,
    /// Cancel the handler future, skip message, continue Actor
    Cancel,
    /// Stop the Actor (supervisor will restart)
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

#### 2. Reentrancy Control (Orleans-style)

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
Actor A calls Actor B
Actor B calls back to Actor A
Without reentrancy: DEADLOCK (A is waiting for B, B is waiting for A)
With CallChain reentrancy: B's callback is allowed through
```

#### 3. Call Chain Tracking

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
pub enum ActorInMsg<G: Actor> {
    Call {
        sender: oneshot::Sender<Result<G::Reply, ActorError>>,
        message: G::Request,
        call_chain: Option<CallChain>,  // For reentrancy
    },
    Cast {
        message: G::Message,
    },
}
```

#### 4. Throughput-Based Fairness (Akka-style)

Prevent one Actor from monopolizing the executor:

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

#### 5. Handler Timeouts

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

#### 6. Starvation Detection (Akka-style)

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

#### 7. Actor Trait Extensions

Add hooks for scheduling events:

```rust
pub trait Actor: Send + Sized {
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

    /// Called when starvation is detected affecting this Actor.
    /// Default implementation logs a warning.
    fn on_starvation(&mut self, event: StarvationEvent) {
        tracing::warn!(event = ?event, "Executor starvation detected");
    }
}
```

#### 8. ReadOnly Handlers (Orleans-style)

Allow concurrent execution of read-only operations:

```rust
pub enum CallResponse<G: Actor> {
    /// Normal reply, exclusive access assumed
    Reply(G::Reply),

    /// Reply from a read-only operation.
    /// Multiple ReadOnly responses can be processed concurrently.
    ReplyReadOnly(G::Reply),

    /// Message type not handled
    Unused,

    /// Reply and stop the Actor
    Stop(G::Reply),
}
```

When a handler returns `ReplyReadOnly`, the Actor knows the operation didn't modify state and can potentially process other ReadOnly messages concurrently (when `Reentrancy::Full` is enabled).

#### 9. Backend Integration

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
impl<G: Actor> G {
    fn start(self, backend: Backend) -> ActorRef<Self>;

    fn start_with_config(
        self,
        backend: Backend,
        config: SchedulingConfig,
    ) -> ActorRef<Self>;
}
```

#### 10. Metrics & Observability

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

#### 11. Yield Helper Utility

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

### Scheduling Implementation Phases

#### Phase 1: Foundation (Core Infrastructure)

**Priority: High**

1. **`SchedulingConfig` struct** and defaults
2. **`scheduling_config()` trait method** on Actor
3. **Throughput counter** and yield-after-N-messages logic
4. **Handler timeout wrapper** with `TimeoutAction` enum

**Deliverables:**
- Actors can be configured with throughput limits
- Handlers can have timeouts with Warn/Cancel/Stop actions
- Backward compatible - default config matches current behavior

#### Phase 2: Detection & Observability

**Priority: High**

1. **Enhanced `WarnOnBlocking`** - make thresholds configurable
2. **`on_slow_handler()` callback** - let users hook into slow handler events
3. **`StarvationDetector`** - global executor health monitoring
4. **`SchedulingMetrics` trait** - integration point for metrics backends

**Deliverables:**
- Users can detect scheduling problems in production
- Metrics can be exported to monitoring systems
- Starvation is detected and logged before it causes cascading failures

#### Phase 3: Reentrancy Control

**Priority: Medium**

1. **`Reentrancy` enum** - None, Full, CallChain
2. **`CallChain` struct** - tracking for deadlock prevention
3. **Modified message types** - carry call chain information
4. **Reentrancy logic in `receive()`** - queue/process decisions

**Deliverables:**
- Users can enable interleaving for high-throughput scenarios
- A→B→A deadlocks are prevented with CallChain reentrancy
- Clear mental model for each reentrancy mode

#### Phase 4: Advanced Features

**Priority: Low**

1. **`ReplyReadOnly` response** - concurrent read operations
2. **Per-handler timeout configuration** - different timeouts for call vs cast
3. **Mailbox size limits** - backpressure when mailbox grows too large
4. **Priority messages** - some messages skip the queue

**Deliverables:**
- Read-heavy workloads can scale better
- Fine-grained control over timeouts
- Backpressure prevents memory exhaustion

### Example Usage

#### Basic: Default Configuration (Current Behavior)

```rust
// No changes needed - defaults match current behavior
let handle = MyServer::new().start(Backend::Async);
```

#### Fair Scheduling

```rust
impl Actor for LatencySensitiveServer {
    fn scheduling_config(&self) -> SchedulingConfig {
        SchedulingConfig {
            throughput: 1,  // Yield after every message
            ..Default::default()
        }
    }
}
```

#### Handler Timeouts with Supervisor Recovery

```rust
impl Actor for UnreliableWorker {
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

#### Preventing Deadlocks

```rust
impl Actor for ServiceA {
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

#### Global Starvation Detection

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

#### Custom Metrics Integration

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

### What This Doesn't Solve

This section is an honest assessment of limitations. Understanding what we *can't* do is as important as knowing what we can.

#### True Preemption

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

#### Blocking FFI Calls

Calls to blocking C libraries or system calls cannot be interrupted by any mechanism:

```rust
async fn handle_call(&mut self, msg: Msg, _: &Handle) -> CallResponse {
    // This blocks the thread entirely - even Backend::Thread can't help mid-call
    let result = unsafe { some_c_library_blocking_call() };
    CallResponse::Reply(result)
}
```

**Mitigations:**
- Use `Backend::Thread` to isolate the Actor to its own OS thread
- Use `Backend::Blocking` to run on Tokio's blocking pool
- Wrap in `spawn_blocking` if only part of the handler blocks

#### Guaranteed Latency

Without true preemption, **latency guarantees cannot be made**. A misbehaving Actor can still cause latency spikes.

```
┌─────────────────────────────────────────────────────────────┐
│  What can happen even with all our features:                │
│                                                             │
│  Actor A: starts processing message                     │
│       │                                                     │
│       └─► Enters CPU-bound section (no awaits)              │
│             │                                               │
│             │   Meanwhile, Actor B is waiting...        │
│             │   Starvation detector fires warning           │
│             │   But A keeps running until it hits an await  │
│             │                                               │
│       ◄─────┘                                               │
│       │                                                     │
│       └─► Finally awaits, B gets to run                     │
└─────────────────────────────────────────────────────────────┘
```

This design provides **detection and recovery**, not **prevention**.

#### Infinite Loops

An infinite loop without await points will hang the Actor (and potentially the executor thread):

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
**Recovery**: `TimeoutAction::Stop` will eventually mark the Actor for termination, but the actual termination only happens if the code reaches an await point (which it won't in an infinite loop).

**For `Backend::Thread`**: The thread is effectively lost until the process exits.

#### Memory Exhaustion

This design doesn't address memory issues:
- A Actor that allocates unboundedly will OOM
- Mailbox size limits (Phase 4) can help with message backlog, but not internal state growth

#### Multi-Actor Deadlocks

While `Reentrancy::CallChain` prevents A→B→A deadlocks, it doesn't prevent:
- A→B→C→A deadlocks (would require full distributed deadlock detection)
- Resource deadlocks (two Actors waiting for the same external resource)

### Frequently Asked Questions

#### Why not just use Erlang/Elixir?

Erlang is excellent for the problems it solves. Use it if:
- Your problem fits Erlang's sweet spot (telecom, messaging, soft real-time)
- You can accept Erlang's performance characteristics
- Your team knows Erlang

Use Spawned if:
- You need Rust's performance, safety, or ecosystem
- You're already in a Rust codebase
- You want actor-model benefits without leaving Rust

#### Why not contribute preemption to Tokio?

Tokio has considered and rejected adding preemption. The fundamental issue is that async Rust is cooperative by design - changing this would require:
1. Language/compiler changes (yield points in loops)
2. Breaking changes to the Future trait
3. Runtime overhead that goes against Tokio's goals

The Tokio team's position is that blocking is a user bug that should be fixed in user code.

#### Should I use `Backend::Thread` for everything?

No. `Backend::Thread` has costs:
- ~2MB stack per thread
- OS thread limits
- Context switch overhead

Use `Backend::Thread` for:
- Actors that do long-running blocking work
- Actors that call blocking FFI
- Isolation of unreliable/untrusted code

Use `Backend::Async` for:
- The majority of Actors
- I/O-bound work
- Well-behaved async code

#### How does this compare to the `async-std` runtime?

Both Tokio and async-std are cooperative. Neither has preemption. The design in this roadmap would work with either runtime (with some adaptation).

#### Will this make my code "as good as" Erlang?

No. Erlang's preemption is a fundamental property of the BEAM VM that cannot be replicated in native code without significant trade-offs.

This design makes your code:
- **More observable**: You'll know when things go wrong
- **More recoverable**: Supervisors can restart stuck Actors
- **More fair**: Throughput limits improve scheduling fairness
- **Safer from deadlocks**: Reentrancy prevents common deadlock patterns

But it does not make your code preemptive.

### Scheduling Design Principles

1. **Backward Compatible**: Default configuration matches current behavior exactly
2. **Opt-in Complexity**: Simple use cases stay simple; advanced features are opt-in
3. **Detection Over Prevention**: We can't prevent blocking, but we can detect and recover
4. **Let It Crash**: Integration with supervisors for automatic recovery
5. **Observable**: Metrics and callbacks for production monitoring
6. **Configurable**: Different workloads need different tradeoffs

### Scheduling Conclusion

This design acknowledges a fundamental truth: **true preemption is not achievable in Rust without extraordinary measures** (WASM compilation, compiler changes, or unsafe signal handling). Rather than fighting this constraint, we embrace it and focus on what we *can* do:

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

### Scheduling References

#### Actor Frameworks

- [Akka Dispatchers Documentation](https://doc.akka.io/docs/akka/current/typed/dispatchers.html) - Akka's approach to fairness and thread pool management
- [Akka Starvation Detector](https://doc.akka.io/docs/akka-diagnostics/current/starvation-detector.html) - How Akka detects executor starvation
- [Orleans Request Scheduling](https://learn.microsoft.com/en-us/dotnet/orleans/grains/request-scheduling) - Orleans' turn-based execution model
- [Orleans Reentrancy](https://dotnet.github.io/orleans/docs/grains/reentrancy.html) - How Orleans handles message interleaving

#### Runtime & Scheduling

- [Erlang Scheduling](https://www.erlang.org/doc/system/system_principles.html) - How BEAM achieves true preemption
- [Go Preemption Design Doc](https://github.com/golang/proposal/blob/master/design/24543-non-cooperative-preemption.md) - Go's signal-based preemption
- [Tokio Cooperative Scheduling](https://tokio.rs/blog/2020-04-preemption) - Why Tokio chose cooperative scheduling
- [Lunatic (WASM-based preemption)](https://github.com/lunatic-solutions/lunatic) - Erlang-like preemption via WASM

#### Further Reading

- [Actors with Tokio](https://ryhl.io/blog/actors-with-tokio/) - Patterns for actors in async Rust
- [Joe Armstrong's Thesis](https://erlang.org/download/armstrong_thesis_2003.pdf) - "Making reliable distributed systems in the presence of software errors" - the philosophical foundation of Erlang's approach

---

## Implementation Details

### Crate Structure

```
spawned/
├── concurrency/           # spawned-concurrency
│   └── src/
│       ├── actor.rs       # Actor trait
│       ├── actor_ref.rs   # ActorRef
│       ├── task.rs        # Task primitive
│       ├── task_group.rs  # TaskGroup
│       ├── router.rs      # Routers
│       ├── pool.rs        # Pools
│       ├── inspect.rs     # Observability
│       ├── supervisor.rs  # Supervisor (simplified)
│       ├── dynamic_supervisor.rs  # DynamicSupervisor (split out)
│       └── ...
├── rt/                    # spawned-rt
│   └── src/
│       └── runtime.rs     # Runtime trait
├── test/                  # spawned-test (NEW)
│   └── src/
│       ├── lib.rs
│       ├── scheduler.rs
│       └── time.rs
└── examples/
```

### PR Order

**Phase 0: Code Quality**

*High Priority:*
1. Refactor `receive()` in actor.rs (87 lines, 5 levels nesting → extract handlers)
2. Fix `todo!()` panic in process.rs:124 (channel close handling)
3. Consolidate result enums (MessageResult, InfoResult, RequestResult, InitResult → single ContinueOrStop)
4. Remove `thiserror` dependency - only 4 error variants, manual Display impl suffices
5. Replace `BoxedChildHandle` trait object with enum (avoid heap allocation)

*Medium Priority:*
6. Simplify `RestartIntensityTracker` - use ring buffer or counter instead of Vec + retain()
7. Remove `ActorInit` struct - intermediate abstraction with no value
8. Evaluate `pin-project-lite` dependency - only used for debug blocking detection

*Terminology Cleanup:*
9. Remove Erlang terminology:
   - Delete `process.rs` (remove unused `Process<T>` trait)
   - Update `actor_table.rs`: rename `processes` field to `actors`
   - Update `LinkError::ProcessNotFound` to `ActorNotFound`
   - Update doc comments: "process" → "actor" in pid.rs, link.rs, registry.rs, supervisor.rs, actor.rs

*Estimated Impact: ~200-300 lines removed, 2 dependencies eliminated, ~30% complexity reduction in core actor loop*

**Phase 1: Testing Infrastructure** ← Moved up (enables TDD for all subsequent phases)
10. spawned-test crate skeleton
11. Runtime trait abstraction
12. Virtual time + deterministic scheduler
13. TestRuntime core

**Phase 2: Structured Concurrency** ← Now testable deterministically
14. Task primitive with ActorTable registration
15. TaskGroup with joiner policies
16. `spawn_detached()` escape hatch
17. Level-triggered cancellation with shield scopes

**Phase 3: Observability** ← Moved up (needed before buffer strategies)
18. Inspection API
19. Metrics integration (`metrics` crate)

**Phase 4: Buffer Strategies** ← Can now measure/debug buffer behavior
20. BufferStrategy enum (Fixed/Dropping/Sliding)
21. ActorConfig for mailbox configuration
22. Metrics for buffer behavior (dropped messages, sizes)

**Phase 5: Scheduling & Fairness** ← Extracted from "spanning" section
23. SchedulingConfig (throughput limits, handler timeouts)
24. Reentrancy control (None/Full/CallChain)
25. Starvation detection
26. YieldBudget helper

**Phase 6: Routers & Pools** ← Highest level, builds on everything
27. Router strategies (RoundRobin, Random, SmallestMailbox, ConsistentHash, Broadcast)
28. Pool with auto-scaling

---

## Complete Feature Matrix

| Feature | Erlang | Akka | Orleans | Spawned |
|---------|--------|------|---------|---------|
| Lightweight processes | Yes | Yes | Yes | Yes |
| Message passing | Yes | Yes | Yes | Yes |
| Supervision trees | Yes | Yes | No | Yes |
| Location transparency | Yes | Yes | Yes | v1.0 |
| Explicit lifecycle | Yes | Yes | No | Yes (default) |
| Virtual actors | No | No* | Yes | v1.0 (opt-in) |
| Event sourcing | No | Yes | Yes | v1.0 |
| Clustering | Yes | Yes | Yes | v1.0 |
| **Deterministic testing** | No | No | No | **v0.5** |
| **Process inspection** | Yes | No | No | **v0.5** |

---

## Insights from Other Frameworks

### Gleam OTP

**Subject-based typed messaging**: Gleam uses a "Subject" type - a typed channel where only the owning process can receive. This validates spawned's approach with `ActorRef<A>` where message types are tied to the Actor trait.

**Actor vs Actor split**: Gleam distinguishes between:
- `gleam_otp/actor` - Fully typed, Gleam-native
- `gleam_erlang/process` - Low-level Erlang compatibility

Spawned follows this pattern: `Actor` is the typed layer, `Pid` and `ActorTable` are lower-level primitives.

### Elixir Patterns

**Task.Supervisor.async_nolink**: Elixir's pattern for spawning async work from within an Actor without blocking. This validates the Task/TaskGroup design - actors need a way to spawn short-lived concurrent work.

**Key anti-pattern to avoid**: Using Actor purely for code organization when no runtime benefit exists. Actors should only be used when you need:
- Isolated state
- Message serialization
- Supervision

Don't use Actor just to organize code - use modules/structs for that.

### Pony Language

**Reference capabilities**: Pony's type system uses 6 reference capabilities (iso, val, ref, box, trn, tag) to guarantee data-race freedom at compile time. While Rust has Send/Sync, Pony's approach is more granular.

**Key innovation**: `iso` (isolated) references guarantee single ownership and are the only way to send mutable data between actors - the type system enforces this.

**Causal messaging**: Pony guarantees that if actor A sends messages M1 then M2 to actor B, B receives them in that order. This is stronger than just FIFO per-pair.

**Per-actor GC (ORCA)**: Each actor has its own heap and garbage collector, eliminating stop-the-world pauses. Actors only GC when idle.

**Adopt for Spawned**: Consider typed message guarantees similar to reference capabilities
**Skip**: Full reference capability system (would require language-level changes)

### Lunatic Runtime

**WASM process isolation**: Each process runs in its own WASM sandbox with memory isolation. Crashes cannot corrupt other processes' memory.

**Serialization-based messaging**: All messages are serialized, enabling true isolation and future distribution. Trade-off: higher overhead than zero-copy.

**Resource limits per process**: Can limit memory, CPU, and file handles per process - prevents runaway processes from taking down the system.

**Mailbox design**: Supports selective receive with pattern matching on message types.

**Adopt for Spawned**: Resource limits per actor (memory/mailbox size caps)
**Skip**: WASM isolation (too heavyweight for embedded Rust runtime)

### Proto.Actor

**Virtual actors (grains)**: Like Orleans, actors are activated on-demand and can migrate between nodes. Identity-based addressing rather than location-based.

**Cross-language messaging**: Uses protobuf for all messages, enabling Go/C#/.NET actors to communicate seamlessly.

**Behavior stack**: Actors can `become(behavior)` to swap message handlers, then `unbecome()` to restore previous behavior. Useful for state machines.

**Persistence providers**: Pluggable event store backends (PostgreSQL, MongoDB, etc.) with snapshot support.

**Adopt for Spawned**: Behavior stack pattern for complex state machines
**Skip**: Protobuf requirement (keep Rust-native serialization optional)

### Trio (Python)

**Structured concurrency pioneer**: Trio introduced "nurseries" - a scope that owns spawned tasks and ensures they all complete before the scope exits.

**Level-triggered cancellation**: Cancellation is a persistent state, not an edge event. Code checks "am I cancelled?" rather than catching cancel exceptions. More reliable.

**Shield scopes**: Temporarily protect critical sections from cancellation. Essential for cleanup code.

**Checkpoints**: Explicit points where cancellation can occur, making cancellation behavior predictable.

**Adopt for Spawned**: Level-triggered cancellation model, shield scopes for cleanup
**Skip**: Checkpoints (Rust's .await points serve this purpose)

### Project Loom (Java)

**StructuredTaskScope**: Java's structured concurrency implementation. Parent scope owns child virtual threads.

**Joiner pattern**: Policies for how to handle multiple subtask results:
- `ShutdownOnSuccess` - Return first success, cancel others
- `ShutdownOnFailure` - Cancel all if any fails
- Custom joiners for quorum, timeout, etc.

**ScopedValue**: Thread-local-like values that are inherited by child tasks but immutable. Safer than ThreadLocal for structured concurrency.

**Adopt for Spawned**: Joiner policies for TaskGroup (`AllSuccessful`, `FirstSuccess`, `Quorum(n)`)
**Skip**: ScopedValue (Rust's ownership handles this better)

### Bastion (Rust)

**Supervision trees**: Hierarchical supervision with configurable restart strategies per level.

**Restart strategies**:
- `RestartStrategy::OneForOne` - Only restart failed child
- `RestartStrategy::OneForAll` - Restart all children if one fails
- `RestartStrategy::RestForOne` - Restart failed child and those started after it
- `ExponentialBackOff` - Delay between restarts to prevent restart storms

**Children group redundancy**: Can specify minimum number of children that must be alive for the group to be considered healthy.

**Runtime-agnostic**: Works with tokio, async-std, or any executor.

**Adopt for Spawned**: Exponential backoff for supervisor restarts, children group redundancy
**Skip**: Runtime-agnostic design (we're committed to tokio)

### Ray (Python)

**Task + Actor unification**: Both tasks (stateless) and actors (stateful) share the same scheduling infrastructure. Tasks are just actors with no state.

**Distributed object store**: Large objects stored once, referenced by ID. Actors receive object references, fetch on demand. Avoids copying large data.

**Placement groups**: Co-locate related actors for locality. E.g., "place these 4 actors on the same node" for communication efficiency.

**Actor pools**: Built-in load balancing across multiple instances of the same actor type.

**Adopt for Spawned**: Placement hints for actor locality in distributed mode
**Skip**: Distributed object store (different problem domain)

### Swift Distributed Actors

**`distributed` keyword**: Language-level support for distributed actors. Methods marked `distributed` can be called remotely.

**Explicit distribution surface**: Only `distributed` methods can be called across nodes. Local-only methods are clearly separated. Makes the "network boundary" visible in code.

**SerializationRequirement**: Distributed actor parameters must be `Codable`. Compiler enforces this.

**Actor isolation**: Swift actors (local) guarantee single-threaded access. Distributed actors extend this across nodes.

**Adopt for Spawned**: Explicit marker for remotely-callable methods (e.g., `#[distributed]` attribute)
**Skip**: Language-level integration (Rust macro is sufficient)

### CAF (C++ Actor Framework)

**Typed actor messaging**: Actors declare their interface as a type signature:
```cpp
using calculator = typed_actor<
    result<int>(add_atom, int, int),
    result<int>(sub_atom, int, int)
>;
```
Compile-time verification that messages match the interface.

**Behavior composition**: Combine multiple behaviors into one actor using `or_else`:
```cpp
auto combined = behavior1.or_else(behavior2);
```

**Response promises**: Explicit promise objects for async responses. Can delegate response to another actor.

**Adopt for Spawned**: Typed message interfaces via traits (already doing this), response delegation
**Skip**: C++ template complexity (Rust traits are cleaner)

### Dapr

**Sidecar architecture**: Actors run in application process, Dapr sidecar handles distribution, discovery, and state.

**Timers vs Reminders**:
- **Timers**: In-memory, lost on restart, for short-lived recurring tasks
- **Reminders**: Persisted, survive restarts, for durable scheduled work

**Reentrancy tracking**: Dapr tracks request chains to allow reentrant calls (A calls B calls A) without deadlock, while still preventing true concurrent access.

**Turn-based concurrency**: Only one "turn" (message handler) executes at a time per actor. Reentrancy creates nested turns.

**Adopt for Spawned**: Timer vs Reminder distinction, reentrancy tracking for `call()` chains
**Skip**: Sidecar architecture (too heavyweight)

### Go CSP Patterns

**Channels**: First-class typed communication primitives. Blocking send/receive by default.

**Select statement**: Wait on multiple channel operations, proceed with first ready. Essential for timeout and cancellation patterns.

**Context cancellation**: `context.Context` carries deadlines and cancellation signals through call chains. Child contexts inherit parent's cancellation.

**errgroup**: Structured concurrency for goroutines. Wait for group completion, collect first error.

**Adopt for Spawned**: Context propagation for cancellation/deadlines through actor calls
**Skip**: Raw channels (actors already provide this abstraction)

### Clojure core.async

**Buffer strategies**:
- **Fixed buffer**: Block when full (backpressure)
- **Dropping buffer**: Drop newest when full (lossy but never blocks)
- **Sliding buffer**: Drop oldest when full (keep most recent)

**Pub/sub with topics**: Publish to topics, subscribers filter by predicate. Decouples publishers from subscribers.

**Mult/Mix combinators**:
- `mult`: One channel to many (broadcast)
- `mix`: Many channels to one (merge)
Composable channel operations.

**Transducers on channels**: Apply transformations (map, filter, etc.) to channel data without intermediate collections.

**Adopt for Spawned**: Buffer strategies for mailboxes (fixed/dropping/sliding)
**Skip**: Transducers (Rust iterators serve this purpose)

### Lessons Applied

1. **Full visibility**: Like Erlang, track ALL processes (including short-lived Tasks) in ActorTable
2. **Typed channels**: ActorRef provides type-safe message passing like Gleam's Subject
3. **Task for async work**: Task/TaskGroup fills the gap for async work within actors (like Elixir's Task.Supervisor)
4. **Layers not magic**: Keep explicit lifecycle as default, virtual actors as opt-in layer
5. **Anti-patterns in docs**: Document when NOT to use Actor
6. **Buffer strategies**: Mailboxes should support fixed/dropping/sliding buffers (core.async)
7. **Level-triggered cancellation**: Cancellation as persistent state, not edge event (Trio)
8. **Joiner policies**: TaskGroup needs configurable completion policies (Loom)
9. **Timer vs Reminder**: Distinguish ephemeral vs durable scheduled work (Dapr)
10. **Typed message interfaces**: Compile-time message type verification (CAF)
11. **Context propagation**: Cancellation/deadlines flow through call chains (Go)
12. **Reentrancy tracking**: Prevent deadlock in A->B->A call chains (Dapr)

---

## Prioritized Features from Research

### Tier 1: Must Have (v0.5)

| Feature | Source | Rationale |
|---------|--------|-----------|
| **Buffer Strategies** | core.async | Fixed/dropping/sliding mailboxes prevent both deadlock and memory exhaustion |
| **Level-triggered Cancellation** | Trio | More reliable than edge-triggered; cancelled state persists |
| **Joiner Policies** | Loom | `AllSuccessful`, `FirstSuccess`, `Quorum(n)` for TaskGroup flexibility |
| **Context Propagation** | Go | Cancellation/deadlines flow through nested calls |

### Tier 2: Should Have (v0.5-v1.0)

| Feature | Source | Rationale |
|---------|--------|-----------|
| **Timers vs Reminders** | Dapr | Ephemeral (in-memory) vs durable (persisted) scheduled work |
| **Explicit Distribution Surface** | Swift | Mark remotely-callable methods with `#[distributed]` |
| **Reentrancy Tracking** | Dapr | Detect and handle A->B->A call chains |
| **Exponential Backoff** | Bastion | Prevent supervisor restart storms |

### Tier 3: Nice to Have (v1.0+)

| Feature | Source | Rationale |
|---------|--------|-----------|
| Behavior Stack | Proto.Actor | `become()`/`unbecome()` for state machines |
| Mult/Mix Combinators | core.async | Broadcast and merge for pub/sub |
| Children Group Redundancy | Bastion | Minimum healthy children threshold |
| Resource Limits | Lunatic | Memory/mailbox caps per actor |
| Placement Groups | Ray | Co-locate related actors |
| Response Delegation | CAF | Delegate reply to another actor |

---

## Decisions Made

1. **Tasks in ActorTable**: All Tasks registered for full observability
2. **Metrics**: Use `metrics` crate (pluggable backends)
3. **TestRuntime**: Full runtime replacement (deterministic spawn, time, channels)
4. **Runtime trait**: Generics with default type parameter
5. **Naming**: Actor/ActorRef/Request/Message/Reply
6. **Structured concurrency**: Default, with `spawn_detached()` escape hatch
7. **Code cleanup first**: Simplify before adding features
8. **Buffer strategies**: Fixed buffer by default (1000 messages), Dropping/Sliding opt-in (from core.async)
9. **Cancellation model**: Level-triggered (Trio-style) - cancelled state persists, not edge event
10. **Typed messages**: Enum-based via Actor trait (no `Box<dyn Any>`) - compile-time safety over runtime flexibility
11. **Joiner policies**: TaskGroup supports `AllCompleted`, `FirstSuccess`, `FirstFailure`, `Quorum(n)` (from Loom)
12. **Timer vs Reminder**: In-memory timers for ephemeral, persisted reminders for durable (from Dapr, in v1.0)

## Open Questions

1. Should routers automatically supervise workers?
2. API stability level for v0.5?
3. Should reentrancy tracking be opt-in or default for `call()`?
4. Context propagation: separate type or integrated into `Context<A>`?

## Success Criteria for v0.5

- [ ] Task and TaskGroup with tests
- [ ] `spawn_detached()` escape hatch available
- [ ] Joiner policies for TaskGroup (AllCompleted, FirstSuccess, Quorum)
- [ ] Buffer strategies implemented (Fixed/Dropping/Sliding)
- [ ] Level-triggered cancellation with shield scopes
- [ ] spawned-test enables deterministic testing
- [ ] Inspection API provides visibility
- [ ] Router supports 3+ strategies
- [ ] Pool supports min/max sizing
- [ ] All existing tests pass
