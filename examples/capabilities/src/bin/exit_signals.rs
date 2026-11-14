//! Exit signals as in OTP (docs/GUIDE.md, "Exit signals"): `exit` vs `kill`,
//! trapping, `NoProc` for registrations and sends after death, and
//! `exit_pending` for work queued before a supervisor's shutdown.
use spawned_concurrency::{
    message::Message,
    reply::PendingReply,
    supervision::{ChildSpec, SupervisorConfig},
    system::ActorSystem,
    tasks::{Actor, ActorStart, Context, Handler},
    ActorError, ChildHandle, Down, Exit, ExitReason, NoProc,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// What an actor observed.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);
impl Seen {
    fn push(&self, entry: String) {
        self.0.lock().unwrap().push(entry);
    }
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

struct Worker {
    trap: bool,
    seen: Seen,
}
impl Worker {
    fn new(trap: bool, seen: &Seen) -> Self {
        Self {
            trap,
            seen: seen.clone(),
        }
    }
}
impl Actor for Worker {
    async fn started(&mut self, ctx: &Context<Self>) {
        // Trap first thing: signals that arrive during started() are
        // resolved with the flag as it is when started() returns.
        ctx.trap_exit(self.trap);
    }
    async fn stopped(&mut self, ctx: &Context<Self>) {
        self.seen
            .push(format!("stopped (exit_pending: {})", ctx.exit_pending()));
    }
    async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
        self.seen.push(format!("Exit {:?}", exit.reason));
    }
}
impl Handler<Down> for Worker {
    async fn handle(&mut self, down: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        self.seen.push(format!("Down {:?}", down.reason));
        reply.reply(());
    }
}

/// Link to and monitor a peer.
struct Watch(ChildHandle);
impl Message for Watch {
    type Result = ();
}
impl Handler<Watch> for Worker {
    async fn handle(&mut self, msg: Watch, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        ctx.link(&msg.0);
        ctx.monitor(&msg.0);
        reply.reply(());
    }
}

/// `try_link`: `Err(NoProc)` instead of a `NoProc` exit signal.
struct TryLink(ChildHandle);
impl Message for TryLink {
    type Result = Result<(), NoProc>;
}
impl Handler<TryLink> for Worker {
    async fn handle(
        &mut self,
        msg: TryLink,
        reply: PendingReply<'_, Result<(), NoProc>>,
        ctx: &Context<Self>,
    ) {
        reply.reply(ctx.try_link(&msg.0));
    }
}

/// Work that refuses to start once the actor's exit is pending.
struct Work(u32);
impl Message for Work {
    type Result = ();
}
impl Handler<Work> for Worker {
    async fn handle(&mut self, msg: Work, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        let pending = ctx.exit_pending();
        self.seen
            .push(format!("work {} (exit_pending: {pending})", msg.0));
        reply.reply(());
    }
}

/// Busy until the actor's exit is pending (bounded), standing in for a long
/// handler that is running when the supervisor shuts down.
struct Busy;
impl Message for Busy {
    type Result = ();
}
impl Handler<Busy> for Worker {
    async fn handle(&mut self, _: Busy, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        for _ in 0..1000 {
            if ctx.exit_pending() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        reply.reply(());
    }
}

/// A round trip: everything queued before it was handled.
struct Sync;
impl Message for Sync {
    type Result = ();
}
impl Handler<Sync> for Worker {
    async fn handle(&mut self, _: Sync, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        reply.reply(());
    }
}

const LIMIT: Duration = Duration::from_secs(5);

async fn ended(handle: &ChildHandle) -> ExitReason {
    tokio::time::timeout(LIMIT, handle.wait_exit_async())
        .await
        .expect("actor did not exit")
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    // 1. exit(reason) follows OTP's exit_signal/2: Normal is ignored by a
    //    non-trapping actor, any other reason ends it without stopped().
    let seen = Seen::default();
    let plain = Worker::new(false, &seen).start();
    plain.request(Sync).await.unwrap();
    plain.child_handle().exit(ExitReason::Normal);
    plain.request(Sync).await.unwrap();
    assert!(plain.child_handle().is_alive(), "exit(Normal) is ignored");
    plain.child_handle().exit(ExitReason::Shutdown);
    assert_eq!(ended(&plain.child_handle()).await, ExitReason::Shutdown);
    assert!(
        seen.take().is_empty(),
        "an untrapped signal skips stopped()"
    );
    println!("exit(Normal) ignored; exit(Shutdown) ended the actor without stopped()");

    // 2. A trapping actor receives exit signals as `Exit` messages; only a
    //    kill is untrappable and ends it `Killed`, also without stopped().
    //    An administrative stop (`stop`, `shutdown`) always runs stopped().
    let (observer_seen, target_seen) = (Seen::default(), Seen::default());
    let observer = Worker::new(true, &observer_seen).start();
    let target = Worker::new(true, &target_seen).start();
    observer
        .request(Watch(target.child_handle()))
        .await
        .unwrap();
    target.child_handle().exit(ExitReason::Shutdown);
    target.request(Sync).await.unwrap();
    assert_eq!(target_seen.take(), vec!["Exit Shutdown"]);
    target.child_handle().kill();
    assert_eq!(ended(&target.child_handle()).await, ExitReason::Killed);
    assert!(target_seen.take().is_empty(), "a kill skips stopped()");
    // The trapping observer gets the link notice and the monitor's Down.
    tokio::time::sleep(Duration::from_millis(50)).await;
    observer.request(Sync).await.unwrap();
    let mut notices = observer_seen.take();
    notices.sort();
    assert_eq!(notices, vec!["Down Killed", "Exit Killed"]);
    println!("trapped Exit(Shutdown); kill() ended a trapping actor Killed; peers saw Killed");

    // 3. Past its death point an actor is gone: link and monitor get NoProc
    //    (one notice each), try_link returns Err(NoProc), sends fail NoProc.
    observer
        .request(Watch(target.child_handle()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    observer.request(Sync).await.unwrap();
    let mut notices = observer_seen.take();
    notices.sort();
    assert_eq!(notices, vec!["Down NoProc", "Exit NoProc"]);
    let tried = observer.request(TryLink(target.child_handle())).await;
    assert_eq!(tried.unwrap(), Err(NoProc));
    assert!(matches!(target.send(Sync), Err(ActorError::NoProc)));
    // The real reason is still available from the handle.
    assert_eq!(
        target.child_handle().exit_reason(),
        Some(ExitReason::Killed)
    );
    // A non-trapping actor that links to a dead one exits NoProc itself.
    let linker = Worker::new(false, &Seen::default()).start();
    linker.request(Watch(target.child_handle())).await.unwrap();
    assert_eq!(ended(&linker.child_handle()).await, ExitReason::NoProc);
    observer.child_handle().stop();
    ended(&observer.child_handle()).await;
    assert_eq!(observer_seen.take(), vec!["stopped (exit_pending: true)"]);
    println!("late link/monitor gave NoProc, try_link Err(NoProc), send NoProc");

    // 4. A supervisor's shutdown is its parent's exit signal. A trapping
    //    child handles the work queued before it, sees `exit_pending()`, then
    //    runs stopped(); a non-trapping child would end without stopped().
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("exit-signals", 1).unwrap();
    let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
    let child_seen = Seen::default();
    let s = child_seen.clone();
    let child = tree
        .add(ChildSpec::new("trapping", &group, move |_| {
            let seen = s.clone();
            async move { Ok::<_, String>(Worker::new(true, &seen)) }
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let worker = child.current().unwrap();
    worker.request(Work(1)).await.unwrap();
    // Work queued behind a busy handler, then the shutdown: the parent exit
    // queues behind that work.
    worker.send(Busy).unwrap();
    worker.send(Work(2)).unwrap();
    let handle = worker.child_handle();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert_eq!(ended(&handle).await, ExitReason::Shutdown);
    assert_eq!(
        child_seen.take(),
        vec![
            "work 1 (exit_pending: false)",
            "work 2 (exit_pending: true)",
            "stopped (exit_pending: true)",
        ]
    );
    assert!(system.shutdown(LIMIT).await.complete);
    println!("supervisor shutdown: queued work saw exit_pending, then stopped() ran");
}
