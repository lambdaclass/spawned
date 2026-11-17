//! Disposal off the deadline service thread
//! (docs/design/deferred-replies.md §2, "Callback progress").
//!
//! The process-wide deadline thread decides expiry: it pops due entries,
//! makes their terminal transitions and returns their credits (atomics and
//! framework locks only). What an expiry releases that can run user code,
//! a caller's waker, a payload destructor routed to the owner's failure
//! sink, the last reference to an entry, is collected while the thread
//! services a visit ([`collect`]) and handed to a disposal worker
//! ([`submit`]). A worker that blocks in user code holds only itself: a
//! batch that finds no idle worker starts another one, so one actor's
//! blocking destructor or waker never delays another actor's expiry or its
//! wake-ups.
//!
//! Outside a [`collect`] scope ([`defer`], [`defer_drop`]) run inline, so
//! every other thread (owner turns, jobs, the simulated service task) keeps
//! its behaviour. The check is one thread-local read, made only on paths
//! that already have something to wake or drop.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

type Work = Box<dyn FnOnce() + Send>;

thread_local! {
    /// `Some` while the deadline thread services a visit.
    static BATCH: RefCell<Option<Vec<Work>>> = const { RefCell::new(None) };
}

/// Run `work` now, or, on the deadline thread inside [`collect`], defer it
/// to a disposal worker.
#[inline]
pub(crate) fn defer(work: impl FnOnce() + Send + 'static) {
    let work = BATCH.with(|batch| match batch.try_borrow_mut() {
        Ok(mut batch) => match batch.as_mut() {
            Some(batch) => {
                batch.push(Box::new(work));
                None
            }
            None => Some(work),
        },
        // Re-entered while the batch is borrowed: never on the service
        // path, which only pushes. Run it.
        Err(_) => Some(work),
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

/// Whether this thread is collecting (the deadline thread in a visit).
#[inline]
pub(crate) fn collecting() -> bool {
    BATCH.with(|batch| batch.try_borrow().is_ok_and(|batch| batch.is_some()))
}

/// Run `step` with deferral on, and return what it deferred. The scope
/// ends even if `step` unwinds (the deferred work is then dropped here,
/// which is why the service contains each step inside it).
pub(crate) fn collect(step: impl FnOnce()) -> Vec<Work> {
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            let _ = BATCH.with(|batch| batch.borrow_mut().take());
        }
    }
    BATCH.with(|batch| *batch.borrow_mut() = Some(Vec::new()));
    let scope = Scope;
    step();
    let work = BATCH.with(|batch| batch.borrow_mut().take().unwrap_or_default());
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
const KEEP_ALIVE: Duration = Duration::from_secs(10);

#[derive(Default)]
struct State {
    queue: VecDeque<Vec<Work>>,
    /// Workers waiting for work.
    idle: usize,
    workers: usize,
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

/// Hand a visit's deferred work to a disposal worker. A batch never waits
/// behind a busy worker: when queued batches outnumber idle workers, a new
/// worker starts (up to [`MAX_WORKERS`]).
pub(crate) fn submit(work: Vec<Work>) {
    if work.is_empty() {
        return;
    }
    let pool = pool();
    let mut state = lock(pool);
    state.queue.push_back(work);
    let start = state.queue.len() > state.idle && state.workers < MAX_WORKERS;
    if start {
        state.workers += 1;
    }
    drop(state);
    if start {
        let worker = pool.clone();
        let spawned = std::thread::Builder::new()
            .name("spawned-disposal".into())
            .spawn(move || run(&worker));
        if spawned.is_err() {
            // No new thread: an existing worker takes the batch. With none
            // at all, run it here rather than lose a wake-up.
            let orphaned = {
                let mut state = lock(pool);
                state.workers -= 1;
                if state.workers == 0 {
                    std::mem::take(&mut state.queue)
                } else {
                    VecDeque::new()
                }
            };
            orphaned.into_iter().for_each(run_batch);
        }
    }
    pool.ready.notify_one();
}

fn run(pool: &Pool) {
    let mut state = lock(pool);
    loop {
        if let Some(work) = state.queue.pop_front() {
            drop(state);
            run_batch(work);
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
            return;
        }
    }
}

/// Each item contained, so one failing callback skips none of the others.
fn run_batch(work: Vec<Work>) {
    for item in work {
        super::registry::contain_entry(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(1));
    }

    #[test]
    fn a_blocked_batch_does_not_hold_back_the_next() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (entered_tx, entered_rx) = mpsc::channel();
        let blocked: Vec<Work> = vec![Box::new(move || {
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
        })];
        submit(blocked);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first batch started");
        let (tx, rx) = mpsc::channel();
        submit(vec![Box::new(move || tx.send(()).unwrap())]);
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
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
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    }
}
