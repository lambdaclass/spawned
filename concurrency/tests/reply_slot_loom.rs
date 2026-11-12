//! Loom model of the reply slot's packed-word protocol
//! (docs/design/deferred-replies.md §1). One test per concurrent pair:
//! reply, expiry, shortening, abandonment, token drop, owner close, defer,
//! linked-job attach and end, and the parked/polled receiver.
//!
//! Run: `RUSTFLAGS="--cfg spawned_scheduler_loom" cargo test -p spawned-concurrency
//! --no-default-features --test reply_slot_loom --release`.
#![cfg(spawned_scheduler_loom)]

mod sync {
    pub(crate) use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    pub(crate) use loom::sync::{Arc, Condvar, Mutex, MutexGuard};
    pub(crate) use loom::thread::yield_now;
    pub(crate) type SlotOwner = Option<()>;
    pub(crate) fn section<T>(f: impl FnOnce() -> T) -> T {
        f()
    }
    pub(crate) fn left_open(_: &SlotOwner, _: usize, _: &'static str, _: bool) {}
    pub(crate) fn shortened(_: &SlotOwner, _: usize) {}
    pub(crate) fn parked() {}

    /// Loom has no timed wait; tests park only on slots without a deadline.
    pub(crate) fn wait_ticks<'a, T>(
        condvar: &Condvar,
        guard: MutexGuard<'a, T>,
        ticks: Option<u64>,
    ) -> MutexGuard<'a, T> {
        assert!(ticks.is_none(), "timed park is not modelled");
        condvar.wait(guard).unwrap_or_else(|p| p.into_inner())
    }
}

#[allow(dead_code)]
#[path = "../src/reply/slot.rs"]
mod slot;

use loom::sync::atomic::{AtomicU64, Ordering};
use loom::sync::Arc;
use slot::{credits, jobs, state, CreditPool, Delivery, Refused, Slot, Terminal, NEVER};

fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.max_permutations = Some(200_000);
    builder.check(f);
}

/// A model clock: ticks advanced explicitly by the test.
#[derive(Clone)]
struct Clock(Arc<AtomicU64>);
impl Clock {
    fn at(t: u64) -> Self {
        Self(Arc::new(AtomicU64::new(t)))
    }
    fn now(&self) -> impl Fn() -> u64 + '_ {
        move || self.0.load(Ordering::SeqCst)
    }
    fn set(&self, t: u64) {
        self.0.store(t, Ordering::SeqCst);
    }
}

/// The pool's count always equals the sum of `credits(word)` over its slots
/// once every thread has joined.
fn assert_balanced(pool: &CreditPool, slots: &[&Slot<u32>]) {
    let held: u64 = slots.iter().map(|s| credits(s.word())).sum();
    assert_eq!(
        pool.used() as u64,
        held,
        "credit counter diverged from slot words"
    );
    assert!(pool.used() <= pool.max(), "credit bound exceeded");
}

fn deferred(pool: &Arc<CreditPool>, deadline: u64) -> Arc<Slot<u32>> {
    let slot = Arc::new(Slot::new(deadline));
    slot.defer(pool, &|| 0).expect("defer on a fresh slot");
    slot
}

