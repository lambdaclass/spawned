use spawned_concurrency::{
    message::Message,
    threads::{Actor, ActorStart, Context, Handler},
    ExitReason,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};
#[path = "support/drop_subprocess.rs"]
mod subprocess;
const LIMIT: Duration = Duration::from_secs(10);
struct Bomb(Arc<AtomicUsize>);
impl Message for Bomb {
    type Result = ();
}
impl Drop for Bomb {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("native queued destructor");
    }
}
struct Victim {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    state_panics: bool,
}
impl Actor for Victim {
    fn started(&mut self, _: &Context<Self>) {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(LIMIT).unwrap();
    }
}
impl Drop for Victim {
    fn drop(&mut self) {
        assert!(!self.state_panics, "native state destructor");
    }
}
impl Handler<Bomb> for Victim {
    fn handle(
        &mut self,
        _: Bomb,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("queued handler ran");
        }
    }
}

struct Crash;
impl Message for Crash {
    type Result = ();
}
impl Handler<Crash> for Victim {
    fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("native handler failure");
        }
    }
}

#[test]
fn native_mailbox_drop_panics_are_contained_before_completion() {
    subprocess::run("native_drop_child", "validated native receiver cleanup");
}
#[test]
#[ignore = "subprocess helper exercised by enabled parent"]
fn native_drop_child() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    for reason in [
        ExitReason::Normal,
        ExitReason::Shutdown,
        ExitReason::Kill,
        ExitReason::Panic("linked failure".into()),
    ] {
        for state_panics in [false, true] {
            let (entered, arrival) = mpsc::channel();
            let (release, gate) = mpsc::channel();
            let actor = Victim {
                entered,
                release: gate,
                state_panics,
            }
            .start();
            arrival.recv_timeout(LIMIT).unwrap();
            let dropped = Arc::new(AtomicUsize::new(0));
            let replies: Vec<_> = (0..2)
                .map(|_| actor.request_raw(Bomb(dropped.clone())).unwrap())
                .collect();
            actor.context().stop_with_reason(reason.clone());
            release.send(()).unwrap();
            let child = actor.child_handle();
            let waiter = child.clone();
            let (done, exit) = mpsc::channel();
            std::thread::spawn(move || done.send(waiter.wait_exit_blocking()).unwrap());
            let exit = exit.recv_timeout(LIMIT).unwrap();
            // Destructor panics after the death point are cleanup failures:
            // the final reason is the requested one.
            let _ = state_panics;
            assert_eq!(exit, reason);
            assert_eq!(
                child.cleanup_failure().as_deref(),
                Some("native queued destructor")
            );
            assert_eq!(dropped.load(Ordering::SeqCst), 2);
            for reply in replies {
                assert!(reply.recv().is_err());
            }
            // Keep ActorRef alive through all assertions: cleanup cannot depend
            // on whoever eventually drops its last sender.
            drop(actor);
        }
    }
    // Preserve the handler panic when both state and queued destructors fail.
    let (entered, arrival) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let actor = Victim {
        entered,
        release: gate,
        state_panics: true,
    }
    .start();
    arrival.recv_timeout(LIMIT).unwrap();
    actor.send(Crash).unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    for _ in 0..2 {
        actor.send(Bomb(dropped.clone())).unwrap();
    }
    release.send(()).unwrap();
    let child = actor.child_handle();
    let (done, exit) = mpsc::channel();
    std::thread::spawn(move || done.send(child.wait_exit_blocking()).unwrap());
    assert_eq!(
        exit.recv_timeout(LIMIT).unwrap(),
        ExitReason::Panic("panic in handler: native handler failure".into())
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    println!("validated native receiver cleanup");
}
