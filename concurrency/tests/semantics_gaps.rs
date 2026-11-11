//! Guarantees from `docs/design/semantics.md` that had no dedicated check:
//! X7 (trapped `Exit` and `Down` follow the source's earlier messages), T1/T2
//! (`send_after` and fixed-delay `send_interval` under paused Tokio time), T3
//! (timers stay with their instance across a supervised restart) and R5 (a
//! panicking request handler gives its caller exactly `ActorStopped`).
//!
//! The `*_contract` modules close the gaps found by the coverage audit in
//! `docs/TESTING.md` ("Semantics coverage"): D3/D4/L7 (the stop window), D5,
//! M1/L1, M4, M5, L2, L5, R3, T3 and S6 on every placement, and V2, V4, V6,
//! V9, S1 and S3 on the simulator.
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
            ctx.trap_exit(true);
            ctx.link(&watch.0);
            ctx.monitor(&watch.0);
            reply.reply(());
        }
    }
    impl Handler<Ping> for Observer {
        fn handle(
            &mut self,
            ping: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let _ = self.0.send(Event::Ping(ping.0));
            reply.reply(());
        }
    }
    impl Handler<Down> for Observer {
        fn handle(
            &mut self,
            down: Down,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let _ = self.0.send(Event::Down(down.reason));
            reply.reply(());
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
            for i in 0..burst.count {
                burst.to.send(Ping(i)).unwrap();
            }
            match burst.reason {
                Some(reason) => ctx.stop_with_reason(reason),
                None => panic!("source panicked"),
            }
            reply.reply(());
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
            _reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            panic!("request handler panic");
        }
    }

    #[test]
    fn panicking_request_handler_gives_the_caller_actor_stopped() {
        let actor = Panicker.start();
        let result = actor.request_with_timeout(Boom, LIMIT);
        assert!(
            matches!(result, Err(ActorError::ActorStopped(ExitReason::Panic(_)))),
            "{result:?}"
        );
        assert!(matches!(actor.wait_exit(), ExitReason::Panic(_)));

        let erased = Panicker.start();
        let recipient: Recipient<Boom> = erased.recipient();
        let result = request(&*recipient, Boom, LIMIT);
        assert!(
            matches!(result, Err(ActorError::ActorStopped(ExitReason::Panic(_)))),
            "{result:?}"
        );

        let raw = Panicker.start();
        let receiver = raw
            .send_request(Boom, spawned_concurrency::Deadline::after(LIMIT))
            .unwrap();
        assert!(matches!(
            receiver.recv(),
            Err(ActorError::ActorStopped(ExitReason::Panic(_)))
        ));
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
            ctx.trap_exit(true);
            ctx.link(&watch.0);
            ctx.monitor(&watch.0);
            reply.reply(());
        }
    }
    impl Handler<Ping> for Observer {
        async fn handle(
            &mut self,
            ping: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let _ = self.0.send(Event::Ping(ping.0));
            reply.reply(());
        }
    }
    impl Handler<Down> for Observer {
        async fn handle(
            &mut self,
            down: Down,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let _ = self.0.send(Event::Down(down.reason));
            reply.reply(());
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
            for i in 0..burst.count {
                burst.to.send(Ping(i)).unwrap();
            }
            match burst.reason {
                Some(reason) => ctx.stop_with_reason(reason),
                None => panic!("source panicked"),
            }
            reply.reply(());
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
            let _ = self.0.send(Instant::now());
            reply.reply(());
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
            let _ = self.log.send((self.generation, stamp.0));
            reply.reply(());
        }
    }
    impl Handler<Crash> for Clocked {
        async fn handle(
            &mut self,
            _: Crash,
            _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                panic!("injected crash");
            }
        }
    }

    /// T3: the first generation's timer never delivers to its replacement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timers_are_not_carried_to_a_supervised_replacement() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("timers", 1).unwrap();
        let config = SupervisorConfig::default().backoff(
            Backoff::default()
                .initial(Duration::from_millis(1))
                .maximum(Duration::from_millis(1))
                .jitter(false),
        );
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
            _reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            panic!("request handler panic");
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
                matches!(result, Err(ActorError::ActorStopped(ExitReason::Panic(_)))),
                "{result:?}"
            );
            let reason = timeout(LIMIT, actor.wait_exit()).await.unwrap();
            assert!(matches!(reason, ExitReason::Panic(_)), "{reason:?}");
        }

        let erased = Panicker.start();
        let recipient: Recipient<Boom> = erased.recipient();
        let result = request(&*recipient, Boom, LIMIT).await;
        assert!(
            matches!(result, Err(ActorError::ActorStopped(ExitReason::Panic(_)))),
            "{result:?}"
        );

        let raw = Panicker.start();
        let receiver = raw
            .send_request(Boom, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
            .unwrap();
        assert!(timeout(LIMIT, receiver).await.unwrap().is_err());
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

// ---------------------------------------------------------------------------
// Coverage-matrix gaps (docs/TESTING.md, "Semantics coverage"): D3, D4, D5,
// L1, L7, M1, M4, M5 on every placement.
// ---------------------------------------------------------------------------

/// Counts its own destruction.
struct Payload(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
struct Queued(#[allow(dead_code)] Payload);
impl Message for Queued {
    type Result = ();
}
struct Asked(#[allow(dead_code)] Payload);
impl Message for Asked {
    type Result = u64;
}
struct Seq(usize);
impl Message for Seq {
    type Result = ();
}
struct Count;
impl Message for Count {
    type Result = Vec<usize>;
}
struct Work(usize);
impl Message for Work {
    type Result = usize;
}
struct Loop;
impl Message for Loop {
    type Result = ();
}

/// Messages queued while the stop window is open: half sends, half requests.
const WINDOW: usize = 8;
/// Backlog behind a blocked handler (D5).
const BACKLOG: usize = 100_000;

#[derive(Clone, Debug, PartialEq)]
enum Turn {
    StartedBegin,
    StartedEnd,
    Begin(usize),
    End(usize),
    StoppedBegin,
    StoppedEnd,
}

/// L1/M1: `started` first, then whole turns, then `stopped`; every handled
/// id once; the read-modify-write counter equals the number of turns; a
/// request succeeded exactly when its turn ran.
fn check_turns(events: &[Turn], counter: usize, outcomes: &[(usize, Result<usize, ActorError>)]) {
    assert!(events.len() >= 4, "{events:?}");
    assert_eq!(
        events[..2],
        [Turn::StartedBegin, Turn::StartedEnd],
        "L1: {events:?}"
    );
    let n = events.len();
    assert_eq!(
        events[n - 2..],
        [Turn::StoppedBegin, Turn::StoppedEnd],
        "L1: {events:?}"
    );
    let body = &events[2..n - 2];
    assert_eq!(body.len() % 2, 0, "M1: a turn was split: {events:?}");
    let mut handled = Vec::new();
    for pair in body.chunks(2) {
        match pair {
            [Turn::Begin(a), Turn::End(b)] if a == b => handled.push(*a),
            _ => panic!("M1/L1: interleaved or misplaced turn {pair:?}"),
        }
    }
    assert_eq!(counter, handled.len(), "M1: lost update across a turn");
    let mut unique = handled.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), handled.len(), "D2: a message ran twice");
    for (id, outcome) in outcomes {
        match outcome {
            Ok(reply) => {
                assert_eq!(reply, id, "R2");
                assert!(handled.contains(id), "R1: Ok without a turn for {id}");
            }
            Err(ActorError::ActorStopped(_)) => {
                assert!(!handled.contains(id), "R1: {id} ran but got ActorStopped")
            }
            // Sent after the death point (0.8: `NoProc`, never admitted).
            Err(ActorError::NoProc) => {
                assert!(!handled.contains(id), "R1: {id} ran but got NoProc")
            }
            Err(other) => panic!("R1: unexpected {other:?} for {id}"),
        }
    }
}

mod native_contract {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    };

    struct Hold {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Message for Hold {
        type Result = ();
    }

    struct Holder {
        seen: Arc<Mutex<Vec<&'static str>>>,
        drops: Arc<AtomicUsize>,
        send_in_stopped: Arc<Mutex<Option<bool>>>,
    }
    impl Actor for Holder {
        fn stopped(&mut self, ctx: &Context<Self>) {
            self.seen.lock().unwrap().push("stopped");
            let accepted = ctx.send(Queued(Payload(self.drops.clone()))).is_ok();
            *self.send_in_stopped.lock().unwrap() = Some(accepted);
        }
    }
    impl Handler<Hold> for Holder {
        fn handle(
            &mut self,
            hold: Hold,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = {
                self.seen.lock().unwrap().push("hold");
                hold.entered.send(()).unwrap();
                let _ = hold.release.recv_timeout(LIMIT);
            };
            __reply.reply(__value);
        }
    }
    impl Handler<Queued> for Holder {
        fn handle(
            &mut self,
            _: Queued,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = {
                self.seen.lock().unwrap().push("queued");
            };
            __reply.reply(__value);
        }
    }
    impl Handler<Asked> for Holder {
        fn handle(
            &mut self,
            _: Asked,
            __reply: spawned_concurrency::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            let __value: u64 = {
                self.seen.lock().unwrap().push("asked");
                1
            };
            __reply.reply(__value);
        }
    }

    /// D3, D4, L7: after a stop request, sends (also from `stopped()`) are
    /// still accepted, none of them is handled, and by the time `join()`
    /// returns every payload was dropped and every reply sender released.
    #[test]
    fn stop_window_accepts_then_discards_and_releases_before_join() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let drops = Arc::new(AtomicUsize::new(0));
        let send_in_stopped = Arc::new(Mutex::new(None));
        let actor = Holder {
            seen: seen.clone(),
            drops: drops.clone(),
            send_in_stopped: send_in_stopped.clone(),
        }
        .start();
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        actor
            .send(Hold {
                entered,
                release: release_rx,
            })
            .unwrap();
        entered_rx.recv_timeout(LIMIT).unwrap();
        actor.child_handle().stop();
        let mut replies = Vec::new();
        for _ in 0..WINDOW / 2 {
            actor.send(Queued(Payload(drops.clone()))).expect("D3");
            replies.push(
                actor
                    .send_request(
                        Asked(Payload(drops.clone())),
                        spawned_concurrency::DEFAULT_REQUEST_TIMEOUT,
                    )
                    .expect("D3"),
            );
        }
        release.send(()).unwrap();
        actor.join();
        assert_eq!(
            drops.load(Ordering::SeqCst),
            WINDOW + 1,
            "D4/L7: a queued payload outlived join()"
        );
        for mut reply in replies {
            assert!(
                matches!(reply.try_recv(), Some(Err(ActorError::ActorStopped(_)))),
                "D4/L7: a queued reply sender outlived join()"
            );
        }
        assert_eq!(*seen.lock().unwrap(), ["hold", "stopped"], "D4");
        assert_eq!(
            *send_in_stopped.lock().unwrap(),
            Some(true),
            "D3: send in stopped()"
        );
        assert_eq!(actor.wait_exit(), ExitReason::Normal);
        assert!(matches!(
            actor.send(Queued(Payload(drops.clone()))),
            Err(ActorError::NoProc)
        ));
    }

    struct Turns {
        log: Arc<Mutex<Vec<Turn>>>,
        counter: usize,
        final_counter: Arc<AtomicUsize>,
    }
    impl Turns {
        fn push(&self, turn: Turn) {
            self.log.lock().unwrap().push(turn);
        }
    }
    impl Actor for Turns {
        fn started(&mut self, _: &Context<Self>) {
            self.push(Turn::StartedBegin);
            std::thread::yield_now();
            self.push(Turn::StartedEnd);
        }
        fn stopped(&mut self, _: &Context<Self>) {
            self.push(Turn::StoppedBegin);
            self.final_counter.store(self.counter, Ordering::SeqCst);
            std::thread::yield_now();
            self.push(Turn::StoppedEnd);
        }
    }
    impl Handler<Work> for Turns {
        fn handle(
            &mut self,
            Work(id): Work,
            __reply: spawned_concurrency::PendingReply<'_, usize>,
            _: &Context<Self>,
        ) {
            let __value: usize = {
                self.push(Turn::Begin(id));
                let seen = self.counter;
                std::thread::yield_now();
                self.counter = seen + 1;
                self.push(Turn::End(id));
                id
            };
            __reply.reply(__value);
        }
    }

    /// L1, M1: requests from several threads race `started()` and a stop.
    #[test]
    fn hooks_and_turns_never_overlap_under_concurrent_requests_and_stop() {
        for _ in 0..ROUNDS {
            let log = Arc::new(Mutex::new(Vec::new()));
            let final_counter = Arc::new(AtomicUsize::new(usize::MAX));
            let actor = Turns {
                log: log.clone(),
                counter: 0,
                final_counter: final_counter.clone(),
            }
            .start();
            let replied = Arc::new(AtomicUsize::new(0));
            let senders: Vec<_> = (0..4)
                .map(|t| {
                    let actor = actor.clone();
                    let replied = replied.clone();
                    std::thread::spawn(move || {
                        (0..16)
                            .map(|k| {
                                let id = t * 100 + k;
                                let outcome = actor.request_with_timeout(Work(id), LIMIT);
                                replied.fetch_add(1, Ordering::SeqCst);
                                (id, outcome)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            while replied.load(Ordering::SeqCst) < 24 {
                std::thread::yield_now();
            }
            actor.child_handle().stop();
            let outcomes: Vec<_> = senders
                .into_iter()
                .flat_map(|s| s.join().unwrap())
                .collect();
            actor.join();
            let events = log.lock().unwrap().clone();
            check_turns(&events, final_counter.load(Ordering::SeqCst), &outcomes);
        }
    }

    /// D5, D1, D2: a large backlog behind a blocked handler is accepted
    /// without blocking the sender, then handled in order, exactly once.
    #[test]
    fn unbounded_mailbox_accepts_a_large_backlog_without_blocking() {
        struct Backlog(Vec<usize>);
        impl Actor for Backlog {}
        impl Handler<Hold> for Backlog {
            fn handle(
                &mut self,
                hold: Hold,
                __reply: spawned_concurrency::PendingReply<'_, ()>,
                _: &Context<Self>,
            ) {
                let __value: () = {
                    hold.entered.send(()).unwrap();
                    let _ = hold.release.recv_timeout(LIMIT * 6);
                };
                __reply.reply(__value);
            }
        }
        impl Handler<Seq> for Backlog {
            fn handle(
                &mut self,
                Seq(i): Seq,
                __reply: spawned_concurrency::PendingReply<'_, ()>,
                _: &Context<Self>,
            ) {
                let __value: () = {
                    self.0.push(i);
                };
                __reply.reply(__value);
            }
        }
        impl Handler<Count> for Backlog {
            fn handle(
                &mut self,
                _: Count,
                __reply: spawned_concurrency::PendingReply<'_, Vec<usize>>,
                _: &Context<Self>,
            ) {
                let __value: Vec<usize> = std::mem::take(&mut self.0);
                __reply.reply(__value);
            }
        }
        let actor = Backlog(Vec::new()).start();
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        actor
            .send(Hold {
                entered,
                release: release_rx,
            })
            .unwrap();
        entered_rx.recv_timeout(LIMIT).unwrap();
        let sender = actor.clone();
        let (done, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for i in 0..BACKLOG {
                sender.send(Seq(i)).expect("D5: send failed for capacity");
            }
            done.send(()).unwrap();
        });
        // The handler is still blocked, so a bounded mailbox would block here.
        done_rx
            .recv_timeout(LIMIT * 6)
            .expect("D5: sends blocked behind a busy handler");
        release.send(()).unwrap();
        let handled = actor.request_with_timeout(Count, LIMIT * 6).unwrap();
        assert_eq!(handled, (0..BACKLOG).collect::<Vec<_>>(), "D1/D2");
        actor.child_handle().stop();
        actor.join();
    }

    struct Looper {
        reports: mpsc::Sender<usize>,
        count: usize,
    }
    impl Actor for Looper {}
    impl Handler<Loop> for Looper {
        fn handle(
            &mut self,
            _: Loop,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            let __value: () = {
                self.count += 1;
                let _ = self.reports.send(self.count);
                if self.count < 100 {
                    ctx.send(Loop).unwrap();
                }
            };
            __reply.reply(__value);
        }
    }

    /// M5: dropping every `ActorRef` does not stop the actor.
    #[test]
    fn dropping_every_actor_ref_keeps_the_actor_running() {
        let (reports, rx) = mpsc::channel();
        let actor = Looper { reports, count: 0 }.start();
        let handle = actor.child_handle();
        actor.send(Loop).unwrap();
        drop(actor);
        for expected in 1..=100 {
            assert_eq!(rx.recv_timeout(LIMIT).unwrap(), expected);
        }
        assert!(handle.is_alive(), "M5: actor stopped when its refs dropped");
        handle.stop();
        assert_eq!(handle.wait_exit_blocking(), ExitReason::Normal);
    }

    /// M4: no `ActorId` is ever reused, also after earlier actors exited.
    #[test]
    fn actor_ids_are_never_reused() {
        struct Idle;
        impl Actor for Idle {}
        let mut ids = std::collections::HashSet::new();
        for _ in 0..4 {
            let batch: Vec<_> = (0..25).map(|_| Idle.start()).collect();
            for actor in &batch {
                assert!(ids.insert(actor.id()), "M4: id reused");
                actor.child_handle().stop();
            }
            for actor in batch {
                actor.join();
            }
        }
    }
}

// Not simulated: Blocking, Thread and thread-group placements need OS threads.
#[cfg(all(feature = "async", not(spawned_sim)))]
mod tasks_contract {
    use super::*;
    use spawned_concurrency::{
        system::{ActorSystem, ExecutionGroup},
        tasks::{Actor, ActorRef, ActorStart, Backend, Context, Handler},
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    #[derive(Clone, Copy, Debug)]
    enum Place {
        Async,
        Blocking,
        Thread,
        AsyncGroup,
        ThreadGroup,
    }
    const PLACES: [Place; 5] = [
        Place::Async,
        Place::Blocking,
        Place::Thread,
        Place::AsyncGroup,
        Place::ThreadGroup,
    ];

    struct Placement {
        system: ActorSystem,
        async_group: ExecutionGroup,
        thread_group: ExecutionGroup,
    }
    impl Placement {
        fn new() -> Self {
            let system = ActorSystem::new().unwrap();
            let async_group = system.async_group("contract-async", 2).unwrap();
            let thread_group = system.thread_group("contract-threads", 128).unwrap();
            Self {
                system,
                async_group,
                thread_group,
            }
        }
        fn start<A: Actor>(&self, actor: A, place: Place) -> ActorRef<A> {
            match place {
                Place::Async => actor.start_with_backend(Backend::Async),
                Place::Blocking => actor.start_with_backend(Backend::Blocking),
                Place::Thread => actor.start_with_backend(Backend::Thread),
                Place::AsyncGroup => actor.start_on(&self.async_group).unwrap(),
                Place::ThreadGroup => actor.start_on(&self.thread_group).unwrap(),
            }
        }
        async fn finish(self) {
            assert!(self.system.shutdown(LIMIT).await.complete);
        }
    }

    struct Hold {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    }
    impl Message for Hold {
        type Result = ();
    }

    struct Holder {
        seen: Arc<Mutex<Vec<&'static str>>>,
        drops: Arc<AtomicUsize>,
        send_in_stopped: Arc<Mutex<Option<bool>>>,
    }
    impl Actor for Holder {
        async fn stopped(&mut self, ctx: &Context<Self>) {
            self.seen.lock().unwrap().push("stopped");
            let accepted = ctx.send(Queued(Payload(self.drops.clone()))).is_ok();
            *self.send_in_stopped.lock().unwrap() = Some(accepted);
        }
    }
    impl Handler<Hold> for Holder {
        async fn handle(
            &mut self,
            hold: Hold,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                self.seen.lock().unwrap().push("hold");
                hold.entered.send(()).unwrap();
                let _ = timeout(LIMIT, hold.release).await;
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Queued> for Holder {
        async fn handle(
            &mut self,
            _: Queued,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                self.seen.lock().unwrap().push("queued");
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Asked> for Holder {
        async fn handle(
            &mut self,
            _: Asked,
            __reply: spawned_concurrency::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            let __value: u64 = async {
                self.seen.lock().unwrap().push("asked");
                1
            }
            .await;
            __reply.reply(__value);
        }
    }

    /// D3, D4, L7 on every tasks placement (threads mode: `native_contract`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_window_accepts_then_discards_and_releases_before_join() {
        let placement = Placement::new();
        for place in PLACES {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let drops = Arc::new(AtomicUsize::new(0));
            let send_in_stopped = Arc::new(Mutex::new(None));
            let actor = placement.start(
                Holder {
                    seen: seen.clone(),
                    drops: drops.clone(),
                    send_in_stopped: send_in_stopped.clone(),
                },
                place,
            );
            let (entered, entered_rx) = oneshot::channel();
            let (release, release_rx) = oneshot::channel();
            actor
                .send(Hold {
                    entered,
                    release: release_rx,
                })
                .unwrap();
            timeout(LIMIT, entered_rx).await.unwrap().unwrap();
            actor.child_handle().stop();
            let mut replies = Vec::new();
            for _ in 0..WINDOW / 2 {
                actor.send(Queued(Payload(drops.clone()))).expect("D3");
                replies.push(
                    actor
                        .send_request(
                            Asked(Payload(drops.clone())),
                            spawned_concurrency::DEFAULT_REQUEST_TIMEOUT,
                        )
                        .expect("D3"),
                );
            }
            release.send(()).unwrap();
            timeout(LIMIT, actor.join()).await.unwrap();
            assert_eq!(
                drops.load(Ordering::SeqCst),
                WINDOW + 1,
                "D4/L7 {place:?}: a queued payload outlived join()"
            );
            for mut reply in replies {
                assert!(
                    matches!(reply.try_recv(), Some(Err(ActorError::ActorStopped(_)))),
                    "D4/L7 {place:?}: a queued reply sender outlived join()"
                );
            }
            assert_eq!(*seen.lock().unwrap(), ["hold", "stopped"], "D4 {place:?}");
            assert_eq!(
                *send_in_stopped.lock().unwrap(),
                Some(true),
                "D3 {place:?}: send in stopped()"
            );
            assert_eq!(actor.wait_exit().await, ExitReason::Normal, "{place:?}");
            assert!(matches!(
                actor.send(Queued(Payload(drops.clone()))),
                Err(ActorError::NoProc)
            ));
        }
        placement.finish().await;
    }

    struct Turns {
        log: Arc<Mutex<Vec<Turn>>>,
        counter: usize,
        final_counter: Arc<AtomicUsize>,
    }
    impl Turns {
        fn push(&self, turn: Turn) {
            self.log.lock().unwrap().push(turn);
        }
    }
    impl Actor for Turns {
        async fn started(&mut self, _: &Context<Self>) {
            self.push(Turn::StartedBegin);
            spawned_rt::tasks::yield_now().await;
            self.push(Turn::StartedEnd);
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            self.push(Turn::StoppedBegin);
            self.final_counter.store(self.counter, Ordering::SeqCst);
            spawned_rt::tasks::yield_now().await;
            self.push(Turn::StoppedEnd);
        }
    }
    impl Handler<Work> for Turns {
        /// A read-modify-write that spans an await: it loses updates unless
        /// the handler keeps the turn while it awaits (M1).
        async fn handle(
            &mut self,
            Work(id): Work,
            __reply: spawned_concurrency::PendingReply<'_, usize>,
            _: &Context<Self>,
        ) {
            let __value: usize = async {
                self.push(Turn::Begin(id));
                let seen = self.counter;
                spawned_rt::tasks::yield_now().await;
                spawned_rt::tasks::yield_now().await;
                self.counter = seen + 1;
                self.push(Turn::End(id));
                id
            }
            .await;
            __reply.reply(__value);
        }
    }

    /// L1, M1 on every tasks placement: concurrent requests race `started()`
    /// and an external stop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hooks_and_turns_never_overlap_under_concurrent_requests_and_stop() {
        let placement = Placement::new();
        for place in PLACES {
            for _ in 0..ROUNDS {
                let log = Arc::new(Mutex::new(Vec::new()));
                let final_counter = Arc::new(AtomicUsize::new(usize::MAX));
                let actor = placement.start(
                    Turns {
                        log: log.clone(),
                        counter: 0,
                        final_counter: final_counter.clone(),
                    },
                    place,
                );
                let replied = Arc::new(AtomicUsize::new(0));
                let (half, half_rx) = oneshot::channel();
                let half = Arc::new(Mutex::new(Some(half)));
                let senders: Vec<_> = (0..4)
                    .map(|t| {
                        let actor = actor.clone();
                        let replied = replied.clone();
                        let half = half.clone();
                        tokio::spawn(async move {
                            let mut outcomes = Vec::new();
                            for k in 0..16 {
                                let id = t * 100 + k;
                                let outcome = actor.request_with_timeout(Work(id), LIMIT).await;
                                outcomes.push((id, outcome));
                                if replied.fetch_add(1, Ordering::SeqCst) + 1 == 24 {
                                    if let Some(half) = half.lock().unwrap().take() {
                                        let _ = half.send(());
                                    }
                                }
                            }
                            outcomes
                        })
                    })
                    .collect();
                timeout(LIMIT, half_rx).await.unwrap().unwrap();
                actor.child_handle().stop();
                let mut outcomes = Vec::new();
                for sender in senders {
                    outcomes.extend(timeout(LIMIT, sender).await.unwrap().unwrap());
                }
                timeout(LIMIT, actor.join()).await.unwrap();
                let events = log.lock().unwrap().clone();
                check_turns(&events, final_counter.load(Ordering::SeqCst), &outcomes);
            }
        }
        placement.finish().await;
    }

    struct Backlog(Vec<usize>);
    impl Actor for Backlog {}
    impl Handler<Hold> for Backlog {
        async fn handle(
            &mut self,
            hold: Hold,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                hold.entered.send(()).unwrap();
                let _ = timeout(LIMIT * 6, hold.release).await;
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Seq> for Backlog {
        async fn handle(
            &mut self,
            Seq(i): Seq,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                self.0.push(i);
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Count> for Backlog {
        async fn handle(
            &mut self,
            _: Count,
            __reply: spawned_concurrency::PendingReply<'_, Vec<usize>>,
            _: &Context<Self>,
        ) {
            let __value: Vec<usize> = async { std::mem::take(&mut self.0) }.await;
            __reply.reply(__value);
        }
    }

    /// D5, D1, D2 on every tasks placement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unbounded_mailbox_accepts_a_large_backlog_without_blocking() {
        let placement = Placement::new();
        for place in PLACES {
            let actor = placement.start(Backlog(Vec::new()), place);
            let (entered, entered_rx) = oneshot::channel();
            let (release, release_rx) = oneshot::channel();
            actor
                .send(Hold {
                    entered,
                    release: release_rx,
                })
                .unwrap();
            timeout(LIMIT, entered_rx).await.unwrap().unwrap();
            let sender = actor.clone();
            // `send` is synchronous: a bounded mailbox would block this thread.
            let filled = std::thread::spawn(move || {
                for i in 0..BACKLOG {
                    sender.send(Seq(i)).expect("D5: send failed for capacity");
                }
            });
            let filled = tokio::task::spawn_blocking(move || filled.join());
            timeout(LIMIT * 6, filled)
                .await
                .expect("D5: sends blocked behind a busy handler")
                .unwrap()
                .unwrap();
            release.send(()).unwrap();
            let handled = actor.request_with_timeout(Count, LIMIT * 6).await.unwrap();
            assert_eq!(handled.len(), BACKLOG, "D2 {place:?}");
            assert!(
                handled.iter().copied().eq(0..BACKLOG),
                "D1 {place:?}: backlog out of order"
            );
            actor.child_handle().stop();
            timeout(LIMIT, actor.join()).await.unwrap();
        }
        placement.finish().await;
    }

    struct Looper {
        reports: tokio::sync::mpsc::UnboundedSender<usize>,
        count: usize,
    }
    impl Actor for Looper {}
    impl Handler<Loop> for Looper {
        async fn handle(
            &mut self,
            _: Loop,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            let __value: () = async {
                self.count += 1;
                let _ = self.reports.send(self.count);
                if self.count < 100 {
                    ctx.send(Loop).unwrap();
                }
            }
            .await;
            __reply.reply(__value);
        }
    }

    /// M5 on every tasks placement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_every_actor_ref_keeps_the_actor_running() {
        let placement = Placement::new();
        for place in PLACES {
            let (reports, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = placement.start(Looper { reports, count: 0 }, place);
            let handle = actor.child_handle();
            actor.send(Loop).unwrap();
            drop(actor);
            for expected in 1..=100 {
                assert_eq!(
                    timeout(LIMIT, rx.recv()).await.unwrap(),
                    Some(expected),
                    "{place:?}"
                );
            }
            assert!(handle.is_alive(), "M5 {place:?}: stopped when refs dropped");
            handle.stop();
            assert_eq!(
                timeout(LIMIT, handle.wait_exit_async()).await.unwrap(),
                ExitReason::Normal
            );
        }
        placement.finish().await;
    }

    /// M4 on every tasks placement: ids are unique across placements and
    /// never reused after earlier actors exited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_ids_are_never_reused() {
        struct Idle;
        impl Actor for Idle {}
        let placement = Placement::new();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..4 {
            for place in PLACES {
                let batch: Vec<_> = (0..25).map(|_| placement.start(Idle, place)).collect();
                for actor in &batch {
                    assert!(ids.insert(actor.id()), "M4 {place:?}: id reused");
                    actor.child_handle().stop();
                }
                for actor in batch {
                    timeout(LIMIT, actor.join()).await.unwrap();
                }
            }
        }
        placement.finish().await;
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Fault {
        Started,
        Handler,
        Stopped,
    }
    struct Victim {
        fault: Fault,
        handled: Arc<AtomicUsize>,
        stopped_ran: Arc<AtomicUsize>,
    }
    impl Actor for Victim {
        async fn started(&mut self, _: &Context<Self>) {
            if self.fault == Fault::Started {
                panic!("victim started");
            }
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            self.stopped_ran.fetch_add(1, Ordering::SeqCst);
            if self.fault == Fault::Stopped {
                panic!("victim stopped");
            }
        }
    }
    impl Handler<Work> for Victim {
        async fn handle(
            &mut self,
            Work(id): Work,
            __reply: spawned_concurrency::PendingReply<'_, usize>,
            _: &Context<Self>,
        ) {
            let __value: usize = async {
                self.handled.fetch_add(1, Ordering::SeqCst);
                if self.fault == Fault::Handler {
                    panic!("victim handler");
                }
                id
            }
            .await;
            __reply.reply(__value);
        }
    }

    /// L5 on every tasks placement: a panic in `started()` skips `stopped()`
    /// and every message; a handler panic loses that message and runs
    /// `stopped()`; a panic in `stopped()` still completes the exit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panics_in_hooks_follow_the_contract_on_every_placement() {
        let placement = Placement::new();
        for place in PLACES {
            for fault in [Fault::Started, Fault::Handler, Fault::Stopped] {
                let handled = Arc::new(AtomicUsize::new(0));
                let stopped_ran = Arc::new(AtomicUsize::new(0));
                let actor = placement.start(
                    Victim {
                        fault,
                        handled: handled.clone(),
                        stopped_ran: stopped_ran.clone(),
                    },
                    place,
                );
                let first = actor.request_with_timeout(Work(1), LIMIT).await;
                let second = actor.send(Work(2));
                if fault == Fault::Stopped {
                    assert!(matches!(first, Ok(1)), "{place:?}");
                    actor.child_handle().stop();
                } else {
                    assert!(
                        matches!(first, Err(ActorError::ActorStopped(_))),
                        "{place:?} {fault:?}: {first:?}"
                    );
                }
                let reason = timeout(LIMIT, actor.wait_exit()).await.unwrap();
                let (prefix, ran, stopped) = match fault {
                    Fault::Started => ("panic in started(): ", 0, 0),
                    Fault::Handler => ("panic in handler: ", 1, 1),
                    Fault::Stopped => ("panic in stopped(): ", usize::from(second.is_ok()) + 1, 1),
                };
                assert!(
                    matches!(&reason, ExitReason::Panic(m) if m.starts_with(prefix)),
                    "L5 {place:?} {fault:?}: {reason:?}"
                );
                if fault != Fault::Stopped {
                    assert_eq!(
                        handled.load(Ordering::SeqCst),
                        ran,
                        "L5 {place:?} {fault:?}"
                    );
                }
                assert_eq!(
                    stopped_ran.load(Ordering::SeqCst),
                    stopped,
                    "L5 {place:?} {fault:?}: stopped() calls"
                );
            }
        }
        placement.finish().await;
    }

    struct Gate {
        release: Option<oneshot::Receiver<()>>,
        handled: Arc<AtomicUsize>,
    }
    impl Actor for Gate {
        async fn started(&mut self, _: &Context<Self>) {
            if let Some(release) = self.release.take() {
                let _ = timeout(LIMIT, release).await;
            }
        }
    }
    impl Handler<Work> for Gate {
        async fn handle(
            &mut self,
            Work(id): Work,
            __reply: spawned_concurrency::PendingReply<'_, usize>,
            _: &Context<Self>,
        ) {
            let __value: usize = async {
                self.handled.fetch_add(1, Ordering::SeqCst);
                id
            }
            .await;
            __reply.reply(__value);
        }
    }

    /// L2 on both group kinds: messages queued while `started()` runs wait
    /// for readiness; `ready()` stays `Ok` after the actor exits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn managed_readiness_gates_handlers_and_stays_historical() {
        let placement = Placement::new();
        for place in [Place::AsyncGroup, Place::ThreadGroup] {
            let handled = Arc::new(AtomicUsize::new(0));
            let (release, release_rx) = oneshot::channel();
            let actor = placement.start(
                Gate {
                    release: Some(release_rx),
                    handled: handled.clone(),
                },
                place,
            );
            let replies: Vec<_> = (0..4)
                .map(|i| {
                    actor
                        .send_request(Work(i), spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                        .unwrap()
                })
                .collect();
            tokio::task::yield_now().await;
            assert!(
                timeout(Duration::from_millis(20), actor.ready())
                    .await
                    .is_err(),
                "L2 {place:?}: ready before started() returned"
            );
            assert_eq!(handled.load(Ordering::SeqCst), 0, "L2 {place:?}");
            release.send(()).unwrap();
            timeout(LIMIT, actor.ready()).await.unwrap().unwrap();
            for (i, reply) in replies.into_iter().enumerate() {
                assert_eq!(timeout(LIMIT, reply).await.unwrap().unwrap(), i);
            }
            actor.child_handle().stop();
            timeout(LIMIT, actor.join()).await.unwrap();
            assert!(
                timeout(LIMIT, actor.ready()).await.unwrap().is_ok(),
                "L2 {place:?}: ready() is the historical barrier"
            );
        }
        placement.finish().await;
    }

    /// Records the thread its destructor runs on.
    struct WhereDropped(Arc<Mutex<Option<String>>>);
    impl Drop for WhereDropped {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = std::thread::current().name().map(str::to_owned);
        }
    }
    struct Late(#[allow(dead_code)] WhereDropped);
    impl Message for Late {
        type Result = ();
    }
    struct Arm(Arc<Mutex<Option<String>>>);
    impl Message for Arm {
        type Result = ();
    }
    struct Timed;
    impl Actor for Timed {}
    impl Handler<Late> for Timed {
        async fn handle(
            &mut self,
            _: Late,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                panic!("T1: a timer fired after its actor stopped");
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Arm> for Timed {
        async fn handle(
            &mut self,
            Arm(slot): Arm,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            let __value: () = async {
                spawned_concurrency::tasks::send_after(
                    Duration::from_secs(3600),
                    ctx.clone(),
                    Late(WhereDropped(slot)),
                );
            }
            .await;
            __reply.reply(__value);
        }
    }

    /// T1, T3: a managed actor's timer task runs on the actor's group, and
    /// stopping the actor before the timer fires drops its payload there
    /// without sending it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn managed_timers_run_on_the_group_and_die_with_the_actor() {
        let placement = Placement::new();
        let actor = placement.start(Timed, Place::AsyncGroup);
        let slot = Arc::new(Mutex::new(None));
        actor
            .request_with_timeout(Arm(slot.clone()), LIMIT)
            .await
            .unwrap();
        actor.child_handle().stop();
        assert_eq!(
            timeout(LIMIT, actor.wait_exit()).await.unwrap(),
            ExitReason::Normal
        );
        let deadline = tokio::time::Instant::now() + LIMIT;
        let thread = loop {
            if let Some(name) = slot.lock().unwrap().clone() {
                break name;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "T1: timer payload not dropped after its actor stopped"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        assert!(
            thread.starts_with("spawned-contract-async"),
            "T3: timer ran on {thread}, not the actor's group"
        );
        placement.finish().await;
    }

    struct Napper;
    impl Actor for Napper {}
    struct Nap(Duration);
    impl Message for Nap {
        type Result = ();
    }
    impl Handler<Nap> for Napper {
        async fn handle(
            &mut self,
            Nap(d): Nap,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                tokio::time::sleep(d).await;
            }
            .await;
            __reply.reply(__value);
        }
    }

    /// R3: the default timeout is 5 s, and a `request` future starts its
    /// timer (and enqueues) only when first polled.
    #[tokio::test(start_paused = true)]
    async fn default_request_timeout_is_five_seconds_from_first_poll() {
        let actor = Napper.start();
        assert!(actor
            .request(Nap(Duration::from_millis(4_900)))
            .await
            .is_ok());
        assert!(matches!(
            actor.request(Nap(Duration::from_millis(5_100))).await,
            Err(ActorError::RequestTimeout)
        ));
        // Let the timed-out nap finish (R4: giving up does not cancel it).
        actor
            .request_with_timeout(Nap(Duration::ZERO), Duration::from_secs(60))
            .await
            .unwrap();
        let pending = actor.request(Nap(Duration::from_secs(4)));
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(
            pending.await.is_ok(),
            "R3: the timer ran before the first poll"
        );
        actor.child_handle().stop();
        actor.join().await;
    }

    /// S6: a `tokio::spawn` task and a raw OS thread started inside a
    /// managed handler are not owned: shutdown completes without joining the
    /// thread, and the task is aborted at group teardown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unmanaged_tasks_and_threads_are_not_joined_by_shutdown() {
        struct Spawn {
            task_drops: Arc<AtomicUsize>,
            thread_release: std::sync::mpsc::Receiver<()>,
            thread_done: Arc<AtomicUsize>,
        }
        impl Message for Spawn {
            type Result = ();
        }
        struct Spawner;
        impl Actor for Spawner {}
        impl Handler<Spawn> for Spawner {
            async fn handle(
                &mut self,
                spawn: Spawn,
                __reply: spawned_concurrency::PendingReply<'_, ()>,
                _: &Context<Self>,
            ) {
                let __value: () = async {
                    let probe = Payload(spawn.task_drops);
                    drop(tokio::spawn(async move {
                        let _probe = probe;
                        std::future::pending::<()>().await;
                    }));
                    let release = spawn.thread_release;
                    let done = spawn.thread_done;
                    drop(std::thread::spawn(move || {
                        let _ = release.recv();
                        done.fetch_add(1, Ordering::SeqCst);
                    }));
                }
                .await;
                __reply.reply(__value);
            }
        }
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("unowned", 1).unwrap();
        let actor = Spawner.start_on(&group).unwrap();
        let task_drops = Arc::new(AtomicUsize::new(0));
        let thread_done = Arc::new(AtomicUsize::new(0));
        let (release, thread_release) = std::sync::mpsc::channel();
        actor
            .request_with_timeout(
                Spawn {
                    task_drops: task_drops.clone(),
                    thread_release,
                    thread_done: thread_done.clone(),
                },
                LIMIT,
            )
            .await
            .unwrap();
        let report = system.shutdown(LIMIT).await;
        assert!(report.complete, "S6: unowned work held up shutdown");
        assert_eq!(thread_done.load(Ordering::SeqCst), 0, "thread was joined");
        assert_eq!(actor.wait_exit().await, ExitReason::Shutdown);
        drop(group);
        drop(system);
        let deadline = tokio::time::Instant::now() + LIMIT;
        while task_drops.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "S6: a tokio::spawn task survived group teardown"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        release.send(()).unwrap();
    }
}

/// Supervision and system gaps, on the simulator under `--cfg spawned_sim`
/// (seed sweeps, virtual time) and once on Tokio otherwise. Upper bounds on
/// latency are asserted only under the simulator, where time is virtual.
#[cfg(feature = "async")]
mod supervision_contract {
    use super::*;
    use spawned_concurrency::{
        supervision::{
            Backoff, ChildSpec, RestartMode, SupervisedChild, SupervisorConfig, SupervisorStrategy,
        },
        system::ActorSystem,
        tasks::{send_after, Actor, ActorRef, ActorStart, Context, Handler},
    };
    use spawned_rt::tasks::{oneshot, sleep, timeout};
    use spawned_rt::time::Instant;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    fn config(strategy: SupervisorStrategy, initial: Duration) -> SupervisorConfig {
        SupervisorConfig::default()
            .strategy(strategy)
            .max_restarts(20)
            .backoff(
                Backoff::default()
                    .initial(initial)
                    .maximum(Duration::from_secs(10))
                    .jitter(false)
                    .seed(7),
            )
    }

    struct Hold {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    }
    impl Message for Hold {
        type Result = ();
    }
    struct Incr;
    impl Message for Incr {
        type Result = ();
    }
    struct Next;
    impl Message for Next {
        type Result = usize;
    }
    struct Crash;
    impl Message for Crash {
        type Result = ();
    }
    struct Slow;
    impl Message for Slow {
        type Result = ();
    }

    /// Counts in its own state; `handled` counts every `Incr` and `Slow` it
    /// ever ran, across generations.
    struct Slot {
        n: usize,
        handled: Arc<AtomicUsize>,
    }
    impl Actor for Slot {}
    impl Handler<Hold> for Slot {
        async fn handle(
            &mut self,
            hold: Hold,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                let _ = hold.entered.send(());
                let _ = hold.release.await;
                panic!("crash with a queue behind it");
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Incr> for Slot {
        async fn handle(
            &mut self,
            _: Incr,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                self.n += 1;
                self.handled.fetch_add(1, Ordering::SeqCst);
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Next> for Slot {
        async fn handle(
            &mut self,
            _: Next,
            __reply: spawned_concurrency::PendingReply<'_, usize>,
            _: &Context<Self>,
        ) {
            let __value: usize = async {
                self.n += 1;
                self.n
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Crash> for Slot {
        async fn handle(
            &mut self,
            _: Crash,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                panic!("injected crash");
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Slow> for Slot {
        async fn handle(
            &mut self,
            _: Slow,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                self.handled.fetch_add(1, Ordering::SeqCst);
                sleep(Duration::from_millis(10)).await;
            }
            .await;
            __reply.reply(__value);
        }
    }

    fn slot_spec(
        name: &str,
        group: &spawned_concurrency::system::ExecutionGroup,
        handled: &Arc<AtomicUsize>,
        calls: &Arc<AtomicUsize>,
    ) -> ChildSpec<Slot> {
        let handled = handled.clone();
        let calls = calls.clone();
        ChildSpec::new(name, group, move |_| {
            let handled = handled.clone();
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok::<_, String>(Slot { n: 0, handled }) }
        })
        .restart(RestartMode::Permanent)
    }

    async fn next_generation(child: &SupervisedChild<Slot>, after: u64) -> ActorRef<Slot> {
        timeout(LIMIT, child.wait_for_generation(after))
            .await
            .expect("no replacement")
            .expect("child retired")
    }

    /// V4: messages queued at the old generation are discarded, never
    /// replayed into the replacement; queued callers get `ActorStopped`.
    #[spawned_macros::sim_test]
    async fn queued_messages_are_not_replayed_into_a_replacement() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("no-replay", 1).unwrap();
        let handled = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tree = system
            .supervisor(config(
                SupervisorStrategy::OneForOne,
                Duration::from_millis(1),
            ))
            .unwrap();
        let child = tree
            .add(slot_spec("slot", &group, &handled, &calls))
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        let old = child.current().unwrap();
        let generation = child.snapshot().generation;
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        old.send(Hold {
            entered,
            release: release_rx,
        })
        .unwrap();
        entered_rx.await.unwrap();
        let mut replies = Vec::new();
        for _ in 0..5 {
            old.send(Incr).unwrap();
            replies.push(
                old.send_request(Next, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                    .unwrap(),
            );
        }
        release.send(()).unwrap();
        let new = next_generation(&child, generation).await;
        for reply in replies {
            assert!(
                timeout(LIMIT, std::future::IntoFuture::into_future(reply))
                    .await
                    .unwrap()
                    .is_err(),
                "V4: a queued request was answered"
            );
        }
        assert_eq!(
            new.request(Next).await.unwrap(),
            1,
            "V4: state or queue replayed"
        );
        assert_eq!(handled.load(Ordering::SeqCst), 0, "V4: a queued Incr ran");
        assert!(matches!(old.send(Incr), Err(ActorError::NoProc)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
    }

    /// Crash `child`'s current generation; time until its next ready one.
    async fn crash_and_wait(
        crashed: &SupervisedChild<Slot>,
        all: &[&SupervisedChild<Slot>],
    ) -> Duration {
        let generations: Vec<u64> = all.iter().map(|c| c.snapshot().generation).collect();
        let began = Instant::now();
        let _ = crashed.current().unwrap().request(Crash).await;
        for (child, generation) in all.iter().zip(generations) {
            next_generation(child, generation).await;
        }
        began.elapsed()
    }

    const INITIAL: Duration = Duration::from_millis(200);

    /// V6: collateral children of a OneForAll round add no delay and do not
    /// grow their own backoff: a round costs one backoff of the failed
    /// child, and a collateral child's first own failure waits `initial`.
    #[spawned_macros::sim_test]
    async fn collateral_children_add_no_backoff() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("collateral", 1).unwrap();
        let handled = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tree = system
            .supervisor(config(SupervisorStrategy::OneForAll, INITIAL))
            .unwrap();
        let a = tree.add(slot_spec("a", &group, &handled, &calls)).unwrap();
        let b = tree.add(slot_spec("b", &group, &handled, &calls)).unwrap();
        let c = tree.add(slot_spec("c", &group, &handled, &calls)).unwrap();
        let supervisor = tree.start().await.unwrap();
        let all = [&a, &b, &c];
        let first = crash_and_wait(&a, &all).await;
        let second = crash_and_wait(&c, &all).await;
        assert!(
            first >= INITIAL && second >= INITIAL,
            "{first:?} {second:?}"
        );
        if cfg!(spawned_sim) {
            // Collateral delays would make the round about 3 x initial; a
            // collateral failure counted against c would make its first
            // own backoff 2 x initial.
            assert!(first < INITIAL * 3 / 2, "V6: collateral delay {first:?}");
            assert!(
                second < INITIAL * 3 / 2,
                "V6: collateral backoff grew {second:?}"
            );
        }
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
    }

    /// V2: concurrent failures of every OneForAll child coalesce into one
    /// recovery round: each slot is rebuilt exactly once.
    #[spawned_macros::sim_test]
    async fn concurrent_failures_coalesce_into_one_round() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("coalesce", 1).unwrap();
        let handled = Arc::new(AtomicUsize::new(0));
        let calls: Vec<_> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let mut tree = system
            .supervisor(config(
                SupervisorStrategy::OneForAll,
                Duration::from_millis(1),
            ))
            .unwrap();
        let children: Vec<_> = ["a", "b", "c"]
            .iter()
            .zip(&calls)
            .map(|(name, calls)| tree.add(slot_spec(name, &group, &handled, calls)).unwrap())
            .collect();
        let supervisor = tree.start().await.unwrap();
        let generations: Vec<u64> = children.iter().map(|c| c.snapshot().generation).collect();
        for child in &children {
            child.current().unwrap().send(Crash).unwrap();
        }
        for (child, generation) in children.iter().zip(&generations) {
            next_generation(child, *generation).await;
        }
        // Give a second, uncoalesced round time to show up.
        sleep(Duration::from_millis(500)).await;
        for (child, calls) in children.iter().zip(&calls) {
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "V2: {} rebuilt more than once",
                child.snapshot().name
            );
            assert!(child.current().is_ok());
        }
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
    }

    /// V9: the supervisor observes a child's exit through completion state,
    /// not its mailbox: a 10 s backlog does not delay the replacement.
    #[spawned_macros::sim_test]
    async fn message_backlog_does_not_delay_restart() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("backlog", 1).unwrap();
        let handled = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tree = system
            .supervisor(config(
                SupervisorStrategy::OneForOne,
                Duration::from_millis(1),
            ))
            .unwrap();
        let child = tree
            .add(slot_spec("slow", &group, &handled, &calls))
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        let old = child.current().unwrap();
        let generation = child.snapshot().generation;
        for _ in 0..1_000 {
            old.send(Slow).unwrap();
        }
        let began = Instant::now();
        old.child_handle().stop();
        next_generation(&child, generation).await;
        let latency = began.elapsed();
        assert!(latency < Duration::from_secs(5), "V9: {latency:?}");
        if cfg!(spawned_sim) {
            assert!(latency < Duration::from_millis(100), "V9: {latency:?}");
        }
        assert!(
            handled.load(Ordering::SeqCst) <= 1,
            "D4: backlog was drained"
        );
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
    }

    struct Fired;
    impl Message for Fired {
        type Result = ();
    }
    struct Probe {
        target: spawned_concurrency::ChildHandle,
    }
    impl Message for Probe {
        type Result = (bool, bool);
    }
    struct Late {
        fired: Arc<AtomicUsize>,
        downs: Arc<AtomicUsize>,
    }
    impl Actor for Late {}
    impl Handler<Fired> for Late {
        async fn handle(
            &mut self,
            _: Fired,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                self.fired.fetch_add(1, Ordering::SeqCst);
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Down> for Late {
        async fn handle(
            &mut self,
            _: Down,
            __reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let __value: () = async {
                self.downs.fetch_add(1, Ordering::SeqCst);
            }
            .await;
            __reply.reply(__value);
        }
    }
    impl Handler<Probe> for Late {
        /// Called while system shutdown is in progress but this actor is
        /// still running: timers, monitors and registrations are inert.
        async fn handle(
            &mut self,
            probe: Probe,
            __reply: spawned_concurrency::PendingReply<'_, (bool, bool)>,
            ctx: &Context<Self>,
        ) {
            let __value: (bool, bool) = async {
                send_after(Duration::from_millis(1), ctx.clone(), Fired);
                ctx.monitor(&probe.target);
                let registered = ctx.register_name("semantics-gaps-late-name").is_ok();
                let started = Late {
                    fired: self.fired.clone(),
                    downs: self.downs.clone(),
                }
                .start_on(&ctx.execution_group().unwrap())
                .is_ok();
                (registered, started)
            }
            .await;
            __reply.reply(__value);
        }
    }
    /// Stops first (admitted last); probes `late` from its `stopped()` and
    /// keeps the system shut down long enough for a live timer or monitor
    /// at `late` to deliver.
    struct Prober {
        late: ActorRef<Late>,
        result: Arc<Mutex<Option<(bool, bool)>>>,
    }
    impl Actor for Prober {
        // 0.8 (E3): the system's shutdown is its parent's exit signal; a
        // trapping actor ends through `stopped()`, where the probe runs.
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(true);
        }
        async fn stopped(&mut self, ctx: &Context<Self>) {
            let probe = Probe {
                target: ctx.actor_ref().child_handle(),
            };
            *self.result.lock().unwrap() = Some(self.late.request(probe).await.unwrap());
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// S1: once system shutdown begins, a still-running managed actor's
    /// timers, monitors, registrations and new actors are inert or refused.
    #[spawned_macros::sim_test]
    async fn admission_is_closed_for_the_whole_tree_once_shutdown_begins() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("late-admission", 1).unwrap();
        let fired = Arc::new(AtomicUsize::new(0));
        let downs = Arc::new(AtomicUsize::new(0));
        let late = Late {
            fired: fired.clone(),
            downs: downs.clone(),
        }
        .start_on(&group)
        .unwrap();
        late.ready().await.unwrap();
        let result = Arc::new(Mutex::new(None));
        let prober = Prober {
            late: late.clone(),
            result: result.clone(),
        }
        .start_on(&group)
        .unwrap();
        prober.ready().await.unwrap();
        assert!(system.shutdown(LIMIT).await.complete);
        let (registered, started) = result.lock().unwrap().expect("probe ran");
        assert!(!registered, "S1: registration admitted during shutdown");
        assert!(!started, "S1: actor admitted during shutdown");
        assert_eq!(
            fired.load(Ordering::SeqCst),
            0,
            "S1: timer fired during shutdown"
        );
        assert_eq!(
            downs.load(Ordering::SeqCst),
            0,
            "S1: monitor live during shutdown"
        );
        assert!(spawned_concurrency::registry::whereis::<ActorRef<Late>>(
            "semantics-gaps-late-name"
        )
        .is_none());
    }

    struct Gated {
        stopping: Option<oneshot::Sender<()>>,
        release: Option<oneshot::Receiver<()>>,
    }
    impl Actor for Gated {
        // 0.8 (E3): shutdown reaches a trapping actor through `stopped()`.
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(true);
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            if let Some(stopping) = self.stopping.take() {
                let _ = stopping.send(());
            }
            if let Some(release) = self.release.take() {
                let _ = release.await;
            }
        }
    }

    /// S3, S2: dropping an `ActorSystem` does not block, stops the latest
    /// admitted actor first, and signals earlier ones only at the 5 s grace
    /// (with the parent's kill, E3).
    #[spawned_macros::sim_test]
    async fn dropping_a_system_shuts_down_in_order_with_a_five_second_grace() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("dropped", 1).unwrap();
        let earlier = Gated {
            stopping: None,
            release: None,
        }
        .start_on(&group)
        .unwrap();
        earlier.ready().await.unwrap();
        let (stopping, stopping_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let later = Gated {
            stopping: Some(stopping),
            release: Some(release_rx),
        }
        .start_on(&group)
        .unwrap();
        later.ready().await.unwrap();
        drop(group);
        let began = Instant::now();
        drop(system);
        assert!(
            began.elapsed() < Duration::from_millis(100),
            "S3: drop blocked"
        );
        timeout(LIMIT, stopping_rx).await.unwrap().unwrap();
        sleep(Duration::from_secs(4)).await;
        if cfg!(spawned_sim) {
            assert!(
                earlier.child_handle().is_alive(),
                "S2/S3: earlier actor signalled before the later one finished or the grace"
            );
        }
        let earlier_reason = timeout(Duration::from_secs(3), earlier.wait_exit())
            .await
            .expect("S3: earlier actor not signalled at the 5 s grace");
        // 0.8 (E3): at the grace the earlier actor gets the parent's
        // `Shutdown`, then every remaining actor the parent's kill; a
        // trapping actor that finishes its parent exit first ends `Shutdown`.
        assert!(
            matches!(earlier_reason, ExitReason::Shutdown | ExitReason::Killed),
            "{earlier_reason:?}"
        );
        assert!(later.child_handle().is_alive());
        release.send(()).unwrap();
        // `later` is inside `stopped()`: it finishes; a kill that reached it
        // before its death point supersedes its reason (X4).
        let later_reason = timeout(LIMIT, later.wait_exit()).await.unwrap();
        assert!(
            matches!(later_reason, ExitReason::Shutdown | ExitReason::Killed),
            "{later_reason:?}"
        );
    }
}
