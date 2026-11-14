//! Per-instance reply registry (docs/design/deferred-replies.md §4, §5).
//!
//! The registry holds the owner's credit pool, a list of weak references to
//! its deferred tokens' slots, and a phase that classifies reply drops:
//!
//! | phase | a dropped reply obligation |
//! |---|---|
//! | `InTurn` | recorded on the turn; committed at turn end: `Dropped` if the handler returned, exit pending if it panicked |
//! | `Running` | `Dropped` at once |
//! | `Exiting` | exit pending |
//! | `Dead` | answered `Stopped` at once (the final reason is known) |
//!
//! **Exit pending** (docs/design/otp-exit-signals.md, "Required changes to
//! rank 11"): the slot stays `OPEN` and listed, and the death point answers
//! it with the owner's final reason, so callers, `Down` and `wait_exit` see
//! one reason. Only the caller's own deadline (or abandonment) completes it
//! earlier; nobody holds a token that could still reply.
//!
//! The final reason is stored once, at the death point ([`Registry::seal`]),
//! before any slot of this owner becomes `STOPPED`; the close copies it into
//! each slot it closes before the transition, and a caller that reads a
//! `STOPPED` slot reads it from there (a slot does not keep its owner alive).
//!
//! No rule uses `thread::panicking()`. The phase is one atomic so a turn costs
//! two uncontended atomic stores; the turn list is locked only when a drop
//! lands on it (`IN_TURN_PENDING`).

use super::deadline::{self, DeadlineHeap, Expirable};
use super::slot::{CreditPool, Slot, Terminal, Wake, NEVER};
use crate::{ActorId, ExitReason};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Default `Actor::MAX_OUTSTANDING`: deferred credits per actor instance.
pub(crate) const DEFAULT_MAX_OUTSTANDING: usize = 1024;

use super::turn::{Outcome, TurnPhase, DEAD, EXITING};

/// A slot seen without its reply type.
pub(crate) trait Closeable: Send + Sync {
    /// Close without waking; the returned work runs with `run_wake`.
    fn close_collect(&self) -> Option<Wake>;
    /// Record the owner's final reason before closing.
    fn set_stopped_reason(&self, reason: &ExitReason);
    fn run_wake(&self, wake: Wake);
    fn notify_parked(&self, wake: &mut Wake);
    fn terminate(&self, to: Terminal) -> bool;
    fn is_terminal(&self) -> bool;
}

/// A closed slot and its deferred wake-up.
/// A slot met by the death point's discharge, retained (whatever its state:
/// an unread reply's value is destroyed only after every caller is answered)
/// with the wake-up its close collected, if it closed it.
type PendingWake = (Arc<dyn Closeable>, Option<Wake>);

impl<R: Send> Expirable for Slot<R> {
    fn pending_deadline(&self) -> Option<u64> {
        let deadline = self.deadline();
        (!self.is_terminal() && deadline != NEVER).then_some(deadline)
    }
    fn expire(&self, now: u64) {
        self.check_deadline(&|| now);
    }
}

impl<R: Send> Closeable for Slot<R> {
    fn close_collect(&self) -> Option<Wake> {
        Slot::close_collect(self)
    }
    fn set_stopped_reason(&self, reason: &ExitReason) {
        Slot::set_stopped_reason(self, reason.clone())
    }
    fn run_wake(&self, wake: Wake) {
        Slot::run_wake(self, wake)
    }
    fn notify_parked(&self, wake: &mut Wake) {
        Slot::notify_parked(self, wake)
    }
    fn terminate(&self, to: Terminal) -> bool {
        Slot::terminate(self, to)
    }
    fn is_terminal(&self) -> bool {
        Slot::is_terminal(self)
    }
}

#[derive(Default)]
struct Lists {
    tokens: Vec<Weak<dyn Closeable>>,
    prune_at: usize,
    deadlines: DeadlineHeap,
}

