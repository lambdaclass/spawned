//! A supervisor child uses an internal actor as its placement/lifecycle adapter.
//! An uncancelled tracked lease waits for the entire nested core to join, even
//! when the adapter panics or its parent requests stop during startup.
use super::*;
use crate::tasks::Context;

type Configure = Arc<dyn Fn(&mut SupervisorBuilder) -> Result<(), String> + Send + Sync>;
type Ready = Arc<Mutex<Option<(ActorId, Supervisor)>>>;

pub struct SupervisorSpec {
    name: String,
    group: ExecutionGroup,
    config: SupervisorConfig,
    configure: Configure,
    restart: RestartMode,
    startup_timeout: Duration,
    shutdown_timeout: Duration,
}
impl SupervisorSpec {
    /// Declare fresh child specs on each subtree generation. The declaration
    /// callback runs on `group`; asynchronous setup belongs in child factories.
    pub fn new<F, E>(
        name: impl Into<String>,
        group: &ExecutionGroup,
        config: SupervisorConfig,
        configure: F,
    ) -> Self
    where
        F: Fn(&mut SupervisorBuilder) -> Result<(), E> + Send + Sync + 'static,
        E: std::fmt::Display,
    {
        Self {
            name: name.into(),
            group: group.clone(),
            config,
            configure: Arc::new(move |builder| configure(builder).map_err(|e| e.to_string())),
            restart: RestartMode::Permanent,
            startup_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
        }
    }
    pub fn restart(mut self, restart: RestartMode) -> Self {
        self.restart = restart;
        self
    }
    pub fn startup_timeout(mut self, limit: Duration) -> Self {
        self.startup_timeout = limit;
        self
    }
    pub fn shutdown_timeout(mut self, limit: Duration) -> Self {
        self.shutdown_timeout = limit;
        self
    }
}

#[derive(Clone)]
pub struct SupervisedSupervisor {
    child: SupervisedChild<SupervisorActor>,
    ready: Ready,
}
impl SupervisedSupervisor {
    pub fn snapshot(&self) -> ChildSnapshot {
        self.child.snapshot()
    }
    /// Resolve this ready subtree instance. Old Supervisor handles never rebind.
    pub fn current(&self) -> Result<Supervisor, ChildUnavailable> {
        let actor = self.child.current()?;
        resolve(&self.ready, actor.id())
    }
    pub async fn wait_ready(&self) -> Result<Supervisor, ChildUnavailable> {
        self.wait_for_generation(0).await
    }
    pub async fn wait_for_generation(&self, after: u64) -> Result<Supervisor, ChildUnavailable> {
        let mut changed = self.child.view.changed.subscribe();
        loop {
            changed.borrow_and_update();
            let actor = self.child.wait_for_generation(after).await?;
            if let Ok(supervisor) = resolve(&self.ready, actor.id()) {
                return Ok(supervisor);
            }
            // The inner gate can close before the adapter's state changes.
            // Await that transition instead of exposing a transient lookup error.
            if changed.changed().await.is_err() {
                return Err(ChildUnavailable(ChildState::Stopped));
            }
        }
    }
}
fn resolve(ready: &Ready, id: ActorId) -> Result<Supervisor, ChildUnavailable> {
    system::lock(ready)
        .as_ref()
        .filter(|(current, supervisor)| *current == id && supervisor.core.gate.is_open())
        .map(|(_, supervisor)| supervisor.clone())
        .ok_or(ChildUnavailable(ChildState::Stopping))
}

pub(super) fn add(
    parent: &mut SupervisorBuilder,
    spec: SupervisorSpec,
) -> Result<SupervisedSupervisor, SupervisorError> {
    let ready: Ready = Arc::new(Mutex::new(None));
    let publish = ready.clone();
    let owner = parent.owner.clone();
    let parent_core = Arc::downgrade(&parent.core);
    let config = spec.config;
    let configure = spec.configure;
    let shutdown_grace = spec.shutdown_timeout;
    let parent_trace = parent.core.trace;
    let parent_slot = parent.runners.len() + 1;
    let child = parent.add(
        ChildSpec::new(spec.name, &spec.group, move |ctx| {
            let owner = owner.clone();
            let parent_core = parent_core.clone();
            let config = config.clone();
            let configure = configure.clone();
            let ready = publish.clone();
            async move {
                let mut builder =
                    SupervisorBuilder::from_owner(owner, config).map_err(|e| e.to_string())?;
                configure(&mut builder)?;
                trace::tree(parent_trace, |id| {
                    TreeEvent::new("TAttach", id)
                        .child(parent_slot, ctx.generation())
                        .peer(
                            {
                                #[cfg(feature = "lifecycle-trace")]
                                {
                                    builder.core.trace.0
                                }
                                #[cfg(not(feature = "lifecycle-trace"))]
                                {
                                    0
                                }
                            },
                            builder.runners.len(),
                        )
                });
                Ok::<_, String>(SupervisorActor {
                    builder: Some(builder),
                    running: None,
                    ready,
                    id: None,
                    shutdown_grace,
                    parent_core,
                })
            }
        })
        .restart(spec.restart)
        .startup_timeout(spec.startup_timeout)
        .shutdown_timeout(spec.shutdown_timeout),
    )?;
    Ok(SupervisedSupervisor { child, ready })
}

