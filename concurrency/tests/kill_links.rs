use spawned_concurrency::{message::Message, ActorId, Exit, ExitReason};
use std::{sync::mpsc, time::Duration};
const LIMIT: Duration = Duration::from_secs(10);
struct Ping;
impl Message for Ping {
    type Result = u8;
}
fn verify_notices(events: Vec<Exit>, direct: ActorId, propagated: ActorId) {
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| event.reason == ExitReason::Kill));
    assert!(events.iter().any(|event| event.from == direct));
    assert!(events.iter().any(|event| event.from == propagated));
}

#[test]
fn native_propagated_kill_is_trappable_but_direct_kill_is_not() {
    use spawned_concurrency::threads::{Actor, ActorRef, ActorStart, Context, Handler};
    struct Node(mpsc::Sender<Exit>);
    impl Actor for Node {
        fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.0.send(exit).unwrap();
        }
    }
    impl Handler<Ping> for Node {
        fn handle(&mut self, _: Ping, reply: spawned_concurrency::reply::PendingReply<'_, u8>, _: &Context<Self>) {
            let __reply_value: u8 = {
            7
        };
            let _ = reply.reply(__reply_value);
        }
    }
    fn wait(actor: &ActorRef<Node>) -> ExitReason {
        let (send, recv) = mpsc::channel();
        let child = actor.child_handle();
        std::thread::spawn(move || send.send(child.wait_exit_blocking()).unwrap());
        recv.recv_timeout(LIMIT).unwrap()
    }
    let (send, events) = mpsc::channel();
    let a = Node(send.clone()).start();
    let b = Node(send.clone()).start();
    let c = Node(send.clone()).start();
    let d = Node(send).start();
    b.context().link(&a.child_handle());
    c.context().trap_exit(true);
    c.context().link(&b.child_handle());
    d.context().trap_exit(true);
    d.context().link(&a.child_handle());
    a.child_handle().stop_with_reason(ExitReason::Kill);
    assert_eq!(wait(&a), ExitReason::Kill);
    assert_eq!(wait(&b), ExitReason::Kill);
    verify_notices(
        vec![
            events.recv_timeout(LIMIT).unwrap(),
            events.recv_timeout(LIMIT).unwrap(),
        ],
        a.id(),
        b.id(),
    );
    assert_eq!(c.request(Ping).unwrap(), 7);
    assert_eq!(d.request(Ping).unwrap(), 7);
    d.child_handle().stop_with_reason(ExitReason::Kill);
    assert_eq!(wait(&d), ExitReason::Kill);
    assert_eq!(c.request(Ping).unwrap(), 7);
    c.child_handle().stop();
    assert_eq!(wait(&c), ExitReason::Normal);
    assert!(matches!(
        events.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
}

// Not simulated: observes the links from spawn_blocking on OS threads.
#[cfg(all(feature = "async", not(spawned_sim)))]
#[tokio::test]
async fn async_propagated_kill_is_trappable_but_direct_kill_is_not() {
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    struct Node(mpsc::Sender<Exit>);
    impl Actor for Node {
        async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.0.send(exit).unwrap();
        }
    }
    impl Handler<Ping> for Node {
        async fn handle(&mut self, _: Ping, reply: spawned_concurrency::reply::PendingReply<'_, u8>, _: &Context<Self>) {
            let __reply_value: u8 = {
            7
        };
            let _ = reply.reply(__reply_value);
        }
    }
    let (send, events) = mpsc::channel();
    let a = Node(send.clone()).start();
    let b = Node(send.clone()).start();
    let c = Node(send.clone()).start();
    let d = Node(send).start();
    b.context().link(&a.child_handle());
    c.context().trap_exit(true);
    c.context().link(&b.child_handle());
    d.context().trap_exit(true);
    d.context().link(&a.child_handle());
    a.child_handle().stop_with_reason(ExitReason::Kill);
    assert_eq!(
        tokio::time::timeout(LIMIT, a.wait_exit()).await.unwrap(),
        ExitReason::Kill
    );
    assert_eq!(
        tokio::time::timeout(LIMIT, b.wait_exit()).await.unwrap(),
        ExitReason::Kill
    );
    let (observed, events) = tokio::task::spawn_blocking(move || {
        (
            vec![
                events.recv_timeout(LIMIT).unwrap(),
                events.recv_timeout(LIMIT).unwrap(),
            ],
            events,
        )
    })
    .await
    .unwrap();
    verify_notices(observed, a.id(), b.id());
    assert_eq!(c.request(Ping).await.unwrap(), 7);
    assert_eq!(d.request(Ping).await.unwrap(), 7);
    d.child_handle().stop_with_reason(ExitReason::Kill);
    assert_eq!(
        tokio::time::timeout(LIMIT, d.wait_exit()).await.unwrap(),
        ExitReason::Kill
    );
    assert_eq!(c.request(Ping).await.unwrap(), 7);
    c.child_handle().stop();
    assert_eq!(
        tokio::time::timeout(LIMIT, c.wait_exit()).await.unwrap(),
        ExitReason::Normal
    );
    assert!(matches!(
        events.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
}
