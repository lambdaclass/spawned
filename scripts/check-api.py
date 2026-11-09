#!/usr/bin/env python3
"""Check public APIs/features in disposable copies; never rewrite the checkout."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile


def run(*command, cwd):
    subprocess.run(command, cwd=cwd, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("check", choices=("features", "semver"))
    parser.add_argument("--baseline", default="v0.6.0")
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if args.offline:
        os.environ["CARGO_NET_OFFLINE"] = "true"
    root = Path(__file__).resolve().parents[1]
    files = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=root,
    ).decode().split("\0")
    with tempfile.TemporaryDirectory(prefix="spawned-api-") as temporary:
        current = Path(temporary) / "current"
        current.mkdir()
        for name in filter(None, files):
            source = root / name
            if source.is_file():
                destination = current / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, destination)
        if args.check == "features":
            # --no-dev-deps rewrites manifests/lockfiles. Only do so in the copy,
            # so dev-dependency feature unification cannot hide missing features.
            run(
                "cargo", "hack", "check", "--each-feature", "--no-dev-deps",
                *(["--offline"] if args.offline else []),
                "-p", "spawned-concurrency", "-p", "spawned-rt", "--lib",
                cwd=current,
            )
        else:
            archive = Path(temporary) / "baseline.tar"
            with archive.open("wb") as output:
                subprocess.run(
                    ["git", "archive", "--format=tar", args.baseline],
                    cwd=root, stdout=output, check=True,
                )
            baseline = Path(temporary) / "baseline"
            baseline.mkdir()
            with tarfile.open(archive) as source:
                source.extractall(baseline, filter="data")
            # Stronger than the planned 0.x minor bump: require compatibility
            # with the released default API rather than hiding breaks in 0.7.
            run(
                "cargo", "semver-checks", "--default-features",
                "--release-type", "patch", "--baseline-root", str(baseline),
                "-p", "spawned-concurrency", "-p", "spawned-rt",
                cwd=current,
            )


if __name__ == "__main__":
    main()
