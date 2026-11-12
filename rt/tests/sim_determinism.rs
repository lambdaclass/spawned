//! Determinism D1: under `--cfg spawned_sim`, wall-clock and OS-entropy reads
//! on the simulated thread come from the world (virtual time, seeded bytes)
//! through `spawned-sim-libc`'s interposers; other threads read the real
//! clock; strict mode turns such a read into a failure naming the caller.
#![cfg(all(spawned_sim, feature = "async"))]

use rand::{Rng, RngCore};
use spawned_rt::tasks::{self as rt, sim};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Everything a run observed through std, rand and the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    instant_debug: String,
    instant_steps: Vec<Duration>,
    system_time: Vec<Duration>,
    thread_rng: Vec<u64>,
    os_rng: u64,
    getrandom: [u8; 16],
    map_order: Vec<u32>,
    set_order: Vec<u32>,
    /// A thread that does not run the world read a real date.
    other_thread_real: bool,
}

async fn observe(out: Arc<Mutex<Vec<Observed>>>) {
    let start = std::time::Instant::now();
    let mut instant_steps = Vec::new();
    let mut system_time = Vec::new();
    for step in [0u64, 250, 1_500] {
        rt::sleep(Duration::from_millis(step)).await;
        instant_steps.push(start.elapsed());
        system_time.push(SystemTime::now().duration_since(UNIX_EPOCH).unwrap());
    }
    let mut rng = rand::thread_rng();
    let thread_rng = (0..4).map(|_| rng.gen::<u64>()).collect();
    let os_rng = rand::rngs::OsRng.next_u64();
    let mut getrandom = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut getrandom);
    let map: HashMap<u32, ()> = (0..64).map(|k| (k, ())).collect();
    let set: HashSet<u32> = (0..64).collect();
    // A thread that does not run the world reads the real clock.
    let other_thread_wall =
        std::thread::spawn(|| SystemTime::now().duration_since(UNIX_EPOCH).unwrap())
            .join()
            .unwrap();
    out.lock().unwrap().push(Observed {
        instant_debug: format!("{start:?}"),
        instant_steps,
        system_time,
        thread_rng,
        os_rng,
        getrandom,
        map_order: map.keys().copied().collect(),
        set_order: set.iter().copied().collect(),
        other_thread_real: other_thread_wall > Duration::from_secs(1_700_000_000),
    });
}

fn run(seed: u64) -> Observed {
    let out = Arc::new(Mutex::new(Vec::new()));
    let outcome = sim::run_once(seed, None, &|_| observe(out.clone()));
    assert_eq!(outcome.failure, None);
    let observed = out.lock().unwrap().pop().unwrap();
    observed
}

#[test]
fn clock_and_entropy_are_virtual_and_seeded() {
    if !sim::INTERPOSED {
        eprintln!("this target does not interpose libc; skipped");
        return;
    }
    let a = run(7);
    // Virtual time: exact, starting at the virtual epoch.
    assert_eq!(
        a.instant_steps,
        [
            Duration::ZERO,
            Duration::from_millis(250),
            Duration::from_millis(1_750)
        ]
    );
    assert_eq!(
        a.system_time,
        [
            sim::VIRTUAL_EPOCH,
            sim::VIRTUAL_EPOCH + Duration::from_millis(250),
            sim::VIRTUAL_EPOCH + Duration::from_millis(1_750),
        ]
    );
    // Another thread: real time (well after the virtual epoch of 2000).
    assert!(a.other_thread_real);

    // Same seed, a later run in this process: identical observations,
    // including the absolute `Instant`, `HashMap` order and `thread_rng`.
    assert_eq!(run(7), a);

    // A different seed draws different bytes and orders maps differently.
    let b = run(8);
    assert_ne!(b.thread_rng, a.thread_rng);
    assert_ne!(b.os_rng, a.os_rng);
    assert_ne!(b.getrandom, a.getrandom);
    assert_ne!(b.map_order, a.map_order);
    // Time does not depend on the seed.
    assert_eq!(b.system_time, a.system_time);
}

/// A seed observed in the middle of a sweep matches the same seed run alone
/// (each run has a fresh thread, so per-thread seeding happens inside it).
#[test]
fn a_seed_in_a_sweep_matches_the_seed_alone() {
    if !sim::INTERPOSED {
        return;
    }
    let sweep: Vec<Observed> = (0..4).map(run).collect();
    assert_eq!(run(2), sweep[2]);
}

/// The same body replayed from its recorded tape sees the same values.
#[test]
fn a_tape_replay_sees_the_same_values() {
    if !sim::INTERPOSED {
        return;
    }
    let out = Arc::new(Mutex::new(Vec::new()));
    let body = |_| observe(out.clone());
    let recorded = sim::run_once(5, None, &body);
    let replayed = sim::run_once(5, Some(&recorded.tape), &body);
    assert_eq!(replayed.failure, None);
    let seen = out.lock().unwrap().clone();
    assert_eq!(seen[0], seen[1]);
}

#[test]
fn strict_mode_fails_the_run_naming_the_caller() {
    if !sim::INTERPOSED {
        return;
    }
    #[inline(never)]
    fn reads_the_wall_clock() -> SystemTime {
        SystemTime::now()
    }
    let outcome = sim::run_once(1, None, &|_| async {
        sim::set_strict(true);
        // Not reported: `HashMap` keys, and the routed clock.
        let map: HashMap<u8, u8> = HashMap::new();
        drop(map);
        let _ = spawned_rt::time::Instant::now();
        rt::sleep(Duration::from_millis(1)).await;
        let _ = reads_the_wall_clock();
        rt::sleep(Duration::from_millis(1)).await;
    });
    let failure = outcome.failure.expect("strict mode must fail the run");
    assert!(
        failure.contains("determinism leak: clock_gettime(CLOCK_REALTIME)")
            && failure.contains("SPAWNED_SIM_STRICT=1")
            && failure.contains("reads_the_wall_clock"),
        "{failure}"
    );

    let outcome = sim::run_once(1, None, &|_| async {
        sim::set_strict(true);
        let _: u64 = rand::thread_rng().gen();
        rt::yield_now().await;
    });
    let failure = outcome.failure.expect("strict mode must fail the run");
    assert!(failure.contains("determinism leak"), "{failure}");
}

/// Run by `separate_processes_observe_the_same_values` in child processes.
#[test]
#[ignore = "run in a child process by separate_processes_observe_the_same_values"]
fn observe_child() {
    let seed = std::env::var("OBSERVE_SEED").unwrap().parse().unwrap();
    println!("OBSERVED {:?}", run(seed));
}

/// The same seed in two separate processes (fresh process-wide state, a
/// different OS entropy pool) observes identical values.
#[test]
fn separate_processes_observe_the_same_values() {
    if !sim::INTERPOSED {
        return;
    }
    let observe = |seed: u64| {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "observe_child", "--nocapture"])
            .env("OBSERVE_SEED", seed.to_string())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "{stdout}");
        stdout
            .lines()
            .find(|line| line.starts_with("OBSERVED "))
            .unwrap_or_else(|| panic!("no observation in {stdout}"))
            .to_owned()
    };
    let first = observe(11);
    assert_eq!(observe(11), first);
    assert_ne!(observe(12), first);
}
