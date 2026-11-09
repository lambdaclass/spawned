#!/usr/bin/env bash
# Model-check the TLA+ specs and optionally validate recorded lifecycle traces.
# Run inside `nix develop` or via `nix run .#check-tla`. Exits non-zero on any
# violation or rejected trace.
#
#   check.sh                     model-check every MC*.cfg except *Deep.cfg
#   check.sh --deep              also the deep configurations
#   check.sh --traces DIR        also check every DIR/*.json trace
#   check.sh --traces-only DIR   only check traces (skip model checking)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

deep=0
mc=1
traces=""
negative=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --deep) deep=1 ;;
    --negative-controls) negative=1 ;;
    --traces) traces="$(cd "$2" && pwd)"; shift ;;
    --traces-only) traces="$(cd "$2" && pwd)"; mc=0; shift ;;
    *) echo "usage: $0 [--deep] [--traces DIR | --traces-only DIR] [--negative-controls]" >&2; exit 2 ;;
  esac
  shift
done

status=0
if [[ "$mc" == 1 ]]; then
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  cp "$here"/*.tla "$here"/*.cfg "$work"/
  cd "$work"
  for cfg in MC*.cfg; do
    if [[ "$cfg" == *Deep.cfg && "$deep" == 0 ]]; then continue; fi
    spec="${cfg%.cfg}"
    echo "== TLC $spec"
    if ! tlc -workers auto -deadlock -cleanup "$spec" >"$spec.log" 2>&1 \
        || ! grep -q "No error has been found" "$spec.log"; then
      status=1
      cat "$spec.log"
    else
      grep -E "states generated|depth of the complete" "$spec.log"
    fi
  done
fi

if [[ -n "$traces" ]]; then
  echo "== Trace validation $traces"
  python3 "$here/trace_check.py" --jobs "${TLA_TRACE_JOBS:-2}" "$traces" || status=1
  if [[ "$negative" == 1 ]]; then
    python3 "$here/negative_checks.py" "$traces" || status=1
  fi
elif [[ "$negative" == 1 ]]; then
  echo "--negative-controls requires --traces or --traces-only" >&2
  exit 2
fi
exit "$status"
