//! Loom model of the link table and the monitor fence
//! (docs/design/otp-exit-signals.md E4-E6). The production protocols
//! (`src/link/table.rs`, `src/monitor/fence.rs`) are compiled here with
//! Loom's `Mutex`; `ActorId`, `ExitReason` and `SignalTarget` are stand-ins.
//! Applying a notice (production: `deliver_signal` on the receiver's exit
//! request, under the receiver's table lock) is modelled as appending to the
//! receiver's log under its own lock. Table `Arc`/`Weak` are `std` (only the
//! locks are explored; reference counts are not part of the protocol).
//!
//! Properties:
//! - at most one notice per edge generation, and none after the receiver's
//!   `unlink` returned or after the receiver's own death point;
//! - a `link` racing the peer's death point gets exactly one notice: the
//!   real reason (linked first) or a late `NoProc` (linked after D), never
//!   both and never none; `try_link` after D gets none;
//! - at most one `Down` per monitor, none after `demonitor` returned or after
//!   the observer's own close; a monitor racing the target's D gets exactly
//!   one `Down`: the real reason or `NoProc`.
//!
//! Run: `RUSTFLAGS="--cfg spawned_scheduler_loom" cargo test --release
//! -p spawned-concurrency --no-default-features --test link_fence_loom`.
#![cfg(spawned_scheduler_loom)]

