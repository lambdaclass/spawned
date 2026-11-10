use spawned_concurrency::{message::Message, protocol, ActorError, Response};
use std::time::Duration;

#[protocol(timeout = Duration::from_secs(10))]
pub trait TimedProtocol: Send + Sync {
    fn work(&self, millis: u64) -> Response<u64>;
    #[request_timeout(Duration::from_millis(2))]
    fn brief(&self, millis: u64) -> Response<u64>;
    #[cfg(any())]
    fn unavailable(&self) -> Response<()>;
}
#[protocol]
pub trait LegacyProtocol: Send + Sync {
    fn legacy(&self, millis: u64) -> Response<u64>;
}
#[protocol(timeout = Duration::ZERO)]
pub trait ZeroProtocol: Send + Sync {
    fn zero(&self) -> Response<()>;
}

pub struct Job {
    timeout: Duration,
    value: u64,
}
#[protocol]
pub trait OwnedProtocol: Send + Sync {
    #[request_timeout(__spawned_request_timeout.timeout)]
    fn owned(&self, __spawned_request_timeout: Job) -> Response<u64>;
}

#[protocol]
pub trait NamingProtocol: Send + Sync + std::fmt::Debug {
    fn timeout(&self) -> Response<u64>;
    fn actor_ref(&self) -> Response<u64>;
    fn __timeout_override(&self) -> Response<u64>;
    fn __dispatch_0(&self) -> Response<u64>;
}

// Existing manually implemented protocols do not acquire new required methods.
struct Fake;
impl TimedProtocol for Fake {
    fn work(&self, millis: u64) -> Response<u64> {
        Response::ready(Ok(millis))
    }
    fn brief(&self, millis: u64) -> Response<u64> {
        Response::ready(Ok(millis))
    }
}
#[test]
fn fake_protocols_remain_source_compatible() {
    assert_eq!(Fake.work(7).unwrap(), 7);
}

#[cfg(feature = "async")]
mod asynchronous {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    struct Worker;
    impl Actor for Worker {}
    impl Handler<timed_protocol::Work> for Worker {
        async fn handle(&mut self, msg: timed_protocol::Work, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            spawned_rt::tasks::sleep(Duration::from_millis(msg.millis)).await;
            msg.millis
        };
            let _ = reply.reply(__reply_value);
        }
    }
    impl Handler<timed_protocol::Brief> for Worker {
        async fn handle(&mut self, msg: timed_protocol::Brief, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            spawned_rt::tasks::sleep(Duration::from_millis(msg.millis)).await;
            msg.millis
        };
            let _ = reply.reply(__reply_value);
        }
    }
    impl Handler<legacy_protocol::Legacy> for Worker {
        async fn handle(&mut self, msg: legacy_protocol::Legacy, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            spawned_rt::tasks::sleep(Duration::from_millis(msg.millis)).await;
            msg.millis
        };
            let _ = reply.reply(__reply_value);
        }
    }
    // Tokio's paused clock normally; the simulator's clock is always virtual.
    #[cfg_attr(not(spawned_sim), tokio::test(start_paused = true))]
    #[cfg_attr(spawned_sim, spawned_macros::sim_test)]
    async fn protocol_method_and_handle_overrides_have_explicit_precedence() {
        let actor = Worker.start();
        // Virtual time: larger than the old five-second default.
        assert_eq!(actor.work(6000).await.unwrap(), 6000);
        assert_eq!(
            actor
                .with_request_timeout(Duration::from_secs(10))
                .brief(6000)
                .await
                .unwrap(),
            6000
        );
        let erased = actor
            .with_request_timeout(Duration::from_secs(10))
            .to_timed_ref();
        assert_eq!(erased.brief(6000).await.unwrap(), 6000);
        assert!(matches!(
            actor.brief(6000).await,
            Err(ActorError::RequestTimeout)
        ));
        // The override did not mutate the original handle; timeout did not
        // preempt the handler. Stop/join still waits for its ordinary completion.
        actor.context().stop();
        actor.join().await;

        let actor = Worker.start();
        assert!(matches!(
            actor
                .with_request_timeout(Duration::from_millis(1))
                .work(6000)
                .await,
            Err(ActorError::RequestTimeout)
        ));
        actor.context().stop();
        actor.join().await;

        let actor = Worker.start();
        assert!(matches!(
            actor.legacy(6000).await,
            Err(ActorError::RequestTimeout)
        ));
        actor.context().stop();
        actor.join().await;
    }
}

