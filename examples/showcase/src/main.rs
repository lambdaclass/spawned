//! Comprehensive showcase of spawned-concurrency features.
//!
//! This example demonstrates all major features:
//! - GenServer with different Backends (Async, Blocking, Thread)
//! - Process registry (Pid and typed handle registration)
//! - Process linking and monitoring
//! - Process groups (pg)
//! - Supervisor and DynamicSupervisor
//! - Process introspection

use spawned_concurrency::{
    pg, process_table, registry,
    supervisor::{ChildSpec, DynamicSupervisor, DynamicSupervisorCall, DynamicSupervisorSpec,
                 RestartStrategy, Supervisor, SupervisorCall, SupervisorSpec},
    Backend, CallResponse, CastResponse, GenServer, GenServerHandle,
    HasPid,
};
use spawned_rt::tasks as rt;
use std::time::Duration;

// =============================================================================
// Counter GenServer - Simple stateful server
// =============================================================================

struct Counter {
    value: i64,
}

impl Counter {
    fn new() -> Self {
        Counter { value: 0 }
    }
}

#[derive(Clone)]
enum CounterCall {
    Get,
    Increment,
    Add(i64),
    Stop,
}

#[derive(Clone)]
enum CounterCast {
    Reset,
}

impl GenServer for Counter {
    type CallMsg = CounterCall;
    type CastMsg = CounterCast;
    type OutMsg = i64;
    type Error = ();

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CallResponse<Self> {
        match message {
            CounterCall::Get => CallResponse::Reply(self.value),
            CounterCall::Increment => {
                self.value += 1;
                CallResponse::Reply(self.value)
            }
            CounterCall::Add(n) => {
                self.value += n;
                CallResponse::Reply(self.value)
            }
            CounterCall::Stop => CallResponse::Stop(self.value),
        }
    }

    async fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CastResponse {
        match message {
            CounterCast::Reset => {
                self.value = 0;
                CastResponse::NoReply
            }
        }
    }
}

// =============================================================================
// Main demonstration
// =============================================================================

fn main() {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    rt::run(async {
        println!("=== Spawned Concurrency Showcase ===\n");

        // 1. Basic GenServer with different backends
        demo_backends().await;

        // 2. Registry demonstration
        demo_registry().await;

        // 3. Process groups demonstration
        demo_process_groups().await;

        // 4. Process linking and monitoring
        demo_linking_monitoring().await;

        // 5. Supervisor demonstration
        demo_supervisor().await;

        // 6. DynamicSupervisor demonstration
        demo_dynamic_supervisor().await;

        // 7. Process introspection
        demo_introspection().await;

        println!("\n=== All demonstrations completed! ===");
    });
}

async fn demo_backends() {
    println!("--- 1. GenServer Backends ---");

    // Async backend (default, best for I/O-bound work)
    let mut async_counter = Counter::new().start(Backend::Async);
    async_counter.call(CounterCall::Increment).await.unwrap();
    let val = async_counter.call(CounterCall::Get).await.unwrap();
    println!("  Async backend counter: {}", val);
    async_counter.call(CounterCall::Stop).await.ok();

    // Blocking backend (for CPU-bound work that might block)
    let mut blocking_counter = Counter::new().start(Backend::Blocking);
    blocking_counter.call(CounterCall::Add(10)).await.unwrap();
    let val = blocking_counter.call(CounterCall::Get).await.unwrap();
    println!("  Blocking backend counter: {}", val);
    blocking_counter.call(CounterCall::Stop).await.ok();

    // Thread backend (dedicated OS thread)
    let mut thread_counter = Counter::new().start(Backend::Thread);
    thread_counter.call(CounterCall::Add(100)).await.unwrap();
    let val = thread_counter.call(CounterCall::Get).await.unwrap();
    println!("  Thread backend counter: {}", val);
    thread_counter.call(CounterCall::Stop).await.ok();

    println!();
}

async fn demo_registry() {
    println!("--- 2. Process Registry ---");

    // Start a counter and register it by name
    let counter = Counter::new().start(Backend::Async);
    let pid = counter.pid();

    // Register the typed handle (allows messaging later)
    registry::register_handle("main_counter", counter).unwrap();
    println!("  Registered 'main_counter' with pid {}", pid);

    // Look up by name and use it
    if let Some(mut handle) = registry::lookup::<Counter>("main_counter") {
        handle.call(CounterCall::Add(42)).await.unwrap();
        let val = handle.call(CounterCall::Get).await.unwrap();
        println!("  Looked up 'main_counter' and got value: {}", val);
        handle.call(CounterCall::Stop).await.ok();
    }

    // Can also get just the Pid
    if let Some(found_pid) = registry::whereis("main_counter") {
        println!("  whereis('main_counter') = {}", found_pid);
    }

    // List all registered names
    let names = registry::registered();
    println!("  Registered names: {:?}", names);

    // Cleanup
    registry::unregister("main_counter");
    println!();
}