/// Reply versus expiry by a polling receiver as the clock reaches the
/// deadline: one terminal outcome, and no success at or after the deadline.
#[test]
fn reply_vs_expiry() {
    model(|| {
        let clock = Clock::at(0);
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, 10);
        let replied_at = Arc::new(AtomicU64::new(NEVER));
        let replier = {
            let (slot, clock, replied_at) = (slot.clone(), clock.clone(), replied_at.clone());
            loom::thread::spawn(move || {
                let now = || {
                    let t = clock.now()();
                    replied_at.store(t, Ordering::SeqCst);
                    t
                };
                slot.reply(7, &now)
            })
        };
        clock.set(10);
        let expired = slot.check_deadline(&clock.now());
        let (delivery, back) = replier.join().unwrap();
        assert!(expired);
        match delivery {
            Delivery::Delivered => {
                assert_eq!(state(slot.word()), slot::REPLIED);
                assert!(
                    replied_at.load(Ordering::SeqCst) < 10,
                    "success at the deadline"
                );
                assert!(back.is_none());
            }
            Delivery::Expired => {
                assert_eq!(state(slot.word()), slot::TIMED_OUT);
                assert_eq!(back, Some(7));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_balanced(&pool, &[&slot]);
    });
}

/// Reply versus `with_deadline` shortening into the past: the reply's
/// `WRITING` CAS linearizes; success implies the reply read a live deadline.
#[test]
fn reply_vs_shortening() {
    model(|| {
        let clock = Clock::at(5);
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, 100);
        let replier = {
            let (slot, clock) = (slot.clone(), clock.clone());
            loom::thread::spawn(move || slot.reply(1, &clock.now()))
        };
        let lowered = slot.shorten(3, &clock.now());
        let (delivery, _) = replier.join().unwrap();
        // Linearization at the reply's WRITING CAS: a shortening after it is
        // a no-op (the deadline stays), one before it is seen by the reply.
        match delivery {
            Delivery::Delivered => {
                assert_eq!(state(slot.word()), slot::REPLIED);
                assert!(
                    !lowered,
                    "a shortening was applied but not seen by the reply"
                );
                assert_eq!(slot.deadline(), 100);
            }
            Delivery::Expired => {
                assert_eq!(state(slot.word()), slot::TIMED_OUT);
                assert!(lowered);
                assert_eq!(slot.deadline(), 3);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_balanced(&pool, &[&slot]);
    });
}

/// Reply versus the caller dropping its receiver.
#[test]
fn reply_vs_abandon() {
    model(|| {
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, NEVER);
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply(1, &|| 0))
        };
        let abandoned = slot.abandon();
        let (delivery, back) = replier.join().unwrap();
        assert_ne!(abandoned, delivery == Delivery::Delivered);
        if abandoned {
            assert_eq!(state(slot.word()), slot::ABANDONED);
            assert_eq!((delivery, back), (Delivery::CallerGone, Some(1)));
        } else {
            assert_eq!(state(slot.word()), slot::REPLIED);
        }
        assert_balanced(&pool, &[&slot]);
    });
}

/// The in-turn reply (one CAS `OPEN -> REPLIED`, value written under the
/// slot lock) versus the caller abandoning: exactly one wins; a lost reply
/// gets its value back.
#[test]
fn reply_in_turn_vs_abandon() {
    model(|| {
        let slot: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply_in_turn(1, &|| 0))
        };
        let abandoned = slot.abandon();
        let (delivery, back) = replier.join().unwrap();
        assert_ne!(abandoned, delivery == Delivery::Delivered);
        if abandoned {
            assert_eq!(state(slot.word()), slot::ABANDONED);
            assert_eq!((delivery, back), (Delivery::CallerGone, Some(1)));
        } else {
            assert_eq!(state(slot.word()), slot::REPLIED);
            assert_eq!(back, None);
        }
    });
}

/// The in-turn reply versus the caller's timer expiring the slot: one wins;
/// an expired slot hands the value back as `Expired`.
#[test]
fn reply_in_turn_vs_expiry() {
    model(|| {
        let slot: Arc<Slot<u32>> = Arc::new(Slot::new(10));
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply_in_turn(7, &|| 0))
        };
        let expired = slot.terminate(Terminal::TimedOut);
        let (delivery, back) = replier.join().unwrap();
        assert_ne!(expired, delivery == Delivery::Delivered);
        if expired {
            assert_eq!(state(slot.word()), slot::TIMED_OUT);
            assert_eq!((delivery, back), (Delivery::Expired, Some(7)));
        } else {
            assert_eq!(state(slot.word()), slot::REPLIED);
        }
    });
}

/// A polled receiver sees the in-turn reply's value (written before the
/// publishing CAS, under the lock the receiver takes).
#[test]
fn polled_receiver_vs_reply_in_turn() {
    model(|| {
        let slot: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply_in_turn(4, &|| 0).0)
        };
        let got = loom::future::block_on(std::future::poll_fn(|cx| slot.poll(cx.waker())));
        assert_eq!(got, Ok(4));
        assert_eq!(replier.join().unwrap(), Delivery::Delivered);
    });
}

