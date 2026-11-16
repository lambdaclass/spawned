use super::*;
use crate::message::Message;
use std::{
    sync::{atomic, Arc},
    thread,
    time::Duration,
};

// --- Counter actor for basic tests ---

struct Counter {
    count: u64,
}

struct GetCount;
impl Message for GetCount {
    type Result = u64;
}

struct Increment;
impl Message for Increment {
    type Result = u64;
}

struct StopCounter;
impl Message for StopCounter {
    type Result = u64;
}

impl Actor for Counter {}

impl Handler<GetCount> for Counter {
    async fn handle(
        &mut self,
        _msg: GetCount,
        reply: crate::reply::PendingReply<'_, u64>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.count);
    }
}

impl Handler<Increment> for Counter {
    async fn handle(
        &mut self,
        _msg: Increment,
        reply: crate::reply::PendingReply<'_, u64>,
        _ctx: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            self.count += 1;
            self.count
        };
        reply.reply(__reply_value);
    }
}

impl Handler<StopCounter> for Counter {
    async fn handle(
        &mut self,
        _msg: StopCounter,
        reply: crate::reply::PendingReply<'_, u64>,
        ctx: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            ctx.stop();
            self.count
        };
        reply.reply(__reply_value);
    }
}

#[test]
pub fn backend_default_is_async() {
    assert_eq!(Backend::default(), Backend::Async);
}

#[test]
#[allow(clippy::clone_on_copy)]
pub fn backend_enum_is_copy_and_clone() {
    let backend = Backend::Async;
    let copied = backend;
    let cloned = backend.clone();
    assert_eq!(backend, copied);
    assert_eq!(backend, cloned);
}

#[test]
pub fn backend_enum_debug_format() {
    assert_eq!(format!("{:?}", Backend::Async), "Async");
    assert_eq!(format!("{:?}", Backend::Blocking), "Blocking");
    assert_eq!(format!("{:?}", Backend::Thread), "Thread");
}

#[test]
pub fn backend_enum_equality() {
    assert_eq!(Backend::Async, Backend::Async);
    assert_eq!(Backend::Blocking, Backend::Blocking);
    assert_eq!(Backend::Thread, Backend::Thread);
    assert_ne!(Backend::Async, Backend::Blocking);
    assert_ne!(Backend::Async, Backend::Thread);
    assert_ne!(Backend::Blocking, Backend::Thread);
}

#[test]
pub fn backend_async_handles_send_and_request() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::backend_async_handles_send_and_request"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let counter = Counter { count: 0 }.start();

                let result = counter.request(GetCount).await.unwrap();
                assert_eq!(result, 0);

                let result = counter.request(Increment).await.unwrap();
                assert_eq!(result, 1);

                // fire-and-forget send; the request below shares its mailbox, so
                // it is handled after the send without any extra wait.
                counter.send(Increment).unwrap();

                let result = counter.request(GetCount).await.unwrap();
                assert_eq!(result, 2);

                let final_count = counter.request(StopCounter).await.unwrap();
                assert_eq!(final_count, 2);
            });
        },
    );
}

#[test]
pub fn backend_blocking_handles_send_and_request() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::backend_blocking_handles_send_and_request"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let counter = Counter { count: 0 }.start_with_backend(Backend::Blocking);

                let result = counter.request(GetCount).await.unwrap();
                assert_eq!(result, 0);

                let result = counter.request(Increment).await.unwrap();
                assert_eq!(result, 1);

                counter.send(Increment).unwrap();

                let result = counter.request(GetCount).await.unwrap();
                assert_eq!(result, 2);

                let final_count = counter.request(StopCounter).await.unwrap();
                assert_eq!(final_count, 2);
            });
        },
    );
}

#[test]
pub fn backend_thread_handles_send_and_request() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::backend_thread_handles_send_and_request"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let counter = Counter { count: 0 }.start_with_backend(Backend::Thread);

                let result = counter.request(GetCount).await.unwrap();
                assert_eq!(result, 0);

                let result = counter.request(Increment).await.unwrap();
                assert_eq!(result, 1);

                counter.send(Increment).unwrap();

                let result = counter.request(GetCount).await.unwrap();
                assert_eq!(result, 2);

                let final_count = counter.request(StopCounter).await.unwrap();
                assert_eq!(final_count, 2);
            });
        },
    );
}

#[test]
pub fn multiple_backends_concurrent() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::multiple_backends_concurrent"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let async_counter = Counter { count: 0 }.start();
                let blocking_counter = Counter { count: 100 }.start_with_backend(Backend::Blocking);
                let thread_counter = Counter { count: 200 }.start_with_backend(Backend::Thread);

                async_counter.request(Increment).await.unwrap();
                blocking_counter.request(Increment).await.unwrap();
                thread_counter.request(Increment).await.unwrap();

                let async_val = async_counter.request(GetCount).await.unwrap();
                let blocking_val = blocking_counter.request(GetCount).await.unwrap();
                let thread_val = thread_counter.request(GetCount).await.unwrap();

                assert_eq!(async_val, 1);
                assert_eq!(blocking_val, 101);
                assert_eq!(thread_val, 201);

                async_counter.request(StopCounter).await.unwrap();
                blocking_counter.request(StopCounter).await.unwrap();
                thread_counter.request(StopCounter).await.unwrap();
            });
        },
    );
}

