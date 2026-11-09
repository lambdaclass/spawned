#!/usr/bin/env bash
set -euo pipefail
cd /tmp/spawned-v07
source /tmp/spawned-fede-research-25dGGmPA/env.sh
export CARGO_PROFILE_TEST_DEBUG=1 CARGO_PROFILE_DEV_DEBUG=1 CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=4 CARGO_TARGET_DIR=/tmp/spawned-pr-security-review/system-target
cargo test --locked --workspace > /tmp/spawned-v07-timeouts-workspace.log 2>&1
cargo test --locked -p spawned-concurrency --features metrics > /tmp/spawned-v07-timeouts-metrics.log 2>&1
cargo test --locked -p spawned-concurrency --no-default-features --features threads,metrics > /tmp/spawned-v07-timeouts-native.log 2>&1
for example in protocol_timeouts streams_and_recipients registrations_and_fakes native_lifecycle coordinated_shutdown; do
  cargo run --locked -p spawned-capability-examples --bin "$example" > "/tmp/spawned-v07-timeouts-example-$example.log" 2>&1
done
python3 scripts/check-thread-only.py > /tmp/spawned-v07-timeouts-thread-graph.log 2>&1
export RUSTDOCFLAGS='-D warnings'
cargo doc --locked -p spawned-concurrency --no-deps > /tmp/spawned-v07-timeouts-docs.log 2>&1
cargo doc --locked -p spawned-concurrency --no-default-features --features threads,metrics --no-deps > /tmp/spawned-v07-timeouts-native-docs.log 2>&1
unset RUSTDOCFLAGS
export RUSTC=/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustc
export RUSTDOC=/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustdoc
export PATH=/nix/store/n88pdyarvdpyw98fahxcb03733nsclhq-clippy-1.98.1/bin:$PATH
export CARGO_TARGET_DIR=/tmp/spawned-pr-security-review/system-clippy198-target
cargo clippy --locked --workspace --all-targets --features spawned-concurrency/metrics -- -D warnings > /tmp/spawned-v07-timeouts-clippy.log 2>&1
cargo clippy --locked -p spawned-concurrency --all-targets --no-default-features --features threads,metrics -- -D warnings > /tmp/spawned-v07-timeouts-native-clippy.log 2>&1