mod sync {
    pub(crate) use loom::sync::{Mutex, MutexGuard};
    pub(crate) use std::sync::{Arc, Weak};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ActorId(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExitReason {
    Panic,
    Shutdown,
    NoProc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Notice {
    from: ActorId,
    gen: u64,
    reason: ExitReason,
}

/// The receiver side of a notice: its applied notices and fenced edges.
pub(crate) struct SignalTarget {
    id: ActorId,
    log: loom::sync::Mutex<Vec<Notice>>,
    /// Generations whose `unlink` (or the receiver's death) has returned.
    fenced: loom::sync::Mutex<Vec<u64>>,
}

#[allow(dead_code)]
#[path = "../src/link/table.rs"]
mod table;

#[allow(dead_code)]
#[path = "../src/monitor/fence.rs"]
mod fence;

use loom::sync::atomic::{AtomicBool, Ordering};
use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};
use std::sync::Arc;
use table::{LinkOutcome, LinkTable};

fn model(name: &'static str, f: impl Fn() + Sync + Send + 'static) {
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = runs.clone();
    // Unbounded: the protocols are lock-based and small enough to explore
    // every interleaving.
    let builder = loom::model::Builder::new();
    builder.check(move || {
        counted.fetch_add(1, StdOrdering::Relaxed);
        f()
    });
    eprintln!("{name}: {} executions", runs.load(StdOrdering::Relaxed));
}

struct Actor {
    target: Arc<SignalTarget>,
    links: LinkTable,
}

impl Actor {
    fn new(id: u64) -> Self {
        Self {
            target: Arc::new(SignalTarget {
                id: ActorId(id),
                log: loom::sync::Mutex::new(Vec::new()),
                fenced: loom::sync::Mutex::new(Vec::new()),
            }),
            links: table::new_link_table(),
        }
    }
    fn id(&self) -> ActorId {
        self.target.id
    }
    fn log(&self) -> Vec<Notice> {
        self.target.log.lock().unwrap().clone()
    }
    fn fence(&self, gen: u64) {
        self.target.fenced.lock().unwrap().push(gen);
    }
    fn records(&self) -> usize {
        table::lock(&self.links).entries.len()
    }
}

/// `deliver_notice`: the receiver-side fenced step.
fn notice(links: &LinkTable, receiver: &SignalTarget, from: ActorId, gen: u64, reason: ExitReason) {
    drop(table::take_record(links, from, gen, |record| {
        assert_eq!(record.late, reason == ExitReason::NoProc);
        assert!(
            !receiver.fenced.lock().unwrap().contains(&gen),
            "notice of edge {gen} took effect after unlink or death returned"
        );
        let mut log = receiver.log.lock().unwrap();
        assert!(
            log.iter().all(|n| n.gen != gen),
            "a second notice of edge {gen}"
        );
        log.push(Notice { from, gen, reason });
    }));
}

/// `link_in_section` for a running caller.
fn link(own: &Actor, peer: &Actor, gen: u64, try_only: bool) -> LinkOutcome {
    let outcome = table::install(
        &own.target,
        &own.links,
        &peer.target,
        &peer.links,
        gen,
        try_only,
    );
    if outcome == LinkOutcome::NoProc && !try_only {
        notice(&own.links, &own.target, peer.id(), gen, ExitReason::NoProc);
    }
    outcome
}

/// `unregister_link`, then the edge is fenced on both sides.
fn unlink(own: &Actor, peer: &Actor, gen: u64) {
    drop(table::remove_pair(
        own.id(),
        &own.links,
        peer.id(),
        &peer.links,
    ));
    own.fence(gen);
}

/// The death point D, then publication (the notices taken at D).
fn die(actor: &Actor, reason: ExitReason, fenced: &[u64]) {
    let notices = table::close(actor.id(), &actor.links, &reason, |_| {});
    // Past D nothing takes effect on this actor: its records are gone.
    for gen in fenced {
        actor.fence(*gen);
    }
    notices.deliver_with(notice);
}

fn spawn<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> loom::thread::JoinHandle<T> {
    loom::thread::spawn(f)
}

/// `unlink` racing the peer's death point and publication: the notice
/// takes effect only before `unlink` returned, at most once.
#[test]
fn unlink_fences_a_death_notice() {
    model("unlink_fences_a_death_notice", || {
        let (a, b) = (Arc::new(Actor::new(1)), Arc::new(Actor::new(2)));
        assert_eq!(link(&b, &a, 1, false), LinkOutcome::Linked);
        let dying = {
            let a = a.clone();
            spawn(move || die(&a, ExitReason::Panic, &[]))
        };
        unlink(&b, &a, 1);
        dying.join().unwrap();
        assert!(b.log().len() <= 1);
        assert!(a.log().is_empty());
        assert_eq!(a.records() + b.records(), 0);
    });
}

/// `link` racing the peer's death point: exactly one notice, the real
/// reason or a late `NoProc`; `try_link` after D gets none and no record.
#[test]
fn link_racing_death_gets_exactly_one_notice() {
    for try_only in [false, true] {
        model("link_racing_death_gets_exactly_one_notice", move || {
            let (a, b) = (Arc::new(Actor::new(1)), Arc::new(Actor::new(2)));
            let dying = {
                let a = a.clone();
                spawn(move || die(&a, ExitReason::Shutdown, &[]))
            };
            let outcome = link(&b, &a, 1, try_only);
            dying.join().unwrap();
            let log = b.log();
            match outcome {
                LinkOutcome::Linked => assert_eq!(
                    log,
                    vec![Notice {
                        from: a.id(),
                        gen: 1,
                        reason: ExitReason::Shutdown
                    }]
                ),
                LinkOutcome::NoProc if try_only => assert!(log.is_empty()),
                LinkOutcome::NoProc => assert_eq!(
                    log,
                    vec![Notice {
                        from: a.id(),
                        gen: 1,
                        reason: ExitReason::NoProc
                    }]
                ),
                LinkOutcome::Unchanged => panic!("a first link is never unchanged"),
            }
            assert_eq!(a.records() + b.records(), 0);
        });
    }
}

/// Unlink then relink while the old edge's notice is in flight: the old
/// generation never takes effect after the unlink; the new one gets exactly
/// one notice (the real reason, or `NoProc` past D).
#[test]
fn relink_after_unlink_ignores_the_stale_generation() {
    model("relink_after_unlink_ignores_the_stale_generation", || {
        let (a, b) = (Arc::new(Actor::new(1)), Arc::new(Actor::new(2)));
        assert_eq!(link(&b, &a, 1, false), LinkOutcome::Linked);
        let dying = {
            let a = a.clone();
            spawn(move || die(&a, ExitReason::Panic, &[]))
        };
        unlink(&b, &a, 1);
        let outcome = link(&b, &a, 2, false);
        dying.join().unwrap();
        let log = b.log();
        assert!(log.iter().filter(|n| n.gen == 1).count() <= 1);
        let fresh: Vec<_> = log.iter().filter(|n| n.gen == 2).collect();
        assert_eq!(fresh.len(), 1, "{outcome:?}: {log:?}");
        let expected = match outcome {
            LinkOutcome::Linked => ExitReason::Panic,
            LinkOutcome::NoProc => ExitReason::NoProc,
            LinkOutcome::Unchanged => panic!("the relink found a stale record"),
        };
        assert_eq!(fresh[0].reason, expected);
        assert_eq!(a.records() + b.records(), 0);
    });
}

/// Three parties: `a` dies while `b` (linked) unlinks and `c` links to it.
/// `b` gets its notice only before its unlink returned; `c` gets exactly
/// one notice (the real reason or a late `NoProc`).
#[test]
fn death_racing_unlink_and_a_new_link() {
    model("death_racing_unlink_and_a_new_link", || {
        let (a, b, c) = (
            Arc::new(Actor::new(1)),
            Arc::new(Actor::new(2)),
            Arc::new(Actor::new(3)),
        );
        assert_eq!(link(&b, &a, 1, false), LinkOutcome::Linked);
        let dying = {
            let a = a.clone();
            spawn(move || die(&a, ExitReason::Panic, &[]))
        };
        let linking = {
            let (a, c) = (a.clone(), c.clone());
            spawn(move || link(&c, &a, 2, false))
        };
        unlink(&b, &a, 1);
        dying.join().unwrap();
        let outcome = linking.join().unwrap();
        assert!(b.log().len() <= 1);
        let expected = match outcome {
            LinkOutcome::Linked => ExitReason::Panic,
            LinkOutcome::NoProc => ExitReason::NoProc,
            LinkOutcome::Unchanged => panic!("a first link is never unchanged"),
        };
        assert_eq!(
            c.log(),
            vec![Notice {
                from: a.id(),
                gen: 2,
                reason: expected
            }]
        );
        assert_eq!(a.records() + b.records() + c.records(), 0);
    });
}

/// Both ends die at once: each notice takes effect at most once, and never
/// on a receiver past its own death point (its close took the record).
#[test]
fn mutual_death_delivers_at_most_one_notice() {
    model("mutual_death_delivers_at_most_one_notice", || {
        let (a, b) = (Arc::new(Actor::new(1)), Arc::new(Actor::new(2)));
        assert_eq!(link(&a, &b, 1, false), LinkOutcome::Linked);
        let dying = {
            let a = a.clone();
            spawn(move || die(&a, ExitReason::Panic, &[1]))
        };
        die(&b, ExitReason::Shutdown, &[1]);
        dying.join().unwrap();
        assert!(a.log().len() + b.log().len() <= 1);
        assert_eq!(a.records() + b.records(), 0);
    });
}

/// Concurrent `link` from both ends: one edge, symmetric records.
#[test]
fn concurrent_links_install_one_edge() {
    model("concurrent_links_install_one_edge", || {
        let (a, b) = (Arc::new(Actor::new(1)), Arc::new(Actor::new(2)));
        let other = {
            let (a, b) = (a.clone(), b.clone());
            spawn(move || link(&b, &a, 2, false))
        };
        let mine = link(&a, &b, 1, false);
        let theirs = other.join().unwrap();
        let outcomes = [mine, theirs];
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| **o == LinkOutcome::Linked)
                .count(),
            1
        );
        assert!(outcomes.contains(&LinkOutcome::Unchanged));
        assert_eq!((a.records(), b.records()), (1, 1));
        // Release the records (no cycle is left behind).
        die(&a, ExitReason::Panic, &[]);
        die(&b, ExitReason::Panic, &[]);
    });
}

