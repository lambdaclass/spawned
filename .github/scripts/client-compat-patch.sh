#!/usr/bin/env bash
# Point a client workspace (ethrex, ethlambda) at a local spawned checkout.
#
#   client-compat-patch.sh <spawned-dir> <client-dir>
#
# The client's committed manifests are left alone. The script reads where the
# client's Cargo.lock takes spawned-concurrency from (crates.io or a git URL),
# appends a matching [patch.<source>] section for every spawned crate to the
# client's .cargo/config.toml, re-locks those packages, and then checks with `cargo metadata` that every
# spawned package in the resolved graph is the local path. It fails loudly
# otherwise; an unused patch is only a cargo warning and would silently test
# the published crate instead.
#
# Version handling (crates.io source only; a git dependency has no version
# requirement): the patch is used only when the local version satisfies the
# client's requirement, i.e. same 0.MINOR and not older. A pre-release suffix
# on spawned main (0.7.2-dev) does not match a plain `0.7.1` requirement, so it
# is stripped in this CI checkout (0.7.2-dev -> 0.7.2). A different 0.MINOR is a
# breaking release: the client must move first, and the script says so.
set -euo pipefail

spawned=$(cd "$1" && pwd -P)
client=$(cd "$2" && pwd -P)
crates=(spawned-rt spawned-macros spawned-concurrency spawned-test)
dirs=(rt macros concurrency testing)

die() { echo "::error title=client-compat::$*" >&2; exit 1; }

[ -f "$client/Cargo.lock" ] || die "$client has no Cargo.lock"

# The [package] block for spawned-concurrency in the lock file.
source=$(awk '
  /^\[\[package\]\]/ { name = ""; next }
  /^name = / { gsub(/"/, "", $3); name = $3; next }
  name == "spawned-concurrency" && /^source = / { gsub(/"/, "", $3); print $3; exit }
' "$client/Cargo.lock")
locked=$(awk '
  /^\[\[package\]\]/ { name = ""; next }
  /^name = / { gsub(/"/, "", $3); name = $3; next }
  name == "spawned-concurrency" && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "$client/Cargo.lock")
[ -n "$source" ] || die "spawned-concurrency not found in $client/Cargo.lock (or it is already a path dependency)"

local_version=$(awk '
  /^\[workspace.package\]/ { ws = 1; next }
  /^\[/ { ws = 0 }
  ws && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "$spawned/Cargo.toml")
[ -n "$local_version" ] || die "cannot read [workspace.package] version from $spawned/Cargo.toml"

echo "client locks spawned-concurrency $locked from $source"
echo "local spawned is $local_version"

case "$source" in
  registry+https://github.com/rust-lang/crates.io-index)
    key=crates-io
    release=${local_version%%-*}
    if [ "$release" != "$local_version" ]; then
      echo "::notice title=client-compat::stripping pre-release: spawned $local_version -> $release in this CI checkout so ^$locked can match"
      sed -i.bak -E "s/^version = \"$local_version\"/version = \"$release\"/" "$spawned/Cargo.toml"
      sed -i.bak -E "s/(spawned-[a-z]+ = \{ path = \"[a-z]+\", version = )\"=?$local_version\"/\1\"$release\"/" "$spawned/Cargo.toml"
      rm -f "$spawned/Cargo.toml.bak"
    fi
    IFS=. read -r lmaj lmin lpat <<<"$release"
    IFS=. read -r cmaj cmin cpat <<<"$locked"
    if [ "$lmaj.$lmin" != "$cmaj.$cmin" ]; then
      die "spawned $release is not semver-compatible with the client's $locked (0.MINOR differs). The client needs a branch that requires $lmaj.$lmin; dispatch with that ref."
    fi
    if [ "$lpat" -lt "$cpat" ]; then
      die "spawned $release is older than the client's $locked; the patch cannot apply"
    fi
    ;;
  git+*)
    key=${source#git+}
    key=${key%%#*}
    key=${key%%\?*}
    ;;
  *) die "unsupported spawned source: $source" ;;
esac

mkdir -p "$client/.cargo"
{
  echo
  echo "# Added by spawned client-compat CI: test against the local checkout."
  echo "[patch.'$key']"
  for i in "${!crates[@]}"; do
    echo "${crates[$i]} = { path = \"$spawned/${dirs[$i]}\" }"
  done
} >>"$client/.cargo/config.toml"
echo "--- $client/.cargo/config.toml patch section"
sed -n '/client-compat CI/,$p' "$client/.cargo/config.toml"

# Re-lock the spawned entries: a patch whose version differs from the locked
# one (0.7.0 locked, 0.7.1 local) is otherwise reported unused and ignored.
cd "$client"
locked_names=$(grep -oE '^name = "spawned-(rt|macros|concurrency|test)"$' Cargo.lock | cut -d'"' -f2 | sort -u)
# shellcheck disable=SC2086 # one argument per package name
cargo update $locked_names

# Resolve and check: every spawned-* package must come from the local path.
cargo metadata --format-version 1 >"${RUNNER_TEMP:-/tmp}/client-metadata.json"
jq -r --arg root "$spawned/" '
  .packages[] | select(.name | startswith("spawned-"))
  | "\(.name) \(.version) \(if .source == null and (.manifest_path | startswith($root)) then "LOCAL" else (.source // .manifest_path) end)"
' "${RUNNER_TEMP:-/tmp}/client-metadata.json" | sort | tee "${RUNNER_TEMP:-/tmp}/spawned-packages.txt"
grep -q '^spawned-concurrency .* LOCAL$' "${RUNNER_TEMP:-/tmp}/spawned-packages.txt" \
  || die "spawned-concurrency does not resolve to $spawned"
if grep -v ' LOCAL$' "${RUNNER_TEMP:-/tmp}/spawned-packages.txt"; then
  die "some spawned packages above still resolve to a non-local source"
fi
cargo tree -i spawned-concurrency --workspace --depth 1 -e normal,dev
