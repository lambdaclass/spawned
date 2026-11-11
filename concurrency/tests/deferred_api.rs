//! Deferred-reply API refinements from porting ethlambda and ethrex to 0.8:
//! bounds set in `#[actor]` and per `ChildSpec`, blocking jobs on Tokio's
//! blocking pool, reserve-then-spawn permits, fenced delayed emits
//! (`emit_at`), hard job deadlines, send-only manual handlers
//! (`#[send_handler]` on an impl) and `spawned_test::in_actor`.
use spawned_concurrency::message::Message;
use std::sync::{Arc, Mutex};

/// What the owner's handlers observed, in dispatch order.
#[derive(Default)]
pub struct Log(pub Mutex<Vec<String>>);
impl Log {
    pub fn push(&self, entry: impl Into<String>) {
        self.0.lock().unwrap().push(entry.into());
    }
    pub fn snapshot(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

pub struct Note(pub String);
impl Message for Note {
    type Result = ();
}

/// A send-only message whose handler returns early on one path.
pub struct Poke(pub bool);
impl Message for Poke {
    type Result = ();
}

#[cfg(feature = "async")]
mod tasks_mode {
    use super::*;
    use spawned_concurrency::supervision::{ChildSpec, SupervisorConfig};
    use spawned_concurrency::system::ActorSystem;
    use spawned_concurrency::tasks::{
        Actor, ActorRef, ActorStart, Context, Handler, Instant, JobError, JobGone, JobOptions,
        JobOutcome, PipeError,
    };
    use spawned_concurrency::{actor, send_handler};
    use std::time::Duration;

    /// `spawned_test::tasks::in_actor` (the dev-dependency is built without
    /// its Tokio half; `testing/tests/tasks_kit.rs` covers the helper).
    async fn in_actor<R, F>(actor: &ActorRef<Owner>, f: F) -> R
    where
        R: Send + 'static,
        F: FnOnce(&mut Owner, &Context<Owner>) -> R + Send + 'static,
    {
        actor.__in_turn(f).unwrap().await.unwrap()
    }

    pub struct Owner {
        log: Arc<Log>,
    }

    #[actor(max_outstanding = 3, max_native_jobs = 2)]
    impl Owner {
        #[send_handler]
        async fn note(&mut self, msg: Note, _: &Context<Self>) {
            self.log.push(msg.0);
        }
    }

    #[send_handler]
    impl Handler<Poke> for Owner {
        async fn handle(&mut self, msg: Poke, _ctx: &Context<Self>) {
            if msg.0 {
                return;
            }
            self.log.push("poked");
        }
    }

    fn describe<T: std::fmt::Debug>(name: &str, outcome: JobOutcome<T>) -> Note {
        Note(match outcome {
            Ok(v) => format!("{name}:ok:{v:?}"),
            Err(JobError::Panicked(m)) => format!("{name}:panicked:{m}"),
            Err(JobError::DeadlineExceeded) => format!("{name}:deadline"),
            Err(other) => format!("{name}:{other}"),
        })
    }

    fn start() -> (ActorRef<Owner>, Arc<Log>) {
        let log = Arc::new(Log::default());
        (Owner { log: log.clone() }.start(), log)
    }

    async fn wait_for(log: &Log, n: usize) -> Vec<String> {
        for _ in 0..1000 {
            let entries = log.snapshot();
            if entries.len() >= n {
                return entries;
            }
            spawned_rt::tasks::sleep(Duration::from_millis(2)).await;
        }
        panic!("log never reached {n} entries: {:?}", log.snapshot());
    }

    #[test]
    fn actor_macro_sets_the_bounds() {
        assert_eq!(<Owner as Actor>::MAX_OUTSTANDING, 3);
        assert_eq!(<Owner as Actor>::MAX_NATIVE_JOBS, Some(2));
    }

    #[spawned_macros::sim_test]
    async fn macro_bounds_apply_and_permits_keep_the_parts() {
        let (actor, _log) = start();
        let kinds = in_actor(&actor, |_, ctx| {
            let a = ctx
                .reserve_blocking_job(JobOptions::new())
                .expect("first native slot");
            let b = ctx
                .reserve_blocking_job(JobOptions::new())
                .expect("second native slot");
            // max_native_jobs = 2: the third is refused, and the caller keeps
            // whatever it meant to move into the job.
            let third = ctx.reserve_blocking_job(JobOptions::new()).err();
            drop((a, b));
            let c = ctx.reserve_job(JobOptions::new()).expect("credit");
            let d = ctx.reserve_job(JobOptions::new()).expect("credit");
            let e = ctx.reserve_job(JobOptions::new()).expect("credit");
            // max_outstanding = 3.
            let fourth = ctx.reserve_job(JobOptions::new()).err();
            drop((c, d, e));
            let again = ctx.reserve_job(JobOptions::new()).is_ok();
            (third, fourth, again, ctx.deferred_outstanding())
        })
        .await;
        assert!(matches!(kinds.0, Some(PipeError::NativeBudget)));
        assert!(matches!(kinds.1, Some(PipeError::Overloaded)));
        assert!(kinds.2, "dropped permits return their credits");
        assert_eq!(kinds.3, 0);
        actor.context().stop();
        actor.join().await;
    }

    #[cfg(not(spawned_sim))] // native jobs are not simulated
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_jobs_run_on_the_tokio_blocking_pool() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            let permit = ctx.reserve_blocking_job(JobOptions::new()).unwrap();
            permit.pipe_blocking(
                |_| std::thread::current().name().map(str::to_owned),
                |o| describe("thread", o),
            );
            ctx.pipe_blocking(
                JobOptions::new(),
                |_| -> u32 { panic!("check boom") },
                |o| describe("panics", o),
            )
            .unwrap();
        })
        .await;
        let mut entries = wait_for(&log, 2).await;
        entries.sort();
        assert_eq!(entries[0], "panics:panicked:check boom");
        assert!(
            entries[1].starts_with("thread:ok:Some(\"tokio"),
            "not on the blocking pool: {}",
            entries[1]
        );
        actor.context().stop();
        actor.join().await;
    }

    #[cfg(not(spawned_sim))] // native jobs are not simulated
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emit_at_arrives_late_but_before_the_terminal_message() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            ctx.pipe_blocking(
                JobOptions::new(),
                |job| {
                    let at = Instant::now() + Duration::from_millis(80);
                    job.emit_at_blocking(at, Note("late".into()))?;
                    job.emit_blocking(Note("now".into()))?;
                    Ok::<_, JobGone>(())
                },
                |o| describe("job", o),
            )
            .unwrap();
        })
        .await;
        let entries = wait_for(&log, 3).await;
        assert_eq!(entries, ["now", "late", "job:ok:Ok(())"]);
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn async_emit_at_holds_the_terminal_message() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            ctx.pipe(
                JobOptions::new(),
                |job| async move {
                    let at = Instant::now() + Duration::from_millis(50);
                    job.emit_at(at, Note("late".into())).await?;
                    job.emit(Note("now".into())).await
                },
                |o| describe("job", o),
            )
            .unwrap();
        })
        .await;
        let entries = wait_for(&log, 3).await;
        assert_eq!(entries, ["now", "late", "job:ok:Ok(())"]);
        let outstanding = in_actor(&actor, |_, ctx| ctx.deferred_outstanding()).await;
        assert_eq!(outstanding, 0);
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn a_cancelled_job_fences_its_scheduled_emits() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            let opts = JobOptions::new().key("k");
            ctx.pipe(
                opts.clone(),
                |job| async move {
                    let at = Instant::now() + Duration::from_millis(60);
                    job.emit_at(at, Note("stale".into())).await
                },
                |o| describe("old", o),
            )
            .unwrap();
        })
        .await;
        // The replacement cancels the previous job: its scheduled emit and
        // its held terminal message are dropped.
        in_actor(&actor, |_, ctx| {
            let opts = JobOptions::new().key("k");
            ctx.pipe(opts, |_| async { 1 }, |o| describe("new", o))
                .unwrap();
        })
        .await;
        spawned_rt::tasks::sleep(Duration::from_millis(150)).await;
        assert_eq!(log.snapshot(), ["new:ok:1"]);
        let outstanding = in_actor(&actor, |_, ctx| ctx.deferred_outstanding()).await;
        assert_eq!(outstanding, 0);
        actor.context().stop();
        actor.join().await;
    }

    #[cfg(not(spawned_sim))] // native jobs are not simulated
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_deadlines_are_hard() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            let opts = JobOptions::new().deadline(Duration::from_millis(30));
            ctx.pipe_blocking(
                opts,
                |job| {
                    std::thread::sleep(Duration::from_millis(120));
                    // Past the deadline: the job is over for the owner.
                    job.emit_blocking(Note("too late".into())).is_err()
                },
                |o| describe("slow", o),
            )
            .unwrap();
        })
        .await;
        assert_eq!(wait_for(&log, 1).await, ["slow:deadline"]);
        spawned_rt::tasks::sleep(Duration::from_millis(200)).await;
        assert_eq!(log.snapshot(), ["slow:deadline"]);
        actor.context().stop();
        actor.join().await;
    }

    /// Review P1 (Codex 8, agy 1): a hard deadline overtakes a result held
    /// for a scheduled emit. The run used to drop its only strong reference
    /// to the deadline when the work returned, so the deadline never fired:
    /// the emit and `Ok` arrived at the emit's instant instead.
    #[spawned_macros::sim_test]
    async fn a_deadline_overtakes_a_result_held_for_scheduled_emits() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            let opts = JobOptions::new().deadline(Duration::from_millis(40));
            ctx.pipe(
                opts,
                |job| async move {
                    let at = Instant::now() + Duration::from_millis(300);
                    job.emit_at(at, Note("late".into())).await.unwrap();
                    7
                },
                |o| describe("job", o),
            )
            .unwrap();
        })
        .await;
        spawned_rt::tasks::sleep(Duration::from_millis(150)).await;
        assert_eq!(log.snapshot(), ["job:deadline"]);
        // The scheduled emit is dropped at its instant (the job is over).
        spawned_rt::tasks::sleep(Duration::from_millis(300)).await;
        assert_eq!(log.snapshot(), ["job:deadline"]);
        let outstanding = in_actor(&actor, |_, ctx| ctx.deferred_outstanding()).await;
        assert_eq!(outstanding, 0);
        actor.context().stop();
        actor.join().await;
    }

    /// Review P1 (Codex 8), tasks-mode blocking job: the deadline overtakes
    /// a result held for a scheduled emit.
    #[cfg(not(spawned_sim))] // native jobs are not simulated
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_deadline_overtakes_a_blocking_jobs_held_result() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            let opts = JobOptions::new().deadline(Duration::from_millis(40));
            ctx.pipe_blocking(
                opts,
                |job| {
                    let at = Instant::now() + Duration::from_millis(400);
                    job.emit_at_blocking(at, Note("late".into())).unwrap();
                    7
                },
                |o| describe("job", o),
            )
            .unwrap();
        })
        .await;
        assert_eq!(wait_for(&log, 1).await, ["job:deadline"]);
        // Past the emit's instant: it was dropped (the job is over).
        spawned_rt::tasks::sleep(Duration::from_millis(500)).await;
        assert_eq!(log.snapshot(), ["job:deadline"]);
        actor.context().stop();
        actor.join().await;
    }

    /// Review P1 (agy 5): a result held for scheduled emits keeps the job's
    /// `MAX_OUTSTANDING` credit until its terminal message is enqueued. It
    /// used to return the credit when the work returned, so held jobs (and
    /// their scheduled emits) were not bounded by `MAX_OUTSTANDING`.
    #[spawned_macros::sim_test]
    async fn a_held_result_keeps_its_credit_until_the_terminal_message() {
        let (actor, log) = start();
        let job = in_actor(&actor, |_, ctx| {
            ctx.pipe(
                JobOptions::new(),
                |job| async move {
                    let at = Instant::now() + Duration::from_millis(200);
                    job.emit_at(at, Note("late".into())).await.unwrap();
                    1
                },
                |o| describe("job", o),
            )
            .unwrap()
        })
        .await;
        // Handshake: the work has returned (its result is now held).
        while job.is_running() {
            spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
        }
        assert!(log.snapshot().is_empty());
        let outstanding = in_actor(&actor, |_, ctx| ctx.deferred_outstanding()).await;
        assert_eq!(outstanding, 1, "a held result returned its credit early");
        assert_eq!(wait_for(&log, 2).await, ["late", "job:ok:1"]);
        let outstanding = in_actor(&actor, |_, ctx| ctx.deferred_outstanding()).await;
        assert_eq!(outstanding, 0);
        actor.context().stop();
        actor.join().await;
    }

    /// `emit_at` with an instant already reached (or in the past) is an
    /// immediate emit: enqueued at the call, ahead of later emits.
    #[spawned_macros::sim_test]
    async fn emit_at_a_past_instant_is_delivered_immediately() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            ctx.pipe(
                JobOptions::new(),
                |job| async move {
                    let past = Instant::now();
                    spawned_rt::tasks::sleep(Duration::from_millis(5)).await;
                    job.emit_at(past, Note("past".into())).await?;
                    job.emit(Note("later".into())).await
                },
                |o| describe("job", o),
            )
            .unwrap();
        })
        .await;
        assert_eq!(wait_for(&log, 3).await, ["past", "later", "job:ok:Ok(())"]);
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn send_handler_impls_answer_requests_with_unit() {
        let (actor, log) = start();
        assert!(matches!(actor.request(Poke(false)).await, Ok(())));
        // The early return still answers.
        assert!(matches!(actor.request(Poke(true)).await, Ok(())));
        actor.send(Poke(false)).unwrap();
        assert_eq!(wait_for(&log, 2).await, ["poked", "poked"]);
        actor.context().stop();
        actor.join().await;
        assert!(actor.request(Poke(false)).await.is_err());
    }

    #[spawned_macros::sim_test]
    async fn child_spec_overrides_the_actor_bounds() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("bounds", 1).unwrap();
        let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
        let log = Arc::new(Log::default());
        let child = builder
            .add(
                ChildSpec::new("bounded", &group, move |_| {
                    let log = log.clone();
                    async move { Ok::<_, String>(Owner { log }) }
                })
                .max_outstanding(2)
                .max_native_jobs(1),
            )
            .unwrap();
        let _supervisor = builder.start().await.unwrap();
        let actor = child.current().unwrap();
        let refused = in_actor(&actor, |_, ctx| {
            let native = ctx
                .reserve_blocking_job(JobOptions::new())
                .expect("one native slot");
            let second_native = ctx.reserve_blocking_job(JobOptions::new()).err();
            drop(native);
            let credits = (
                ctx.reserve_job(JobOptions::new()),
                ctx.reserve_job(JobOptions::new()),
            );
            assert!(credits.0.is_ok() && credits.1.is_ok(), "two credits");
            let second_credit = ctx.reserve_job(JobOptions::new()).err();
            drop(credits);
            (second_native, second_credit)
        })
        .await;
        assert!(matches!(refused.0, Some(PipeError::NativeBudget)));
        assert!(matches!(refused.1, Some(PipeError::Overloaded)));
        let _ = system.shutdown(Duration::from_secs(5)).await;
    }
}

