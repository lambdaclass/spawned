#!/usr/bin/env python3
"""Check feature builds, migrated APIs and the release's declared API breaks.

`verification/api/release.json` names the baseline release and the release
type of the work in progress. A `patch` release must keep the baseline's API
(v0.7's feature-split breaks against v0.6.0 are declared item by item in
`v0.6.0-no-default.json`); a breaking pre-1.0 release (`minor`, as 0.8 is
against 0.7.3, declared `major` to the tool) only checks that both APIs build
in every profile: the tool skips its breaking-change lints for a major
release.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess


def run(*command, cwd):
    subprocess.run(command, cwd=cwd, check=True)


def findings(output):
    """Read the pinned tool's item-level diagnostics; unknown formats fail closed."""
    found = set()
    for block in output.split("--- failure ")[1:]:
        lint = block.split(":", 1)[0]
        if "Failed in:\n" not in block:
            raise ValueError("semver-checks diagnostic format changed")
        items = block.split("Failed in:\n", 1)[1].split("\n\n", 1)[0].splitlines()
        if not items or any(not line.startswith("  ") for line in items):
            raise ValueError("semver-checks diagnostic items missing")
        for line in items:
            item = re.split(r", previously in file | in file ", line.strip())[0]
            found.add((lint, item))
    return found


def declared_break(command, expected, root):
    result = subprocess.run(command, cwd=root, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, end="", flush=True)
    actual = findings(result.stdout)
    expected = {tuple(item) for item in expected}
    # An arbitrary build/tool failure is never treated as an acknowledged break.
    if result.returncode != 100 or actual != expected:
        raise RuntimeError(
            "no-default API audit differs from the reviewed v0.7 migration: "
            f"exit={result.returncode}, unexpected={sorted(actual - expected)}, "
            f"missing={sorted(expected - actual)}"
        )
    print("Confirmed declared v0.7 no-default feature-split breaks; this configuration is NOT patch-compatible.", flush=True)


def semver(root, baseline):
    release = json.loads((root / "verification/api/release.json").read_text())
    baseline = baseline or release["baseline"]
    release_type = release["release_type"] if baseline == release["baseline"] else "patch"
    if release_type != "patch":
        print(f"Breaking release against {baseline} ({release_type}): {release['reason']}", flush=True)
    declarations = json.loads((root / "verification/api/v0.6.0-no-default.json").read_text())
    if release_type == "patch" and baseline != declarations["baseline"]:
        raise ValueError("the declared feature-split audit requires baseline " + declarations["baseline"])
    base = ["cargo", "semver-checks", "--color", "never", "--release-type", release_type, "--baseline-rev", baseline]
    libraries = ["-p", "spawned-concurrency", "-p", "spawned-rt"]
    # Old no-default consumers restore the previous API by explicitly enabling
    # async. Check this migration as well as defaults, with/without metrics.
    for profile, features, packages in [
        ("default", ["--default-features"], libraries),
        ("metrics", ["--default-features", "--features", "metrics"], ["-p", "spawned-concurrency"]),
        ("no-default + explicit async migration", ["--only-explicit-features", "--current-features", "async"], libraries),
        ("no-default + metrics + async migration", ["--only-explicit-features", "--features", "metrics", "--current-features", "async"], ["-p", "spawned-concurrency"]),
    ]:
        print("Checking API profile: " + profile, flush=True)
        run(*base, *features, *packages, cwd=root)
    if release_type != "patch":
        # Breaks are allowed and listed in MIGRATION; the item-level audit
        # applies to the v0.6.0 -> v0.7 patch line only.
        return
    for package, expected in declarations["packages"].items():
        print("Auditing raw no-default API: " + package, flush=True)
        command = base + ["--only-explicit-features", "-p", package]
        declared_break(command, expected, root)
        if package == "spawned-concurrency":
            print("Auditing raw no-default + metrics API", flush=True)
            declared_break(command + ["--features", "metrics"], expected, root)


def features(root, cache, offline):
    # A stable source path avoids a fresh set of Cargo fingerprints on every run.
    # The separate build tree never holds the normal workspace target lock.
    import fcntl
    workspace = cache / "feature-check"
    workspace.mkdir(parents=True, exist_ok=True)
    with (workspace / "workspace.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("another feature check is using " + str(workspace)) from None
        files = subprocess.check_output(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=root,
        ).decode().split("\0")
        current = workspace / "source"
        if current.exists():
            shutil.rmtree(current)
        current.mkdir()
        for name in filter(None, files):
            source = root / name
            # Custom target dirs may not be gitignored. Never copy a cache into
            # its own source workspace (or copy unrelated build output).
            if source.is_file() and not source.resolve().is_relative_to(cache):
                destination = current / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, destination)
        os.environ["CARGO_TARGET_DIR"] = str(workspace / "build")
        run("cargo", "hack", "check", "--each-feature", "--no-dev-deps",
            *(["--offline"] if offline else []),
            "-p", "spawned-concurrency", "-p", "spawned-rt", "--lib", cwd=current)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("check", choices=("features", "semver"))
    # Default: the baseline of verification/api/release.json.
    parser.add_argument("--baseline", default=None)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if args.offline:
        os.environ["CARGO_NET_OFFLINE"] = "true"
    root = Path(__file__).resolve().parents[1]
    # Stable outside the disposable workspace, so rust-cache actually applies.
    os.environ["CARGO_TARGET_DIR"] = str(Path(os.environ.get("CARGO_TARGET_DIR", root / "target")).resolve())
    if args.check == "semver":
        # The tool handles baseline-rev itself. No Python 3.12 tarfile API and
        # no hand-rolled extraction or copying of the Git baseline is needed.
        semver(root, args.baseline)
        return
    features(root, Path(os.environ["CARGO_TARGET_DIR"]), args.offline)


if __name__ == "__main__":
    main()
