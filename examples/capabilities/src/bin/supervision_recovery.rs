//! Recovery constructs fresh state; callers explicitly resolve replacements.
use spawned_concurrency::{
    message::Message,
    supervision::{ChildSpec, RestartMode, SupervisorConfig},
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
};
use std::time::Duration;
struct Next;
impl Message for Next {
    type Result = u64;
}
struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Counter(u64);
impl Actor for Counter {}
impl Handler<Next> for Counter {
    async fn handle(&mut self, _: Next, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
        let __reply_value: u64 = {
        self.0 += 1;
        self.0
    };
        let _ = reply.reply(__reply_value);
    }
}
impl Handler<Crash> for Counter {
    async fn handle(&mut self, _: Crash, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        {
        panic!("intentional recovery example failure");
    }
        let _ = reply.reply(());
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("service", 1).unwrap();
    let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
    let service = tree
        .add(
            ChildSpec::new("counter", &group, |_| async { Ok::<_, String>(Counter(0)) })
                .restart(RestartMode::Transient),
        )
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let old = service.current().unwrap();
    assert_eq!(old.request(Next).await.unwrap(), 1);
    let generation = service.snapshot().generation;
    assert!(old.request(Crash).await.is_err());
    let replacement = tokio::time::timeout(
        Duration::from_secs(3),
        service.wait_for_generation(generation),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(old.id(), replacement.id());
    assert!(old.request(Next).await.is_err());
    assert_eq!(replacement.request(Next).await.unwrap(), 1); // Fresh state, not restored state.
    assert!(supervisor.shutdown(Duration::from_secs(3)).await.complete);
    assert!(system.shutdown(Duration::from_secs(3)).await.complete);
    println!("OneForOne recovery passed; the old reference failed and fresh state started at one.");
}
