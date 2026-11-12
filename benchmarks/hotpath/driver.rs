//! Hot-path driver for `benchmarks/hotpath.py` (the CI instruction-count gate
//! and the manual wall-clock A/B). The script copies this file into
//! `concurrency/examples/` of both trees, so it must only use APIs that are
//! stable across releases: `#[actor]` handlers, `request()` and `send()`.
//!
//! Usage: hotpath <benchmark> <ops>   (or `list`)
//! Prints `benchmark,ops,elapsed_s`.
use spawned_concurrency::actor;
use spawned_concurrency::message::Message;
use std::time::{Duration, Instant};

pub struct Q(pub u64);
impl Message for Q {
    type Result = u64;
}
pub struct Hit(pub u64);
impl Message for Hit {
    type Result = ();
}

mod a {
    use super::*;
    use spawned_concurrency::tasks::{Actor, Context, Handler};
    pub struct Sink(pub u64);
    #[actor]
    impl Sink {
        #[request_handler]
        async fn q(&mut self, msg: Q, _ctx: &Context<Self>) -> u64 {
            self.0 = self.0.wrapping_add(msg.0);
            self.0
        }
        #[send_handler]
        async fn hit(&mut self, msg: Hit, _ctx: &Context<Self>) {
            self.0 = self.0.wrapping_add(msg.0);
        }
    }
}

mod t {
    use super::*;
    use spawned_concurrency::threads::{Actor, Context, Handler};
    pub struct Sink(pub u64);
    #[actor]
    impl Sink {
        #[request_handler]
        fn q(&mut self, msg: Q, _ctx: &Context<Self>) -> u64 {
            self.0 = self.0.wrapping_add(msg.0);
            self.0
        }
        #[send_handler]
        fn hit(&mut self, msg: Hit, _ctx: &Context<Self>) {
            self.0 = self.0.wrapping_add(msg.0);
        }
    }
}

const BENCHMARKS: &[&str] = &[
    "async_burst",
    "async_request_seq",
    "async_request_conc16",
    "async_rpc_thread",
    "async_send_p1",
    "async_send_p2",
    "threads_burst4",
    "threads_request_seq",
    "threads_send_p1",
    "threads_send_p2",
];

const WARMUP: u64 = 200;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Async actor benchmarks. Each returns the measured time; setup and warm-up
/// are outside it (the gate cancels them by differencing two sizes).
fn run_async(name: &str, n: u64) -> Duration {
    use spawned_concurrency::tasks::ActorStart;
    let rt = runtime();
    let actor = rt.block_on(async { a::Sink(0).start() });
    let fence = |actor: &spawned_concurrency::tasks::ActorRef<a::Sink>| {
        rt.block_on(actor.request(Q(0))).unwrap();
    };
    for i in 0..WARMUP {
        rt.block_on(actor.request(Q(i))).unwrap();
    }
    let start = Instant::now();
    match name {
        // Every request is enqueued before the first reply is awaited.
        "async_burst" => rt.block_on(async {
            let all = (0..n).map(|i| actor.request(Q(i)));
            for r in futures::future::join_all(all).await {
                std::hint::black_box(r.unwrap());
            }
        }),
        "async_request_seq" | "async_request_conc16" => {
            let tasks = if name == "async_request_seq" { 1 } else { 16 };
            rt.block_on(async {
                let joins: Vec<_> = (0..tasks)
                    .map(|_| {
                        let actor = actor.clone();
                        tokio::spawn(async move {
                            for i in 0..n / tasks {
                                std::hint::black_box(actor.request(Q(i)).await.unwrap());
                            }
                        })
                    })
                    .collect();
                for j in joins {
                    j.await.unwrap();
                }
            })
        }
        // A foreign OS thread blocking on each reply.
        "async_rpc_thread" => {
            let h = rt.handle().clone();
            let actor = actor.clone();
            std::thread::spawn(move || {
                for i in 0..n {
                    std::hint::black_box(h.block_on(actor.request(Q(i))).unwrap());
                }
            })
            .join()
            .unwrap();
        }
        "async_send_p1" | "async_send_p2" => {
            let producers = if name == "async_send_p1" { 1 } else { 2 };
            std::thread::scope(|s| {
                for _ in 0..producers {
                    let actor = actor.clone();
                    s.spawn(move || {
                        for i in 0..n / producers {
                            actor.send(Hit(i)).unwrap();
                        }
                    });
                }
            });
            // FIFO per sender: this reply follows every message sent above.
            fence(&actor);
        }
        _ => panic!("unknown benchmark {name}"),
    }
    start.elapsed()
}

fn run_threads(name: &str, n: u64) -> Duration {
    use spawned_concurrency::threads::ActorStart;
    let actor = t::Sink(0).start();
    for i in 0..WARMUP {
        actor.request(Q(i)).unwrap();
    }
    let start = Instant::now();
    let (workers, send) = match name {
        "threads_burst4" => (4, false),
        "threads_request_seq" => (1, false),
        "threads_send_p1" => (1, true),
        "threads_send_p2" => (2, true),
        _ => panic!("unknown benchmark {name}"),
    };
    std::thread::scope(|s| {
        for _ in 0..workers {
            let actor = actor.clone();
            s.spawn(move || {
                for i in 0..n / workers {
                    if send {
                        actor.send(Hit(i)).unwrap();
                    } else {
                        std::hint::black_box(actor.request(Q(i)).unwrap());
                    }
                }
            });
        }
    });
    if send {
        actor.request(Q(0)).unwrap();
    }
    start.elapsed()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("list") {
        for b in BENCHMARKS {
            println!("{b}");
        }
        return;
    }
    let name = args[1].as_str();
    let n: u64 = args[2].parse().unwrap();
    assert!(BENCHMARKS.contains(&name), "unknown benchmark {name}");
    let elapsed = if name.starts_with("async_") {
        run_async(name, n)
    } else {
        run_threads(name, n)
    };
    println!("{name},{n},{:.6}", elapsed.as_secs_f64());
    // Skip actor teardown: it is not part of any measurement.
    std::process::exit(0);
}