pub(crate) struct Registry {
    owner: ActorId,
    /// Service key, unique per registry.
    #[cfg_attr(all(spawned_sim, feature = "async"), allow(dead_code))]
    key: u64,
    pool: Arc<CreditPool>,
    /// Turn phase and in-turn drops (its phase sits on its own cache line).
    turn: TurnPhase<Arc<dyn Closeable>>,
    lists: Mutex<Lists>,
    /// When exit began: the cleanup deadline is armed from here.
    exit_began: Mutex<Option<spawned_rt::time::Instant>>,
    /// The owner's final exit reason, set once at its death point. Set means
    /// the mailbox refuses admission (`NoProc`).
    final_reason: OnceLock<ExitReason>,
    /// Reply admission for deferred tokens answered off the owner's turn:
    /// closed at the death point, before the final reason is visible, so no
    /// reply succeeds once `NoProc` is observable.
    replies: crate::mailbox::SendGate,
    /// Exit-signal admission for this owner's contexts (`ctx.exit`): closed
    /// at the death point, before the death notices are taken, so a signal
    /// sent before death is delivered before them.
    signals: crate::mailbox::SendGate,
    /// While the death point discharges (from its close until
    /// [`end_discharge`](Self::end_discharge)): the wake-ups of the slots it
    /// closed, run only once every obligation is answered, so no waker runs
    /// between two closes.
    discharge: Mutex<Option<Vec<PendingWake>>>,
    #[cfg(all(spawned_sim, feature = "async"))]
    sim: deadline::SimService,
}

fn lock(lists: &Mutex<Lists>) -> std::sync::MutexGuard<'_, Lists> {
    lists.lock().unwrap_or_else(|p| p.into_inner())
}

impl Registry {
    pub(crate) fn new(owner: ActorId, max_outstanding: usize) -> Arc<Self> {
        Self::create(owner, max_outstanding, false)
    }

    /// Native actors keep real-time deadlines even in a simulation build.
    pub(crate) fn new_native(owner: ActorId, max_outstanding: usize) -> Arc<Self> {
        Self::create(owner, max_outstanding, true)
    }

    #[cfg(spawned_sim)]
    pub(crate) fn for_test() -> Arc<Self> {
        #[cfg(feature = "async")]
        let native = spawned_rt::tasks::Handle::try_current_or_ambient().is_err();
        #[cfg(not(feature = "async"))]
        let native = true;
        Self::create(ActorId::next(), usize::MAX, native)
    }

    fn create(owner: ActorId, max_outstanding: usize, _native: bool) -> Arc<Self> {
        let max = max_outstanding.min(super::slot::MAX_JOBS as usize);
        static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
        let registry = Arc::new(Self {
            owner,
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            pool: Arc::new(CreditPool::new(max)),
            turn: TurnPhase::default(),
            lists: Mutex::new(Lists {
                prune_at: 16,
                ..Lists::default()
            }),
            exit_began: Mutex::new(None),
            final_reason: OnceLock::new(),
            replies: crate::mailbox::SendGate::default(),
            signals: crate::mailbox::SendGate::default(),
            discharge: Mutex::new(None),
            #[cfg(all(spawned_sim, feature = "async"))]
            sim: if _native {
                deadline::SimService::native()
            } else {
                deadline::SimService::default()
            },
        });
        keys()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(registry.key, Arc::downgrade(&registry));
        registry
    }

    /// The live registry with this `key` (a raw request's slot records it).
    #[cfg_attr(not(feature = "async"), allow(dead_code))] // awaited receivers only
    pub(crate) fn by_key(key: u64) -> Option<Arc<Self>> {
        let weak = keys()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned()?;
        weak.upgrade()
    }

