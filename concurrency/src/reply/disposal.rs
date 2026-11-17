//! Disposal off the deadline service thread
//! (docs/design/deferred-replies.md §2, "Callback progress").
//!
//! The process-wide deadline thread decides expiry: it pops due entries,
//! makes their terminal transitions and returns their deferral credits
//! (atomics and framework locks only). What an expiry releases that can run
//! user code (a caller's waker, a payload destructor routed to the owner's
//! failure sink, the last reference to an entry) is collected while the
//! thread services a visit ([`collect`]) and handed to the disposal workers
//! ([`submit`]) one item at a time. A worker that blocks in user code holds
//! only the item it runs: a queued item that finds no idle worker starts
//! another one, so a blocking destructor or waker delays no other expiry and
//! no other wake-up, of another actor or of the same one.
//!
//! Outside a [`collect`] scope, [`defer`] and [`defer_drop`] run inline, so
//! every other thread (owner turns, jobs, the simulated service task) keeps
//! its behaviour. The check is one thread-local read, made only on paths
//! that already have something to wake or drop.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

pub(crate) type Work = Box<dyn FnOnce() + Send>;

thread_local! {
    /// `Some` while the deadline thread services a visit.
    static BATCH: RefCell<Option<Vec<Work>>> = const { RefCell::new(None) };
}

/// Run `work` now, or, on the deadline thread inside [`collect`], defer it
/// to a disposal worker. Runs inline when this thread's batch is gone (a
/// thread-local destructor dropping a payload at thread exit) or borrowed.
#[inline]
pub(crate) fn defer(work: impl FnOnce() + Send + 'static) {
    let mut work = Some(work);
    let _ = BATCH.try_with(|batch| {
        if let Ok(mut batch) = batch.try_borrow_mut() {
            if let Some(batch) = batch.as_mut() {
                batch.push(Box::new(work.take().expect("not yet taken")));
            }
        }
    });
    if let Some(work) = work {
        work();
    }
}

/// Drop `value` now, or defer its drop (see [`defer`]).
#[inline]
pub(crate) fn defer_drop<T: Send + 'static>(value: T) {
    defer(move || drop(value));
}

/// Drop `arc` now, or, when it is the last reference, defer dropping its
/// value (see [`defer`]). A reference that is not the last runs no user
/// code, so it costs no hand-off.
#[inline]
pub(crate) fn defer_drop_arc<T: Send + 'static>(arc: Arc<T>) {
    if let Some(value) = Arc::into_inner(arc) {
        defer_drop(value);
    }
}

/// Whether this thread is collecting (the deadline thread in a visit).
#[inline]
pub(crate) fn collecting() -> bool {
    BATCH
        .try_with(|batch| batch.try_borrow().is_ok_and(|batch| batch.is_some()))
        .unwrap_or(false)
}

/// Run `step` with deferral on, and return what it deferred. If `step`
/// unwinds, what it deferred so far is submitted, not dropped here.
pub(crate) fn collect(step: impl FnOnce()) -> Vec<Work> {
    struct Scope;
    impl Scope {
        fn take() -> Vec<Work> {
            BATCH
                .try_with(|batch| batch.borrow_mut().take().unwrap_or_default())
                .unwrap_or_default()
        }
    }
    impl Drop for Scope {
        fn drop(&mut self) {
            // Only on unwind: the normal path took the batch already.
            submit(Self::take());
        }
    }
    BATCH.with(|batch| *batch.borrow_mut() = Some(Vec::new()));
    let scope = Scope;
    step();
    let work = Scope::take();
    drop(scope);
    work
}

// ---------------------------------------------------------------------------
// Workers
// ---------------------------------------------------------------------------

/// Workers beyond this many wait for a busy one: a bound on threads, reached
/// only when that many callbacks are blocked at once.
const MAX_WORKERS: usize = 64;
/// An idle worker exits after this long without work.
#[cfg(not(test))]
const KEEP_ALIVE: Duration = Duration::from_secs(10);
/// Short in tests, so submissions race idle exits.
#[cfg(test)]
const KEEP_ALIVE: Duration = Duration::from_millis(2);

#[derive(Default)]
struct State {
    queue: VecDeque<Work>,
    /// Workers waiting for work (or woken and not yet back under the lock).
    idle: usize,
    workers: usize,
    /// A worker is starting: it will take an item when it runs, so no other
    /// starts meanwhile (a burst of quick items does not start one each).
    starting: bool,
    /// The cap was reported; reset once below it again.
    capped: bool,
}

