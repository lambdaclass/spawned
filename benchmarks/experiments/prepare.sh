#!/usr/bin/env bash
# Prepare a scratch tree in which the backend experiments build
# (benchmarks/README.md, "Experiments").
#
#   benchmarks/experiments/prepare.sh [DIR] [REV]
#
# Adds a detached git worktree of REV (default HEAD) at DIR (default
# target/experiments-tree) and applies backend-experiments.patch, which puts
# the `backend-experiments` features and hooks back into spawned-rt and
# spawned-concurrency. The published crates never carry them. Then, in DIR:
#
#   cargo bench --manifest-path benchmarks/experiments/Cargo.toml --bench designs
#   cargo test --manifest-path benchmarks/experiments/Cargo.toml
#
# The research drivers (benchmarks/*.py) take the same tree as their root.
# Remove it with `git worktree remove --force DIR`.
#
# If the patch no longer applies to REV, rebase it (apply to the last revision
# it fits, rebase that branch, regenerate with `git diff`), or run the original
# in-tree experiments at d082d0d8, the last commit before they moved here.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
dir=${1:-$root/target/experiments-tree}
rev=${2:-HEAD}
patch="$root/benchmarks/experiments/backend-experiments.patch"

git -C "$root" worktree add --detach "$dir" "$rev"
if ! git -C "$dir" apply "$patch"; then
    echo "backend-experiments.patch does not apply to $rev; see the header of $0" >&2
    exit 1
fi
echo "$dir"
