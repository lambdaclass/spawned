//! Deterministic simulation tests with `#[spawned_concurrency::sim_test]`.
//!
//! A plain `cargo test` runs each test once on Tokio. Under the simulator
//!
//! ```sh
//! cargo sim-test -p spawned-capability-examples --test deterministic_simulation
//! ```
//!
//! (the `.cargo/config.toml` alias for `RUSTFLAGS="--cfg spawned_sim --cfg
//! tokio_unstable" cargo test`, built in target/sim) each test runs on one
//! thread over seeds `0..SPAWNED_SIM_SEEDS` (default 8): the seed picks which
//! ready task runs next, and time is virtual, so timers fire as soon as
//! nothing else is ready. A failing seed prints a replay command and the path
//! of a decision tape. Rerun it with `SPAWNED_SIM_SEED=<n>`, or
//! `SPAWNED_SIM_TAPE=<path>` to replay the exact schedule; the `.min.tape`
//! written beside it is a shrunk schedule that fails the same way. See
//! `docs/TESTING.md`, "Seeds and decision tapes under `--cfg spawned_sim`",
//! for what the simulator does not model. Tests marked
//! `#[sim_test(sim_only)]` exist only under the simulator; the crate needs
//! no `#[cfg(spawned_sim)]` and no `unexpected_cfgs` configuration.
use spawned_concurrency::tasks::{Actor, ActorStart as _, Context, Handler};
use spawned_concurrency::{actor, error::ActorError, protocol, Response};
use spawned_rt::tasks as rt;

#[protocol]
pub trait LogProtocol: Send + Sync {
    fn record(&self, sender: u32, n: u32) -> Result<(), ActorError>;
    fn entries(&self) -> Response<Vec<(u32, u32)>>;
}

#[derive(Default)]
struct Log(Vec<(u32, u32)>);

#[actor(protocol = LogProtocol)]
impl Log {
    #[send_handler]
    async fn handle_record(&mut self, msg: log_protocol::Record, _: &Context<Self>) {
        self.0.push((msg.sender, msg.n));
    }

    #[request_handler]
    async fn handle_entries(
        &mut self,
        _: log_protocol::Entries,
        _: &Context<Self>,
    ) -> Vec<(u32, u32)> {
        self.0.clone()
    }
}

/// Two senders race into one mailbox. How their messages interleave depends
/// on the schedule (and so on the seed), but each sender's own messages
/// arrive in the order it sent them: this must hold for every seed.
#[spawned_concurrency::sim_test]
async fn per_sender_order_holds_under_every_schedule() {
    let log = Log::default().start();
    let senders: Vec<_> = (0..2)
        .map(|sender| {
            let log = log.clone();
            rt::spawn(async move {
                for n in 0..5 {
                    log.record(sender, n).unwrap();
                    rt::yield_now().await;
                }
            })
        })
        .collect();
    for sender in senders {
        sender.await.unwrap();
    }

    let entries = log.entries().await.unwrap();
    assert_eq!(entries.len(), 10);
    for sender in 0..2 {
        let order: Vec<u32> = entries
            .iter()
            .filter(|(s, _)| *s == sender)
            .map(|(_, n)| *n)
            .collect();
        assert_eq!(order, (0..5).collect::<Vec<_>>());
    }
}

/// Virtual time: an actor whose timer is an hour away. On Tokio this test
/// would take an hour, so it only exists under the simulator (`sim_only`),
/// where it takes milliseconds of wall-clock time.
#[spawned_concurrency::sim_test(sim_only)]
async fn an_hour_long_timer_fires_in_virtual_time() {
    use spawned_concurrency::tasks::send_after;
    use std::time::Duration;

    const HOUR: Duration = Duration::from_secs(3600);

    #[protocol]
    pub trait ReminderProtocol: Send + Sync {
        fn ring(&self) -> Result<(), ActorError>;
        fn rung(&self) -> Response<bool>;
    }

    #[derive(Default)]
    struct Reminder(bool);

    #[actor(protocol = ReminderProtocol)]
    impl Reminder {
        #[started]
        async fn started(&mut self, ctx: &Context<Self>) {
            send_after(HOUR, ctx.clone(), reminder_protocol::Ring);
        }

        #[send_handler]
        async fn handle_ring(&mut self, _: reminder_protocol::Ring, _: &Context<Self>) {
            self.0 = true;
        }

        #[request_handler]
        async fn handle_rung(&mut self, _: reminder_protocol::Rung, _: &Context<Self>) -> bool {
            self.0
        }
    }

    // The real clock: inside a simulated run `std::time::Instant` reads
    // virtual time too (determinism D1).
    let wall = spawned_rt::tasks::sim::wall_now();
    let virtual_start = spawned_rt::time::Instant::now();
    let reminder = Reminder::default().start();

    rt::sleep(HOUR - Duration::from_secs(1)).await;
    assert!(!reminder.rung().await.unwrap());
    rt::sleep(Duration::from_secs(2)).await;
    assert!(reminder.rung().await.unwrap());

    assert!(virtual_start.elapsed() >= HOUR);
    assert!(spawned_rt::tasks::sim::wall_now() - wall < Duration::from_secs(10));
}

// A race that Tokio hides and the simulator finds (README, "Testing your own
// actors deterministically"): two clients bump a counter with a
// read-modify-write over two requests, so both can read the same value.
#[protocol]
pub trait CounterProtocol: Send + Sync {
    fn get(&self) -> Response<u64>;
    fn set(&self, value: u64) -> Response<()>;
    fn increment(&self) -> Response<u64>;
}

#[derive(Default)]
struct Counter(u64);

#[actor(protocol = CounterProtocol)]
impl Counter {
    #[request_handler]
    async fn handle_get(&mut self, _: counter_protocol::Get, _: &Context<Self>) -> u64 {
        self.0
    }

    #[request_handler]
    async fn handle_set(&mut self, msg: counter_protocol::Set, _: &Context<Self>) {
        self.0 = msg.value;
    }

    #[request_handler]
    async fn handle_increment(&mut self, _: counter_protocol::Increment, _: &Context<Self>) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Before: on Tokio this passes almost every run. The simulator explores the
/// schedules and finds the one that loses an update, on every run, with a
/// seed and a tape that replay it (`sim_only`: on Tokio it is flaky).
#[spawned_concurrency::sim_test(sim_only, explore = "pct:d=3", iterations = 200)]
#[should_panic(expected = "lost update")]
async fn read_modify_write_loses_an_update() {
    let counter = Counter::default().start();
    let clients: Vec<_> = (0..2)
        .map(|_| {
            let counter = counter.clone();
            rt::spawn(async move {
                let n = counter.get().await.unwrap();
                counter.set(n + 1).await.unwrap();
            })
        })
        .collect();
    for client in clients {
        client.await.unwrap();
    }
    assert_eq!(counter.get().await.unwrap(), 2, "lost update");
}

/// After: the increment is one message, handled in one turn, so it holds on
/// every schedule the simulator tries (and on Tokio).
#[spawned_concurrency::sim_test(explore = "pct:d=3", iterations = 200)]
async fn one_message_increment_never_loses_an_update() {
    let counter = Counter::default().start();
    let clients: Vec<_> = (0..2)
        .map(|_| {
            let counter = counter.clone();
            rt::spawn(async move {
                counter.increment().await.unwrap();
            })
        })
        .collect();
    for client in clients {
        client.await.unwrap();
    }
    assert_eq!(counter.get().await.unwrap(), 2);
}
