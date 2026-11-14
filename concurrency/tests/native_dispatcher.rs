#![cfg(feature = "async")]
#![cfg(feature = "backend-experiments")]
//! Keep this as the sole test in its binary: terminal shutdown closes the process registry.
//! Terminal executor lifecycle and drop-panic isolation in a fresh test process.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler},
    ExitReason,
};
use std::time::Duration;
const LIMIT: Duration = Duration::from_secs(5);
struct Ping;
impl Message for Ping {
    type Result = u64;
}
struct Healthy;
impl Actor for Healthy {}
impl Handler<Ping> for Healthy {
    async fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            tokio::task::yield_now().await;
            42
        };
        reply.reply(__reply_value);
    }
}
struct DropPanic;
impl Actor for DropPanic {}
impl Drop for DropPanic {
    fn drop(&mut self) {
        panic!("injected actor drop panic");
    }
}
#[test]
fn drop_panic_isolated_and_terminal_shutdown_rejects_new_actors() {
    let scheduler = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
    if spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&scheduler).is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let bad = DropPanic.start();
        let healthy = Healthy.start();
        assert_eq!(healthy.request_with_timeout(Ping, LIMIT).await.unwrap(), 42);
        bad.child_handle().stop();
        assert_eq!(
            tokio::time::timeout(LIMIT, bad.wait_exit()).await.unwrap(),
            ExitReason::Kill
        );
        assert_eq!(healthy.request_with_timeout(Ping, LIMIT).await.unwrap(), 42);
        healthy.child_handle().stop();
        assert_eq!(
            tokio::time::timeout(LIMIT, healthy.wait_exit())
                .await
                .unwrap(),
            ExitReason::Normal
        );
    });
    // Shutdown must join workers from outside their runtime, and be idempotent.
    spawned_rt::tasks::experimental::dispatcher::shutdown().unwrap();
    spawned_rt::tasks::experimental::dispatcher::shutdown().unwrap();
    runtime.block_on(async {
        let refused = Healthy.start();
        assert_eq!(
            tokio::time::timeout(LIMIT, refused.wait_exit())
                .await
                .unwrap(),
            ExitReason::Kill
        );
        assert!(refused.request_with_timeout(Ping, LIMIT).await.is_err());
    });
}
