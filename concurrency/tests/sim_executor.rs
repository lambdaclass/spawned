//! Self-tests of the simulation executor (`spawned_rt::tasks::sim`): Tokio
//! fidelity (cooperative budget, seeded `select!`, destructor panics, zero
//! sleeps) and bookkeeping. Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test -p spawned-concurrency --test sim_executor
//! ```
#![cfg(all(spawned_sim, feature = "async"))]

use spawned_rt::tasks::{self as rt, sim, Runtime};
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::task::Poll;
use std::time::Duration;

/// Run `f` on its own thread and fail (instead of hanging the suite) if it
/// does not return within `limit`. A stuck thread is left behind; the test
/// process exits anyway.
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std_mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| panic!("did not finish within {limit:?}: a poll never returned"))
}

/// Codex P1: without Tokio's cooperative budget a task that keeps receiving
/// from a channel it refills never returns from one poll. With the budget it
/// is forced to yield after 128 operations and the root makes progress.
#[test]
fn coop_budget_forces_a_ready_channel_loop_to_yield() {
    let received = within(Duration::from_secs(20), || {
        let rt = Runtime::with_seed(3);
        rt.block_on(async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
            tx.send(0).unwrap();
            let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let seen = count.clone();
            let looper = rt::spawn(async move {
                while let Some(n) = rx.recv().await {
                    seen.fetch_add(1, Ordering::Relaxed);
                    tx.send(n + 1).unwrap();
                }
            });
            // Hand the thread to the looper until it has been forced to
            // yield a few times.
            while count.load(Ordering::Relaxed) < 1_000 {
                sim::yield_now().await;
            }
            looper.abort();
            assert!(looper.await.unwrap_err().is_cancelled());
            count.load(Ordering::Relaxed)
        })
    });
    assert!(received >= 1_000);
}

/// `tokio::task::unconstrained` opts out of the budget, as on Tokio.
#[test]
fn unconstrained_opts_out_of_the_budget() {
    let rt = Runtime::with_seed(1);
    rt.block_on(rt::unconstrained(async {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        for i in 0..1_000 {
            tx.send(i).unwrap();
        }
        // 1,000 ready receives in one poll: no forced yield.
        let mut polls = 0;
        std::future::poll_fn(|cx| {
            polls += 1;
            for _ in 0..1_000 {
                match rx.poll_recv(cx) {
                    Poll::Ready(Some(_)) => {}
                    other => panic!("unexpected {other:?}"),
                }
            }
            Poll::Ready(())
        })
        .await;
        assert_eq!(polls, 1);
    }));
}

/// Eight unbiased two-way `tokio::select!`s on ready branches.
async fn select_branches() -> Vec<bool> {
    let mut picks = Vec::new();
    for _ in 0..8 {
        let first = tokio::select! {
            _ = async {} => true,
            _ = async {} => false,
        };
        picks.push(first);
        sim::yield_now().await;
    }
    picks
}

/// Codex P1: Tokio's `select!` draws from a thread-local RNG. It is seeded
/// from the world's seed, so the same seed takes the same branches every run,
/// while different seeds still explore both.
#[test]
fn select_branch_order_is_seeded() {
    let run = |seed: u64| Runtime::with_seed(seed).block_on(select_branches());
    let mut all = std::collections::BTreeSet::new();
    for seed in 0..16 {
        let first = run(seed);
        for _ in 0..3 {
            assert_eq!(run(seed), first, "seed {seed} took different branches");
        }
        all.extend(first);
    }
    assert_eq!(all.len(), 2, "select! never varied across seeds");
}

struct PanicOnDrop;
impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            panic!("destructor panic");
        }
    }
}

/// A future that is ready at once and whose destructor panics.
struct ReadyThenPanic(PanicOnDrop);
impl Future for ReadyThenPanic {
    type Output = u32;
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<u32> {
        Poll::Ready(7)
    }
}