#[cfg(not(spawned_sim))]
mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{
        Actor, ActorStart, Context, Handler, Instant, JobGone, JobOptions, JobOutcome, PipeError,
    };
    use spawned_concurrency::{actor, send_handler};
    use spawned_test::threads::in_actor;
    use std::time::Duration;

    pub struct Owner {
        log: Arc<Log>,
    }

    #[actor(max_native_jobs = 1)]
    impl Owner {
        #[send_handler]
        fn note(&mut self, msg: Note, _: &Context<Self>) {
            self.log.push(msg.0);
        }
    }

    #[send_handler]
    impl Handler<Poke> for Owner {
        fn handle(&mut self, msg: Poke, _ctx: &Context<Self>) {
            if msg.0 {
                return;
            }
            self.log.push("poked");
        }
    }

    fn wait_for(log: &Log, n: usize) -> Vec<String> {
        for _ in 0..1000 {
            let entries = log.snapshot();
            if entries.len() >= n {
                return entries;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("log never reached {n} entries: {:?}", log.snapshot());
    }

    fn label(name: &str, outcome: JobOutcome<Result<(), JobGone>>) -> Note {
        Note(format!("{name}:{:?}", outcome))
    }

    #[test]
    fn in_actor_permits_and_emit_at() {
        let log = Arc::new(Log::default());
        let actor = Owner { log: log.clone() }.start();
        let refused = in_actor(&actor, |_, ctx| {
            let permit = ctx
                .reserve_blocking_job(JobOptions::new())
                .expect("one native slot");
            // max_native_jobs = 1.
            let second = ctx.reserve_blocking_job(JobOptions::new()).err();
            permit.pipe_blocking(
                |job| {
                    let at = Instant::now() + Duration::from_millis(60);
                    job.emit_at_blocking(at, Note("late".into()))?;
                    job.emit_blocking(Note("now".into()))
                },
                |o| label("job", o),
            );
            second
        });
        assert!(matches!(refused, Some(PipeError::NativeBudget)));
        assert_eq!(wait_for(&log, 3), ["now", "late", "job:Ok(Ok(()))"]);
        actor.context().stop();
        actor.join();
    }

    /// Review P1 (Codex 8, agy 1), threads mode: a hard deadline overtakes
    /// a result held for a scheduled emit.
    #[test]
    fn a_deadline_overtakes_a_result_held_for_scheduled_emits() {
        let log = Arc::new(Log::default());
        let actor = Owner { log: log.clone() }.start();
        in_actor(&actor, |_, ctx| {
            let opts = JobOptions::new().deadline(Duration::from_millis(40));
            ctx.pipe_blocking(
                opts,
                |job| {
                    let at = Instant::now() + Duration::from_millis(300);
                    job.emit_at_blocking(at, Note("late".into()))
                },
                |o| label("job", o),
            )
            .unwrap();
        });
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(log.snapshot(), ["job:Err(DeadlineExceeded)"]);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(log.snapshot(), ["job:Err(DeadlineExceeded)"]);
        assert_eq!(in_actor(&actor, |_, ctx| ctx.deferred_outstanding()), 0);
        actor.context().stop();
        actor.join();
    }

    /// Review P1 (agy 5), threads mode: a held result keeps its credit.
    #[test]
    fn a_held_result_keeps_its_credit_until_the_terminal_message() {
        let log = Arc::new(Log::default());
        let actor = Owner { log: log.clone() }.start();
        let job = in_actor(&actor, |_, ctx| {
            ctx.pipe_blocking(
                JobOptions::new(),
                |job| {
                    let at = Instant::now() + Duration::from_millis(200);
                    job.emit_at_blocking(at, Note("late".into()))
                },
                |o| label("job", o),
            )
            .unwrap()
        });
        // Handshake: the work has returned (its result is now held).
        while job.is_running() {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(log.snapshot().is_empty());
        assert_eq!(
            in_actor(&actor, |_, ctx| ctx.deferred_outstanding()),
            1,
            "a held result returned its credit early"
        );
        assert_eq!(wait_for(&log, 2), ["late", "job:Ok(Ok(()))"]);
        assert_eq!(in_actor(&actor, |_, ctx| ctx.deferred_outstanding()), 0);
        actor.context().stop();
        actor.join();
    }

    /// `emit_at_blocking` with an instant already reached is immediate.
    #[test]
    fn emit_at_a_past_instant_is_delivered_immediately() {
        let log = Arc::new(Log::default());
        let actor = Owner { log: log.clone() }.start();
        in_actor(&actor, |_, ctx| {
            ctx.pipe_blocking(
                JobOptions::new(),
                |job| {
                    let past = Instant::now();
                    std::thread::sleep(Duration::from_millis(5));
                    job.emit_at_blocking(past, Note("past".into()))?;
                    job.emit_blocking(Note("later".into()))
                },
                |o| label("job", o),
            )
            .unwrap();
        });
        assert_eq!(wait_for(&log, 3), ["past", "later", "job:Ok(Ok(()))"]);
        actor.context().stop();
        actor.join();
    }

    #[test]
    fn send_handler_impls_answer_requests_with_unit() {
        let log = Arc::new(Log::default());
        let actor = Owner { log: log.clone() }.start();
        assert!(matches!(actor.request(Poke(false)), Ok(())));
        assert!(matches!(actor.request(Poke(true)), Ok(())));
        assert_eq!(log.snapshot(), ["poked"]);
        actor.context().stop();
        actor.join();
    }
}

/// Review P2 (Codex 14, agy 3): shortening a delegated token's deadline
/// wakes its watchers so they re-arm; the caller-gone watcher of a blocking
/// `pipe_reply` job must recheck the slot, not cancel the job. A blocking
/// linked job's deadline hands the token back while the work keeps running;
/// the handler then shortens the token and checks the job.
#[cfg(all(feature = "async", not(spawned_sim)))] // native jobs are not simulated
mod caller_gone_vs_shortening_tasks {
    use spawned_concurrency::message::Message;
    use spawned_concurrency::tasks::{
        Actor, ActorStart, Context, Handler, JobError, JobHandle, JobOptions, JobOutcome,
        PendingReply, ReplyTo,
    };
    use std::time::Duration;

    #[derive(Default)]
    struct Delegator {
        job: Option<JobHandle>,
    }
    impl Actor for Delegator {
        // Two blocking jobs at once (the default is 1 on one CPU).
        const MAX_NATIVE_JOBS: Option<usize> = Some(2);
    }
    struct Ask;
    impl Message for Ask {
        type Result = (bool, bool);
    }
    struct Delegated(JobOutcome<()>, ReplyTo<(bool, bool)>);
    impl Message for Delegated {
        type Result = ();
    }
    impl Handler<Ask> for Delegator {
        async fn handle(
            &mut self,
            _: Ask,
            reply: PendingReply<'_, (bool, bool)>,
            ctx: &Context<Self>,
        ) {
            let token = reply.defer().unwrap();
            let opts = JobOptions::new()
                .cancel_on_caller_gone()
                .deadline(Duration::from_millis(20));
            let job = ctx
                .pipe_reply_blocking(
                    opts,
                    token,
                    |_| std::thread::sleep(Duration::from_millis(300)),
                    Delegated,
                )
                .unwrap();
            self.job = Some(job);
        }
    }
    struct Leave;
    impl Message for Leave {
        type Result = ();
    }
    struct Probe;
    impl Message for Probe {
        type Result = bool;
    }
    struct Ended(ReplyTo<()>);
    impl Message for Ended {
        type Result = ();
    }
    impl Handler<Ended> for Delegator {
        async fn handle(&mut self, msg: Ended, _: PendingReply<'_, ()>, _: &Context<Self>) {
            msg.0.reply(());
        }
    }
    impl Handler<Leave> for Delegator {
        async fn handle(&mut self, _: Leave, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
            let token = reply.defer().unwrap();
            let opts = JobOptions::new().cancel_on_caller_gone();
            let job = ctx
                .pipe_reply_blocking(
                    opts,
                    token,
                    |job| {
                        while !job.is_cancelled() {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    },
                    |_, token| Ended(token),
                )
                .unwrap();
            self.job = Some(job);
        }
    }
    impl Handler<Probe> for Delegator {
        async fn handle(&mut self, _: Probe, reply: PendingReply<'_, bool>, _: &Context<Self>) {
            reply.reply(self.job.as_ref().unwrap().is_cancelled());
        }
    }
    impl Handler<Delegated> for Delegator {
        async fn handle(&mut self, msg: Delegated, _: PendingReply<'_, ()>, _: &Context<Self>) {
            let Delegated(outcome, token) = msg;
            let deadline = matches!(outcome, Err(JobError::DeadlineExceeded));
            token.shorten_deadline(Duration::from_secs(30));
            let cancelled = self.job.as_ref().unwrap().is_cancelled();
            token.reply((deadline, cancelled));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shortening_the_deadline_does_not_cancel_a_caller_gone_job() {
        let actor = Delegator::default().start();
        let (deadline, cancelled) = actor
            .request_with_timeout(Ask, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(deadline, "the job deadline handed the token back");
        assert!(!cancelled, "a shortened deadline cancelled the job");
        // The caller leaving still cancels a blocking linked job.
        let left = actor
            .send_request(Leave, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
            .unwrap();
        assert!(
            !actor.request(Probe).await.unwrap(),
            "cancelled before the caller left"
        );
        drop(left);
        let mut cancelled = false;
        for _ in 0..500 {
            cancelled = actor.request(Probe).await.unwrap();
            if cancelled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(cancelled, "the caller left but the job was not cancelled");
        actor.context().stop();
        actor.join().await;
    }
}

#[cfg(not(spawned_sim))]
mod caller_gone_vs_shortening_threads {
    use spawned_concurrency::message::Message;
    use spawned_concurrency::threads::{
        Actor, ActorStart, Context, Handler, JobError, JobHandle, JobOptions, JobOutcome,
        PendingReply, ReplyTo,
    };
    use std::time::Duration;

    #[derive(Default)]
    struct Delegator {
        job: Option<JobHandle>,
    }
    impl Actor for Delegator {
        // Two blocking jobs at once (the default is 1 on one CPU).
        const MAX_NATIVE_JOBS: Option<usize> = Some(2);
    }
    struct Ask;
    impl Message for Ask {
        type Result = (bool, bool);
    }
    struct Delegated(JobOutcome<()>, ReplyTo<(bool, bool)>);
    impl Message for Delegated {
        type Result = ();
    }
    impl Handler<Ask> for Delegator {
        fn handle(&mut self, _: Ask, reply: PendingReply<'_, (bool, bool)>, ctx: &Context<Self>) {
            let token = reply.defer().unwrap();
            let opts = JobOptions::new()
                .cancel_on_caller_gone()
                .deadline(Duration::from_millis(20));
            let job = ctx
                .pipe_reply_blocking(
                    opts,
                    token,
                    |_| std::thread::sleep(Duration::from_millis(300)),
                    Delegated,
                )
                .unwrap();
            self.job = Some(job);
        }
    }
    struct Leave;
    impl Message for Leave {
        type Result = ();
    }
    struct Probe;
    impl Message for Probe {
        type Result = bool;
    }
    struct Ended(ReplyTo<()>);
    impl Message for Ended {
        type Result = ();
    }
    impl Handler<Ended> for Delegator {
        fn handle(&mut self, msg: Ended, _: PendingReply<'_, ()>, _: &Context<Self>) {
            msg.0.reply(());
        }
    }
    impl Handler<Leave> for Delegator {
        fn handle(&mut self, _: Leave, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
            let token = reply.defer().unwrap();
            let opts = JobOptions::new().cancel_on_caller_gone();
            let job = ctx
                .pipe_reply_blocking(
                    opts,
                    token,
                    |job| {
                        while !job.is_cancelled() {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    },
                    |_, token| Ended(token),
                )
                .unwrap();
            self.job = Some(job);
        }
    }
    impl Handler<Probe> for Delegator {
        fn handle(&mut self, _: Probe, reply: PendingReply<'_, bool>, _: &Context<Self>) {
            reply.reply(self.job.as_ref().unwrap().is_cancelled());
        }
    }
    impl Handler<Delegated> for Delegator {
        fn handle(&mut self, msg: Delegated, _: PendingReply<'_, ()>, _: &Context<Self>) {
            let Delegated(outcome, token) = msg;
            let deadline = matches!(outcome, Err(JobError::DeadlineExceeded));
            token.shorten_deadline(Duration::from_secs(30));
            let cancelled = self.job.as_ref().unwrap().is_cancelled();
            token.reply((deadline, cancelled));
        }
    }

    #[test]
    fn shortening_the_deadline_does_not_cancel_a_caller_gone_job() {
        let actor = Delegator::default().start();
        let (deadline, cancelled) = actor
            .request_with_timeout(Ask, Duration::from_secs(60))
            .unwrap();
        assert!(deadline, "the job deadline handed the token back");
        assert!(!cancelled, "a shortened deadline cancelled the job");
        // The caller leaving still cancels a blocking linked job.
        let left = actor
            .send_request(Leave, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
            .unwrap();
        assert!(
            !actor.request(Probe).unwrap(),
            "cancelled before the caller left"
        );
        drop(left);
        let mut cancelled = false;
        for _ in 0..500 {
            cancelled = actor.request(Probe).unwrap();
            if cancelled {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(cancelled, "the caller left but the job was not cancelled");
        actor.context().stop();
        actor.join();
    }
}