async fn demo_process_groups() {
    println!("--- 3. Process Groups (pg) ---");

    // Create some worker processes
    let worker1 = Counter::new().start(Backend::Async);
    let worker2 = Counter::new().start(Backend::Async);
    let worker3 = Counter::new().start(Backend::Async);

    let pid1 = worker1.pid();
    let pid2 = worker2.pid();
    let pid3 = worker3.pid();

    // Join workers to groups
    pg::join("workers", pid1).unwrap();
    pg::join("workers", pid2).unwrap();
    pg::join("workers", pid3).unwrap();
    pg::join("active", pid1).unwrap();
    pg::join("active", pid2).unwrap();

    println!("  Created 3 workers and joined them to groups");

    // Get group members
    let workers = pg::get_members("workers");
    println!("  'workers' group has {} members", workers.len());

    let active = pg::get_members("active");
    println!("  'active' group has {} members", active.len());

    // Check which groups a process belongs to
    let groups = pg::which_groups(pid1);
    println!("  Worker 1 is in groups: {:?}", groups);

    // Check membership
    println!("  Is worker1 in 'workers'? {}", pg::is_member("workers", pid1));
    println!("  Is worker3 in 'active'? {}", pg::is_member("active", pid3));

    // Cleanup
    pg::leave_all(pid1);
    pg::leave_all(pid2);
    pg::leave_all(pid3);
    println!();
}

async fn demo_linking_monitoring() {
    println!("--- 4. Process Linking & Monitoring ---");

    // Start two processes
    let mut counter1 = Counter::new().start(Backend::Async);
    let mut counter2 = Counter::new().start(Backend::Async);

    let pid1 = counter1.pid();
    let pid2 = counter2.pid();

    // Link processes (bidirectional - if one crashes, other is notified)
    counter1.link(&counter2).unwrap();
    println!("  Linked {} and {}", pid1, pid2);

    // Monitor a process (one-way notification on exit)
    let monitor_ref = counter1.monitor(&counter2).unwrap();
    println!("  {} is now monitoring {} (ref: {:?})", pid1, pid2, monitor_ref);

    // Check trap_exit setting
    counter1.trap_exit(true);
    println!("  {} is now trapping exits", pid1);

    // Cleanup
    counter1.call(CounterCall::Stop).await.ok();
    counter2.call(CounterCall::Stop).await.ok();
    tokio::time::sleep(Duration::from_millis(50)).await;
    println!();
}

async fn demo_supervisor() {
    println!("--- 5. Supervisor ---");

    // Create a supervisor spec with workers
    let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
        .max_restarts(3, Duration::from_secs(5))
        .child(ChildSpec::worker("worker_a", || {
            Counter::new().start(Backend::Async)
        }))
        .child(ChildSpec::worker("worker_b", || {
            Counter::new().start(Backend::Async)
        }));

    // Start the supervisor
    let mut supervisor = Supervisor::start(spec);
    println!("  Started supervisor with 2 workers");

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Get child counts
    if let Ok(response) = supervisor.call(SupervisorCall::CountChildren).await {
        println!("  Supervisor response: {:?}", response);
    }

    // Restart a child
    if let Ok(response) = supervisor.call(SupervisorCall::RestartChild("worker_a".to_string())).await {
        println!("  Restarted worker_a: {:?}", response);
    }

    // Cleanup
    supervisor.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;
    println!();
}

async fn demo_dynamic_supervisor() {
    println!("--- 6. DynamicSupervisor ---");

    // Create a dynamic supervisor (children added at runtime)
    let spec = DynamicSupervisorSpec::new()
        .max_children(10)
        .max_restarts(5, Duration::from_secs(10));

    let mut dyn_sup = DynamicSupervisor::start(spec);
    println!("  Started dynamic supervisor");

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Start children dynamically
    let child_spec1 = ChildSpec::worker("dyn_worker_1", || {
        Counter::new().start(Backend::Async)
    });

    if let Ok(response) = dyn_sup.call(DynamicSupervisorCall::StartChild(child_spec1)).await {
        println!("  Started dynamic child 1: {:?}", response);
    }

    let child_spec2 = ChildSpec::worker("dyn_worker_2", || {
        Counter::new().start(Backend::Async)
    });

    if let Ok(response) = dyn_sup.call(DynamicSupervisorCall::StartChild(child_spec2)).await {
        println!("  Started dynamic child 2: {:?}", response);
    }

    // Count children
    if let Ok(response) = dyn_sup.call(DynamicSupervisorCall::CountChildren).await {
        println!("  Dynamic supervisor children: {:?}", response);
    }

    // Cleanup
    dyn_sup.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;
    println!();
}

async fn demo_introspection() {
    println!("--- 7. Process Introspection ---");

    // Start some processes
    let mut counter1 = Counter::new().start(Backend::Async);
    let counter2 = Counter::new().start(Backend::Async);

    let pid1 = counter1.pid();
    let _pid2 = counter2.pid();

    // Link and monitor for richer introspection
    counter1.link(&counter2).unwrap();
    counter1.trap_exit(true);

    // Register a name
    registry::register("introspect_test", pid1).unwrap();

    // Get process info
    if let Some(info) = process_table::process_info(pid1) {
        println!("  Process Info for {}:", info.pid);
        println!("    - alive: {}", info.alive);
        println!("    - trap_exit: {}", info.trap_exit);
        println!("    - links: {:?}", info.links);
        println!("    - monitored_by: {:?}", info.monitored_by);
        println!("    - monitoring: {:?}", info.monitoring);
        println!("    - registered_name: {:?}", info.registered_name);
    }

    // List all processes
    let all = process_table::all_processes();
    println!("  Total registered processes: {}", all.len());

    // Cleanup
    registry::unregister("introspect_test");
    counter1.call(CounterCall::Stop).await.ok();
    // counter2 will be killed due to link when counter1 stops
    tokio::time::sleep(Duration::from_millis(50)).await;
    println!();
}
