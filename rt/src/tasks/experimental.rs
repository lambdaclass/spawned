//! Feature-gated research hooks, not a stable runtime selection API.
//! Process-owned current-thread Tokio shards. Actor count does not own runtime
//! lifetime: detached helpers and blocking children may outlive their parent.
#[doc(hidden)]
pub mod dispatcher;

#[doc(hidden)]
pub fn try_task_id() -> Option<tokio::task::Id> {
    tokio::task::try_id()
}
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
pub use tokio::task::yield_now;
type Job = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
struct State {
    senders: Option<Vec<mpsc::UnboundedSender<Job>>>,
    active: usize,
}
struct Pool {
    state: Mutex<State>,
    idle: Condvar,
    workers: Mutex<Vec<JoinHandle<()>>>,
    worker_ids: Vec<thread::ThreadId>,
    count: usize,
    next: AtomicUsize,
    affinity: Option<Vec<usize>>,
    spin: bool,
}
struct Lease(Arc<Pool>);
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.active -= 1;
        self.0.idle.notify_all();
    }
}
impl Pool {
    #[cfg(test)]
    fn new(count: usize) -> io::Result<Self> {
        Self::configured(count, None, false)
    }
    fn configured(count: usize, affinity: Option<Vec<usize>>, spin: bool) -> io::Result<Self> {
        assert!(count > 0);
        if affinity.as_ref().is_some_and(|cores| cores.len() != count) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "one CPU per shard required",
            ));
        }
        let mut pool = Self {
            state: Mutex::new(State {
                senders: Some(Vec::new()),
                active: 0,
            }),
            idle: Condvar::new(),
            workers: Mutex::new(Vec::new()),
            worker_ids: Vec::new(),
            count,
            next: AtomicUsize::new(0),
            affinity: affinity.clone(),
            spin,
        };
        for id in 0..count {
            let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
            let (ready, initialized) = std::sync::mpsc::sync_channel(1);
            let core = affinity.as_ref().map(|cores| cores[id]);
            let worker = thread::Builder::new()
                .name(format!("spawned-shard-{id}"))
                .spawn(move || {
                    if let Some(core) = core {
                        if let Err(error) = pin_core(core) {
                            let _ = ready.send(Err(error));
                            return;
                        }
                    }
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let _ = ready.send(Err(error));
                            return;
                        }
                    };
                    let _ = ready.send(Ok(()));
                    runtime.block_on(async {
                        loop {
                            let job = if spin {
                                match rx.try_recv() {
                                    Ok(job) => Some(job),
                                    Err(mpsc::error::TryRecvError::Empty) => {
                                        // Always-ready owner keeps the driver polling; yield
                                        // also lets actor tasks, timers and I/O make progress.
                                        tokio::task::yield_now().await;
                                        continue;
                                    }
                                    Err(mpsc::error::TryRecvError::Disconnected) => None,
                                }
                            } else {
                                rx.recv().await
                            };
                            let Some(job) = job else { break };
                            tokio::spawn(job);
                            if spin {
                                tokio::task::yield_now().await;
                            }
                        }
                    });
                    // Explicit pool shutdown cancels detached async helpers. Already-running
                    // blocking helpers cannot be forcibly interrupted and may outlive shutdown.
                    runtime.shutdown_background();
                })?;
            pool.state
                .get_mut()
                .unwrap()
                .senders
                .as_mut()
                .unwrap()
                .push(tx);
            pool.worker_ids.push(worker.thread().id());
            pool.workers.get_mut().unwrap().push(worker);
            initialized.recv().map_err(io::Error::other)??;
        }
        Ok(pool)
    }
    fn spawn(
        self: &Arc<Self>,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> io::Result<()> {
        let tx = {
            let mut state = self.state.lock().unwrap();
            let senders = state
                .senders
                .as_ref()
                .ok_or_else(|| io::Error::other("experimental shards are shut down"))?;
            let tx = senders[self.next.fetch_add(1, Ordering::Relaxed) % senders.len()].clone();
            state.active += 1;
            tx
        };
        let lease = Lease(self.clone());
        // Send/drop outside the state lock: a rejected job drops its lease.
        tx.send(Box::pin(async move {
            let _lease = lease;
            future.await;
        }))
        .map_err(|_| io::Error::other("experimental actor shard terminated"))
    }
    fn shutdown(&self) -> io::Result<()> {
        if self.worker_ids.contains(&thread::current().id()) {
            return Err(io::Error::other(
                "shutdown must run outside the shard workers",
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = self.state.lock().unwrap();
        // join() publishes actor completion just before the job releases its lease.
        // Wait for that release, without closing a runtime under a live actor.
        while state.active != 0 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(io::Error::other(
                    "stop and join all actors before shard shutdown",
                ));
            };
            state = self.idle.wait_timeout(state, remaining).unwrap().0;
        }
        let senders = state.senders.take();
        drop(state);
        drop(senders);
        for worker in self.workers.lock().unwrap().drain(..) {
            worker
                .join()
                .map_err(|_| io::Error::other("experimental shard panicked"))?;
        }
        Ok(())
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        // Construction failures also close and join any partially initialized pool.
        self.state.get_mut().unwrap().senders.take();
        for worker in self.workers.get_mut().unwrap().drain(..) {
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

fn pin_core(core: usize) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let thread = std::fs::read_link("/proc/thread-self")?;
        let tid = thread
            .file_name()
            .ok_or_else(|| io::Error::other("missing thread id"))?;
        let result = std::process::Command::new("taskset")
            .args(["-pc", &core.to_string()])
            .arg(tid)
            .output()?;
        if !result.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&result.stderr).into_owned(),
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = core;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "research pinning requires Linux taskset",
        ))
    }
}
fn shard_options() -> io::Result<(Option<Vec<usize>>, bool)> {
    let affinity = std::env::var("SPAWNED_EXPERIMENT_PIN_CORES")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(|cpu| {
                    cpu.parse::<usize>().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "invalid CPU list")
                    })
                })
                .collect::<io::Result<Vec<_>>>()
        })
        .transpose()?;
    if affinity
        .as_ref()
        .is_some_and(|v| v.iter().collect::<std::collections::HashSet<_>>().len() != v.len())
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "duplicate CPU"));
    }
    let spin = match std::env::var("SPAWNED_EXPERIMENT_IDLE_SPIN").as_deref() {
        Ok("1") => true,
        Ok("0") | Err(_) => false,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "idle spin must be 0/1",
            ))
        }
    };
    Ok((affinity, spin))
}