mod synchronous {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    struct Worker;
    impl Actor for Worker {}
    impl Handler<timed_protocol::Work> for Worker {
        fn handle(&mut self, msg: timed_protocol::Work, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            std::thread::sleep(Duration::from_millis(msg.millis));
            msg.millis
        };
            let _ = reply.reply(__reply_value);
        }
    }
    impl Handler<timed_protocol::Brief> for Worker {
        fn handle(&mut self, msg: timed_protocol::Brief, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            std::thread::sleep(Duration::from_millis(msg.millis));
            msg.millis
        };
            let _ = reply.reply(__reply_value);
        }
    }
    #[test]
    fn method_override_and_configured_type_erasure_work_without_async() {
        let actor = Worker.start();
        let configured = actor.with_request_timeout(Duration::from_secs(1));
        assert_eq!(configured.brief(25).unwrap(), 25);
        let erased = configured.to_timed_ref();
        assert_eq!(erased.brief(25).unwrap(), 25);
        assert!(matches!(
            futures::executor::block_on(actor.brief(25)),
            Err(ActorError::RequestTimeout)
        ));
        actor.context().stop();
        actor.join();
    }

    struct Held {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Actor for Held {
        fn started(&mut self, _: &Context<Self>) {
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }
    impl Handler<zero_protocol::Zero> for Held {
        fn handle(&mut self, _: zero_protocol::Zero, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
            let _ = reply.reply(());
        }
    }
    impl Handler<naming_protocol::Timeout> for Worker {
        fn handle(&mut self, _: naming_protocol::Timeout, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            31
        };
            let _ = reply.reply(__reply_value);
        }
    }
    impl Handler<naming_protocol::ActorRef> for Worker {
        fn handle(&mut self, _: naming_protocol::ActorRef, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            32
        };
            let _ = reply.reply(__reply_value);
        }
    }
    impl Handler<naming_protocol::TimeoutOverride> for Worker {
        fn handle(&mut self, _: naming_protocol::TimeoutOverride, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            33
        };
            let _ = reply.reply(__reply_value);
        }
    }
    impl Handler<naming_protocol::Dispatch0> for Worker {
        fn handle(&mut self, _: naming_protocol::Dispatch0, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            34
        };
            let _ = reply.reply(__reply_value);
        }
    }
    #[test]
    fn configured_handles_do_not_shadow_protocol_method_names() {
        let actor = Worker.start();
        let configured = actor.with_request_timeout(Duration::from_secs(1));
        assert_eq!(configured.timeout().unwrap(), 31);
        assert_eq!(configured.actor_ref().unwrap(), 32);
        assert_eq!(actor.__timeout_override().unwrap(), 33);
        assert_eq!(configured.__timeout_override().unwrap(), 33);
        assert_eq!(actor.__dispatch_0().unwrap(), 34);
        assert_eq!(configured.__dispatch_0().unwrap(), 34);
        let erased = configured.to_naming_ref();
        assert_eq!(erased.timeout().unwrap(), 31);
        actor.context().stop();
        actor.join();
    }
    impl Handler<owned_protocol::Owned> for Worker {
        fn handle(&mut self, msg: owned_protocol::Owned, reply: spawned_concurrency::reply::PendingReply<'_, u64>, _: &Context<Self>) {
            let __reply_value: u64 = {
            msg.__spawned_request_timeout.value
        };
            let _ = reply.reply(__reply_value);
        }
    }
    #[test]
    fn timeout_expression_precedes_argument_moves_without_capturing_user_names() {
        let actor = Worker.start();
        assert_eq!(
            actor
                .owned(Job {
                    timeout: Duration::from_secs(1),
                    value: 9
                })
                .unwrap(),
            9
        );
        assert_eq!(
            actor
                .with_request_timeout(Duration::from_secs(1))
                .owned(Job {
                    timeout: Duration::ZERO,
                    value: 10
                })
                .unwrap(),
            10
        );
        actor.context().stop();
        actor.join();
    }
    #[test]
    fn protocol_timeout_is_used_by_native_requests() {
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let actor = Held {
            entered,
            release: wait,
        }
        .start();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            futures::executor::block_on(actor.zero()),
            Err(ActorError::RequestTimeout)
        ));
        release.send(()).unwrap();
        actor.context().stop();
        actor.join();
    }
}

#[test]
fn generated_messages_keep_their_original_reply_types() {
    fn check<M: Message<Result = u64>>() {}
    check::<timed_protocol::Work>();
    check::<timed_protocol::Brief>();
}
