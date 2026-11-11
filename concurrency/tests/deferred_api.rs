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
        JobOutcome, PipeErrorKind,
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
        Note(match outcome.into_result() {
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
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("log never reached {n} entries: {:?}", log.snapshot());
    }

    #[test]
    fn actor_macro_sets_the_bounds() {
        assert_eq!(<Owner as Actor>::MAX_OUTSTANDING, 3);
        assert_eq!(<Owner as Actor>::MAX_NATIVE_JOBS, Some(2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn macro_bounds_apply_and_permits_keep_the_parts() {
        let (actor, _log) = start();
        let kinds = in_actor(&actor, |_, ctx| {
            let a = ctx.reserve_blocking_job().expect("first native slot");
            let b = ctx.reserve_blocking_job().expect("second native slot");
            // max_native_jobs = 2: the third is refused, and the caller keeps
            // whatever it meant to move into the job.
            let third = ctx.reserve_blocking_job().err().map(|e| e.kind);
            drop((a, b));
            let c = ctx.reserve_job().expect("credit");
            let d = ctx.reserve_job().expect("credit");
            let e = ctx.reserve_job().expect("credit");
            // max_outstanding = 3.
            let fourth = ctx.reserve_job().err().map(|e| e.kind);
            drop((c, d, e));
            let again = ctx.reserve_job().is_ok();
            (third, fourth, again, ctx.deferred_outstanding())
        })
        .await;
        assert!(matches!(kinds.0, Some(PipeErrorKind::NativeBudget)));
        assert!(matches!(kinds.1, Some(PipeErrorKind::Overloaded)));
        assert!(kinds.2, "dropped permits return their credits");
        assert_eq!(kinds.3, 0);
        actor.context().stop();
        actor.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_jobs_run_on_the_tokio_blocking_pool() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            let permit = ctx.reserve_blocking_job().unwrap();
            permit.pipe_blocking_with(
                JobOptions::default(),
                |_| std::thread::current().name().map(str::to_owned),
                |o| describe("thread", o),
            );
            ctx.pipe_blocking_with(
                JobOptions::default(),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emit_at_arrives_late_but_before_the_terminal_message() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            ctx.pipe_blocking_with(
                JobOptions::default(),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_job_fences_its_scheduled_emits() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            let opts = JobOptions {
                key: Some("k".into()),
                ..JobOptions::default()
            };
            ctx.pipe_with_ctx(
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
            let opts = JobOptions {
                key: Some("k".into()),
                ..JobOptions::default()
            };
            ctx.pipe_with(opts, async { 1 }, |o| describe("new", o))
                .unwrap();
        })
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(log.snapshot(), ["new:ok:1"]);
        let outstanding = in_actor(&actor, |_, ctx| ctx.deferred_outstanding()).await;
        assert_eq!(outstanding, 0);
        actor.context().stop();
        actor.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_deadlines_are_hard() {
        let (actor, log) = start();
        in_actor(&actor, |_, ctx| {
            let opts = JobOptions::default().deadline_after(Duration::from_millis(30));
            ctx.pipe_blocking_with(
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
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(log.snapshot(), ["slow:deadline"]);
        actor.context().stop();
        actor.join().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
            let native = ctx.reserve_blocking_job().expect("one native slot");
            let second_native = ctx.reserve_blocking_job().err().map(|e| e.kind);
            drop(native);
            let credits = (ctx.reserve_job(), ctx.reserve_job());
            assert!(credits.0.is_ok() && credits.1.is_ok(), "two credits");
            let second_credit = ctx.reserve_job().err().map(|e| e.kind);
            drop(credits);
            (second_native, second_credit)
        })
        .await;
        assert!(matches!(refused.0, Some(PipeErrorKind::NativeBudget)));
        assert!(matches!(refused.1, Some(PipeErrorKind::Overloaded)));
        let _ = system.shutdown(Duration::from_secs(5)).await;
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{
        Actor, ActorStart, Context, Handler, Instant, JobGone, JobOptions, JobOutcome,
        PipeErrorKind,
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
        Note(format!("{name}:{:?}", outcome.into_result()))
    }

    #[test]
    fn in_actor_permits_and_emit_at() {
        let log = Arc::new(Log::default());
        let actor = Owner { log: log.clone() }.start();
        let refused = in_actor(&actor, |_, ctx| {
            let permit = ctx.reserve_blocking_job().expect("one native slot");
            // max_native_jobs = 1.
            let second = ctx.reserve_blocking_job().err().map(|e| e.kind);
            permit.pipe_blocking_with(
                JobOptions::default(),
                |job| {
                    let at = Instant::now() + Duration::from_millis(60);
                    job.emit_at_blocking(at, Note("late".into()))?;
                    job.emit_blocking(Note("now".into()))
                },
                |o| label("job", o),
            );
            second
        });
        assert!(matches!(refused, Some(PipeErrorKind::NativeBudget)));
        assert_eq!(wait_for(&log, 3), ["now", "late", "job:Ok(Ok(()))"]);
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