impl State {
    /// Queued work needs another worker: more items than idle workers that
    /// could take them, and none starting. Returns whether to start one.
    fn needs_worker(&mut self) -> Start {
        if self.queue.len() <= self.idle || self.starting {
            return Start::No;
        }
        if self.workers >= MAX_WORKERS {
            if std::mem::replace(&mut self.capped, true) {
                return Start::No;
            }
            return Start::Capped;
        }
        self.workers += 1;
        self.starting = true;
        Start::Yes
    }
}

enum Start {
    No,
    Yes,
    /// At the cap for the first time since below it: report it.
    Capped,
}

#[derive(Default)]
struct Pool {
    state: Mutex<State>,
    ready: Condvar,
}

fn pool() -> &'static Arc<Pool> {
    static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
    POOL.get_or_init(Default::default)
}

fn lock(pool: &Pool) -> std::sync::MutexGuard<'_, State> {
    pool.state.lock().unwrap_or_else(|p| p.into_inner())
}

/// Hand deferred work to the disposal workers, one queue entry per item, so
/// a blocking item delays none of the others. When queued items outnumber
/// idle workers a new worker starts (one at a time, up to [`MAX_WORKERS`]);
/// each worker re-checks when it takes an item.
pub(crate) fn submit(work: Vec<Work>) {
    if work.is_empty() {
        return;
    }
    let pool = pool();
    let mut state = lock(pool);
    let wake = work.len().min(state.idle);
    state.queue.extend(work);
    let start = state.needs_worker();
    drop(state);
    for _ in 0..wake {
        pool.ready.notify_one();
    }
    start_worker(pool, start);
}

fn start_worker(pool: &Arc<Pool>, start: Start) {
    match start {
        Start::No => {}
        Start::Capped => crate::actor_cleanup::log_safely(|| {
            tracing::warn!(
                event = "disposal_workers_capped",
                max = MAX_WORKERS,
                "every deadline disposal worker is busy: later wake-ups and \
                 payload drops queue until one returns"
            );
        }),
        Start::Yes => {
            let worker = pool.clone();
            let spawned = std::thread::Builder::new()
                .name("spawned-disposal".into())
                .spawn(move || run(&worker));
            if spawned.is_err() {
                // No new thread: an existing worker takes the work. With
                // none at all, run it here rather than lose a wake-up.
                let orphaned = {
                    let mut state = lock(pool);
                    state.workers -= 1;
                    state.starting = false;
                    if state.workers == 0 {
                        std::mem::take(&mut state.queue)
                    } else {
                        VecDeque::new()
                    }
                };
                orphaned.into_iter().for_each(run_item);
            }
        }
    }
}

fn run(pool: &Arc<Pool>) {
    let mut state = lock(pool);
    state.starting = false;
    loop {
        if let Some(item) = state.queue.pop_front() {
            // Whatever is still queued must not wait for this item.
            if !state.queue.is_empty() && state.idle > 0 {
                pool.ready.notify_one();
            }
            let start = state.needs_worker();
            drop(state);
            start_worker(pool, start);
            run_item(item);
            state = lock(pool);
            continue;
        }
        state.idle += 1;
        let (next, timeout) = pool
            .ready
            .wait_timeout(state, KEEP_ALIVE)
            .unwrap_or_else(|p| p.into_inner());
        state = next;
        state.idle -= 1;
        if timeout.timed_out() && state.queue.is_empty() {
            state.workers -= 1;
            state.capped = false;
            return;
        }
    }
}

