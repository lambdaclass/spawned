//! Pipe-to-self jobs (docs/design/deferred-replies.md §9): results as
//! messages, panics, keyed replacement and fencing, cancellation, job
//! deadlines, emits, delegation with `pipe_reply`, bounds and owner exit.
use spawned_concurrency::message::Message;
use std::sync::{Arc, Mutex};

/// What the owner's handlers observed, in dispatch order.
#[derive(Default)]
pub struct Log(pub Mutex<Vec<String>>);
impl Log {
    pub fn push(&self, entry: impl Into<String>) {
        self.0.lock().unwrap().push(entry.into());
    }
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

#[cfg(feature = "async")]
mod tasks_mode {
    use super::*;
    use spawned_concurrency::tasks::{
        Actor, ActorRef, ActorStart, Context, Deadline, Handler, JobError, JobGone, JobOptions,
        JobOutcome, PendingReply, PipeErrorKind, PipeReplyError, ReplyTo,
    };
    use spawned_concurrency::ActorError;
    use spawned_rt::tasks::{oneshot, sleep};
    use std::time::Duration;

    struct Owner {
        log: Arc<Log>,
        gates: Vec<oneshot::Sender<()>>,
    }
    impl Actor for Owner {
        const MAX_OUTSTANDING: usize = 4;
        const MAX_NATIVE_JOBS: Option<usize> = Some(1);
        async fn stopped(&mut self, _: &Context<Self>) {
            self.log.push("stopped");
        }
    }

    struct Done(String);
    impl Message for Done {
        type Result = ();
    }
    impl Handler<Done> for Owner {
        async fn handle(&mut self, msg: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {
            self.log.push(msg.0);
        }
    }

    struct Open;
    impl Message for Open {
        type Result = ();
    }
    impl Handler<Open> for Owner {
        async fn handle(&mut self, _: Open, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            for gate in self.gates.drain(..) {
                let _ = gate.send(());
            }
            reply.reply(());
        }
    }

    struct Outstanding;
    impl Message for Outstanding {
        type Result = usize;
    }
    impl Handler<Outstanding> for Owner {
        async fn handle(
            &mut self,
            _: Outstanding,
            reply: PendingReply<'_, usize>,
            ctx: &Context<Self>,
        ) {
            reply.reply(ctx.deferred_outstanding());
        }
    }

    fn describe<T: std::fmt::Debug>(name: &str, outcome: JobOutcome<T>) -> Done {
        Done(match outcome {
            JobOutcome::Ok(v) => format!("{name}:ok:{v:?}"),
            JobOutcome::Err(JobError::Panicked(m)) => format!("{name}:panicked:{m}"),
            JobOutcome::Err(JobError::DeadlineExceeded) => format!("{name}:deadline"),
        })
    }

