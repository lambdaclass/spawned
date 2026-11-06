#![cfg(feature = "backend-experiments")]
//! Run each experiment mode in a separate process; never mutate env inside tests.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Backend, Context, Handler},
};
use std::{sync::mpsc, time::Duration};
struct Probe;
struct Nested;
impl Message for Probe {
    type Result = u64;
}
impl Message for Nested {
    type Result = bool;
}
struct Worker;
impl Actor for Worker {}
impl Handler<Probe> for Worker {
    async fn handle(&mut self, _: Probe, _: &Context<Self>) -> u64 {
        let child = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            42
        });
        child.await.unwrap()
    }
}
impl Handler<Nested> for Worker {
    async fn handle(&mut self, _: Nested, _: &Context<Self>) -> bool {
        let (tx, rx) = mpsc::channel();
        tokio::spawn(async move {
            let _ = tx.send(());
        });
        // Characterizes progress of a detached helper while its actor blocks.
        rx.recv_timeout(Duration::from_millis(100)).is_ok()
    }
}
#[test]
fn experiment_contracts() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let actor = Worker.start_with_backend(Backend::Thread);
        assert_eq!(actor.request(Probe).await.unwrap(), 42);
        let helper_progress = actor.request(Nested).await.unwrap();
        let current = std::env::var("SPAWNED_EXPERIMENT_RUNTIME").as_deref() == Ok("current");
        assert_eq!(
            helper_progress, !current,
            "runtime flavor has an observable blocking-helper tradeoff"
        );
        actor.child_handle().stop();
        tokio::time::timeout(Duration::from_secs(5), actor.join())
            .await
            .unwrap();
    });
}

struct Network;
impl Message for Network {
    type Result = u64;
}
impl Handler<Network> for Worker {
    async fn handle(&mut self, _: Network, _: &Context<Self>) -> u64 {
        use std::io::{Read, Write};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut data = [0; 8];
            socket.read_exact(&mut data).unwrap();
            socket.write_all(&data).unwrap();
        });
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket.write_all(&42u64.to_le_bytes()).await.unwrap();
        let mut data = [0; 8];
        socket.read_exact(&mut data).await.unwrap();
        server.join().unwrap();
        u64::from_le_bytes(data)
    }
}
#[test]
fn experiment_io_reactor() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let actor = Worker.start_with_backend(Backend::Thread);
        assert_eq!(actor.request(Network).await.unwrap(), 42);
        actor.child_handle().stop();
        actor.join().await;
    });
}

#[test]
fn characterize_caller_runtime_lifetime() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let actor = rt.block_on(async {
        let actor = Worker.start_with_backend(Backend::Thread);
        assert_eq!(actor.request(Probe).await.unwrap(), 42);
        actor
    });
    drop(rt);
    let observer = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        let outcome = actor.request(Probe).await;
        if std::env::var("SPAWNED_EXPERIMENT_RUNTIME").as_deref() == Ok("shared") {
            assert!(outcome.is_err());
        } else {
            assert_eq!(outcome.unwrap(), 42);
        }
        actor.child_handle().stop();
        tokio::time::timeout(Duration::from_secs(5), actor.join())
            .await
            .unwrap();
    });
}
