//! Named fault-injection sites in Spawned's own lifecycle code
//! (FoundationDB `BUGGIFY` / etcd `gofail` style).
//!
//! Available only with the `failpoints` feature, which is off by default.
//! Without the feature the sites compile to nothing: no evaluation, no site
//! names and no registry exist in the binary.
//!
//! Each site in [`SITES`] sits where a real race or failure can happen
//! (restart, backoff, publication, cleanup, registration, linking, actor
//! startup and stop). Tests switch sites at runtime:
//!
//! ```ignore
//! use spawned_concurrency::failpoints::{self, Action, FailPoint, FailScenario};
//! let scenario = FailScenario::setup(); // serializes tests, clears on drop
//! failpoints::configure("child.factory", FailPoint::new(Action::Error).times(2)).unwrap();
//! // ... drive a supervisor; the first two constructions fail ...
//! assert_eq!(failpoints::stats("child.factory").fired, 2);
//! drop(scenario);
//! ```
//!
//! Probabilistic sites draw from a per-site generator derived from one seed
//! ([`set_seed`], or `SPAWNED_FAILPOINTS_SEED` read by [`FailScenario::setup`]).
//! The seed is included in injected panic messages and printed when a test
//! holding a [`FailScenario`] panics, so a failure can be replayed. Under
//! concurrency the order in which actors reach a site can still vary.
//!
//! Configuration is process-global, and every actor in the process reaches
//! the armed sites. Tests in one binary that use failpoints should hold a
//! [`FailScenario`]; keep them in a binary of their own (or bound sites with
//! `times`) so unrelated parallel tests are not hit.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex, MutexGuard,
    },
    time::Duration,
};

/// A failpoint site and what triggering it models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Site {
    pub name: &'static str,
    /// Whether [`Action::Error`] has an error path at this site. At sites
    /// without one, `Error` counts as fired but has no effect.
    pub has_error: bool,
    pub description: &'static str,
}

