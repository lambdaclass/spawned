//! Actor startup and the main loop: spawn on a backend, `started()`, the dispatch loop, `stopped()` and the death point.
use super::*;

// ---------------------------------------------------------------------------
// Actor startup + main loop
// ---------------------------------------------------------------------------

/// The parent side of a `start_link`/`start_monitor` (type-erased).
pub(super) struct ParentParts {
    pub(super) id: ActorId,
    pub(super) signal: Arc<SignalTarget>,
    pub(super) links: LinkTable,
    pub(super) request: LinkedExitReason,
    pub(super) monitors: MonitorTable,
    pub(super) token: StopToken,
    pub(super) placement: start::Placement,
}

/// What the commit applied (stage 1, design section 3).
struct Committed {
    managed: Option<(Arc<crate::system::Scope>, crate::system::Lease)>,
    name: Option<crate::registry::RegistrationGuard>,
    monitor: Option<(MonitorRef, spawned_rt::tasks::CancellationToken)>,
}

/// Whether an explicit placement equals the parent's (`placement_of`).
fn same_placement(a: &start::Placement, b: &start::Placement) -> bool {
    match (a, b) {
        (start::Placement::Backend(x), start::Placement::Backend(y)) => x == y,
        (start::Placement::Group(x), start::Placement::Group(y)) => x.same_as(y),
        _ => false,
    }
}

impl<A: Actor> ActorRef<A> {
    /// A supervised generation, admitted to `scope` by its supervisor.
    pub(crate) fn spawn_managed(
        actor: A,
        scope: Arc<crate::system::Scope>,
        lease: crate::system::Lease,
    ) -> Result<Self, crate::system::SystemError> {
        if !scope.can_start() {
            return Err(crate::system::SystemError::Closed);
        }
        Self::launch_inner(
            actor,
            start::StartOptions::new(),
            start::Verb::Spawn,
            None,
            Some((scope, lease)),
        )
        .map(|(actor, _)| actor)
        .map_err(|error| match error {
            crate::start::StartError::System(error) => error,
            _ => crate::system::SystemError::Closed,
        })
    }

    /// Stage 1 of every start (design section 3): validate, prepare the
    /// instance privately, commit it atomically (group admission, the
    /// parent's edge, the name), then dispatch it. A refused start was never
    /// observable.
    pub(super) fn launch(
        actor: A,
        options: start::StartOptions<A>,
        verb: start::Verb,
        arm: Option<start::ArmMonitor>,
    ) -> Result<(Self, Option<MonitorRef>), crate::start::StartError> {
        Self::launch_inner(actor, options, verb, arm, None)
    }

