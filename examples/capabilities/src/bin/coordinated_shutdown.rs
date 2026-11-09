//! Close producers, snapshot dependencies while live, then join the tree.
use spawned_concurrency::{
    message::Message,
    supervision::{ChildSpec, SupervisorConfig, SupervisorStrategy},
    system::ActorSystem,
    tasks::{Actor, ActorRef, Context, Handler},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
type Stops = Arc<Mutex<Vec<&'static str>>>;
struct Peer(u64);
impl Message for Peer {
    type Result = ();
}
struct Snapshot;
impl Message for Snapshot {
    type Result = Vec<u64>;
}
struct Import(u64);
impl Message for Import {
    type Result = ();
}
struct Fence;
impl Message for Fence {
    type Result = ();
}
struct Peers {
    peers: Vec<u64>,
    stops: Stops,
}
impl Actor for Peers {
    async fn stopped(&mut self, _: &Context<Self>) {
        self.stops.lock().unwrap().push("peers");
    }
}
impl Handler<Peer> for Peers {
    async fn handle(&mut self, msg: Peer, _: &Context<Self>) {
        self.peers.push(msg.0);
    }
}
impl Handler<Snapshot> for Peers {
    async fn handle(&mut self, _: Snapshot, _: &Context<Self>) -> Vec<u64> {
        self.peers.clone()
    }
}
struct Importer {
    peers: ActorRef<Peers>,
    stops: Stops,
}
impl Actor for Importer {
    async fn stopped(&mut self, _: &Context<Self>) {
        self.stops.lock().unwrap().push("importer");
    }
}
impl Handler<Import> for Importer {
    async fn handle(&mut self, msg: Import, _: &Context<Self>) {
        self.peers.send(Peer(msg.0)).unwrap();
    }
}
impl Handler<Fence> for Importer {
    async fn handle(&mut self, _: Fence, _: &Context<Self>) {}
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let system = ActorSystem::new().unwrap();
    let network = system.async_group("network", 1).unwrap();
    let chain = system.async_group("chain", 1).unwrap();
    let stops: Stops = Arc::default();
    // RestForOne rebuilds dependents retaining an earlier instance's reference.
    let mut tree = system
        .supervisor(SupervisorConfig {
            strategy: SupervisorStrategy::RestForOne,
            ..SupervisorConfig::default()
        })
        .unwrap();
    let events = stops.clone();
    let peers = tree
        .add(ChildSpec::new("peers", &network, move |_| {
            let events = events.clone();
            async move {
                Ok::<_, String>(Peers {
                    peers: Vec::new(),
                    stops: events,
                })
            }
        }))
        .unwrap();
    let dependency = peers.clone();
    let events = stops.clone();
    let importer = tree
        .add(ChildSpec::new("importer", &chain, move |_| {
            let dependency = dependency.clone();
            let events = events.clone();
            async move {
                let peers = dependency.wait_ready().await.map_err(|e| e.to_string())?;
                Ok::<_, String>(Importer {
                    peers,
                    stops: events,
                })
            }
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let importer = importer.current().unwrap();
    let sender = importer.clone();
    let producer = tokio::spawn(async move {
        for id in 0..16 {
            sender.send(Import(id)).unwrap();
        }
    });
    // In a real client: stop accepting ingress and join every producer first.
    producer.await.unwrap();
    importer.request(Fence).await.unwrap();
    let peer_snapshot = peers.current().unwrap().request(Snapshot).await.unwrap();
    assert_eq!(peer_snapshot, (0..16).collect::<Vec<_>>());
    // Persist this snapshot and await the application's durability contract.
    // This example retains it in memory; a FIFO fence is not disk durability.
    assert!(supervisor.failure().is_none());
    assert!(system.shutdown(Duration::from_secs(2)).await.complete);
    assert_eq!(*stops.lock().unwrap(), ["importer", "peers"]);
    println!("Producers joined, snapshot taken before closure, reverse dependency shutdown.");
}
