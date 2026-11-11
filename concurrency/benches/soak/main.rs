//! Long-running soak harness: hours of seeded actor churn, with resource
//! series sampled into a CSV and judged for growth. Linux only (`/proc`).
//! Run by hand, never in CI; see `docs/TESTING.md` ("Soak harness").
//!
//! ```text
//! CARGO_PROFILE_BENCH_DEBUG=line-tables-only cargo bench -p spawned-concurrency \
//!     --features metrics --bench soak -- --duration 4h --seed 1 --out /path/to/out
//! ```
//!
//! Exit code 0 is PASS, 1 is FAIL (a trend, a quiesce check or the workload),
//! 2 is a usage error.
mod native;
mod report;
mod state;
mod workload;

use report::{Phase, Sample, Thresholds, SERIES};
use spawned_concurrency::{metrics, registry};
use state::{Leaks, Rng, Stats, LEAKS, LIMIT, LIVE, STARTED};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    fs::{self, File},
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering::Relaxed},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

// ---------------------------------------------------------------------------
// Live-heap counter. Harness binary only: the library never installs an
// allocator. It costs one shared atomic per allocation, which slows the
// workload a little and does not change what is measured.
// ---------------------------------------------------------------------------

static HEAP: AtomicI64 = AtomicI64::new(0);
struct Counting;
// SAFETY: every call forwards to `System` unchanged; the counter only observes sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            HEAP.fetch_add(layout.size() as i64, Relaxed);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            HEAP.fetch_add(layout.size() as i64, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        HEAP.fetch_sub(layout.size() as i64, Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new) };
        if !p.is_null() {
            HEAP.fetch_add(new as i64 - layout.size() as i64, Relaxed);
        }
        p
    }
}
#[global_allocator]
static GLOBAL: Counting = Counting;

/// Bound for one async scenario; exceeding it is reported as a stuck episode.
const SCENARIO_LIMIT: Duration = Duration::from_secs(60);

struct Args {
    duration: Duration,
    seed: u64,
    sample: Duration,
    warmup: Option<Duration>,
    lanes: usize,
    workers: usize,
    out: PathBuf,
    config: PathBuf,
    leaks: Leaks,
    raw: String,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let mut total = 0f64;
    let mut number = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let n: f64 = number.parse().map_err(|_| format!("bad duration {s}"))?;
        number.clear();
        total += n * match c {
            'h' => 3600.0,
            'm' => 60.0,
            's' => 1.0,
            _ => return Err(format!("bad duration unit in {s} (use h, m, s)")),
        };
    }
    if !number.is_empty() {
        total += number
            .parse::<f64>()
            .map_err(|_| format!("bad duration {s}"))?;
    }
    Ok(Duration::from_secs_f64(total))
}

fn parse_args() -> Result<Args, String> {
    let mut pairs = Vec::new();
    let mut it = std::env::args().skip(1).filter(|a| a != "--bench");
    while let Some(arg) = it.next() {
        let key = arg
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument {arg}"))?;
        let (key, value) = match key.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                let v = it
                    .next()
                    .ok_or_else(|| format!("missing value for --{key}"))?;
                (key.to_string(), v)
            }
        };
        pairs.push((key, value));
    }
    let get = |k: &str| pairs.iter().rev().find(|p| p.0 == k).map(|p| p.1.clone());
    for (k, _) in &pairs {
        if ![
            "duration",
            "seed",
            "sample",
            "warmup",
            "lanes",
            "workers",
            "out",
            "config",
            "inject-leak",
        ]
        .contains(&k.as_str())
        {
            return Err(format!("unknown option --{k}"));
        }
    }
    let duration = parse_duration(&get("duration").unwrap_or("10m".into()))?;
    let seed = get("seed")
        .unwrap_or("1".into())
        .parse()
        .map_err(|_| "bad --seed")?;
    let sample = match get("sample") {
        Some(s) => parse_duration(&s)?,
        None => Duration::from_secs((duration.as_secs() / 360).clamp(1, 10)),
    };
    let warmup = get("warmup").map(|s| parse_duration(&s)).transpose()?;
    let lanes = get("lanes")
        .unwrap_or("4".into())
        .parse()
        .map_err(|_| "bad --lanes")?;
    let workers = get("workers")
        .unwrap_or("4".into())
        .parse()
        .map_err(|_| "bad --workers")?;
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out = get("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("soak-out/seed{seed}-{unix}")));
    let config = get("config").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/benches/soak/thresholds.toml"
        ))
    });
    let mut leaks = Leaks::default();
    if let Some(list) = get("inject-leak") {
        for kind in list.split(',') {
            match kind {
                "registry" => leaks.registry = true,
                "arc" => leaks.arc = true,
                "task" => leaks.task = true,
                "all" => {
                    leaks = Leaks {
                        registry: true,
                        arc: true,
                        task: true,
                    }
                }
                _ => return Err(format!("unknown leak {kind} (registry, arc, task, all)")),
            }
        }
    }
    if lanes == 0 || workers == 0 || duration.is_zero() || sample.is_zero() {
        return Err("--duration, --sample, --lanes and --workers must be positive".into());
    }
    let raw = pairs
        .iter()
        .map(|(k, v)| format!("--{k} {v}"))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Args {
        duration,
        seed,
        sample,
        warmup,
        lanes,
        workers,
        out,
        config,
        leaks,
        raw,
    })
}