struct SupervisorActor {
    builder: Option<SupervisorBuilder>,
    running: Option<Supervisor>,
    ready: Ready,
    id: Option<ActorId>,
    shutdown_grace: Duration,
    parent_core: std::sync::Weak<Core>,
}
impl Actor for SupervisorActor {
    async fn started(&mut self, ctx: &Context<Self>) {
        let builder = self.builder.take().expect("nested actor starts once");
        let core = builder.core.clone();
        let observe = core.clone();
        let actor = ctx.actor_ref();
        let parent = ctx.cancellation_token();
        let stop_parent = parent.clone();
        let grace = self.shutdown_grace;
        let parent_core = self.parent_core.clone();
        let tracked = ctx
            .managed_scope()
            .expect("nested supervisor requires managed placement")
            .track_lifecycle(async move {
                match select(
                    Box::pin(stop_parent.cancelled()),
                    Box::pin(observe.cancel.cancelled()),
                )
                .await
                {
                    Either::Left(_) => {
                        trace::atomic(|| {
                            observe.close();
                            trace::tree(observe.trace, |id| {
                                TreeEvent::new("TPropagate", id)
                                    .cancelled(observe.cancel.is_cancelled())
                            });
                        });
                    }
                    Either::Right(_) => {
                        let failure = system::lock(&observe.failure).clone();
                        if let Some(error) = failure {
                            actor.signal_failure(error);
                        } else {
                            actor.context().stop();
                        }
                    }
                };
                if rt::timeout(grace, system::wait_true(observe.done.subscribe()))
                    .await
                    .is_err()
                {
                    observe.fail_cleanup("nested supervisor cleanup grace exceeded".to_string());
                    observe.stop_remaining();
                }
                let _ = system::wait_true(observe.done.subscribe()).await;
                let failure = system::lock(&observe.cleanup_failure).clone();
                if let (Some(parent), Some(error)) = (parent_core.upgrade(), failure) {
                    // Preserve cleanup's own cause, including a secondary timeout
                    // after an ordinary restartable startup/runtime failure.
                    parent.fail_cleanup(format!("nested supervisor cleanup failed: {error}"));
                }
            });
        if tracked.is_err() {
            // No subtree has started. Normal admission closure is cancellation,
            // not an adapter panic; dropping builder completes unstarted cleanup.
            core.close();
            return;
        }
        let started = select(
            Box::pin(parent.cancelled()),
            Box::pin(builder.start_owned(false)),
        )
        .await;
        match started {
            Either::Left(_) => {
                core.close();
            }
            Either::Right((Ok(supervisor), _)) => {
                self.id = Some(ctx.id());
                self.running = Some(supervisor.clone());
                let old = { system::lock(&self.ready).replace((ctx.id(), supervisor)) };
                drop(old);
            }
            Either::Right((Err(error), _)) => {
                if parent.is_cancelled() {
                    core.close();
                } else {
                    // Fail this hook before the adapter can publish ready. An
                    // enqueued FailureEnvelope could run only after readiness.
                    panic!("nested supervisor startup failed: {error}");
                }
            }
        }
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        if let Some(supervisor) = &self.running {
            supervisor.stop();
        }
    }
}
impl Drop for SupervisorActor {
    fn drop(&mut self) {
        if let Some(supervisor) = &self.running {
            supervisor.stop();
        }
        let old = {
            let mut ready = system::lock(&self.ready);
            if self.id.is_some() && ready.as_ref().map(|(id, _)| *id) == self.id {
                ready.take()
            } else {
                None
            }
        };
        drop(old);
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    fn adapter(builder: SupervisorBuilder) -> SupervisorActor {
        SupervisorActor {
            builder: Some(builder),
            running: None,
            ready: Arc::new(Mutex::new(None)),
            id: None,
            shutdown_grace: Duration::from_secs(1),
            parent_core: Weak::new(),
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_hook_poll_after_admission_closes_is_normal_cancellation() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("closed-nested-hook", 1).unwrap();
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (scope, lease) = group.reserve(None).unwrap();
        let blocker = scope.handle.spawn(async move {
            let _ = entered_tx.send(());
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        entered.await.unwrap();
        let context_owner = ActorRef::spawn_managed(
            adapter(system.supervisor(SupervisorConfig::default()).unwrap()),
            scope.clone(),
            lease,
        )
        .unwrap();
        let mut hook = adapter(system.supervisor(SupervisorConfig::default()).unwrap());
        let cleanup = hook.builder.as_ref().unwrap().core.clone();
        scope.close_admission();
        // Exercise the narrow gap between the actor's can_start check and the
        // first poll of the nested started hook, with real closed scope state.
        hook.started(&context_owner.context()).await;
        assert!(*cleanup.done.borrow());
        assert!(cleanup.failure.lock().unwrap().is_none());
        scope.stop(ExitReason::Shutdown);
        release.send(()).unwrap();
        blocker.await.unwrap();
        context_owner.child_handle().wait_exit_async().await;
        assert!(system.shutdown(Duration::from_secs(5)).await.complete);
    }
}
