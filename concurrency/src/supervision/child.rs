//! Per-child state: the recovery queue, the typed child view behind `SupervisedChild`, and the type-erased `ChildControl`.
use super::*;

pub(super) struct RecoveryMeta {
    pub(super) id: u64,
    pub(super) origin: usize,
    pub(super) finished: std::sync::atomic::AtomicBool,
}
pub(super) struct RecoverySlot {
    pub(super) meta: Arc<RecoveryMeta>,
    pub(super) stop: CancellationToken,
    pub(super) resume: CancellationToken,
    pub(super) cleaned: watch::Sender<bool>,
    pub(super) ready: watch::Sender<bool>,
}
impl RecoverySlot {
    pub(super) fn new(meta: Arc<RecoveryMeta>) -> Arc<Self> {
        let (cleaned, _) = watch::channel(false);
        let (ready, _) = watch::channel(false);
        Arc::new(Self {
            meta,
            stop: CancellationToken::new(),
            resume: CancellationToken::new(),
            cleaned,
            ready,
        })
    }
}
pub(crate) struct RecoveryQueue {
    pub(crate) pending: Vec<bool>,
    pub(crate) covered: Vec<u64>,
    pub(crate) next_id: u64,
}
impl RecoveryQueue {
    /// Opens the next round at the lowest pending origin: clears pending from
    /// `start` on, stamps those children as covered by the fresh round id and
    /// returns `(origin, start, id)`.
    pub(crate) fn select(&mut self, one_for_all: bool) -> Option<(usize, usize, u64)> {
        let origin = self.pending.iter().position(|pending| *pending)?;
        let start = if one_for_all { 0 } else { origin };
        for pending in &mut self.pending[start..] {
            *pending = false;
        }
        let id = self.next_id;
        self.next_id = id.checked_add(1).expect("recovery IDs exhausted");
        self.covered[start..].fill(id);
        Some((origin, start, id))
    }
    pub(crate) fn request(&mut self, index: usize, after: u64) -> bool {
        if after < self.covered[index] {
            return false;
        }
        self.pending[index] = true;
        true
    }
}
pub(super) struct ViewData<A: Actor> {
    pub(super) recovery: Option<Arc<RecoverySlot>>,
    pub(super) snapshot: ChildSnapshot,
    pub(super) current: Option<ActorRef<A>>,
    pub(super) scope: Option<Arc<Scope>>,
}
pub(super) struct View<A: Actor> {
    pub(super) index: usize,
    pub(super) data: Mutex<ViewData<A>>,
    pub(super) gate: Arc<Admission>,
    pub(super) failed: Arc<std::sync::atomic::AtomicBool>,
    pub(super) changed: watch::Sender<u64>,
    pub(super) initial: watch::Sender<bool>,
    pub(super) stop_turn: CancellationToken,
    pub(super) done: watch::Sender<bool>,
    pub(super) shutdown_timeout: Duration,
    /// 1-based declaration index: the model's child id in lifecycle traces.
    pub(super) trace_index: usize,
}
impl<A: Actor> View<A> {
    pub(super) fn unavailable(&self) -> ChildUnavailable {
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            ChildUnavailable(ChildState::Failed)
        } else {
            let state = system::lock(&self.data).snapshot.state;
            ChildUnavailable(
                if matches!(state, ChildState::Stopped | ChildState::Failed) {
                    state
                } else {
                    ChildState::Stopping
                },
            )
        }
    }
    pub(super) fn change(&self, f: impl FnOnce(&mut ViewData<A>)) {
        {
            f(&mut system::lock(&self.data));
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
    pub(super) fn recovery(&self, after: u64) -> Option<Arc<RecoverySlot>> {
        system::lock(&self.data)
            .recovery
            .clone()
            .filter(|r| r.meta.id > after)
    }
    pub(super) async fn next_recovery(&self, after: u64) -> Arc<RecoverySlot> {
        let mut changes = self.changed.subscribe();
        loop {
            changes.borrow_and_update();
            if let Some(round) = self.recovery(after) {
                return round;
            }
            let _ = changes.changed().await;
        }
    }
    pub(super) async fn recovery_stopping(&self, after: u64) {
        self.next_recovery(after).await.stop.cancelled().await;
    }
    pub(super) fn finish_unless_recovering(&self, after: u64, failed: bool) -> bool {
        {
            let mut data = system::lock(&self.data);
            if !failed && data.recovery.as_ref().is_some_and(|r| r.meta.id > after) {
                return false;
            }
            data.current = None;
            data.scope = None;
            data.snapshot.state = if failed || data.snapshot.state == ChildState::Failed {
                ChildState::Failed
            } else {
                ChildState::Stopped
            };
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        self.done.send_replace(true);
        true
    }
    /// Cleanup stuck past its deadlines: the snapshot settles as Failed,
    /// which releases a waiting `start()` (S2), while the controller keeps
    /// awaiting the real join.
    pub(super) fn mark_stuck(&self) {
        self.change(|data| {
            data.current = None;
            data.snapshot.state = ChildState::Failed;
        });
    }
    /// Terminal, or settled as Failed by a stuck cleanup.
    pub(super) async fn settled(&self) {
        let mut changes = self.changed.subscribe();
        loop {
            changes.borrow_and_update();
            let state = system::lock(&self.data).snapshot.state;
            if matches!(state, ChildState::Stopped | ChildState::Failed) || *self.done.borrow() {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }
    pub(super) fn finish(&self, failed: bool) {
        self.change(|data| {
            data.current = None;
            data.scope = None;
            data.snapshot.state = if failed || data.snapshot.state == ChildState::Failed {
                ChildState::Failed
            } else {
                ChildState::Stopped
            };
        });
        self.done.send_replace(true);
    }
}

/// Explicit lookup of a replacement, not a rebound ActorRef. Resolution can
/// race with a subsequent crash; callers must still handle delivery errors.
pub struct SupervisedChild<A: Actor> {
    pub(super) view: Arc<View<A>>,
}
impl<A: Actor> Clone for SupervisedChild<A> {
    fn clone(&self) -> Self {
        Self {
            view: self.view.clone(),
        }
    }
}
impl<A: Actor> SupervisedChild<A> {
    pub fn snapshot(&self) -> ChildSnapshot {
        system::lock(&self.view.data).snapshot.clone()
    }
    pub fn current(&self) -> Result<ActorRef<A>, ChildUnavailable> {
        self.view
            .gate
            .if_open(|| {
                let data = system::lock(&self.view.data);
                if data.scope.as_ref().is_some_and(|s| s.token.is_cancelled()) {
                    return Err(ChildUnavailable(ChildState::Stopping));
                }
                data.current
                    .clone()
                    .ok_or(ChildUnavailable(data.snapshot.state))
            })
            .unwrap_or_else(|| Err(self.view.unavailable()))
    }
    pub async fn wait_ready(&self) -> Result<ActorRef<A>, ChildUnavailable> {
        self.wait_for_generation(0).await
    }
    /// Wait for a ready generation strictly greater than the supplied value.
    pub async fn wait_for_generation(&self, after: u64) -> Result<ActorRef<A>, ChildUnavailable> {
        let mut changes = self.view.changed.subscribe();
        loop {
            changes.borrow_and_update();
            let snapshot = self.snapshot();
            if snapshot.generation > after {
                if let Ok(actor) = self.current() {
                    return Ok(actor);
                }
            }
            if matches!(snapshot.state, ChildState::Stopped | ChildState::Failed) {
                return Err(ChildUnavailable(snapshot.state));
            }
            if !self.view.gate.is_open() {
                return Err(self.view.unavailable());
            }
            if changes.changed().await.is_err() {
                return Err(ChildUnavailable(ChildState::Stopped));
            }
        }
    }
}

pub(super) trait ChildControl: Send + Sync {
    fn assign_recovery(&self, slot: Arc<RecoverySlot>) -> bool;
    fn stop_generation(&self, slot: &RecoverySlot);
    fn snapshot(&self) -> ChildSnapshot;
    fn close_admission(&self);
    fn stop(&self);
    fn done(&self) -> watch::Receiver<bool>;
    fn initial(&self) -> watch::Receiver<bool>;
    fn shutdown_timeout(&self) -> Duration;
    fn finish_unstarted(&self, failed: bool);
    fn settled(&self) -> BoxFuture<'_, ()>;
}
impl<A: Actor> ChildControl for View<A> {
    fn assign_recovery(&self, slot: Arc<RecoverySlot>) -> bool {
        let old = {
            let mut data = system::lock(&self.data);
            if matches!(
                data.snapshot.state,
                ChildState::Stopped | ChildState::Failed
            ) {
                return false;
            }
            let old = data.recovery.replace(slot);
            data.current = None;
            data.snapshot.state = ChildState::Stopping;
            if let Some(scope) = &data.scope {
                scope.close_admission();
            }
            old
        };
        drop(old);
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        true
    }
    /// Collateral stop (OneForAll/RestForOne): the supervisor's parent exit
    /// signal (E3); the controller escalates to a kill at `shutdown_timeout`.
    fn stop_generation(&self, slot: &RecoverySlot) {
        slot.stop.cancel();
        let scope = system::lock(&self.data).scope.clone();
        if let Some(scope) = scope {
            scope.signal_parent(ExitReason::Shutdown);
        }
    }
    fn snapshot(&self) -> ChildSnapshot {
        system::lock(&self.data).snapshot.clone()
    }
    fn close_admission(&self) {
        let (scope, constructing) = {
            let data = system::lock(&self.data);
            (data.scope.clone(), data.current.is_none())
        };
        if let Some(scope) = scope {
            scope.close_admission();
            if constructing {
                scope.stop(ExitReason::Shutdown);
            }
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
    /// Supervisor shutdown of this child (V9, reverse order): the parent
    /// exit signal `exit(Child, shutdown)` (E3). A non-trapping child ends
    /// `Shutdown` without `stopped()`; a trapping child finishes its earlier
    /// messages and runs `stopped()`. The controller kills it at
    /// `shutdown_timeout`.
    fn stop(&self) {
        self.stop_turn.cancel();
        if let Some(scope) = system::lock(&self.data).scope.clone() {
            scope.signal_parent(ExitReason::Shutdown);
        }
    }
    fn done(&self) -> watch::Receiver<bool> {
        self.done.subscribe()
    }
    fn initial(&self) -> watch::Receiver<bool> {
        self.initial.subscribe()
    }
    fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }
    fn finish_unstarted(&self, failed: bool) {
        if self.snapshot().generation == 0 {
            self.finish(failed);
        }
    }
    fn settled(&self) -> BoxFuture<'_, ()> {
        Box::pin(View::settled(self))
    }
}
