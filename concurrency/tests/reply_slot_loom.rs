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
    pub(crate) use loom::sync::{Arc, Mutex, MutexGuard};
    pub(crate) type SlotOwner = Option<()>;
    pub(crate) type Reason = ();
    pub(crate) fn section<T>(f: impl FnOnce() -> T) -> T {
        f()
    }
    pub(crate) fn left_open(_: &SlotOwner, _: usize, _: &'static str, _: bool) {}
    pub(crate) fn shortened(_: &SlotOwner, _: usize) {}
    pub(crate) fn parked() {}

    /// The thread's park token: `std::thread::park`'s semantics (a wake
    /// sets the token; a park consumes it), modelled with a Loom mutex and
    /// condvar because Loom's own `unpark` also wakes a thread blocked in a
    /// `join`.
    #[derive(Default)]
    pub(crate) struct Token {
        set: loom::sync::Mutex<bool>,
        cv: loom::sync::Condvar,
    }
    impl std::task::Wake for Token {
        fn wake(self: std::sync::Arc<Self>) {
            *self.set.lock().unwrap() = true;
            self.cv.notify_all();
        }
    }
    loom::thread_local! {
        static TOKEN: std::sync::Arc<Token> = std::sync::Arc::new(Token::default());
    }

    /// A waker that unparks the current Loom thread.
    pub(crate) fn with_thread_waker<T>(f: impl FnOnce(&std::task::Waker) -> T) -> T {
        let waker = TOKEN.with(|token| std::task::Waker::from(token.clone()));
        f(&waker)
    }

    /// Loom has no timed park; tests park only on slots without a deadline.
    pub(crate) fn park(ticks: Option<u64>) {
        assert!(ticks.is_none(), "timed park is not modelled");
        let token = TOKEN.with(|token| token.clone());
        let mut set = token.set.lock().unwrap();
        while !*set {
            set = token.cv.wait(set).unwrap();
        }
        *set = false;
    }
}

#[allow(dead_code)]
#[path = "../src/reply/slot.rs"]
mod slot;

use loom::sync::atomic::{AtomicU64, Ordering};
use loom::sync::Arc;
use slot::{credits, jobs, state, CreditPool, Delivery, Refused, Slot, Terminal, NEVER};

#[test]
fn timer_rearm_cannot_lose_a_concurrent_shortening() {
    model(|| {
        let slot = Arc::new(Slot::<u32>::new(100));
        assert!(slot.shorten(80, &|| 0));
        let shortening = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.shorten(20, &|| 0))
        };
        assert!(slot.take_shortened());
        let observed = slot.deadline();
        shortening.join().unwrap();
        assert!(
            observed == 20 || slot.take_shortened(),
            "a shortening after the timer read must leave a new rearm mark"
        );
    });
}

#[test]
fn receiver_bookkeeping_preserves_owner_key_and_reply() {
    model(|| {
        let slot = Arc::new(Slot::<u32>::with_owner_key(100, None, slot::MAX_KEY));
        let tracking = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.mark_rx_tracked())
        };
        let replying = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply(21).0)
        };
        let got = loom::future::block_on(std::future::poll_fn(|cx| slot.poll(cx.waker())));
        tracking.join().unwrap();
        assert_eq!(replying.join().unwrap(), Delivery::Delivered);
        assert_eq!(got, Ok(21));
        assert_eq!(slot::owner_key(slot.word()), slot::MAX_KEY);
        assert_ne!(slot.word() & slot::RX_TRACKED, 0);
        assert_eq!(credits(slot.word()), 0);
    });
}

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

/// Reply versus expiry as the clock reaches the deadline (OTP: the
/// caller's expiry is the only timeout authority; the reply reads no
/// clock). Exactly one wins the word: a reply that wins is delivered even
/// at the deadline (before the expiry ran); one that loses gets its value
/// back as `Expired`, and the caller sees `TimedOut`, never the value.
#[test]
fn reply_vs_expiry() {
    model(|| {
        let clock = Clock::at(0);
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, 10);
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply(7))
        };
        clock.set(10);
        let expired = slot.check_deadline(&clock.now());
        let (delivery, back) = replier.join().unwrap();
        assert!(expired, "the deadline has passed");
        match delivery {
            Delivery::Delivered => {
                assert_eq!(state(slot.word()), slot::REPLIED);
                assert!(back.is_none());
                assert_eq!(slot.try_take(&clock.now()), Some(Ok(7)));
            }
            Delivery::Expired => {
                assert_eq!(state(slot.word()), slot::TIMED_OUT);
                assert_eq!(back, Some(7));
                assert_eq!(slot.try_take(&clock.now()), Some(Err(Terminal::TimedOut)));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_balanced(&pool, &[&slot]);
    });
}

/// Reply versus `shorten_deadline` into the past: the shortening's
/// `[deadline]` check expires the slot, racing the reply on the word. The
/// reply wins (delivered, whatever the deadline became) or loses
/// (`Expired`, and then the shortening was applied).
#[test]
fn reply_vs_shortening() {
    model(|| {
        let clock = Clock::at(5);
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, 100);
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply(1))
        };
        let lowered = slot.shorten(3, &clock.now());
        let (delivery, _) = replier.join().unwrap();
        match delivery {
            Delivery::Delivered => assert_eq!(state(slot.word()), slot::REPLIED),
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
            loom::thread::spawn(move || slot.reply(1))
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
            loom::thread::spawn(move || slot.reply_in_turn(1))
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
            loom::thread::spawn(move || slot.reply_in_turn(7))
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
            loom::thread::spawn(move || slot.reply_in_turn(4).0)
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
            loom::thread::spawn(move || slot.reply(1).0)
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
        assert_eq!(slot.reply(9).0, Delivery::Delivered);
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
            loom::thread::spawn(move || slot.reply(3).0)
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
            loom::thread::spawn(move || slot.reply(4).0)
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

/// A parked threads-mode receiver is woken by the in-turn reply (one CAS
/// publishing the value; the receiver's unparker sits in the waker cell).
#[test]
fn parked_receiver_vs_reply_in_turn() {
    model(|| {
        let slot: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply_in_turn(5).0)
        };
        assert_eq!(slot.recv_blocking(&|| 0), Ok(5));
        assert_eq!(replier.join().unwrap(), Delivery::Delivered);
    });
}

