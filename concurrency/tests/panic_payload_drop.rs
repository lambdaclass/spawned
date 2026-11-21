//! A panic whose payload's own destructor panics (semantics.md L5, L6, P1).
//!
//! The payload of a caught panic is user code too. The run loop used to drop
//! the payload of a `started()`, handler or `stopped()` panic in place, so a
//! payload destructor that panicked unwound the whole lifecycle: after a
//! handler panic `stopped()` was skipped and the reason became "actor
//! lifecycle unwound", and a managed actor never left its running set. Found
//! by the scenario fuzzer's panic injection (`PAYLOAD_DROP`). Every caught
//! payload now goes through `dispose_panic`, which forgets a secondary one.
#![cfg(all(feature = "async", not(spawned_sim)))]

use spawned_concurrency::{message::Message, ExitReason, PendingReply};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

/// A panic payload whose destructor panics (outside an unwind).
struct Exploding;
impl Drop for Exploding {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            panic!("panic payload destructor");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hook {
    Started,
    Handler,
    Stopped,
}

struct Go;
impl Message for Go {
    type Result = ();
}

fn expect(hook: Hook, reason: &ExitReason, stopped: usize) {
    let prefix = match hook {
        Hook::Started => "panic in started()",
        Hook::Handler => "panic in handler",
        Hook::Stopped => "panic in stopped()",
    };
    match reason {
        ExitReason::Panic(message) => assert!(
            message.starts_with(prefix),
            "{hook:?}: reason {message:?}, expected {prefix:?}"
        ),
        other => panic!("{hook:?}: reason {other:?}, expected a panic"),
    }
    // L5: no `stopped()` after a failed start; after a handler panic it runs.
    let runs = if hook == Hook::Started { 0 } else { 1 };
    assert_eq!(stopped, runs, "{hook:?}: stopped() ran {stopped} times");
}

mod tasks_mode {
    use super::*;
    use spawned_concurrency::system::ActorSystem;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};

    struct Victim {
        hook: Hook,
        stopped: Arc<AtomicUsize>,
    }
    impl Actor for Victim {
        async fn started(&mut self, _: &Context<Self>) {
            if self.hook == Hook::Started {
                std::panic::panic_any(Exploding);
            }
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            self.stopped.fetch_add(1, Ordering::SeqCst);
            if self.hook == Hook::Stopped {
                std::panic::panic_any(Exploding);
            }
        }
    }
    impl Handler<Go> for Victim {
        async fn handle(&mut self, _: Go, _: PendingReply<'_, ()>, ctx: &Context<Self>) {
            if self.hook == Hook::Handler {
                std::panic::panic_any(Exploding);
            }
            ctx.stop();
        }
    }

    #[tokio::test]
    async fn hook_panics_with_exploding_payloads_keep_the_lifecycle() {
        for hook in [Hook::Started, Hook::Handler, Hook::Stopped] {
            let stopped = Arc::new(AtomicUsize::new(0));
            let actor = Victim {
                hook,
                stopped: stopped.clone(),
            }
            .start();
            let _ = actor.send(Go);
            let reason = tokio::time::timeout(Duration::from_secs(10), actor.wait_exit())
                .await
                .expect("the actor exits");
            expect(hook, &reason, stopped.load(Ordering::SeqCst));
        }
    }

    /// A managed actor leaves its running set, so the system shuts down.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn managed_handler_panic_with_exploding_payload_shuts_down() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("payload", 1).unwrap();
        let stopped = Arc::new(AtomicUsize::new(0));
        let actor = Victim {
            hook: Hook::Handler,
            stopped: stopped.clone(),
        }
        .start_on(&group)
        .unwrap();
        actor.send(Go).unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(10), actor.wait_exit())
            .await
            .expect("the actor exits");
        expect(Hook::Handler, &reason, stopped.load(Ordering::SeqCst));
        let report = system.shutdown(Duration::from_secs(10)).await;
        assert!(report.complete, "system shutdown incomplete");
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};

    struct Victim {
        hook: Hook,
        stopped: Arc<AtomicUsize>,
    }
    impl Actor for Victim {
        fn started(&mut self, _: &Context<Self>) {
            if self.hook == Hook::Started {
                std::panic::panic_any(Exploding);
            }
        }
        fn stopped(&mut self, _: &Context<Self>) {
            self.stopped.fetch_add(1, Ordering::SeqCst);
            if self.hook == Hook::Stopped {
                std::panic::panic_any(Exploding);
            }
        }
    }
    impl Handler<Go> for Victim {
        fn handle(&mut self, _: Go, _: PendingReply<'_, ()>, ctx: &Context<Self>) {
            if self.hook == Hook::Handler {
                std::panic::panic_any(Exploding);
            }
            ctx.stop();
        }
    }

    #[test]
    fn hook_panics_with_exploding_payloads_keep_the_lifecycle() {
        for hook in [Hook::Started, Hook::Handler, Hook::Stopped] {
            let stopped = Arc::new(AtomicUsize::new(0));
            let actor = Victim {
                hook,
                stopped: stopped.clone(),
            }
            .start();
            let _ = actor.send(Go);
            let (tx, rx) = std::sync::mpsc::channel();
            let waiter = actor.clone();
            std::thread::spawn(move || {
                let _ = tx.send(waiter.wait_exit());
            });
            let reason = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the actor exits");
            expect(hook, &reason, stopped.load(Ordering::SeqCst));
        }
    }
}
