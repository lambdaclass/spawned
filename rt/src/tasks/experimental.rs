//! Feature-gated research hooks, not a stable runtime selection API.
//! Process-owned current-thread Tokio shards. Actor count does not own runtime
//! lifetime: detached helpers and blocking children may outlive their parent.
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
    fn new(count: usize) -> io::Result<Self> {
        assert!(count > 0);
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
        };
        for id in 0..count {
            let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
            let (ready, initialized) = std::sync::mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name(format!("spawned-shard-{id}"))
                .spawn(move || {
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
                        while let Some(job) = rx.recv().await {
                            tokio::spawn(job);
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
    let pool = {
        let mut registry = registry().lock().unwrap();
        if let Some(pool) = registry.as_ref() {
            assert_eq!(pool.count, count, "one configuration per process");
            pool.clone()
        } else {
            let pool = Arc::new(Pool::new(count)?);
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