/// Every site compiled into the library with the `failpoints` feature.
pub const SITES: &[Site] = &[
    Site {
        name: "actor.before_started",
        has_error: false,
        description: "Before `started()` runs (tasks and threads), inside its panic guard. \
                      Panic models a failing `started()`; delay widens the window in which \
                      a stop, link exit or shutdown arrives before startup.",
    },
    Site {
        name: "actor.after_started_before_ready",
        has_error: false,
        description: "Managed tasks actor after `started()` returned, before its scope is \
                      marked ready. Delay races readiness against owner shutdown: a closed \
                      scope must not publish ready.",
    },
    Site {
        name: "actor.before_stopped",
        has_error: false,
        description: "Before `stopped()` runs (tasks and threads), inside its panic guard. \
                      Delay models slow cleanup (shutdown deadlines); panic models a \
                      failing `stopped()`.",
    },
    Site {
        name: "reply.close_upgraded",
        has_error: false,
        description: "The death point's close holds an upgraded deferred slot, before \
                      closing it. Delay lets the caller drop its receiver meanwhile: an \
                      unread reply must still be destroyed only after every caller is \
                      answered (tasks and threads).",
    },
    Site {
        name: "monitor.down_before_dispatch",
        has_error: false,
        description: "A monitor's `Down` dequeued by its observer (tasks and threads), \
                      before the flush check. Delay lets a `demonitor` on another thread \
                      return while the `Down` is in dispatch: it must not reach \
                      `Handler<Down>` (OTP `demonitor(Ref, [flush])`).",
    },
    Site {
        name: "child.factory",
        has_error: true,
        description: "Supervised child construction, before the user factory runs, inside \
                      its panic guard. Error or panic is a failed start; delay can exceed \
                      the startup timeout.",
    },
    Site {
        name: "child.after_ready_before_publish",
        has_error: true,
        description: "A child generation became ready but the supervisor has not yet \
                      published it as current. Delay races publication against stop and \
                      recovery; error discards the ready generation as a failed start.",
    },
    Site {
        name: "child.before_cleanup_join",
        has_error: false,
        description: "A child generation ended; its controller is about to wait for the \
                      owned scope to join within the remaining shutdown budget. Delay \
                      models a slowly scheduled controller.",
    },
    Site {
        name: "supervisor.before_restart",
        has_error: false,
        description: "A child controller decided to restart and is entering backoff. Panic \
                      models a controller bug; delay races the restart with stop.",
    },
    Site {
        name: "supervisor.during_backoff",
        has_error: false,
        description: "Backoff elapsed; before the restart budget is taken. Delay races a \
                      stop or recovery round with the retry decision.",
    },
    Site {
        name: "shutdown.before_stop_child",
        has_error: false,
        description: "Supervisor stop turn, before signalling the next child (reverse \
                      declaration order). Delay consumes the shutdown deadline.",
    },
    Site {
        name: "system.shutdown_before_target",
        has_error: false,
        description: "ActorSystem shutdown coordinator, before stopping the next unit or \
                      standalone scope. Delay consumes the system grace period.",
    },
    Site {
        name: "system.group_start",
        has_error: true,
        description: "Execution group construction after its runtimes/threads were built. \
                      Error exercises rollback of partially built groups.",
    },
    Site {
        name: "system.reserve",
        has_error: true,
        description: "Admission of a new scope on an execution group (actor start or \
                      supervised generation). Error reports `SystemError::Capacity`.",
    },
    Site {
        name: "registry.before_register",
        has_error: true,
        description: "Managed `register_name`, before the owner's admission is checked. \
                      Delay races registration with shutdown (a closed owner must not \
                      leave an entry); error reports `OwnerClosed`.",
    },
    Site {
        name: "registry.before_unregister",
        has_error: false,
        description: "An owned registration is being removed during scope cleanup. Delay \
                      checks that a replacement only starts after the old name is gone.",
    },
    Site {
        name: "actor.at_death",
        has_error: false,
        description: "Right after the death point D (the final reason is fixed, admission \
                      refuses with `NoProc`), before the open deferred replies are closed \
                      (tasks and threads). Delay widens the window in which an external \
                      `ReplyTo::reply` races the death: it must not succeed once `NoProc` \
                      was observed.",
    },
    Site {
        name: "actor.exit_before_deliver",
        has_error: false,
        description: "`ctx.exit(target, reason)` after its sender-lifetime check, before the \
                      signal is delivered (tasks and threads). Delay lets the sender reach \
                      its death point meanwhile: its death notice must not overtake the \
                      signal.",
    },
    Site {
        name: "link.before_install",
        has_error: false,
        description: "`ctx.link(..)` before both link tables are updated (tasks and \
                      threads). Delay lets the peer exit first; the exit must still be \
                      delivered.",
    },
    Site {
        name: "monitor.before_install",
        has_error: false,
        description:
            "`ctx.monitor(..)` (tasks and threads) before the monitor is registered. Delay races \
                      registration with the target's exit or the owner's stop.",
    },
];

/// What a triggered site does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Configured but inert (still counts hits).
    Off,
    /// Panic at the site.
    Panic,
    /// Take the site's error path (see [`Site::has_error`]).
    Error,
    /// Sleep: `std::thread::sleep` at synchronous sites, async sleep at async
    /// sites (thread sleep when no Tokio runtime is current).
    Delay(Duration),
    /// Yield once to the scheduler at async sites; a thread yield at
    /// synchronous sites.
    Yield,
}

/// Site configuration: an action plus optional probability and limits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FailPoint {
    action: Action,
    probability: f64,
    skip: u64,
    times: Option<u64>,
}
impl FailPoint {
    pub fn new(action: Action) -> Self {
        Self {
            action,
            probability: 1.0,
            skip: 0,
            times: None,
        }
    }
    /// Fire with probability `p` (clamped to `0.0..=1.0`) per eligible hit,
    /// drawn from the seeded per-site generator.
    pub fn probability(mut self, p: f64) -> Self {
        self.probability = if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
        self
    }
    /// Ignore the first `n` hits.
    pub fn skip(mut self, n: u64) -> Self {
        self.skip = n;
        self
    }
    /// Fire at most `n` times, then behave as [`Action::Off`].
    pub fn times(mut self, n: u64) -> Self {
        self.times = Some(n);
        self
    }
}
impl From<Action> for FailPoint {
    fn from(action: Action) -> Self {
        Self::new(action)
    }
}

/// Counters for one site since it was configured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Times execution reached the site while it was configured.
    pub hits: u64,
    /// Times the action was taken.
    pub fired: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown failpoint site: {0}")]
pub struct UnknownSite(pub String);

