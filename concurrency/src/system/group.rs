use super::*;
use std::{sync::mpsc, thread};

type WorkFuture = BoxFuture<'static, ()>;
enum Work {
    Actor(WorkFuture),
    Stop,
}
struct Target {
    handle: Handle,
    native: Option<mpsc::Sender<Work>>,
}
pub(crate) struct GroupInner {
    pub(super) name: String,
    pub(super) profile: ExecutionProfile,
    pub(super) state: Mutex<GroupState>,
    targets: Vec<Target>,
    cursor: std::sync::atomic::AtomicUsize,
    finish: mpsc::Sender<()>,
    pub(super) done: watch::Receiver<bool>,
    pub(super) failures: Arc<Mutex<Vec<String>>>,
}
impl GroupInner {
    pub(super) fn new(
        name: String,
        profile: ExecutionProfile,
        borrowed: Option<Handle>,
    ) -> Result<Arc<Self>, SystemError> {
        let stop = CancellationToken::new();
        let mut owners = Vec::new();
        let mut targets = Vec::new();
        let result = (|| -> Result<(), SystemError> {
            match &profile {
                ExecutionProfile::AsyncPool { workers } => {
                    let handle = match borrowed {
                        Some(handle) => handle,
                        None => build_runtime(
                            format!("spawned-{name}"),
                            *workers,
                            None,
                            stop.clone(),
                            &mut owners,
                        )?,
                    };
                    targets.push(Target {
                        handle,
                        native: None,
                    });
                }
                ExecutionProfile::PinnedShards { cpus } => {
                    for (index, cpu) in cpus.iter().enumerate() {
                        let handle = build_runtime(
                            format!("spawned-{name}-shard-{index}"),
                            0,
                            Some(*cpu),
                            stop.clone(),
                            &mut owners,
                        )
                        .map_err(|error| {
                            SystemError::Runtime(format!("pinned shard {index} failed: {error}"))
                        })?;
                        targets.push(Target {
                            handle,
                            native: None,
                        });
                    }
                }
                ExecutionProfile::ThreadPool {
                    workers,
                    helper_workers,
                } => {
                    let helper = build_runtime(
                        format!("spawned-{name}-helper"),
                        *helper_workers,
                        None,
                        stop.clone(),
                        &mut owners,
                    )?;
                    for index in 0..*workers {
                        let (sender, receiver) = mpsc::channel();
                        let handle = helper.clone();
                        let worker = thread::Builder::new()
                            .name(format!("spawned-{name}-actor-{index}"))
                            .spawn(move || {
                                while let Ok(Work::Actor(future)) = receiver.recv() {
                                    if let Err(payload) = std::panic::catch_unwind(
                                        std::panic::AssertUnwindSafe(|| handle.block_on(future)),
                                    ) {
                                        tracing::error!(
                                            "native actor root unwound: {}",
                                            crate::error::panic_message(&*payload)
                                        );
                                    }
                                }
                            })
                            .map_err(|e| SystemError::Runtime(e.to_string()))?;
                        owners.push(worker);
                        targets.push(Target {
                            handle: helper.clone(),
                            native: Some(sender),
                        });
                    }
                }
            }
            fail_point!("system.group_start" => {
                return Err(SystemError::Runtime("failpoint system.group_start".into()))
            });
            Ok(())
        })();
        if let Err(error) = result {
            rollback(&stop, &targets, owners);
            return Err(error);
        }
        let owners = Arc::new(Mutex::new(Some(owners)));
        let stopping = stop.clone();
        let joining = owners.clone();
        let native_stop: Vec<_> = targets.iter().filter_map(|t| t.native.clone()).collect();
        let (finish, finished) = mpsc::channel();
        let (done_tx, done) = watch::channel(false);
        let failures = Arc::new(Mutex::new(Vec::new()));
        let errors = failures.clone();
        // Preallocate teardown capacity before admitting actors. A coordinator
        // never runs application work and signals done after actual owner joins,
        // including native/pinned worker TLS destructors and Runtime::drop.
        let coordinator = thread::Builder::new()
            .name(format!("spawned-{name}-join"))
            .spawn(move || {
                let _ = finished.recv();
                stopping.cancel();
                for native in native_stop {
                    let _ = native.send(Work::Stop);
                }
                let owners = lock(&joining).take().unwrap_or_default();
                for owner in owners {
                    if let Err(payload) = owner.join() {
                        lock(&errors).push(crate::error::panic_message(&*payload));
                    }
                }
                done_tx.send_replace(true);
            });
        if let Err(error) = coordinator {
            rollback(&stop, &targets, lock(&owners).take().unwrap_or_default());
            return Err(SystemError::Runtime(error.to_string()));
        }
        let free = if matches!(profile, ExecutionProfile::ThreadPool { .. }) {
            Some((0..targets.len()).collect())
        } else {
            None
        };
        Ok(Arc::new(Self {
            name,
            profile,
            state: Mutex::new(GroupState {
                closing: false,
                retiring: false,
                scopes: BTreeMap::new(),
                free,
            }),
            targets,
            cursor: std::sync::atomic::AtomicUsize::new(0),
            finish,
            done,
            failures,
        }))
    }
    pub(super) fn choose(
        &self,
        state: &mut GroupState,
        selection: Option<usize>,
    ) -> Result<usize, SystemError> {
        let start = selection
            .unwrap_or_else(|| self.cursor.fetch_add(1, Ordering::Relaxed) % self.targets.len());
        if start >= self.targets.len() {
            return Err(SystemError::InvalidShard);
        }
        if let Some(free) = &mut state.free {
            let target = if selection.is_some() {
                free.get(&start).copied()
            } else {
                free.range(start..).next().or_else(|| free.first()).copied()
            }
            .ok_or(SystemError::Capacity)?;
            free.remove(&target);
            Ok(target)
        } else {
            Ok(start)
        }
    }
    pub(super) fn handle(&self, target: usize) -> Handle {
        self.targets[target].handle.clone()
    }
    pub(super) fn shard_count(&self) -> usize {
        self.targets.len()
    }
    pub(super) fn dispatch<F>(&self, target: usize, future: F) -> Result<(), SystemError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match &self.targets[target].native {
            Some(sender) => sender
                .send(Work::Actor(Box::pin(future)))
                .map_err(|_| SystemError::Runtime("native worker is unavailable".into())),
            None => {
                drop(self.targets[target].handle.spawn(future));
                Ok(())
            }
        }
    }
    pub(super) fn close_admission(&self) {
        lock(&self.state).closing = true;
        self.maybe_finish();
    }
    pub(super) fn remove_scope(&self, id: u64) {
        let removed = {
            let mut state = lock(&self.state);
            let removed = state.scopes.remove(&id);
            if let Some(scope) = &removed {
                if let Some(free) = &mut state.free {
                    free.insert(scope.target);
                }
            }
            removed
        };
        drop(removed);
        self.maybe_finish();
    }
    fn maybe_finish(&self) {
        let finish = {
            let mut state = lock(&self.state);
            if state.closing && state.scopes.is_empty() && !state.retiring {
                state.retiring = true;
                true
            } else {
                false
            }
        };
        if finish {
            let _ = self.finish.send(());
        }
    }
}
fn rollback(stop: &CancellationToken, targets: &[Target], owners: Vec<thread::JoinHandle<()>>) {
    stop.cancel();
    for target in targets {
        if let Some(native) = &target.native {
            let _ = native.send(Work::Stop);
        }
    }
    for owner in owners {
        let _ = owner.join();
    }
}
fn build_runtime(
    name: String,
    workers: usize,
    cpu: Option<usize>,
    stop: CancellationToken,
    owners: &mut Vec<thread::JoinHandle<()>>,
) -> Result<Handle, SystemError> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let owner = thread::Builder::new()
        .name(format!("{name}-owner"))
        .spawn(move || {
            let built = std::panic::catch_unwind(|| -> std::io::Result<rt::Runtime> {
                if let Some(cpu) = cpu {
                    rt::affinity::pin_current(cpu)?;
                }
                if workers == 0 {
                    rt::control_runtime()
                } else {
                    rt::worker_runtime(&name, workers)
                }
            });
            match built {
                Ok(Ok(runtime)) => {
                    if sender.send(Ok(runtime.handle().clone())).is_ok() {
                        runtime.block_on(stop.cancelled());
                    }
                    drop(runtime);
                }
                Ok(Err(error)) => {
                    let _ = sender.send(Err(error.to_string()));
                }
                Err(_) => {
                    let _ = sender.send(Err("runtime construction panicked".into()));
                }
            }
        })
        .map_err(|e| SystemError::Runtime(e.to_string()))?;
    owners.push(owner);
    receiver
        .recv()
        .map_err(|e| SystemError::Runtime(e.to_string()))?
        .map_err(SystemError::Runtime)
}