/// Contained, so a failing callback ends neither its worker nor the others.
fn run_item(item: Work) {
    super::registry::contain_entry(item);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Generous: sanitizer builds are slow, and a panic's first report there
    /// can take seconds.
    const WAIT: Duration = Duration::from_secs(60);

    #[test]
    fn defer_runs_inline_outside_a_scope() {
        let (tx, rx) = mpsc::channel();
        defer(move || tx.send(()).unwrap());
        rx.try_recv().expect("ran inline");
        assert!(!collecting());
    }

    #[test]
    fn collect_defers_and_ends_its_scope() {
        let (tx, rx) = mpsc::channel();
        let work = collect(|| {
            assert!(collecting());
            let sender = tx.clone();
            defer(move || sender.send(1).unwrap());
            defer_drop(tx);
        });
        assert!(!collecting());
        assert!(rx.try_recv().is_err(), "deferred, not run");
        assert_eq!(work.len(), 2);
        submit(work);
        assert_eq!(rx.recv_timeout(WAIT), Ok(1));
    }

    #[test]
    fn an_unwinding_scope_submits_what_it_collected() {
        let (tx, rx) = mpsc::channel();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = collect(|| {
                defer(move || tx.send(()).unwrap());
                panic!("injected visit panic");
            });
        }));
        crate::mailbox::dispose_panic(result.unwrap_err());
        assert!(!collecting());
        assert!(rx.recv_timeout(WAIT).is_ok(), "collected work was lost");
    }

    #[test]
    fn last_reference_drops_are_deferred_others_are_not() {
        let shared = Arc::new(());
        let work = collect(|| {
            defer_drop_arc(shared.clone()); // not the last: nothing deferred
            defer_drop_arc(Arc::new(())); // the last: deferred
        });
        assert_eq!(work.len(), 1);
        assert_eq!(Arc::strong_count(&shared), 1);
    }

    #[test]
    fn a_blocked_item_does_not_hold_back_the_next_of_its_batch() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        // One visit's work: a blocking callback, then another caller's wake.
        submit(vec![
            Box::new(move || {
                entered_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(WAIT);
            }),
            Box::new(move || tx.send(()).unwrap()),
        ]);
        entered_rx
            .recv_timeout(WAIT)
            .expect("the first item started");
        assert!(
            rx.recv_timeout(WAIT).is_ok(),
            "a blocked item held back the next one of its batch"
        );
        release_tx.send(()).unwrap();
    }

    #[test]
    fn a_blocked_batch_does_not_hold_back_the_next() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (entered_tx, entered_rx) = mpsc::channel();
        submit(vec![Box::new(move || {
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(WAIT);
        })]);
        entered_rx
            .recv_timeout(WAIT)
            .expect("the first batch started");
        let (tx, rx) = mpsc::channel();
        submit(vec![Box::new(move || tx.send(()).unwrap())]);
        assert!(
            rx.recv_timeout(WAIT).is_ok(),
            "a blocked worker held back another batch"
        );
        release_tx.send(()).unwrap();
    }

    #[test]
    fn a_panicking_item_is_contained() {
        let (tx, rx) = mpsc::channel();
        submit(vec![
            Box::new(|| panic!("injected disposal panic")),
            Box::new(move || tx.send(()).unwrap()),
        ]);
        assert!(rx.recv_timeout(WAIT).is_ok());
    }

    /// Submissions race workers going idle and exiting (`KEEP_ALIVE` is 2 ms
    /// in tests), with some items blocked meanwhile: every item runs.
    #[test]
    fn submissions_racing_idle_exits_lose_no_item() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (tx, rx) = mpsc::channel::<usize>();
        let mut expected = 0;
        let submitters: Vec<_> = (0..4)
            .map(|t| {
                let tx = tx.clone();
                let release_rx = release_rx.clone();
                std::thread::spawn(move || {
                    let mut sent = 0;
                    for round in 0..150usize {
                        let mut work: Vec<Work> = Vec::new();
                        for _ in 0..(round % 3 + 1) {
                            let tx = tx.clone();
                            work.push(Box::new(move || tx.send(1).unwrap()));
                            sent += 1;
                        }
                        if round % 50 == t {
                            // A blocked callback, released at the end.
                            let release_rx = release_rx.clone();
                            work.insert(
                                0,
                                Box::new(move || {
                                    let _ = release_rx.lock().unwrap().recv_timeout(WAIT);
                                }),
                            );
                        }
                        submit(work);
                        // Around the keep-alive: idle workers time out
                        // while later submissions arrive.
                        std::thread::sleep(Duration::from_micros(500 * (round as u64 % 6)));
                    }
                    sent
                })
            })
            .collect();
        for submitter in submitters {
            expected += submitter.join().unwrap();
        }
        let mut ran = 0;
        while ran < expected {
            match rx.recv_timeout(WAIT) {
                Ok(n) => ran += n,
                Err(_) => break,
            }
        }
        for _ in 0..16 {
            let _ = release_tx.send(());
        }
        assert_eq!(
            ran, expected,
            "an item was lost or held behind a blocked one"
        );
    }

    /// A thread-local destructor that drops a payload after this thread's
    /// batch is destroyed runs it inline instead of aborting.
    #[test]
    fn deferral_after_thread_local_teardown_runs_inline() {
        struct DropsLater(Option<mpsc::Sender<bool>>);
        impl Drop for DropsLater {
            fn drop(&mut self) {
                let tx = self.0.take().unwrap();
                let collecting = collecting();
                let sender = tx.clone();
                defer(move || sender.send(true).unwrap());
                let _ = tx.send(collecting);
            }
        }
        thread_local! {
            static LATER: RefCell<Option<DropsLater>> = const { RefCell::new(None) };
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Registered first, so destroyed after the batch (reverse order).
            LATER.with(|later| *later.borrow_mut() = Some(DropsLater(Some(tx))));
            let _ = collect(|| {});
        })
        .join()
        .expect("a deferral during thread-local teardown must not abort");
        let got: Vec<bool> = rx.try_iter().collect();
        assert_eq!(got, [true, false], "ran inline, not collecting");
    }
}