struct State {
    config: FailPoint,
    rng: u64,
    stats: Stats,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SEED: AtomicU64 = AtomicU64::new(0);
static REGISTRY: Mutex<BTreeMap<&'static str, State>> = Mutex::new(BTreeMap::new());
static SCENARIO: Mutex<()> = Mutex::new(());

fn registry() -> MutexGuard<'static, BTreeMap<&'static str, State>> {
    REGISTRY.lock().unwrap_or_else(|p| p.into_inner())
}

fn site(name: &str) -> Result<&'static Site, UnknownSite> {
    #[cfg(not(test))]
    let test_sites: &[Site] = &[];
    #[cfg(test)]
    let test_sites = tests::SITES;
    SITES
        .iter()
        .chain(test_sites)
        .find(|s| s.name == name)
        .ok_or_else(|| UnknownSite(name.to_string()))
}

fn site_rng(seed: u64, name: &str) -> u64 {
    // FNV-1a of the name, so each site's sequence is independent of how often
    // other sites are evaluated.
    let hash = name.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    seed ^ hash
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

/// Configure a site, replacing any previous configuration and its counters.
pub fn configure(name: &str, failpoint: impl Into<FailPoint>) -> Result<(), UnknownSite> {
    let site = site(name)?;
    let mut registry = registry();
    registry.insert(
        site.name,
        State {
            config: failpoint.into(),
            rng: site_rng(SEED.load(Ordering::Relaxed), site.name),
            stats: Stats::default(),
        },
    );
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

/// Remove one site's configuration.
pub fn remove(name: &str) {
    let mut registry = registry();
    registry.remove(name);
    ACTIVE.store(!registry.is_empty(), Ordering::Release);
}

/// Remove every site's configuration and counters.
pub fn clear() {
    let mut registry = registry();
    registry.clear();
    ACTIVE.store(false, Ordering::Release);
}

/// Hits and fires of a configured site (zero when not configured).
pub fn stats(name: &str) -> Stats {
    registry()
        .get(name)
        .map(|state| state.stats)
        .unwrap_or_default()
}

/// Seed for probabilistic sites. Re-seeds already configured sites.
pub fn set_seed(seed: u64) {
    SEED.store(seed, Ordering::Relaxed);
    for (name, state) in registry().iter_mut() {
        state.rng = site_rng(seed, name);
    }
}

/// Current seed.
pub fn seed() -> u64 {
    SEED.load(Ordering::Relaxed)
}

/// Serializes failpoint tests in one process and resets all sites on entry and
/// exit. When dropped during a panic it prints the seed so the run can be
/// replayed with `SPAWNED_FAILPOINTS_SEED`.
pub struct FailScenario {
    _serial: MutexGuard<'static, ()>,
}
impl FailScenario {
    /// Seed from `SPAWNED_FAILPOINTS_SEED` when set, otherwise from the clock.
    pub fn setup() -> Self {
        let seed = std::env::var("SPAWNED_FAILPOINTS_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64
            });
        Self::with_seed(seed)
    }
    pub fn with_seed(seed: u64) -> Self {
        let serial = SCENARIO.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        set_seed(seed);
        Self { _serial: serial }
    }
}
impl Drop for FailScenario {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Test-facing replay hint; tracing may have no subscriber here.
            eprintln!(
                "failpoints: scenario failed with SPAWNED_FAILPOINTS_SEED={}",
                seed()
            );
        }
        clear();
    }
}

enum Fired {
    Error,
    Delay(Duration),
    Yield,
}

fn decide(name: &'static str) -> Option<Fired> {
    if !ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    debug_assert!(
        site(name).is_ok(),
        "failpoint site {name} missing from SITES"
    );
    let action = {
        let mut registry = registry();
        let state = registry.get_mut(name)?;
        state.stats.hits += 1;
        if state.config.skip > 0 {
            state.config.skip -= 1;
            return None;
        }
        if state.config.times == Some(0) || state.config.action == Action::Off {
            return None;
        }
        if state.config.probability < 1.0 {
            let draw = (splitmix(&mut state.rng) >> 11) as f64 / (1u64 << 53) as f64;
            if draw >= state.config.probability {
                return None;
            }
        }
        if let Some(times) = &mut state.config.times {
            *times -= 1;
        }
        state.stats.fired += 1;
        state.config.action
    };
    match action {
        Action::Off => None,
        Action::Panic => panic!("failpoint {name} injected panic (seed {})", seed()),
        Action::Error => Some(Fired::Error),
        Action::Delay(delay) => Some(Fired::Delay(delay)),
        Action::Yield => Some(Fired::Yield),
    }
}

