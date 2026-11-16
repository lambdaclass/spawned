use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
struct Counter(Arc<AtomicUsize>);
impl Actor for Counter {}
struct Increment;
impl Message for Increment {
    type Result = ();
}
impl Handler<Increment> for Counter {
    async fn handle(
        &mut self,
        _: Increment,
        reply: crate::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.0.fetch_add(1, Ordering::SeqCst);
        reply.reply(());
    }
}
#[test]
fn stop_while_idle_does_not_dispatch_a_queued_request_on_resume() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::stop_while_idle_does_not_dispatch_a_queued_request_on_resume"
        ),
        move || {
            rt::Runtime::new().unwrap().block_on(async {
                let (tx, rx) = mpsc::channel();
                let token = StopToken::new();
                let (ctx, mut cleanup) = test_context(&tx, &token);
                let count = Arc::new(AtomicUsize::new(0));
                let mut actor = Box::pin(run_actor(
                    Counter(count.clone()),
                    ctx,
                    rx,
                    token.clone(),
                    &mut cleanup,
                ));
                // Start and park the actual actor loop, without allowing another
                // executor task to run it between admission and stop below.
                assert!(futures::poll!(actor.as_mut()).is_pending());
                let (slot, response) = reply_slot(crate::reply::NEVER_TICKS, None, false);
                tx.send(Box::new(MessageEnvelope {
                    msg: Some(Increment),
                    slot: {
                        let mut slot = slot;
                        slot.take()
                    },
                    #[cfg(feature = "metrics")]
                    observation: None,
                    #[cfg(feature = "lifecycle-trace")]
                    from: 0,
                }) as Box<dyn Envelope<Counter> + Send>)
                    .unwrap();
                token.cancel();
                actor.await;
                assert_eq!(cleanup.die(), ExitReason::Normal);
                assert_eq!(
                    count.load(Ordering::SeqCst),
                    0,
                    "idle actor dispatched queued work after stop"
                );
                assert!(response.await.is_err(), "queued request unexpectedly ran");
            });
        },
    );
}
