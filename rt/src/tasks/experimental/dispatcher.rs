//! Research-only native actor executor, plus a process-owned Tokio control.
//!
//! Native actor futures are polled by our job scheduler, not by Tokio tasks.
//! A shared Tokio runtime supplies I/O, timers, helpers and a single-poll
//! cooperative-budget bridge. Native actor task IDs are intentionally absent.
use super::super::dispatch_gate::Gate;
use std::{
    cell::Cell,
    collections::VecDeque,
    future::Future,
    io,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, OnceLock, Weak,
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
};
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(5);
type FutureJob = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
thread_local! {static POLLING:Cell<Option<(usize,usize)>>=const {Cell::new(None)};}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Global,
    Local,
    Steal,
    Root,
    TokioPool,
}
struct Active {
    count: usize,
    closed: bool,
}
struct Queues {
    global: VecDeque<Arc<Job>>,
    local: Vec<VecDeque<Arc<Job>>>,
}
struct Pool {
    policy: Policy,
    count: usize,
    local_budget: usize,
    active: Mutex<Active>,
    idle: Condvar,
    closed: AtomicBool,
    teardown: Mutex<()>,
    queues: Mutex<Queues>,
    split: Vec<Mutex<VecDeque<Arc<Job>>>>,
    sleepers: Vec<AtomicBool>,
    wake_handles: Vec<OnceLock<thread::Thread>>,
    wake_next: AtomicUsize,
    root_ready: Vec<tokio::sync::Notify>,
    ready: Condvar,
    next: AtomicUsize,
    workers: Mutex<Vec<JoinHandle<()>>>,
    ids: Arc<Mutex<Vec<thread::ThreadId>>>,
    reactor_stop: Mutex<Option<oneshot::Sender<()>>>,
    owner: Mutex<Option<JoinHandle<()>>>,
    handle: Handle,
    tokio_jobs: Mutex<Option<mpsc::UnboundedSender<FutureJob>>>,
}
struct Lease(Arc<Pool>);
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.0.active.lock().unwrap();
        state.count -= 1;
        self.0.idle.notify_all();
    }
}
struct Job {
    gate: Gate,
    future: Mutex<Option<FutureJob>>,
    pool: Weak<Pool>,
    preferred: AtomicUsize,
}
impl Wake for Job {
    fn wake(self: Arc<Self>) {
        self.schedule();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.schedule();
    }
}
impl Job {
    fn schedule(self: &Arc<Self>) {
        if self.gate.notify() {
            if let Some(pool) = self.pool.upgrade() {
                pool.enqueue(self.clone());
            }
        }
    }
    fn poll(self: Arc<Self>, worker: usize, handle: &Handle, bridge: bool) {
        if !self.gate.claim() {
            return;
        }
        self.preferred.store(worker, Ordering::Relaxed);
        let waker = Waker::from(self.clone());
        let mut cx = Context::from_waker(&waker);
        let mut future = self.future.lock().unwrap();
        let old = POLLING.with(|p| p.replace(Some((self.pool.as_ptr() as usize, worker))));
        struct Reset(Option<(usize, usize)>);
        impl Drop for Reset {
            fn drop(&mut self) {
                POLLING.with(|p| p.set(self.0));
            }
        }
        let reset = Reset(old);
        // Handle::block_on installs Tokio's runtime and cooperative budget on
        // this native thread. The wrapper always returns Ready after one actor
        // poll, so it never parks waiting on the actor or uses its own waker.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            if bridge {
                handle.block_on(std::future::poll_fn(|_| {
                    Poll::Ready(future.as_mut().unwrap().as_mut().poll(&mut cx))
                }))
            } else {
                // The one-per-worker Tokio root driver already owns the
                // cooperative budget and a persistent shared-I/O handle scope.
                future.as_mut().unwrap().as_mut().poll(&mut cx)
            }
        }));
        drop(reset);
        if matches!(outcome, Ok(Poll::Pending)) {
            drop(future);
            if self.gate.pending() {
                if let Some(pool) = self.pool.upgrade() {
                    pool.enqueue_from(self.clone(), Some(worker));
                }
            }
        } else {
            let finished = future.take();
            drop(future);
            self.gate.complete();
            // Drop after unlocking. Unwinding an actor future closes its normal
            // completion channel; the library reports Kill when unpublished.
            let dropped = catch_unwind(AssertUnwindSafe(|| drop(finished)));
            if outcome.is_err() || dropped.is_err() {
                tracing::error!("Experimental native actor future panicked");
            }
        }
    }
}
impl Pool {
    fn new(
        policy: Policy,
        count: usize,
        reactors: usize,
        local_budget: usize,
    ) -> io::Result<Arc<Self>> {
        let ids = Arc::new(Mutex::new(Vec::new()));
        let (stop, stopped) = oneshot::channel();
        let (ready, initialized) = std::sync::mpsc::sync_channel(1);
        let (job_tx, mut job_rx) = mpsc::unbounded_channel::<FutureJob>();
        let ids_clone = ids.clone();
        let runtime_threads = if policy == Policy::TokioPool {
            count
        } else {
            reactors
        };
        let owner = thread::Builder::new()
            .name("actor-reactor-owner".into())
            .spawn(move || {
                let ids_start = ids_clone.clone();
                let ids_stop = ids_clone.clone();
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder
                    .worker_threads(runtime_threads)
                    .thread_name(if policy == Policy::TokioPool {
                        "actor-worker-pool"
                    } else {
                        "actor-reactor"
                    })
                    .on_thread_start(move || ids_start.lock().unwrap().push(thread::current().id()))
                    .on_thread_stop(move || {
                        ids_stop
                            .lock()
                            .unwrap()
                            .retain(|id| *id != thread::current().id())
                    })
                    .enable_all();
                #[cfg(test)]
                builder.thread_keep_alive(Duration::from_millis(10));
                let runtime = builder.build();
                let runtime = match runtime {
                    Ok(r) => r,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(runtime.handle().clone())).is_err() {
                    return;
                }
                runtime.block_on(async {
                    if policy == Policy::TokioPool {
                        while let Some(job) = job_rx.recv().await {
                            tokio::spawn(job);
                        }
                    } else {
                        let _ = stopped.await;
                    }
                });
                runtime.shutdown_timeout(SHUTDOWN_LIMIT);
            })?;
        ids.lock().unwrap().push(owner.thread().id());
        let handle = match initialized.recv() {
            Ok(Ok(handle)) => handle,
            Ok(Err(error)) => {
                let _ = owner.join();
                return Err(error);
            }
            Err(error) => {
                let _ = owner.join();
                return Err(io::Error::other(error));
            }
        };
        let pool = Arc::new(Self {
            policy,
            count,
            local_budget,
            active: Mutex::new(Active {
                count: 0,
                closed: false,
            }),
            idle: Condvar::new(),
            closed: AtomicBool::new(false),
            teardown: Mutex::new(()),
            queues: Mutex::new(Queues {
                global: VecDeque::new(),
                local: (0..count).map(|_| VecDeque::new()).collect(),
            }),
            split: (0..count).map(|_| Mutex::new(VecDeque::new())).collect(),
            sleepers: (0..count).map(|_| AtomicBool::new(false)).collect(),
            wake_handles: (0..count).map(|_| OnceLock::new()).collect(),
            wake_next: AtomicUsize::new(0),
            root_ready: (0..count).map(|_| tokio::sync::Notify::new()).collect(),
            ready: Condvar::new(),
            next: AtomicUsize::new(0),
            workers: Mutex::new(Vec::new()),
            ids,
            reactor_stop: Mutex::new(Some(stop)),
            owner: Mutex::new(Some(owner)),
            handle,
            tokio_jobs: Mutex::new(if policy == Policy::TokioPool {
                Some(job_tx)
            } else {
                None
            }),
        });
        if policy != Policy::TokioPool {
            for worker in 0..count {
                let cloned = pool.clone();
                let (started, startup) = std::sync::mpsc::sync_channel::<io::Result<()>>(1);
                match thread::Builder::new()
                    .name(format!("actor-worker-n{worker}"))
                    .spawn(move || {
                        if cloned.policy == Policy::Root {
                            match tokio::runtime::Builder::new_current_thread().build() {
                                Ok(runtime) => {
                                    if started.send(Ok(())).is_ok() {
                                        cloned.run_root(worker, runtime);
                                    }
                                }
                                Err(error) => {
                                    let _ = started.send(Err(error));
                                }
                            }
                        } else {
                            let _ = started.send(Ok(()));
                            cloned.run(worker);
                        }
                    }) {
                    Ok(handle) => {
                        pool.ids.lock().unwrap().push(handle.thread().id());
                        pool.workers.lock().unwrap().push(handle);
                        match startup.recv() {
                            Ok(Ok(())) => {}
                            failure => {
                                let _ = pool.close_workers();
                                return Err(match failure {
                                    Ok(Err(error)) => error,
                                    Err(error) => io::Error::other(error),
                                    _ => unreachable!(),
                                });
                            }
                        }
                    }
                    Err(error) => {
                        let _ = pool.close_workers();
                        return Err(error);
                    }
                }
            }
        }
        Ok(pool)
    }
    fn enqueue(&self, job: Arc<Job>) {
        let current = POLLING
            .with(Cell::get)
            .and_then(|(pool, worker)| (pool == self as *const Self as usize).then_some(worker));
        self.enqueue_from(job, current);
    }
    fn enqueue_from(&self, job: Arc<Job>, current: Option<usize>) {
        if matches!(self.policy, Policy::Steal | Policy::Root) {
            let target =
                current.unwrap_or_else(|| job.preferred.load(Ordering::Relaxed)) % self.count;
            let depth = {
                let mut queue = self.split[target].lock().unwrap();
                queue.push_back(job);
                queue.len()
            };
            if current != Some(target) {
                self.unpark(target);
            }
            if depth > 1 {
                self.unpark_peer(target);
            }
            return;
        }
        let mut queues = self.queues.lock().unwrap();
        if self.policy == Policy::Local {
            let target =
                current.unwrap_or_else(|| job.preferred.load(Ordering::Relaxed)) % self.count;
            queues.local[target].push_back(job);
            // Keep a single cheap continuation local; a periodic steal check
            // also prevents a blocking owner from hiding that continuation.
            if current != Some(target) || queues.local[target].len() > 1 {
                self.ready.notify_one();
            }
        } else {
            queues.global.push_back(job);
            self.ready.notify_one();
        }
    }
    fn unpark(&self, worker: usize) {
        if self.policy == Policy::Root {
            self.root_ready[worker].notify_one();
            return;
        }
        if let Some(handle) = self.wake_handles[worker].get() {
            handle.unpark();
        }
    }
    fn unpark_peer(&self, owner: usize) {
        let first = self.wake_next.fetch_add(1, Ordering::Relaxed);
        for step in 0..self.count {
            let peer = first.wrapping_add(step) % self.count;
            if peer != owner && self.sleepers[peer].load(Ordering::Acquire) {
                self.unpark(peer);
                break;
            }
        }
    }
    fn take_split(&self, worker: usize, oldest: bool, victim: &mut usize) -> Option<Arc<Job>> {
        let own = {
            let mut queue = self.split[worker].lock().unwrap();
            if oldest {
                queue.pop_front()
            } else {
                queue.pop_back()
            }
        };
        own.or_else(|| {
            for _ in 0..self.count {
                let peer = *victim % self.count;
                *victim = (*victim).wrapping_add(1);
                if peer == worker {
                    continue;
                }
                let job = self.split[peer].lock().unwrap().pop_front();
                if job.is_some() {
                    return job;
                }
            }
            None
        })
    }
    fn run_split(&self, worker: usize) {
        let _ = self.wake_handles[worker].set(thread::current());
        let mut local_run = 0;
        let mut victim = worker + 1;
        loop {
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            let oldest = local_run >= self.local_budget;
            let job = self.take_split(worker, oldest, &mut victim).or_else(|| {
                // Register before the final search. An external unpark retains
                // its token even if it races the transition into park_timeout.
                self.sleepers[worker].store(true, Ordering::Release);
                self.take_split(worker, oldest, &mut victim)
            });
            if let Some(job) = job {
                self.sleepers[worker].store(false, Ordering::Release);
                local_run = if oldest { 0 } else { local_run + 1 };
                if !self.split[worker].lock().unwrap().is_empty() {
                    self.unpark_peer(worker);
                }
                job.poll(worker, &self.handle, true);
            } else {
                if self.closed.load(Ordering::Acquire) {
                    return;
                }
                thread::park_timeout(Duration::from_millis(1));
                self.sleepers[worker].store(false, Ordering::Release);
            }
        }
    }
    fn run_root(&self, worker: usize, runtime: tokio::runtime::Runtime) {
        // This root future is driven only by block_on on this native thread.
        // It never migrates and no tasks are spawned on the driver runtime.
        runtime.block_on(async {
            let _io_scope = self.handle.enter();
            let mut local_run = 0;
            let mut victim = worker + 1;
            let mut polls = 0;
            loop {
                if self.closed.load(Ordering::Acquire) {
                    return;
                }
                let oldest = local_run >= self.local_budget;
                let job = if let Some(job) = self.take_split(worker, oldest, &mut victim) {
                    job
                } else {
                    let notified = self.root_ready[worker].notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    self.sleepers[worker].store(true, Ordering::Release);
                    if let Some(job) = self.take_split(worker, oldest, &mut victim) {
                        self.sleepers[worker].store(false, Ordering::Release);
                        job
                    } else {
                        if self.closed.load(Ordering::Acquire) {
                            return;
                        }
                        // Constructed within the shared-I/O handle scope: that
                        // reactor drives the fallback even if another worker blocks.
                        let _ = tokio::time::timeout(Duration::from_millis(1), notified).await;
                        self.sleepers[worker].store(false, Ordering::Release);
                        polls = 0;
                        continue;
                    }
                };
                local_run = if oldest { 0 } else { local_run + 1 };
                if !self.split[worker].lock().unwrap().is_empty() {
                    self.unpark_peer(worker);
                }
                job.poll(worker, &self.handle, false);
                polls += 1;
                if polls == 8 {
                    polls = 0;
                    // Reset the outer cooperative budget and flush deferred
                    // custom-job wakes, without creating one Tokio task per actor.
                    tokio::task::yield_now().await;
                }
            }
        });
    }
    fn run(self: Arc<Self>, worker: usize) {
        if self.policy == Policy::Steal {
            self.run_split(worker);
            return;
        }
        let mut local_run = 0;
        let mut victim = worker + 1;
        loop {
            let job = {
                let mut queues = self.queues.lock().unwrap();
                loop {
                    if self.closed.load(Ordering::Acquire) {
                        return;
                    }
                    let oldest = local_run >= self.local_budget;
                    let own = if self.policy == Policy::Global {
                        queues.global.pop_front()
                    } else if oldest {
                        queues.local[worker].pop_front()
                    } else {
                        queues.local[worker].pop_back()
                    };
                    let job = own.or_else(|| {
                        if self.policy == Policy::Global {
                            return None;
                        }
                        for _ in 0..self.count {
                            let other = victim % self.count;
                            victim += 1;
                            if other == worker {
                                continue;
                            }
                            if let Some(job) = queues.local[other].pop_front() {
                                return Some(job);
                            }
                        }
                        None
                    });
                    if let Some(job) = job {
                        local_run = if oldest { 0 } else { local_run + 1 };
                        if !queues.global.is_empty()
                            || (self.policy == Policy::Local
                                && queues.local.iter().any(|q| !q.is_empty()))
                        {
                            self.ready.notify_one();
                        }
                        break job;
                    }
                    // Bounded idle steal interval: no spinning or stranded local
                    // work if its owner is inside a blocking handler.
                    queues = if self.policy == Policy::Global {
                        self.ready.wait(queues).unwrap()
                    } else {
                        self.ready
                            .wait_timeout(queues, Duration::from_millis(1))
                            .unwrap()
                            .0
                    };
                }
            };
            job.poll(worker, &self.handle, true);
        }
    }
    fn spawn(
        self: &Arc<Self>,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> io::Result<()> {
        {
            let mut state = self.active.lock().unwrap();
            if state.closed {
                return Err(io::Error::other("experimental executor is shut down"));
            }
            state.count += 1;
        }
        let lease = Lease(self.clone());
        let future: FutureJob = Box::pin(async move {
            let _lease = lease;
            future.await;
        });
        if self.policy == Policy::TokioPool {
            let tx = self
                .tokio_jobs
                .lock()
                .unwrap()
                .as_ref()
                .cloned()
                .ok_or_else(|| io::Error::other("Tokio pool closed"))?;
            tx.send(future)
                .map_err(|_| io::Error::other("Tokio pool terminated"))
        } else {
            let job = Arc::new(Job {
                gate: Gate::new(),
                future: Mutex::new(Some(future)),
                pool: Arc::downgrade(self),
                preferred: AtomicUsize::new(self.next.fetch_add(1, Ordering::Relaxed) % self.count),
            });
            job.schedule();
            Ok(())
        }
    }
    fn close_workers(&self) -> io::Result<()> {
        let mut panicked = false;
        // The condition-variable predicate must change under the same lock
        // used by idle workers. Otherwise notify can precede their wait.
        {
            let _queues = self.queues.lock().unwrap();
            self.closed.store(true, Ordering::Release);
        }
        self.ready.notify_all();
        for worker in 0..self.count {
            self.unpark(worker);
        }
        for worker in self.workers.lock().unwrap().drain(..) {
            let id = worker.thread().id();
            panicked |= worker.join().is_err();
            self.ids.lock().unwrap().retain(|value| *value != id);
        }
        self.tokio_jobs.lock().unwrap().take();
        if let Some(stop) = self.reactor_stop.lock().unwrap().take() {
            let _ = stop.send(());
        }
        if let Some(owner) = self.owner.lock().unwrap().take() {
            let id = owner.thread().id();
            panicked |= owner.join().is_err();
            self.ids.lock().unwrap().retain(|value| *value != id);
        }
        let remaining = self.ids.lock().unwrap().len();
        if panicked || remaining != 0 {
            return Err(io::Error::other(format!("executor teardown: panicked={panicked}, remaining runtime threads={remaining}; blocking helpers must finish")));
        }
        Ok(())
    }
    fn shutdown(&self) -> io::Result<()> {
        if self.ids.lock().unwrap().contains(&thread::current().id()) {
            return Err(io::Error::other(
                "shutdown must run outside executor threads",
            ));
        }
        // Check self-join before this lock: a worker must never wait behind
        // an external caller that is waiting for that worker's active lease.
        let _teardown = self.teardown.lock().unwrap();
        let deadline = Instant::now() + SHUTDOWN_LIMIT;
        let mut state = self.active.lock().unwrap();
        while state.count != 0 {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "stop/join actors before shutdown: {} active runtime leases",
                        state.count
                    ))
                })?;
            state = self.idle.wait_timeout(state, remaining).unwrap().0;
        }
        state.closed = true;
        drop(state);
        self.close_workers()
    }
}
fn registry() -> &'static Mutex<Option<Arc<Pool>>> {
    static REGISTRY: OnceLock<Mutex<Option<Arc<Pool>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}
