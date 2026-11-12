//! Determinism D2 and D3: `spawn_blocking`, `block_in_place`, `sleep_until`,
//! `interval` and `JoinSet` through `spawned_rt::tasks`, on Tokio in normal
//! builds and on the simulator (virtual time, one thread) under
//! `--cfg spawned_sim`. The `sim` module checks what only the simulator
//! guarantees: exact virtual instants, inline blocking work, and the failure
//! a direct `tokio::time` call produces.
#![cfg(feature = "async")]

use spawned_rt::tasks::{self as rt, MissedTickBehavior, SimTest};
use spawned_rt::time::Instant;
use std::time::Duration;

const MS: Duration = Duration::from_millis(1);

#[test]
fn spawn_blocking_returns_its_value() {
    rt::sim_test(|seed| async move {
        let doubled = rt::spawn_blocking(move || seed * 2).await.unwrap();
        assert_eq!(doubled, seed * 2);
        let panicked = rt::spawn_blocking(|| -> u8 { panic!("blocking work failed") }).await;
        assert!(panicked.unwrap_err().is_panic());
    });
}

#[test]
fn block_in_place_returns_its_value() {
    // Tokio needs a multi-thread runtime for block_in_place.
    SimTest::new()
        .multi_thread(2)
        .run(|_| async { assert_eq!(rt::block_in_place(|| 21 * 2), 42) });
}

#[test]
fn sleep_until_waits_for_the_deadline() {
    rt::sim_test(|_| async {
        let start = Instant::now();
        rt::sleep_until(start + 20 * MS).await;
        assert!(start.elapsed() >= 20 * MS);
        // A deadline in the past completes.
        rt::sleep_until(start).await;
    });
}

#[test]
fn interval_ticks_every_period() {
    rt::sim_test(|_| async {
        let mut ticks = rt::interval(10 * MS);
        assert_eq!(ticks.period(), 10 * MS);
        let first = ticks.tick().await;
        let second = ticks.tick().await;
        let third = ticks.tick().await;
        assert_eq!(second - first, 10 * MS);
        assert_eq!(third - first, 20 * MS);
        assert_eq!(ticks.missed_tick_behavior(), MissedTickBehavior::Burst);
    });
}

#[test]
fn join_set_collects_every_result() {
    rt::sim_test(|_| async {
        let mut set = rt::JoinSet::new();
        for n in 0..5u64 {
            set.spawn(async move {
                rt::sleep(Duration::from_millis(n)).await;
                n
            });
        }
        set.spawn_blocking(|| 100);
        assert_eq!(set.len(), 6);
        let mut seen = Vec::new();
        while let Some(result) = set.join_next().await {
            seen.push(result.unwrap());
        }
        seen.sort_unstable();
        assert_eq!(seen, [0, 1, 2, 3, 4, 100]);
        assert!(set.is_empty());
        assert!(set.join_next().await.is_none());

        let mut set = rt::JoinSet::new();
        let handle = set.spawn(async {
            rt::sleep(Duration::from_secs(3600)).await;
            1u8
        });
        handle.abort();
        assert!(set.join_next().await.unwrap().unwrap_err().is_cancelled());

        let set: rt::JoinSet<u32> = (1..=3u32).map(|n| async move { n * 10 }).collect();
        let mut all = set.join_all().await;
        all.sort_unstable();
        assert_eq!(all, [10, 20, 30]);
    });
}

#[cfg(spawned_sim)]
mod sim {
    use super::MS;
    use spawned_rt::tasks::{self as rt, sim, MissedTickBehavior};
    use spawned_rt::time::Instant;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// D2: the closure runs on the simulated thread as one step of a new
    /// task: after the spawner yields, in virtual time, never concurrently.
    #[test]
    fn spawn_blocking_runs_inline_as_one_step() {
        let outcome = sim::run_once(3, None, &|_| async {
            let thread = std::thread::current().id();
            let ran = Arc::new(AtomicBool::new(false));
            let flag = ran.clone();
            let handle = rt::spawn_blocking(move || {
                flag.store(true, Ordering::SeqCst);
                (std::thread::current().id(), Instant::now())
            });
            // Not run at the spawn: it is a task the scheduler polls.
            assert!(!ran.load(Ordering::SeqCst));
            rt::sleep(5 * MS).await;
            let (ran_on, at) = handle.await.unwrap();
            assert_eq!(ran_on, thread);
            assert!(at < Instant::now());
        });
        assert_eq!(outcome.failure, None);
    }

