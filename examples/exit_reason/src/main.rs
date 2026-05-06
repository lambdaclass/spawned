use spawned_concurrency::protocol;
use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
use spawned_concurrency::{ChildHandle, Response};
use spawned_rt::tasks as rt;
use std::time::Duration;

// -- A simple worker that can be told to stop, panic, or just keep running --

struct Worker {
    name: String,
}

#[protocol]
trait WorkerProtocol: Send + Sync {
    fn stop(&self) -> Response<()>;
    fn panic_now(&self) -> Response<()>;
    fn ping(&self) -> Response<String>;
}

#[spawned_concurrency::actor(protocol = WorkerProtocol)]
impl Worker {
    fn new(name: &str) -> Self {
        Worker {
            name: name.to_string(),
        }
    }

    #[started]
    async fn started(&mut self, _ctx: &Context<Self>) {
        tracing::info!("[{}] started", self.name);
    }

    #[stopped]
    async fn stopped(&mut self, _ctx: &Context<Self>) {
        tracing::info!("[{}] stopped callback running", self.name);
    }

    #[request_handler]
    async fn handle_stop(&mut self, _msg: worker_protocol::Stop, ctx: &Context<Self>) {
        tracing::info!("[{}] received stop request", self.name);
        ctx.stop();
    }

    #[request_handler]
    async fn handle_panic_now(&mut self, _msg: worker_protocol::PanicNow, _ctx: &Context<Self>) {
        tracing::info!("[{}] about to panic!", self.name);
        panic!("intentional panic from {}", self.name);
    }

    #[request_handler]
    async fn handle_ping(&mut self, _msg: worker_protocol::Ping, _ctx: &Context<Self>) -> String {
        format!("pong from {}", self.name)
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    rt::run(async {
        println!("=== Exit Reason Demo ===\n");

        // 1. Clean stop via ctx.stop()
        println!("--- Scenario 1: Clean stop ---");
        let worker1 = Worker::new("worker-1").start();
        let pong = worker1.ping().await.unwrap();
        println!("  {pong}");
        worker1.stop().await.unwrap();
        let reason = worker1.wait_exit().await;
        println!("  Exit reason: {reason}");
        println!("  is_abnormal: {}\n", reason.is_abnormal());

        // 2. Panic in handler
        println!("--- Scenario 2: Panic in handler ---");
        let worker2 = Worker::new("worker-2").start();
        let _ = worker2.panic_now().await; // will fail, actor panics
        let reason = worker2.wait_exit().await;
        println!("  Exit reason: {reason}");
        println!("  is_abnormal: {}\n", reason.is_abnormal());

        // 3. Panic in started()
        println!("--- Scenario 3: Panic in started() ---");
        struct PanicOnStart;
        struct Noop;
        impl spawned_concurrency::message::Message for Noop {
            type Result = ();
        }
        impl Actor for PanicOnStart {
            async fn started(&mut self, _ctx: &Context<Self>) {
                panic!("can't start!");
            }
        }
        impl Handler<Noop> for PanicOnStart {
            async fn handle(&mut self, _msg: Noop, _ctx: &Context<Self>) {}
        }
        let worker3 = PanicOnStart.start();
        let reason = worker3.wait_exit().await;
        println!("  Exit reason: {reason}");
        println!("  is_abnormal: {}\n", reason.is_abnormal());

        // 4. Polling exit_reason() while running
        println!("--- Scenario 4: Polling exit_reason() ---");
        let worker4 = Worker::new("worker-4").start();
        println!("  While running: {:?}", worker4.exit_reason());
        worker4.stop().await.unwrap();
        worker4.join().await;
        println!("  After stop:   {:?}", worker4.exit_reason());

        // 5. ChildHandle — type-erased supervision handle
        println!("\n--- Scenario 5: ChildHandle ---");
        let worker5 = Worker::new("worker-5").start();
        let handle: ChildHandle = worker5.child_handle();
        println!("  ActorId: {}", handle.id());
        println!("  Same id as ActorRef: {}", handle.id() == worker5.id());
        println!("  is_alive: {}", handle.is_alive());

        // Stop via ChildHandle (type-erased, no message sending)
        handle.stop();
        let reason = handle.wait_exit_async().await;
        println!("  Stopped via ChildHandle");
        println!("  Exit reason: {reason}");
        println!("  is_alive: {}", handle.is_alive());

        // 6. ChildHandle from a panicking actor
        println!("\n--- Scenario 6: ChildHandle observes panic ---");
        let worker6 = Worker::new("worker-6").start();
        let handle6 = worker6.child_handle();
        let _ = worker6.panic_now().await;
        let reason = handle6.wait_exit_async().await;
        println!("  Exit reason: {reason}");
        println!("  is_abnormal: {}", reason.is_abnormal());

        // 7. Multiple ChildHandles from different actor types
        println!("\n--- Scenario 7: Heterogeneous ChildHandle vec ---");
        struct Idler;
        impl Actor for Idler {}

        let w = Worker::new("worker-7").start();
        let i = Idler.start();
        let handles: Vec<ChildHandle> = vec![w.child_handle(), i.child_handle()];
        println!("  {} actors managed via Vec<ChildHandle>", handles.len());
        for h in &handles {
            println!("  {} — alive: {}", h.id(), h.is_alive());
        }
        for h in &handles {
            h.stop();
        }
        for h in &handles {
            let reason = h.wait_exit_async().await;
            println!("  {} — exit: {reason}", h.id());
        }

        // Give tracing a moment to flush
        rt::sleep(Duration::from_millis(50)).await;
        println!("\n=== Done ===");
    });
}