fn registry() -> &'static Mutex<Option<Arc<Pool>>> {
    static REGISTRY: OnceLock<Mutex<Option<Arc<Pool>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}
/// Research-only dispatch. Initialization failures drop the actor future, letting
/// its ordinary completion channel report termination; no caller-thread panic.
pub fn spawn_sharded(future: impl Future<Output = ()> + Send + 'static) -> io::Result<()> {
    let count: usize = std::env::var("SPAWNED_EXPERIMENT_WORKERS")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .expect("SPAWNED_EXPERIMENT_WORKERS must be an integer");
    assert!(
        (1..=256).contains(&count),
        "experimental worker count: 1..=256"
    );
    let (affinity, spin) = shard_options()?;
    let pool = {
        let mut registry = registry().lock().unwrap();
        if let Some(pool) = registry.as_ref() {
            assert_eq!(pool.count, count, "one configuration per process");
            assert_eq!(pool.affinity, affinity, "one affinity per process");
            assert_eq!(pool.spin, spin, "one waiting policy per process");
            pool.clone()
        } else {
            let pool = Arc::new(Pool::configured(count, affinity, spin)?);
            *registry = Some(pool.clone());
            pool
        }
    };
    pool.spawn(future)
}
/// Track other actor backends that can borrow a shard reactor (notably blocking
/// children). Conservatively tracks them whenever this experimental pool exists.
#[doc(hidden)]
pub struct RuntimeLease {
    _lease: Lease,
}
#[doc(hidden)]
pub fn lease_if_initialized() -> Option<RuntimeLease> {
    let pool = registry().lock().unwrap().clone()?;
    let mut state = pool.state.lock().unwrap();
    state.senders.as_ref()?;
    state.active += 1;
    drop(state);
    Some(RuntimeLease {
        _lease: Lease(pool),
    })
}

/// Final, one-shot experimental pool shutdown. Stop/join actors first and await
/// helpers whose results matter. Cancels remaining async helpers; no reinitialization.
pub fn shutdown_sharded() -> io::Result<()> {
    let pool = registry().lock().unwrap().clone();
    if let Some(pool) = pool {
        pool.shutdown()?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_wait_modes_progress_and_join() {
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        let allowed = status
            .lines()
            .find_map(|s| s.strip_prefix("Cpus_allowed_list:"))
            .unwrap()
            .trim();
        let core: usize = allowed.split([',', '-']).next().unwrap().parse().unwrap();
        for spin in [false, true] {
            let pool = Arc::new(Pool::configured(1, Some(vec![core]), spin).unwrap());
            let (tx, rx) = std::sync::mpsc::channel();
            pool.spawn(async move {
                tokio::time::sleep(Duration::from_millis(2)).await;
                let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
                let actual = status
                    .lines()
                    .find_map(|s| s.strip_prefix("Cpus_allowed_list:"))
                    .unwrap()
                    .trim();
                tx.send(actual.to_owned()).unwrap();
            })
            .unwrap();
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                core.to_string()
            );
            pool.shutdown().unwrap();
            assert!(pool.workers.lock().unwrap().is_empty());
        }
        assert!(Pool::configured(2, Some(vec![core]), false).is_err());
    }
    #[test]
    fn idle_pool_joins_all_workers() {
        let pool = Pool::new(2).unwrap();
        pool.shutdown().unwrap();
        assert!(pool.workers.lock().unwrap().is_empty());
    }
    #[test]
    fn shutdown_waits_for_registered_jobs_and_is_terminal() {
        let pool = Arc::new(Pool::new(2).unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        pool.spawn(async move { rx.await.unwrap() }).unwrap();
        let other = pool.clone();
        let close = thread::spawn(move || other.shutdown());
        tx.send(()).unwrap();
        close.join().unwrap().unwrap();
        assert!(pool.spawn(async {}).is_err());
        pool.shutdown().unwrap(); // idempotent
    }
    #[test]
    fn shutdown_from_a_shard_is_rejected_without_self_join() {
        let pool = Arc::new(Pool::new(1).unwrap());
        let other = pool.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        pool.spawn(async move {
            tx.send(other.shutdown().is_err()).unwrap();
        })
        .unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        pool.shutdown().unwrap();
    }
}
