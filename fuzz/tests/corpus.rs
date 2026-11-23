//! Runs the committed seed corpus (`fuzz/seeds/`) and a batch of generated
//! inputs through the harness on stable Rust, without libFuzzer:
//!
//! ```sh
//! cd fuzz && RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test --test corpus
//! ```
//!
//! `SPAWNED_FUZZ_RANDOM=n` sets the number of generated inputs (default 64),
//! decoded as the fuzz target does (with fault-injection steps).
#![cfg(spawned_sim)]

use spawned_fuzz::{decode_as, execute, Format};

fn check(name: &str, bytes: &[u8], format: Format) -> bool {
    let Some(scenario) = decode_as(bytes, format) else {
        return true;
    };
    let run = execute(&scenario, None);
    if std::env::var_os("SPAWNED_FUZZ_TRACE").is_some() {
        eprintln!(
            "{name}: {} ops, {:?}, {} polls",
            scenario.ops.len(),
            scenario.sched,
            run.polls
        );
    }
    if let Some(failure) = run.failure {
        eprintln!("{name}: {failure}");
        if let Some(dir) = std::env::var_os("SPAWNED_FUZZ_DUMP") {
            let file = name.replace(|c: char| !c.is_ascii_alphanumeric(), "_");
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(std::path::Path::new(&dir).join(file), bytes);
        }
        for d in &run.details {
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
        // Seeds are base-format inputs; the fuzz target reads them with
        // fault steps, so run both decodings.
        if !check(&path.display().to_string(), &bytes, Format::Base)
            || !check(
                &format!("{} (faults)", path.display()),
                &bytes,
                Format::Faults,
            )
        {
            failed += 1;
        }
    }
    println!("{} seed inputs, {failed} failed", entries.len());
    assert_eq!(failed, 0);
}

/// `n` generated inputs (deterministic).
fn generated(n: u64) -> Vec<Vec<u8>> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    (0..n)
        .map(|i| {
            let len = 64 + (i as usize * 37) % 960;
            (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect()
        })
        .collect()
}

#[test]
fn generated_inputs_pass() {
    let n: u64 = std::env::var("SPAWNED_FUZZ_RANDOM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let mut failed = 0;
    for (i, bytes) in generated(n).iter().enumerate() {
        if !check(&format!("generated input {i}"), bytes, Format::Faults) {
            failed += 1;
        }
    }
    assert_eq!(failed, 0, "{failed} of {n} generated inputs failed");
}

/// A run's report (violation details, site statistics) is its own: the same
/// inputs give the same statistics run alone and on parallel threads.
#[test]
fn reports_are_per_run() {
    let inputs: Vec<_> = generated(64)
        .iter()
        .filter_map(|b| decode_as(b, Format::Faults))
        .collect();
    let alone: Vec<_> = inputs.iter().map(|s| execute(s, None).sites).collect();
    assert!(
        alone.iter().any(|s| s.fired.iter().any(|&n| n > 0)),
        "no generated input made a site go off"
    );
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for (s, expected) in inputs.iter().zip(&alone) {
                    let run = execute(s, None);
                    assert_eq!(&run.sites, expected);
                    assert!(run.details.is_empty());
                }
            });
        }
    });
}
