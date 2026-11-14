//! Fresh-process metrics overhead comparison. Not a client benchmark.
use spawned_concurrency::{message::Message, tasks, threads};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};
struct Work(u64);
impl Message for Work {
    type Result = u64;
}
struct Sink;
fn compute(work: Work) -> u64 {
    let mut x = 17u64;
    for _ in 0..work.0 {
        x = black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    x
}
impl tasks::Actor for Sink {}
impl tasks::Handler<Work> for Sink {
    async fn handle(
        &mut self,
        work: Work,
        reply: spawned_concurrency::PendingReply<'_, u64>,
        _: &tasks::Context<Self>,
    ) {
        reply.reply(compute(work));
    }
}
impl threads::Actor for Sink {}
impl threads::Handler<Work> for Sink {
    fn handle(
        &mut self,
        work: Work,
        reply: spawned_concurrency::PendingReply<'_, u64>,
        _: &threads::Context<Self>,
    ) {
        reply.reply(compute(work));
    }
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        7,
        "mode actors producers work seconds sample_every"
    );
    let mode = &args[1];
    let actors: usize = args[2].parse().unwrap();
    let producers: usize = args[3].parse().unwrap();
    let work: u64 = args[4].parse().unwrap();
    let duration = Duration::from_secs(args[5].parse().unwrap());
    let _sample_every: u64 = args[6].parse().unwrap();
    assert!(actors > 0 && actors <= 1024 && producers > 0 && producers <= 1024);
    assert!(duration.as_secs() > 0 && duration.as_secs() <= 60 && work <= 1_000_000);
    #[cfg(feature = "metrics")]
    spawned_concurrency::metrics::configure(
        spawned_concurrency::metrics::Config::default().timing_sample_every(_sample_every),
    )
    .unwrap();
    let (requests, elapsed) = if mode == "async" {
        use tasks::ActorStart;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let actors: Vec<_> = (0..actors).map(|_| Sink.start()).collect();
            for actor in &actors {
                black_box(actor.request(Work(work)).await.unwrap());
            }
            let began = Instant::now();
            let deadline = began + duration;
            let mut jobs = Vec::new();
            for p in 0..producers {
                let actor = actors[p % actors.len()].clone();
                jobs.push(tokio::spawn(async move {
                    let mut n = 0u64;
                    while Instant::now() < deadline {
                        black_box(actor.request(Work(work)).await.unwrap());
                        n += 1;
                    }
                    n
                }));
            }
            let mut requests = 0;
            for job in jobs {
                requests += job.await.unwrap();
            }
            let elapsed = began.elapsed();
            for actor in &actors {
                actor.context().stop();
            }
            for actor in actors {
                actor.join().await;
            }
            (requests, elapsed)
        })
    } else {
        assert_eq!(mode, "threads");
        use threads::ActorStart;
        let actors: Vec<_> = (0..actors).map(|_| Sink.start()).collect();
        for actor in &actors {
            black_box(actor.request(Work(work)).unwrap());
        }
        let began = Instant::now();
        let deadline = began + duration;
        let mut jobs = Vec::new();
        for p in 0..producers {
            let actor = actors[p % actors.len()].clone();
            jobs.push(std::thread::spawn(move || {
                let mut n = 0u64;
                while Instant::now() < deadline {
                    black_box(actor.request(Work(work)).unwrap());
                    n += 1;
                }
                n
            }));
        }
        let requests = jobs.into_iter().map(|job| job.join().unwrap()).sum::<u64>();
        let elapsed = began.elapsed();
        for actor in &actors {
            actor.context().stop();
        }
        for actor in actors {
            actor.join();
        }
        (requests, elapsed)
    };
    #[cfg(feature = "metrics")]
    {
        let s = spawned_concurrency::metrics::snapshot();
        assert_eq!(
            s.actors_active + s.handlers_active + s.requests_active + s.mailbox_depth,
            0
        );
        assert_eq!(
            s.messages_enqueued_total,
            s.messages_handled_total + s.messages_discarded_total
        );
    }
    println!("metrics,mode,actors,producers,work,sample_every,requests,seconds,requests_per_second,actor_ref_bytes");
    println!(
        "{},{mode},{actors},{producers},{work},{_sample_every},{requests},{:.9},{:.3},{}",
        cfg!(feature = "metrics"),
        elapsed.as_secs_f64(),
        requests as f64 / elapsed.as_secs_f64(),
        std::mem::size_of::<tasks::ActorRef<Sink>>()
    );
}