#[test]
pub fn request_timeout() {
    crate::test_wait::bounded(concat!(module_path!(), "::request_timeout"), move || {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            struct SlowActor;
            struct SlowOp;
            impl Message for SlowOp {
                type Result = ();
            }
            impl Actor for SlowActor {}
            impl Handler<SlowOp> for SlowActor {
                async fn handle(
                    &mut self,
                    _msg: SlowOp,
                    reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    // Never replies, so the timeout is the only possible outcome
                    // however late the runtime polls it.
                    std::future::pending::<()>().await;
                    reply.reply(());
                }
            }

            let actor = SlowActor.start();
            let result = actor
                .request_with_timeout(SlowOp, Duration::from_millis(50))
                .await;
            assert!(matches!(result, Err(ActorError::RequestTimeout)));
        });
    });
}

#[test]
pub fn recipient_type_erasure() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::recipient_type_erasure"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let counter = Counter { count: 42 }.start();
                let recipient: Recipient<GetCount> = counter.recipient();

                let rx = recipient
                    .send_request(GetCount, Deadline::after(DEFAULT_REQUEST_TIMEOUT))
                    .unwrap();
                let result = rx.await.unwrap();
                assert_eq!(result, 42);

                // Also test request helper
                let result = request(&*recipient, GetCount, Duration::from_secs(5))
                    .await
                    .unwrap();
                assert_eq!(result, 42);
            });
        },
    );
}

// --- SlowShutdownActor for join tests ---

struct SlowShutdownActor;

struct StopSlow;
impl Message for StopSlow {
    type Result = ();
}

impl Actor for SlowShutdownActor {
    async fn stopped(&mut self, _ctx: &Context<Self>) {
        thread::sleep(Duration::from_millis(500));
    }
}

impl Handler<StopSlow> for SlowShutdownActor {
    async fn handle(
        &mut self,
        _msg: StopSlow,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.stop();
        reply.reply(());
    }
}

/// Blocks its thread in a callback until the test releases it.
struct HeldThread {
    entered: Arc<atomic::AtomicBool>,
    release: std::sync::mpsc::Receiver<()>,
    released: Arc<atomic::AtomicBool>,
}

impl HeldThread {
    fn new() -> (Self, std::sync::mpsc::Sender<()>) {
        let (tx, release) = std::sync::mpsc::channel();
        let actor = HeldThread {
            entered: Arc::default(),
            release,
            released: Arc::default(),
        };
        (actor, tx)
    }

    fn hold(&mut self) {
        self.entered.store(true, atomic::Ordering::SeqCst);
        let released = self.release.recv_timeout(crate::test_wait::WAIT).is_ok();
        self.released.store(released, atomic::Ordering::SeqCst);
    }
}

impl Actor for HeldThread {
    async fn stopped(&mut self, _ctx: &Context<Self>) {
        // Only the join test sends StopSlow; DoBlock already held.
        if !self.entered.load(atomic::Ordering::SeqCst) {
            self.hold();
        }
    }
}

impl Handler<StopSlow> for HeldThread {
    async fn handle(
        &mut self,
        _msg: StopSlow,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.stop();
        reply.reply(());
    }
}

impl Handler<DoBlock> for HeldThread {
    async fn handle(
        &mut self,
        _msg: DoBlock,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        self.hold();
        ctx.stop();
        reply.reply(());
    }
}

#[test]
pub fn thread_backend_join_does_not_block_runtime() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::thread_backend_join_does_not_block_runtime"
        ),
        move || {
            // stopped() blocks its OS thread until a task on this current-thread
            // runtime releases it. If join() blocked the runtime, that task could
            // never run and stopped() would give up after the timeout instead.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            runtime.block_on(async move {
                let (held, release) = HeldThread::new();
                let released = held.released.clone();
                let actor = held.start_with_backend(Backend::Thread);
                actor.send(StopSlow).unwrap();
                let _releaser = rt::spawn(async move {
                    let _ = release.send(());
                });

                actor.join().await;

                assert!(
                    released.load(atomic::Ordering::SeqCst),
                    "join() blocked the runtime: the releasing task never ran"
                );
            });
        },
    );
}

#[test]
pub fn multiple_join_callers_all_notified() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::multiple_join_callers_all_notified"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let actor = SlowShutdownActor.start();
                let actor_clone1 = actor.clone();
                let actor_clone2 = actor.clone();

                let join1 = rt::spawn(async move {
                    actor_clone1.join().await;
                    1u32
                });
                let join2 = rt::spawn(async move {
                    actor_clone2.join().await;
                    2u32
                });

                rt::sleep(Duration::from_millis(10)).await;

                actor.send(StopSlow).unwrap();

                let (r1, r2) = tokio::join!(join1, join2);
                assert_eq!(r1.unwrap(), 1);
                assert_eq!(r2.unwrap(), 2);

                actor.join().await;
            });
        },
    );
}

// --- Badly behaved actors for blocking tests ---

struct BadlyBehavedTask;

struct DoBlock;
impl Message for DoBlock {
    type Result = ();
}

impl Actor for BadlyBehavedTask {}

impl Handler<DoBlock> for BadlyBehavedTask {
    async fn handle(
        &mut self,
        _msg: DoBlock,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        rt::sleep(Duration::from_millis(20)).await;
        thread::sleep(Duration::from_secs(2));
        ctx.stop();
        reply.reply(());
    }
}

struct IncrementWell;
impl Message for IncrementWell {
    type Result = ();
}

struct WellBehavedTask {
    pub count: u64,
}

