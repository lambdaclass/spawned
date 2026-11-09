#!/usr/bin/env bash
set -euo pipefail
cd /tmp/spawned-v07
source /tmp/spawned-fede-research-25dGGmPA/env.sh
export CARGO_PROFILE_TEST_DEBUG=1 CARGO_PROFILE_DEV_DEBUG=1 CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=4 CARGO_TARGET_DIR=/tmp/spawned-pr-security-review/system-target
cargo test --locked --offline --workspace > /tmp/spawned-v07-gates-workspace.log 2>&1
cargo test --locked --offline -p spawned-concurrency --no-default-features --features threads,metrics > /tmp/spawned-v07-gates-native.log 2>&1
PROPTEST_CASES=512 PROPTEST_RNG_SEED=20261109 cargo test --locked --offline -p spawned-concurrency --features metrics --test supervisor_policies > /tmp/spawned-v07-gates-policy-512.log 2>&1
RUSTDOCFLAGS='-D warnings' cargo doc --locked --offline -p spawned-concurrency --no-deps > /tmp/spawned-v07-gates-docs.log 2>&1
export RUSTC=/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustc
export RUSTDOC=/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustdoc
export PATH=/nix/store/n88pdyarvdpyw98fahxcb03733nsclhq-clippy-1.98.1/bin:$PATH
export CARGO_TARGET_DIR=/tmp/spawned-pr-security-review/system-clippy198-target
cargo clippy --locked --offline --workspace --all-targets --features spawned-concurrency/metrics -- -D warnings > /tmp/spawned-v07-gates-clippy.log 2>&1
cargo clippy --locked --offline -p spawned-concurrency --all-targets --no-default-features --features threads,metrics -- -D warnings > /tmp/spawned-v07-gates-native-clippy.log 2>&1
