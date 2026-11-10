use spawned_concurrency::{message::Message, tasks::{Actor, ActorStart, Context, Handler}, Response};
struct Probe<const N: usize>([u8; N]);
impl<const N: usize> Actor for Probe<N> {}
struct Ping;
impl Message for Ping { type Result = u64; }
impl<const N: usize> Handler<Ping> for Probe<N> {
    async fn handle(&mut self, _: Ping, _: &Context<Self>) -> u64 { self.0.len() as u64 }
}
fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        for (name, actor) in [("small", Probe([0;0]).start())] {
            println!("{name} ActorRef={} Context={} Response={} request_future={}", std::mem::size_of_val(&actor), std::mem::size_of_val(&actor.context()), std::mem::size_of::<Response<u64>>(), std::mem::size_of_val(&actor.request(Ping)));
            actor.request(Ping).await.unwrap(); actor.context().stop(); actor.join().await;
        }
        let actor = Probe([0;16384]).start();
        println!("large ActorRef={} Context={} Response={} request_future={}", std::mem::size_of_val(&actor), std::mem::size_of_val(&actor.context()), std::mem::size_of::<Response<u64>>(), std::mem::size_of_val(&actor.request(Ping)));
        actor.request(Ping).await.unwrap(); actor.context().stop(); actor.join().await;
    });
}