impl Actor for WellBehavedTask {}

impl Handler<GetCount> for WellBehavedTask {
    async fn handle(
        &mut self,
        _msg: GetCount,
        reply: crate::reply::PendingReply<'_, u64>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.count);
    }
}

impl Handler<StopCounter> for WellBehavedTask {
    async fn handle(
        &mut self,
        _msg: StopCounter,
        reply: crate::reply::PendingReply<'_, u64>,
        ctx: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            ctx.stop();
            self.count
        };
        reply.reply(__reply_value);
    }
}

impl Handler<IncrementWell> for WellBehavedTask {
    async fn handle(
        &mut self,
        _msg: IncrementWell,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        self.count += 1;
        use crate::tasks::send_after;
        send_after(Duration::from_millis(100), ctx.clone(), IncrementWell);
        reply.reply(());
    }
}

#[test]
pub fn badly_behaved_thread_non_blocking() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::badly_behaved_thread_non_blocking"),
        move || {
            // An async actor that blocks its thread starves everything else on
            // it. With one runtime thread this is deterministic; on a multi-thread
            // runtime the well-behaved actor may run on another worker.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let badboy = BadlyBehavedTask.start();
                badboy.send(DoBlock).unwrap();
                let goodboy = WellBehavedTask { count: 0 }.start();
                goodboy.send(IncrementWell).unwrap();
                rt::sleep(Duration::from_secs(1)).await;
                let count = goodboy.request(GetCount).await.unwrap();
                // The blocking handler held the only thread for 2 s of the 1 s
                // window, so the 100 ms chain could not get far.
                assert!(count < 10, "blocked runtime still made progress: {count}");
                goodboy.request(StopCounter).await.unwrap();
            });
        },
    );
}

/// An async actor on a single-threaded runtime keeps making progress while
/// an actor on `backend` is blocked inside a handler. With only one runtime
/// thread, a blocked handler on that thread would stop all progress.
fn blocked_handler_does_not_stall_runtime(backend: Backend) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let (held, release) = HeldThread::new();
        let entered = held.entered.clone();
        let badboy = held.start_with_backend(backend);
        badboy.send(DoBlock).unwrap();
        let entered = crate::test_wait::wait_for_async(
            || async { entered.load(atomic::Ordering::SeqCst) },
            |entered| *entered,
        )
        .await;
        assert!(entered, "blocking handler never started");

        let goodboy = WellBehavedTask { count: 0 }.start();
        goodboy.send(IncrementWell).unwrap();
        let count = crate::test_wait::wait_for_async(
            || goodboy.request(GetCount),
            |count| matches!(count, Ok(n) if *n >= 5),
        )
        .await
        .unwrap();
        assert!(count >= 5, "runtime stalled behind a blocked handler");

        release.send(()).unwrap();
        badboy.join().await;
        goodboy.request(StopCounter).await.unwrap();
    });
}

#[test]
pub fn badly_behaved_thread() {
    blocked_handler_does_not_stall_runtime(Backend::Blocking);
}

#[test]
pub fn backend_thread_isolates_blocking_work() {
    blocked_handler_does_not_stall_runtime(Backend::Thread);
}

// --- Panic recovery tests ---

#[test]
pub fn panic_in_started_stops_actor() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::panic_in_started_stops_actor"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                struct PanicOnStart;
                struct Ping;
                impl Message for Ping {
                    type Result = ();
                }
                impl Actor for PanicOnStart {
                    async fn started(&mut self, _ctx: &Context<Self>) {
                        panic!("boom in started");
                    }
                }
                impl Handler<Ping> for PanicOnStart {
                    async fn handle(
                        &mut self,
                        _msg: Ping,
                        reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        reply.reply(());
                    }
                }

                let actor = PanicOnStart.start();
                actor.join().await;
                let result = actor.send(Ping);
                assert!(result.is_err());
            });
        },
    );
}

#[test]
pub fn panic_in_handler_stops_actor() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::panic_in_handler_stops_actor"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                struct PanicOnMsg;
                struct Explode;
                impl Message for Explode {
                    type Result = ();
                }
                struct Check;
                impl Message for Check {
                    type Result = u32;
                }
                impl Actor for PanicOnMsg {}
                impl Handler<Explode> for PanicOnMsg {
                    async fn handle(
                        &mut self,
                        _msg: Explode,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        {
                            panic!("boom in handler");
                        }
                    }
                }
                impl Handler<Check> for PanicOnMsg {
                    async fn handle(
                        &mut self,
                        _msg: Check,
                        reply: crate::reply::PendingReply<'_, u32>,
                        _ctx: &Context<Self>,
                    ) {
                        reply.reply(42);
                    }
                }

                let actor = PanicOnMsg.start();
                actor.send(Explode).unwrap();
                actor.join().await;
                let result = actor.request(Check).await;
                assert!(result.is_err());
            });
        },
    );
}