/// Reply versus the owner's registry close at death. A close that finds
/// `WRITING` waits for the reply to resolve.
#[test]
fn reply_vs_owner_close() {
    model(|| {
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, NEVER);
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply(1, &|| 0).0)
        };
        let closed = slot.close();
        assert!(
            slot.is_terminal(),
            "close returned while a reply was WRITING"
        );
        let delivery = replier.join().unwrap();
        if closed {
            assert_eq!(state(slot.word()), slot::STOPPED);
            assert_eq!(delivery, Delivery::OwnerExited);
        } else {
            assert_eq!(state(slot.word()), slot::REPLIED);
            assert_eq!(delivery, Delivery::Delivered);
        }
        assert_balanced(&pool, &[&slot]);
    });
}

/// A held token dropped (`DROPPED`) versus the owner close (`STOPPED`) versus
/// the deadline heap expiring it: exactly one wins, the credit returns once.
#[test]
fn drop_vs_close_vs_expiry() {
    model(|| {
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, 10);
        let dropper = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.terminate(Terminal::Dropped))
        };
        let expirer = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.check_deadline(&|| 10))
        };
        let closed = slot.close();
        let dropped = dropper.join().unwrap();
        assert!(expirer.join().unwrap());
        let s = state(slot.word());
        let expired = s == slot::TIMED_OUT;
        assert_eq!([closed, dropped, expired].iter().filter(|b| **b).count(), 1);
        assert_eq!(pool.used(), 0);
        assert_balanced(&pool, &[&slot]);
    });
}

/// `defer()` versus the caller abandoning: a slot that went terminal never
/// acquires a credit, and a deferred one releases it on abandonment.
#[test]
fn defer_vs_abandon() {
    model(|| {
        let pool = Arc::new(CreditPool::new(1));
        let slot: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let caller = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.abandon())
        };
        let deferred = slot.defer(&pool, &|| 0);
        caller.join().unwrap();
        assert_eq!(state(slot.word()), slot::ABANDONED);
        match deferred {
            Ok(()) | Err(Refused::Closed) => {}
            Err(Refused::Overloaded) => panic!("bound of one exceeded by one token"),
        }
        assert_eq!(pool.used(), 0);
        assert_balanced(&pool, &[&slot]);
    });
}

/// Two slots defer concurrently against a bound of one: one wins, the other's
/// caller is answered `Overloaded` in the same step.
#[test]
fn defer_at_the_bound() {
    model(|| {
        let pool = Arc::new(CreditPool::new(1));
        let a: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let b: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let other = {
            let (b, pool) = (b.clone(), pool.clone());
            loom::thread::spawn(move || b.defer(&pool, &|| 0))
        };
        let first = a.defer(&pool, &|| 0);
        let second = other.join().unwrap();
        let outcomes = [first, second];
        assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
        let loser = if first.is_ok() { &b } else { &a };
        assert_eq!(state(loser.word()), slot::OVERLOADED);
        assert_balanced(&pool, &[&a, &b]);
    });
}

/// Linked jobs: a second attach, both job ends, and the owner close race.
/// The token and its first job share one credit; credits always match
/// `max(JOBS, live token)` and return to zero.
#[test]
fn attach_vs_job_end_vs_close() {
    model(|| {
        let pool = Arc::new(CreditPool::new(2));
        let slot = deferred(&pool, NEVER);
        slot.attach(&|| 0)
            .expect("first attach shares the token's credit");
        assert_eq!(pool.used(), 1);
        let second = {
            let slot = slot.clone();
            loom::thread::spawn(move || {
                if slot.attach(&|| 0).is_ok() {
                    slot.job_end();
                }
            })
        };
        let first_end = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.job_end())
        };
        slot.close();
        second.join().unwrap();
        first_end.join().unwrap();
        assert_eq!(jobs(slot.word()), 0);
        assert_eq!(state(slot.word()), slot::STOPPED);
        assert_eq!(pool.used(), 0);
        assert_balanced(&pool, &[&slot]);
    });
}

/// A job ending while the reply moves `WRITING -> REPLIED`: the state-field
/// CAS must not lose the `JOBS` decrement.
#[test]
fn job_end_during_reply() {
    model(|| {
        let pool = Arc::new(CreditPool::new(2));
        let slot = deferred(&pool, NEVER);
        slot.attach(&|| 0).unwrap();
        slot.attach(&|| 0).unwrap();
        assert_eq!(pool.used(), 2);
        let ender = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.job_end())
        };
        assert_eq!(slot.reply(9, &|| 0).0, Delivery::Delivered);
        ender.join().unwrap();
        assert_eq!(jobs(slot.word()), 1);
        assert_eq!(pool.used(), 1, "the job still running keeps its credit");
        slot.job_end();
        assert_eq!(pool.used(), 0);
        assert_balanced(&pool, &[&slot]);
    });
}