    fn launch_inner(
        actor: A,
        options: start::StartOptions<A>,
        verb: start::Verb,
        arm: Option<start::ArmMonitor>,
        reserved: Option<(Arc<crate::system::Scope>, crate::system::Lease)>,
    ) -> Result<(Self, Option<MonitorRef>), crate::start::StartError> {
        use crate::lifecycle_trace as trace;
        use crate::start::{EdgeKind, StartEdge, StartError};
        let id = ActorId::next();
        let c = trace::start_child(id.raw());
        let parent = match &verb {
            start::Verb::Link(p) | start::Verb::Monitor(p) => Some(p),
            _ => None,
        };
        // The placement: explicit, else the parent's for link/monitor
        // (design section 4), else unmanaged Async.
        let explicit = options.placement.clone();
        let placement = match (&explicit, parent) {
            (Some(placement), _) => placement.clone(),
            (None, Some(p)) => p.placement.clone(),
            (None, None) => start::Placement::Backend(Backend::Async),
        };
        trace::start_emit(c, || {
            trace::StartEvent::new("SBegin", c)
                .x(verb.trace_name())
                .y(match (&explicit, parent) {
                    (None, _) => "none",
                    (Some(p), Some(parent)) if same_placement(p, &parent.placement) => "same",
                    (Some(p), _) => p.trace_name(),
                })
                .f(options.name.is_some())
        });
        if let Err(error) = options.validate(&placement) {
            trace::start_emit(c, || trace::StartEvent::new("SPrepareFail", c));
            return Err(error);
        }
        let (backend, group) = match (&placement, &reserved) {
            (_, Some(_)) => (Backend::Async, None),
            (start::Placement::Group(group), None) => (Backend::Async, Some(group.clone())),
            (start::Placement::Backend(backend), None) => (Backend::to_owned(backend), None),
        };

        // Prepare: the instance's state, private until the commit. A managed
        // instance's stop token and exit request are its scope's, created
        // here and handed to the scope the commit creates.
        let (tx, rx) = mpsc::channel::<Box<dyn Envelope<A> + Send>>();
        let cancellation_token = reserved
            .as_ref()
            .map(|(s, _)| s.token.clone())
            .unwrap_or_default();
        let linked_reason = reserved
            .as_ref()
            .map(|(s, _)| s.cause.clone())
            .unwrap_or_else(new_linked_exit_reason);
        let (completion_tx, completion_rx) = watch::channel(None);
        let monitors: MonitorTable = Arc::new(crate::monitor::fence::open());
        let trap_exit = new_trap_exit_flag();
        let links = new_link_table();
        {
            // Phase 0: signals that arrive before started() returns are held.
            let mut request = link::lock_request(&linked_reason);
            request.begin_starting();
            #[cfg(feature = "lifecycle-trace")]
            if c != 0 {
                request.trace = trace::StartRole::Child(c);
            }
        }
        let cell = crate::start::StartupCell::new();
        cell.prepare(
            verb.waiting(),
            parent.map(|p| StartEdge {
                kind: if matches!(verb, start::Verb::Link(_)) {
                    EdgeKind::Link
                } else {
                    EdgeKind::Monitor
                },
                parent: p.signal.clone(),
            }),
            reserved
                .as_ref()
                .map_or(DEFAULT_STOP_DEADLINE, |(s, _)| s.cleanup_timeout()),
            c,
        );
        let link_parent = match &verb {
            start::Verb::Link(p) => Some(p.id),
            _ => None,
        };
        let signal = signal_target::<A>(
            id,
            link_parent,
            &tx,
            trap_exit.clone(),
            linked_reason.clone(),
            cancellation_token.clone(),
            cell.clone(),
        );
        let (cleaned_tx, cleaned_rx) = watch::channel(false);
        // A supervised child's `ChildSpec` may override the actor's bounds;
        // so may the start's options.
        let bounds = reserved
            .as_ref()
            .map(|(scope, _)| scope.bounds())
            .unwrap_or_default()
            .or(options.max_outstanding, options.max_native_jobs);
        let (max_outstanding, max_native) = bounds.resolve(A::MAX_OUTSTANDING, A::MAX_NATIVE_JOBS);
        let managed_cell = std::sync::OnceLock::new();
        if let Some((scope, _)) = &reserved {
            let _ = managed_cell.set(scope.clone());
        }
        let actor_ref = ActorRef {
            shared: Arc::new(ActorShared {
                id,
                replies: Registry::new(id, max_outstanding),
                jobs: Arc::new(crate::reply::jobs::JobSet::new(id, max_native)),
                cleaned: cleaned_rx,
                sender: tx,
                cancellation_token: cancellation_token.clone(),
                completion_rx,
                monitors,
                trap_exit,
                links: links.clone(),
                linked_reason: linked_reason.clone(),
                signal,
                mailbox: crate::instrumentation::Mailbox::default(),
                managed: managed_cell,
                backend,
            }),
        };
        let child_handle = actor_ref.child_handle();

        // Commit (design section 3): the group's admission, the parent's
        // edge, the name, all or nothing, each check and its trace event
        // under the locks. Lock order: the group's admission, the parent's
        // table, the parent's exit request, the registry.
        let register = || -> Result<Option<crate::registry::RegistrationGuard>, StartError> {
            let Some(name) = &options.name else {
                return Ok(None);
            };
            match crate::registry::register_start(
                name.key(),
                id,
                actor_ref.clone(),
                child_handle.clone(),
                c,
                || Ok::<(), StartError>(()),
            )? {
                Ok(guard) => Ok(Some(guard)),
                Err(existing) => Err(StartError::AlreadyStarted {
                    name: name.key().to_owned(),
                    existing,
                }),
            }
        };
        let edge_commit = || -> Result<Committed, StartError> {
            let mut committed = Committed {
                managed: None,
                name: None,
                monitor: None,
            };
            match &verb {
                start::Verb::Link(p) => {
                    let mut inner = Ok(None);
                    let installed = link::install_start_edge(
                        &p.signal,
                        &p.links,
                        &actor_ref.shared.signal,
                        &links,
                        || {
                            let request = link::lock_request(&p.request);
                            if request.reason().is_some() || request.is_dead() {
                                trace::commit_point(Some("parentexited"));
                                return false;
                            }
                            // The registry is the innermost lock.
                            inner = register();
                            if inner.is_ok() {
                                trace::commit_point(None);
                            }
                            inner.is_ok()
                        },
                    );
                    match (installed, inner) {
                        (_, Err(error)) => return Err(error),
                        (false, Ok(_)) => return Err(StartError::ParentExited),
                        (true, Ok(name)) => committed.name = name,
                    }
                }
                start::Verb::Monitor(p) => {
                    let monitor_ref = MonitorRef::next();
                    let active = spawned_rt::tasks::CancellationToken::new();
                    let mut inner = Ok(None);
                    // The parent's monitor table is checked open first (the
                    // fence), so a name is never registered for a start the
                    // closed table then refuses.
                    let registered = crate::monitor::fence::register(
                        &p.monitors,
                        monitor_ref,
                        active.clone(),
                        || {
                            let request = link::lock_request(&p.request);
                            if request.reason().is_some()
                                || request.is_dead()
                                || p.token.is_cancelled()
                            {
                                trace::commit_point(Some("parentexited"));
                                return true;
                            }
                            inner = register();
                            if inner.is_ok() {
                                trace::commit_point(None);
                            }
                            inner.is_err()
                        },
                        || false,
                        || {},
                        || {},
                    );
                    match (registered, inner) {
                        (_, Err(error)) => return Err(error),
                        (crate::monitor::fence::Registration::Active, Ok(name)) => {
                            committed.name = name;
                            committed.monitor = Some((monitor_ref, active));
                        }
                        // The parent is exiting, or its monitor table closed.
                        _ => return Err(StartError::ParentExited),
                    }
                }
                _ => committed.name = register()?,
            }
            Ok(committed)
        };
        let committed = trace::start_section(|| {
            trace::commit_begin(c, placement.trace_name());
            let result = match &group {
                Some(group) => group
                    .reserve_with(cancellation_token.clone(), linked_reason.clone(), || {
                        edge_commit()
                    })
                    .map(|(scope, lease, mut committed)| {
                        committed.managed = Some((scope, lease));
                        committed
                    }),
                None => edge_commit(),
            };
            trace::commit_end(result.as_ref().err().map(StartError::trace_refusal));
            result
        });
        // A refusal applied nothing: the prepared state is dropped (no hook
        // ran, nothing was published or registered).
        let mut committed = committed?;
        // A waiting start_link/start_monitor: the parent's exit by a signal
        // abandons the wait (OTP: the caller dies at once).
        cell.watch_parent();
        let (managed, lease) = match (committed.managed.take(), reserved) {
            (Some((scope, lease)), _) | (None, Some((scope, lease))) => (Some(scope), Some(lease)),
            (None, None) => (None, None),
        };
        if let Some(scope) = &managed {
            // Admitted by the commit: the instance adopts the scope.
            let _ = actor_ref.shared.managed.set(scope.clone());
            cell.set_cleanup(scope.cleanup_timeout());
            scope.attach::<A>(actor_ref.child_handle());
            scope.set_jobs(Arc::downgrade(&actor_ref.shared.jobs));
            #[cfg(feature = "lifecycle-trace")]
            scope.set_start_trace(c);
            if let Some(label) = &options.label {
                scope.set_label(label);
            }
        }
        let ctx = actor_ref.context();
        let monitor_ref = committed.monitor.take().map(|(monitor_ref, active)| {
            if let Some(arm) = arm {
                arm(child_handle.clone(), monitor_ref, active, cell.clone());
            }
            monitor_ref
        });
        // Recorded from here: the start_link link is the model's initial one.
        crate::lifecycle_trace::life_spawn(id.raw(), link_parent.map_or(0, |p| p.raw()));
        if let start::Timeout::After(after) = options.init_timeout {
            crate::start::spawn_init_timer_async(&cell, child_handle.clone(), after);
        }

        let cleanup_token = cancellation_token.clone();
        let cleanup = crate::actor_cleanup::ActorCleanup::new(
            actor_ref.shared.id,
            links,
            linked_reason,
            actor_ref.shared.monitors.clone(),
            completion_tx,
            move || cleanup_token.cancel(),
        )
        .with_mailbox_failures(actor_ref.shared.sender.failures())
        .with_replies(actor_ref.shared.replies.clone())
        .with_jobs(actor_ref.shared.jobs.clone())
        .with_startup(cell, link_parent)
        .with_name(committed.name.take());
        let startup = crate::actor_cleanup::ActorStartup::new(
            (actor, ctx, rx, cancellation_token),
            cleanup,
            discharge_startup::<A>,
        );
        let job_owner = actor_ref.shared.clone();
        let cleaned_tx = cleaned_tx;
        // Destruction-safe: if the runtime drops this future before it
        // completes, `ActorStartup` (never polled) or `ExitOnDrop` (polled)
        // still reach D before destroying anything, so no caller waits for an
        // instance that can never answer.
        let inner_future = async move {
            let ((actor, ctx, rx, cancellation_token), mut cleanup) = startup.into_parts();
            run_actor(actor, ctx, rx, cancellation_token, &mut cleanup).await;
            let reason = cleanup.die();
            if let Some(scope) = &managed {
                scope.exited(reason);
            }
            // Publication (wait_exit, monitors, link notices) after the
            // state drop, before the job join.
            drop(cleanup);
            // Job cleanup (deferred-replies §5 step 4), after death: join the
            // cancelled jobs, bounded by the cleanup deadline; a job still
            // running then does not undo death and is reported unfinished.
            if job_owner.jobs.running() > 0 {
                let timeout = job_owner
                    .managed()
                    .map_or(DEFAULT_STOP_DEADLINE, |scope| scope.cleanup_timeout());
                let remaining = job_owner.replies.cleanup_remaining(timeout);
                let unfinished = job_owner.jobs.join(remaining).await;
                crate::reply::jobs::report_unfinished(job_owner.id, unfinished);
            }
            // An unowned child's owned work is done (`Start.tla` WorkDoneC;
            // a managed one's is its scope's completion).
            if job_owner.managed().is_none() {
                let id = job_owner.id.raw();
                let c = crate::lifecycle_trace::start_child_of(id);
                crate::lifecycle_trace::start_emit(c, || {
                    crate::lifecycle_trace::StartEvent::new("SWorkDone", c)
                });
            }
            drop(job_owner);
            let _ = cleaned_tx.send(true);
        };

        if let Some(lease) = lease {
            let scope = actor_ref
                .shared
                .managed()
                .expect("managed lease without scope");
            // Committed: a dispatch failure (the group dropped mid-shutdown,
            // its worker gone) is not a stage-1 refusal. The child is killed
            // before started() and its start answers `InitExited { Killed }`.
            if let Err(future) = scope.dispatch_actor(crate::system::Tracked {
                future: inner_future,
                lease,
            }) {
                tracing::error!(actor = ?id, "actor dispatch failed after commit; killing the child");
                crate::start::run_killed(&child_handle, || crate::start::run_inline(future));
            }
            return Ok((actor_ref, monitor_ref));
        }

        match backend {
            Backend::Async => {
                #[cfg(debug_assertions)]
                let inner_future = warn_on_block::WarnOnBlocking::new(inner_future);
                let _handle = rt::spawn(inner_future);
            }
            Backend::Blocking => {
                let _handle = rt::spawn_blocking(move || rt::block_on(inner_future));
            }
            Backend::Thread => {
                let mut builder = std::thread::Builder::new();
                if let Some(bytes) = options.stack_size {
                    builder = builder.stack_size(bytes);
                }
                if let Some(label) = &options.label {
                    builder = builder.name(label.to_string());
                }
                crate::start::spawn_thread_or_kill(
                    builder,
                    &child_handle,
                    inner_future,
                    threads::block_on,
                    crate::start::run_inline,
                );
            }
        }

        Ok((actor_ref, monitor_ref))
    }
}

