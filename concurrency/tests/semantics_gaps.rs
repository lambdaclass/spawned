//! Guarantees from `docs/design/semantics.md` that had no dedicated check:
//! X7 (trapped `Exit` and `Down` follow the source's earlier messages), T1/T2
//! (`send_after` and fixed-delay `send_interval` under paused Tokio time), T3
//! (timers stay with their instance across a supervised restart) and R5 (a
//! panicking request handler gives its caller exactly `ActorStopped`).
//! Every wait is event-driven and bounded; no sleep is used to synchronize.
use spawned_concurrency::{message::Message, ActorError, ChildHandle, Down, Exit, ExitReason};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(10);
const PINGS: usize = 64;
const ROUNDS: usize = 10;

#[derive(Debug, PartialEq)]
enum Event {
    Ping(usize),
    Exit(ExitReason),
    Down(ExitReason),
}

struct Ping(usize);
impl Message for Ping {
    type Result = ();
}
struct Watch(ChildHandle);
impl Message for Watch {
    type Result = ();
}
/// Sent to the source: send `count` pings, then end with `reason`
/// (`None`: panic in the handler).
struct Burst<R> {
    to: R,
    count: usize,
    reason: Option<ExitReason>,
}
impl<R: Send + 'static> Message for Burst<R> {
    type Result = ();
}
struct Boom;
impl Message for Boom {
    type Result = u64;
}

fn reasons() -> Vec<Option<ExitReason>> {
    vec![
        Some(ExitReason::Normal),
        Some(ExitReason::Shutdown),
        Some(ExitReason::Kill),
        Some(ExitReason::Panic("chosen".into())),
        None,
    ]
}

/// All pings in send order, and both notices after the last ping.
fn check_order(events: &[Event], reason: &Option<ExitReason>) {
    let pings: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            Event::Ping(i) => Some(*i),
            _ => None,
        })
        .collect();
    assert_eq!(pings, (0..PINGS).collect::<Vec<_>>(), "per-sender FIFO");
    let last_ping = events
        .iter()
        .rposition(|e| matches!(e, Event::Ping(_)))
        .unwrap();
    let exit = events
        .iter()
        .position(|e| matches!(e, Event::Exit(_)))
        .expect("trapped Exit");
    let down = events
        .iter()
        .position(|e| matches!(e, Event::Down(_)))
        .expect("Down");
    assert!(exit > last_ping, "Exit overtook a ping: {events:?}");
    assert!(down > last_ping, "Down overtook a ping: {events:?}");
    let observed = match &events[exit] {
        Event::Exit(r) => r.clone(),
        _ => unreachable!(),
    };
    assert_eq!(events[down], Event::Down(observed.clone()));
    match reason {
        Some(expected) => assert_eq!(&observed, expected),
        None => assert!(
            matches!(&observed, ExitReason::Panic(m) if m.contains("source panicked")),
            "{observed:?}"
        ),
    }
}

mod native {
    use super::*;
    use spawned_concurrency::threads::{request, Actor, ActorStart, Context, Handler, Recipient};
    use std::sync::mpsc;