    /// The registry of replies not tied to an actor (`reply_pair`): always
    /// between turns, with the largest bound.
    #[cfg(not(spawned_sim))]
    pub(crate) fn detached() -> &'static Arc<Self> {
        static DETACHED: OnceLock<Arc<Registry>> = OnceLock::new();
        DETACHED.get_or_init(|| Self::new(ActorId::next(), usize::MAX))
    }

    pub(crate) fn pool(&self) -> &Arc<CreditPool> {
        &self.pool
    }

    pub(crate) fn owner(&self) -> ActorId {
        self.owner
    }

    /// Credits in use: live deferred tokens and running linked jobs.
    pub(crate) fn outstanding(&self) -> usize {
        self.pool.used()
    }

    /// List a newly deferred token. Called by `defer()` after its CAS, in the
    /// owner's turn, so it never races the close.
    pub(crate) fn register<R: Send + 'static>(&self, slot: &Arc<Slot<R>>) {
        let weak: Weak<dyn Closeable> = Arc::downgrade(slot) as Weak<dyn Closeable>;
        let mut lists = lock(&self.lists);
        if lists.tokens.len() < lists.prune_at {
            lists.tokens.push(weak);
            return;
        }
        // Prune outside the lock: a temporary upgrade can become a slot's
        // last strong reference, and dropping it drops an unread reply (user
        // code, which may itself drop a token of this owner).
        let listed = std::mem::take(&mut lists.tokens);
        drop(lists);
        let mut keep = Vec::with_capacity(listed.len());
        let mut upgraded = Vec::new();
        for token in listed {
            if let Some(slot) = token.upgrade() {
                if !slot.is_terminal() {
                    keep.push(token);
                }
                upgraded.push(slot);
            }
        }
        let mut lists = lock(&self.lists);
        keep.append(&mut lists.tokens);
        lists.prune_at = (keep.len() * 2).max(16);
        lists.tokens = keep;
        lists.tokens.push(weak);
        drop(lists);
        drop(upgraded);
    }

    #[cfg_attr(all(spawned_sim, feature = "async"), allow(dead_code))]
    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    #[cfg(all(spawned_sim, feature = "async"))]
    pub(crate) fn sim_service(&self) -> &deadline::SimService {
        &self.sim
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.turn.load() == DEAD
    }

    /// Put a deferred token's deadline in the owner's heap, so it expires
    /// (and returns its credit) even when no caller polls.
    pub(crate) fn track_deadline<R: Send + 'static>(self: &Arc<Self>, slot: &Arc<Slot<R>>) {
        let at = slot.deadline();
        if at == NEVER || slot.is_terminal() {
            return;
        }
        let weak: Weak<dyn Expirable> = Arc::downgrade(slot) as Weak<dyn Expirable>;
        self.track(at, weak);
    }

    /// Add a heap entry; reschedule the service on a new earliest deadline.
    pub(crate) fn track(self: &Arc<Self>, at: u64, item: Weak<dyn Expirable>) {
        let (earliest, upgraded) = {
            let mut lists = lock(&self.lists);
            if self.is_dead() {
                return;
            }
            lists.deadlines.push(at, item)
        };
        // Compaction's temporary upgrades are dropped outside the lock.
        drop(upgraded);
        if earliest {
            deadline::schedule(self, at);
        }
    }

    /// Entries in the deadline heap, live and stale (bounded by compaction).
    #[cfg(test)]
    pub(crate) fn heap_entries(&self) -> usize {
        lock(&self.lists).deadlines.len()
    }

    /// The service visits: expire every due entry outside the lock, and
    /// return the next earliest deadline.
    ///
    /// Entries run framework code that can reach user code (a waker, a
    /// payload destructor the entry did not route to its failure sink, the
    /// last reference to a job's `done`). Each runs contained, so one
    /// failing entry neither skips the others nor ends the shared service.
    pub(crate) fn service_deadlines(&self, now: u64) -> Option<u64> {
        let due = lock(&self.lists).deadlines.due(now);
        for item in due {
            contain_entry(|| {
                if item.pending_deadline().is_some_and(|d| d <= now) {
                    item.expire(now);
                }
                // Possibly the last reference: dropped inside the guard.
                drop(item);
            });
        }
        lock(&self.lists).deadlines.earliest()
    }

    /// A reply obligation (`PendingReply` or `ReplyTo`) was dropped unanswered.
    #[inline]
    pub(crate) fn dropped<R: Send + 'static>(&self, slot: Arc<Slot<R>>) {
        if slot.is_terminal() {
            return;
        }
        let slot: Arc<dyn Closeable> = slot;
        if let Some((slot, outcome)) = self.turn.dropped(slot) {
            self.commit(vec![slot], outcome);
        }
    }

    /// Answer `drops`: `Dropped` at once, or (`Stopped`) exit pending until
    /// the death point.
    fn commit(&self, drops: Vec<Arc<dyn Closeable>>, outcome: Outcome) {
        for slot in drops {
            match outcome {
                Outcome::Dropped => {
                    slot.terminate(Terminal::Dropped);
                }
                Outcome::Stopped => self.hold(slot),
            }
        }
    }

    /// Exit pending: keep `slot` open and listed so the death point answers
    /// it with the final reason. Past the death point, answer it now.
    fn hold(&self, slot: Arc<dyn Closeable>) {
        let mut lists = lock(&self.lists);
        // `close` sets `DEAD` before it takes the list under this lock, so a
        // slot listed here is either taken by that close or closed below.
        if self.is_dead() {
            drop(lists);
            self.close_slot(slot.clone());
        } else {
            lists.tokens.push(Arc::downgrade(&slot));
            drop(lists);
        }
        // The caller's receiver may have been the other strong reference:
        // dropping the last one runs no user code (the slot holds no value).
        drop(slot);
    }

    /// An undispatched envelope dropped its request (mailbox disposal, a
    /// rejected enqueue): answered at the death point with the final reason.
    pub(crate) fn dispose<R: Send + 'static>(&self, slot: Arc<Slot<R>>) {
        if !slot.is_terminal() {
            self.hold(slot);
        }
    }

    /// Admit a deferred reply's linearization (its `OPEN -> REPLIED` CAS),
    /// or `None` once the owner reached its death point: the slot then stays
    /// listed and the registry's close answers it with the final reason.
    pub(crate) fn reply_permit(&self) -> Option<crate::mailbox::SendPermit<'_>> {
        self.replies.enter()
    }

    /// Admit an exit signal sent by this owner, held until it is delivered;
    /// `None` once the owner reached its death point (the signal is inert).
    pub(crate) fn signal_permit(&self) -> Option<crate::mailbox::SendPermit<'_>> {
        self.signals.enter()
    }

    /// The death point's fences, before the final reason is visible: refuse
    /// later replies and exit signals, and wait for those in flight (a CAS,
    /// or one signal delivery: no user code). Never call it inside a
    /// lifecycle-trace section: an in-flight delivery may need one.
    pub(crate) fn fence(&self) {
        self.replies.close();
        self.signals.close();
    }

    /// The death point D (called inside the owner's exit-table critical
    /// section): fix the final reason callers will see and refuse admission.
    /// Idempotent: the first reason stays.
    pub(crate) fn seal(&self, reason: &ExitReason) {
        let _ = self.final_reason.set(reason.clone());
    }

    /// The owner's final reason, once it reached its death point.
    pub(crate) fn final_reason(&self) -> Option<&ExitReason> {
        self.final_reason.get()
    }

    /// Whether the owner is past its death point: sends and requests report
    /// `NoProc`.
    pub(crate) fn refuses_admission(&self) -> bool {
        self.final_reason.get().is_some()
    }

    /// Created only by the owner's handler turn (or a standalone test
    /// reply pair). A job may move an existing token, never create one
    /// concurrently with an owner turn: end_turn's zero-token fast path
    /// relies on that admission rule.
    pub(crate) fn token_added(&self) {
        self.turn.external_added();
    }

    #[cfg(all(test, feature = "async"))]
    pub(crate) fn external_count(&self) -> usize {
        self.turn.external_count()
    }

    /// Keep the ownership hold through every callback run by token drop,
    /// including unwinding from a foreign waker.
    pub(crate) fn token_drop_guard(&self) -> impl Drop + '_ {
        struct Guard<'a>(&'a TurnPhase<Arc<dyn Closeable>>);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.external_finished();
            }
        }
        Guard(&self.turn)
    }

    /// The run loop is about to dispatch a handler.
    #[inline]
    pub(crate) fn begin_turn(&self) {
        self.turn.begin_turn();
    }

    /// The handler returned (`panicked == false`) or unwound.
    #[inline]
    pub(crate) fn end_turn(&self, panicked: bool) {
        let (drops, outcome) = self.turn.end_turn(panicked);
        // Most turns drop no obligation: skip the commit entirely.
        if !drops.is_empty() {
            self.commit(drops, outcome);
        }
    }

    /// Exit begins: later drops, and the current turn's recorded drops, are
    /// exit pending until the death point.
    pub(crate) fn exiting(&self) {
        if self.turn.load() == DEAD {
            return;
        }
        let first = {
            let mut began = self.exit_began.lock().unwrap_or_else(|p| p.into_inner());
            let first = began.is_none();
            began.get_or_insert_with(spawned_rt::time::Instant::now);
            first
        };
        self.commit(self.turn.exiting(), Outcome::Stopped);
        if first {
            crate::lifecycle_trace::reply(self.owner.raw(), "Exit", 0, "");
        }
    }

    /// What remains of `timeout`, measured from the start of exit.
    pub(crate) fn cleanup_remaining(&self, timeout: std::time::Duration) -> std::time::Duration {
        let began = *self.exit_began.lock().unwrap_or_else(|p| p.into_inner());
        began.map_or(timeout, |began| timeout.saturating_sub(began.elapsed()))
    }

    pub(crate) fn is_exiting(&self) -> bool {
        self.turn.load() >= EXITING
    }

    /// The death point starts discharging: closes from here until
    /// [`end_discharge`](Self::end_discharge) defer their wake-ups.
    pub(crate) fn begin_discharge(&self) {
        let mut batch = self.discharge.lock().unwrap_or_else(|p| p.into_inner());
        batch.get_or_insert_with(Vec::new);
    }

    /// Every obligation of the death point is answered (deferred slots and
    /// the queued mailbox): run the collected wake-ups outside every lock,
    /// parked blocking receivers first, so none waits behind an arbitrary
    /// waker; then the wakers (executor or user code); then release the
    /// retained slots (an unread reply's destructor is user code). Later
    /// closes wake at once. Idempotent.
    pub(crate) fn end_discharge(&self) {
        let mut batch = self
            .discharge
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_default();
        for (slot, wake) in &mut batch {
            if let Some(wake) = wake {
                slot.notify_parked(wake);
            }
        }
        for (slot, wake) in &mut batch {
            if let Some(wake) = wake.take() {
                slot.run_wake(wake);
            }
        }
        drop(batch);
    }

    /// A turn destroyed unfinished (its runtime dropped the actor): its
    /// caller is answered at the death point like every other one.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn answer_turn(&self, caller: Arc<dyn Closeable>) {
        self.close_slot(caller);
    }

    /// Close one slot as `STOPPED`, deferring its wake-up while discharging.
    /// While discharging, the slot itself is retained too, closed here or
    /// already terminal: this may be the last strong reference to an unread
    /// reply, whose destructor must not run before every caller is answered.
    fn close_slot(&self, slot: Arc<dyn Closeable>) -> bool {
        // The final reason is sealed before any close: the caller reads it
        // from the slot once it sees `STOPPED`.
        if let Some(reason) = self.final_reason() {
            slot.set_stopped_reason(reason);
        }
        let wake = slot.close_collect();
        let closed = wake.is_some();
        let mut batch = self.discharge.lock().unwrap_or_else(|p| p.into_inner());
        match batch.as_mut() {
            Some(batch) => batch.push((slot, wake)),
            None => {
                drop(batch);
                if let Some(wake) = wake {
                    slot.run_wake(wake);
                }
                drop(slot);
            }
        }
        closed
    }

    /// Death: every open token listed, and every exit-pending slot, becomes
    /// `STOPPED` with the final reason, wherever it is. Returns how many were
    /// closed.
    ///
    /// The run loops [`seal`](Self::seal) the reason at the death point
    /// first. A registry closed without it (its lifecycle was dropped by the
    /// runtime before the death point, G1) answers `Killed`.
    pub(crate) fn close(&self) -> usize {
        self.seal(&ExitReason::Killed);
        crate::lifecycle_trace::reply(self.owner.raw(), "Death", 0, "");
        for slot in self.turn.close() {
            self.close_slot(slot);
        }
        let tokens = std::mem::take(&mut lock(&self.lists).tokens);
        let closed = tokens
            .into_iter()
            .filter_map(|t| t.upgrade())
            .filter(|s| {
                fail_point!("reply.close_upgraded");
                self.close_slot(s.clone())
            })
            .count();
        // The heap is dropped after the close (§5 step 3.4), and the service
        // forgets this owner, so dead actors leave nothing behind.
        lock(&self.lists).deadlines.clear();
        deadline::forget(self);
        #[cfg(all(spawned_sim, feature = "async"))]
        self.sim.wake();
        closed
    }
}