    /// Start jobs from inside a turn, as handlers do.
    #[cfg_attr(spawned_sim, allow(dead_code))] // native jobs are not simulated
    enum Start {
        Pipe(&'static str, u64),
        Panics,
        Gated(&'static str, Option<Duration>),
        Keyed(&'static str),
        CancelAfterSpawn,
        Blocking(u32),
        NativeBudget,
        Emits(&'static str, u32),
    }
    impl Message for Start {
        type Result = Result<(), String>;
    }
    impl Handler<Start> for Owner {
        async fn handle(
            &mut self,
            msg: Start,
            reply: PendingReply<'_, Result<(), String>>,
            ctx: &Context<Self>,
        ) {
            let result = match msg {
                Start::Pipe(name, v) => ctx
                    .pipe(async move { v * 2 }, move |o| describe(name, o))
                    .map(drop)
                    .map_err(|e| e.to_string()),
                Start::Panics => ctx
                    .pipe(async { panic!("job boom") }, |o: JobOutcome<()>| {
                        describe("p", o)
                    })
                    .map(drop)
                    .map_err(|e| e.to_string()),
                Start::Gated(name, deadline) => {
                    let (open, gate) = oneshot::channel::<()>();
                    self.gates.push(open);
                    let opts = JobOptions {
                        deadline: deadline.map(|d| spawned_rt::time::Instant::now() + d),
                        ..JobOptions::default()
                    };
                    ctx.pipe_with(opts, async move { gate.await.is_ok() }, move |o| {
                        describe(name, o)
                    })
                    .map(drop)
                    .map_err(|e| e.to_string())
                }
                Start::Keyed(name) => {
                    let (open, gate) = oneshot::channel::<()>();
                    self.gates.push(open);
                    ctx.pipe_keyed("k", async move { gate.await.is_ok() }, move |o| {
                        describe(name, o)
                    })
                    .map(drop)
                    .map_err(|e| e.to_string())
                }
                Start::CancelAfterSpawn => {
                    let (open, gate) = oneshot::channel::<()>();
                    self.gates.push(open);
                    ctx.pipe(async move { gate.await.is_ok() }, |o| {
                        describe("cancelled", o)
                    })
                    .map(|job| job.cancel())
                    .map_err(|e| e.to_string())
                }
                Start::Blocking(v) => ctx
                    .pipe_blocking_with(
                        JobOptions::default(),
                        move |_| v + 1,
                        |o| describe("blocking", o),
                    )
                    .map(drop)
                    .map_err(|e| e.to_string()),
                Start::NativeBudget => {
                    let (hold, held) = std::sync::mpsc::channel::<()>();
                    let first = ctx.pipe_blocking_with(
                        JobOptions::default(),
                        move |_| {
                            let _ = held.recv();
                        },
                        |o| describe("first", o),
                    );
                    let second = ctx.pipe_blocking_with(
                        JobOptions::default(),
                        |_| (),
                        |o| describe("second", o),
                    );
                    drop(hold);
                    match (&first, &second) {
                        (Ok(_), Err(e)) if matches!(e.kind, PipeErrorKind::NativeBudget) => Ok(()),
                        _ => Err(format!("{first:?} / {second:?}")),
                    }
                }
                Start::Emits(name, n) => ctx
                    .pipe_blocking_with(
                        JobOptions {
                            key: Some("emits".into()),
                            emit_credits: 2,
                            ..JobOptions::default()
                        },
                        move |job| {
                            for i in 0..n {
                                job.emit_blocking(Done(format!("{name}:emit:{i}")))?;
                            }
                            Ok::<u32, JobGone>(n)
                        },
                        move |o| describe(name, o),
                    )
                    .map(drop)
                    .map_err(|e| e.to_string()),
            };
            reply.reply(result);
        }
    }

    fn start() -> (ActorRef<Owner>, Arc<Log>) {
        let log = Arc::new(Log::default());
        let actor = Owner {
            log: log.clone(),
            gates: Vec::new(),
        }
        .start();
        (actor, log)
    }

    /// Wait until the log has `n` entries (jobs report asynchronously).
    async fn entries(actor: &ActorRef<Owner>, log: &Log, n: usize) -> Vec<String> {
        for _ in 0..500 {
            actor.request(Outstanding).await.unwrap();
            if log.0.lock().unwrap().len() >= n {
                return log.take();
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("log never reached {n} entries: {:?}", log.take());
    }

    async fn stop(actor: ActorRef<Owner>) {
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn job_results_and_panics_arrive_as_one_message() {
        let (actor, log) = start();
        actor.request(Start::Pipe("a", 21)).await.unwrap().unwrap();
        assert_eq!(entries(&actor, &log, 1).await, ["a:ok:42"]);
        actor.request(Start::Panics).await.unwrap().unwrap();
        assert_eq!(entries(&actor, &log, 1).await, ["p:panicked:job boom"]);
        assert_eq!(actor.request(Outstanding).await.unwrap(), 0);
        stop(actor).await;
    }

    #[spawned_macros::sim_test]
    async fn keyed_replacement_and_cancel_fence_results() {
        let (actor, log) = start();
        actor.request(Start::Keyed("old")).await.unwrap().unwrap();
        actor.request(Start::Keyed("new")).await.unwrap().unwrap();
        actor
            .request(Start::CancelAfterSpawn)
            .await
            .unwrap()
            .unwrap();
        actor.request(Open).await.unwrap();
        // Only the replacing job reports; the replaced and cancelled ones are
        // fenced even though their work finished.
        assert_eq!(entries(&actor, &log, 1).await, ["new:ok:true"]);
        sleep(Duration::from_millis(20)).await;
        assert!(log.take().is_empty());
        assert_eq!(actor.request(Outstanding).await.unwrap(), 0);
        stop(actor).await;
    }

    /// Starts keyed job `old`, publishing its handle.
    type OldJob = Arc<Mutex<Option<(spawned_concurrency::tasks::JobHandle, oneshot::Sender<()>)>>>;
    struct StartOld(OldJob);
    impl Message for StartOld {
        type Result = ();
    }
    impl Handler<StartOld> for Owner {
        async fn handle(&mut self, msg: StartOld, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
            let (open, gate) = oneshot::channel::<()>();
            let job = ctx
                .pipe_keyed("r", async move { gate.await.map(|()| 1).unwrap_or(0) }, |o| {
                    describe("old", o)
                })
                .unwrap();
            *msg.0.lock().unwrap() = Some((job, open));
            reply.reply(());
        }
    }
    /// Busy until `go`, then replaces the key.
    struct ReplaceWhen(Arc<std::sync::atomic::AtomicBool>);
    impl Message for ReplaceWhen {
        type Result = ();
    }
    impl Handler<ReplaceWhen> for Owner {
        async fn handle(&mut self, msg: ReplaceWhen, _: PendingReply<'_, ()>, ctx: &Context<Self>) {
            while !msg.0.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            ctx.pipe_keyed("r", async { 2 }, |o| describe("new", o)).unwrap();
        }
    }

    /// Review P1 (`NoStaleKeyed`): the old keyed job finishes and queues its
    /// terminal message while the owner is busy; the replacement made in that
    /// busy turn fences it.
    #[cfg(not(spawned_sim))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replacement_fences_a_finished_keyed_job() {
        let (actor, log) = start();
        let handle = Arc::new(Mutex::new(None));
        actor.request(StartOld(handle.clone())).await.unwrap();
        let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
        actor.send(ReplaceWhen(go.clone())).unwrap();
        // `ReplaceWhen` is queued before the old job can finish.
        let (old, open) = handle.lock().unwrap().take().unwrap();
        open.send(()).unwrap();
        while old.is_running() {
            sleep(Duration::from_millis(1)).await;
        }
        go.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(entries(&actor, &log, 1).await, ["new:ok:2"]);
        sleep(Duration::from_millis(20)).await;
        assert!(log.take().is_empty(), "the replaced job's terminal message was dispatched");
        stop(actor).await;
    }

    struct Bomb;
    impl Message for Bomb {
        type Result = ();
    }
    impl Handler<Bomb> for Owner {
        async fn handle(&mut self, _: Bomb, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
            ctx.pipe(async {}, |_: JobOutcome<()>| -> Done { panic!("done boom") })
                .unwrap();
            reply.reply(());
        }
    }

    /// Review P1: a panicking `done` is a handler panic: `stopped()` runs
    /// and the owner's other jobs are cancelled.
    #[spawned_macros::sim_test]
    async fn a_panicking_done_runs_stopped_and_cancels_jobs() {
        let (actor, log) = start();
        actor.request(Start::Gated("survivor", None)).await.unwrap().unwrap();
        actor.request(Bomb).await.unwrap();
        let reason = actor.wait_exit().await;
        assert!(matches!(reason, spawned_concurrency::ExitReason::Panic(_)), "{reason:?}");
        assert!(log.take().contains(&"stopped".to_string()), "stopped() did not run");
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn job_deadline_notifies_while_the_work_keeps_its_credit() {
        let (actor, log) = start();
        actor
            .request(Start::Gated("slow", Some(Duration::from_millis(20))))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entries(&actor, &log, 1).await, ["slow:deadline"]);
        assert_eq!(
            actor.request(Outstanding).await.unwrap(),
            1,
            "the hung work still holds its credit"
        );
        actor.request(Open).await.unwrap();
        for _ in 0..200 {
            if actor.request(Outstanding).await.unwrap() == 0 {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(actor.request(Outstanding).await.unwrap(), 0);
        assert!(log.take().is_empty(), "the late result is dropped");
        stop(actor).await;
    }

    #[spawned_macros::sim_test]
    async fn pipe_rejects_at_the_credit_bound() {
        let (actor, _) = start();
        for _ in 0..4 {
            actor
                .request(Start::Gated("g", None))
                .await
                .unwrap()
                .unwrap();
        }
        let refused = actor.request(Start::Gated("g", None)).await.unwrap();
        assert!(refused.unwrap_err().contains("deferred credits"));
        actor.request(Open).await.unwrap();
        stop(actor).await;
    }

    #[cfg(not(spawned_sim))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_jobs_emit_in_order_and_respect_the_native_budget() {
        let (actor, log) = start();
        actor.request(Start::Blocking(1)).await.unwrap().unwrap();
        assert_eq!(entries(&actor, &log, 1).await, ["blocking:ok:2"]);
        actor.request(Start::NativeBudget).await.unwrap().unwrap();
        assert_eq!(entries(&actor, &log, 1).await, ["first:ok:()"]);
        actor.request(Start::Emits("e", 5)).await.unwrap().unwrap();
        // Emits are FIFO and the terminal message is last.
        assert_eq!(
            entries(&actor, &log, 6).await,
            [
                "e:emit:0",
                "e:emit:1",
                "e:emit:2",
                "e:emit:3",
                "e:emit:4",
                "e:ok:Ok(5)"
            ]
        );
        stop(actor).await;
    }

    // -- Delegation -----------------------------------------------------------

    struct Backend;
    impl Actor for Backend {}
    struct Lookup(u64);
    impl Message for Lookup {
        type Result = u64;
    }
    impl Handler<Lookup> for Backend {
        async fn handle(&mut self, msg: Lookup, reply: PendingReply<'_, u64>, _: &Context<Self>) {
            sleep(Duration::from_millis(msg.0)).await;
            reply.reply(msg.0 * 10);
        }
    }

    struct Router {
        backend: ActorRef<Backend>,
    }
    impl Actor for Router {}
    struct Route(u64);
    impl Message for Route {
        type Result = u64;
    }
    struct Delegated {
        res: JobOutcome<Result<u64, ActorError>>,
        token: ReplyTo<u64>,
    }
    impl Message for Delegated {
        type Result = ();
    }
    struct Ping;
    impl Message for Ping {
        type Result = &'static str;
    }
    struct Foreign(ReplyTo<u64>);
    impl Message for Foreign {
        type Result = bool;
    }
    impl Handler<Route> for Router {
        async fn handle(&mut self, msg: Route, reply: PendingReply<'_, u64>, ctx: &Context<Self>) {
            let Ok(token) = reply.defer() else { return };
            let downstream = self.backend.request_raw_until(
                Lookup(msg.0),
                token.deadline().map_or(Deadline::Never, Deadline::At),
            );
            let work = async move {
                match downstream {
                    Ok(rx) => rx.await,
                    Err(e) => Err(e),
                }
            };
            if let Err(e) = ctx.pipe_reply(token, work, |res, token| Delegated { res, token }) {
                drop(e.into_token());
            }
        }
    }
    impl Handler<Delegated> for Router {
        async fn handle(&mut self, msg: Delegated, _: PendingReply<'_, ()>, _: &Context<Self>) {
            match msg.res {
                JobOutcome::Ok(Ok(v)) => {
                    msg.token.reply(v);
                }
                _ => msg.token.overloaded(),
            }
        }
    }
    impl Handler<Ping> for Router {
        async fn handle(
            &mut self,
            _: Ping,
            reply: PendingReply<'_, &'static str>,
            _: &Context<Self>,
        ) {
            reply.reply("pong");
        }
    }
    impl Handler<Foreign> for Router {
        async fn handle(
            &mut self,
            msg: Foreign,
            reply: PendingReply<'_, bool>,
            ctx: &Context<Self>,
        ) {
            let refused = ctx.pipe_reply(msg.0, async { 1u64 }, |_, token| Delegated {
                res: JobOutcome::Ok(Ok(0)),
                token,
            });
            let wrong_owner = matches!(refused, Err(PipeReplyError::WrongOwner(_)));
            if let Err(e) = refused {
                e.into_token().reply(7);
            }
            reply.reply(wrong_owner);
        }
    }

    /// Holds a deferred token so a test can hand it to another actor.
    struct Holder(Option<ReplyTo<u64>>);
    impl Actor for Holder {}
    struct Hold;
    impl Message for Hold {
        type Result = u64;
    }
    struct Give;
    impl Message for Give {
        type Result = Option<ReplyTo<u64>>;
    }
    impl Handler<Hold> for Holder {
        async fn handle(&mut self, _: Hold, reply: PendingReply<'_, u64>, _: &Context<Self>) {
            self.0 = reply.defer().ok();
        }
    }
    impl Handler<Give> for Holder {
        async fn handle(
            &mut self,
            _: Give,
            reply: PendingReply<'_, Option<ReplyTo<u64>>>,
            _: &Context<Self>,
        ) {
            let token = self.0.take();
            reply.reply(token);
        }
    }

    #[spawned_macros::sim_test]
    async fn router_delegates_with_pipe_reply_while_serving_its_mailbox() {
        let backend = Backend.start();
        let router = Router {
            backend: backend.clone(),
        }
        .start();
        let slow = router.request_raw(Route(200)).unwrap();
        // The router's mailbox is free while the slow request is delegated.
        assert_eq!(router.request(Ping).await.unwrap(), "pong");
        assert_eq!(router.request(Route(1)).await.unwrap(), 10);
        assert_eq!(slow.await.unwrap(), 2000);
        router.context().stop();
        router.join().await;
        backend.context().stop();
        backend.join().await;
    }

    #[spawned_macros::sim_test]
    async fn pipe_reply_refuses_a_token_of_another_actor() {
        let backend = Backend.start();
        let router = Router {
            backend: backend.clone(),
        }
        .start();
        let holder = Holder(None).start();
        let rx = holder.request_raw_until(Hold, Deadline::Never).unwrap();
        let token = holder.request(Give).await.unwrap().unwrap();
        assert!(router.request(Foreign(token)).await.unwrap());
        assert_eq!(
            rx.await.unwrap(),
            7,
            "the refused token came back and was answered"
        );
        for actor in [
            router.child_handle(),
            backend.child_handle(),
            holder.child_handle(),
        ] {
            actor.stop();
        }
        router.join().await;
    }
}

#[cfg(not(spawned_sim))]
mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{
        Actor, ActorRef, ActorStart, Context, Deadline, Handler, JobError, JobOptions, JobOutcome,
        PendingReply,
    };

    struct Owner {
        log: Arc<Log>,
    }
    impl Actor for Owner {}

    struct Done(String);
    impl Message for Done {
        type Result = ();
    }
    impl Handler<Done> for Owner {
        fn handle(&mut self, msg: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {
            self.log.push(msg.0);
        }
    }
    struct Compute(u64);
    impl Message for Compute {
        type Result = u64;
    }
    struct Computed {
        res: JobOutcome<u64>,
        token: spawned_concurrency::ReplyTo<u64>,
    }
    impl Message for Computed {
        type Result = ();
    }
    impl Handler<Compute> for Owner {
        fn handle(&mut self, msg: Compute, reply: PendingReply<'_, u64>, ctx: &Context<Self>) {
            let Ok(token) = reply.defer() else { return };
            let n = msg.0;
            let _ = ctx.pipe_reply_blocking_with(
                JobOptions::default(),
                token,
                move |job| {
                    job.emit_blocking(Done(format!("computing {n}"))).unwrap();
                    if n == 0 {
                        panic!("zero");
                    }
                    n * n
                },
                |res, token| Computed { res, token },
            );
        }
    }
    impl Handler<Computed> for Owner {
        fn handle(&mut self, msg: Computed, _: PendingReply<'_, ()>, _: &Context<Self>) {
            match msg.res {
                JobOutcome::Ok(v) => {
                    msg.token.reply(v);
                }
                JobOutcome::Err(JobError::Panicked(m)) => {
                    self.log.push(format!("panicked {m}"));
                    msg.token.overloaded();
                }
                JobOutcome::Err(JobError::DeadlineExceeded) => msg.token.overloaded(),
            }
        }
    }

    #[test]
    fn native_jobs_answer_deferred_requests() {
        let log = Arc::new(Log::default());
        let actor: ActorRef<Owner> = Owner { log: log.clone() }.start();
        let rx = actor
            .request_raw_until(Compute(7), Deadline::Never)
            .unwrap();
        assert_eq!(rx.recv().unwrap(), 49);
        assert!(matches!(
            actor.request(Compute(0)),
            Err(spawned_concurrency::ActorError::Overloaded)
        ));
        let entries = log.take();
        assert_eq!(entries[0], "computing 7");
        assert!(
            entries.contains(&"panicked zero".to_string()),
            "{entries:?}"
        );
        actor.context().stop();
        actor.join();
    }
}
