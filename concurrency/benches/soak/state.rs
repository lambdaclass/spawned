//! Harness-side counters, the seeded RNG, scenario statistics and leak injection.
use std::sync::{
    atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed},
    Arc, Mutex, OnceLock,
};
use std::time::Duration;

/// Bound for any single wait inside a scenario (an exit, a restart, a Down).
pub const LIMIT: Duration = Duration::from_secs(10);

/// Actor states constructed and not yet dropped: "started minus fully stopped".
pub static LIVE: AtomicI64 = AtomicI64::new(0);
/// `started()` hooks run, both modes.
pub static STARTED: AtomicU64 = AtomicU64::new(0);

/// Held in every harness actor; its drop is the "fully stopped" event.
pub struct Live(());
impl Live {
    pub fn new() -> Self {
        LIVE.fetch_add(1, Relaxed);
        Self(())
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Relaxed);
    }
}

/// SplitMix64: small, seedable and good enough to pick scenarios.
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    /// True with probability 1/n.
    pub fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
    pub fn ms(&mut self, max: u64) -> Duration {
        Duration::from_millis(self.below(max + 1))
    }
}

/// Planted leaks for the harness's negative control (`--inject-leak`).
#[derive(Default, Debug, Clone, Copy)]
pub struct Leaks {
    /// Never unregister one in four unmanaged registry names.
    pub registry: bool,
    /// Leak a 4 KiB `Arc` and an actor-state token on every supervised (re)start.
    pub arc: bool,
    /// Leak a pending Tokio task per supervisor cycle.
    pub task: bool,
}
pub static LEAKS: OnceLock<Leaks> = OnceLock::new();
static LEAKED: Mutex<Vec<Arc<[u8]>>> = Mutex::new(Vec::new());
pub fn leaks() -> Leaks {
    LEAKS.get().copied().unwrap_or_default()
}
/// Called from every supervised child factory.
pub fn leak_on_restart() {
    if leaks().arc {
        LEAKED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::from(vec![7u8; 4096]));
        std::mem::forget(Live::new());
    }
}

pub const SCENARIOS: &[&str] = &[
    // async lanes (tasks mode)
    "churn_async",
    "churn_blocking",
    "churn_thread",
    "group",
    "supervisor_one_for_one",
    "supervisor_one_for_all",
    "supervisor_rest_for_one",
    "supervisor_nested",
    "links",
    "monitors",
    "timers",
    "registry",
    "requests",
    "streams",
    "system_cycle",
    "burst",
    // threads lane (threads mode)
    "threads_churn",
    "threads_links_monitors",
    "threads_timers_streams",
    "threads_registry_requests",
    "threads_burst",
];
pub const ASYNC_SCENARIOS: usize = 16;

pub struct Stats {
    runs: Vec<AtomicU64>,
    pub capacity: AtomicU64,
    errors: AtomicU64,
    first_errors: Mutex<Vec<String>>,
    pub stop: AtomicBool,
    seq: AtomicU64,
}
impl Stats {
    pub fn new() -> Self {
        Self {
            runs: SCENARIOS.iter().map(|_| AtomicU64::new(0)).collect(),
            capacity: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            first_errors: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            seq: AtomicU64::new(0),
        }
    }
    pub fn ran(&self, scenario: usize) {
        self.runs[scenario].fetch_add(1, Relaxed);
    }
    pub fn error(&self, what: impl Into<String>) {
        self.errors.fetch_add(1, Relaxed);
        let mut first = self.first_errors.lock().unwrap_or_else(|p| p.into_inner());
        if first.len() < 20 {
            let what = what.into();
            eprintln!("soak error: {what}");
            first.push(what);
        }
    }
    pub fn errors(&self) -> u64 {
        self.errors.load(Relaxed)
    }
    pub fn first_errors(&self) -> Vec<String> {
        self.first_errors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn runs(&self) -> Vec<(&'static str, u64)> {
        SCENARIOS
            .iter()
            .zip(&self.runs)
            .map(|(n, c)| (*n, c.load(Relaxed)))
            .collect()
    }
    pub fn total_runs(&self) -> u64 {
        self.runs.iter().map(|c| c.load(Relaxed)).sum()
    }
    /// Process-unique suffix for registry names.
    pub fn next_id(&self) -> u64 {
        self.seq.fetch_add(1, Relaxed)
    }
    pub fn stopping(&self) -> bool {
        self.stop.load(Relaxed)
    }
}

/// `return Err(..)` from a scenario unless the condition holds.
macro_rules! ensure {
    ($cond:expr, $($fmt:tt)+) => {
        if !$cond {
            return Err(format!($($fmt)+));
        }
    };
}
pub(crate) use ensure;

pub type R = Result<(), String>;
pub fn e<T: std::fmt::Debug>(what: &'static str) -> impl FnOnce(T) -> String {
    move |err| format!("{what}: {err:?}")
}
