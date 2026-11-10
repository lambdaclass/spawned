#![cfg(feature = "async")]
//! Hold a channel-block allocation after send admission to exercise shutdown
//! against a sender that has reserved a queue position but not published it.
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler},
    ChildHandle, ExitReason,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
thread_local! {static PRODUCER: Cell<bool> = const { Cell::new(false) };}

// ARMED -> PAUSED -> RELEASE gates exactly one allocation on the producer.
static ARMED: AtomicBool = AtomicBool::new(false);
static PAUSED: AtomicBool = AtomicBool::new(false);
static RELEASE: AtomicBool = AtomicBool::new(false);
static SENT: AtomicUsize = AtomicUsize::new(0);
struct GateAllocator;
unsafe impl GlobalAlloc for GateAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        // The envelope and reply state are smaller than 256 bytes; Tokio's later
        // mpsc block allocation is larger. A changed allocation strategy must
        // fail the admission assertion below instead of silently passing.
        if layout.size() > 256
            && PRODUCER.try_with(Cell::get).unwrap_or(false)
            && ARMED.swap(false, Ordering::SeqCst)
        {
            PAUSED.store(true, Ordering::SeqCst);
            while !RELEASE.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: GateAllocator = GateAllocator;
const LIMIT: Duration = Duration::from_secs(5);

enum Command {
    Hold {
        entered: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    },
    Work,
}
impl Message for Command {
    type Result = ();
}
struct Worker(Option<tokio::sync::oneshot::Sender<()>>);
impl Actor for Worker {
    async fn stopped(&mut self, _: &Context<Self>) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}
impl Handler<Command> for Worker {
    async fn handle(
        &mut self,
        msg: Command,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            match msg {
                Command::Hold { entered, release } => {
                    let _ = entered.send(());
                    let _ = release.await;
                }
                Command::Work => panic!("queued work must be discarded after stop"),
            }
        }
        let _ = reply.reply(());
    }
}
struct Cleanup(ChildHandle);
impl Drop for Cleanup {
    fn drop(&mut self) {
        RELEASE.store(true, Ordering::SeqCst);
        self.0.stop();
    }
}

// Not simulated: builds its own multi-thread Tokio runtime.
#[cfg(not(spawned_sim))]
#[test]
fn admitted_request_racing_shutdown_drops_its_reply_before_join_completes() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (stopped, stop_rx) = tokio::sync::oneshot::channel();
        let actor = Worker(Some(stopped)).start();
        let _cleanup = Cleanup(actor.child_handle());
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        let _hold_reply = actor
            .request_raw(Command::Hold {
                entered,
                release: gate,
            })
            .unwrap();
        tokio::time::timeout(LIMIT, entered_rx)
            .await
            .unwrap()
            .unwrap();
        ARMED.store(true, Ordering::SeqCst);
        let sender = actor.clone();
        let producer = std::thread::spawn(move || {
            PRODUCER.with(|p| p.set(true));
            for _ in 0..1024 {
                let reply = sender.request_raw(Command::Work);
                if PAUSED.load(Ordering::SeqCst) {
                    return reply;
                }
                SENT.fetch_add(1, Ordering::SeqCst);
                drop(reply);
            }
            panic!("channel allocation gate was not reached");
        });
        tokio::time::timeout(LIMIT, async {
            while !PAUSED.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("producer never reserved the next channel block");
        assert!(
            SENT.load(Ordering::SeqCst) > 0,
            "gate must be inside a later mailbox send"
        );
        actor.child_handle().stop();
        release.send(()).unwrap();
        tokio::time::timeout(LIMIT, stop_rx).await.unwrap().unwrap();
        // Let the actor reach its receiver cleanup while publication is suspended.
        // Both completion timings are legal; the reply must resolve after send/join.
        let _ = tokio::time::timeout(Duration::from_millis(50), actor.wait_exit()).await;
        RELEASE.store(true, Ordering::SeqCst);
        let result = producer.join().unwrap();
        assert_eq!(
            tokio::time::timeout(LIMIT, actor.wait_exit())
                .await
                .unwrap(),
            ExitReason::Normal
        );
        let reply = result.expect("gate fired before mailbox admission; allocation hook is stale");
        assert!(spawned_rt::tasks::timeout(LIMIT, reply)
            .await
            .expect("reply sender retained after actor termination")
            .is_err());
        drop(actor); // keep sender references alive through the assertion
    });
}