/// A pending user future of the lifecycle: `started()`, a turn (handler,
/// `exit_received`, a job's `done`) or `stopped()`. If the runtime drops the
/// lifecycle while it is pending (G1), its `Drop` reaches the death point
/// first: the turn's own caller, every deferred token and every queued
/// request are answered and woken before this future and its captures (the
/// message, the handler's locals) are destroyed. A destructor panic of the
/// future is a cleanup failure.
pub(super) struct DeathFirst<'a, A: Actor, C: Fn()> {
    pub(super) inner: Option<Pin<Box<dyn Future<Output = ()> + Send + 'a>>>,
    pub(super) rx: &'a mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    pub(super) cleanup: &'a mut crate::actor_cleanup::ActorCleanup<C>,
    pub(super) caller: Option<&'a dyn Caller>,
    /// The run loop already polled `inner` once in this task poll (it was
    /// pending): the next poll returns `Pending` without polling it again.
    pub(super) polled: bool,
}
impl<'a, A: Actor, C: Fn()> DeathFirst<'a, A, C> {
    pub(super) fn new(
        inner: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
        rx: &'a mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
        cleanup: &'a mut crate::actor_cleanup::ActorCleanup<C>,
        caller: Option<&'a dyn Caller>,
    ) -> Self {
        Self {
            inner: Some(inner),
            rx,
            cleanup,
            caller,
            polled: false,
        }
    }

