use super::*;
use crate::test_wait::WAIT;

struct Echo;
struct Ask(u32);
impl Message for Ask {
    type Result = u32;
}
/// Holds the actor inside a handler until released, then stops it, so a
/// request queued behind it is admitted but never answered.
struct StopAfter(rt::oneshot::Receiver<()>);
impl Message for StopAfter {
    type Result = ();
}
struct OwnRecipient;
impl Message for OwnRecipient {
    type Result = (Recipient<Ask>, ActorId);
}
struct Boom;
impl Message for Boom {
    type Result = ();
}
impl Actor for Echo {}
impl Handler<Ask> for Echo {
    async fn handle(
        &mut self,
        msg: Ask,
        reply: crate::reply::PendingReply<'_, u32>,
        _: &Context<Self>,
    ) {
        reply.reply(msg.0);
    }
}
impl Handler<StopAfter> for Echo {
    async fn handle(
        &mut self,
        msg: StopAfter,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        let _ = msg.0.await;
        ctx.stop();
        reply.reply(());
    }
}
impl Handler<OwnRecipient> for Echo {
    async fn handle(
        &mut self,
        _: OwnRecipient,
        reply: crate::reply::PendingReply<'_, (Recipient<Ask>, ActorId)>,
        ctx: &Context<Self>,
    ) {
        reply.reply((ctx.recipient::<Ask>(), ctx.id()));
    }
}
impl Handler<Boom> for Echo {
    async fn handle(
        &mut self,
        _: Boom,
        _reply: crate::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("boom");
        }
    }
}

#[test]
fn context_recipient_reaches_its_actor() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::context_recipient_reaches_its_actor"),
        move || {
            rt::Runtime::new().unwrap().block_on(async {
                let actor = Echo.spawn();
                let (recipient, id) = actor.request(OwnRecipient).await.unwrap();
                assert_eq!(id, actor.id());
                assert!(matches!(request(&*recipient, Ask(7), WAIT).await, Ok(7)));
                recipient.send(Ask(1)).unwrap();
                actor.child_handle().stop();
                actor.join().await;
                // Past the death point: NoProc (v0.8, decision 4 of otp-exit-signals).
                assert!(matches!(recipient.send(Ask(1)), Err(ActorError::NoProc)));
            });
        },
    );
}

#[test]
fn requests_to_a_stopped_actor_fail_without_waiting() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::requests_to_a_stopped_actor_fail_without_waiting"
        ),
        move || {
            rt::Runtime::new().unwrap().block_on(async {
                let actor = Echo.spawn();
                let recipient = actor.recipient::<Ask>();
                actor.child_handle().stop();
                actor.join().await;
                // Past the death point: NoProc (v0.8, decision 4 of otp-exit-signals).
                assert!(matches!(
                    actor.request_with_timeout(Ask(1), WAIT).await,
                    Err(ActorError::NoProc)
                ));
                assert!(matches!(
                    request(&*recipient, Ask(1), WAIT).await,
                    Err(ActorError::NoProc)
                ));
            });
        },
    );
}

#[test]
fn admitted_requests_fail_when_the_actor_stops_before_replying() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::admitted_requests_fail_when_the_actor_stops_before_replying"
        ),
        move || {
            rt::Runtime::new().unwrap().block_on(async {
                let actor = Echo.spawn();
                let recipient = actor.recipient::<Ask>();
                let (release, held) = rt::oneshot::channel();
                actor.send(StopAfter(held)).unwrap();
                let mut direct = Box::pin(actor.request_with_timeout(Ask(1), WAIT));
                let mut erased = Box::pin(request(&*recipient, Ask(2), WAIT));
                assert!(futures::poll!(direct.as_mut()).is_pending());
                assert!(futures::poll!(erased.as_mut()).is_pending());
                release.send(()).unwrap();
                assert!(matches!(
                    direct.await,
                    Err(ActorError::ActorStopped(ExitReason::Normal))
                ));
                assert!(matches!(
                    erased.await,
                    Err(ActorError::ActorStopped(ExitReason::Normal))
                ));
            });
        },
    );
}

#[test]
fn erased_request_times_out() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::erased_request_times_out"),
        move || {
            rt::Runtime::new().unwrap().block_on(async {
                let actor = Echo.spawn();
                let recipient = actor.recipient::<Ask>();
                let (release, held) = rt::oneshot::channel();
                actor.send(StopAfter(held)).unwrap();
                assert!(matches!(
                    request(&*recipient, Ask(1), Duration::from_millis(1)).await,
                    Err(ActorError::RequestTimeout)
                ));
                release.send(()).unwrap();
                actor.join().await;
            });
        },
    );
}

#[test]
fn default_exit_received_keeps_a_trapping_actor_alive() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::default_exit_received_keeps_a_trapping_actor_alive"
        ),
        move || {
            struct Trapper;
            impl Actor for Trapper {
                async fn started(&mut self, ctx: &Context<Self>) {
                    ctx.trap_exit(true);
                }
            }
            impl Handler<Ask> for Trapper {
                async fn handle(
                    &mut self,
                    msg: Ask,
                    reply: crate::reply::PendingReply<'_, u32>,
                    _: &Context<Self>,
                ) {
                    reply.reply(msg.0);
                }
            }
            rt::Runtime::new().unwrap().block_on(async {
                let trapper = Trapper.spawn();
                assert!(matches!(trapper.request(Ask(0)).await, Ok(0)));
                let peer = Echo.spawn();
                trapper.context().link(&peer.child_handle());
                peer.send(Boom).unwrap();
                assert!(matches!(peer.wait_exit().await, ExitReason::Panic(_)));
                // The Exit is handled by the default `exit_received`, which ignores it.
                assert!(matches!(trapper.request(Ask(3)).await, Ok(3)));
                assert!(trapper.exit_reason().is_none());
                trapper
                    .child_handle()
                    .stop_with_reason(crate::ExitReason::Shutdown);
                assert_eq!(trapper.wait_exit().await, ExitReason::Shutdown);
            });
        },
    );
}

#[test]
fn debug_output_names_the_handle() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::debug_output_names_the_handle"),
        move || {
            rt::Runtime::new().unwrap().block_on(async {
                let actor = Echo.spawn();
                assert!(format!("{actor:?}").starts_with("ActorRef"));
                assert!(format!("{:?}", actor.context()).starts_with("Context"));
                actor.child_handle().stop();
                actor.join().await;
            });
        },
    );
}
