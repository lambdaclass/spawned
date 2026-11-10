#!/usr/bin/env bash
# Point a client workspace (ethrex, ethlambda) at a local spawned checkout.
#
#   client-compat-patch.sh <spawned-dir> <client-dir>
#
# The client's committed manifests are left alone. The script asks cargo where
# the client's own workspace members take spawned-concurrency from (crates.io or
# a git URL), appends a matching [patch.<source>] section for every spawned
# crate to the client's .cargo/config.toml, re-locks those packages, and checks
# that every workspace member now uses the local path. It fails loudly
# otherwise: an unused patch is only a cargo warning and would silently test the
# published crate instead.
#
# A client may also pull an unrelated older spawned through a dependency
# (ethlambda gets spawned 0.5 via ethrex-p2p 25). That copy is a different
# 0.MINOR, is not patched, and is reported; a non-local copy with the same
# 0.MINOR as the local checkout is an error.
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

tmp=${RUNNER_TEMP:-/tmp}
[ -f "$client/Cargo.lock" ] || die "$client has no Cargo.lock"
cd "$client"

# Which spawned-concurrency do the client's workspace members depend on?
cargo metadata --format-version 1 >"$tmp/client-metadata-before.json"
mapfile -t used < <(jq -r '
  (.packages | map({key: .id, value: .}) | from_entries) as $pkg
  | .workspace_members as $members
  | .resolve.nodes[] | select(.id as $id | $members | index($id))
  | .deps[] | $pkg[.pkg] | select(.name == "spawned-concurrency")
  | "\(.source // "path") \(.version)"
' "$tmp/client-metadata-before.json" | sort -u)
[ "${#used[@]}" -gt 0 ] || die "no workspace member of $client depends on spawned-concurrency"
[ "${#used[@]}" -eq 1 ] || die "workspace members use several spawned-concurrency copies: ${used[*]}"
read -r source locked <<<"${used[0]}"
[ "$source" != path ] || die "$client already uses a path spawned-concurrency"

local_version=$(awk '
  /^\[workspace.package\]/ { ws = 1; next }
  /^\[/ { ws = 0 }
  ws && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "$spawned/Cargo.toml")
[ -n "$local_version" ] || die "cannot read [workspace.package] version from $spawned/Cargo.toml"

echo "client workspace uses spawned-concurrency $locked from $source"
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
local_minor=$(echo "$local_version" | cut -d. -f1,2)

mkdir -p .cargo
{
  echo
  echo "# Added by spawned client-compat CI: test against the local checkout."
  echo "[patch.'$key']"
  for i in "${!crates[@]}"; do
    echo "${crates[$i]} = { path = \"$spawned/${dirs[$i]}\" }"
  done
} >>.cargo/config.toml
echo "--- .cargo/config.toml patch section"
sed -n '/client-compat CI/,$p' .cargo/config.toml

# Re-lock the patched source's spawned entries: a patch whose version differs
# from the locked one (0.7.0 locked, 0.7.1 local) is otherwise reported unused.
# Full package-id specs, because an unrelated older copy may share the name.
mapfile -t relock < <(jq -r --arg src "$source" '
  .packages[] | select((.name | startswith("spawned-")) and .source == $src) | .id
' "$tmp/client-metadata-before.json")
cargo update "${relock[@]}"

# Check the resolved graph.
cargo metadata --format-version 1 >"$tmp/client-metadata.json"
jq -r --arg root "$spawned/" '
  .packages[] | select(.name | startswith("spawned-"))
  | "\(.name) \(.version) \(if .source == null and (.manifest_path | startswith($root)) then "LOCAL" else (.source // .manifest_path) end)"
' "$tmp/client-metadata.json" | sort | tee "$tmp/spawned-packages.txt"
member_uses=$(jq -r --arg root "$spawned/" '
  (.packages | map({key: .id, value: .}) | from_entries) as $pkg
  | .workspace_members as $members
  | .resolve.nodes[] | select(.id as $id | $members | index($id))
  | .deps[] | $pkg[.pkg] | select(.name | startswith("spawned-"))
  | if .source == null and (.manifest_path | startswith($root)) then "LOCAL" else "\(.name) \(.version) \(.source)" end
' "$tmp/client-metadata.json" | sort -u)
echo "$member_uses" | grep -qx LOCAL || die "no workspace member resolves spawned to $spawned"
if echo "$member_uses" | grep -vx LOCAL; then
  die "workspace members still use the non-local spawned packages above"
fi
while read -r name version src; do
  [ "$src" = LOCAL ] && continue
  if [ "$(echo "$version" | cut -d. -f1,2)" = "$local_minor" ]; then
    die "$name $version from $src has the same 0.MINOR as the local checkout but was not patched"
  fi
  echo "::notice title=client-compat::unrelated $name $version from $src stays unpatched (a dependency's own spawned)"
done <"$tmp/spawned-packages.txt"
cargo tree -i "spawned-concurrency@$local_version" --workspace --depth 1 -e normal,dev 2>/dev/null \
  || cargo tree -i "spawned-concurrency@${local_version%%-*}" --workspace --depth 1 -e normal,dev