    /// A turn the run loop polled once and found pending.
    pub(super) fn pending(
        inner: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
        rx: &'a mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
        cleanup: &'a mut crate::actor_cleanup::ActorCleanup<C>,
        caller: Option<&'a dyn Caller>,
    ) -> Self {
        let mut turn = Self::new(inner, rx, cleanup, caller);
        turn.polled = true;
        turn
    }
}

/// A boxed turn of the lifecycle.
pub(super) type TurnFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Poll `turn` once, catching its panic.
#[inline]
pub(super) fn poll_caught(
    turn: &mut TurnFuture<'_>,
    cx: &mut std::task::Context<'_>,
) -> Result<std::task::Poll<()>, Panic> {
    std::panic::catch_unwind(AssertUnwindSafe(|| turn.as_mut().poll(cx)))
}

/// Drop a turn that panicked; a panic of its destructor is disposed of.
pub(super) fn drop_panicked(turn: Option<TurnFuture<'_>>) {
    if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(turn))) {
        crate::mailbox::dispose_panic(secondary);
    }
}
/// A caught panic payload.
pub(super) type Panic = Box<dyn std::any::Any + Send>;

impl<A: Actor, C: Fn()> Future for DeathFirst<'_, A, C> {
    /// The future's panic, caught: the turn's (or hook's) panic, not a
    /// runtime drop. The future is gone before it is returned.
    type Output = Result<(), Panic>;
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        if std::mem::take(&mut this.polled) {
            // Polled (and its waker registered) in this same task poll.
            return std::task::Poll::Pending;
        }
        let inner = this.inner.as_mut().expect("polled after completion");
        match poll_caught(inner, cx) {
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Ok(std::task::Poll::Ready(())) => {
                this.inner = None;
                std::task::Poll::Ready(Ok(()))
            }
            Err(panic) => {
                drop_panicked(this.inner.take());
                std::task::Poll::Ready(Err(panic))
            }
        }
    }
}
impl<A: Actor, C: Fn()> Drop for DeathFirst<'_, A, C> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        // Destroyed while pending: the runtime dropped the actor.
        self.cleanup.die_dropped();
        if let Some(caller) = self.caller.take() {
            self.cleanup.answer_turn(caller.retain());
        }
        let queued = answer_mailbox(self.rx, self.cleanup.replies());
        self.cleanup.discharged();
        self.rx.discard_all(queued);
        if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(inner))) {
            let message = panic_message(&*panic);
            crate::mailbox::dispose_panic(panic);
            self.cleanup
                .cleanup_failed(format!("dropped turn destructor: {message}"));
        }
    }
}

