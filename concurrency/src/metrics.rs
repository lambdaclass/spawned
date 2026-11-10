//! Optional, bounded framework metrics (`metrics` Cargo feature).
//!
//! Counters cover all operations; histograms contain sampled elapsed times.
//! There are no actor/message labels, callbacks, background tasks or network I/O.
//! See `docs/GUIDE.md` (Metrics section) for accounting and raw-receiver limitations.
//!
//! ```
//! use spawned_concurrency::metrics::{self, Config};
//! metrics::configure(Config { timing_sample_every: 16 }).unwrap();
//! let snapshot = metrics::snapshot();
//! assert_eq!(snapshot.actors_active, 0);
//! let text = metrics::render_prometheus();
//! assert!(text.contains("spawned_mailbox_depth"));
//! ```
use crate::{ActorError, ExitReason};
use std::{
    fmt::Write,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc, Mutex, OnceLock,
    },
};
// Virtual time under `--cfg spawned_sim`; `std::time::Instant` otherwise.
use spawned_rt::time::Instant;

/// Timing samples are deterministic, one in every N observations per category.
/// Zero disables clocks/histograms while retaining counters and gauges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub timing_sample_every: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            timing_sample_every: 1,
        }
    }
}
struct SamplingPolicy {
    // Only explicit requirements compose. Implicit default instrumentation must
    // not prevent an embedded component from configuring sampling later.
    requested: Mutex<Option<u64>>,
    interval: AtomicU64,
}
impl SamplingPolicy {
    fn new(requested: Option<u64>) -> Self {
        Self {
            interval: AtomicU64::new(requested.unwrap_or(1)),
            requested: Mutex::new(requested),
        }
    }
}
static CONFIG: OnceLock<SamplingPolicy> = OnceLock::new();
/// Set timing policy before creating actors, messages or observed requests.
/// Returns the supplied configuration if instrumentation has already initialized.
pub fn configure(config: Config) -> Result<(), Config> {
    CONFIG
        .set(SamplingPolicy::new(Some(config.timing_sample_every)))
        .map_err(|_| config)
}
/// Merge a component's process-lifetime timing requirement, even after actors start.
///
/// The most frequent requested sampling wins: the smallest nonzero interval.
/// Zero requests counters only and never disables another component's timings.
/// Requirements cannot be withdrawn; counters and histograms remain process-wide.
/// This opt-in alternative to [`configure`] lets embedded libraries compose
/// without racing to initialize a single immutable policy.
pub fn configure_composable(config: Config) {
    let policy = CONFIG.get_or_init(|| SamplingPolicy::new(None));
    let mut requested = policy.requested.lock().unwrap_or_else(|p| p.into_inner());
    let interval = requested.map_or(config.timing_sample_every, |current| {
        merge_sampling(current, config.timing_sample_every)
    });
    *requested = Some(interval);
    policy.interval.store(interval, Relaxed);
}

/// The current effective policy. Reading this initializes the default policy
/// (sample every observation) if no component has configured instrumentation.
pub fn configuration() -> Config {
    Config {
        timing_sample_every: CONFIG
            .get_or_init(|| SamplingPolicy::new(None))
            .interval
            .load(Relaxed),
    }
}

fn merge_sampling(current: u64, requested: u64) -> u64 {
    match (current, requested) {
        (0, value) | (value, 0) => value,
        _ => current.min(requested),
    }
}

fn sample(sequence: &AtomicU64) -> Option<Instant> {
    let every = configuration().timing_sample_every;
    (every != 0 && sequence.fetch_add(1, Relaxed).is_multiple_of(every)).then(Instant::now)
}

