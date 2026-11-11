//! Loom model of the pipe-to-self job core (`src/reply/job_core.rs`,
//! docs/design/deferred-replies.md §8, §9, `verification/tla/DeferredJobs.tla`):
//! admission and its rollback, keyed replacement of a completed job, emits
//! racing the terminal notification and cancellation, native completion
//! racing the owner's cleanup join, and the final destruction of a job whose
//! last strong reference comes from a `Weak` upgrade.
//!
//! `Arc`/`Weak` for jobs are modelled below (Loom has no `Weak`): the strong
//! count is a Loom atomic and upgrade is the same CAS loop as `std`, so the
//! thread that drops the last strong reference is explored. A job's final
//! destruction takes the job set's locks, which deadlocks (and Loom reports
//! it) if the destruction runs under one of them.
#![cfg(spawned_scheduler_loom)]

mod sync {
    pub(crate) use loom::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    pub(crate) use loom::sync::{Arc, Condvar, Mutex, MutexGuard};
    pub(crate) use loom::thread::yield_now;
    pub(crate) type SlotOwner = Option<()>;
    pub(crate) fn section<T>(f: impl FnOnce() -> T) -> T {
        f()
    }
    pub(crate) fn left_open(_: &SlotOwner, _: usize, _: &'static str, _: bool) {}
    pub(crate) use super::model_arc::{JobArc, JobWeak};

    /// Loom has no timed wait; the model never parks on a slot.
    pub(crate) fn wait_ticks<'a, T>(
        condvar: &Condvar,
        guard: MutexGuard<'a, T>,
        ticks: Option<u64>,
    ) -> MutexGuard<'a, T> {
        assert!(ticks.is_none(), "timed park is not modelled");
        condvar.wait(guard).unwrap_or_else(|p| p.into_inner())
    }
}

/// `std::sync::{Arc, Weak}` for jobs, with a hook at the final strong drop.
mod model_arc {
    use loom::sync::atomic::{AtomicUsize, Ordering};
    use std::any::Any;

    struct Shared<T> {
        strong: AtomicUsize,
        value: T,
    }

    pub(crate) struct JobArc<T: 'static>(loom::sync::Arc<Shared<T>>);
    pub(crate) struct JobWeak<T: 'static>(loom::sync::Arc<Shared<T>>);

    impl<T: 'static> JobArc<T> {
        pub(crate) fn new(value: T) -> Self {
            Self(loom::sync::Arc::new(Shared {
                strong: AtomicUsize::new(1),
                value,
            }))
        }
        pub(crate) fn downgrade(this: &Self) -> JobWeak<T> {
            JobWeak(this.0.clone())
        }
    }
    impl<T: 'static> Clone for JobArc<T> {
        fn clone(&self) -> Self {
            self.0.strong.fetch_add(1, Ordering::Relaxed);
            Self(self.0.clone())
        }
    }
    impl<T: 'static> std::ops::Deref for JobArc<T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0.value
        }
    }
    impl<T: 'static> Drop for JobArc<T> {
        fn drop(&mut self) {
            if self.0.strong.fetch_sub(1, Ordering::AcqRel) == 1 {
                if let Some(job) = (&self.0.value as &dyn Any).downcast_ref::<super::TestJob>() {
                    job.destroyed();
                }
            }
        }
    }
    impl<T: 'static> JobWeak<T> {
        /// `std::sync::Weak::upgrade`: increment unless already zero.
        pub(crate) fn upgrade(&self) -> Option<JobArc<T>> {
            let mut n = self.0.strong.load(Ordering::Relaxed);
            loop {
                if n == 0 {
                    return None;
                }
                match self
                    .0
                    .strong
                    .compare_exchange(n, n + 1, Ordering::Acquire, Ordering::Relaxed)
                {
                    Ok(_) => return Some(JobArc(self.0.clone())),
                    Err(now) => n = now,
                }
            }
        }
    }
}

#[allow(dead_code)]
#[path = "../src/reply/job_core.rs"]
mod job_core;

#[allow(dead_code)]
#[path = "../src/reply/slot.rs"]
mod slot;

use job_core::{Job, JobCore, JobGone, JobSetCore, Notification};
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use model_arc::JobArc;
use slot::CreditPool;
use std::sync::atomic::Ordering as StdOrdering;

type Set = JobSetCore<TestJob, u8>;

fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(f);
}

/// A job: the real core plus a destruction probe.
struct TestJob {
    id: u32,
    core: JobCore,
    /// Taken at the final destruction, which then takes the set's locks.
    set: Mutex<Option<Arc<Set>>>,
    destroyed: Arc<AtomicUsize>,
    gone: std::sync::atomic::AtomicBool,
}