/// Run one deadline-service step; a panic is logged and disposed of.
pub(crate) fn contain_entry(step: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(step)) {
        let message = crate::error::panic_message(&*payload);
        crate::mailbox::dispose_panic(payload);
        crate::actor_cleanup::log_safely(|| {
            tracing::error!(event = "deadline_entry_failed", %message,
                "a deadline entry panicked on the deadline service; servicing continues");
        });
    }
}

/// Registries by key, for a raw receiver that finds its owner by the key in
/// its slot's word (one insert per actor instance, one removal at its drop).
fn keys() -> &'static Mutex<std::collections::HashMap<u64, Weak<Registry>>> {
    static KEYS: OnceLock<Mutex<std::collections::HashMap<u64, Weak<Registry>>>> = OnceLock::new();
    KEYS.get_or_init(Default::default)
}

impl Drop for Registry {
    fn drop(&mut self) {
        let forgotten = keys()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.key);
        drop(forgotten);
        // Every handle to the instance is gone; nothing can still answer.
        if self.turn.load() != DEAD {
            self.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reply::slot::Slot;

    fn deferred(registry: &Arc<Registry>, deadline: u64) -> Arc<Slot<u32>> {
        let slot = Arc::new(Slot::new(deadline));
        slot.defer(registry.pool(), &|| 0).unwrap();
        registry.register(&slot);
        registry.track_deadline(&slot);
        slot
    }

    fn terminal_of<R>(slot: &Slot<R>) -> Option<Terminal> {
        Terminal::of(crate::reply::slot::state(slot.word()))
    }

    #[cfg(all(spawned_sim, feature = "async"))]
    #[test]
    fn deadline_service_reports_foreign_thread_escape_in_owner_world() {
        let runtime = spawned_rt::tasks::sim::Runtime::with_seed(17);
        let (registry, slot) = runtime.block_on(async {
            let registry = Registry::new(ActorId::next(), 1);
            let slot = Arc::new(Slot::<u32>::new(1_000_000));
            slot.defer(registry.pool(), &|| 0).unwrap();
            registry.register(&slot);
            (registry, slot)
        });
        let owner = registry.clone();
        let pending = slot.clone();
        // Arm the first deadline with neither an entered runtime nor an
        // ambient world on this thread. The service belongs to the owner.
        std::thread::spawn(move || owner.track_deadline(&pending))
            .join()
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                spawned_rt::tasks::sleep(std::time::Duration::from_millis(2)).await;
            });
        }));
        let panic = result.expect_err("foreign scheduling must fail the owning simulation");
        assert!(crate::error::panic_message(&*panic).contains("foreign thread"));
        crate::mailbox::dispose_panic(panic);
        registry.close();
    }

    #[cfg(all(spawned_sim, feature = "async"))]
    #[test]
    fn deadline_service_stays_on_its_owners_simulation() {
        let runtime = spawned_rt::tasks::sim::Runtime::with_seed(17);
        let (registry, slot) = runtime.block_on(async {
            let registry = Registry::new(ActorId::next(), 1);
            let slot = Arc::new(Slot::<u32>::new(1_000_000));
            slot.defer(registry.pool(), &|| 0).unwrap();
            registry.register(&slot);
            (registry, slot)
        });
        // Entering another world's handle on the same executor thread must
        // not transfer the owner's service to that world.
        let other = spawned_rt::tasks::sim::Runtime::with_seed(19);
        other.block_on(async {
            registry.track_deadline(&slot);
        });
        runtime.block_on(async {
            spawned_rt::tasks::sleep(std::time::Duration::from_millis(2)).await;
        });
        assert_eq!(terminal_of(&slot), Some(Terminal::TimedOut));
        assert_eq!(registry.outstanding(), 0);
        registry.close();
    }

    #[cfg(all(spawned_sim, feature = "async"))]
    #[test]
    fn deadline_service_setup_uses_ambient_world_and_runtime_drop_breaks_cycles() {
        let runtime = spawned_rt::tasks::sim::Runtime::with_seed(23);
        assert!(spawned_rt::tasks::Handle::try_current().is_err());
        let registry = Registry::new(ActorId::next(), 1);
        let slot = Arc::new(Slot::<u32>::new(1_000_000));
        slot.defer(registry.pool(), &|| 0).unwrap();
        registry.register(&slot);
        registry.track_deadline(&slot);
        runtime.block_on(async {
            spawned_rt::tasks::sleep(std::time::Duration::from_millis(2)).await;
        });
        assert_eq!(terminal_of(&slot), Some(Terminal::TimedOut));
        let weak = Arc::downgrade(&registry);
        let external = registry.clone();
        let payload = Arc::new(());
        let payload_weak = Arc::downgrade(&payload);
        runtime.spawn(async move {
            std::future::pending::<()>().await;
            drop((registry, payload));
        });
        drop(runtime);
        assert!(
            payload_weak.upgrade().is_none(),
            "an external registry must not retain pending task payloads"
        );
        assert!(
            weak.upgrade().is_some(),
            "the explicit external holder still owns its registry"
        );
        drop(external);
        assert!(
            weak.upgrade().is_none(),
            "runtime teardown must drain pending task cycles"
        );
    }

    #[cfg(all(spawned_sim, feature = "async"))]
    #[test]
    fn deadline_service_reply_pairs_have_fresh_owners_between_worlds() {
        for seed in 0..3 {
            let runtime = spawned_rt::tasks::sim::Runtime::with_seed(seed);
            let weak = runtime.block_on(async {
                let (direct, direct_receiver) =
                    crate::reply::reply_pair::<u32>(crate::Deadline::Never);
                let direct_owner = Arc::downgrade(direct.owned_registry.as_ref().unwrap());
                assert_eq!(direct.reply(7), crate::Delivery::Delivered);
                assert_eq!(direct_receiver.await.unwrap(), 7);
                assert!(
                    direct_owner.upgrade().is_none(),
                    "direct reply must release its standalone owner"
                );
                let (pending, receiver) = crate::reply::reply_pair::<u32>(crate::Deadline::after(
                    std::time::Duration::from_millis(1),
                ));
                let token = pending.defer().unwrap();
                let weak = Arc::downgrade(&token.registry);
                spawned_rt::tasks::sleep(std::time::Duration::from_millis(2)).await;
                assert!(matches!(
                    receiver.await,
                    Err(crate::ActorError::RequestTimeout)
                ));
                drop(token);
                weak
            });
            drop(runtime);
            assert!(
                weak.upgrade().is_none(),
                "test helper retained seed {seed}'s owner"
            );
        }
    }

    #[cfg(all(spawned_sim, feature = "async"))]
    #[test]
    fn deadline_service_native_reply_pair_does_not_create_a_world() {
        std::thread::spawn(|| {
            assert!(spawned_rt::tasks::Handle::try_current_or_ambient().is_err());
            let (pending, receiver) = crate::reply::reply_pair::<u32>(crate::Deadline::Never);
            assert!(
                spawned_rt::tasks::Handle::try_current_or_ambient().is_err(),
                "a native test helper must not change its caller's clock"
            );
            drop(pending.defer().unwrap());
            drop(receiver);
        })
        .join()
        .unwrap();
    }

    #[cfg(all(spawned_sim, feature = "async"))]
    #[test]
    fn deadline_service_rejects_mixed_native_and_virtual_clock_requests() {
        use crate::threads::{Actor, ActorStart, Context, Handler};
        struct Native;
        impl Actor for Native {}
        struct Probe;
        impl crate::message::Message for Probe {
            type Result = ();
        }
        impl Handler<Probe> for Native {
            fn handle(&mut self, _: Probe, reply: crate::PendingReply<'_, ()>, _: &Context<Self>) {
                reply.reply(());
            }
        }
        let actor = std::thread::spawn(|| Native.start()).join().unwrap();
        let runtime = spawned_rt::tasks::sim::Runtime::with_seed(29);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime
                .block_on(async { actor.send_request(Probe, std::time::Duration::from_millis(1)) })
        }));
        drop(runtime);
        actor.context().stop();
        actor.join();
        let panic = match result {
            Err(panic) => panic,
            Ok(_) => panic!("mixed clock request was admitted"),
        };
        assert!(crate::error::panic_message(&*panic).contains("native actors cannot run"));
        crate::mailbox::dispose_panic(panic);

        let runtime = spawned_rt::tasks::sim::Runtime::with_seed(31);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async { Native.start() })
        }));
        drop(runtime);
        let panic = match result {
            Err(panic) => panic,
            Ok(actor) => {
                actor.context().stop();
                actor.join();
                panic!("native actor started in simulated world")
            }
        };
        assert!(crate::error::panic_message(&*panic).contains("native actors cannot run"));
        crate::mailbox::dispose_panic(panic);
    }

    #[cfg(all(spawned_sim, feature = "async"))]
    #[test]
    fn deadline_service_for_native_owner_uses_real_time_in_sim_builds() {
        std::thread::spawn(|| {
            struct Probe(u64, std::sync::mpsc::Sender<()>);
            impl Expirable for Probe {
                fn pending_deadline(&self) -> Option<u64> {
                    Some(self.0)
                }
                fn expire(&self, _: u64) {
                    let _ = self.1.send(());
                }
            }
            assert!(spawned_rt::tasks::Handle::try_current().is_err());
            let registry = Registry::new_native(ActorId::next(), 1);
            let (sent, received) = std::sync::mpsc::channel();
            let at = super::super::now_ticks() + 20_000_000;
            let probe: Arc<dyn Expirable> = Arc::new(Probe(at, sent));
            registry.track(at, Arc::downgrade(&probe));
            received
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("a native owner's deadline must fire without a simulation");
            registry.close();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn token_unwind_before_registration_retires_credit_and_waits_for_death() {
        let registry = Registry::new(ActorId::next(), 1);
        registry.begin_turn();
        let slot = Arc::new(Slot::<u32>::new(NEVER));
        slot.defer(registry.pool(), &|| 0).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _token = crate::reply::ReplyTo::new(slot.clone(), registry.clone());
            panic!("registration failed before listing the token");
        }));
        crate::mailbox::dispose_panic(result.unwrap_err());
        assert!(
            !slot.is_terminal(),
            "an unwinding turn defers its error until D"
        );
        #[cfg(feature = "async")]
        assert_eq!(registry.external_count(), 0);
        registry.end_turn(true);
        registry.exiting();
        registry.seal(&ExitReason::Shutdown);
        registry.close();
        assert_eq!(terminal_of(&slot), Some(Terminal::Stopped));
        assert_eq!(registry.outstanding(), 0);
    }

    #[test]
    fn turn_drops_commit_dropped_or_wait_for_death() {
        let registry = Registry::new(ActorId::next(), 8);
        // A turn that returns answers its drops `Dropped`.
        let slot = deferred(&registry, NEVER);
        registry.begin_turn();
        registry.dropped(slot.clone());
        assert!(
            !slot.is_terminal(),
            "an in-turn drop waits for the turn end"
        );
        registry.end_turn(false);
        assert_eq!(terminal_of(&slot), Some(Terminal::Dropped));
        // Between turns a drop answers at once.
        let slot = deferred(&registry, NEVER);
        registry.dropped(slot.clone());
        assert_eq!(terminal_of(&slot), Some(Terminal::Dropped));
        // A panicking turn's drops, and drops while exiting, are exit
        // pending: open until the death point answers them with the final
        // reason.
        let unwound = deferred(&registry, NEVER);
        registry.begin_turn();
        registry.dropped(unwound.clone());
        registry.end_turn(true);
        registry.exiting();
        let exiting = deferred(&registry, NEVER);
        registry.dropped(exiting.clone());
        assert!(!unwound.is_terminal() && !exiting.is_terminal());
        registry.seal(&ExitReason::Shutdown);
        assert_eq!(registry.close(), 2);
        assert_eq!(terminal_of(&unwound), Some(Terminal::Stopped));
        assert_eq!(terminal_of(&exiting), Some(Terminal::Stopped));
        assert_eq!(registry.final_reason(), Some(&ExitReason::Shutdown));
        // Past the death point a drop answers at once.
        let late = Arc::new(Slot::<u32>::new(NEVER));
        registry.dropped(late.clone());
        assert_eq!(terminal_of(&late), Some(Terminal::Stopped));
        assert_eq!(registry.outstanding(), 0);
    }

    #[test]
    fn unsealed_close_answers_killed() {
        let registry = Registry::new(ActorId::next(), 8);
        let open = deferred(&registry, NEVER);
        assert_eq!(registry.close(), 1);
        assert_eq!(terminal_of(&open), Some(Terminal::Stopped));
        assert_eq!(registry.final_reason(), Some(&ExitReason::Killed));
    }

    #[test]
    fn close_stops_every_listed_open_token_once() {
        let registry = Registry::new(ActorId::next(), 8);
        let open = deferred(&registry, NEVER);
        let answered = deferred(&registry, NEVER);
        answered.reply(1);
        assert_eq!(registry.close(), 1);
        assert_eq!(
            Terminal::of(crate::reply::slot::state(open.word())),
            Some(Terminal::Stopped)
        );
        assert_eq!(registry.outstanding(), 0);
        assert_eq!(registry.heap_entries(), 0, "the heap is dropped at death");
    }

    #[test]
    fn heap_stays_bounded_under_shortening_churn() {
        let registry = Registry::new(ActorId::next(), 64);
        let far = u64::MAX / 2;
        let slots: Vec<_> = (0..32).map(|i| deferred(&registry, far + i)).collect();
        for round in 0..200u64 {
            for slot in &slots {
                // Each shortening supersedes the previous entry (stale).
                slot.shorten(far - round - 1, &|| 0);
                registry.track_deadline(slot);
            }
            assert!(
                registry.heap_entries() <= 2 * slots.len() + 16 + slots.len(),
                "heap grew to {} entries for {} live tokens",
                registry.heap_entries(),
                slots.len()
            );
        }
        // Expire them all: the service visit empties the heap of due entries.
        assert_eq!(registry.service_deadlines(u64::MAX - 2), None);
        assert_eq!(registry.outstanding(), 0);
    }
}
