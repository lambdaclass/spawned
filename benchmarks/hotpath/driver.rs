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

/// (name, deterministic). The instruction-count gate runs only the
/// deterministic ones: everything on one thread (`async_ct_*`, a Tokio
/// current-thread runtime, identical counts run to run) or a lockstep
/// request/reply pair of threads (within 0.5%). Multi-threaded runtimes and
/// free-running OS-thread producers change how often actors park and wake
/// from run to run (up to ±20% instructions under callgrind), so those are
/// wall-clock only (`hotpath.py abba`).
const BENCHMARKS: &[(&str, bool)] = &[
    ("async_ct_burst", true),
    ("async_ct_request_seq", true),
    ("async_ct_request_conc16", true),
    ("async_ct_send_p1", true),
    ("async_ct_send_p2", true),
    ("threads_request_seq", true),
    ("threads_burst4", false),
    ("threads_send_p1", false),
    ("async_burst", false),
    ("async_request_seq", false),
    ("async_request_conc16", false),
    ("async_rpc_thread", false),
    ("async_send_p1", false),
    ("async_send_p2", false),
    ("threads_send_p2", false),
];

const WARMUP: u64 = 200;

fn runtime(current_thread: bool) -> tokio::runtime::Runtime {
    let mut builder = if current_thread {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut b = tokio::runtime::Builder::new_multi_thread();
        b.worker_threads(2);
        b
    };
    builder.enable_all().build().unwrap()
}

/// Async actor benchmarks. Each returns the measured time; setup and warm-up
/// are outside it (the gate cancels them by differencing two sizes).
fn run_async(name: &str, n: u64) -> Duration {
    use spawned_concurrency::tasks::ActorStart;
    let current_thread = name.starts_with("async_ct_");
    let rt = runtime(current_thread);
    let name = name.replacen("async_ct_", "async_", 1);
    let name = name.as_str();
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
            if current_thread {
                // Two producer tasks on the one runtime thread, interleaved
                // in chunks; the actor runs when they yield.
                rt.block_on(async {
                    let joins: Vec<_> = (0..producers)
                        .map(|_| {
                            let actor = actor.clone();
                            tokio::spawn(async move {
                                for i in 0..n / producers {
                                    actor.send(Hit(i)).unwrap();
                                    if i % 64 == 63 {
                                        tokio::task::yield_now().await;
                                    }
                                }
                            })
                        })
                        .collect();
                    for j in joins {
                        j.await.unwrap();
                    }
                });
                fence(&actor);
                return start.elapsed();
            }
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
    match args.get(1).map(String::as_str) {
        // `list`: every benchmark; `list-gate`: the deterministic ones.
        Some(list @ ("list" | "list-gate")) => {
            for (b, deterministic) in BENCHMARKS {
                if list == "list" || *deterministic {
                    println!("{b}");
                }
            }
            return;
        }
        _ => {}
    }
    let name = args[1].as_str();
    let n: u64 = args[2].parse().unwrap();
    assert!(BENCHMARKS.iter().any(|(b, _)| *b == name), "unknown benchmark {name}");
    let elapsed = if name.starts_with("async_") {
        run_async(name, n)
    } else {
        run_threads(name, n)
    };
    println!("{name},{n},{:.6}", elapsed.as_secs_f64());
    // Skip actor teardown: it is not part of any measurement.
    std::process::exit(0);
}