    struct Observer(mpsc::Sender<Event>);
    impl Actor for Observer {
        fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            let _ = self.0.send(Event::Exit(exit.reason));
        }
    }
    impl Handler<Watch> for Observer {
        fn handle(
            &mut self,
            watch: Watch,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            {
                ctx.trap_exit(true);
                ctx.link(&watch.0);
                ctx.monitor(&watch.0);
            }
            let _ = reply.reply(());
        }
    }
    impl Handler<Ping> for Observer {
        fn handle(
            &mut self,
            ping: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                let _ = self.0.send(Event::Ping(ping.0));
            }
            let _ = reply.reply(());
        }
    }
    impl Handler<Down> for Observer {
        fn handle(
            &mut self,
            down: Down,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                let _ = self.0.send(Event::Down(down.reason));
            }
            let _ = reply.reply(());
        }
    }

    struct Source;
    impl Actor for Source {}
    impl Handler<Burst<Recipient<Ping>>> for Source {
        fn handle(
            &mut self,
            burst: Burst<Recipient<Ping>>,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            {
                for i in 0..burst.count {
                    burst.to.send(Ping(i)).unwrap();
                }
                match burst.reason {
                    Some(reason) => ctx.stop_with_reason(reason),
                    None => panic!("source panicked"),
                }
            }
            let _ = reply.reply(());
        }
    }

    #[test]
    fn exit_and_down_follow_earlier_messages_from_the_source() {
        for reason in reasons() {
            for _ in 0..ROUNDS {
                let (tx, rx) = mpsc::channel();
                let observer = Observer(tx).start();
                let source = Source.start();
                observer
                    .request_with_timeout(Watch(source.child_handle()), LIMIT)
                    .unwrap();
                source
                    .send(Burst {
                        to: observer.recipient::<Ping>(),
                        count: PINGS,
                        reason: reason.clone(),
                    })
                    .unwrap();
                let mut events = Vec::new();
                while events.len() < PINGS + 2 {
                    events.push(rx.recv_timeout(LIMIT).expect("missing event"));
                }
                check_order(&events, &reason);
                observer.child_handle().stop();
                observer.join();
            }
        }
    }

    struct Panicker;
    impl Actor for Panicker {}
    impl Handler<Boom> for Panicker {
        fn handle(
            &mut self,
            _: Boom,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            let __reply_value: u64 = { panic!("request handler panic") };
            let _ = reply.reply(__reply_value);
        }
    }

    #[test]
    fn panicking_request_handler_gives_the_caller_actor_stopped() {
        let actor = Panicker.start();
        let result = actor.request_with_timeout(Boom, LIMIT);
        assert!(
            matches!(result, Err(ActorError::ActorStopped)),
            "{result:?}"
        );
        assert!(matches!(actor.wait_exit(), ExitReason::Panic(_)));

        let erased = Panicker.start();
        let recipient: Recipient<Boom> = erased.recipient();
        let result = request(&*recipient, Boom, LIMIT);
        assert!(
            matches!(result, Err(ActorError::ActorStopped)),
            "{result:?}"
        );

        let raw = Panicker.start();
        let receiver = raw.request_raw(Boom).unwrap();
        assert!(receiver
            .recv_timeout(LIMIT)
            .is_err_and(|e| { e == std::sync::mpsc::RecvTimeoutError::Disconnected }));
    }
}