/// Evaluate a synchronous site. Returns whether the error path was requested.
#[doc(hidden)]
pub fn eval_sync(name: &'static str) -> bool {
    match decide(name) {
        None => false,
        Some(Fired::Error) => true,
        Some(Fired::Delay(delay)) => {
            std::thread::sleep(delay);
            false
        }
        Some(Fired::Yield) => {
            std::thread::yield_now();
            false
        }
    }
}

/// Evaluate an asynchronous site. Returns whether the error path was requested.
#[doc(hidden)]
#[cfg(feature = "async")]
pub async fn eval_async(name: &'static str) -> bool {
    match decide(name) {
        None => false,
        Some(Fired::Error) => true,
        Some(Fired::Delay(delay)) => {
            if spawned_rt::tasks::Handle::try_current().is_ok() {
                spawned_rt::tasks::sleep(delay).await;
            } else {
                std::thread::sleep(delay);
            }
            false
        }
        Some(Fired::Yield) => {
            YieldOnce(false).await;
            false
        }
    }
}

#[cfg(feature = "async")]
struct YieldOnce(bool);
#[cfg(feature = "async")]
impl std::future::Future for YieldOnce {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.0 {
            return std::task::Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests share a process with the library's other tests, which reach
    // the real sites; arm only these private names here.
    pub(super) const SITES: &[Site] = &[
        Site {
            name: "test.a",
            has_error: true,
            description: "unit test",
        },
        Site {
            name: "test.b",
            has_error: true,
            description: "unit test",
        },
    ];

    #[test]
    fn unknown_sites_are_rejected() {
        let _scenario = FailScenario::with_seed(1);
        assert_eq!(
            configure("no.such.site", Action::Panic),
            Err(UnknownSite("no.such.site".into()))
        );
    }

    #[test]
    fn site_names_are_unique() {
        let mut names: Vec<_> = super::SITES.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), super::SITES.len());
    }

    #[test]
    fn skip_and_times_bound_fires() {
        let _scenario = FailScenario::with_seed(1);
        configure("test.a", FailPoint::new(Action::Error).skip(2).times(3)).unwrap();
        let fired: Vec<_> = (0..8).map(|_| eval_sync("test.a")).collect();
        assert_eq!(fired, [false, false, true, true, true, false, false, false]);
        assert_eq!(stats("test.a"), Stats { hits: 8, fired: 3 });
        remove("test.a");
        assert!(!eval_sync("test.a"));
        assert_eq!(stats("test.a"), Stats::default());
    }

    #[test]
    fn probability_is_reproducible_from_the_seed() {
        let run = |seed| {
            let _scenario = FailScenario::with_seed(seed);
            configure("test.b", FailPoint::new(Action::Error).probability(0.5)).unwrap();
            // Other sites' evaluations do not perturb this site's sequence.
            configure("test.a", FailPoint::new(Action::Error).probability(0.5)).unwrap();
            (0..256)
                .map(|i| {
                    if i % 3 == 0 {
                        eval_sync("test.a");
                    }
                    eval_sync("test.b")
                })
                .collect::<Vec<_>>()
        };
        let a = run(42);
        assert_eq!(a, run(42));
        assert_ne!(a, run(43));
        let fired = a.iter().filter(|f| **f).count();
        assert!((64..192).contains(&fired), "fired {fired} of 256");
    }

    #[test]
    fn panic_names_site_and_seed() {
        let _scenario = FailScenario::with_seed(99);
        configure("test.a", Action::Panic).unwrap();
        let payload = std::panic::catch_unwind(|| eval_sync("test.a")).unwrap_err();
        let message = crate::error::panic_message(&*payload);
        assert!(message.contains("test.a"), "{message}");
        assert!(message.contains("seed 99"), "{message}");
    }

    #[test]
    fn unconfigured_and_off_sites_do_nothing() {
        let _scenario = FailScenario::with_seed(1);
        assert!(!eval_sync("test.a"));
        configure("test.a", Action::Off).unwrap();
        assert!(!eval_sync("test.a"));
        assert_eq!(stats("test.a"), Stats { hits: 1, fired: 0 });
    }
}
