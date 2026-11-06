//! Controlled cross-runtime request/reply under competing actor work.
#[cfg(target_os = "linux")]
mod linux {
    use spawned_concurrency::{
        message::Message,
        tasks::{Actor, ActorRef, ActorStart, Context, Handler},
        ExitReason,
    };
    use std::{
        fs,
        hint::black_box,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    const MUL: u64 = 6364136223846793005;
    #[derive(Default)]
    struct Counts {
        started: AtomicUsize,
        stopped: AtomicUsize,
        dropped: AtomicUsize,
    }
    struct Stage {
        iterations: usize,
        next: Option<ActorRef<Stage>>,
        counts: Arc<Counts>,
        handled: u64,
    }
    impl Actor for Stage {
        async fn started(&mut self, _: &Context<Self>) {
            self.counts.started.fetch_add(1, Ordering::Relaxed);
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            self.counts.stopped.fetch_add(1, Ordering::Relaxed);
        }
    }
    impl Drop for Stage {
        fn drop(&mut self) {
            self.counts.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    struct Work(u64);
    impl Message for Work {
        type Result = (u64, u64, Instant);
    }
    impl Handler<Work> for Stage {
        async fn handle(&mut self, m: Work, _: &Context<Self>) -> (u64, u64, Instant) {
            self.handled += 1;
            if let Some(next) = &self.next {
                return next.request_raw(m).unwrap().await.unwrap();
            }
            let mut value = m.0;
            for _ in 0..self.iterations {
                value = black_box(value.wrapping_mul(MUL).wrapping_add(1));
            }
            (m.0, value, Instant::now())
        }
    }
    struct Audit;
    impl Message for Audit {
        type Result = (String, u64);
    }
    impl Handler<Audit> for Stage {
        async fn handle(&mut self, _: Audit, _: &Context<Self>) -> (String, u64) {
            let status = fs::read_to_string("/proc/thread-self/status").unwrap();
            (
                status
                    .lines()
                    .find_map(|s| s.strip_prefix("Cpus_allowed_list:"))
                    .unwrap()
                    .trim()
                    .into(),
                self.handled,
            )
        }
    }
    fn pin(mask: &str) {
        let tid = fs::read_link("/proc/thread-self").unwrap();
        let r = std::process::Command::new("taskset")
            .args(["-pc", mask, tid.file_name().unwrap().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    }
    fn ticks() -> u64 {
        let s = fs::read_to_string("/proc/self/stat").unwrap();
        let f: Vec<_> = s.rsplit_once(')').unwrap().1.split_whitespace().collect();
        f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
    }
    fn arg(k: &str, d: &str) -> String {
        std::env::args()
            .find_map(|s| s.strip_prefix(&format!("--{k}=")).map(str::to_owned))
            .unwrap_or_else(|| d.into())
    }
    fn runtime(n: usize, mask: &str, name: &str) -> tokio::runtime::Runtime {
        let mask = mask.to_owned();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(n)
            .thread_name(name)
            .on_thread_start(move || pin(&mask))
            .enable_all()
            .build()
            .unwrap()
    }
    fn spawn(
        rt: &tokio::runtime::Runtime,
        iterations: usize,
        next: Option<ActorRef<Stage>>,
        counts: &Arc<Counts>,
    ) -> ActorRef<Stage> {
        let _entered = rt.enter();
        Stage {
            iterations,
            next,
            counts: counts.clone(),
            handled: 0,
        }
        .start()
    }
    fn affine(n: usize) -> (u64, u64) {
        let (mut a, mut b) = (1u64, 0u64);
        for _ in 0..n {
            a = a.wrapping_mul(MUL);
            b = b.wrapping_mul(MUL).wrapping_add(1);
        }
        (a, b)
    }
    fn check(reply: (u64, u64, Instant), id: u64, a: u64, b: u64) {
        assert_eq!((reply.0, reply.1), (id, id.wrapping_mul(a).wrapping_add(b)));
    }
    pub fn run() {
        assert!(
            !std::env::vars()
                .any(|(k, _)| k.starts_with("SPAWNED_EXPERIMENT_") || k == "TOKIO_WORKER_THREADS"),
            "reference uses public default scheduling only"
        );
        let layout = arg("layout", "shared");
        let path = arg("path", "direct");
        let load = arg("load", "busy");
        let seconds: f64 = arg("seconds", "2").parse().unwrap();
        assert!(
            ["shared", "split"].contains(&layout.as_str())
                && ["direct", "relay"].contains(&path.as_str())
                && ["idle", "busy"].contains(&load.as_str())
                && (0.05..=5.).contains(&seconds)
        );
        let split = layout == "split";
        let busy = load == "busy";
        let relay = path == "relay";
        pin("2,3,4,5");
        let general = runtime(
            if split { 3 } else { 4 },
            if split { "3,4,5" } else { "2,3,4,5" },
            "isolation-general",
        );
        let isolated = split.then(|| runtime(1, "2", "isolation-critical"));
        let critical_rt = isolated.as_ref().unwrap_or(&general);
        let counts = Arc::new(Counts::default());
        let critical = spawn(critical_rt, 800, None, &counts);
        let router = spawn(&general, 0, Some(critical.clone()), &counts);
        let backgrounds: Vec<_> = (0..16)
            .map(|_| spawn(&general, 50_000, None, &counts))
            .collect();
        let mut actors = vec![critical.clone(), router.clone()];
        actors.extend(backgrounds.iter().cloned());
        let (ab, bb) = affine(50_000);
        let (ac, bc) = affine(800);
        general.block_on(async {
            for (i, actor) in actors.iter().enumerate() {
                let (mask, handled) = actor.request(Audit).await.unwrap();
                assert_eq!(handled, 0);
                let expected = if !split {
                    "2-5"
                } else if i == 0 {
                    "2"
                } else {
                    "3-5"
                };
                assert_eq!(mask, expected, "actual actor CPU placement");
            }
            for id in 0..100 {
                let reply = router.request(Work(id)).await.unwrap();
                check(reply, id, ac, bc);
            }
        });
        pin("10,11,12,13");
        let driver = runtime(4, "10,11,12,13", "isolation-driver");
        let producers = if busy { 128 } else { 0 };
        let barrier = Arc::new(tokio::sync::Barrier::new(producers + 2));
        let finish = Arc::new(std::sync::OnceLock::<Instant>::new());
        let mut jobs = Vec::new();
        for i in 0..producers {
            let actor = backgrounds[i % backgrounds.len()].clone();
            let barrier = barrier.clone();
            let finish = finish.clone();
            jobs.push(driver.spawn(async move {
                barrier.wait().await;
                let mut n = 0u64;
                while Instant::now() < *finish.get().unwrap() {
                    let reply = actor.request_raw(Work(n)).unwrap().await.unwrap();
                    check(reply, n, ab, bb);
                    n += 1;
                }
                n
            }));
        }
        let endpoint = if relay {
            router.clone()
        } else {
            critical.clone()
        };
        let cbarrier = barrier.clone();
        let cfinish = finish.clone();
        let cjob = driver.spawn(async move {
            cbarrier.wait().await;
            let mut latency = Vec::new();
            let mut n = 0;
            while Instant::now() < *cfinish.get().unwrap() {
                let start = Instant::now();
                let reply = endpoint.request_raw(Work(n)).unwrap().await.unwrap();
                let end = Instant::now();
                check(reply, n, ac, bc);
                latency.push(end.duration_since(start).as_nanos() as u64);
                n += 1;
            }
            latency
        });
        let hz = String::from_utf8(
            std::process::Command::new("getconf")
                .arg("CLK_TCK")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .parse::<f64>()
        .unwrap();
        let start = Instant::now();
        finish
            .set(start + Duration::from_secs_f64(seconds))
            .unwrap();
        let cpu = ticks();
        let (bg, mut latency) = driver.block_on(async {
            barrier.wait().await;
            let a = futures::future::join_all(jobs).await;
            let b = cjob.await.unwrap();
            (a.into_iter().map(Result::unwrap).sum::<u64>(), b)
        });
        let elapsed = start.elapsed().as_secs_f64();
        let cpu_s = (ticks() - cpu) as f64 / hz;
        let completed = latency.len();
        assert!(completed > 0);
        driver.block_on(async {
            let mut handled = 0;
            for actor in &actors {
                handled += actor.request(Audit).await.unwrap().1;
            }
            assert_eq!(
                handled,
                bg + (completed as u64) * if relay { 2 } else { 1 } + 200
            );
            for actor in &actors {
                actor.stop();
            }
            for actor in &actors {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(5), actor.wait_exit())
                        .await
                        .unwrap(),
                    ExitReason::Normal
                );
            }
        });
        drop(endpoint);
        drop(critical);
        drop(router);
        drop(backgrounds);
        drop(actors);
        drop(isolated);
        drop(general);
        assert_eq!(counts.started.load(Ordering::Relaxed), 18);
        assert_eq!(counts.stopped.load(Ordering::Relaxed), 18);
        assert_eq!(counts.dropped.load(Ordering::Relaxed), 18);
        latency.sort_unstable();
        let q = |p: usize| latency[(completed * p).div_ceil(100).saturating_sub(1)];
        println!("layout,path,load,elapsed_s,critical_completed,critical_per_s,critical_p50_ns,critical_p99_ns,bg_completed,bg_per_s,cpu_s,started,stopped,dropped");
        println!("{layout},{path},{load},{elapsed:.6},{completed},{:.3},{},{},{bg},{:.3},{cpu_s:.3},18,18,18",completed as f64/elapsed,q(50),q(99),bg as f64/elapsed);
    }
}
#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("runtime_isolation requires Linux and explicit fede CPU masks");
}
