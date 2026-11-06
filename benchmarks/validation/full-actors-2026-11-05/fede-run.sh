#!/usr/bin/env bash
set -euo pipefail
REPRO_ROOT=/tmp/spawned-fede-repro-50cQ1RW4
export PATH=/nix/store/5gk010lr12bwp64cm9rj3pj21c0hviw8-rustc-1.93.0/bin:$PATH
export RUSTC=/nix/store/5gk010lr12bwp64cm9rj3pj21c0hviw8-rustc-1.93.0/bin/rustc
export RUSTDOC=/nix/store/5gk010lr12bwp64cm9rj3pj21c0hviw8-rustc-1.93.0/bin/rustdoc
export CARGO_TARGET_DIR="$REPRO_ROOT/target"
export CARGO_BUILD_JOBS=4
export LC_ALL=C LANG=C
export PKG_CONFIG=/nix/store/j5fz28pic24azmxkv183hk6fchfs7ag8-pkg-config-0.29.2/bin/pkg-config
export PKG_CONFIG_PATH=/nix/store/1p14m1hrdkykyxs0l6wg7y29ahppcnsr-openssl-3.6.1-dev/lib/pkgconfig
export LD_LIBRARY_PATH=/nix/store/p96a7p297gmia8zcy4i72qd45wzw8lh6-openssl-3.6.1/lib:${LD_LIBRARY_PATH-}
unset RUSTFLAGS RUSTC_WRAPPER RUSTDOCFLAGS SPAWNED_EXPERIMENT_SCHEDULER SPAWNED_EXPERIMENT_RUNTIME SPAWNED_EXPERIMENT_WORKERS SPAWNED_EXPERIMENT_BATCH TOKIO_WORKER_THREADS
cd "$REPRO_ROOT/source"
trap 'printf "failed\n" > "$REPRO_ROOT/state"' ERR
printf 'correctness\n' > "$REPRO_ROOT/state"
git checkout --quiet --detach 22d3ad4ca00f0cedd8e1e9bce3578da55c399fca
rustc --version
cargo --version
cargo test --locked --workspace --all-features > "$REPRO_ROOT/default-tests.log" 2>&1
SPAWNED_EXPERIMENT_SCHEDULER=sharded SPAWNED_EXPERIMENT_WORKERS=2 SPAWNED_EXPERIMENT_BATCH=0 cargo test --locked -p spawned-concurrency --all-features -- --test-threads=1 --skip badly_behaved_thread_non_blocking > "$REPRO_ROOT/sharded-tests.log" 2>&1
PROPTEST_CASES=1000 cargo test --locked -p spawned-concurrency --all-features --test actor_scenarios generated_actor_scenarios -- --test-threads=1 > "$REPRO_ROOT/scenario-fixed-stress.log" 2>&1
printf 'benchmark-2\n' > "$REPRO_ROOT/state"
git checkout --quiet --detach 9b223463599ffb18615e0fc7f538924bd3edd8e7
python3 benchmarks/design_experiments.py --output "$REPRO_ROOT/fede2" --group messages --variants async actor-tokio-1 actor-tokio-16 actor-tokio-adaptive actor-sharded-1 actor-sharded-16 actor-sharded-adaptive --cases rpc1 active128_p128 active1024_p256 hotspot128 async_wait128 blocking128 tcp128 idle4 alloc1 --workers 2 --repetitions 5 > "$REPRO_ROOT/fede2.log" 2>&1
printf 'benchmark-4\n' > "$REPRO_ROOT/state"
python3 benchmarks/design_experiments.py --output "$REPRO_ROOT/fede4" --group messages --variants async actor-tokio-1 actor-tokio-16 actor-tokio-adaptive actor-sharded-1 actor-sharded-16 actor-sharded-adaptive --cases active128_p128 active1024_p256 hotspot128 async_wait128 blocking128 tcp128 --workers 4 --repetitions 5 > "$REPRO_ROOT/fede4.log" 2>&1
printf 'complete\n' > "$REPRO_ROOT/state"