/// A further attach while a job still runs after the token closed is
/// refused (`Closed`), and the running job's credit survives the close.
#[test]
fn attach_after_close_is_refused() {
    model(|| {
        let pool = Arc::new(CreditPool::new(2));
        let slot = deferred(&pool, NEVER);
        slot.attach(&|| 0).unwrap();
        let closer = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.terminate(Terminal::Dropped))
        };
        let attached = slot.attach(&|| 0);
        closer.join().unwrap();
        match attached {
            Ok(()) => {
                assert_eq!(jobs(slot.word()), 2);
                assert_eq!(pool.used(), 2);
                slot.job_end();
            }
            Err(refused) => assert_eq!(refused, Refused::Closed),
        }
        assert_eq!(pool.used(), 1);
        slot.job_end();
        assert_eq!(pool.used(), 0);
        assert_balanced(&pool, &[&slot]);
    });
}

/// A parked threads-mode receiver is woken by the reply (no lost wakeup).
#[test]
fn parked_receiver_vs_reply() {
    model(|| {
        let slot: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply(3, &|| 0).0)
        };
        assert_eq!(slot.recv_blocking(&|| 0), Ok(3));
        assert_eq!(replier.join().unwrap(), Delivery::Delivered);
    });
}

/// A parked receiver is woken by the owner close.
#[test]
fn parked_receiver_vs_close() {
    model(|| {
        let slot: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let closer = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.close())
        };
        assert_eq!(slot.recv_blocking(&|| 0), Err(Terminal::Stopped));
        assert!(closer.join().unwrap());
    });
}

/// A polled (async) receiver is woken by the reply (no lost wakeup).
#[test]
fn polled_receiver_vs_reply() {
    model(|| {
        let slot: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply(4, &|| 0).0)
        };
        let got = loom::future::block_on(std::future::poll_fn(|cx| slot.poll(cx.waker())));
        assert_eq!(got, Ok(4));
        assert_eq!(replier.join().unwrap(), Delivery::Delivered);
    });
}

/// A polled receiver woken by a drop of the deferred token.
#[test]
fn polled_receiver_vs_token_drop() {
    model(|| {
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, NEVER);
        let dropper = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.terminate(Terminal::Dropped))
        };
        let got = loom::future::block_on(std::future::poll_fn(|cx| slot.poll(cx.waker())));
        assert_eq!(got, Err(Terminal::Dropped));
        assert!(dropper.join().unwrap());
        assert_balanced(&pool, &[&slot]);
    });
}

/// Caller give-up (abandon) versus deadline shortening versus the heap's
/// expiry: one terminal state and a single credit return.
#[test]
fn abandon_vs_shorten_vs_expiry() {
    model(|| {
        let clock = Clock::at(5);
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, 100);
        let shortener = {
            let (slot, clock) = (slot.clone(), clock.clone());
            loom::thread::spawn(move || {
                slot.shorten(5, &clock.now());
            })
        };
        let abandoned = slot.abandon();
        shortener.join().unwrap();
        let s = state(slot.word());
        assert!(s == slot::ABANDONED || s == slot::TIMED_OUT);
        assert_eq!(abandoned, s == slot::ABANDONED);
        assert_eq!(pool.used(), 0);
        assert_balanced(&pool, &[&slot]);
    });
}

/// agy #6: a deferral that finds the pool full while the caller abandons
/// reports `Overloaded` only if the slot really became `OVERLOADED`; a slot
/// that closed first reports `Closed`.
#[test]
fn defer_at_the_bound_vs_abandon() {
    model(|| {
        let pool = Arc::new(CreditPool::new(0));
        let slot: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let caller = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.abandon())
        };
        let deferred = slot.defer(&pool, &|| 0);
        let abandoned = caller.join().unwrap();
        match deferred {
            Err(Refused::Overloaded) => {
                assert_eq!(state(slot.word()), slot::OVERLOADED);
                assert!(!abandoned);
            }
            Err(Refused::Closed) => assert!(abandoned),
            Ok(()) => panic!("a full pool granted a credit"),
        }
        assert_eq!(pool.used(), 0);
    });
}