#[doc(hidden)]
pub struct RuntimeLease {
    _lease: Lease,
}
#[doc(hidden)]
pub fn lease_if_initialized() -> Option<RuntimeLease> {
    let pool = registry().lock().unwrap().clone()?;
    let mut state = pool.active.lock().unwrap();
    if state.closed {
        return None;
    }
    state.count += 1;
    drop(state);
    Some(RuntimeLease {
        _lease: Lease(pool),
    })
}
pub fn spawn(policy: Policy, future: impl Future<Output = ()> + Send + 'static) -> io::Result<()> {
    let pool = {
        let mut slot = registry().lock().unwrap();
        if let Some(pool) = slot.as_ref() {
            if pool.policy != policy {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "one executor policy per process",
                ));
            }
            pool.clone()
        } else {
            // Read immutable startup settings once, outside actor hot paths.
            let number = |key: &str, default: &str| -> io::Result<usize> {
                std::env::var(key)
                    .unwrap_or_else(|_| default.into())
                    .parse()
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("{key} must be an integer"),
                        )
                    })
            };
            let count = number("SPAWNED_EXPERIMENT_WORKERS", "2")?;
            let reactors = number("SPAWNED_EXPERIMENT_REACTORS", "1")?;
            let budget = number("SPAWNED_EXPERIMENT_LOCAL_BUDGET", "3")?;
            if !(1..=64).contains(&count)
                || !(1..=8).contains(&reactors)
                || !(1..=64).contains(&budget)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "experimental executor setting outside supported range",
                ));
            }
            let pool = Pool::new(policy, count, reactors, budget)?;
            *slot = Some(pool.clone());
            pool
        }
    };
    pool.spawn(future)
}
pub fn shutdown() -> io::Result<()> {
    let pool = registry().lock().unwrap().clone();
    if let Some(pool) = pool {
        pool.shutdown()?;
    }
    Ok(())
}

