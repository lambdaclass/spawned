//! Public #[actor] lifecycle wiring for trapped exits in both modes.
use spawned_concurrency::{actor, Exit, ExitReason};

mod asynchronous {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context};
    struct Observer {
        seen: Option<tokio::sync::oneshot::Sender<Exit>>,
    }
    #[actor]
    impl Observer {
        #[started]
        async fn begin(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(true);
        }
        #[exit_received]
        async fn linked_exit(&mut self, event: Exit, ctx: &Context<Self>) {
            self.seen.take().unwrap().send(event).unwrap();
            ctx.stop();
        }
    }
    struct Failing;
    #[actor]
    impl Failing {
        #[started]
        async fn begin(&mut self, _: &Context<Self>) {
            panic!("linked startup failure");
        }
    }
    #[test]
    fn macro_receives_linked_exit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let observer = Observer { seen: Some(tx) }.start();
            observer.context().trap_exit(true);
            let child = Failing.start_linked(&observer.context());
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event.from, child.id());
            assert!(matches!(event.reason, ExitReason::Panic(_)));
            child.wait_exit().await;
            assert_eq!(observer.wait_exit().await, ExitReason::Normal);
        });
    }
}
mod synchronous {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context};
    struct Observer {
        seen: std::sync::mpsc::Sender<Exit>,
    }
    #[actor]
    impl Observer {
        #[started]
        fn begin(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(true);
        }
        #[exit_received]
        fn linked_exit(&mut self, event: Exit, ctx: &Context<Self>) {
            self.seen.send(event).unwrap();
            ctx.stop();
        }
    }
    struct Failing;
    #[actor]
    impl Failing {
        #[started]
        fn begin(&mut self, _: &Context<Self>) {
            panic!("linked startup failure");
        }
    }
    #[test]
    fn macro_receives_linked_exit() {
        let (tx, rx) = std::sync::mpsc::channel();
        let observer = Observer { seen: tx }.start();
        observer.context().trap_exit(true);
        let child = Failing.start_linked(&observer.context());
        let event = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(event.from, child.id());
        assert!(matches!(event.reason, ExitReason::Panic(_)));
        child.child_handle().wait_exit_blocking();
        assert_eq!(
            observer.child_handle().wait_exit_blocking(),
            ExitReason::Normal
        );
    }
}