fn proc_status(key: &str) -> f64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix(key)
                    .and_then(|v| v.split_whitespace().next()?.parse::<f64>().ok())
            })
        })
        .unwrap_or(f64::NAN)
}
fn count_dir(path: &str) -> f64 {
    fs::read_dir(path).map_or(f64::NAN, |d| d.count() as f64)
}

/// Shared sink for samples: kept in memory for the verdict and appended to the CSV.
struct Recorder {
    started: Instant,
    phase: AtomicU8,
    csv: Mutex<File>,
    samples: Mutex<Vec<Sample>>,
    runtime: tokio::runtime::Handle,
    stats: Arc<Stats>,
}
impl Recorder {
    fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Relaxed);
    }
    fn phase(&self) -> Phase {
        match self.phase.load(Relaxed) {
            0 => Phase::Baseline,
            1 => Phase::Warmup,
            2 => Phase::Steady,
            _ => Phase::Quiesce,
        }
    }
    fn sample(&self) -> Sample {
        let m = metrics::snapshot();
        let values = [
            proc_status("VmRSS:") / 1024.0,
            HEAP.load(Relaxed) as f64 / (1024.0 * 1024.0),
            count_dir("/proc/self/task"),
            count_dir("/proc/self/fd"),
            self.runtime.metrics().num_alive_tasks() as f64,
            registry::registered().len() as f64,
            LIVE.load(Relaxed) as f64,
            m.actors_active as f64,
            m.mailbox_depth as f64,
            m.handlers_active as f64,
            m.requests_active as f64,
        ];
        debug_assert_eq!(values.len(), SERIES.len());
        let sample = Sample {
            elapsed: self.started.elapsed().as_secs_f64(),
            phase: self.phase(),
            values,
            started: STARTED.load(Relaxed),
            scenarios: self.stats.total_runs(),
            errors: self.stats.errors(),
        };
        let mut csv = self.csv.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(csv, "{}", report::csv_row(&sample));
        let _ = csv.flush();
        drop(csv);
        self.samples
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(sample.clone());
        sample
    }
}

fn lane_seed(seed: u64, lane: u64) -> u64 {
    Rng::new(seed ^ lane.wrapping_mul(0xa076_1d64_78bd_642f)).next()
}

async fn async_lane(env: Arc<workload::Env>, mut rng: Rng) {
    while !env.stats.stopping() {
        let pick = rng.below(state::ASYNC_SCENARIOS as u64) as usize;
        match tokio::time::timeout(SCENARIO_LIMIT, workload::run(pick, &mut rng, &env)).await {
            Ok(Ok(())) => env.stats.ran(pick),
            Ok(Err(err)) => env
                .stats
                .error(format!("{}: {err}", state::SCENARIOS[pick])),
            Err(_) => env.stats.error(format!(
                "{}: stuck for {SCENARIO_LIMIT:?}",
                state::SCENARIOS[pick]
            )),
        }
    }
}