/// Review P1 (Codex 9): a manual `Handler` may be a plain `fn` that
/// panics before it returns its future. That is a handler panic like
/// any other: the turn ends, `stopped()` runs, and the caller and
/// `wait_exit` see the handler-panic reason. The future used to be built
/// outside the turn's `catch_unwind`, so the panic unwound the whole
/// lifecycle (no `stopped()`, an "actor lifecycle unwound" reason).
#[test]
pub fn panic_building_the_handler_future_is_a_handler_panic() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::panic_building_the_handler_future_is_a_handler_panic"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                struct Eager(Arc<atomic::AtomicBool>);
                struct Explode;
                impl Message for Explode {
                    type Result = ();
                }
                impl Actor for Eager {
                    async fn stopped(&mut self, _ctx: &Context<Self>) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
                impl Handler<Explode> for Eager {
                    #[allow(unreachable_code, clippy::diverging_sub_expression)]
                    fn handle(
                        &mut self,
                        _msg: Explode,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) -> impl Future<Output = ()> + Send {
                        panic!("boom building the handler future");
                        async {}
                    }
                }

                let stopped = Arc::new(atomic::AtomicBool::new(false));
                let actor = Eager(stopped.clone()).start();
                let reply = actor.request(Explode).await;
                let reason = actor.wait_exit().await;
                let ExitReason::Panic(message) = &reason else {
                    panic!("unexpected reason {reason:?}");
                };
                assert!(
                    message.starts_with("panic in handler: boom building"),
                    "not a handler panic: {message}"
                );
                assert!(
                    matches!(&reply, Err(ActorError::ActorStopped(r)) if *r == reason),
                    "caller saw {reply:?}"
                );
                assert!(stopped.load(Ordering::SeqCst), "stopped() was skipped");
            });
        },
    );
}

#[test]
pub fn panic_in_stopped_still_completes() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::panic_in_stopped_still_completes"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                struct PanicOnStop;
                struct StopMe;
                impl Message for StopMe {
                    type Result = ();
                }
                impl Actor for PanicOnStop {
                    async fn stopped(&mut self, _ctx: &Context<Self>) {
                        panic!("boom in stopped");
                    }
                }
                impl Handler<StopMe> for PanicOnStop {
                    async fn handle(
                        &mut self,
                        _msg: StopMe,
                        reply: crate::reply::PendingReply<'_, ()>,
                        ctx: &Context<Self>,
                    ) {
                        ctx.stop();
                        reply.reply(());
                    }
                }

                let actor = PanicOnStop.start();
                actor.send(StopMe).unwrap();
                actor.join().await;
            });
        },
    );
}

#[test]
pub fn send_message_on_delivers() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::send_message_on_delivers"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let counter = Counter { count: 0 }.start();
                let ctx = counter.context();
                send_message_on(ctx, rt::sleep(Duration::from_millis(10)), Increment);
                let count = crate::test_wait::wait_for_async(
                    || counter.request(GetCount),
                    |count| matches!(count, Ok(1)),
                )
                .await
                .unwrap();
                assert_eq!(count, 1);
            });
        },
    );
}

#[test]
pub fn send_message_on_cancelled() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::send_message_on_cancelled"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let counter = Counter { count: 0 }.start();
                let ctx = counter.context();
                send_message_on(ctx, rt::sleep(Duration::from_millis(200)), Increment);
                // Stop actor before the future resolves
                let final_count = counter.request(StopCounter).await.unwrap();
                assert_eq!(final_count, 0, "message should not have been delivered");
                counter.join().await;
            });
        },
    );
}

// --- ExitReason tests ---

#[test]
pub fn exit_reason_normal_on_clean_stop() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::exit_reason_normal_on_clean_stop"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let actor = Counter { count: 0 }.start();
                actor.request(StopCounter).await.unwrap();
                let reason = actor.wait_exit().await;
                assert!(matches!(reason, ExitReason::Normal));
            });
        },
    );
}

#[test]
pub fn exit_reason_panic_in_started() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::exit_reason_panic_in_started"),
        move || {
            struct PanicStart;
            struct Ping;
            impl Message for Ping {
                type Result = ();
            }
            impl Actor for PanicStart {
                async fn started(&mut self, _ctx: &Context<Self>) {
                    panic!("boom in started");
                }
            }
            impl Handler<Ping> for PanicStart {
                async fn handle(
                    &mut self,
                    _msg: Ping,
                    reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    reply.reply(());
                }
            }

            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let actor = PanicStart.start();
                let reason = actor.wait_exit().await;
                assert!(
                    matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in started"))
                );
            });
        },
    );
}

#[test]
pub fn exit_reason_panic_in_handler() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::exit_reason_panic_in_handler"),
        move || {
            struct PanicHandler;
            struct Explode;
            impl Message for Explode {
                type Result = ();
            }
            impl Actor for PanicHandler {}
            impl Handler<Explode> for PanicHandler {
                async fn handle(
                    &mut self,
                    _msg: Explode,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("boom in handler");
                    }
                }
            }

            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let actor = PanicHandler.start();
                let _ = actor.send(Explode);
                let reason = actor.wait_exit().await;
                assert!(
                    matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in handler"))
                );
            });
        },
    );
}

#[test]
pub fn exit_reason_poll_returns_none_while_running() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::exit_reason_poll_returns_none_while_running"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let actor = Counter { count: 0 }.start();
                assert!(actor.exit_reason().is_none());
                actor.request(StopCounter).await.unwrap();
                actor.join().await;
                assert!(actor.exit_reason().is_some());
            });
        },
    );
}

// --- Monitor tests ---

struct GetDowns;
impl Message for GetDowns {
    type Result = Vec<crate::monitor::Down>;
}

/// Actor that exposes `monitor`/`demonitor` via messages, so tests can
/// drive it from outside. Records all received Down messages.
struct Watcher {
    downs: Arc<Mutex<Vec<crate::monitor::Down>>>,
}

