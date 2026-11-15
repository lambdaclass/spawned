//! Tasks-mode blocking jobs: where their native body runs is reserved with
//! the native slot (so a failure is an error, never a lost job), and a
//! managed actor's job keeps its scope owned until the body ends.
#![cfg(all(feature = "async", not(spawned_sim)))] // native jobs are not simulated
use spawned_concurrency::{
    message::Message,
    system::ActorSystem,
    tasks::{Actor, Context, Handler, JobOptions, JobOutcome, PendingReply},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;
#[cfg(target_os = "linux")]
#[path = "support/drop_subprocess.rs"]
mod subprocess;

struct Done;
impl Message for Done {
    type Result = ();
}

/// Starts one blocking job in `started()` that ignores cancellation and
/// runs until the test releases it.
struct Gated {
    gate: Mutex<Option<mpsc::Receiver<()>>>,
    running: Arc<AtomicBool>,
}
impl Actor for Gated {
    async fn started(&mut self, ctx: &Context<Self>) {
        let gate = self.gate.lock().unwrap().take().unwrap();
        let running = self.running.clone();
        ctx.pipe_blocking(
            JobOptions::new(),
            move |_| {
                running.store(true, Ordering::SeqCst);
                let _ = gate.recv();
                running.store(false, Ordering::SeqCst);
            },
            |_: JobOutcome<()>| Done,
        )
        .unwrap();
    }
}
impl Handler<Done> for Gated {
    async fn handle(&mut self, _: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {}
}

/// Review P1 (Codex 7): a managed actor's blocking job holds a scope lease
/// until its body ends, so a system shutdown is not complete (and a
/// supervisor does not see the cleanup done) while the native work still
/// runs. The job used to run on the blocking pool without a lease: once the
/// actor's bounded job join expired, shutdown reported completion with the
/// job still executing, on an owned group and on a borrowed runtime alike.
async fn shutdown_waits_for_the_blocking_job(borrowed: bool) {
    let system = ActorSystem::new().unwrap();
    let group = if borrowed {
        system
            .async_group_on("borrowed-jobs", &tokio::runtime::Handle::current())
            .unwrap()
    } else {
        system.async_group("owned-jobs", 1).unwrap()
    };
    let (release, gate) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(false));
    let actor = group
        .start(Gated {
            gate: Mutex::new(Some(gate)),
            running: running.clone(),
        })
        .unwrap();
    actor.ready().await.unwrap();
    for _ in 0..500 {
        if running.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(running.load(Ordering::SeqCst), "the job never started");
    let report = system.shutdown(Duration::from_millis(300)).await;
    assert!(running.load(Ordering::SeqCst));
    assert!(
        !report.complete,
        "shutdown completed while a managed blocking job still ran"
    );
    release.send(()).unwrap();
    let report = report.wait(Duration::from_secs(5)).await;
    assert!(
        report.complete,
        "shutdown did not complete after the job ended"
    );
    assert!(!running.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_a_managed_blocking_job() {
    shutdown_waits_for_the_blocking_job(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_on_a_borrowed_runtime_waits_for_a_managed_blocking_job() {
    shutdown_waits_for_the_blocking_job(true).await;
}

/// Review P1 (Codex 7): with a `shutdown_timeout` above the scope's default
/// job-join limit (5 s), the actor's root released its lease after that
/// limit while its blocking job still ran; the supervisor saw the cleanup
/// complete and started a replacement next to the old job's effects. The
/// job's lease keeps the old scope owned: no replacement until it ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supervisor_does_not_replace_a_child_whose_blocking_job_still_runs() {
    use spawned_concurrency::supervision::{ChildSpec, SupervisorConfig};
    use std::sync::atomic::AtomicUsize;
    /// The first instance starts a gated job and exits (`Normal`; the
    /// child is `Permanent`).
    struct Child(Option<Gated>);
    impl Actor for Child {
        async fn started(&mut self, ctx: &Context<Self>) {
            if let Some(gated) = &mut self.0 {
                let gate = gated.gate.lock().unwrap().take().unwrap();
                let running = gated.running.clone();
                ctx.pipe_blocking(
                    JobOptions::new(),
                    move |_| {
                        running.store(true, Ordering::SeqCst);
                        let _ = gate.recv();
                        running.store(false, Ordering::SeqCst);
                    },
                    |_: JobOutcome<()>| Done,
                )
                .unwrap();
                // Exit after readiness (a startup exit is a startup failure).
                ctx.send(Leave).unwrap();
            }
        }
    }
    struct Leave;
    impl Message for Leave {
        type Result = ();
    }
    impl Handler<Leave> for Child {
        async fn handle(&mut self, _: Leave, _: PendingReply<'_, ()>, ctx: &Context<Self>) {
            ctx.stop();
        }
    }
    impl Handler<Done> for Child {
        async fn handle(&mut self, _: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {}
    }
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("supervised-jobs", 2).unwrap();
    let (release, gate) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(false));
    let starts = Arc::new(AtomicUsize::new(0));
    let first = Arc::new(Mutex::new(Some(Gated {
        gate: Mutex::new(Some(gate)),
        running: running.clone(),
    })));
    let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
    let factory_starts = starts.clone();
    builder
        .add(
            ChildSpec::new("gated", &group, move |_| {
                factory_starts.fetch_add(1, Ordering::SeqCst);
                let gated = first.lock().unwrap().take();
                async move { Ok::<_, String>(Child(gated)) }
            })
            .shutdown_timeout(Duration::from_secs(10)),
        )
        .unwrap();
    let _supervisor = builder.start().await.unwrap();
    // Past the scope's 5 s job-join limit, with the job still running.
    tokio::time::sleep(Duration::from_millis(5600)).await;
    assert!(running.load(Ordering::SeqCst), "the job ended early");
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "a replacement started while the old blocking job still ran"
    );
    release.send(()).unwrap();
    for _ in 0..1000 {
        if starts.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "no replacement after the job ended"
    );
    let _ = system.shutdown(Duration::from_secs(5)).await;
}

/// Review P1 (Codex 6): job admission is one step with the owner's exit.
/// `pipe` builds the work (user code) between its exit check and
/// the admission; an owner that exits meanwhile refuses the job, so it
/// never runs. It used to be admitted into the job set the exit had already
/// drained: uncancelled work outliving its owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_admitted_after_its_owner_exited_never_runs() {
    use spawned_concurrency::tasks::ActorStart;
    struct Owner;
    impl Actor for Owner {}
    impl Handler<Done> for Owner {
        async fn handle(&mut self, _: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {}
    }
    let actor = Owner.start();
    let ctx = actor.context();
    let owner = actor.clone();
    let ran = Arc::new(AtomicBool::new(false));
    let work_ran = ran.clone();
    let handle = tokio::task::spawn_blocking(move || {
        ctx.pipe(
            JobOptions::new(),
            |job| {
                // While the work is built, the owner exits and dies.
                owner.context().stop();
                while owner.exit_reason().is_none() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                async move {
                    work_ran.store(true, Ordering::SeqCst);
                    job.cancelled().await;
                }
            },
            |_: JobOutcome<()>| Done,
        )
        .unwrap()
    })
    .await
    .unwrap();
    assert!(
        handle.is_cancelled(),
        "a job admitted after its owner's exit escaped the exit's cancellation"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!ran.load(Ordering::SeqCst), "a refused job ran");
    actor.join().await;
}

/// Review round 2 (Codex 4): an unused blocking permit held outside the
/// actor does not keep a dead generation's scope
/// owned. Its scope lease used to be taken at reservation, so the stopped
/// actor's scope could not join, and a system shutdown stayed incomplete,
/// until the external permit was dropped. The lease is now taken when the
/// job is admitted; an executing job still holds it until its body ends.
async fn an_unused_permit_does_not_hold_the_scope(borrowed: bool) {
    use spawned_concurrency::tasks::{ActorStart, BlockingJobPermit};
    struct Owner;
    impl Actor for Owner {}
    impl Handler<Done> for Owner {
        async fn handle(&mut self, _: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {}
    }
    let system = ActorSystem::new().unwrap();
    let group = if borrowed {
        system
            .async_group_on("unused-permit-borrowed", &tokio::runtime::Handle::current())
            .unwrap()
    } else {
        system.async_group("unused-permit-owned", 1).unwrap()
    };
    let actor = Owner.start_on(&group).unwrap();
    actor.ready().await.unwrap();
    // Reserved through a cloned context and kept outside the actor, unused.
    let permit: BlockingJobPermit<Owner> = actor
        .context()
        .reserve_blocking_job(JobOptions::new())
        .unwrap();
    actor.context().stop();
    assert_eq!(
        actor.wait_exit().await,
        spawned_concurrency::ExitReason::Normal
    );
    let report = system.shutdown(Duration::from_secs(2)).await;
    assert!(
        report.complete,
        "an unused blocking permit kept the dead actor's scope owned"
    );
    // Using it now is refused: nothing runs.
    let handle = permit.pipe_blocking(|_| (), |_| Done);
    assert!(handle.is_cancelled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unused_permit_does_not_hold_the_scope_owned_runtime() {
    an_unused_permit_does_not_hold_the_scope(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unused_permit_does_not_hold_the_scope_borrowed_runtime() {
    an_unused_permit_does_not_hold_the_scope(true).await;
}

/// Review P1 (Codex 12, agy 6): outside a Tokio runtime a tasks actor's
/// blocking job runs on an owned thread. Its creation is part of the
/// reservation, so a failure is `SpawnFailed` (and `pipe_reply*` hands the
/// token back), as in threads mode. The thread used to be spawned after the
/// permit was granted and the token consumed: a failure was only logged,
/// the caller got a `JobHandle` and `done` never ran.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_job_thread_spawn_is_a_reservation_error() {
    subprocess::run(
        "job_thread_spawn_failure_child",
        "job thread spawn failure reported",
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "subprocess helper exercised by the enabled parent"]
fn job_thread_spawn_failure_child() {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};
    use spawned_concurrency::tasks::{ActorStart, Backend, PipeError};
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    // Wait until the actor's runtime and its worker thread exist before
    // refusing new threads. Otherwise the limit can kill actor startup and
    // turn the intended SpawnFailed result into OwnerStopping.
    struct ReadyOwner(mpsc::Sender<()>);
    impl Actor for ReadyOwner {
        async fn started(&mut self, _: &Context<Self>) {
            self.0.send(()).unwrap();
        }
    }
    impl Handler<Done> for ReadyOwner {
        async fn handle(&mut self, _: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {}
    }
    let (started, ready) = mpsc::channel();
    // The actor runs on its own thread; this thread has no Tokio runtime.
    let actor = ReadyOwner(started).start_with_backend(Backend::Thread);
    ready
        .recv_timeout(Duration::from_secs(30))
        .expect("actor runtime did not finish starting before thread fault injection");
    let ctx = actor.context();
    assert!(tokio::runtime::Handle::try_current().is_err());
    let (soft, hard) = getrlimit(Resource::RLIMIT_NPROC).unwrap();
    // No new threads for this user: the job thread cannot be created.
    setrlimit(Resource::RLIMIT_NPROC, 0, hard).unwrap();
    if let Ok(probe) = std::thread::Builder::new().spawn(|| ()) {
        // Not enforced here (a privileged user): nothing to observe.
        let _ = probe.join();
        setrlimit(Resource::RLIMIT_NPROC, soft, hard).unwrap();
        ctx.stop();
        println!("job thread spawn failure reported (RLIMIT_NPROC not enforced)");
        return;
    }
    let reserved = ctx.reserve_blocking_job(JobOptions::new());
    let piped = ctx.pipe_blocking(JobOptions::new(), |_| (), |_| Done);
    setrlimit(Resource::RLIMIT_NPROC, soft, hard).unwrap();
    match reserved {
        Err(error) => assert!(matches!(error, PipeError::SpawnFailed(_)), "{error}"),
        Ok(_) => panic!("the reservation succeeded without a job thread"),
    }
    assert!(
        matches!(piped, Err(ref e) if matches!(e, PipeError::SpawnFailed(_))),
        "pipe_blocking returned {piped:?}"
    );
    assert_eq!(
        ctx.deferred_outstanding(),
        0,
        "a refused job kept its credit"
    );
    // With threads available again, the same call runs the job.
    let permit = ctx
        .reserve_blocking_job(JobOptions::new())
        .expect("a job thread");
    drop(permit);
    ctx.stop();
    std::thread::sleep(Duration::from_millis(50));
    println!("job thread spawn failure reported");
}
