//! Panic-injection coverage of a corpus: for every injection site, how many
//! runs armed it and made it go off, and how often.
//!
//! ```sh
//! cd fuzz
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" \
//!   cargo run --release --example sites -- <corpus-dir> [max-inputs]
//! ```

use spawned_fuzz::{decode_faults, execute, last_sites};

const NAMES: [&str; 16] = [
    "MSG_DROP",
    "MSG_CLONE",
    "REPLY_DROP",
    "RESULT_DROP",
    "DONE_DROP",
    "DONE_PANIC",
    "WORK_PANIC",
    "DOWN_PANIC",
    "EXIT_PANIC",
    "PAYLOAD_DROP",
    "FMT",
    "TRACING",
    "CONFIGURE_PANIC",
    "EMIT_DROP",
    "NOTE_PANIC",
    "HELPER_DROP",
];

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
    let (mut armed, mut fired) = ([0u64; 16], [0u64; 16]);
    let (mut runs_armed, mut runs_fired) = ([0u64; 16], [0u64; 16]);
    let (mut runs, mut failed) = (0u64, 0u64);
    for path in paths.iter().take(max) {
        let Some(scenario) = decode_faults(&std::fs::read(path).unwrap_or_default()) else {
            continue;
        };
        runs += 1;
        failed += execute(&scenario, None).failure.is_some() as u64;
        let (a, f, _) = last_sites();
        for i in 0..16 {
            armed[i] += a[i];
            fired[i] += f[i];
            runs_armed[i] += (a[i] > 0) as u64;
            runs_fired[i] += (f[i] > 0) as u64;
        }
    }
    println!("{runs} inputs, {failed} failing");
    println!(
        "{:<16} {:>12} {:>12} {:>12} {:>12}",
        "site", "runs armed", "runs fired", "armed", "fired"
    );
    for i in 0..16 {
        println!(
            "{:<16} {:>12} {:>12} {:>12} {:>12}",
            NAMES[i], runs_armed[i], runs_fired[i], armed[i], fired[i]
        );
    }
}