struct StartMonitor(crate::ChildHandle);
impl Message for StartMonitor {
    type Result = crate::monitor::MonitorRef;
}
struct CallDemonitor(crate::monitor::MonitorRef);
impl Message for CallDemonitor {
    type Result = ();
}
/// Monitor, then demonitor in the same turn.
struct MonitorThenDemonitor(crate::ChildHandle);
impl Message for MonitorThenDemonitor {
    type Result = ();
}

impl Actor for Watcher {}

impl Handler<StartMonitor> for Watcher {
    async fn handle(
        &mut self,
        msg: StartMonitor,
        reply: crate::reply::PendingReply<'_, crate::monitor::MonitorRef>,
        ctx: &Context<Self>,
    ) {
        reply.reply(ctx.monitor(&msg.0));
    }
}

impl Handler<MonitorThenDemonitor> for Watcher {
    async fn handle(
        &mut self,
        msg: MonitorThenDemonitor,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        let monitor = ctx.monitor(&msg.0);
        ctx.demonitor(monitor);
        reply.reply(());
    }
}

impl Handler<CallDemonitor> for Watcher {
    async fn handle(
        &mut self,
        msg: CallDemonitor,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.demonitor(msg.0);
        reply.reply(());
    }
}

impl Handler<crate::monitor::Down> for Watcher {
    async fn handle(
        &mut self,
        msg: crate::monitor::Down,
        reply: crate::reply::PendingReply<'_, ()>,
        _ctx: &Context<Self>,
    ) {
        self.downs.lock().unwrap().push(msg);
        reply.reply(());
    }
}

impl Handler<GetDowns> for Watcher {
    async fn handle(
        &mut self,
        _msg: GetDowns,
        reply: crate::reply::PendingReply<'_, Vec<crate::monitor::Down>>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.downs.lock().unwrap().clone());
    }
}

#[test]
pub fn monitor_running_actor_delivers_down_on_exit() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::monitor_running_actor_delivers_down_on_exit"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let target = Counter { count: 0 }.start();
                let target_handle = target.child_handle();

                let watcher = Watcher {
                    downs: Arc::new(Mutex::new(Vec::new())),
                }
                .start();

                let monitor_ref = watcher.request(StartMonitor(target_handle)).await.unwrap();

                // Stop the target — Down should be delivered
                target.request(StopCounter).await.unwrap();
                target.join().await;

                let downs = crate::test_wait::wait_for_async(
                    || watcher.request(GetDowns),
                    |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
                )
                .await
                .unwrap();
                assert_eq!(downs.len(), 1);
                assert_eq!(downs[0].monitor_ref, monitor_ref);
                assert!(matches!(downs[0].reason, ExitReason::Normal));
            });
        },
    );
}

/// OTP `demonitor(Ref, [flush])`: a `Down` already in the mailbox
/// (here the `NoProc` a monitor of a dead target queues at once) is
/// never handled once `demonitor` returned.
#[test]
pub fn demonitor_flushes_a_queued_down() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::demonitor_flushes_a_queued_down"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let target = Counter { count: 0 }.start();
                target.request(StopCounter).await.unwrap();
                target.join().await;
                let watcher = Watcher {
                    downs: Arc::new(Mutex::new(Vec::new())),
                }
                .start();
                watcher
                    .request(MonitorThenDemonitor(target.child_handle()))
                    .await
                    .unwrap();
                // FIFO: the queued Down is dispatched before this request.
                let downs = watcher.request(GetDowns).await.unwrap();
                assert!(downs.is_empty(), "a flushed Down was handled: {downs:?}");
            });
        },
    );
}

#[test]
pub fn monitor_already_dead_actor_delivers_down_immediately() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::monitor_already_dead_actor_delivers_down_immediately"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let target = Counter { count: 0 }.start();
                target.request(StopCounter).await.unwrap();
                target.join().await;
                let target_handle = target.child_handle();

                let watcher = Watcher {
                    downs: Arc::new(Mutex::new(Vec::new())),
                }
                .start();

                let _ = watcher.request(StartMonitor(target_handle)).await.unwrap();

                let downs = crate::test_wait::wait_for_async(
                    || watcher.request(GetDowns),
                    |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
                )
                .await
                .unwrap();
                assert_eq!(downs.len(), 1);
            });
        },
    );
}

#[test]
pub fn demonitor_before_target_dies_suppresses_down() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::demonitor_before_target_dies_suppresses_down"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let target = Counter { count: 0 }.start();
                let target_handle = target.child_handle();

                let watcher = Watcher {
                    downs: Arc::new(Mutex::new(Vec::new())),
                }
                .start();

                let monitor_ref = watcher.request(StartMonitor(target_handle)).await.unwrap();
                watcher.request(CallDemonitor(monitor_ref)).await.unwrap();

                // Now stop the target
                target.request(StopCounter).await.unwrap();
                target.join().await;
                rt::sleep(Duration::from_millis(50)).await;

                let downs = watcher.request(GetDowns).await.unwrap();
                assert!(downs.is_empty(), "expected no Down, got {:?}", downs.len());
            });
        },
    );
}

