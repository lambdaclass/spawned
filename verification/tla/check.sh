#!/usr/bin/env bash
# Model-check the TLA+ specs and optionally validate recorded lifecycle traces.
# Run inside `nix develop` or via `nix run .#check-tla`. Exits non-zero on any
# violation or rejected trace.
#
#   check.sh                     model-check every MC*.cfg except *Deep.cfg
#   check.sh --deep              also the deep configurations
#   check.sh --only GLOBS        only configurations whose name matches one of
#                                the comma-separated GLOBS (e.g. --deep --only '*Deep')
#   check.sh --exclude GLOBS     skip configurations matching one of GLOBS
#   check.sh --traces DIR        also check every DIR/*.json trace
#   check.sh --traces-only DIR   only check traces (skip model checking)
#   TLC_WORKERS=N check.sh ...   TLC workers per configuration (default auto)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

deep=0
mc=1
traces=""
negative=0
only=""
exclude=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --deep) deep=1 ;;
    --negative-controls) negative=1 ;;
    --only) only="$only,$2"; shift ;;
    --exclude) exclude="$exclude,$2"; shift ;;
    --traces) traces="$(cd "$2" && pwd)"; shift ;;
    --traces-only) traces="$(cd "$2" && pwd)"; mc=0; shift ;;
    *) echo "usage: $0 [--deep] [--only GLOBS] [--exclude GLOBS] [--traces DIR | --traces-only DIR] [--negative-controls]" >&2; exit 2 ;;
  esac
  shift
done

# matches NAME ",GLOB,GLOB...": NAME matches one of the globs.
matches() {
  local glob globs
  IFS=, read -ra globs <<<"$2"   # no pathname expansion of the globs
  for glob in "${globs[@]}"; do
    # shellcheck disable=SC2053
    if [[ -n "$glob" && "$1" == $glob ]]; then return 0; fi
  done
  return 1
}

status=0
if [[ "$mc" == 1 ]]; then
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  cp "$here"/*.tla "$here"/*.cfg "$work"/
  cd "$work"
  checked=0
  for cfg in MC*.cfg; do
    if [[ "$cfg" == *Deep.cfg && "$deep" == 0 ]]; then continue; fi
    spec="${cfg%.cfg}"
    if [[ -n "$only" ]] && ! matches "$spec" "$only"; then continue; fi
    if [[ -n "$exclude" ]] && matches "$spec" "$exclude"; then continue; fi
    echo "== TLC $spec"
    start=$SECONDS
    if ! tlc -workers "${TLC_WORKERS:-auto}" -deadlock -cleanup "$spec" >"$spec.log" 2>&1 \
        || ! grep -q "No error has been found" "$spec.log"; then
      status=1
      cat "$spec.log"
    else
      grep -E "states generated|depth of the complete" "$spec.log"
    fi
    echo "   $spec: $((SECONDS - start)) s"
    checked=$((checked + 1))
  done
  if [[ "$checked" == 0 ]]; then
    echo "no configuration selected (--only '${only#,}' --exclude '${exclude#,}')" >&2
    status=1
  fi
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
