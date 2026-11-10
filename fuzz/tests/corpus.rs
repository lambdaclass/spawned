//! Runs the committed seed corpus (`fuzz/seeds/`) and a batch of generated
//! inputs through the harness on stable Rust, without libFuzzer:
//!
//! ```sh
//! cd fuzz && RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test --test corpus
//! ```
//!
//! `SPAWNED_FUZZ_RANDOM=n` sets the number of generated inputs (default 64).
#![cfg(spawned_sim)]

use spawned_fuzz::{decode, execute, last_details};

fn check(name: &str, bytes: &[u8]) -> bool {
    let Some(scenario) = decode(bytes) else {
        return true;
    };
    let run = execute(&scenario, None);
    if std::env::var_os("SPAWNED_FUZZ_TRACE").is_some() {
        eprintln!("{name}: {} ops, {:?}, {} polls", scenario.ops.len(), scenario.sched, run.polls);
    }
    if let Some(failure) = run.failure {
        eprintln!("{name}: {failure}");
        for d in last_details() {
            eprintln!("  {d}");
        }
        return false;
    }
    true
}

#[test]
fn seed_corpus_passes() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    let mut failed = 0;
    for path in &entries {
        let bytes = std::fs::read(path).unwrap();
        if !check(&path.display().to_string(), &bytes) {
            failed += 1;
        }
    }
    println!("{} seed inputs, {failed} failed", entries.len());
    assert_eq!(failed, 0);
}

#[test]
fn generated_inputs_pass() {
    let n: u64 = std::env::var("SPAWNED_FUZZ_RANDOM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut failed = 0;
    for i in 0..n {
        let len = 64 + (i as usize * 37) % 960;
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        if !check(&format!("generated input {i}"), &bytes) {
            failed += 1;
        }
    }
    assert_eq!(failed, 0, "{failed} of {n} generated inputs failed");
}