#[test]
pub fn multiple_monitors_each_get_own_ref_and_down() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::multiple_monitors_each_get_own_ref_and_down"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let target = Counter { count: 0 }.start();
                let target_handle = target.child_handle();

                let watcher = Watcher {
                    downs: Arc::new(Mutex::new(Vec::new())),
                }
                .start();

                let r1 = watcher
                    .request(StartMonitor(target_handle.clone()))
                    .await
                    .unwrap();
                let r2 = watcher.request(StartMonitor(target_handle)).await.unwrap();
                assert_ne!(r1, r2);

                target.request(StopCounter).await.unwrap();
                target.join().await;
                let downs = crate::test_wait::wait_for_async(
                    || watcher.request(GetDowns),
                    |downs| downs.as_ref().map_or(true, |d| d.len() >= 2),
                )
                .await
                .unwrap();
                assert_eq!(downs.len(), 2);
                let refs: Vec<_> = downs.iter().map(|d| d.monitor_ref).collect();
                assert!(refs.contains(&r1));
                assert!(refs.contains(&r2));
            });
        },
    );
}

#[test]
pub fn monitor_table_is_cleaned_up_after_target_dies() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::monitor_table_is_cleaned_up_after_target_dies"
        ),
        move || {
            // Watcher should remove its entry from the monitor table after the
            // target dies, so the table doesn't accumulate stale entries.
            struct Inspect;
            impl Message for Inspect {
                type Result = usize;
            }
            impl Handler<Inspect> for Watcher {
                async fn handle(
                    &mut self,
                    _msg: Inspect,
                    reply: crate::reply::PendingReply<'_, usize>,
                    ctx: &Context<Self>,
                ) {
                    reply.reply(
                        ctx.shared
                            .monitors
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map_or(0, HashMap::len),
                    );
                }
            }

            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let target = Counter { count: 0 }.start();
                let watcher = Watcher {
                    downs: Arc::new(Mutex::new(Vec::new())),
                }
                .start();

                let _ = watcher
                    .request(StartMonitor(target.child_handle()))
                    .await
                    .unwrap();
                assert_eq!(watcher.request(Inspect).await.unwrap(), 1);

                target.request(StopCounter).await.unwrap();
                target.join().await;
                let entries = crate::test_wait::wait_for_async(
                    || watcher.request(Inspect),
                    |entries| !matches!(entries, Ok(n) if *n > 0),
                )
                .await;
                assert_eq!(
                    entries.unwrap(),
                    0,
                    "monitor table should be empty after target died"
                );
            });
        },
    );
}

#[test]
pub fn monitor_observes_panic_reason() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::monitor_observes_panic_reason"),
        move || {
            struct PanicMsg;
            impl Message for PanicMsg {
                type Result = ();
            }
            struct PanicMe;
            impl Actor for PanicMe {}
            impl Handler<PanicMsg> for PanicMe {
                async fn handle(
                    &mut self,
                    _msg: PanicMsg,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("intentional panic");
                    }
                }
            }

            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let target = PanicMe.start();
                let target_handle = target.child_handle();

                let watcher = Watcher {
                    downs: Arc::new(Mutex::new(Vec::new())),
                }
                .start();

                let _ = watcher.request(StartMonitor(target_handle)).await.unwrap();
                let _ = target.send(PanicMsg);

                let downs = crate::test_wait::wait_for_async(
                    || watcher.request(GetDowns),
                    |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
                )
                .await
                .unwrap();
                assert_eq!(downs.len(), 1);
                assert!(matches!(downs[0].reason, ExitReason::Panic(_)));
            });
        },
    );
}

#[test]
pub fn monitoring_actor_stops_before_target_does_not_panic() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::monitoring_actor_stops_before_target_does_not_panic"
        ),
        move || {
            // Regression: if the monitoring actor stops while the target is still
            // alive, the watcher must not crash when the target eventually dies.
            // The watcher's send() will fail silently (mailbox closed), and the
            // watcher exits cleanly.
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let target = Counter { count: 0 }.start();
                let target_handle = target.child_handle();

                let watcher = Watcher {
                    downs: Arc::new(Mutex::new(Vec::new())),
                }
                .start();

                let _ = watcher.request(StartMonitor(target_handle)).await.unwrap();

                // Stop the monitoring actor first
                let watcher_handle = watcher.child_handle();
                watcher_handle.stop();
                watcher_handle.wait_exit_async().await;

                // Now stop the target — the watcher should clean up without panicking
                target.request(StopCounter).await.unwrap();
                target.join().await;
                // Give the orphaned watcher task time to attempt delivery
                rt::sleep(Duration::from_millis(50)).await;
                // If we got here without panicking, the test passes.
            });
        },
    );
}

// --- Link tests ---

/// Trapping actor that records `Exit` notifications.
struct TrapActor {
    exits: Arc<Mutex<Vec<Exit>>>,
    trap: bool,
}

struct GetExits;
impl Message for GetExits {
    type Result = Vec<Exit>;
}
struct LinkTo(crate::ChildHandle);
impl Message for LinkTo {
    type Result = ();
}

impl Actor for TrapActor {
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.trap_exit(self.trap);
    }
    async fn exit_received(&mut self, exit: Exit, _ctx: &Context<Self>) {
        self.exits.lock().unwrap().push(exit);
    }
}

impl Handler<LinkTo> for TrapActor {
    async fn handle(
        &mut self,
        msg: LinkTo,
        reply: crate::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.link(&msg.0);
        reply.reply(());
    }
}

impl Handler<GetExits> for TrapActor {
    async fn handle(
        &mut self,
        _msg: GetExits,
        reply: crate::reply::PendingReply<'_, Vec<Exit>>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.exits.lock().unwrap().clone());
    }
}

fn make_trapper(trap: bool) -> ActorRef<TrapActor> {
    TrapActor {
        exits: Arc::new(Mutex::new(Vec::new())),
        trap,
    }
    .start()
}

