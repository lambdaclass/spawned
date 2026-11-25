//! `SupervisorBuilder` (ordered startup, rollback) and the running `Supervisor` handle and report.
use super::*;

pub struct SupervisorBuilder {
    pub(super) owner: Weak<system::SystemInner>,
    pub(super) core: Arc<Core>,
    pub(super) runners: Vec<Box<dyn Runner>>,
    pub(super) started: bool,
}
impl SupervisorBuilder {
    pub(crate) fn new(
        system: &ActorSystem,
        config: SupervisorConfig,
    ) -> Result<Self, SupervisorError> {
        Self::from_owner(Arc::downgrade(&system.inner), config)
    }
    pub(super) fn from_owner(
        owner: Weak<system::SystemInner>,
        config: SupervisorConfig,
    ) -> Result<Self, SupervisorError> {
        if config.restart_window.is_zero()
            || config.max_children == 0
            || config.max_children > 65536
            || config.max_restarts > 65536
            || config.backoff.maximum < config.backoff.initial
        {
            return Err(SupervisorError::Configuration(
                "invalid limits or backoff".into(),
            ));
        }
        let (done, _) = watch::channel(false);
        let (ready, _) = watch::channel(false);
        let (recovery_changed, _) = watch::channel(0);
        let name = config
            .name
            .clone()
            .unwrap_or_else(|| "supervisor".to_string());
        let system = owner.upgrade().map(|system| system.id());
        let core = Arc::new(Core {
            name,
            system,
            recovery: Mutex::new(RecoveryQueue {
                pending: Vec::new(),
                covered: Vec::new(),
                next_id: 1,
            }),
            recovery_changed,
            gate: Admission::new(),
            cancel: CancellationToken::new(),
            admission_closed: CancellationToken::new(),
            done,
            ready,
            failure: Mutex::new(None),
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cleanup_failure: Mutex::new(None),
            cleanup_failures: Mutex::new(Vec::new()),
            children: Mutex::new(Vec::new()),
            budget: Mutex::new(RetryBudget {
                attempts: VecDeque::new(),
                reserved: 0,
                max: config.max_restarts,
                window: config.restart_window,
            }),
            config,
            trace: TraceId::next(),
            #[cfg(test)]
            initial_ready_pause: Mutex::new(None),
            #[cfg(test)]
            retry_admission_pause: Mutex::new(None),
            #[cfg(feature = "lifecycle-trace")]
            deadline_recorded: Default::default(),
        });
        #[cfg(feature = "lifecycle-trace")]
        trace::describe(
            core.trace,
            match core.config.strategy {
                SupervisorStrategy::OneForOne => "OneForOne",
                SupervisorStrategy::OneForAll => "OneForAll",
                SupervisorStrategy::RestForOne => "RestForOne",
            },
            core.config.max_restarts,
        );
        Ok(Self {
            owner,
            core,
            runners: Vec::new(),
            started: false,
        })
    }

    /// Lifecycle trace id of this supervisor (`lifecycle_trace::take`).
    #[cfg(feature = "lifecycle-trace")]
    #[doc(hidden)]
    pub fn trace_id(&self) -> u64 {
        self.core.trace.0
    }

