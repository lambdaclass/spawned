//! Panic-injection coverage of a corpus: for every injection site, how many
//! runs armed it and made it go off, and how often. "payloads armed" counts
//! payloads created while their site was armed (payload sites only; a
//! callback site has no payload).
//!
//! ```sh
//! cd fuzz
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" \
//!   cargo run --release --example sites -- <corpus-dir> [max-inputs]
//! ```

use spawned_fuzz::{decode_faults, execute, site::NAMES};

const N: usize = NAMES.len();

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first() else {
        eprintln!("usage: sites <corpus-dir> [max-inputs]");
        std::process::exit(2);
    };
    let max: usize = args
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("read corpus")
        .flatten()
        .map(|e| e.path())
        .collect();
    paths.sort();
    let (mut armed, mut fired) = ([0u64; N], [0u64; N]);
    let (mut runs_armed, mut runs_fired) = ([0u64; N], [0u64; N]);
    let (mut runs, mut failed) = (0u64, 0u64);
    for path in paths.iter().take(max) {
        let Some(scenario) = decode_faults(&std::fs::read(path).unwrap_or_default()) else {
            continue;
        };
        runs += 1;
        let run = execute(&scenario, None);
        failed += run.failure.is_some() as u64;
        let sites = run.sites;
        for i in 0..N {
            armed[i] += sites.payloads_armed[i];
            fired[i] += sites.fired[i];
            runs_armed[i] += (sites.armed & (1 << i) != 0) as u64;
            runs_fired[i] += (sites.fired[i] > 0) as u64;
        }
    }
    println!("{runs} inputs, {failed} failing");
    println!(
        "{:<16} {:>12} {:>12} {:>12} {:>12}",
        "site", "runs armed", "runs fired", "payloads armed", "fired"
    );
    for i in 0..N {
        println!(
            "{:<16} {:>12} {:>12} {:>12} {:>12}",
            NAMES[i], runs_armed[i], runs_fired[i], armed[i], fired[i]
        );
    }
}
