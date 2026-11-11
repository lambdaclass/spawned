//! Deterministic simulation tests with `#[spawned_concurrency::sim_test]`.
//!
//! A plain `cargo test` runs each test once on Tokio. Under the simulator
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" \
//!   cargo test -p spawned-capability-examples --test deterministic_simulation
//! ```
//!
//! each test runs on one thread over seeds `0..SPAWNED_SIM_SEEDS` (default 8):
//! the seed picks which ready task runs next, and time is virtual, so timers
//! fire as soon as nothing else is ready. A failing seed prints a replay
//! command and the path of a decision tape. Rerun it with the same `RUSTFLAGS`
//! plus `SPAWNED_SIM_SEED=<n>`, or `SPAWNED_SIM_TAPE=<path>` to replay the
//! exact schedule; the `.min.tape` written beside it is a shrunk schedule that
//! fails the same way. See `docs/TESTING.md`, "Seeds and decision tapes under
//! `--cfg spawned_sim`", for what the simulator does not model. A crate that
//! uses `#[cfg(spawned_sim)]` declares it under `[lints.rust]` in its
//! `Cargo.toml` (`unexpected_cfgs`), as this package does.
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
/// would take an hour, so it only exists under the simulator, where it takes
/// milliseconds of wall-clock time.
#[cfg(spawned_sim)]
mod virtual_time {
    use super::*;
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

    #[spawned_concurrency::sim_test]
    async fn an_hour_long_timer_fires_in_virtual_time() {
        let wall = std::time::Instant::now();
        let virtual_start = spawned_rt::time::Instant::now();
        let reminder = Reminder::default().start();

        rt::sleep(HOUR - Duration::from_secs(1)).await;
        assert!(!reminder.rung().await.unwrap());
        rt::sleep(Duration::from_secs(2)).await;
        assert!(reminder.rung().await.unwrap());

        assert!(virtual_start.elapsed() >= HOUR);
        assert!(wall.elapsed() < Duration::from_secs(10));
    }
}