    pub fn add_supervisor(
        &mut self,
        spec: SupervisorSpec,
    ) -> Result<SupervisedSupervisor, SupervisorError> {
        nested::add(self, spec)
    }
    pub fn add<A: Actor>(
        &mut self,
        spec: ChildSpec<A>,
    ) -> Result<SupervisedChild<A>, SupervisorError> {
        self.owner
            .upgrade()
            .ok_or(SystemError::Closed)?
            .accepts_group(&spec.group)?;
        if spec.name.is_empty()
            || spec.name.contains('\0')
            || spec.startup_timeout.is_zero()
            || spec.shutdown_timeout.is_zero()
        {
            return Err(SupervisorError::Configuration(
                "invalid child name or timeout".into(),
            ));
        }
        let mut children = system::lock(&self.core.children);
        if children.len() >= self.core.config.max_children {
            return Err(SupervisorError::Capacity);
        }
        if children.iter().any(|c| c.snapshot().name == spec.name) {
            return Err(SupervisorError::DuplicateChild(spec.name));
        }
        let (changed, _) = watch::channel(0);
        let (initial, _) = watch::channel(false);
        let (done, _) = watch::channel(false);
        let kind =
            if std::any::TypeId::of::<A>() == std::any::TypeId::of::<nested::SupervisorActor>() {
                ChildKind::Supervisor
            } else {
                ChildKind::Worker
            };
        let view = Arc::new(View {
            index: children.len(),
            restart: spec.restart,
            kind,
            nested: std::sync::OnceLock::new(),
            data: Mutex::new(ViewData {
                recovery: None,
                snapshot: ChildSnapshot {
                    name: spec.name.clone(),
                    state: ChildState::Waiting,
                    generation: 0,
                    actor_id: None,
                    last_exit: None,
                    last_error: None,
                },
                current: None,
                scope: None,
            }),
            gate: self.core.gate.clone(),
            failed: self.core.failed.clone(),
            changed,
            initial,
            done,
            stop_turn: CancellationToken::new(),
            shutdown_timeout: spec.shutdown_timeout,
            trace_index: children.len() + 1,
        });
        children.push(view.clone());
        #[cfg(feature = "lifecycle-trace")]
        trace::describe_child(
            self.core.trace,
            match spec.restart {
                RestartMode::Permanent => "Permanent",
                RestartMode::Transient => "Transient",
                RestartMode::Temporary => "Temporary",
            },
        );
        {
            let mut queue = system::lock(&self.core.recovery);
            queue.pending.push(false);
            queue.covered.push(0);
        }
        self.runners.push(Box::new(TypedRunner {
            spec,
            view: view.clone(),
        }));
        Ok(SupervisedChild { view })
    }

    /// Start children in declaration order. Dropping this future before readiness
    /// commits requests cleanup. After readiness, ActorSystem owns the tree even
    /// if this future is dropped before returning its Supervisor handle.
    pub async fn start(self) -> Result<Supervisor, SupervisorError> {
        self.start_owned(true).await
    }
    pub(super) async fn start_owned(
        mut self,
        register_root: bool,
    ) -> Result<Supervisor, SupervisorError> {
        let owner = self.owner.upgrade().ok_or(SystemError::Closed)?;
        // Registration and controller dispatch happen without an intervening await.
        if register_root {
            owner.register_unit(self.core.clone())?;
        }
        trace::tree(self.core.trace, |id| {
            TreeEvent::new("TCreate", id).peer(0, self.runners.len())
        });
        self.started = true;
        let runners = std::mem::take(&mut self.runners);
        let core = self.core.clone();
        owner
            .control
            .handle
            .spawn(run_supervisor(core.clone(), runners));
        let mut guard = StartupGuard {
            core: core.clone(),
            armed: true,
        };
        let ready = core.ready.subscribe();
        let cancelled = core.cancel.cancelled();
        let _ = select(Box::pin(cancelled), Box::pin(system::wait_true(ready))).await;
        // Readiness is historical, just like ActorRef::ready(). A later system
        // admission close must not turn successful startup into rollback.
        let initialized = *core.ready.borrow();
        if initialized {
            guard.armed = false;
            return Ok(Supervisor { core: core.clone() });
        }
        // S2: the error is returned only after the rollback finished: every
        // child is terminal, or settled as Failed by its stuck cleanup.
        let children = system::lock(&core.children).clone();
        for child in &children {
            child.settled().await;
        }
        Err(core.start_error())
    }
}
impl Core {
    /// The cause of a failed initial start (S2), with the rollback's cleanup
    /// failures listed separately.
    pub(super) fn start_error(&self) -> SupervisorError {
        let Some(failure) = system::lock(&self.failure).clone() else {
            return SupervisorError::Stopped;
        };
        let rollback_failures: Vec<String> = system::lock(&self.cleanup_failures)
            .iter()
            .filter(|f| **f != failure.message)
            .cloned()
            .collect();
        match (failure.kind, failure.origin) {
            (FailureKind::FirstStart, Some(origin)) => {
                let prefix = format!("failed to start child {}: ", origin.child);
                let cause = failure
                    .message
                    .strip_prefix(&prefix)
                    .unwrap_or(&failure.message)
                    .to_string();
                SupervisorError::FailedToStartChild {
                    child: origin.child,
                    cause,
                    rollback_failures,
                }
            }
            (FailureKind::Cleanup, Some(origin)) => SupervisorError::StartupCleanupStuck {
                child: origin.child,
                cause: failure.message,
                rollback_failures,
            },
            _ => SupervisorError::Failed(failure.message),
        }
    }
}
impl Drop for SupervisorBuilder {
    fn drop(&mut self) {
        if !self.started {
            self.core.close();
            for child in system::lock(&self.core.children).iter() {
                child.finish_unstarted(self.core.failed.load(std::sync::atomic::Ordering::Acquire));
            }
            trace::atomic(|| {
                trace::tree(self.core.trace, |id| TreeEvent::new("TDone", id));
                self.core.done.send_replace(true);
            });
        }
    }
}
pub(super) struct StartupGuard {
    pub(super) core: Arc<Core>,
    pub(super) armed: bool,
}
impl Drop for StartupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.core.cancel_unready_startup();
        }
    }
}