impl TestJob {
    fn new(
        id: u32,
        credits: u16,
        set: Option<&Arc<Set>>,
        destroyed: &Arc<AtomicUsize>,
    ) -> JobArc<Self> {
        JobArc::new(Self {
            id,
            core: JobCore::new(credits),
            set: Mutex::new(set.cloned()),
            destroyed: destroyed.clone(),
            gone: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn destroyed(&self) {
        assert!(
            !self.gone.swap(true, StdOrdering::AcqRel),
            "job {} destroyed twice",
            self.id
        );
        self.destroyed.fetch_add(1, Ordering::AcqRel);
        let set = self.set.lock().unwrap().take();
        if let Some(set) = set {
            set.probe_locks();
        }
    }
}

impl Job for TestJob {
    fn cancel(&self) {
        self.core.cancel();
    }
    fn is_running(&self) -> bool {
        self.core.is_running()
    }
    fn finished(&self) {
        self.core.finished();
    }
}

/// Messages a job enqueues to its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Msg {
    Emit(u32, u32),
    Terminal(u32),
}

#[derive(Default)]
struct Mailbox(Mutex<Vec<Msg>>);
impl Mailbox {
    fn push(&self, msg: Msg) {
        self.0.lock().unwrap().push(msg);
    }
    fn take(&self) -> Vec<Msg> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// Admission at the bound: a blocking job reserves a credit then a native
/// slot, and rolls both back when its thread cannot be spawned, racing a
/// running job that ends (releasing its native slot and credit) and a second
/// admission. The bounds are never exceeded and nothing leaks.
#[test]
fn admission_rollback_vs_job_end() {
    model(|| {
        // Two credits: the running job holds one, so both admissions can
        // reach the native check at once.
        let pool = Arc::new(CreditPool::new(2));
        let set = Arc::new(Set::new(1));
        let holding = Arc::new(AtomicUsize::new(0));
        let destroyed = Arc::new(AtomicUsize::new(0));
        // A running job holds the credit and the native slot.
        assert!(pool.reserve(1) && set.reserve_native());
        let running = TestJob::new(0, 1, None, &destroyed);
        set.admit(&running, None);
        holding.fetch_add(1, Ordering::AcqRel);

        let ender = {
            let (pool, set, holding) = (pool.clone(), set.clone(), holding.clone());
            loom::thread::spawn(move || {
                holding.fetch_sub(1, Ordering::AcqRel);
                set.release_native();
                pool.release(1);
                set.ended(&running);
            })
        };
        // Admission whose spawn fails: roll back what was reserved.
        let admitter = {
            let (pool, set, holding) = (pool.clone(), set.clone(), holding.clone());
            loom::thread::spawn(move || {
                if pool.reserve(1) {
                    if set.reserve_native() {
                        assert_eq!(
                            holding.fetch_add(1, Ordering::AcqRel),
                            0,
                            "native bound exceeded"
                        );
                        holding.fetch_sub(1, Ordering::AcqRel);
                        set.release_native();
                    }
                    pool.release(1);
                }
            })
        };
        // A second admission on this thread: admitted, or rolled back.
        let mut second = None;
        if pool.reserve(1) {
            if set.reserve_native() {
                assert_eq!(
                    holding.fetch_add(1, Ordering::AcqRel),
                    0,
                    "native bound exceeded"
                );
                let job = TestJob::new(1, 1, None, &destroyed);
                set.admit(&job, None);
                second = Some(job);
            } else {
                pool.release(1);
            }
        }
        ender.join().unwrap();
        admitter.join().unwrap();
        if let Some(job) = second {
            holding.fetch_sub(1, Ordering::AcqRel);
            set.release_native();
            pool.release(1);
            set.ended(&job);
        }
        assert_eq!(set.running(), 0);
        assert_eq!(set.native(), 0, "a native slot leaked");
        assert_eq!(pool.used(), 0, "a credit leaked");
    });
}

/// A keyed job completes (terminal message enqueued, execution ended) while
/// the owner replaces its key: the replaced job is cancelled whatever its
/// notification state, so its terminal message is fenced at dispatch.
#[test]
fn completed_key_replacement() {
    model(|| {
        let set = Arc::new(Set::new(1));
        let mailbox = Arc::new(Mailbox::default());
        let destroyed = Arc::new(AtomicUsize::new(0));
        let old = TestJob::new(0, 1, None, &destroyed);
        set.admit(&old, Some(7));
        let worker = {
            let (set, mailbox, old) = (set.clone(), mailbox.clone(), old.clone());
            loom::thread::spawn(move || {
                old.core.notify(|| {
                    mailbox.push(Msg::Terminal(0));
                    None::<()>
                });
                set.ended(&old);
            })
        };
        let new = TestJob::new(1, 1, None, &destroyed);
        set.admit(&new, Some(7));
        assert!(
            old.core.is_cancelled(),
            "the replaced job was not cancelled"
        );
        assert!(!new.core.is_cancelled());
        worker.join().unwrap();
        // Dispatch after the replacement: the old terminal message is fenced.
        for msg in mailbox.take() {
            assert_eq!(msg, Msg::Terminal(0));
            assert!(
                old.core.is_cancelled(),
                "a replaced job's terminal message was dispatched"
            );
        }
        assert_ne!(old.core.notification(), Notification::Pending);
        set.cancel_all();
    });
}

/// An emitter (one credit, two emits) races the terminal notification and,
/// optionally, a cancel. No emit is enqueued after the terminal message, a
/// blocked emitter is always released, and every credit comes back.
#[test]
fn emit_vs_notification_and_cancel() {
    for cancel in [false, true] {
        model(move || {
            let mailbox = Arc::new(Mailbox::default());
            let destroyed = Arc::new(AtomicUsize::new(0));
            let job = TestJob::new(0, 1, None, &destroyed);
            let emitter = {
                let (job, mailbox) = (job.clone(), mailbox.clone());
                loom::thread::spawn(move || -> Result<(), JobGone> {
                    for i in 0..2 {
                        job.core.acquire_blocking()?;
                        job.core
                            .enqueue_emit(|| {
                                mailbox.push(Msg::Emit(0, i));
                                Ok::<(), ()>(())
                            })
                            .map_err(|(gone, _)| gone)?;
                    }
                    Ok(())
                })
            };
            let canceller = cancel.then(|| {
                let job = job.clone();
                loom::thread::spawn(move || job.core.cancel())
            });
            let (taken, _) = job.core.notify(|| {
                mailbox.push(Msg::Terminal(0));
                None::<()>
            });
            if let Some(canceller) = canceller {
                canceller.join().unwrap();
            }
            // Dispatch: each emit's credit returns.
            let mut terminal_seen = false;
            for msg in mailbox.take() {
                match msg {
                    Msg::Terminal(_) => terminal_seen = true,
                    Msg::Emit(..) => {
                        assert!(
                            !terminal_seen,
                            "an emit was enqueued after the terminal message"
                        );
                        job.core.return_emit_credit();
                    }
                }
            }
            assert_eq!(terminal_seen, taken);
            if !cancel {
                assert!(taken, "nothing suppressed the notification");
            }
            // The emitter stops at the terminal notification or the cancel
            // (or after both emits); nothing is enqueued after the dispatch.
            let _ = emitter.join().unwrap();
            for msg in mailbox.take() {
                panic!("enqueued after dispatch: {msg:?}");
            }
            assert_eq!(job.core.emit_credits(), 1, "an emit credit was lost");
        });
    }
}

/// A delayed emit (`emit_at`) scheduled by the work fires on the deadline
/// service while the work finishes and, optionally, the job is cancelled.
/// The terminal message is held until the scheduled emit has fired, so it is
/// always last; it is delivered exactly once unless cancelled; a cancel drops
/// a held result; every credit comes back.
#[test]
fn scheduled_emit_vs_finish_and_cancel() {
    for cancel in [false, true] {
        model(move || {
            let mailbox = Arc::new(Mailbox::default());
            let destroyed = Arc::new(AtomicUsize::new(0));
            let job = TestJob::new(0, 1, None, &destroyed);
            // The work schedules one delayed emit (its credit taken now).
            job.core.acquire_blocking().unwrap();
            job.core.schedule().unwrap();
            let timer = {
                let (job, mailbox) = (job.clone(), mailbox.clone());
                loom::thread::spawn(move || {
                    let (result, held) = job.core.fire(Some(|| {
                        mailbox.push(Msg::Emit(0, 0));
                        Ok::<(), ()>(())
                    }));
                    if result.is_err() {
                        // Not enqueued: the credit guard returns it.
                        job.core.return_emit_credit();
                    }
                    if let Some(held) = held {
                        held();
                    }
                })
            };
            let canceller = cancel.then(|| {
                let job = job.clone();
                loom::thread::spawn(move || job.core.cancel())
            });
            // The work ends: notify now, or hold for the scheduled emit.
            let later_job = job.clone();
            let later_mailbox = mailbox.clone();
            job.core.finish(
                || {
                    mailbox.push(Msg::Terminal(0));
                    None::<()>
                },
                move || {
                    Box::new(move || {
                        later_job.core.notify(|| {
                            later_mailbox.push(Msg::Terminal(0));
                            None::<()>
                        });
                    })
                },
            );
            timer.join().unwrap();
            if let Some(canceller) = canceller {
                canceller.join().unwrap();
            }
            let (mut terminals, mut emits) = (0, 0);
            for msg in mailbox.take() {
                match msg {
                    Msg::Terminal(_) => terminals += 1,
                    Msg::Emit(..) => {
                        assert_eq!(terminals, 0, "the delayed emit followed the terminal");
                        emits += 1;
                        job.core.return_emit_credit();
                    }
                }
            }
            assert!(terminals <= 1, "two terminal messages");
            assert_eq!(job.core.scheduled(), 0);
            if !cancel {
                assert_eq!(terminals, 1, "the held terminal was lost");
                assert_eq!(emits, 1, "the delayed emit was dropped without a cancel");
            }
            if terminals == 0 {
                assert_eq!(job.core.notification(), Notification::Suppressed);
            }
            assert_eq!(job.core.emit_credits(), 1, "an emit credit was lost");
        });
    }
}

/// A native job completes (native slot released, terminal notification,
/// execution ended) while the owner exits: cancel every job, then join.
/// The join is always woken, and once it returns no job runs, the native
/// slot is free and the job's notification is decided.
#[test]
fn native_completion_vs_cleanup() {
    model(|| {
        let set = Arc::new(Set::new(1));
        let mailbox = Arc::new(Mailbox::default());
        let destroyed = Arc::new(AtomicUsize::new(0));
        assert!(set.reserve_native());
        let job = TestJob::new(0, 1, None, &destroyed);
        set.admit(&job, None);
        let native = {
            let (set, mailbox, job) = (set.clone(), mailbox.clone(), job.clone());
            loom::thread::spawn(move || {
                set.release_native();
                job.core.notify(|| {
                    mailbox.push(Msg::Terminal(0));
                    None::<()>
                });
                set.ended(&job);
            })
        };
        set.cancel_all();
        let unfinished = set.join_with(|idle, running| Ok(idle.wait(running).unwrap()));
        assert_eq!(unfinished, 0);
        assert_eq!(
            set.native(),
            0,
            "the join returned before the native slot was released"
        );
        assert!(!job.core.is_running());
        assert_ne!(job.core.notification(), Notification::Pending);
        native.join().unwrap();
    });
}

/// The owner's cleanup upgrades its weak job list while the job thread drops
/// the last other strong reference: the job is destroyed exactly once, by
/// whichever thread drops last, and never under the set's locks.
#[test]
fn weak_upgrade_final_destruction_at_cancel_all() {
    model(|| {
        let set = Arc::new(Set::new(1));
        let destroyed = Arc::new(AtomicUsize::new(0));
        let job = TestJob::new(0, 1, Some(&set), &destroyed);
        set.admit(&job, Some(1));
        let worker = {
            let set = set.clone();
            loom::thread::spawn(move || {
                set.ended(&job);
                drop(job);
            })
        };
        set.cancel_all();
        worker.join().unwrap();
        assert_eq!(
            destroyed.load(Ordering::Acquire),
            1,
            "the job was not destroyed exactly once"
        );
    });
}

/// Compaction of the weak job list (at 32 entries) upgrades a finished job
/// whose last other reference is being dropped: if the upgraded reference is
/// the last, the destruction runs after the lock is released.
#[test]
fn weak_upgrade_final_destruction_at_admission() {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(2);
    builder.check(|| {
        let set = Arc::new(Set::new(1));
        let destroyed = Arc::new(AtomicUsize::new(0));
        let filler = Arc::new(AtomicUsize::new(0));
        // 31 entries already dead, plus the racing job: 32 at admission.
        for id in 0..31 {
            let dead = TestJob::new(id, 1, None, &filler);
            set.admit(&dead, None);
            set.ended(&dead);
        }
        let job = TestJob::new(0, 1, Some(&set), &destroyed);
        set.admit(&job, None);
        let worker = {
            let set = set.clone();
            loom::thread::spawn(move || {
                set.ended(&job);
                drop(job);
            })
        };
        // Compacts the weak list, upgrading the racing job.
        let next = TestJob::new(1, 1, None, &destroyed);
        set.admit(&next, Some(1));
        worker.join().unwrap();
        assert_eq!(
            destroyed.load(Ordering::Acquire),
            1,
            "the job was not destroyed exactly once"
        );
        set.ended(&next);
        set.cancel_all();
    });
}
