# Backend experiment validation

All public actor/protocol signatures and default runtime branches are retained.
Runtime alternatives require the explicit `backend-experiments` feature and a
private environment selector; native schedulers and I/O controls are bench-only.
The normal async reply transport remains Tokio oneshot.

[Validation logs and hashes](validation/designs-2026-11-05/sha256.json) accompany
the [measurement manifest](results/designs-2026-11-05/manifest.json). Archived
CSV files use LF line endings with identical field values; both original and
archived CSV hashes are retained. Log trailing whitespace is normalized.

| Check | Evidence / scope |
|---|---|
| Workspace tests, all features | Passed at corrected source `aae6312`; subsequent Rust changes affect the I/O benchmark only. |
| Runtime characterization | Three tests passed separately for default, one-worker, current-thread, caller-shared and process-managed modes. Tests explicitly expect their documented helper-progress and caller-lifetime differences. |
| Shared scheduling gate, bounded Loom model | Passed at `aae6312`, preemption bound 3, up to 10,000 permutations; actual gate code imported by model. This is not exhaustive proof of the complete scheduler. |
| Native one/four-reactor I/O smoke | 14/14 cases passed on macOS, source `53eb0d0`. |
| Linux I/O smoke, including explicit io_uring | 20/20 cases passed, source `d4c7e1f`. |
| Latest bench build | Both release benches compile; full native and Linux measurements exercise four-reactor paths. |
| Latest Clippy | All targets and all features passed with `-D warnings`. A separate target directory matches the installed Clippy compiler. |
| Formatting and patch whitespace | `cargo fmt --all -- --check` and `git diff --check` passed. |
| Recorded measurements | 1,125 successes and 9 resource-limit failures across 1,134 samples; 113,200,977 successful measured requests/echoes. |

## Failures are evidence

- macOS `dedicated-current` at 1,024 actors: three `Too many open files` failures
  with a 4,096 soft limit. Three separate runs with a child-only 16,384 limit pass.
- `Backend::Blocking` at 1,024 actors: three 15-second watchdogs each on macOS
  and Linux. The 512-actor control completes three times on each platform.
  Persistent loops exhaust the default finite blocking-worker budget; throughput
  is absent for watchdogs. The benchmark's actor warmup waits for every actor.
- Initial sandbox-denied attempts are excluded, as are smoke results and the old
  atomic-gate exploratory campaign. No fallback results are labeled io_uring.

## Reproduction commands

```sh
cargo test --locked --workspace --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
RUSTFLAGS='--cfg spawned_scheduler_loom' CARGO_TARGET_DIR=/tmp/spawned-loom \
  cargo test -p spawned-concurrency --features backend-experiments --test ready_gate_loom
SPAWNED_EXPERIMENT_RUNTIME=current \
  cargo test -p spawned-concurrency --features backend-experiments --test backend_experiments
```

Run each runtime selector in a separate process, including with the selector
unset for default. Tests do not mutate environment variables inside a process.
Some tests require loopback sockets. Linux and native timing must run sequentially
without builds, model checks or chart rendering competing for CPU.

Linux uses `spawned-perf:rust193-arm64` built from `benchmarks/Dockerfile`, an
unprivileged OrbStack container with `--cpuset-cpus=0-3 --memory=8g
--pids-limit=4096 --security-opt seccomp=unconfined`, read-only source at `/work`,
separate output and Cargo registry volumes. The extra seccomp setting permits
io_uring in this VM experiment. Compiler, CPU set, resource limits, schedules,
features and binary hashes are in each campaign's metadata.

The Linux main campaign uses cases `rpc1 active128_p128 active512_p128
active1024_p256 hotspot128 idle4 alloc1` and variants `async threads plain-tokio
dedicated-current dedicated-managed native-block sharded-16 ready-16
ready-adaptive local-ready-16`, with three repetitions and a 30-second watchdog.
The pool control uses four workers on `active128_p128 active1024_p256 hotspot128`
for `async plain-tokio ready-16 ready-adaptive local-ready-16 sharded-16`.
The separate blocking capacity campaign uses a 15-second watchdog.

Native macOS has the full main matrix, a 16-worker control, a descriptor-limit
control, a large-capacity burst control, and supplemental I/O/blocking campaigns.
The exact cells and invocation configuration are retained in metadata and CSV.

Before promoting any candidate, test full actor lifecycle, async suspension,
panic cleanup, cancellation, monitors, dynamic actor registration, timer/I/O
compatibility, skew, long-running saturation, and resource ceilings. No native
prototype in this study meets all those requirements yet.