// ---------------------------------------------------------------------------
// Monitors
// ---------------------------------------------------------------------------

type Monitors = fence::Entries<u64, ()>;

/// A target: its exit table (D) and its publication flag.
struct Target {
    actor: Actor,
    published: AtomicBool,
}

impl Target {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            actor: Actor::new(9),
            published: AtomicBool::new(false),
        })
    }
    fn is_past_death(&self) -> bool {
        table::lock(&self.actor.links).is_dead()
    }
    fn die(&self) {
        drop(table::close(
            self.actor.id(),
            &self.actor.links,
            &ExitReason::Panic,
            |_| {},
        ));
        self.published.store(true, Ordering::Release);
    }
}

/// The observer: its monitors, mailbox of `Down`s and fence flag.
struct Observer {
    monitors: Monitors,
    downs: loom::sync::Mutex<Vec<ExitReason>>,
    fenced: AtomicBool,
    cancelled: AtomicBool,
}

impl Observer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            monitors: loom::sync::Mutex::new(Some(Default::default())),
            downs: loom::sync::Mutex::new(Vec::new()),
            fenced: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        })
    }
    fn enqueue(&self, reason: ExitReason) {
        assert!(
            !self.fenced.load(Ordering::Acquire),
            "a Down was enqueued after demonitor or the observer's close returned"
        );
        let mut downs = self.downs.lock().unwrap();
        assert!(downs.is_empty(), "a second Down");
        downs.push(reason);
    }
    fn downs(&self) -> Vec<ExitReason> {
        self.downs.lock().unwrap().clone()
    }
    fn register(&self, target: &Target) -> fence::Registration {
        fence::register(
            &self.monitors,
            1,
            (),
            || false,
            || target.is_past_death(),
            || self.enqueue(ExitReason::NoProc),
            || {},
        )
    }
    /// The waiter: woken by publication or by its token.
    fn waiter(self: &Arc<Self>, target: &Arc<Target>) -> loom::thread::JoinHandle<bool> {
        let (observer, target) = (self.clone(), target.clone());
        spawn(move || loop {
            if observer.cancelled.load(Ordering::Acquire) {
                return false;
            }
            if target.published.load(Ordering::Acquire) {
                return fence::deliver(&observer.monitors, &1, || {
                    observer.enqueue(ExitReason::Panic)
                });
            }
            loom::thread::yield_now();
        })
    }
}