#[cfg(all(test, not(spawned_loom)))]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, mpsc as std_mpsc};
    const LIMIT: Duration = Duration::from_secs(5);
    struct WakeLoop {
        left: usize,
        active: Arc<AtomicUsize>,
        done: Option<std_mpsc::Sender<()>>,
    }
    impl Future for WakeLoop {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            assert_eq!(
                self.active.fetch_add(1, Ordering::SeqCst),
                0,
                "concurrent future poll"
            );
            let ready = self.left == 0;
            if !ready {
                self.left -= 1;
                cx.waker().wake_by_ref();
                cx.waker().wake_by_ref();
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if ready {
                self.done.take().unwrap().send(()).unwrap();
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }
    #[test]
    fn duplicate_self_wakes_progress_without_concurrent_polls() {
        for policy in [
            Policy::Global,
            Policy::Local,
            Policy::Steal,
            Policy::Root,
            Policy::TokioPool,
        ] {
            let pool = Pool::new(policy, 2, 1, 3).unwrap();
            let (tx, rx) = std_mpsc::channel();
            for _ in 0..32 {
                pool.spawn(WakeLoop {
                    left: 1000,
                    active: Arc::new(AtomicUsize::new(0)),
                    done: Some(tx.clone()),
                })
                .unwrap();
            }
            for _ in 0..32 {
                rx.recv_timeout(LIMIT).unwrap();
            }
            pool.shutdown().unwrap();
            assert!(pool.spawn(async {}).is_err());
        }
    }
    #[test]
    fn shared_reactor_timers_tcp_and_detached_helpers_survive_zero_actors() {
        use std::io::{Read, Write};
        for policy in [
            Policy::Global,
            Policy::Local,
            Policy::Steal,
            Policy::Root,
            Policy::TokioPool,
        ] {
            let pool = Pool::new(policy, 2, 1, 3).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(LIMIT)).unwrap();
                let mut data = [0; 8];
                socket.read_exact(&mut data).unwrap();
                socket.write_all(&data).unwrap();
            });
            let (release, gate) = oneshot::channel();
            let (done, rx) = std_mpsc::channel();
            let (helper, helper_rx) = std_mpsc::channel();
            pool.spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                tokio::time::sleep(Duration::from_millis(2)).await;
                let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
                socket.write_all(&42u64.to_le_bytes()).await.unwrap();
                let mut data = [0; 8];
                socket.read_exact(&mut data).await.unwrap();
                assert_eq!(u64::from_le_bytes(data), 42);
                tokio::spawn(async move {
                    gate.await.unwrap();
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    helper.send(42).unwrap();
                });
                done.send(()).unwrap();
            })
            .unwrap();
            rx.recv_timeout(LIMIT).unwrap();
            release.send(()).unwrap();
            assert_eq!(helper_rx.recv_timeout(LIMIT).unwrap(), 42);
            server.join().unwrap();
            pool.shutdown().unwrap();
        }
    }
    #[test]
    fn reactor_helpers_progress_while_native_actor_workers_block() {
        for policy in [Policy::Global, Policy::Local, Policy::Steal, Policy::Root] {
            let pool = Pool::new(policy, 1, 1, 3).unwrap();
            let (tx, rx) = std_mpsc::channel();
            let (entered, enter_rx) = std_mpsc::channel();
            let (release, gate) = std_mpsc::channel();
            pool.spawn(async move {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    tx.send(()).unwrap();
                });
                entered.send(()).unwrap();
                gate.recv_timeout(LIMIT).unwrap();
            })
            .unwrap();
            enter_rx.recv_timeout(LIMIT).unwrap();
            rx.recv_timeout(LIMIT).unwrap();
            release.send(()).unwrap();
            pool.shutdown().unwrap();
        }
    }
    #[test]
    fn worker_self_shutdown_rejected_and_panic_does_not_kill_other_jobs() {
        for policy in [
            Policy::Global,
            Policy::Local,
            Policy::Steal,
            Policy::Root,
            Policy::TokioPool,
        ] {
            let pool = Pool::new(policy, 2, 1, 3).unwrap();
            let other = pool.clone();
            let (tx, rx) = std_mpsc::channel();
            pool.spawn(async move {
                assert!(other.shutdown().is_err());
                tx.send(()).unwrap();
            })
            .unwrap();
            rx.recv_timeout(LIMIT).unwrap();
            pool.spawn(async { panic!("injected executor job panic") })
                .unwrap();
            let (tx, rx) = std_mpsc::channel();
            pool.spawn(async move {
                tx.send(42).unwrap();
            })
            .unwrap();
            assert_eq!(rx.recv_timeout(LIMIT).unwrap(), 42);
            pool.shutdown().unwrap();
        }
    }
    #[test]
    fn retired_blocking_helpers_do_not_accumulate_thread_ids() {
        for policy in [
            Policy::Global,
            Policy::Local,
            Policy::Steal,
            Policy::Root,
            Policy::TokioPool,
        ] {
            let pool = Pool::new(policy, 2, 1, 3).unwrap();
            let steady = if policy == Policy::TokioPool { 3 } else { 4 };
            let wait_for = |expected| {
                let deadline = Instant::now() + LIMIT;
                while pool.ids.lock().unwrap().len() != expected {
                    assert!(
                        Instant::now() < deadline,
                        "runtime thread IDs did not settle"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            };
            wait_for(steady);
            let gate = Arc::new(std::sync::Barrier::new(33));
            let handles: Vec<_> = (0..32)
                .map(|_| {
                    let gate = gate.clone();
                    pool.handle.spawn_blocking(move || {
                        gate.wait();
                    })
                })
                .collect();
            gate.wait();
            pool.handle.block_on(async {
                for handle in handles {
                    handle.await.unwrap();
                }
            });
            wait_for(steady);
            pool.shutdown().unwrap();
            assert!(pool.ids.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn concurrent_shutdown_of_idle_workers_is_idempotent() {
        for policy in [
            Policy::Global,
            Policy::Local,
            Policy::Steal,
            Policy::TokioPool,
        ] {
            for _ in 0..16 {
                let pool = Pool::new(policy, 2, 1, 3).unwrap();
                let gate = Arc::new(std::sync::Barrier::new(5));
                let (done, rx) = std_mpsc::channel();
                let mut closers = Vec::new();
                pool.ready.notify_all();
                for _ in 0..4 {
                    let (p, g, d) = (pool.clone(), gate.clone(), done.clone());
                    closers.push(thread::spawn(move || {
                        g.wait();
                        d.send(p.shutdown()).unwrap();
                    }));
                }
                gate.wait();
                for _ in 0..4 {
                    rx.recv_timeout(LIMIT)
                        .expect("idle shutdown stalled")
                        .unwrap();
                }
                for closer in closers {
                    closer.join().unwrap();
                }
                assert!(pool.ids.lock().unwrap().is_empty());
            }
        }
    }
    #[test]
    fn completed_job_ignores_retained_waker() {
        for policy in [Policy::Global, Policy::Local, Policy::Steal, Policy::Root] {
            let pool = Pool::new(policy, 2, 1, 3).unwrap();
            let (tx, rx) = std_mpsc::channel();
            pool.spawn(std::future::poll_fn(move |cx| {
                tx.send(cx.waker().clone()).unwrap();
                Poll::Ready(())
            }))
            .unwrap();
            let waker = rx.recv_timeout(LIMIT).unwrap();
            pool.shutdown().unwrap();
            waker.wake_by_ref();
            waker.wake();
        }
    }
    #[test]
    fn cooperative_receive_loop_yields_with_single_native_worker() {
        for policy in [Policy::Global, Policy::Local, Policy::Steal, Policy::Root] {
            let pool = Pool::new(policy, 1, 1, 3).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_job = stop.clone();
            let (tx, mut rx) = mpsc::unbounded_channel();
            tx.send(()).unwrap();
            pool.spawn(async move {
                while !stop_job.load(Ordering::Relaxed) {
                    rx.recv().await.unwrap();
                    tx.send(()).unwrap();
                }
            })
            .unwrap();
            let (done, rx) = std_mpsc::channel();
            pool.spawn(async move {
                done.send(()).unwrap();
            })
            .unwrap();
            let result = rx.recv_timeout(LIMIT);
            stop.store(true, Ordering::Relaxed);
            result.unwrap();
            pool.shutdown().unwrap();
        }
    }
}
