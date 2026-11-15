//! Existing API: two owned Tokio runtimes exchanging ordinary typed requests.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
};

struct Critical;
struct Ping;
impl Message for Ping {
    type Result = String;
}
impl Actor for Critical {}
impl Handler<Ping> for Critical {
    async fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::PendingReply<'_, String>,
        _: &Context<Self>,
    ) {
        reply.reply(std::thread::current().name().unwrap().to_owned());
    }
}
struct Router(ActorRef<Critical>);
impl Actor for Router {}
impl Handler<Ping> for Router {
    async fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::PendingReply<'_, String>,
        _: &Context<Self>,
    ) {
        let __reply_value: String = {
            assert_eq!(std::thread::current().name(), Some("background-worker"));
            self.0.request(Ping).await.unwrap()
        };
        reply.reply(__reply_value);
    }
}

fn main() {
    let critical = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("critical-worker")
        .enable_all()
        .build()
        .unwrap();
    let background = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("background-worker")
        .enable_all()
        .build()
        .unwrap();
    let worker = {
        let _entered = critical.enter();
        Critical.start()
    };
    let router = {
        let _entered = background.enter();
        Router(worker.clone()).start()
    };
    critical.block_on(async {
        let thread = router.request(Ping).await.unwrap();
        assert_eq!(thread, "critical-worker");
        println!("background router -> critical actor -> reply; handled on {thread}");
        // Stop callers first, then their dependency; join before dropping runtimes.
        router.child_handle().stop();
        router.join().await;
        worker.child_handle().stop();
        worker.join().await;
    });
    // Separate worker budgets do not pin cores or guarantee a latency improvement.
}
