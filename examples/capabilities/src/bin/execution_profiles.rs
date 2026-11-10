//! Fixed native roots for blocking work; parked pinned shards for a pipeline.
use spawned_concurrency::{
    message::Message,
    system::ActorSystem,
    tasks::{Actor, ActorStart, Context, Handler},
};
use std::time::Duration;
struct Worker;
impl Actor for Worker {}
struct Location;
impl Message for Location {
    type Result = (String, Option<usize>);
}
impl Handler<Location> for Worker {
    async fn handle(
        &mut self,
        _: Location,
        reply: spawned_concurrency::reply::PendingReply<'_, <Location as Message>::Result>,
        _: &Context<Self>,
    ) {
        reply.reply((
                std::thread::current().name().unwrap_or_default().to_owned(),
                spawned_rt::tasks::affinity::current_cpu().ok(),
            ));
    }
}
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let system = ActorSystem::new().unwrap();
    let native = system.thread_group("blocking-roots", 2).unwrap();
    let a = Worker.start_on(&native).unwrap();
    let b = Worker.start_on(&native).unwrap();
    a.ready().await.unwrap();
    b.ready().await.unwrap();
    assert!(
        Worker.start_on(&native).is_err(),
        "capacity is two live scopes"
    );
    println!("native actor: {:?}", a.request(Location).await.unwrap());
    let peer = b.clone();
    let cross = a
        .context()
        .spawn_helper(async move { peer.request(Location).await.unwrap() })
        .unwrap();
    println!(
        "peer through helper runtime: {:?}",
        cross.join().await.unwrap()
    );
    #[cfg(target_os = "linux")]
    {
        let cpus: Vec<_> = spawned_rt::tasks::affinity::available_cpus()
            .unwrap()
            .into_iter()
            .take(2)
            .collect();
        let pipeline = system.pinned_group("pipeline", cpus.clone()).unwrap();
        for (index, cpu) in cpus.iter().enumerate() {
            let stage = Worker.start_on(&pipeline.shard(index).unwrap()).unwrap();
            stage.ready().await.unwrap();
            let location = stage.request(Location).await.unwrap();
            assert_eq!(location.1, Some(*cpu));
            println!("pinned stage {index}: {location:?}");
        }
    }
    let report = system.shutdown(Duration::from_secs(5)).await;
    assert!(report.complete);
    assert!(report.group_failures.is_empty());
}
