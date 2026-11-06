//! Optional SIGPROF sampling for research processes where perf is unavailable.
//! Profiled runs must never be pooled with unprofiled throughput measurements.
#[cfg(all(target_os = "linux", feature = "profiling-experiments"))]
pub type Guard = pprof::ProfilerGuard<'static>;
#[cfg(not(all(target_os = "linux", feature = "profiling-experiments")))]
pub struct Guard;

pub fn start() -> Option<Guard> {
    let prefix = std::env::var_os("SPAWNED_PROFILE_PREFIX")?;
    assert!(!prefix.is_empty());
    #[cfg(all(target_os = "linux", feature = "profiling-experiments"))]
    return Some(
        pprof::ProfilerGuardBuilder::default()
            .frequency(199)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .expect("SIGPROF profiler startup"),
    );
    #[cfg(not(all(target_os = "linux", feature = "profiling-experiments")))]
    panic!("profiling requires Linux and profiling-experiments");
}

pub fn finish(guard: Option<Guard>) {
    #[cfg(all(target_os = "linux", feature = "profiling-experiments"))]
    if let Some(guard) = guard {
        use std::{fs::File, io::Write};
        let prefix = std::env::var("SPAWNED_PROFILE_PREFIX").unwrap();
        let report = guard.report().build().expect("profile report");
        assert!(!report.data.is_empty(), "no CPU profile samples collected");
        // Raw per-stack counts preserve thread identity for separate driver and
        // actor summaries. Counts are samples, not hardware counters or off-CPU time.
        let mut folded = File::create(format!("{prefix}.stacks")).unwrap();
        for (frames, count) in &report.data {
            let mut names = vec![frames.thread_name_or_id()];
            names.extend(
                frames
                    .frames
                    .iter()
                    .rev()
                    .flat_map(|f| f.iter().rev())
                    .map(|s| s.name().replace(';', ":").replace('\n', " ")),
            );
            writeln!(folded, "{} {}", names.join(";"), count).unwrap();
        }
        report
            .flamegraph(File::create(format!("{prefix}.svg")).unwrap())
            .unwrap();
    }
    #[cfg(not(all(target_os = "linux", feature = "profiling-experiments")))]
    let _ = guard;
}
