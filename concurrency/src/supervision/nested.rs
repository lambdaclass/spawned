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
                            actor.signal_failure(error.message);
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
    // Stop the subtree inside the lifecycle, so a panic while stopping it
    // (e.g. a panicking waker woken by the core's admission close) is the
    // actor's `stopped()` panic, and Drop below still retries the stop.
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
        crate::test_wait::within(
            concat!(
                module_path!(),
                "::first_hook_poll_after_admission_closes_is_normal_cancellation"
            ),
            async move {
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
            },
        )
        .await;
    }
}

// Kill cargo-mutants survivors in `resolve` and the adapter's Drop (docs/TESTING.md,
// "Planted-bug sensitivity"): a handle resolves only to the current, open
// subtree, and a dropped adapter stops its subtree and clears only its own
// published entry.
#[cfg(test)]
mod resolve_and_drop_tests {
    use super::*;

    async fn running_tree(system: &ActorSystem) -> Supervisor {
        system
            .supervisor(SupervisorConfig::default())
            .unwrap()
            .start()
            .await
            .unwrap()
    }
    fn adapter(running: Option<Supervisor>, ready: &Ready, id: Option<ActorId>) -> SupervisorActor {
        SupervisorActor {
            builder: None,
            running,
            ready: ready.clone(),
            id,
            shutdown_grace: Duration::from_secs(1),
            parent_core: Weak::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_requires_the_current_id_and_an_open_tree() {
        crate::test_wait::within(
            concat!(
                module_path!(),
                "::resolve_requires_the_current_id_and_an_open_tree"
            ),
            async move {
                let limit = Duration::from_secs(5);
                let system = ActorSystem::new().unwrap();
                let tree = running_tree(&system).await;
                let (current, other) = (ActorId::next(), ActorId::next());
                let ready: Ready = Arc::new(Mutex::new(Some((current, tree.clone()))));
                assert!(resolve(&ready, current).is_ok());
                // An open tree published for another adapter instance never resolves.
                assert!(resolve(&ready, other).is_err());
                tree.stop();
                tree.join().await;
                // The current adapter's tree, once closed, no longer resolves either.
                assert!(resolve(&ready, current).is_err());
                assert!(system.shutdown(limit).await.complete);
            },
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_adapter_stops_its_tree_and_clears_only_its_own_entry() {
        crate::test_wait::within(
            concat!(
                module_path!(),
                "::dropped_adapter_stops_its_tree_and_clears_only_its_own_entry"
            ),
            async move {
                let limit = Duration::from_secs(5);
                let system = ActorSystem::new().unwrap();
                let (mine, newer) = (ActorId::next(), ActorId::next());

                // Its own published entry is cleared and its subtree stopped.
                let tree = running_tree(&system).await;
                let ready: Ready = Arc::new(Mutex::new(Some((mine, tree.clone()))));
                drop(adapter(Some(tree.clone()), &ready, Some(mine)));
                assert!(system::lock(&ready).is_none());
                assert!(!tree.core.gate.is_open());

                // A newer generation's entry survives an older adapter's drop.
                let replacement = running_tree(&system).await;
                let ready: Ready = Arc::new(Mutex::new(Some((newer, replacement.clone()))));
                drop(adapter(None, &ready, Some(mine)));
                assert_eq!(
                    system::lock(&ready).as_ref().map(|(id, _)| *id),
                    Some(newer)
                );
                assert!(replacement.core.gate.is_open());
                replacement.stop();
                assert!(system.shutdown(limit).await.complete);
            },
        )
        .await;
    }
}

#[cfg(test)]
mod stop_panic_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Leaf;
    impl Actor for Leaf {}

    /// Panics the first time it is woken.
    struct PanicOnWake(AtomicBool);
    impl std::task::Wake for PanicOnWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            if !self.0.swap(true, Ordering::SeqCst) {
                panic!("waiter waker panicked");
            }
        }
    }

    /// Stopping the subtree runs foreign code (wakers of inner waiters). The
    /// adapter does that in its `stopped()` hook, so such a panic is a
    /// `stopped()` panic of the adapter (with Drop as a backstop), rather than
    /// a destructor failure. The hook is called directly: through the actor,
    /// the adapter's lifecycle helper races it to close the subtree first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopped_hook_stops_the_running_subtree() {
        crate::test_wait::within(
            concat!(module_path!(), "::stopped_hook_stops_the_running_subtree"),
            async move {
                use futures::FutureExt;
                let limit = Duration::from_secs(10);
                let system = ActorSystem::new().unwrap();
                let group = system.async_group("nested-stop-panic", 1).unwrap();
                let published: Arc<Mutex<Option<SupervisedChild<Leaf>>>> = Arc::default();
                let expose = published.clone();
                let placement = group.clone();
                let mut root = system.supervisor(SupervisorConfig::default()).unwrap();
                let subtree = root
                    .add_supervisor(SupervisorSpec::new(
                        "subtree",
                        &group,
                        SupervisorConfig::default(),
                        move |tree| {
                            let leaf = tree.add(ChildSpec::new("leaf", &placement, |_| async {
                                Ok::<_, String>(Leaf)
                            }))?;
                            *system::lock(&expose) = Some(leaf);
                            Ok::<_, SupervisorError>(())
                        },
                    ))
                    .unwrap();
                let root = root.start().await.unwrap();
                let leaf = system::lock(&published).clone().unwrap();
                let mut waiter = Box::pin(leaf.wait_for_generation(leaf.snapshot().generation));
                let waker = std::task::Waker::from(Arc::new(PanicOnWake(AtomicBool::new(false))));
                let mut cx = std::task::Context::from_waker(&waker);
                assert!(waiter.as_mut().poll(&mut cx).is_pending());

                let inner = subtree.current().unwrap();
                let ctx = subtree.child.current().unwrap().context();
                let mut hook = SupervisorActor {
                    builder: None,
                    running: Some(inner.clone()),
                    ready: Arc::default(),
                    id: None,
                    shutdown_grace: limit,
                    parent_core: std::sync::Weak::new(),
                };
                let unwound = AssertUnwindSafe(hook.stopped(&ctx)).catch_unwind().await;
                let payload = unwound.expect_err("stopped() did not stop the running subtree");
                assert_eq!(
                    crate::error::panic_message(&*payload),
                    "waiter waker panicked"
                );
                // The backstop stop in Drop is idempotent once the waiter has fired.
                drop(hook);
                assert!(!inner.core.gate.is_open());
                drop(waiter);
                drop(root);
                assert!(system.shutdown(limit).await.complete);
            },
        )
        .await;
    }
}
