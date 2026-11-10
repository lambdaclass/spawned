#![cfg(feature = "async")]
//! Integration test: verifies that #[cfg]-gated protocol methods compile and work
//! correctly, including blanket impls and type-erased references.

use spawned_concurrency::tasks::{Actor, ActorStart as _, Context, Handler};
use spawned_concurrency::{protocol, Response};
use spawned_rt::tasks as rt;

#[protocol]
pub trait MixedProtocol: Send + Sync {
    /// Always available.
    fn get_value(&self) -> Response<u64>;

    /// Only available in test builds.
    #[cfg(test)]
    fn test_only_increment(&self) -> Response<u64>;
}

struct MixedActor {
    value: u64,
}

impl Actor for MixedActor {}

impl Handler<mixed_protocol::GetValue> for MixedActor {
    async fn handle(
        &mut self,
        _msg: mixed_protocol::GetValue,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        _ctx: &Context<Self>,
    ) {
        let __reply_value: u64 = { self.value };
        let _ = reply.reply(__reply_value);
    }
}

#[cfg(test)]
impl Handler<mixed_protocol::TestOnlyIncrement> for MixedActor {
    async fn handle(
        &mut self,
        _msg: mixed_protocol::TestOnlyIncrement,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        _ctx: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            self.value += 1;
            self.value
        };
        let _ = reply.reply(__reply_value);
    }
}

/// Verify that cfg-gated methods work through the blanket impl on ActorRef.
#[test]
fn cfg_gated_method_works_via_blanket_impl() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async {
        let actor = MixedActor { value: 10 }.start();

        let val = actor.get_value().await.unwrap();
        assert_eq!(val, 10);

        // This method only exists because cfg(test) is active
        let val = actor.test_only_increment().await.unwrap();
        assert_eq!(val, 11);

        let val = actor.get_value().await.unwrap();
        assert_eq!(val, 11);
        let configured = actor.with_request_timeout(std::time::Duration::from_secs(1));
        assert_eq!(configured.test_only_increment().await.unwrap(), 12);
        actor.context().stop();
        actor.join().await;
    });
}

/// Verify that cfg-gated methods work through the type-erased protocol reference.
#[test]
fn cfg_gated_method_works_via_protocol_ref() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async {
        let actor = MixedActor { value: 0 }.start();
        let proto_ref: MixedRef = actor.to_mixed_ref();

        let val = proto_ref.get_value().await.unwrap();
        assert_eq!(val, 0);

        let val = proto_ref.test_only_increment().await.unwrap();
        assert_eq!(val, 1);
        let configured = actor
            .with_request_timeout(std::time::Duration::from_secs(1))
            .to_mixed_ref();
        assert_eq!(configured.test_only_increment().await.unwrap(), 2);
        actor.context().stop();
        actor.join().await;
    });
}