#[test]
pub fn link_propagates_panic_to_non_trapping_peer() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::link_propagates_panic_to_non_trapping_peer"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                // Two non-trapping actors. When A panics, B should be terminated.
                let a = make_trapper(false);
                let b = make_trapper(false);
                a.request(LinkTo(b.child_handle())).await.unwrap();

                // Make A panic by dropping its only ActorRef while it's mid-handler... easier:
                // make A panic via a panic message
                struct Boom;
                impl Message for Boom {
                    type Result = ();
                }
                impl Handler<Boom> for TrapActor {
                    async fn handle(
                        &mut self,
                        _msg: Boom,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        {
                            panic!("boom");
                        }
                    }
                }

                let _ = a.send(Boom);
                // Both should die
                let reason_a = a.wait_exit().await;
                let reason_b = b.wait_exit().await;
                assert!(matches!(reason_a, ExitReason::Panic(_)));
                // B should propagate A's reason (transitive propagation)
                assert!(matches!(reason_b, ExitReason::Panic(_)));
            });
        },
    );
}

#[test]
pub fn link_delivers_exit_to_trapping_peer() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::link_delivers_exit_to_trapping_peer"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let a = make_trapper(false);
                let b = make_trapper(true); // B traps
                b.request(LinkTo(a.child_handle())).await.unwrap();

                struct Boom2;
                impl Message for Boom2 {
                    type Result = ();
                }
                impl Handler<Boom2> for TrapActor {
                    async fn handle(
                        &mut self,
                        _msg: Boom2,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        {
                            panic!("boom2");
                        }
                    }
                }

                let _ = a.send(Boom2);
                a.wait_exit().await;
                let exits = crate::test_wait::wait_for_async(
                    || b.request(GetExits),
                    |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                )
                .await
                .unwrap();

                // B should still be alive and have received an Exit
                assert!(b.exit_reason().is_none());
                assert_eq!(exits.len(), 1);
                assert_eq!(exits[0].from, a.id());
                assert!(matches!(exits[0].reason, ExitReason::Panic(_)));

                // Clean up
                let bh = b.child_handle();
                bh.stop();
                bh.wait_exit_async().await;
            });
        },
    );
}

#[test]
pub fn link_normal_exit_not_propagated_to_non_trapping_peer() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::link_normal_exit_not_propagated_to_non_trapping_peer"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let a = Counter { count: 0 }.start();
                let b = make_trapper(false);
                b.request(LinkTo(a.child_handle())).await.unwrap();

                // A stops cleanly
                a.request(StopCounter).await.unwrap();
                a.join().await;
                rt::sleep(Duration::from_millis(50)).await;

                // B should still be alive
                assert!(b.exit_reason().is_none());

                let bh = b.child_handle();
                bh.stop();
                bh.wait_exit_async().await;
            });
        },
    );
}

#[test]
pub fn link_to_already_dead_actor_delivers_noproc() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::link_to_already_dead_actor_delivers_noproc"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                // Start and panic a target
                struct Boom3;
                impl Message for Boom3 {
                    type Result = ();
                }
                impl Handler<Boom3> for TrapActor {
                    async fn handle(
                        &mut self,
                        _msg: Boom3,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        {
                            panic!("boom3");
                        }
                    }
                }
                let target = make_trapper(false);
                let _ = target.send(Boom3);
                target.wait_exit().await;

                // Now link from a trapping observer: the target is past its
                // death point, so the Exit carries NoProc (E4), not its reason.
                let observer = make_trapper(true);
                observer
                    .request(LinkTo(target.child_handle()))
                    .await
                    .unwrap();

                let exits = crate::test_wait::wait_for_async(
                    || observer.request(GetExits),
                    |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                )
                .await
                .unwrap();
                assert_eq!(exits.len(), 1);
                assert_eq!(exits[0].reason, ExitReason::NoProc);
                assert!(matches!(target.exit_reason(), Some(ExitReason::Panic(_))));

                let oh = observer.child_handle();
                oh.stop();
                oh.wait_exit_async().await;
            });
        },
    );
}

#[test]
pub fn unlink_prevents_signal_delivery() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::unlink_prevents_signal_delivery"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let a = Counter { count: 0 }.start();
                let b = make_trapper(true);
                b.request(LinkTo(a.child_handle())).await.unwrap();

                // Now unlink
                struct UnlinkFrom(crate::ChildHandle);
                impl Message for UnlinkFrom {
                    type Result = ();
                }
                impl Handler<UnlinkFrom> for TrapActor {
                    async fn handle(
                        &mut self,
                        msg: UnlinkFrom,
                        reply: crate::reply::PendingReply<'_, ()>,
                        ctx: &Context<Self>,
                    ) {
                        ctx.unlink(&msg.0);
                        reply.reply(());
                    }
                }
                b.request(UnlinkFrom(a.child_handle())).await.unwrap();

                // A panics
                struct Boom4;
                impl Message for Boom4 {
                    type Result = u64;
                }
                impl Handler<Boom4> for Counter {
                    async fn handle(
                        &mut self,
                        _msg: Boom4,
                        _reply: crate::reply::PendingReply<'_, u64>,
                        _ctx: &Context<Self>,
                    ) {
                        panic!("boom4");
                    }
                }
                // Counter's Boom4 returns u64; just use send with no expectation of reply
                let _ = a.send(Boom4);
                a.wait_exit().await;
                rt::sleep(Duration::from_millis(50)).await;

                // B should NOT have received an Exit
                let exits = b.request(GetExits).await.unwrap();
                assert!(exits.is_empty());

                let bh = b.child_handle();
                bh.stop();
                bh.wait_exit_async().await;
            });
        },
    );
}

