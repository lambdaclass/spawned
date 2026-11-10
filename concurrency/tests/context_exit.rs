//! Explicit exit reasons preserve cooperative cleanup in both execution modes.
use spawned_concurrency::{message::Message, ExitReason};

struct Exit(ExitReason);
impl Message for Exit {
    type Result = ();
}

#[test]
fn native_explicit_exit_is_cooperative_and_first_reason_wins() {
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    struct Worker(Arc<AtomicBool>);
    impl Actor for Worker {
        fn stopped(&mut self, _: &Context<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    impl Handler<Exit> for Worker {
        fn handle(
            &mut self,
            exit: Exit,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            ctx.stop_with_reason(exit.0);
            ctx.stop();
            reply.reply(());
        }
    }
    for reason in [ExitReason::Shutdown, ExitReason::Kill] {
        let cleaned = Arc::new(AtomicBool::new(false));
        let actor = Worker(cleaned.clone()).start();
        actor.request(Exit(reason.clone())).unwrap();
        actor.join();
        assert_eq!(actor.child_handle().exit_reason(), Some(reason));
        assert!(cleaned.load(Ordering::SeqCst));
    }
}

#[cfg(feature = "async")]
#[spawned_macros::sim_test]
async fn async_explicit_exit_is_cooperative_and_first_reason_wins() {
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    struct Worker(Arc<AtomicBool>);
    impl Actor for Worker {
        async fn stopped(&mut self, _: &Context<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    impl Handler<Exit> for Worker {
        async fn handle(
            &mut self,
            exit: Exit,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            ctx.stop_with_reason(exit.0);
            ctx.stop();
            reply.reply(());
        }
    }
    for reason in [ExitReason::Shutdown, ExitReason::Kill] {
        let cleaned = Arc::new(AtomicBool::new(false));
        let actor = Worker(cleaned.clone()).start();
        actor.request(Exit(reason.clone())).await.unwrap();
        spawned_rt::tasks::timeout(std::time::Duration::from_secs(5), actor.join())
            .await
            .unwrap();
        assert_eq!(actor.child_handle().exit_reason(), Some(reason));
        assert!(cleaned.load(Ordering::SeqCst));
    }
}