/// The actor's state and mailbox while its lifecycle runs. They are
/// destroyed only after the death point D: normally at the end of
/// [`run_actor`]; if the lifecycle future is dropped mid-way (its runtime was
/// destroyed), here, which reaches D first (`Killed` unless a reason was
/// requested), so tokens and queued requests answer `ActorStopped` before
/// any destructor runs.
pub(super) struct ExitOnDrop<'c, A: Actor, C: Fn()> {
    pub(super) actor: Option<A>,
    pub(super) rx: mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    pub(super) cleanup: &'c mut crate::actor_cleanup::ActorCleanup<C>,
}
impl<A: Actor, C: Fn()> ExitOnDrop<'_, A, C> {
    /// D, then the queued requests, then destruction (mailbox, then state).
    pub(super) fn finish(&mut self, dropped: bool) {
        let Some(actor) = self.actor.take() else {
            return;
        };
        if dropped {
            self.cleanup.die_dropped();
        } else {
            self.cleanup.die();
        }
        let queued = answer_mailbox(&mut self.rx, self.cleanup.replies());
        // Every caller is answered: wake them, then destroy (queued
        // payloads, then state). A destructor panic is a cleanup failure,
        // never the reason.
        self.cleanup.discharged();
        self.rx.discard_all(queued);
        if let Some(failure) = crate::actor_cleanup::finish_actor_state(actor) {
            self.cleanup.cleanup_failed(failure);
        }
    }
}
impl<A: Actor, C: Fn()> Drop for ExitOnDrop<'_, A, C> {
    fn drop(&mut self) {
        self.finish(true);
    }
}