/// agy/Codex P2: a panicking destructor of an aborted, completed or
/// torn-down task becomes that task's `JoinError` (as on Tokio) instead of
/// unwinding through the executor.
#[test]
fn task_destructor_panics_stay_in_the_join_handle() {
    let rt = Runtime::with_seed(5);
    rt.block_on(async {
        // Aborted while parked: the child signals that it reached its await
        // before the abort, so the panicking destructor runs on a parked
        // future, not one that was never polled.
        let (parked_tx, parked_rx) = tokio::sync::oneshot::channel::<()>();
        let guard = PanicOnDrop;
        let parked = rt::spawn(async move {
            let _guard = guard;
            let _ = parked_tx.send(());
            std::future::pending::<()>().await;
        });
        parked_rx.await.unwrap();
        parked.abort();
        let error = parked.await.unwrap_err();
        assert!(error.is_panic(), "{error}");
        assert_eq!(
            *error.into_panic().downcast::<&str>().unwrap(),
            "destructor panic"
        );

        // Completed, then dropped.
        let done = rt::spawn(ReadyThenPanic(PanicOnDrop));
        assert!(done.await.unwrap_err().is_panic());
    });
    // Torn down with the runtime while parked (polled to its await first).
    let (parked_tx, parked_rx) = tokio::sync::oneshot::channel::<()>();
    let guard = PanicOnDrop;
    let left = rt.spawn(async move {
        let _guard = guard;
        let _ = parked_tx.send(());
        std::future::pending::<()>().await;
    });
    rt.block_on(async { parked_rx.await.unwrap() });
    drop(rt);
    assert!(left.is_finished());
}

/// agy (round 2): a task's *output* whose destructor panics, unread because
/// the `JoinHandle` was dropped (before or after completion), is dropped
/// inside a panic guard as on Tokio, not unwound through the executor.
#[test]
fn unread_output_destructor_panics_are_contained() {
    let rt = Runtime::with_seed(5);
    rt.block_on(async {
        drop(rt::spawn(async { PanicOnDrop }));
        let later = rt::spawn(async { PanicOnDrop });
        for _ in 0..4 {
            sim::yield_now().await;
        }
        assert!(later.is_finished());
        drop(later);
    });
}

/// Codex (round 2): a task that aborts itself and returns `Pending` is
/// cancelled right after that poll, as on Tokio, before any other task runs.
#[test]
fn self_abort_cancels_right_after_the_poll() {
    use std::sync::Mutex;
    for seed in 0..32 {
        let rt = Runtime::with_seed(seed);
        rt.block_on(async {
            let slot: Arc<Mutex<Option<rt::JoinHandle<()>>>> = Arc::default();
            let polled = Arc::new(AtomicBool::new(false));
            let (slot2, polled2) = (slot.clone(), polled.clone());
            let task = rt::spawn(async move {
                let own = slot2.lock().unwrap().take().unwrap();
                own.abort();
                polled2.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            });
            let observer = rt::spawn({
                let polled = polled.clone();
                async move {
                    while !polled.load(Ordering::SeqCst) {
                        sim::yield_now().await;
                    }
                }
            });
            *slot.lock().unwrap() = Some(task);
            observer.await.unwrap();
            // The first thing that runs after the aborting poll sees it done.
            assert!(polled.load(Ordering::SeqCst));
        });
        assert_eq!(
            rt.stats().tasks,
            0,
            "seed {seed}: self-aborted task still parked"
        );
    }
}

/// agy H1: a task that wakes itself during its final poll must not stay in
/// the ready list as a dead id (which shifts later picks and adds polls).
#[test]
fn self_wake_on_final_poll_leaves_no_ghost() {
    let rt = Runtime::with_seed(9);
    rt.record_trace();
    let id = rt.block_on(async {
        let task = rt::spawn(std::future::poll_fn(|cx| {
            cx.waker().wake_by_ref();
            Poll::Ready(())
        }));
        let id = task.id().to_string().parse::<u64>().unwrap();
        task.await.unwrap();
        // A few more scheduling points in which a ghost could be picked.
        for _ in 0..8 {
            sim::yield_now().await;
        }
        id
    });
    let polls = rt.trace().iter().filter(|(task, _)| *task == id).count();
    assert_eq!(polls, 1, "finished task {id} polled {polls} times");
}

/// agy M2: `sleep(Duration::ZERO)` is still a scheduling point.
#[test]
fn zero_sleep_yields_once() {
    let rt = Runtime::with_seed(2);
    rt.block_on(async {
        let mut sleep = pin!(rt::sleep(Duration::ZERO));
        let mut polls = 0;
        std::future::poll_fn(|cx| {
            polls += 1;
            sleep.as_mut().poll(cx)
        })
        .await;
        assert_eq!(polls, 2);
        assert_eq!(sim::now(), Duration::ZERO);
    });
}

