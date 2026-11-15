//! Await-free generated handlers without payload destructors use guarded direct dispatch. Public async
//! methods and direct Handler futures remain lazy; opaque awaits fall back.
#![cfg(feature = "async")]
use spawned_concurrency::{
    actor,
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler},
    ActorError, Deadline, ExitReason,
};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

struct Compute {
    value: u64,
}
impl Message for Compute {
    type Result = Result<u64, &'static str>;
}
fn __spawned_msg() -> u64 {
    3
}
fn __spawned_ctx() -> u64 {
    7
}
fn __spawned_reply() -> u64 {
    11
}
fn checked(value: u64) -> Result<u64, &'static str> {
    if value == u64::MAX {
        Err("too large")
    } else {
        Ok(value)
    }
}
struct Worker {
    calls: Arc<AtomicUsize>,
}
#[actor]
impl Worker {
    #[request_handler]
    async fn compute(
        &mut self,
        Compute { value }: Compute,
        _context: &Context<Self>,
    ) -> Result<u64, &'static str> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if value == 0 {
            return Ok(__spawned_reply());
        }
        let value = checked(value)?;
        Ok(value + __spawned_msg() + __spawned_ctx() + __spawned_reply())
    }
}

#[spawned_macros::sim_test]
async fn direct_dispatch_keeps_patterns_returns_question_mark_and_name_hygiene() {
    let calls = Arc::new(AtomicUsize::new(0));
    let actor = Worker {
        calls: calls.clone(),
    }
    .start();
    assert_eq!(actor.request(Compute { value: 2 }).await.unwrap(), Ok(23));
    assert_eq!(actor.request(Compute { value: 0 }).await.unwrap(), Ok(11));
    assert_eq!(
        actor.request(Compute { value: u64::MAX }).await.unwrap(),
        Err("too large")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    actor.context().stop();
    actor.join().await;
}

#[spawned_macros::sim_test]
async fn inherent_methods_and_public_handler_futures_stay_lazy() {
    let calls = Arc::new(AtomicUsize::new(0));
    let actor = Worker {
        calls: calls.clone(),
    }
    .start();
    let context = actor.context();
    let mut direct = Worker {
        calls: calls.clone(),
    };
    let inherent = direct.compute(Compute { value: 1 }, &context);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(inherent.await, Ok(22));
    let future = <Worker as Handler<Compute>>::handle(
        &mut direct,
        Compute { value: 1 },
        spawned_test::detached_reply(),
        &context,
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    future.await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    actor.context().stop();
    actor.join().await;
}

#[spawned_macros::sim_test]
async fn eligible_handler_uses_the_immediate_hook() {
    let calls = Arc::new(AtomicUsize::new(0));
    let actor = Worker {
        calls: calls.clone(),
    }
    .start();
    let context = actor.context();
    let mut direct = Worker {
        calls: calls.clone(),
    };
    assert!(<Worker as Handler<Compute>>::__spawned_handle_immediate(
        &mut direct,
        Compute { value: 1 },
        spawned_test::detached_reply(),
        &context,
    )
    .is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    actor.context().stop();
    actor.join().await;
}

#[cfg(not(spawned_sim))]
mod local_items {
    use super::*;
    struct ItemCounter;
    impl Message for ItemCounter {
        type Result = usize;
    }
    struct NestedItemCounter;
    impl Message for NestedItemCounter {
        type Result = usize;
    }
    struct ItemWorker;
    struct ClosureIdentity;
    impl Message for ClosureIdentity {
        type Result = std::any::TypeId;
    }
    struct AsyncIdentity;
    impl Message for AsyncIdentity {
        type Result = std::any::TypeId;
    }
    fn identity<T: 'static>(_: T) -> std::any::TypeId {
        std::any::TypeId::of::<T>()
    }
    #[actor]
    impl ItemWorker {
        #[request_handler]
        async fn count(&mut self, _msg: ItemCounter, _ctx: &Context<Self>) -> usize {
            static CALLS: AtomicUsize = AtomicUsize::new(0);
            CALLS.fetch_add(1, Ordering::SeqCst)
        }

        #[request_handler]
        async fn nested(&mut self, _msg: NestedItemCounter, _ctx: &Context<Self>) -> usize {
            fn next() -> usize {
                static CALLS: AtomicUsize = AtomicUsize::new(0);
                CALLS.fetch_add(1, Ordering::SeqCst)
            }
            next()
        }

        #[request_handler]
        async fn closure(
            &mut self,
            _msg: ClosureIdentity,
            _ctx: &Context<Self>,
        ) -> std::any::TypeId {
            identity(|| ())
        }

        #[request_handler]
        async fn nested_async(
            &mut self,
            _msg: AsyncIdentity,
            _ctx: &Context<Self>,
        ) -> std::any::TypeId {
            identity(async {})
        }
    }

    // Process-local statics intentionally cross the direct/mailbox boundary. This
    // test uses the real runtime: statics are not reset between simulator seeds.
    #[tokio::test]
    async fn local_items_have_one_identity_across_direct_and_mailbox_calls() {
        let actor = ItemWorker.start();
        let context = actor.context();
        let mut direct = ItemWorker;
        assert_eq!(direct.count(ItemCounter, &context).await, 0);
        assert_eq!(actor.request(ItemCounter).await.unwrap(), 1);
        assert_eq!(direct.count(ItemCounter, &context).await, 2);
        assert_eq!(direct.nested(NestedItemCounter, &context).await, 0);
        assert_eq!(actor.request(NestedItemCounter).await.unwrap(), 1);
        assert_eq!(direct.nested(NestedItemCounter, &context).await, 2);
        assert_eq!(
            direct.closure(ClosureIdentity, &context).await,
            actor.request(ClosureIdentity).await.unwrap()
        );
        assert_eq!(
            direct.nested_async(AsyncIdentity, &context).await,
            actor.request(AsyncIdentity).await.unwrap()
        );
        actor.context().stop();
        actor.join().await;
    }
}

#[cfg(not(spawned_sim))]
mod fairness {
    use super::*;

    struct Tick;
    impl Message for Tick {
        type Result = ();
    }
    struct Flood {
        calls: Arc<AtomicUsize>,
        observed: Arc<AtomicUsize>,
    }
    fn spawn_probe(calls: Arc<AtomicUsize>, observed: Arc<AtomicUsize>) {
        spawned_rt::tasks::spawn(async move {
            observed.store(calls.load(Ordering::SeqCst), Ordering::SeqCst);
        });
    }
    #[actor]
    impl Flood {
        #[request_handler]
        async fn tick(&mut self, _msg: Tick, _ctx: &Context<Self>) {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                spawn_probe(self.calls.clone(), self.observed.clone());
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn immediate_dispatch_keeps_mailbox_cooperative_budget() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(AtomicUsize::new(usize::MAX));
        let actor = Flood {
            calls,
            observed: observed.clone(),
        }
        .start();
        for _ in 0..1024 {
            actor.send(Tick).unwrap();
        }
        actor.request(Tick).await.unwrap();
        tokio::task::yield_now().await;
        assert!(
            observed.load(Ordering::SeqCst) < 1024,
            "a flooded immediate handler starved a runnable task"
        );
        actor.context().stop();
        actor.join().await;
    }
}

macro_rules! opaque_await {
    () => {
        spawned_rt::tasks::yield_now().await
    };
}
struct Yielding;
impl Message for Yielding {
    type Result = u64;
}
struct Opaque;
#[actor]
impl Opaque {
    #[request_handler]
    async fn via_macro(&mut self, _m: Yielding, _ctx: &Context<Self>) -> u64 {
        opaque_await!();
        9
    }
}
#[spawned_macros::sim_test]
async fn opaque_macros_can_still_suspend() {
    let actor = Opaque.start();
    assert_eq!(actor.request(Yielding).await.unwrap(), 9);
    actor.context().stop();
    actor.join().await;
}

struct Explode;
impl Message for Explode {
    type Result = ();
}
struct After;
impl Message for After {
    type Result = ();
}
struct Panicker {
    stopped: Arc<AtomicBool>,
}
#[actor]
impl Panicker {
    #[request_handler]
    async fn explode(&mut self, _m: Explode, _ctx: &Context<Self>) {
        std::panic::panic_any("immediate handler panic");
    }
    #[request_handler]
    async fn after(&mut self, _m: After, _ctx: &Context<Self>) {}
    #[stopped]
    async fn on_stop(&mut self, _ctx: &Context<Self>) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}
#[spawned_macros::sim_test]
async fn direct_dispatch_panic_answers_queued_callers_and_runs_stopped() {
    let stopped = Arc::new(AtomicBool::new(false));
    let actor = Panicker {
        stopped: stopped.clone(),
    }
    .start();
    let first = actor.send_request(Explode, Deadline::Never).unwrap();
    let queued = actor.send_request(After, Deadline::Never).unwrap();
    for result in [first.await, queued.await] {
        assert!(
            matches!(result, Err(ActorError::ActorStopped(ExitReason::Panic(ref reason))) if reason == "panic in handler: immediate handler panic"),
            "wrong direct-dispatch outcome: {result:?}"
        );
    }
    actor.join().await;
    assert!(stopped.load(Ordering::SeqCst));
}

// The generated hook's lifetime must not shadow an impl lifetime.
struct Generic<'__spawned_reply_turn: 'static>(std::marker::PhantomData<&'__spawned_reply_turn ()>);
struct GenericGet;
impl Message for GenericGet {
    type Result = u64;
}
#[actor]
impl<'__spawned_reply_turn: 'static> Generic<'__spawned_reply_turn> {
    #[request_handler]
    async fn get(&mut self, _m: GenericGet, _ctx: &Context<Self>) -> u64 {
        4
    }
}
#[spawned_macros::sim_test]
async fn generic_lifetimes_do_not_shadow_generated_lifetimes() {
    let actor = Generic::<'static>(std::marker::PhantomData).start();
    assert_eq!(actor.request(GenericGet).await.unwrap(), 4);
    actor.context().stop();
    actor.join().await;
}

struct SharedRead;
impl Message for SharedRead {
    type Result = usize;
}
struct ViewRead;
impl Message for ViewRead {
    type Result = u64;
}
trait View: Sync {
    fn id(&self) -> u64;
}
impl<A: Actor> View for Context<A> {
    fn id(&self) -> u64 {
        123
    }
}
// Separate actor block would implement Actor twice, so these methods use a
// separate type to exercise shared receivers and context coercions together.
struct BorrowViews(Arc<AtomicUsize>);
#[actor]
impl BorrowViews {
    #[request_handler]
    async fn shared(&self, _m: SharedRead, _ctx: &Context<Self>) -> usize {
        let alias = self;
        alias.0.load(Ordering::SeqCst) + self.0.load(Ordering::SeqCst)
    }
    #[request_handler]
    async fn viewed(&mut self, _m: ViewRead, context: &dyn View) -> u64 {
        context.id()
    }
}
#[spawned_macros::sim_test]
async fn shared_receiver_and_declared_context_types_keep_their_borrow_rules() {
    let actor = BorrowViews(Arc::new(AtomicUsize::new(4))).start();
    assert_eq!(actor.request(SharedRead).await.unwrap(), 8);
    assert_eq!(actor.request(ViewRead).await.unwrap(), 123);
    actor.context().stop();
    actor.join().await;
}

struct DropOrder {
    body_ran: Arc<AtomicBool>,
    observed: Arc<AtomicBool>,
    panic: bool,
}
impl Drop for DropOrder {
    fn drop(&mut self) {
        self.observed
            .store(self.body_ran.load(Ordering::SeqCst), Ordering::SeqCst);
        if self.panic {
            std::panic::panic_any("ignored parameter destructor");
        }
    }
}
struct Ignored(
    #[expect(
        dead_code,
        reason = "the ignored field must stay alive through dispatch"
    )]
    DropOrder,
);
impl Message for Ignored {
    type Result = ();
}
struct Partial {
    keep: Arc<AtomicBool>,
    _ignored: DropOrder,
}
impl Message for Partial {
    type Result = ();
}
struct DropWorker {
    body_ran: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}
#[actor]
impl DropWorker {
    #[request_handler]
    async fn wildcard(&mut self, _: Ignored, _: &Context<Self>) {
        self.body_ran.store(true, Ordering::SeqCst);
    }
    #[request_handler]
    async fn partial(&mut self, Partial { keep, .. }: Partial, _: &Context<Self>) {
        keep.store(true, Ordering::SeqCst);
    }
    #[stopped]
    async fn on_stop(&mut self, _: &Context<Self>) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}
#[spawned_macros::sim_test]
async fn ignored_parameters_drop_after_the_body_in_mailbox_and_inherent_calls() {
    for partial in [false, true] {
        let body_ran = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let actor = DropWorker {
            body_ran: body_ran.clone(),
            stopped: stopped.clone(),
        }
        .start();
        let context = actor.context();
        let mut direct = DropWorker {
            body_ran: body_ran.clone(),
            stopped,
        };
        let guard = || DropOrder {
            body_ran: body_ran.clone(),
            observed: observed.clone(),
            panic: false,
        };
        if partial {
            direct
                .partial(
                    Partial {
                        keep: body_ran.clone(),
                        _ignored: guard(),
                    },
                    &context,
                )
                .await;
        } else {
            direct.wildcard(Ignored(guard()), &context).await;
        }
        assert!(
            observed.load(Ordering::SeqCst),
            "inherent method dropped a parameter early"
        );
        body_ran.store(false, Ordering::SeqCst);
        observed.store(false, Ordering::SeqCst);
        if partial {
            actor
                .request(Partial {
                    keep: body_ran.clone(),
                    _ignored: guard(),
                })
                .await
                .unwrap();
        } else {
            actor.request(Ignored(guard())).await.unwrap();
        }
        assert!(
            observed.load(Ordering::SeqCst),
            "dispatch dropped a parameter before its body"
        );
        actor.context().stop();
        actor.join().await;
    }
}
#[spawned_macros::sim_test]
async fn ignored_parameter_destructor_panics_after_body_and_remains_contained() {
    let body_ran = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let actor = DropWorker {
        body_ran: body_ran.clone(),
        stopped: stopped.clone(),
    }
    .start();
    let result = actor
        .request(Ignored(DropOrder {
            body_ran,
            observed: observed.clone(),
            panic: true,
        }))
        .await;
    assert!(
        matches!(result, Err(ActorError::ActorStopped(ExitReason::Panic(ref reason))) if reason.contains("ignored parameter destructor")),
        "wrong panic result: {result:?}"
    );
    actor.join().await;
    assert!(observed.load(Ordering::SeqCst));
    assert!(stopped.load(Ordering::SeqCst));
}

#[spawned_macros::sim_test]
async fn payload_destructors_keep_the_original_future_drop_boundary() {
    let body_ran = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicBool::new(false));
    let actor = DropWorker {
        body_ran: body_ran.clone(),
        stopped: Arc::new(AtomicBool::new(false)),
    }
    .start();
    let mut direct = DropWorker {
        body_ran: body_ran.clone(),
        stopped: Arc::new(AtomicBool::new(false)),
    };
    let fallback = <DropWorker as Handler<Ignored>>::__spawned_handle_immediate(
        &mut direct,
        Ignored(DropOrder {
            body_ran: body_ran.clone(),
            observed: observed.clone(),
            panic: false,
        }),
        spawned_test::detached_reply(),
        &actor.context(),
    );
    assert!(
        fallback.is_some(),
        "payload with drop glue bypassed the future fence"
    );
    assert!(!body_ran.load(Ordering::SeqCst));
    let (message, reply) = fallback.unwrap();
    <DropWorker as Handler<Ignored>>::handle(&mut direct, message, reply, &actor.context()).await;
    assert!(observed.load(Ordering::SeqCst));
    actor.context().stop();
    actor.join().await;
}