pub(super) async fn run_actor<A: Actor, C: Fn()>(
    actor: A,
    ctx: Context<A>,
    rx: mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cancellation_token: StopToken,
    cleanup: &mut crate::actor_cleanup::ActorCleanup<C>,
) {
    let request = ctx.shared.linked_reason.clone();
    let mut owned = ExitOnDrop {
        actor: Some(actor),
        rx,
        cleanup,
    };
    if let Err(payload) = AssertUnwindSafe(run_actor_lifecycle(
        owned.actor.as_mut().expect("the state lives until death"),
        ctx,
        &mut owned.rx,
        &mut *owned.cleanup,
        cancellation_token,
    ))
    .catch_unwind()
    .await
    {
        let message = panic_message(&*payload);
        crate::mailbox::dispose_panic(payload);
        link::lock_request(&request).panic(ExitReason::Panic(format!(
            "actor lifecycle unwound: {message}"
        )));
    }
    // The death point D, only now that `stopped()` returned (or was
    // skipped): the reason becomes final, the link table closes, job
    // admission and the registry close (every open token is answered), then
    // the mailbox refuses admission and its queued requests are answered,
    // all before any destructor. Early returns (startup panic, closed scope)
    // and an unwound lifecycle also come here, so jobs never outlive their
    // owner.
    owned.finish(false);
}