/// `demonitor` racing the target's death and the waiter: once it returns no
/// `Down` is enqueued; if it found no entry, the `Down` was delivered.
#[test]
fn demonitor_fences_the_down() {
    model("demonitor_fences_the_down", || {
        let (observer, target) = (Observer::new(), Target::new());
        assert_eq!(observer.register(&target), fence::Registration::Active);
        let waiter = observer.waiter(&target);
        let dying = {
            let target = target.clone();
            spawn(move || target.die())
        };
        let removed = fence::remove(&observer.monitors, &1, || {});
        observer.fenced.store(true, Ordering::Release);
        if removed.is_some() {
            observer.cancelled.store(true, Ordering::Release);
        }
        dying.join().unwrap();
        let delivered = waiter.join().unwrap();
        assert_eq!(delivered, removed.is_none());
        let expected = if removed.is_some() {
            vec![]
        } else {
            vec![ExitReason::Panic]
        };
        assert_eq!(observer.downs(), expected);
    });
}

/// `monitor` racing the target's death point: exactly one `Down`, the real
/// reason (registered before D) or `NoProc` (after D).
#[test]
fn monitor_racing_death_gets_exactly_one_down() {
    model("monitor_racing_death_gets_exactly_one_down", || {
        let (observer, target) = (Observer::new(), Target::new());
        let dying = {
            let target = target.clone();
            spawn(move || target.die())
        };
        // Seen published before the call: the registration is after D.
        let dead_before = target.published.load(Ordering::Acquire);
        let registration = observer.register(&target);
        if dead_before {
            assert_eq!(registration, fence::Registration::Late, "monitor after D");
        }
        let delivered = match registration {
            fence::Registration::Active => observer.waiter(&target).join().unwrap(),
            fence::Registration::Late => false,
            fence::Registration::Inert => panic!("a running observer is never inert"),
        };
        dying.join().unwrap();
        let expected = match registration {
            fence::Registration::Active => {
                assert!(delivered);
                ExitReason::Panic
            }
            _ => ExitReason::NoProc,
        };
        assert_eq!(observer.downs(), vec![expected]);
    });
}

/// The observer's own exit racing the delivery: at most one `Down`, none
/// after the close returned; a monitor after the close is inert.
#[test]
fn observer_close_fences_the_down() {
    model("observer_close_fences_the_down", || {
        let (observer, target) = (Observer::new(), Target::new());
        assert_eq!(observer.register(&target), fence::Registration::Active);
        let waiter = observer.waiter(&target);
        let dying = {
            let target = target.clone();
            spawn(move || target.die())
        };
        let taken = fence::close(&observer.monitors);
        observer.fenced.store(true, Ordering::Release);
        if taken.is_some_and(|entries| !entries.is_empty()) {
            observer.cancelled.store(true, Ordering::Release);
        }
        assert_eq!(observer.register(&target), fence::Registration::Inert);
        dying.join().unwrap();
        let delivered = waiter.join().unwrap();
        assert_eq!(observer.downs().len(), usize::from(delivered));
    });
}