#[test]
pub fn duplicate_link_is_idempotent() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::duplicate_link_is_idempotent"),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let a = Counter { count: 0 }.start();
                let b = make_trapper(true);
                b.request(LinkTo(a.child_handle())).await.unwrap();
                b.request(LinkTo(a.child_handle())).await.unwrap(); // duplicate

                struct Boom5;
                impl Message for Boom5 {
                    type Result = u64;
                }
                impl Handler<Boom5> for Counter {
                    async fn handle(
                        &mut self,
                        _msg: Boom5,
                        _reply: crate::reply::PendingReply<'_, u64>,
                        _ctx: &Context<Self>,
                    ) {
                        panic!("boom5");
                    }
                }
                let _ = a.send(Boom5);
                a.wait_exit().await;
                crate::test_wait::wait_for_async(
                    || b.request(GetExits),
                    |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                )
                .await
                .unwrap();
                // Leave room for a duplicate before checking there is none.
                rt::sleep(Duration::from_millis(50)).await;

                // Should receive only ONE Exit, not two
                let exits = b.request(GetExits).await.unwrap();
                assert_eq!(exits.len(), 1);

                let bh = b.child_handle();
                bh.stop();
                bh.wait_exit_async().await;
            });
        },
    );
}

#[test]
pub fn chain_propagation_through_non_trapping_middle() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::chain_propagation_through_non_trapping_middle"
        ),
        move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                // A linked to B linked to C. A panics → B dies (not trapping) →
                // B's death propagates to C.
                let a = make_trapper(false);
                let b = make_trapper(false);
                let c = make_trapper(true); // C traps

                a.request(LinkTo(b.child_handle())).await.unwrap();
                c.request(LinkTo(b.child_handle())).await.unwrap();

                struct Boom6;
                impl Message for Boom6 {
                    type Result = ();
                }
                impl Handler<Boom6> for TrapActor {
                    async fn handle(
                        &mut self,
                        _msg: Boom6,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        {
                            panic!("boom6");
                        }
                    }
                }
                let _ = a.send(Boom6);
                a.wait_exit().await;
                b.wait_exit().await;
                let exits = crate::test_wait::wait_for_async(
                    || c.request(GetExits),
                    |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                )
                .await
                .unwrap();
                assert_eq!(exits.len(), 1, "expected C to receive Exit from B's death");

                let ch = c.child_handle();
                ch.stop();
                ch.wait_exit_async().await;
            });
        },
    );
}

#[test]
pub fn link_to_already_dead_delivers_exactly_once_to_trapping_peer() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::link_to_already_dead_delivers_exactly_once_to_trapping_peer"
        ),
        move || {
            // Regression: previously, `ctx.link()` would deliver a duplicate Exit
            // to trapping actors when the target died concurrently — once from
            // propagate_exit (since register_link inserted self in target's table
            // before target's drain ran) and once from the fallback exit_reason()
            // check.
            struct Boom7;
            impl Message for Boom7 {
                type Result = ();
            }
            impl Handler<Boom7> for TrapActor {
                async fn handle(
                    &mut self,
                    _msg: Boom7,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("boom7");
                    }
                }
            }

            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let target = make_trapper(false);
                // Kick off the panic
                let _ = target.send(Boom7);
                // Wait for target to fully die so exit_reason() is Some
                target.wait_exit().await;

                // Now link from a trapping observer — should receive Exit EXACTLY ONCE
                let observer = make_trapper(true);
                observer
                    .request(LinkTo(target.child_handle()))
                    .await
                    .unwrap();

                crate::test_wait::wait_for_async(
                    || observer.request(GetExits),
                    |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                )
                .await
                .unwrap();
                // Leave room for a duplicate before checking there is none.
                rt::sleep(Duration::from_millis(50)).await;
                let exits = observer.request(GetExits).await.unwrap();
                assert_eq!(exits.len(), 1, "expected exactly one Exit, got {:?}", exits);

                let oh = observer.child_handle();
                oh.stop();
                oh.wait_exit_async().await;
            });
        },
    );
}

#[test]
pub fn start_linked_links_atomically() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::start_linked_links_atomically"),
        move || {
            // Parent + child via start_linked. When parent stops via panic,
            // child (not trapping) should die too.
            struct Parent;
            struct PanicParent;
            impl Message for PanicParent {
                type Result = ();
            }
            impl Actor for Parent {}
            impl Handler<PanicParent> for Parent {
                async fn handle(
                    &mut self,
                    _msg: PanicParent,
                    _reply: crate::reply::PendingReply<'_, ()>,
                    _ctx: &Context<Self>,
                ) {
                    {
                        panic!("parent boom");
                    }
                }
            }

            struct Child;
            impl Actor for Child {}

            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let parent = Parent.start();
                let child = Child.start_linked(&parent.context());

                // Parent panics — child should die too
                let _ = parent.send(PanicParent);
                parent.wait_exit().await;
                // Child should also die (link propagation)
                let reason = child.wait_exit().await;
                // Reason should be the parent's panic
                assert!(matches!(reason, ExitReason::Panic(_)));
            });
        },
    );
}
