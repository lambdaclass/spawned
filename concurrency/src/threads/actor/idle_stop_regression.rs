use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Barrier,
};
struct Counter(Arc<AtomicUsize>, Arc<Mutex<Vec<&'static str>>>);
impl Actor for Counter {
    fn stopped(&mut self, _: &Context<Self>) {
        self.1.lock().unwrap().push("stopped");
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.1.lock().unwrap().push("actor-dropped");
    }
}
struct Increment(Arc<Mutex<Vec<&'static str>>>, MonitorTable);
impl Drop for Increment {
    fn drop(&mut self) {
        assert!(
            self.1.lock().unwrap().is_none(),
            "message dropped before monitor cleanup"
        );
        self.0.lock().unwrap().push("message-dropped");
    }
}
impl Message for Increment {
    type Result = ();
}
impl Handler<Increment> for Counter {
    fn handle(
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
fn stop_while_receiving_does_not_dispatch_a_later_request() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::stop_while_receiving_does_not_dispatch_a_later_request"
        ),
        move || {
            let (tx, rx) = mpsc::channel();
            let token = CancellationToken::new();
            let (_, completion_rx) = spawned_rt::sync::watch::channel(None);
            let monitors = Arc::new(Mutex::new(Some(HashMap::new())));
            let events = Arc::new(Mutex::new(Vec::new()));
            let (id, trap, request) = (
                ActorId::next(),
                new_trap_exit_flag(),
                new_linked_exit_reason(),
            );
            let ctx = Context {
                shared: Arc::new(ActorShared {
                    id,
                    sender: tx.clone(),
                    cancellation_token: token.clone(),
                    completion: completion_rx,
                    monitors: monitors.clone(),
                    trap_exit: trap.clone(),
                    links: new_link_table(),
                    linked_reason: request.clone(),
                    signal: signal_target::<_>(
                        id,
                        None,
                        &tx,
                        trap.clone(),
                        request.clone(),
                        token.clone(),
                        crate::start::StartupCell::new(),
                    ),
                    mailbox: crate::instrumentation::Mailbox::default(),
                    replies: Registry::new_native(ActorId::next(), DEFAULT_MAX_OUTSTANDING),
                    jobs: Arc::new(crate::reply::jobs::JobSet::new(ActorId::next(), 1)),
                    cleaned: Arc::default(),
                }),
            };
            let count = Arc::new(AtomicUsize::new(0));
            let seen = count.clone();
            let entered = Arc::new(Barrier::new(2));
            let release = Arc::new(Barrier::new(2));
            let (e, r) = (entered.clone(), release.clone());
            let cancel = token.clone();
            let trace = events.clone();
            let actor = std::thread::spawn(move || {
                let mut first = true;
                let mut counter = Counter(seen, trace);
                // The envelope received after the stop comes back undispatched;
                // the death point answers it and destroys it before the state.
                let discarded = run_actor_receiving(
                    &mut counter,
                    ctx,
                    move || {
                        // Pause the real receive boundary after the started/cancel
                        // checks; the standard channel still carries the envelope.
                        if first {
                            first = false;
                            e.wait();
                            r.wait();
                        }
                        rx.recv_timeout(Duration::from_secs(5))
                    },
                    cancel,
                );
                drop(discarded);
                drop(counter);
            });
            entered.wait();
            let (slot, response) = reply_slot(crate::reply::NEVER_TICKS, None, false);
            tx.send(MailboxItem::Message(Box::new(MessageEnvelope {
                msg: Increment(events.clone(), monitors),
                slot,
                #[cfg(feature = "metrics")]
                observation: None,
                #[cfg(feature = "lifecycle-trace")]
                from: 0,
            })))
            .unwrap();
            token.cancel();
            release.wait();
            actor.join().unwrap();
            assert_eq!(
                link::lock_request(&request).reason(),
                None,
                "a plain stop request"
            );
            assert_eq!(
                count.load(Ordering::SeqCst),
                0,
                "thread actor dispatched work after idle stop"
            );
            assert!(response.recv().is_err());
            assert_eq!(
                *events.lock().unwrap(),
                ["stopped", "message-dropped", "actor-dropped"]
            );
        },
    );
}
