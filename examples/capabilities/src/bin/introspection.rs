//! Introspection (0.9): which actor is stuck, on what, and what is it linked
//! to? `info()` answers out of band, even while a handler is wedged;
//! `get_state`/`get_status` read state in a turn of their own; a supervisor
//! lists and expands its tree. Both modes.
use spawned_concurrency::{
    actor,
    introspection::{Inspect, Lifecycle, StateView},
    message::Message,
    supervision::{ChildSpec, SupervisorConfig, TreeNode},
    system::ActorSystem,
    ActorError, PendingReply,
};
use std::{sync::mpsc, time::Duration};

/// Blocks its handler's thread until released: a stuck actor.
struct Wedge(mpsc::Sender<()>, mpsc::Receiver<()>);
impl Message for Wedge {
    type Result = ();
}

mod tasks_peer {
    use super::*;
    use spawned_concurrency::tasks::{Actor, Context, Handler};

    /// A connection actor, labelled by its peer and showing its state.
    pub struct Connection {
        pub peer: &'static str,
        pub received: u64,
    }
    impl Inspect for Connection {
        fn format_status(&self) -> String {
            format!("peer {} received {}", self.peer, self.received)
        }
    }
    #[actor(inspect)]
    impl Connection {
        #[started]
        async fn label(&mut self, ctx: &Context<Self>) {
            ctx.set_label(format!("peer {}", self.peer));
        }
    }
    impl Handler<Wedge> for Connection {
        async fn handle(&mut self, msg: Wedge, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.received += 1;
            msg.0.send(()).unwrap();
            let _ = msg.1.recv_timeout(Duration::from_secs(5));
            reply.reply(());
        }
    }
}

mod threads_peer {
    use super::*;
    use spawned_concurrency::threads::{Actor, Context, Handler};

    #[derive(Debug)]
    pub struct Ledger {
        pub entries: u64,
    }
    #[actor(inspect = debug)]
    impl Ledger {
        #[started]
        fn label(&mut self, ctx: &Context<Self>) {
            ctx.set_label("ledger");
        }
    }
    impl Handler<Wedge> for Ledger {
        fn handle(&mut self, msg: Wedge, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.entries += 1;
            msg.0.send(()).unwrap();
            let _ = msg.1.recv_timeout(Duration::from_secs(5));
            reply.reply(());
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    use spawned_concurrency::tasks::ActorStart as _;
    use tasks_peer::Connection;

    // 1. A wedged handler: info() still answers; a status request times out.
    let conn = Connection {
        peer: "0x3a1f",
        received: 0,
    }
    .start();
    let (entered_tx, entered) = mpsc::channel();
    let (release_tx, release) = mpsc::channel();
    conn.send(Wedge(entered_tx, release)).unwrap();
    entered.recv().unwrap();
    let info = conn.info();
    println!(
        "actor {} {} label {:?}: {:?}",
        info.id,
        info.actor_type,
        info.label.as_deref(),
        info.lifecycle
    );
    assert_eq!(info.lifecycle, Lifecycle::InTurn);
    let late = conn
        .with_request_timeout(Duration::from_millis(50))
        .get_state(|c| c.received)
        .await;
    assert!(matches!(late, Err(ActorError::RequestTimeout)));
    println!("get_state on the wedged actor: {late:?} (use info() for stuck actors)");
    release_tx.send(()).unwrap();

    // 2. Between turns: state and status (formatted, the actor opted in).
    println!(
        "received: {}",
        conn.get_state(|c| c.received).await.unwrap()
    );
    let status = conn.get_status().await.unwrap();
    println!("status: {:?} {:?}", status.info.lifecycle, status.state);
    assert_eq!(
        status.state,
        StateView::Formatted("peer 0x3a1f received 1".into())
    );
    conn.child_handle().stop();
    conn.join().await;

    // 3. A supervision tree: rows, counts, and the expanded tree.
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("peers", 2).unwrap();
    let mut builder = system
        .supervisor(SupervisorConfig::default().name("p2p"))
        .unwrap();
    for peer in ["0x01", "0x02"] {
        builder
            .add(ChildSpec::new(peer, &group, move |_| async move {
                Ok::<_, String>(Connection { peer, received: 0 })
            }))
            .unwrap();
    }
    let supervisor = builder.start().await.unwrap();
    for row in supervisor.which_children() {
        println!(
            "child {} {:?} generation {} actor {:?}",
            row.key, row.state, row.generation, row.actor
        );
    }
    println!("{:?}", supervisor.count_children());
    let tree = supervisor.tree();
    for child in &tree.children {
        if let TreeNode::Worker(spawned_concurrency::introspection::Field::Value(info)) =
            &child.node
        {
            println!(
                "  {} -> {} {:?}",
                child.info.key,
                info.id,
                info.label.as_deref()
            );
        }
    }
    assert!(system.shutdown(Duration::from_secs(5)).await.complete);

    // 4. Threads mode: the same calls, blocking.
    use spawned_concurrency::threads::ActorStart as _;
    let ledger = threads_peer::Ledger { entries: 0 }.start();
    let status = ledger.get_status().unwrap();
    println!(
        "ledger: {:?} {:?}",
        status.info.label.as_deref(),
        status.state
    );
    ledger.child_handle().stop();
    ledger.join();
}