/// The death point's batched close: the close collects the wake-up, the
/// parked receivers are notified first (`notify_parked`), the rest runs
/// after. A parked receiver is never lost between the two steps.
#[test]
fn parked_receiver_vs_batched_close() {
    model(|| {
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, NEVER);
        let closer = {
            let slot = slot.clone();
            loom::thread::spawn(move || {
                let mut wake = slot.close_collect().expect("the close wins an open slot");
                slot.notify_parked(&mut wake);
                slot.run_wake(wake);
            })
        };
        assert_eq!(slot.recv_blocking(&|| 0), Err(Terminal::Stopped));
        closer.join().unwrap();
        assert_eq!(pool.used(), 0, "the credit returned with the close");
        assert_balanced(&pool, &[&slot]);
    });
}

/// A token watcher (`ReplyTo::closed`, caller-gone job cancellation) is
/// woken by a deferred reply from another thread (no lost wakeup), and the
/// caller's receiver gets the value.
#[test]
fn watcher_and_receiver_vs_deferred_reply() {
    model(|| {
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, NEVER);
        let replier = {
            let slot = slot.clone();
            loom::thread::spawn(move || slot.reply(8).0)
        };
        loom::future::block_on(std::future::poll_fn(|cx| {
            slot.poll_closed(cx.waker(), &|| 0)
        }));
        assert_eq!(replier.join().unwrap(), Delivery::Delivered);
        let got = loom::future::block_on(std::future::poll_fn(|cx| slot.poll(cx.waker())));
        assert_eq!(got, Ok(8));
        assert_balanced(&pool, &[&slot]);
    });
}

/// A polled receiver versus a shortening (which wakes it to re-arm) and
/// the expiry that follows: the receiver ends `TimedOut` or with the
/// reply, never blocked on a taken waker.
#[test]
fn polled_receiver_vs_shorten_and_reply() {
    model(|| {
        let clock = Clock::at(5);
        let pool = Arc::new(CreditPool::new(1));
        let slot = deferred(&pool, 100);
        let owner = {
            let (slot, clock) = (slot.clone(), clock.clone());
            loom::thread::spawn(move || {
                slot.shorten(5, &clock.now());
                slot.reply(2).0
            })
        };
        let got = loom::future::block_on(std::future::poll_fn(|cx| slot.poll(cx.waker())));
        assert_eq!(got, Err(Terminal::TimedOut));
        assert_eq!(owner.join().unwrap(), Delivery::Expired);
        assert_balanced(&pool, &[&slot]);
    });
}

/// The dispatching envelope lends its slot to the turn (no reference taken
/// per message): the in-turn reply goes through the borrow while the
/// caller takes the value or abandons, and either side may drop the last
/// reference. The value is dropped exactly once, by whoever holds it last,
/// and never while the other side still reads it.
#[test]
fn lent_reply_vs_receiver_last_reference() {
    use loom::sync::atomic::AtomicUsize;
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let lent: Arc<Slot<Counted>> = Arc::new(Slot::new(NEVER));
        let receiver = {
            let slot = lent.clone();
            loom::thread::spawn(move || match slot.try_take(&|| 0) {
                Some(Ok(value)) => {
                    drop(value);
                    true
                }
                Some(Err(_)) => false,
                None => {
                    slot.abandon();
                    false
                }
            })
        };
        let borrowed: &Arc<Slot<Counted>> = &lent;
        let (delivery, back) = borrowed.reply_in_turn(Counted(drops.clone()));
        drop(back);
        drop(lent);
        let took = receiver.join().unwrap();
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the value is dropped exactly once"
        );
        if took {
            assert_eq!(delivery, Delivery::Delivered);
        }
    });
}

/// The caller takes a reply without the slot lock once it owns the slot
/// alone (`Arc::try_unwrap`, then `into_reply`), while the owner's turn
/// replies through its lent reference and drops it: the value is the
/// replied one, taken exactly once, by the lock-free path or the locked one.
#[test]
fn unshared_take_vs_lent_reply_and_release() {
    model(|| {
        let lent: Arc<Slot<u32>> = Arc::new(Slot::new(NEVER));
        let mut caller = Some(lent.clone());
        let owner = loom::thread::spawn(move || {
            let (delivery, back) = lent.reply_in_turn(6);
            drop(lent);
            (delivery, back)
        });
        let got = loop {
            let shared = caller.take().expect("the caller holds its slot");
            if state(shared.word()) == slot::REPLIED && Arc::strong_count(&shared) == 1 {
                match Arc::try_unwrap(shared) {
                    Ok(owned) => break owned.into_reply(),
                    Err(shared) => caller = Some(shared),
                }
            } else {
                caller = Some(shared);
            }
            let shared = caller.as_ref().expect("put back");
            if let Some(result) = shared.try_take(&|| 0) {
                break result.ok();
            }
            loom::thread::yield_now();
        };
        assert_eq!(got, Some(6));
        assert_eq!(owner.join().unwrap(), (Delivery::Delivered, None));
    });
}
