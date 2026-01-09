//! Supervisor Example
//!
//! This example demonstrates how to use the supervisor module to manage
//! child processes with different restart strategies.
//!
//! The supervisor module provides OTP-style supervision trees for automatic
//! process restart and fault tolerance.
//!
//! Note: This example focuses on the supervisor state management API.
//! In a full implementation, the Supervisor would be a Actor itself
//! that monitors its children and restarts them automatically.

use spawned_concurrency::messages::Unused;
use spawned_concurrency::tasks::{
    RequestResult, Actor, ActorRef, HasPid, InitResult,
};
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        println!("=== Supervisor Example ===\n");

        // Example 1: Using supervisor with actual Actor
        example_genserver_supervisor().await;

        // Example 2: Supervisor concepts explanation
        example_supervisor_concepts();

        println!("\n=== All examples completed! ===");
    });
}

// A simple counter Actor for demonstration
struct Counter {
    name: String,
    count: u64,
}

#[derive(Debug, Clone)]
enum CounterMsg {
    Get,
    Increment,
    IncrementBy(u64),
}

impl Counter {
    fn named(name: &str) -> Self {
        Counter {
            name: name.to_string(),
            count: 0,
        }
    }
}

impl Actor for Counter {
    type Request = CounterMsg;
    type Message = Unused;
    type Reply = u64;
    type Error = String;

    async fn init(
        self,
        _handle: &ActorRef<Self>,
    ) -> Result<InitResult<Self>, Self::Error> {
        println!("  Counter '{}' initialized", self.name);
        Ok(InitResult::Success(self))
    }

    async fn handle_request(
        &mut self,
        message: Self::Request,
        _handle: &ActorRef<Self>,
    ) -> RequestResult<Self> {
        match message {
            CounterMsg::Get => RequestResult::Reply(self.count),
            CounterMsg::Increment => {
                self.count += 1;
                RequestResult::Reply(self.count)
            }
            CounterMsg::IncrementBy(n) => {
                self.count += n;
                RequestResult::Reply(self.count)
            }
        }
    }
}

/// Demonstrates using actual Actors that could be supervised
async fn example_genserver_supervisor() {
    println!("--- Example 1: Actors that could be supervised ---\n");

    // Start workers using Actor
    let mut worker1 = Counter::named("worker1").start();
    let mut worker2 = Counter::named("worker2").start();

    println!("Started workers:");
    println!("  worker1 pid: {}", worker1.pid());
    println!("  worker2 pid: {}", worker2.pid());

    // Interact with workers
    let count1 = worker1.call(CounterMsg::Increment).await.unwrap();
    let count2 = worker1.call(CounterMsg::Increment).await.unwrap();
    println!("\nIncremented worker1 twice: {} -> {}", count1, count2);

    let _ = worker2.call(CounterMsg::IncrementBy(10)).await.unwrap();
    let worker2_count = worker2.call(CounterMsg::Get).await.unwrap();
    println!("Worker2 count after incrementing by 10: {}", worker2_count);

    println!("\nIn a full supervisor implementation, the supervisor would:");
    println!("  1. Start these workers as children");
    println!("  2. Monitor them for crashes");
    println!("  3. Restart them according to the restart strategy\n");
}

/// Explains supervisor concepts
fn example_supervisor_concepts() {
    println!("--- Example 2: Supervisor Concepts ---\n");

    println!("The supervisor module provides these key types:\n");

    println!("ChildSpec - Specifies how to start and supervise a child:");
    println!("  ChildSpec::new(\"worker\", start_fn)");
    println!("    .permanent()   // Always restart");
    println!("    .transient()   // Restart on crash only");
    println!("    .temporary()   // Never restart\n");

    println!("SupervisorSpec - Configures the supervisor:");
    println!("  SupervisorSpec::new(RestartStrategy::OneForOne)");
    println!("    .max_restarts(5, Duration::from_secs(60))");
    println!("    .child(child_spec1)");
    println!("    .child(child_spec2)\n");

    println!("=== Restart Strategies ===\n");

    println!("OneForOne:");
    println!("  When a child crashes, only that child is restarted.");
    println!("  Use when children are independent.");
    println!("  Example: Multiple HTTP request handlers.\n");

    println!("OneForAll:");
    println!("  When any child crashes, ALL children are restarted.");
    println!("  Use when children are tightly coupled.");
    println!("  Example: Database + cache that must stay in sync.\n");

    println!("RestForOne:");
    println!("  When a child crashes, that child and all children");
    println!("  started AFTER it are restarted.");
    println!("  Use when children have a dependency chain.");
    println!("  Example: Config -> Database -> API server\n");

    println!("=== Restart Types ===\n");

    println!("Permanent (default):");
    println!("  Always restart, regardless of exit reason.");
    println!("  Use for long-running services.\n");

    println!("Transient:");
    println!("  Restart only on abnormal exit (crash).");
    println!("  Use for tasks that may complete successfully.\n");

    println!("Temporary:");
    println!("  Never restart.");
    println!("  Use for one-shot tasks.\n");

    println!("=== Restart Intensity ===\n");

    println!("Prevents rapid restart loops:");
    println!("  .max_restarts(5, Duration::from_secs(60))");
    println!();
    println!("If more than 5 restarts occur within 60 seconds,");
    println!("the supervisor shuts down to prevent cascading failures.");
}
