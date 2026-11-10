// Not simulated: borrows a caller-owned multi-thread Tokio runtime.
#![cfg(all(feature = "async", not(spawned_sim)))]

use spawned_concurrency::{
    message::Message,
    system::{ActorSystem, SystemError},
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
    ExitReason,
};
use std::{thread::ThreadId, time::Duration};
const LIMIT: Duration = Duration::from_secs(30);
struct Worker;
struct Identity;
struct Ambient;
impl Message for Identity {
    type Result = ThreadId;
}
impl Message for Ambient {
    type Result = ActorRef<Worker>;
}
impl Actor for Worker {}
impl Handler<Identity> for Worker {
    async fn handle(
        &mut self,
        _: Identity,
        reply: spawned_concurrency::reply::PendingReply<'_, ThreadId>,
        _: &Context<Self>,
    ) {
        let __reply_value: ThreadId = { std::thread::current().id() };
        let _ = reply.reply(__reply_value);
    }
}
impl Handler<Ambient> for Worker {
    async fn handle(
        &mut self,
        _: Ambient,
        reply: spawned_concurrency::reply::PendingReply<'_, ActorRef<Worker>>,
        _: &Context<Self>,
    ) {
        let __reply_value: ActorRef<Worker> = { Worker.start() };
        let _ = reply.reply(__reply_value);
    }
}

#[test]
fn borrowed_groups_join_owned_work_and_leave_the_runtime_and_ambient_tasks_alive() {
    for current_thread in [true, false] {
        let mut builder = if current_thread {
            tokio::runtime::Builder::new_current_thread()
        } else {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        };
        let runtime = builder.enable_all().build().unwrap();
        runtime.block_on(async {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group_on("borrowed", runtime.handle()).unwrap();
            assert!(matches!(
                system.async_group_on("borrowed", runtime.handle()),
                Err(SystemError::DuplicateName(_))
            ));
            let actor = Worker.start_on(&group).unwrap();
            tokio::time::timeout(LIMIT, actor.ready())
                .await
                .unwrap()
                .unwrap();
            if current_thread {
                assert_eq!(
                    actor.request(Identity).await.unwrap(),
                    std::thread::current().id()
                );
            }
            let ambient = actor.request(Ambient).await.unwrap();
            let (entered, arrival) = tokio::sync::oneshot::channel();
            let (release, gate) = std::sync::mpsc::channel();
            let helper = actor
                .context()
                .spawn_blocking_helper(move || {
                    entered.send(()).unwrap();
                    gate.recv_timeout(LIMIT)
                        .expect("borrowed group helper not released");
                })
                .unwrap();
            tokio::time::timeout(LIMIT, arrival).await.unwrap().unwrap();
            let report = system.shutdown(Duration::from_millis(10)).await;
            assert!(!report.complete);
            assert_eq!(
                actor.child_handle().exit_reason(),
                Some(ExitReason::Shutdown)
            );
            release.send(()).unwrap();
            tokio::time::timeout(LIMIT, helper.join())
                .await
                .unwrap()
                .unwrap();
            assert!(report.wait(LIMIT).await.complete);
            assert!(matches!(Worker.start_on(&group), Err(SystemError::Closed)));
            // Ambient work spawned from the managed handler was never adopted.
            tokio::time::timeout(LIMIT, ambient.request(Identity))
                .await
                .unwrap()
                .unwrap();
            ambient.context().stop();
            tokio::time::timeout(LIMIT, ambient.join()).await.unwrap();
            // The external runtime is still usable after ActorSystem shutdown.
            assert_eq!(tokio::spawn(async { 42 }).await.unwrap(), 42);
        });
    }
}