// Not simulated: drives Tokio runtimes directly (multi-thread workers and the
// paused Tokio clock via `tokio::time`), outside the simulator's control.
#[cfg(all(feature = "async", not(spawned_sim)))]
mod tasks {
    use super::*;
    use spawned_concurrency::{
        supervision::{Backoff, ChildSpec, RestartMode, SupervisorConfig},
        system::ActorSystem,
        tasks::{
            request, send_after, send_interval, Actor, ActorStart, Backend, Context, Handler,
            Recipient,
        },
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
    use tokio::time::{timeout, Instant};

    async fn next<T>(rx: &mut UnboundedReceiver<T>) -> T {
        timeout(LIMIT, rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed")
    }

    struct Observer(UnboundedSender<Event>);
    impl Actor for Observer {
        async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            let _ = self.0.send(Event::Exit(exit.reason));
        }
    }
    impl Handler<Watch> for Observer {
        async fn handle(
            &mut self,
            watch: Watch,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            {
                ctx.trap_exit(true);
                ctx.link(&watch.0);
                ctx.monitor(&watch.0);
            }
            let _ = reply.reply(());
        }
    }
    impl Handler<Ping> for Observer {
        async fn handle(
            &mut self,
            ping: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                let _ = self.0.send(Event::Ping(ping.0));
            }
            let _ = reply.reply(());
        }
    }
    impl Handler<Down> for Observer {
        async fn handle(
            &mut self,
            down: Down,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                let _ = self.0.send(Event::Down(down.reason));
            }
            let _ = reply.reply(());
        }
    }

    struct Source;
    impl Actor for Source {}
    impl Handler<Burst<Recipient<Ping>>> for Source {
        async fn handle(
            &mut self,
            burst: Burst<Recipient<Ping>>,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            {
                for i in 0..burst.count {
                    burst.to.send(Ping(i)).unwrap();
                }
                match burst.reason {
                    Some(reason) => ctx.stop_with_reason(reason),
                    None => panic!("source panicked"),
                }
            }
            let _ = reply.reply(());
        }
    }

    const BACKENDS: [Backend; 3] = [Backend::Async, Backend::Blocking, Backend::Thread];

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exit_and_down_follow_earlier_messages_from_the_source() {
        for backend in BACKENDS {
            for reason in reasons() {
                for _ in 0..ROUNDS {
                    let (tx, mut rx) = unbounded_channel();
                    let observer = Observer(tx).start_with_backend(backend);
                    let source = Source.start_with_backend(backend);
                    observer
                        .request_with_timeout(Watch(source.child_handle()), LIMIT)
                        .await
                        .unwrap();
                    source
                        .send(Burst {
                            to: observer.recipient::<Ping>(),
                            count: PINGS,
                            reason: reason.clone(),
                        })
                        .unwrap();
                    let mut events = Vec::new();
                    while events.len() < PINGS + 2 {
                        events.push(next(&mut rx).await);
                    }
                    check_order(&events, &reason);
                    observer.child_handle().stop();
                    timeout(LIMIT, observer.join()).await.unwrap();
                }
            }
        }
    }

    #[derive(Clone)]
    struct Tick;
    impl Message for Tick {
        type Result = ();
    }
    struct Ticker(UnboundedSender<Instant>);
    impl Actor for Ticker {}
    impl Handler<Tick> for Ticker {
        async fn handle(
            &mut self,
            _: Tick,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                let _ = self.0.send(Instant::now());
            }
            let _ = reply.reply(());
        }
    }

    const PERIOD: Duration = Duration::from_millis(100);

    /// T1: under paused time the delay is exact and the message arrives once.
    #[tokio::test(start_paused = true)]
    async fn send_after_fires_once_no_earlier_than_its_delay() {
        let (tx, mut rx) = unbounded_channel();
        let actor = Ticker(tx).start();
        let start = Instant::now();
        let _timer = send_after(PERIOD, actor.context(), Tick);
        let fired = next(&mut rx).await;
        assert_eq!(fired - start, PERIOD);
        // Paused time auto-advances while idle, so this waits ten periods of
        // virtual time without a wall-clock sleep.
        assert!(
            timeout(PERIOD * 10, rx.recv()).await.is_err(),
            "fired twice"
        );
    }

    /// T2: after a clock jump of 3.5 periods the interval fires once (no
    /// burst), then every period measured from that late send (fixed delay),
    /// so the ticks stay half a period off the original grid.
    #[tokio::test(start_paused = true)]
    async fn send_interval_is_fixed_delay_and_never_catches_up() {
        let (tx, mut rx) = unbounded_channel();
        let actor = Ticker(tx).start();
        let start = Instant::now();
        let _timer = send_interval(PERIOD, actor.context(), Tick);
        tokio::task::yield_now().await;
        tokio::time::advance(PERIOD * 3 + PERIOD / 2).await;
        let mut ticks = Vec::new();
        for _ in 0..4 {
            ticks.push(next(&mut rx).await - start);
        }
        // The first tick is at the jump (3.5 P) if the timer had armed its
        // sleep before the jump, otherwise one period later; never earlier.
        assert!(
            ticks[0] == PERIOD * 3 + PERIOD / 2 || ticks[0] == PERIOD * 4 + PERIOD / 2,
            "{ticks:?}"
        );
        for pair in ticks.windows(2) {
            assert_eq!(pair[1] - pair[0], PERIOD, "burst or drift lost: {ticks:?}");
        }
        for tick in &ticks {
            assert_eq!(
                tick.as_nanos() % PERIOD.as_nanos(),
                (PERIOD / 2).as_nanos(),
                "realigned to the original schedule: {ticks:?}"
            );
        }
    }

    #[derive(Clone)]
    struct Stamp(u64);
    impl Message for Stamp {
        type Result = ();
    }
    struct Crash;
    impl Message for Crash {
        type Result = ();
    }
    /// Starts an interval tagged with its own generation; the first generation
    /// ticks five times faster, so a retargeted timer could not hide.
    struct Clocked {
        generation: u64,
        first: bool,
        log: UnboundedSender<(u64, u64)>,
    }
    impl Actor for Clocked {
        async fn started(&mut self, ctx: &Context<Self>) {
            let period = if self.first {
                Duration::from_millis(2)
            } else {
                Duration::from_millis(10)
            };
            send_interval(period, ctx.clone(), Stamp(self.generation));
        }
    }
    impl Handler<Stamp> for Clocked {
        async fn handle(
            &mut self,
            stamp: Stamp,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                let _ = self.log.send((self.generation, stamp.0));
            }
            let _ = reply.reply(());
        }
    }
    impl Handler<Crash> for Clocked {
        async fn handle(
            &mut self,
            _: Crash,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                panic!("injected crash");
            }
            let _ = reply.reply(());
        }
    }

    /// T3: the first generation's timer never delivers to its replacement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timers_are_not_carried_to_a_supervised_replacement() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("timers", 1).unwrap();
        let config = SupervisorConfig {
            backoff: Backoff {
                initial: Duration::from_millis(1),
                maximum: Duration::from_millis(1),
                jitter: false,
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        };
        let (tx, mut rx) = unbounded_channel();
        let first = Arc::new(AtomicBool::new(true));
        let mut tree = system.supervisor(config).unwrap();
        let child = tree
            .add(
                ChildSpec::new("clocked", &group, move |ctx| {
                    let log = tx.clone();
                    let first = first.swap(false, Ordering::SeqCst);
                    async move {
                        Ok::<_, String>(Clocked {
                            generation: ctx.generation(),
                            first,
                            log,
                        })
                    }
                })
                .restart(RestartMode::Permanent),
            )
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        let old_generation = child.snapshot().generation;
        // The first interval is running.
        for _ in 0..3 {
            let (receiver, origin) = next(&mut rx).await;
            assert_eq!((receiver, origin), (old_generation, old_generation));
        }
        let old = child.current().unwrap();
        assert!(old.request(Crash).await.is_err());
        let new = timeout(LIMIT, child.wait_for_generation(old_generation))
            .await
            .unwrap()
            .unwrap();
        let new_generation = child.snapshot().generation;
        assert!(new_generation > old_generation);
        // Stop at the fifth tick of the replacement's own (slower) interval;
        // in that time the old one would have fired about 25 times.
        let mut own = 0;
        while own < 5 {
            let (receiver, origin) = next(&mut rx).await;
            assert_eq!(receiver, origin, "a timer crossed generations");
            if receiver == new_generation {
                own += 1;
            } else {
                assert_eq!(receiver, old_generation);
            }
        }
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert_eq!(
            new.child_handle().wait_exit_async().await,
            ExitReason::Shutdown
        );
        assert!(system.shutdown(LIMIT).await.complete);
    }

    struct Panicker;
    impl Actor for Panicker {}
    impl Handler<Boom> for Panicker {
        async fn handle(
            &mut self,
            _: Boom,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            let __reply_value: u64 = { panic!("request handler panic") };
            let _ = reply.reply(__reply_value);
        }
    }

    /// R5 on every backend and on a managed group: the exact caller error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panicking_request_handler_gives_the_caller_actor_stopped() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("panics", 1).unwrap();
        let mut actors = BACKENDS
            .iter()
            .map(|backend| Panicker.start_with_backend(*backend))
            .collect::<Vec<_>>();
        actors.push(Panicker.start_on(&group).unwrap());
        for actor in actors {
            let result = actor.request_with_timeout(Boom, LIMIT).await;
            assert!(
                matches!(result, Err(ActorError::ActorStopped)),
                "{result:?}"
            );
            let reason = timeout(LIMIT, actor.wait_exit()).await.unwrap();
            assert!(matches!(reason, ExitReason::Panic(_)), "{reason:?}");
        }

        let erased = Panicker.start();
        let recipient: Recipient<Boom> = erased.recipient();
        let result = request(&*recipient, Boom, LIMIT).await;
        assert!(
            matches!(result, Err(ActorError::ActorStopped)),
            "{result:?}"
        );

        let raw = Panicker.start();
        let receiver = raw.request_raw(Boom).unwrap();
        assert!(timeout(LIMIT, receiver).await.unwrap().is_err());
        assert!(system.shutdown(LIMIT).await.complete);
    }
}
