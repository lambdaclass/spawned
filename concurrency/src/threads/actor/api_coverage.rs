
use super::*;
use crate::test_wait::WAIT;
use std::sync::mpsc as std_mpsc;

struct Echo;
struct Ask(u32);
impl Message for Ask {
    type Result = u32;
}
/// Holds the actor inside a handler until released, then stops it, so a
/// request queued behind it is admitted but never answered.
struct StopAfter(std_mpsc::Receiver<()>);
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
struct Unlink(ChildHandle);
impl Message for Unlink {
    type Result = ();
}
impl Actor for Echo {}
impl Handler<Ask> for Echo {
    fn handle(&mut self, msg: Ask, reply: crate::reply::PendingReply<'_, u32>, _: &Context<Self>) {
        reply.reply(msg.0);
    }
}
impl Handler<StopAfter> for Echo {
    fn handle(
        &mut self,
        msg: StopAfter,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        let _ = msg.0.recv_timeout(WAIT);
        ctx.stop();
        reply.reply(());
    }
}
impl Handler<OwnRecipient> for Echo {
    fn handle(
        &mut self,
        _: OwnRecipient,
        reply: crate::reply::PendingReply<'_, (Recipient<Ask>, ActorId)>,
        ctx: &Context<Self>,
    ) {
        reply.reply((ctx.recipient::<Ask>(), ctx.id()));
    }
}
impl Handler<Boom> for Echo {
    fn handle(&mut self, _: Boom, _reply: crate::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        {
            panic!("boom");
        }
    }
}

#[test]
fn context_recipient_and_identity_reach_their_actor() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::context_recipient_and_identity_reach_their_actor"
        ),
        move || {
            let actor = Echo.start();
            let (recipient, id) = actor.request(OwnRecipient).unwrap();
            assert_eq!(id, actor.id());
            assert!(matches!(request(&*recipient, Ask(7), WAIT), Ok(7)));
            recipient.send(Ask(1)).unwrap();
            actor.child_handle().stop();
            actor.join();
            // Past the death point: NoProc (v0.8, decision 4 of otp-exit-signals).
            assert!(matches!(recipient.send(Ask(1)), Err(ActorError::NoProc)));
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
            let actor = Echo.start();
            let recipient = actor.recipient::<Ask>();
            actor.child_handle().stop();
            actor.join();
            // Past the death point: NoProc (v0.8, decision 4 of otp-exit-signals).
            assert!(matches!(
                actor.request_with_timeout(Ask(1), WAIT),
                Err(ActorError::NoProc)
            ));
            assert!(matches!(
                request(&*recipient, Ask(1), WAIT),
                Err(ActorError::NoProc)
            ));
        },
    );
}

#[test]
fn admitted_requests_fail_when_the_handler_unwinds_before_replying() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::admitted_requests_fail_when_the_handler_unwinds_before_replying"
        ),
        move || {
            let actor = Echo.start();
            assert!(matches!(
                actor.request_with_timeout(Boom, WAIT),
                Err(ActorError::ActorStopped(ExitReason::Panic(_)))
            ));
            let other = Echo.start();
            let recipient = other.recipient::<Boom>();
            assert!(matches!(
                request(&*recipient, Boom, WAIT),
                Err(ActorError::ActorStopped(ExitReason::Panic(_)))
            ));
        },
    );
}

#[test]
fn requests_time_out_while_the_actor_is_busy() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::requests_time_out_while_the_actor_is_busy"
        ),
        move || {
            let actor = Echo.start();
            let recipient = actor.recipient::<Ask>();
            let (release, held) = std_mpsc::channel();
            actor.send(StopAfter(held)).unwrap();
            let short = Duration::from_millis(1);
            assert!(matches!(
                actor.request_with_timeout(Ask(1), short),
                Err(ActorError::RequestTimeout)
            ));
            assert!(matches!(
                request(&*recipient, Ask(1), short),
                Err(ActorError::RequestTimeout)
            ));
            release.send(()).unwrap();
            actor.join();
        },
    );
}