/// Non-cumulative bucket upper limits in nanoseconds. The last bucket is +Inf.
pub const BUCKET_BOUNDS_NS: [u64; 32] = {
    let mut bounds = [u64::MAX; 32];
    let mut i = 0;
    while i < 31 {
        bounds[i] = 1_000u64 << i;
        i += 1;
    }
    bounds
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistogramSnapshot {
    pub buckets: [u64; 32],
    pub count: u64,
    pub sum_ns: u64,
    pub max_ns: u64,
}
struct Histogram {
    buckets: [AtomicU64; 32],
    count: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
}
impl Histogram {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; 32],
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            max: AtomicU64::new(0),
        }
    }
    fn record(&self, elapsed: std::time::Duration) {
        let ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        let bucket = BUCKET_BOUNDS_NS
            .iter()
            .position(|&bound| ns <= bound)
            .unwrap_or(31);
        self.buckets[bucket].fetch_add(1, Relaxed);
        self.sum
            .fetch_update(Relaxed, Relaxed, |sum| Some(sum.saturating_add(ns)))
            .unwrap();
        self.max.fetch_max(ns, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }
    fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            buckets: std::array::from_fn(|i| self.buckets[i].load(Relaxed)),
            count: self.count.load(Relaxed),
            sum_ns: self.sum.load(Relaxed),
            max_ns: self.max.load(Relaxed),
        }
    }
}
macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        struct Counters { $( $name: AtomicU64, )* }
        static COUNTERS: Counters = Counters { $( $name: AtomicU64::new(0), )* };
        /// Process aggregates. Reads are independent atomic samples, not a
        /// transaction. Conservation equations hold after operations quiesce.
        #[derive(Debug, Clone)]
        pub struct Snapshot { $( pub $name: u64, )* pub queue_wait: HistogramSnapshot, pub handler_duration: HistogramSnapshot, pub request_latency: HistogramSnapshot }
        pub fn snapshot() -> Snapshot { Snapshot { $( $name: COUNTERS.$name.load(Relaxed), )* queue_wait: QUEUE_WAIT.snapshot(), handler_duration: HANDLER_DURATION.snapshot(), request_latency: REQUEST_LATENCY.snapshot() } }
        fn render_counters(out: &mut String, s: &Snapshot) {
            $(
                let name = concat!("spawned_", stringify!($name));
                let kind = if name.ends_with("_total") { "counter" } else { "gauge" };
                writeln!(out, "# TYPE {name} {kind}\n{name} {}", s.$name).unwrap();
            )*
        }
    };
}
counters!(
    actors_spawned_total,
    actors_active,
    actors_exited_normal_total,
    actors_exited_shutdown_total,
    actors_exited_panic_total,
    actors_exited_kill_total,
    messages_enqueued_total,
    messages_rejected_total,
    mailbox_depth,
    messages_handled_total,
    messages_discarded_total,
    handlers_active,
    handlers_completed_total,
    handlers_panicked_total,
    handlers_aborted_total,
    replies_sent_total,
    reply_receivers_dropped_total,
    requests_started_total,
    requests_active,
    requests_replied_total,
    requests_timed_out_total,
    requests_actor_stopped_total,
    requests_cancelled_total,
);
static QUEUE_WAIT: Histogram = Histogram::new();
static HANDLER_DURATION: Histogram = Histogram::new();
static REQUEST_LATENCY: Histogram = Histogram::new();
static MESSAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Render a fixed set of Prometheus text-format counters, gauges and histograms.
/// Call from the application's exporter/HTTP task; no server is started here.
pub fn render_prometheus() -> String {
    let snapshot = snapshot();
    let mut out = String::with_capacity(16_384);
    render_counters(&mut out, &snapshot);
    for (name, hist) in [
        ("spawned_queue_wait_seconds", &snapshot.queue_wait),
        (
            "spawned_handler_duration_seconds",
            &snapshot.handler_duration,
        ),
        ("spawned_request_latency_seconds", &snapshot.request_latency),
    ] {
        writeln!(out, "# TYPE {name} histogram").unwrap();
        let mut count = 0;
        for (&bound, &bucket) in BUCKET_BOUNDS_NS.iter().zip(&hist.buckets) {
            count += bucket;
            if bound == u64::MAX {
                writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {count}").unwrap();
            } else {
                writeln!(
                    out,
                    "{name}_bucket{{le=\"{}\"}} {count}",
                    bound as f64 / 1e9
                )
                .unwrap();
            }
        }
        // Derive count from this snapshot's buckets so +Inf always equals count,
        // even if an observation arrived between independent atomic loads.
        writeln!(
            out,
            "{name}_count {count}\n{name}_sum {}\n# TYPE {name}_max gauge\n{name}_max {}",
            hist.sum_ns as f64 / 1e9,
            hist.max_ns as f64 / 1e9
        )
        .unwrap();
    }
    out
}