mod shadowed_prelude {
    use super::*;
    struct Option;
    enum State {
        Some,
        None,
    }
    use State::{None, Some};
    mod std {}
    pub(super) struct Shadow;
    impl Message for Shadow {
        type Result = u64;
    }
    pub(super) struct ShadowActor;
    #[actor]
    impl ShadowActor {
        #[request_handler]
        async fn read(&mut self, _: Shadow, _: &Context<Self>) -> u64 {
            let _: Option = Option;
            let _ = (None, Some);
            19
        }
    }
}
#[spawned_macros::sim_test]
async fn generated_paths_do_not_capture_shadowed_prelude_names() {
    let actor = shadowed_prelude::ShadowActor.start();
    assert_eq!(actor.request(shadowed_prelude::Shadow).await.unwrap(), 19);
    actor.context().stop();
    actor.join().await;
}

struct Discard;
impl Message for Discard {
    type Result = ();
}
struct Discarder;
impl Actor for Discarder {}
impl Handler<Discard> for Discarder {
    fn __spawned_handle_immediate<'turn>(
        &mut self,
        _: Discard,
        reply: spawned_concurrency::PendingReply<'turn, ()>,
        _: &Context<Self>,
    ) -> Option<(Discard, spawned_concurrency::PendingReply<'turn, ()>)> {
        drop(reply);
        None
    }
    async fn handle(
        &mut self,
        _: Discard,
        _: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        std::panic::panic_any("direct adapter unexpectedly used the future");
    }
}
#[spawned_macros::sim_test]
async fn direct_dispatch_commits_an_unanswered_obligation_on_normal_return() {
    let actor = Discarder.start();
    assert!(matches!(
        actor.request(Discard).await,
        Err(ActorError::ReplyDropped)
    ));
    assert!(actor.exit_reason().is_none());
    actor.context().stop();
    actor.join().await;
}
struct RejectedResult(Arc<AtomicBool>);
impl Drop for RejectedResult {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        std::panic::panic_any("rejected result destructor");
    }
}
struct GetRejected;
impl Message for GetRejected {
    type Result = RejectedResult;
}
struct ResultWorker {
    dropped: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}
#[actor]
impl ResultWorker {
    #[request_handler]
    async fn get(&mut self, _: GetRejected, _: &Context<Self>) -> RejectedResult {
        RejectedResult(self.dropped.clone())
    }
    #[stopped]
    async fn on_stop(&mut self, _: &Context<Self>) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}
#[spawned_macros::sim_test]
async fn a_rejected_result_destructor_has_the_same_handler_panic_boundary() {
    let dropped = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let actor = ResultWorker {
        dropped: dropped.clone(),
        stopped: stopped.clone(),
    }
    .start();
    let receiver = actor.send_request(GetRejected, Deadline::Never).unwrap();
    drop(receiver);
    actor.join().await;
    assert!(
        matches!(actor.exit_reason(), Some(ExitReason::Panic(reason)) if reason == "panic in handler: rejected result destructor")
    );
    assert!(dropped.load(Ordering::SeqCst));
    assert!(stopped.load(Ordering::SeqCst));
}
