//! Synchronous monitors, demonitoring, trapped exits and atomic linked startup.
use spawned_concurrency::{
    actor,
    message::Message,
    threads::{Actor, ActorStart, Context, Handler, Init},
    Down, Exit, ExitReason, StartError,
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
        reply: spawned_concurrency::PendingReply<'_, ()>,
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
        reply: spawned_concurrency::PendingReply<'_, (usize, usize)>,
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
/// Ready, then fails in its continuation (before any message).
struct FailsAfterStart;
struct Boom;
impl Message for Boom {
    type Result = ();
}
impl Actor for FailsAfterStart {
    #[allow(refining_impl_trait)]
    fn started(&mut self, _: &Context<Self>) -> Init<Self> {
        Init::continue_with(Boom)
    }
}
impl Handler<Boom> for FailsAfterStart {
    fn handle(&mut self, _: Boom, _: spawned_concurrency::PendingReply<'_, ()>, _: &Context<Self>) {
        panic!("intentional failure after start");
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
    .spawn();
    observer.context().trap_exit(true);
    // A failed start_link answers the error; the trapping parent's Exit is
    // consumed by it (OTP's start_link flushes the 'EXIT').
    let failed = Failing.start_link(&observer.context());
    assert!(matches!(
        failed,
        Err(StartError::InitExited {
            reason: ExitReason::Panic(_),
            pending: None
        })
    ));
    assert_eq!(observer.request(Counts).unwrap(), (0, 0));
    // A ready child's later death is an ordinary Exit, and Down to a monitor.
    let (started, monitor) = FailsAfterStart.start_monitor(&observer.context()).unwrap();
    observer.context().link(&started.child_handle());
    let down = down_rx.recv_timeout(LIMIT).unwrap();
    assert_eq!(down.monitor_ref, monitor);
    assert!(matches!(down.reason, ExitReason::Panic(_)));
    let exit = exit_rx.recv_timeout(LIMIT).unwrap();
    assert_eq!(exit.from, started.id());
    assert!(matches!(
        exit.reason,
        ExitReason::Panic(_) | ExitReason::NoProc
    ));
    assert_eq!(observer.request(Counts).unwrap(), (1, 1));
    started.join();
    let idle = Idle.spawn();
    let watch = observer.context().monitor(&idle.child_handle());
    observer.context().demonitor(watch);
    idle.context().stop();
    idle.join();
    assert_eq!(observer.request(Counts).unwrap(), (1, 1));
    observer.context().stop();
    observer.join();
    // Without trap_exit, a failed start_link still stops the parent (the
    // link kills the caller, as in OTP).
    let peer = Idle.spawn();
    assert!(Failing.start_link(&peer.context()).is_err());
    assert!(matches!(peer.wait_exit(), ExitReason::Panic(_)));
    peer.join();
    println!("Native monitor/link notifications preserve reasons; trapped peers keep running.");
}
