use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Idle(mpsc::Sender<()>);
impl Actor for Idle {
    fn stopped(&mut self, _: &Context<Self>) {
        let _ = self.0.send(());
    }
}
struct Noop;
impl Message for Noop {
    type Result = ();
}
impl Handler<Noop> for Idle {
    fn handle(&mut self, _: Noop, reply: crate::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        reply.reply(());
    }
}

#[test]
fn concurrent_cancellation_enqueues_exactly_one_shutdown() {
    for already_cancelled in [false, true] {
        let (tx, rx) = mpsc::channel::<MailboxItem<Idle>>();
        let token = CancellationToken::new();
        if already_cancelled {
            token.cancel();
        }
        register_stop_wakeup(&token, tx.clone());
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let token = &token;
                scope.spawn(move || {
                    for _ in 0..256 {
                        token.cancel();
                    }
                });
            }
        });
        assert!(matches!(rx.try_recv(), Ok(MailboxItem::Shutdown)));
        // Keep tx alive: Empty proves no duplicate, rather than disconnection.
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }
}

#[test]
fn cancellation_wakes_the_real_blocking_receive_boundary() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::cancellation_wakes_the_real_blocking_receive_boundary"
        ),
        move || {
            let (tx, rx) = mpsc::channel();
            let token = CancellationToken::new();
            register_stop_wakeup(&token, tx.clone());
            let (_, completion) = spawned_rt::sync::watch::channel(None);
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
                    completion,
                    monitors: Arc::new(Mutex::new(Some(HashMap::new()))),
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
                        Default::default(),
                    ),
                    mailbox: crate::instrumentation::Mailbox::default(),
                    replies: Registry::new_native(ActorId::next(), DEFAULT_MAX_OUTSTANDING),
                    jobs: Arc::new(crate::reply::jobs::JobSet::new(ActorId::next(), 1)),
                    cleaned: Arc::default(),
                }),
            };
            let (entered_tx, entered) = mpsc::channel();
            let (stopped_tx, stopped) = mpsc::channel();
            let cancel = token.clone();
            let actor = std::thread::spawn(move || {
                run_actor_receiving(
                    &mut Idle(stopped_tx),
                    ctx,
                    move || {
                        let _ = entered_tx.send(());
                        rx.recv()
                            .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
                    },
                    cancel,
                )
            });
            entered.recv_timeout(Duration::from_secs(2)).unwrap();
            token.cancel();
            stopped
                .recv_timeout(Duration::from_secs(2))
                .expect("cancel did not wake recv");
            actor.join().unwrap();
            assert_eq!(
                link::lock_request(&request).reason(),
                None,
                "a plain stop request"
            );
        },
    );
}

struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Failing;
impl Actor for Failing {}
impl Handler<Crash> for Failing {
    fn handle(&mut self, _: Crash, _reply: crate::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        {
            panic!("linked wakeup regression");
        }
    }
}

#[test]
fn every_stop_route_wakes_an_idle_thread() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::every_stop_route_wakes_an_idle_thread"),
        move || {
            for route in 0..4 {
                let (stopped_tx, stopped) = mpsc::channel();
                let actor = Idle(stopped_tx).spawn();
                // A reply ensures startup is complete before the cancellation race.
                actor.request(Noop).unwrap();
                let failing = if route == 3 {
                    let peer = Failing.spawn();
                    actor.context().link(&peer.child_handle());
                    Some(peer)
                } else {
                    None
                };
                match route {
                    0 => actor.context().stop(),
                    1 => actor.child_handle().stop(),
                    2 => actor.shared.cancellation_token.cancel(),
                    _ => failing.as_ref().unwrap().send(Crash).unwrap(),
                }
                if route == 3 {
                    // An untrapped link notice ends the idle actor without
                    // stopped() (decision 2): the wakeup is observed as its exit.
                    let reason = actor.child_handle().wait_exit_blocking();
                    assert!(matches!(reason, ExitReason::Panic(_)), "got {reason:?}");
                    assert!(stopped.try_recv().is_err(), "stopped() ran");
                    failing.unwrap().join();
                    continue;
                }
                let result = stopped.recv_timeout(Duration::from_secs(2));
                // A failed test must not hang cleanup on a broken wakeup callback.
                if result.is_err() {
                    actor.child_handle().stop();
                    let _ = actor.send(Noop);
                }
                assert!(
                    result.is_ok(),
                    "stop route {route} failed to wake the mailbox"
                );
                let reason = actor.wait_exit();
                if route == 3 {
                    assert!(matches!(reason, ExitReason::Panic(_)));
                    failing.unwrap().join();
                } else {
                    assert_eq!(reason, ExitReason::Normal);
                }
            }
        },
    );
}

struct Busy {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    handled: Arc<AtomicUsize>,
}
impl Actor for Busy {}
struct Work;
impl Message for Work {
    type Result = ();
}
impl Handler<Work> for Busy {
    fn handle(&mut self, _: Work, reply: crate::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        self.handled.fetch_add(1, Ordering::SeqCst);
        let _ = self.entered.send(());
        self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        reply.reply(());
    }
}

#[test]
fn repeated_stop_during_a_handler_discards_queued_requests() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::repeated_stop_during_a_handler_discards_queued_requests"
        ),
        move || {
            let (entered_tx, entered) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            let handled = Arc::new(AtomicUsize::new(0));
            let actor = Busy {
                entered: entered_tx,
                release: release_rx,
                handled: handled.clone(),
            }
            .spawn();
            let first = actor.send_request(Work, DEFAULT_REQUEST_TIMEOUT).unwrap();
            entered.recv_timeout(Duration::from_secs(2)).unwrap();
            let replies: Vec<_> = (0..256)
                .map(|_| actor.send_request(Work, DEFAULT_REQUEST_TIMEOUT).unwrap())
                .collect();
            std::thread::scope(|scope| {
                for _ in 0..16 {
                    let child = actor.child_handle();
                    scope.spawn(move || {
                        for _ in 0..256 {
                            child.stop();
                        }
                    });
                }
            });
            release.send(()).unwrap();
            first.recv().unwrap();
            for reply in replies {
                assert!(matches!(
                    reply.recv(),
                    Err(ActorError::ActorStopped(ExitReason::Normal))
                ));
            }
            assert_eq!(actor.wait_exit(), ExitReason::Normal);
            assert_eq!(handled.load(Ordering::SeqCst), 1);
        },
    );
}
