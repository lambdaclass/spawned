#!/usr/bin/env bash
# Run one client's compatibility gates from its workspace root.
#
#   client-compat-gates.sh ethrex|ethlambda
#
# Every gate runs even if an earlier one failed, so one report shows all
# breakage. Each gate's output goes to the console and to
# $RUNNER_TEMP/client-compat-logs/<gate>.log; a result table goes to the job
# summary. Exits non-zero if any gate failed.
set -uo pipefail

client=$1
tmp=${RUNNER_TEMP:-/tmp}
logs=$tmp/client-compat-logs
summary=${GITHUB_STEP_SUMMARY:-/dev/stdout}
mkdir -p "$logs"

{
  echo "### $client gates"
  echo
  echo "| Gate | Result | Duration |"
  echo "|------|--------|----------|"
} >>"$summary"

failed=0
gate() {
  local name=$1; shift
  local start rc
  echo "::group::$name: $*"
  start=$(date +%s)
  "$@" 2>&1 | tee "$logs/$name.log"
  rc=${PIPESTATUS[0]}
  local secs=$(($(date +%s) - start))
  echo "::endgroup::"
  local result=pass
  if [ "$rc" -ne 0 ]; then
    result="**FAIL** (exit $rc)"
    failed=1
    echo "::error title=client-compat $client::gate '$name' failed (exit $rc): $*"
  fi
  printf '| `%s` | %s | %dm%02ds |\n' "$*" "$result" $((secs / 60)) $((secs % 60)) >>"$summary"
  df -h / | tail -1
}

# Workspace members that depend directly on a spawned crate, as -p flags.
spawned_dependents() {
  jq -r '
    (.workspace_members) as $members
    | .packages[]
    | select(.id as $id | $members | index($id))
    | select(any(.dependencies[]; .name | startswith("spawned-")))
    | "-p \(.name)"
  ' "$tmp/client-metadata.json" | sort | tr '\n' ' '
}

case "$client" in
  ethrex)
    pkgs=$(spawned_dependents)
    echo "spawned dependents: $pkgs"
    gate check cargo check --workspace --all-targets
    # shellcheck disable=SC2086 # one -p flag per package
    gate clippy cargo clippy $pkgs --all-targets -- -D warnings
    gate test-p2p cargo test --no-fail-fast -p ethrex-p2p
    gate test-p2p-group cargo test -p ethrex-test --test ethrex_tests p2p::
    gate test-rpc-lib cargo test -p ethrex-rpc --lib
    # Debug build at the default 2 MiB test-thread stack: no RUST_MIN_STACK.
    gate test-rpc-group cargo test -p ethrex-test --test ethrex_tests rpc::
    gate test-ethrex-lib cargo test -p ethrex --lib
    ;;
  ethlambda)
    gate clippy cargo clippy --workspace --all-targets -- -D warnings
    # ethlambda's suite is only supported under its `release-fast` profile
    # (its Makefile `test` target): in plain debug, signature checks overflow
    # the stack and several rpc test fixtures overflow u8 arithmetic, failures
    # that have nothing to do with spawned. release-fast keeps debug assertions.
    gate test cargo test --workspace --profile release-fast --no-fail-fast
    ;;
  *)
    echo "::error::unknown client $client"
    exit 2
    ;;
esac

if [ -n "${RUST_MIN_STACK:-}" ]; then
  echo "::warning::RUST_MIN_STACK=$RUST_MIN_STACK was set; the default-stack check did not run as intended"
fi
exit "$failed"