/// Owned by ActorSystem even if public handles are dropped.
#[derive(Clone)]
pub struct Supervisor {
    pub(super) core: Arc<Core>,
}
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SupervisorReport {
    pub complete: bool,
    pub failure: Option<String>,
    pub children: Vec<ChildSnapshot>,
}
impl Supervisor {
    pub fn stop(&self) {
        self.core.close();
    }
    /// Lifecycle trace id of this supervisor (`lifecycle_trace::take`).
    #[cfg(feature = "lifecycle-trace")]
    #[doc(hidden)]
    pub fn trace_id(&self) -> u64 {
        self.core.trace.0
    }
    pub fn snapshots(&self) -> Vec<ChildSnapshot> {
        system::lock(&self.core.children)
            .iter()
            .map(|c| c.snapshot())
            .collect()
    }
    /// The children, in declaration order (OTP `which_children/1`). Each
    /// row is consistent with itself; rows may be sampled at different
    /// instants. Out of band: takes each child's lock briefly, runs no
    /// child code and waits for nothing.
    pub fn which_children(&self) -> Vec<ChildInfo> {
        self.core.children().iter().map(|c| c.capture().0).collect()
    }
    /// OTP `count_children/1`, derived from one `which_children` call.
    pub fn count_children(&self) -> ChildCounts {
        ChildCounts::of(&self.which_children())
    }
    /// This supervisor and its subtree, each child expanded through the
    /// instance captured with its row (see [`SupervisorTree`]).
    pub fn tree(&self) -> SupervisorTree {
        use crate::introspection::{Field, Unavailable};
        let captured: Vec<(ChildInfo, Captured)> =
            self.core.children().iter().map(|c| c.capture()).collect();
        let rows: Vec<ChildInfo> = captured.iter().map(|(row, _)| row.clone()).collect();
        let children = captured
            .into_iter()
            .map(|(info, handle)| {
                let node = match handle {
                    Captured::Worker(Some(child)) => TreeNode::Worker(Field::Value(child.info())),
                    Captured::Worker(None) => {
                        TreeNode::Worker(Field::Unavailable(Unavailable::Dead))
                    }
                    Captured::Supervisor(Ok(nested)) => {
                        TreeNode::Supervisor(Field::Value(nested.tree()))
                    }
                    Captured::Supervisor(Err(starting)) => {
                        TreeNode::Supervisor(Field::Unavailable(if starting {
                            Unavailable::Starting
                        } else {
                            Unavailable::Dead
                        }))
                    }
                };
                TreeChild { info, node }
            })
            .collect();
        SupervisorTree {
            system: self.core.system,
            name: self.core.name.clone(),
            strategy: self.core.config.strategy,
            phase: self.core.phase(),
            budget: self.core.budget(),
            counts: ChildCounts::of(&rows),
            children,
        }
    }
    pub fn failure(&self) -> Option<String> {
        system::lock(&self.core.failure)
            .as_ref()
            .map(|f| f.message.clone())
    }
    pub async fn join(&self) {
        let _ = system::wait_true(self.core.done.subscribe()).await;
    }
    pub async fn shutdown(&self, grace: Duration) -> SupervisorReport {
        self.stop();
        let complete = rt::timeout(grace, system::wait_true(self.core.done.subscribe()))
            .await
            .unwrap_or(false);
        if !complete {
            self.core.stop_remaining();
        }
        SupervisorReport {
            complete,
            failure: self.failure(),
            children: self.snapshots(),
        }
    }
}
