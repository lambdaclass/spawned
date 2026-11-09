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
    fn handle(&mut self, _: Bomb, _: &Context<Self>) {
        panic!("queued handler ran");
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
            if reason != ExitReason::Normal {
                assert_eq!(exit, reason);
            } else if state_panics {
                assert!(
                    matches!(exit, ExitReason::Panic(ref s) if s.contains("native state destructor"))
                );
            } else {
                assert_eq!(
                    exit,
                    ExitReason::Panic("queued message destructor: native queued destructor".into())
                );
            }
            assert_eq!(
                child.mailbox_cleanup_failure().as_deref(),
                Some("native queued destructor")
            );
            assert_eq!(dropped.load(Ordering::SeqCst), 2);
            for reply in replies {
                assert!(reply.recv_timeout(LIMIT).is_err());
            }
            // Keep ActorRef alive through all assertions: cleanup cannot depend
            // on whoever eventually drops its last sender.
            drop(actor);
        }
    }
    println!("validated native receiver cleanup");
}
