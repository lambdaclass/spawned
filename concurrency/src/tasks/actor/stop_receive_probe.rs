use super::*;
use std::{cell::RefCell, rc::Rc, sync::atomic::AtomicUsize};
thread_local! {
    static INTERLEAVE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}
pub(super) fn before_receive() {
    let action = INTERLEAVE.with(|hook| hook.borrow_mut().take());
    if let Some(action) = action {
        action();
    }
}
struct Count(Arc<AtomicUsize>);
impl Actor for Count {}
struct Ping;
impl Message for Ping {
    type Result = ();
}
impl Handler<Ping> for Count {
    async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
        reply.reply(());
    }
}
#[test]
fn stop_between_check_and_receive_discards_post_stop_request() {
    crate::test_wait::bounded(
        "stop_between_check_and_receive_discards_post_stop_request",
        || {
            rt::Runtime::new().unwrap().block_on(async {
                let (tx, rx) = mpsc::channel();
                let token = StopToken::new();
                let (ctx, mut cleanup) = test_context::<Count>(&tx, &token);
                let hits = Arc::new(AtomicUsize::new(0));
                let response = Rc::new(RefCell::new(None));
                let stop_ctx = ctx.clone();
                let saved = response.clone();
                INTERLEAVE.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        stop_ctx.stop();
                        *saved.borrow_mut() =
                            Some(stop_ctx.send_request(Ping, Deadline::Never).unwrap());
                    }))
                });
                run_actor(Count(hits.clone()), ctx, rx, token, &mut cleanup).await;
                assert_eq!(cleanup.die(), ExitReason::Normal);
                let response = response
                    .borrow_mut()
                    .take()
                    .expect("receive probe was reached");
                let reply = response.await;
                assert!(
                    matches!(reply, Err(ActorError::ActorStopped(ExitReason::Normal))),
                    "post-stop request was handled: {reply:?}"
                );
                assert_eq!(hits.load(Ordering::SeqCst), 0);
            });
        },
    );
}

struct CheckDrop {
    owner: Arc<Registry>,
    response: Arc<std::sync::Mutex<Option<ReplyReceiver<()>>>>,
    checked: Arc<std::sync::atomic::AtomicBool>,
}
impl Message for CheckDrop {
    type Result = ();
}
impl Drop for CheckDrop {
    fn drop(&mut self) {
        let reply = self
            .response
            .lock()
            .unwrap()
            .as_mut()
            .and_then(ReplyReceiver::try_recv);
        let after_death = match reply {
            Some(Err(ActorError::ActorStopped(ref reason))) => {
                self.owner.final_reason() == Some(reason)
            }
            _ => false,
        };
        self.checked.store(after_death, Ordering::SeqCst);
    }
}
struct CleanupPause {
    hits: Arc<AtomicUsize>,
    park: bool,
}
impl Actor for CleanupPause {
    async fn stopped(&mut self, _: &Context<Self>) {
        if self.park {
            std::future::pending::<()>().await;
        }
    }
}
impl Handler<CheckDrop> for CleanupPause {
    async fn handle(&mut self, _msg: CheckDrop, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        self.hits.fetch_add(1, Ordering::SeqCst);
        reply.reply(());
    }
}
#[test]
fn receive_rejected_by_stop_keeps_its_caller_until_the_death_point() {
    crate::test_wait::bounded(
        "receive_rejected_by_stop_keeps_its_caller_until_the_death_point",
        || {
            for (park, requested) in [
                (false, ExitReason::Normal),
                (true, ExitReason::Normal),
                (false, ExitReason::Shutdown),
                (true, ExitReason::Shutdown),
                (false, ExitReason::Panic("requested".into())),
                (true, ExitReason::Panic("requested".into())),
                (false, ExitReason::Kill),
                (true, ExitReason::Kill),
            ] {
                rt::Runtime::new().unwrap().block_on(async {
                    let (tx, rx) = mpsc::channel();
                    let token = StopToken::new();
                    let (ctx, mut cleanup) = test_context::<CleanupPause>(&tx, &token);
                    let hits = Arc::new(AtomicUsize::new(0));
                    let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let response = Arc::new(std::sync::Mutex::new(None));
                    let stop_ctx = ctx.clone();
                    let probe = CheckDrop {
                        owner: ctx.shared.replies.clone(),
                        response: response.clone(),
                        checked: checked.clone(),
                    };
                    let stop_reason = requested.clone();
                    INTERLEAVE.with(|hook| {
                        *hook.borrow_mut() = Some(Box::new(move || {
                            stop_ctx.stop_with_reason(stop_reason);
                            let reply = stop_ctx.send_request(probe, Deadline::Never).unwrap();
                            *response.lock().unwrap() = Some(reply);
                        }))
                    });
                    let mut actor = Box::pin(run_actor(
                        CleanupPause {
                            hits: hits.clone(),
                            park,
                        },
                        ctx,
                        rx,
                        token,
                        &mut cleanup,
                    ));
                    let expected = if park {
                        assert!(futures::poll!(actor.as_mut()).is_pending());
                        assert!(
                            !checked.load(Ordering::SeqCst),
                            "payload was dropped before stopped() finished"
                        );
                        drop(actor);
                        if requested == ExitReason::Normal {
                            ExitReason::Killed
                        } else {
                            requested
                        }
                    } else {
                        actor.await;
                        requested
                    };
                    assert_eq!(cleanup.die(), expected);
                    assert_eq!(hits.load(Ordering::SeqCst), 0);
                    assert!(
                        checked.load(Ordering::SeqCst),
                        "held caller was not answered before payload destruction"
                    );
                });
            }
        },
    );
}