fn threads_lane(stats: &Stats, mut rng: Rng, once: bool) {
    let mut first = 0;
    while (once && first < native::COUNT) || (!once && !stats.stopping()) {
        let pick = if once {
            first += 1;
            first - 1
        } else {
            rng.below(native::COUNT as u64) as usize
        };
        let index = state::ASYNC_SCENARIOS + pick;
        match native::run(pick, &mut rng, stats) {
            Ok(()) => stats.ran(index),
            Err(err) => stats.error(format!("{}: {err}", state::SCENARIOS[index])),
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(err) => {
            eprintln!("soak: {err}");
            eprintln!(
                "usage: soak [--duration 4h] [--seed N] [--sample 10s] [--warmup 30m] [--lanes 4] \
                 [--workers 4] [--out DIR] [--config thresholds.toml] [--inject-leak registry,arc,task|all]"
            );
            std::process::exit(2);
        }
    };
    if !std::path::Path::new("/proc/self/task").is_dir() {
        eprintln!("soak: needs Linux /proc (threads, fds and RSS are read from it)");
        std::process::exit(2);
    }
    let thresholds = match Thresholds::load(&args.config) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("soak: {err}");
            std::process::exit(2);
        }
    };
    LEAKS.set(args.leaks).unwrap();
    // Counters and gauges only: no sampled clocks.
    let _ = metrics::configure(metrics::Config {
        timing_sample_every: 0,
    });
    // Injected crashes are expected millions of times; keep other panics loud.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let injected = info
            .payload()
            .downcast_ref::<&str>()
            .is_some_and(|s| s.starts_with("soak:"));
        if !injected {
            default_hook(info);
        }
    }));
    let warmup = args.warmup.unwrap_or_else(|| {
        let fraction = thresholds.or("warmup.fraction", 0.25);
        let max = thresholds.or("warmup.max_secs", 1800.0);
        Duration::from_secs_f64((args.duration.as_secs_f64() * fraction).min(max))
    });
    fs::create_dir_all(&args.out).expect("create --out directory");
    let mut csv = File::create(args.out.join("samples.csv")).expect("create samples.csv");
    writeln!(csv, "{}", report::csv_header()).unwrap();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.workers)
        .thread_keep_alive(Duration::from_secs(2))
        .thread_name("soak-worker")
        .enable_all()
        .build()
        .expect("runtime");
    let stats = Arc::new(Stats::new());
    let recorder = Arc::new(Recorder {
        started: Instant::now(),
        phase: AtomicU8::new(Phase::Baseline as u8),
        csv: Mutex::new(csv),
        samples: Mutex::new(Vec::new()),
        runtime: runtime.handle().clone(),
        stats: stats.clone(),
    });
    println!(
        "soak: {} for {:?} (warm-up {:?}, sample {:?}), output {}",
        args.raw,
        args.duration,
        warmup,
        args.sample,
        args.out.display()
    );

    // Prime every scenario once, so one-time lazy initialisation (runtime
    // slabs, registry capacity, thread-locals) lands before the baseline.
    let prime_stats = Arc::new(Stats::new());
    runtime.block_on(async {
        let env = Arc::new(workload::Env::new(prime_stats.clone()).expect("prime system"));
        let mut rng = Rng::new(lane_seed(args.seed, 999));
        for pick in 0..state::ASYNC_SCENARIOS {
            if let Err(err) = workload::run(pick, &mut rng, &env).await {
                stats.error(format!("prime {}: {err}", state::SCENARIOS[pick]));
            }
        }
        let report = env.system.shutdown(LIMIT).await;
        assert!(report.complete, "prime system shutdown incomplete");
    });
    threads_lane(&prime_stats, Rng::new(lane_seed(args.seed, 998)), true);
    std::thread::sleep(Duration::from_secs(4));

    // Sampler: the baseline first, then one row per interval until quiesce.
    for _ in 0..3 {
        recorder.sample();
        std::thread::sleep(Duration::from_secs(1));
    }
    recorder.set_phase(Phase::Warmup);
    let sampling = Arc::new(AtomicBool::new(true));
    let sampler = {
        let (recorder, sampling, every) = (recorder.clone(), sampling.clone(), args.sample);
        std::thread::Builder::new()
            .name("soak-sampler".into())
            .spawn(move || {
                let mut next = Instant::now();
                while sampling.load(Relaxed) {
                    next += every;
                    recorder.sample();
                    while sampling.load(Relaxed) && Instant::now() < next {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .unwrap()
    };

    let env = Arc::new(
        runtime
            .block_on(async { workload::Env::new(stats.clone()) })
            .expect("system"),
    );
    let lanes: Vec<_> = (0..args.lanes)
        .map(|i| {
            let env = env.clone();
            runtime.spawn(async_lane(env, Rng::new(lane_seed(args.seed, i as u64))))
        })
        .collect();
    let (native_done, native_finished) = mpsc::channel();
    {
        let (stats, seed) = (stats.clone(), lane_seed(args.seed, args.lanes as u64));
        std::thread::Builder::new()
            .name("soak-threads-lane".into())
            .spawn(move || {
                threads_lane(&stats, Rng::new(seed), false);
                let _ = native_done.send(());
            })
            .unwrap();
    }

    let begin = Instant::now();
    let report_every = Duration::from_secs(60).max(args.sample);
    let mut next_report = begin + report_every;
    while begin.elapsed() < args.duration {
        if begin.elapsed() >= warmup {
            recorder.set_phase(Phase::Steady);
        }
        if Instant::now() >= next_report {
            next_report += report_every;
            println!(
                "soak: {:>6.0}s actors started {} scenarios {} errors {} heap {:.1} MiB",
                begin.elapsed().as_secs_f64(),
                STARTED.load(Relaxed),
                stats.total_runs(),
                stats.errors(),
                HEAP.load(Relaxed) as f64 / 1048576.0
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Quiesce: stop the lanes, shut the long-lived system down, then wait for
    // every count to return to its baseline.
    stats.stop.store(true, Relaxed);
    let quiesce_start = Instant::now();
    let mut lanes_joined = runtime.block_on(async {
        tokio::time::timeout(SCENARIO_LIMIT * 2, futures::future::join_all(lanes))
            .await
            .is_ok()
    });
    lanes_joined &= native_finished.recv_timeout(SCENARIO_LIMIT * 2).is_ok();
    let system_shutdown = runtime.block_on(async {
        let report = env.system.shutdown(LIMIT).await;
        if report.complete {
            Ok(())
        } else {
            Err(format!(
                "groups {:?}, actors {:?}",
                report.unfinished_groups, report.unfinished_actor_details
            ))
        }
    });
    drop(env);
    sampling.store(false, Relaxed);
    sampler.join().unwrap();
    recorder.set_phase(Phase::Quiesce);
    let timeout = Duration::from_secs_f64(thresholds.or("quiesce.timeout_secs", 60.0));
    let baseline = recorder
        .samples
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|s| matches!(s.phase, Phase::Baseline))
        .unwrap()
        .clone();
    let settled = loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = recorder.sample();
        let ok = SERIES.iter().enumerate().all(|(i, name)| {
            thresholds
                .get(&format!("quiesce.{name}"))
                .is_none_or(|slack| now.values[i] <= baseline.values[i] + slack)
        });
        if ok {
            break true;
        }
        if quiesce_start.elapsed() > timeout {
            break false;
        }
    };
    let samples = recorder.samples.lock().unwrap().clone();
    let verdict = report::judge(
        &thresholds,
        &samples,
        &report::RunInfo {
            args: &args.raw,
            warmup_secs: warmup.as_secs_f64(),
            runs: stats.runs(),
            capacity: stats.capacity.load(Relaxed),
            errors: stats.errors(),
            first_errors: stats.first_errors(),
            quiesce_secs: quiesce_start.elapsed().as_secs_f64(),
            quiesce_settled: settled,
            lanes_joined,
            system_shutdown,
        },
    );
    let leaks = if args.leaks.registry || args.leaks.arc || args.leaks.task {
        format!("injected leaks: {:?}\n", args.leaks)
    } else {
        String::new()
    };
    let text = format!("{leaks}{}", verdict.text);
    fs::write(args.out.join("summary.txt"), &text).expect("write summary.txt");
    println!("\n{text}");
    println!("samples: {}", args.out.join("samples.csv").display());
    // Injected leaks keep a pending task on the runtime: don't wait for it.
    runtime.shutdown_background();
    std::process::exit(if verdict.pass { 0 } else { 1 });
}
