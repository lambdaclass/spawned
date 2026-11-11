//! Synchronous monitors, demonitoring, trapped exits and atomic linked startup.
use spawned_concurrency::{
    actor,
    message::Message,
    threads::{Actor, ActorStart, Context, Handler},
    Down, Exit, ExitReason,
};
use std::{sync::mpsc, time::Duration};
const LIMIT: Duration = Duration::from_secs(2);
struct Observer {
    exits: mpsc::Sender<Exit>,
    downs: mpsc::Sender<Down>,
    counts: (usize, usize),
}
impl Actor for Observer {
    fn exit_received(&mut self, event: Exit, _: &Context<Self>) {
        self.counts.0 += 1;
        self.exits.send(event).unwrap();
    }
}
impl Handler<Down> for Observer {
    fn handle(
        &mut self,
        event: Down,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.counts.1 += 1;
        self.downs.send(event).unwrap();
        reply.reply(());
    }
}
struct Counts;
impl Message for Counts {
    type Result = (usize, usize);
}
impl Handler<Counts> for Observer {
    fn handle(
        &mut self,
        _: Counts,
        reply: spawned_concurrency::reply::PendingReply<'_, (usize, usize)>,
        _: &Context<Self>,
    ) {
        reply.reply(self.counts);
    }
}
struct Failing;
#[actor]
impl Failing {
    #[started]
    fn started(&mut self, _: &Context<Self>) {
        panic!("intentional linked startup failure");
    }
}
struct Idle;
impl Actor for Idle {}
fn main() {
    let (exits, exit_rx) = mpsc::channel();
    let (downs, down_rx) = mpsc::channel();
    let observer = Observer {
        exits,
        downs,
        counts: (0, 0),
    }
    .start();
    observer.context().trap_exit(true);
    let failed = Failing.start_linked(&observer.context());
    let monitor = observer.context().monitor(&failed.child_handle());
    let exit = exit_rx.recv_timeout(LIMIT).unwrap();
    let down = down_rx.recv_timeout(LIMIT).unwrap();
    assert_eq!(exit.from, failed.id());
    assert_eq!(down.monitor_ref, monitor);
    assert!(matches!(exit.reason, ExitReason::Panic(_)));
    assert!(matches!(down.reason, ExitReason::Panic(_)));
    assert_eq!(observer.request(Counts).unwrap(), (1, 1));
    failed.join();
    let idle = Idle.start();
    let watch = observer.context().monitor(&idle.child_handle());
    observer.context().demonitor(watch);
    idle.context().stop();
    idle.join();
    assert_eq!(observer.request(Counts).unwrap(), (1, 1));
    observer.context().stop();
    observer.join();
    // Without trap_exit, a linked abnormal exit stops the peer as well.
    let peer = Idle.start();
    let failed = Failing.start_linked(&peer.context());
    assert!(matches!(peer.wait_exit(), ExitReason::Panic(_)));
    failed.join();
    peer.join();
    println!("Native monitor/link notifications preserve reasons; trapped peers keep running.");
}