#[derive(Clone, Default)]
pub(crate) struct Mailbox(Arc<AtomicU64>);
impl Mailbox {
    pub(crate) fn depth(&self) -> u64 {
        self.0.load(Relaxed)
    }
    pub(crate) fn enqueued(&self) -> MessageObservation {
        self.0.fetch_add(1, Relaxed);
        COUNTERS.mailbox_depth.fetch_add(1, Relaxed);
        MessageObservation {
            mailbox: self.clone(),
            submitted: sample(&MESSAGE_SEQUENCE),
            queued: true,
            rejected: false,
        }
    }
}
pub(crate) fn accepted() {
    COUNTERS.messages_enqueued_total.fetch_add(1, Relaxed);
}
pub(crate) fn reply(delivered: bool) {
    if delivered {
        &COUNTERS.replies_sent_total
    } else {
        &COUNTERS.reply_receivers_dropped_total
    }
    .fetch_add(1, Relaxed);
}
pub(crate) struct MessageObservation {
    mailbox: Mailbox,
    submitted: Option<Instant>,
    queued: bool,
    rejected: bool,
}
impl MessageObservation {
    fn release(&mut self) {
        self.queued = false;
        self.mailbox.0.fetch_sub(1, Relaxed);
        COUNTERS.mailbox_depth.fetch_sub(1, Relaxed);
    }
    pub(crate) fn begin(mut self) -> HandlerObservation {
        self.release();
        COUNTERS.messages_handled_total.fetch_add(1, Relaxed);
        COUNTERS.handlers_active.fetch_add(1, Relaxed);
        let began = self.submitted.map(|submitted| {
            let now = Instant::now();
            QUEUE_WAIT.record(now.duration_since(submitted));
            now
        });
        HandlerObservation {
            began,
            completed: false,
        }
    }
    pub(crate) fn reject(mut self) {
        self.rejected = true;
    }
}
impl Drop for MessageObservation {
    fn drop(&mut self) {
        if self.queued {
            self.release();
            if self.rejected {
                &COUNTERS.messages_rejected_total
            } else {
                &COUNTERS.messages_discarded_total
            }
            .fetch_add(1, Relaxed);
        }
    }
}
pub(crate) struct HandlerObservation {
    began: Option<Instant>,
    completed: bool,
}
impl HandlerObservation {
    pub(crate) fn finish(mut self, panicked: bool) {
        self.completed = true;
        if panicked {
            &COUNTERS.handlers_panicked_total
        } else {
            &COUNTERS.handlers_completed_total
        }
        .fetch_add(1, Relaxed);
    }
}
impl Drop for HandlerObservation {
    fn drop(&mut self) {
        COUNTERS.handlers_active.fetch_sub(1, Relaxed);
        if !self.completed {
            COUNTERS.handlers_aborted_total.fetch_add(1, Relaxed);
        }
        if let Some(began) = self.began {
            HANDLER_DURATION.record(began.elapsed());
        }
    }
}
pub(crate) struct ActorObservation {
    finished: bool,
}
impl ActorObservation {
    pub(crate) fn new() -> Self {
        let _ = configuration();
        COUNTERS.actors_spawned_total.fetch_add(1, Relaxed);
        COUNTERS.actors_active.fetch_add(1, Relaxed);
        Self { finished: false }
    }
    pub(crate) fn finish(&mut self, reason: &ExitReason) {
        if self.finished {
            return;
        }
        self.finished = true;
        COUNTERS.actors_active.fetch_sub(1, Relaxed);
        match reason {
            ExitReason::Normal => &COUNTERS.actors_exited_normal_total,
            ExitReason::Shutdown => &COUNTERS.actors_exited_shutdown_total,
            ExitReason::Panic(_) => &COUNTERS.actors_exited_panic_total,
            ExitReason::Kill => &COUNTERS.actors_exited_kill_total,
        }
        .fetch_add(1, Relaxed);
    }
}
impl Drop for ActorObservation {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(&ExitReason::Kill);
        }
    }
}
pub(crate) struct RequestObservation {
    began: Option<Instant>,
    finished: bool,
}
impl RequestObservation {
    pub(crate) fn new() -> Self {
        COUNTERS.requests_started_total.fetch_add(1, Relaxed);
        COUNTERS.requests_active.fetch_add(1, Relaxed);
        Self {
            began: sample(&REQUEST_SEQUENCE),
            finished: false,
        }
    }
    pub(crate) fn finish<T>(mut self, result: &Result<T, ActorError>) {
        self.finished = true;
        match result {
            Ok(_) => &COUNTERS.requests_replied_total,
            Err(ActorError::RequestTimeout) => &COUNTERS.requests_timed_out_total,
            Err(ActorError::ActorStopped) => &COUNTERS.requests_actor_stopped_total,
        }
        .fetch_add(1, Relaxed);
    }
}
impl Drop for RequestObservation {
    fn drop(&mut self) {
        COUNTERS.requests_active.fetch_sub(1, Relaxed);
        if !self.finished {
            COUNTERS.requests_cancelled_total.fetch_add(1, Relaxed);
        }
        if let Some(began) = self.began {
            REQUEST_LATENCY.record(began.elapsed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn histogram_boundaries_and_overflow_bucket() {
        let histogram = Histogram::new();
        for ns in [0, 1_000, 1_001, 2_000, u64::MAX] {
            histogram.record(std::time::Duration::from_nanos(ns));
        }
        let s = histogram.snapshot();
        assert_eq!(s.count, 5);
        assert_eq!(s.buckets[0], 2);
        assert_eq!(s.buckets[1], 2);
        assert_eq!(s.buckets[31], 1);
        assert_eq!(s.buckets.iter().sum::<u64>(), s.count);
        assert_eq!(s.max_ns, u64::MAX);
        assert_eq!(s.sum_ns, u64::MAX);
        assert!(BUCKET_BOUNDS_NS.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