    /// A closure that blocks on async work in the same world cannot finish
    /// on one thread: a nested `block_on` fails the run even though the
    /// panic lands in a task nobody joins.
    #[test]
    fn nested_block_on_in_spawn_blocking_fails_the_run() {
        let outcome = sim::run_once(1, None, &|_| async {
            drop(rt::spawn_blocking(|| rt::block_on(rt::sleep(MS))));
            rt::sleep(10 * MS).await;
        });
        let failure = outcome.failure.expect("must fail");
        assert!(failure.contains("nested block_on"), "{failure}");
    }

    #[test]
    fn timers_are_exact_in_virtual_time() {
        let outcome = sim::run_once(2, None, &|_| async {
            let start = Instant::now();
            rt::sleep_until(start + 7 * MS).await;
            assert_eq!(start.elapsed(), 7 * MS);

            let base = Instant::now();
            let mut burst = rt::interval(10 * MS);
            assert_eq!(burst.tick().await, base);
            rt::sleep(35 * MS).await;
            // Burst: the missed ticks come back to back, then the schedule.
            assert_eq!(burst.tick().await, base + 10 * MS);
            assert_eq!(burst.tick().await, base + 20 * MS);
            assert_eq!(burst.tick().await, base + 30 * MS);
            assert_eq!(burst.tick().await, base + 40 * MS);
            assert_eq!(Instant::now(), base + 40 * MS);

            // Up to 5 ms late keeps the schedule, as on Tokio.
            let base = Instant::now();
            let mut slightly_late = rt::interval_at(base, 10 * MS);
            slightly_late.set_missed_tick_behavior(MissedTickBehavior::Delay);
            slightly_late.tick().await;
            rt::sleep(13 * MS).await;
            assert_eq!(slightly_late.tick().await, base + 10 * MS);
            assert_eq!(slightly_late.tick().await, base + 20 * MS);

            for (behavior, next) in [
                (MissedTickBehavior::Delay, 45),
                (MissedTickBehavior::Skip, 40),
            ] {
                let base = Instant::now();
                let mut ticks = rt::interval_at(base, 10 * MS);
                ticks.set_missed_tick_behavior(behavior);
                ticks.tick().await;
                rt::sleep(35 * MS).await;
                assert_eq!(ticks.tick().await, base + 10 * MS);
                assert_eq!(ticks.tick().await, base + next * MS, "{behavior:?}");
            }
        });
        assert_eq!(outcome.failure, None);
    }

    /// `join_next` returns tasks in completion order in virtual time, and
    /// tasks that finish together by task id: the same for every run.
    #[test]
    fn join_set_order_is_deterministic() {
        let order = |seed| {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let out = seen.clone();
            let outcome = sim::run_once(seed, None, &move |_| {
                let out = out.clone();
                async move {
                    let mut set = rt::JoinSet::new();
                    for n in [30u64, 10, 20, 10, 0] {
                        set.spawn(async move {
                            rt::sleep(Duration::from_millis(n)).await;
                            n
                        });
                    }
                    while let Some(n) = set.join_next().await {
                        out.lock().unwrap().push(n.unwrap());
                    }
                }
            });
            assert_eq!(outcome.failure, None);
            let seen = seen.lock().unwrap().clone();
            seen
        };
        let first = order(4);
        assert_eq!(first, [0, 10, 10, 20, 30]);
        assert_eq!(order(4), first);
    }

    /// D3: a direct Tokio timer fails the run with a message naming the
    /// `spawned_rt` replacement, from the root and from a detached task.
    #[test]
    fn direct_tokio_time_names_the_replacement() {
        let root = sim::run_once(1, None, &|_| async {
            tokio::time::sleep(MS).await;
        });
        let failure = root.failure.expect("must fail");
        assert!(
            failure.contains("direct tokio::time call")
                && failure.contains("spawned_rt::tasks::{sleep, sleep_until, timeout, interval}"),
            "{failure}"
        );

        let detached = sim::run_once(1, None, &|_| async {
            drop(rt::spawn(async {
                let _ = tokio::time::timeout(MS, std::future::pending::<()>()).await;
            }));
            rt::sleep(10 * MS).await;
        });
        let failure = detached.failure.expect("must fail");
        assert!(failure.contains("direct tokio::time call"), "{failure}");
    }
}