struct Trapper {
    trap: bool,
}
impl Actor for Trapper {
    fn started(&mut self, ctx: &Context<Self>) {
        ctx.trap_exit(self.trap);
    }
}
impl Handler<Ask> for Trapper {
    fn handle(&mut self, msg: Ask, reply: crate::reply::PendingReply<'_, u32>, _: &Context<Self>) {
        reply.reply(msg.0);
    }
}
impl Handler<Unlink> for Trapper {
    fn handle(
        &mut self,
        msg: Unlink,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.unlink(&msg.0);
        reply.reply(());
    }
}

#[test]
fn default_exit_received_keeps_a_trapping_actor_alive() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::default_exit_received_keeps_a_trapping_actor_alive"
        ),
        move || {
            let trapper = Trapper { trap: true }.start();
            assert!(matches!(trapper.request(Ask(0)), Ok(0)));
            let peer = Echo.start();
            trapper.context().link(&peer.child_handle());
            peer.send(Boom).unwrap();
            assert!(matches!(peer.wait_exit(), ExitReason::Panic(_)));
            // The Exit is handled by the default `exit_received`, which ignores it.
            assert!(matches!(trapper.request(Ask(3)), Ok(3)));
            assert!(trapper.exit_reason().is_none());
            trapper
                .child_handle()
                .stop_with_reason(crate::ExitReason::Shutdown);
            assert_eq!(trapper.wait_exit(), ExitReason::Shutdown);
        },
    );
}

#[test]
fn unlink_prevents_signal_delivery() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::unlink_prevents_signal_delivery"),
        move || {
            // Not trapping: a linked peer's panic would take this actor down.
            let trapper = Trapper { trap: false }.start();
            let peer = Echo.start();
            let peer_handle = peer.child_handle();
            trapper.context().link(&peer_handle);
            trapper.request(Unlink(peer_handle.clone())).unwrap();
            // Unlinking yourself or an already-unlinked peer is a no-op.
            trapper.request(Unlink(trapper.child_handle())).unwrap();
            trapper.request(Unlink(peer_handle)).unwrap();
            peer.send(Boom).unwrap();
            assert!(matches!(peer.wait_exit(), ExitReason::Panic(_)));
            assert!(matches!(trapper.request(Ask(1)), Ok(1)));
            assert!(trapper.exit_reason().is_none());
            trapper
                .child_handle()
                .stop_with_reason(crate::ExitReason::Shutdown);
            trapper.join();
        },
    );
}

#[test]
fn opposite_links_between_the_same_pair_do_not_deadlock() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::opposite_links_between_the_same_pair_do_not_deadlock"
        ),
        move || {
            // Each link/unlink locks both link tables. Both directions must take
            // the locks in the same global order, or these two loops deadlock.
            let a = Trapper { trap: true }.start();
            let b = Trapper { trap: true }.start();
            let (done, finished) = std_mpsc::channel();
            let workers: Vec<_> = [
                (a.context(), b.child_handle()),
                (b.context(), a.child_handle()),
            ]
            .into_iter()
            .map(|(own, peer)| {
                let done = done.clone();
                std::thread::spawn(move || {
                    for _ in 0..2_000 {
                        own.link(&peer);
                        own.unlink(&peer);
                    }
                    done.send(()).unwrap();
                })
            })
            .collect();
            for _ in 0..2 {
                finished
                    .recv_timeout(WAIT)
                    .expect("opposite link directions deadlocked");
            }
            for worker in workers {
                worker.join().unwrap();
            }
            for actor in [a, b] {
                actor
                    .child_handle()
                    .stop_with_reason(crate::ExitReason::Shutdown);
                actor.join();
            }
        },
    );
}

#[test]
fn debug_output_names_the_handle() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::debug_output_names_the_handle"),
        move || {
            let actor = Echo.start();
            assert!(format!("{actor:?}").starts_with("ActorRef"));
            assert!(format!("{:?}", actor.context()).starts_with("Context"));
            actor.child_handle().stop();
            actor.join();
        },
    );
}