/// agy L2: polling a `JoinHandle` again after it returned panics (as Tokio's
/// does) instead of parking forever.
#[test]
#[should_panic(expected = "JoinHandle polled after completion")]
fn join_handle_polled_after_completion_panics() {
    let rt = Runtime::with_seed(2);
    rt.block_on(async {
        let mut handle = rt::spawn(async { 1 });
        assert_eq!((&mut handle).await.unwrap(), 1);
        let _ = (&mut handle).await;
    });
}

/// A direct `tokio::spawn` would run outside the simulated scheduler; it
/// fails the run as a determinism leak.
#[test]
#[should_panic(expected = "handed work to Tokio directly")]
fn direct_tokio_spawn_is_a_determinism_leak() {
    let rt = Runtime::with_seed(2);
    rt.block_on(async {
        drop(tokio::spawn(async {}));
        sim::yield_now().await;
    });
}

const WATCHDOG_CHILD: &str = "watchdog_child_busy_loop";

/// Run by `poll_watchdog_aborts_a_poll_that_never_returns` in a subprocess.
#[test]
#[ignore = "spins forever; run as a subprocess by poll_watchdog_aborts_a_poll_that_never_returns"]
fn watchdog_child_busy_loop() {
    let stop = AtomicBool::new(false);
    Runtime::with_seed(11).block_on(async {
        while !stop.load(Ordering::Relaxed) {
            std::hint::spin_loop();
        }
    });
}

/// A loop with no await point cannot be interrupted on one thread; the
/// watchdog turns the hang into an abort that names the task and seed.
#[test]
fn poll_watchdog_aborts_a_poll_that_never_returns() {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([WATCHDOG_CHILD, "--exact", "--ignored", "--nocapture"])
        .env("SPAWNED_SIM_POLL_TIMEOUT", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The parent's own deadline: without the watchdog the child spins
    // forever, and this test must fail rather than hang.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the watchdog did not abort a poll that never returns within 30 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("spawned_sim: task 0 has been inside one poll")
            && stderr.contains("seed 11"),
        "{stderr}"
    );
}

/// Codex (round 2): a root that hands work to Tokio and returns at once is a
/// failure too (it was only reported at the next scheduling decision), and
/// the escaped task never runs.
#[test]
fn root_that_spawns_on_tokio_and_returns_fails() {
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    let rt = Runtime::with_seed(2);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(async move {
            drop(tokio::spawn(
                async move { flag.store(true, Ordering::SeqCst) },
            ));
        })
    }));
    let message = *result.unwrap_err().downcast::<String>().unwrap();
    assert!(
        message.contains("handed work to Tokio directly"),
        "{message}"
    );
    assert!(!ran.load(Ordering::SeqCst), "the escaped task ran");
}

/// Codex (round 2): `tokio::task::spawn_blocking` runs on a real OS thread
/// from Tokio's blocking pool; it is a determinism leak like `tokio::spawn`.
#[test]
#[should_panic(expected = "handed work to Tokio directly")]
fn direct_tokio_spawn_blocking_is_a_determinism_leak() {
    Runtime::with_seed(2).block_on(async {
        drop(tokio::task::spawn_blocking(|| ()));
    });
}

/// Codex (round 2): an escaped Tokio task that holds the simulation (a
/// cycle the simulated task table does not see) is dropped when the world
/// is torn down.
#[test]
fn escaped_tokio_task_does_not_keep_the_world_alive() {
    let probe = Arc::new(());
    let weak = Arc::downgrade(&probe);
    let rt = Runtime::with_seed(2);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(async move {
            let handle = rt::Handle::current();
            drop(tokio::spawn(async move {
                let _keep = (handle, probe);
                std::future::pending::<()>().await;
            }));
        })
    }));
    drop(rt);
    assert_eq!(weak.strong_count(), 0, "escaped task kept its captures");
}

/// agy (round 2): a root future whose destructor wakes it must not leave
/// ROOT in the ready list after `block_on` returns.
#[test]
fn root_waking_itself_in_its_destructor_leaves_nothing_ready() {
    struct WakeOnDrop(Option<std::task::Waker>);
    impl Drop for WakeOnDrop {
        fn drop(&mut self) {
            if let Some(waker) = self.0.take() {
                waker.wake();
            }
        }
    }
    let rt = Runtime::with_seed(2);
    rt.block_on(async {
        let mut guard = WakeOnDrop(None);
        std::future::poll_fn(|cx| {
            guard.0 = Some(cx.waker().clone());
            Poll::Ready(())
        })
        .await;
        let _guard = guard;
    });
    assert_eq!(rt.stats().ready, 0);
}