pub(super) async fn run_actor_lifecycle<A: Actor, C: Fn()>(
    actor: &mut A,
    ctx: Context<A>,
    rx: &mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cleanup: &mut crate::actor_cleanup::ActorCleanup<C>,
    cancellation_token: StopToken,
) {
    let _monitor_cleanup = MonitorCleanup(ctx.shared.monitors.clone());
    let request = ctx.shared.linked_reason.clone();
    let trace_id = ctx.shared.id.raw();
    let cell = ctx.shared.signal.startup.clone();
    // A kill or the init timer before started() began (`KilledEarly`), or
    // a managed admission already closed: started() is not run.
    let (killed, closed) = crate::lifecycle_trace::start_section(|| {
        let killed = link::lock_request(&request).killed_early()
            || cell.commit() == crate::start::Commit::TimedOut;
        let closed = ctx
            .shared
            .managed()
            .is_some_and(|scope| !scope.admission_open());
        if !killed {
            // A closed admission skips started() but resolves as if it
            // returned ready (row 9, or row 3 for a scope's stop): the
            // model's StartedBegin and StartedEnd.
            let c = cell.trace_id();
            crate::lifecycle_trace::start_emit(c, || {
                crate::lifecycle_trace::StartEvent::new("SStartedBegin", c)
            });
        }
        (killed, closed)
    });
    let (returned, continuation) = if killed {
        (crate::start::Returned::NotBegun, None)
    } else if closed {
        (crate::start::Returned::Ready, None)
    } else {
        let mut init: Option<start::Init<A>> = None;
        let start_result = AssertUnwindSafe(async {
            fail_point!("actor.before_started", async);
            let started = actor.started(&ctx);
            let slot = &mut init;
            DeathFirst::new(
                crate::lifecycle_trace::in_actor(
                    ctx.shared.id.raw(),
                    Box::pin(async move {
                        *slot = Some(started.await.into());
                    }),
                ),
                &mut *rx,
                &mut *cleanup,
                None,
            )
            .await
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
        .catch_unwind()
        .await;
        match start_result {
            Err(panic) => {
                let msg = panic_message(&*panic);
                crate::actor_cleanup::log_safely(|| {
                    tracing::error!("Panic in started() callback: {msg}");
                });
                (crate::start::Returned::Panic(msg), None)
            }
            Ok(()) => init.map_or((crate::start::Returned::Ready, None), |init| {
                init.into_returned()
            }),
        }
    };
    fail_point!("actor.after_started_before_ready", async);
    // The startup decision (design 2.3) and, when ready, the readiness
    // commit (5.1): under the child's admission gate, its exit-request lock
    // and the cell lock, one step. No failed start runs stopped().
    let trapping = ctx.shared.trap_exit.load(Ordering::Acquire);
    let continued = continuation.is_some();
    let decide = |open: bool| {
        let mut req = link::lock_request(&request);
        cell.decide(&returned, continued, trapping, |timed_out| {
            let row = req.resolve_start(trapping, timed_out, &returned, open);
            let cause = match (&row, &returned) {
                (crate::start::Row::Error, crate::start::Returned::Error(cause)) => {
                    Some(cause.clone())
                }
                _ => None,
            };
            (row, req.reason().cloned(), cause)
        })
    };
    let row = crate::lifecycle_trace::start_section(|| {
        crate::lifecycle_trace::life_section(trace_id, || {
            let row = match ctx.shared.managed() {
                Some(scope) => scope.ready_commit(|open| {
                    let row = decide(open);
                    let ready = row == crate::start::Row::Ready;
                    (row, ready)
                }),
                None => decide(true),
            };
            if row == crate::start::Row::Panic {
                crate::lifecycle_trace::life_emit(trace_id, || {
                    crate::lifecycle_trace::LifeEvent::new("Panic", trace_id).x("started")
                });
            }
            row
        })
    });
    if row != crate::start::Row::Ready {
        if let (crate::start::Returned::Error(cause), false) =
            (&returned, row == crate::start::Row::Error)
        {
            let id = ctx.shared.id;
            crate::actor_cleanup::log_safely(|| {
                tracing::debug!(actor = %id, cause, ?row, "init error superseded by a higher-precedence exit");
            });
        }
        cancellation_token.cancel();
        return;
    }
    // A trapping child's held signals are queued; a non-trapping child's
    // held Normal ones were dropped. A continuation runs first (L10); a stop
    // observed before it begins discards it, as queued messages.
    let continuation = match continuation {
        Some(continuation) if cancellation_token.is_cancelled() => {
            rx.hold_for_shutdown(continuation);
            None
        }
        Some(continuation) => {
            let c = cell.trace_id();
            crate::lifecycle_trace::start_emit(c, || {
                crate::lifecycle_trace::StartEvent::new("SContinue", c)
            });
            Some(continuation)
        }
        None => None,
    };

    // One cancellation future for the whole loop: polled only when the
    // mailbox has nothing, so a busy actor registers no waiter per message.
    let mut cancelled = pin!(cancellation_token.cancelled());
    // Check stop after receiving and before dispatch: a message published
    // after stop must not slip through an earlier snapshot. A rejected receive
    // stays in the mailbox wrapper until the death point answers its caller.
    // The fast check is one atomic load; only parked loops register a waiter.
    // One turn (a mailbox entry or the continuation): true when the actor
    // must stop (its turn panicked). The loop's body, also run once for
    // the continuation before the loop, so the loop itself is unchanged.
    macro_rules! turn {
        ($envelope:expr) => {{
            let envelope = $envelope;

            // Simulator turn budget (rank 8b); compiled only under the cfg.
            #[cfg(spawned_sim)]
            let _sim_turn = rt::sim::turn(std::any::type_name::<A>(), envelope.message_type());
            #[cfg(feature = "diagnostics")]
            let _turn = ctx
                .shared
                .managed()
                .map(|scope| scope.begin_turn(envelope.message_type()));
            #[cfg(feature = "metrics")]
            let mut envelope = envelope;

            #[cfg(feature = "metrics")]
            let observation = envelope
                .observation()
                .map(|observation| observation.begin());
            ctx.shared.replies.begin_turn();
            // The handler's future is built inside the guarded future: a
            // manual `Handler` may panic before it returns one, and that
            // is a handler panic like any other (threads mode catches the
            // same call). The envelope outlives the turn and lends it the
            // request's slot.
            let mut envelope = envelope;
            let result = match dispatch(&mut *envelope, &mut *actor, &ctx) {
                // Dispatch completed the turn synchronously. The caller
                // hold is only needed if an unfinished future is dropped;
                // turn accounting and observation bracket either branch.
                Ok((None, _)) => Ok(()),
                Ok((Some(turn), caller)) => {
                    let mut turn =
                        Some(crate::lifecycle_trace::in_actor(ctx.shared.id.raw(), turn));
                    // Most turns finish in their first poll, which runs
                    // here, with no await point before it returns: only
                    // a turn that stays pending is guarded by
                    // `DeathFirst` against the runtime dropping it.
                    let first = future::poll_fn(|cx| {
                        std::task::Poll::Ready(poll_caught(turn.as_mut().expect("polled once"), cx))
                    })
                    .await;
                    match first {
                        Ok(std::task::Poll::Ready(())) => {
                            drop(turn);
                            Ok(())
                        }
                        Ok(std::task::Poll::Pending) => {
                            let turn = turn.take().expect("pending turn");
                            DeathFirst::pending(turn, &mut *rx, &mut *cleanup, caller).await
                        }
                        Err(panic) => {
                            drop_panicked(turn.take());
                            Err(panic)
                        }
                    }
                }
                Err(panic) => Err(panic),
            };
            ctx.shared.replies.end_turn(result.is_err());
            #[cfg(feature = "diagnostics")]
            drop(_turn);
            #[cfg(feature = "metrics")]
            if let Some(observation) = observation {
                observation.finish(result.is_err());
            }
            if let Err(panic) = result {
                let msg = panic_message(&*panic);
                crate::actor_cleanup::log_safely(|| {
                    tracing::error!("Panic in message handler: {msg}");
                });
                // A panic begins exit, or replaces the requested reason
                // unless a kill was received.
                crate::lifecycle_trace::life_section(trace_id, || {
                    crate::lifecycle_trace::life_emit(trace_id, || {
                        crate::lifecycle_trace::LifeEvent::new("Panic", trace_id).x("turn")
                    });
                    link::lock_request(&request)
                        .panic(ExitReason::Panic(format!("panic in handler: {msg}")));
                });
                true
            } else {
                false
            }
        }};
    }
    {
        // The continuation is the first turn, before any mailbox entry; one
        // copy of the turn body keeps the message loop as compact as 0.8's.
        let mut continuation = continuation;
        loop {
            let msg = if let Some(envelope) = continuation.take() {
                Some(envelope)
            } else {
                let recv = pin!(future::poll_fn(|cx| {
                    // Stop and closed-mailbox both take the normal lifecycle exit,
                    // just like the cancellation arm below. Check after the
                    // mailbox acquire and retain a rejected item for shutdown.
                    // The receive, check and trace event are one step
                    // of the lifecycle trace: a recorded stop is seen here.
                    crate::lifecycle_trace::life_section(trace_id, || {
                        #[cfg(all(test, not(feature = "lifecycle-trace"), not(spawned_sim)))]
                        stop_receive_probe::before_receive();
                        let received = rx.poll_recv(cx);
                        let result = cancellation_token.after_receive(received, |item| {
                            rx.hold_for_shutdown(item);
                        });
                        #[cfg(feature = "lifecycle-trace")]
                        if let std::task::Poll::Ready(Some(envelope)) = &result {
                            crate::lifecycle_trace::life_emit(trace_id, || {
                                crate::lifecycle_trace::LifeEvent::new("Dispatch", trace_id)
                                    .k(envelope.trace_kind())
                            });
                            crate::lifecycle_trace::start_dispatch(trace_id, envelope.trace_kind());
                        }
                        result
                    })
                }));
                let mut recv = recv;
                future::poll_fn(|cx| match recv.as_mut().poll(cx) {
                    std::task::Poll::Pending => match cancelled.as_mut().poll(cx) {
                        std::task::Poll::Ready(()) => std::task::Poll::Ready(None),
                        std::task::Poll::Pending => std::task::Poll::Pending,
                    },
                    ready => ready,
                })
                .await
            };
            match msg {
                Some(envelope) => {
                    if turn!(envelope) {
                        break;
                    }
                }
                None => break,
            }
        }
    }

    if let Some(scope) = ctx.shared.managed() {
        scope.end_running();
    }
    cancellation_token.cancel();
    ctx.shared.replies.exiting();
    // Payload drops are held until every caller is answered (DropFailures).
    ctx.shared.sender.failures().hold();
    ctx.shared.jobs.cancel_all();
    // stopped() is skipped when a kill, an untrapped exit signal or link
    // notice began the exit (decision 2); one that already began finishes.
    let (run_hook, skipped) = crate::lifecycle_trace::life_section(trace_id, || {
        let mut req = link::lock_request(&request);
        let run = req.begin_hook();
        let skipped = (!run).then(|| (req.skip_cause(), req.reason().cloned()));
        drop(req);
        crate::lifecycle_trace::life_emit(trace_id, || {
            crate::lifecycle_trace::LifeEvent::new("Hook", trace_id).x(if run {
                "run"
            } else {
                "skip"
            })
        });
        (run, skipped)
    });
    if let Some((cause, reason)) = skipped {
        crate::actor_cleanup::log_stopped_skipped(
            ctx.shared.id,
            std::any::type_name::<A>(),
            cause,
            reason.as_ref(),
        );
    }
    if !run_hook {
        return;
    }
    let stop_result = AssertUnwindSafe(async {
        fail_point!("actor.before_stopped", async);
        DeathFirst::new(
            crate::lifecycle_trace::in_actor(ctx.shared.id.raw(), Box::pin(actor.stopped(&ctx))),
            &mut *rx,
            &mut *cleanup,
            None,
        )
        .await
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
    .catch_unwind()
    .await;
    if let Err(panic) = stop_result {
        let msg = panic_message(&*panic);
        crate::actor_cleanup::log_safely(|| {
            tracing::error!("Panic in stopped() callback: {msg}");
        });
        crate::lifecycle_trace::life_section(trace_id, || {
            crate::lifecycle_trace::life_emit(trace_id, || {
                crate::lifecycle_trace::LifeEvent::new("Panic", trace_id).x("stopped")
            });
            link::lock_request(&request)
                .panic(ExitReason::Panic(format!("panic in stopped(): {msg}")));
        });
    }
}
