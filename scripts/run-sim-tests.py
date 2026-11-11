#!/usr/bin/env python3
"""Run simulated test suites one test per process, with a per-test timeout.

Builds the integration tests of the given packages (and their library tests
with --lib) with the RUSTFLAGS in the environment, lists every non-ignored
test of every binary and runs each in its own process (`--exact`), several
processes at a time. A test that outlives the timeout is killed and counted as
failed. Exits non-zero if any test failed or timed out.

Usage (docs/TESTING.md, "Simulated suites in CI"):

  RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" \\
    python3 scripts/run-sim-tests.py -p spawned-concurrency -p spawned-rt:lib \\
      --jobs 4 --timeout 120

`-p NAME` runs the integration tests of package NAME; `-p NAME:lib` runs its
library unit tests too. `--test NAME` (repeatable) limits the run to those
integration test targets.
"""
import argparse
import json
import os
import signal
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor


def build(packages, tests, locked):
    cmd = ["cargo", "test", "--no-run", "--message-format=json"]
    if locked:
        cmd.append("--locked")
    for name, lib in packages:
        cmd += ["-p", name]
    if tests:
        for t in tests:
            cmd += ["--test", t]
    else:
        cmd.append("--tests")
    if any(lib for _, lib in packages):
        cmd.append("--lib")
    print("+", " ".join(cmd), flush=True)
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True)
    wanted = {name: lib for name, lib in packages}
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        capture_output=True, text=True, check=True,
    ).stdout)
    names = {p["manifest_path"]: p["name"] for p in metadata["packages"]}
    bins = []
    for line in proc.stdout:
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get("reason") != "compiler-artifact" or not msg.get("executable"):
            continue
        if not msg["profile"]["test"]:
            continue
        manifest_dir = os.path.dirname(msg["manifest_path"])
        target = msg["target"]
        name = names.get(msg["manifest_path"])
        if "lib" in target["kind"] and not wanted.get(name, False):
            continue
        bins.append((target["name"], msg["executable"], manifest_dir))
    if proc.wait() != 0:
        sys.exit("build failed")
    return bins


def list_tests(exe, cwd):
    def names(extra):
        out = subprocess.run(
            [exe, "--list", "--format", "terse"] + extra,
            cwd=cwd, capture_output=True, text=True, check=True,
        ).stdout
        return [l[: -len(": test")] for l in out.splitlines() if l.endswith(": test")]

    ignored = set(names(["--ignored"]))
    return [t for t in names([]) if t not in ignored]


def run_one(job, timeout):
    target, exe, cwd, test = job
    start = time.monotonic()
    proc = subprocess.Popen(
        [exe, "--exact", test, "--test-threads", "1"],
        cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        start_new_session=True,
    )
    try:
        out, _ = proc.communicate(timeout=timeout)
        status = "ok" if proc.returncode == 0 else f"FAILED (exit {proc.returncode})"
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        out, _ = proc.communicate()
        status = f"TIMEOUT ({timeout}s)"
    return target, test, status, time.monotonic() - start, out


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("-p", "--package", action="append", required=True)
    ap.add_argument("--test", action="append", default=[])
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--timeout", type=float, default=120)
    ap.add_argument("--no-locked", action="store_true")
    args = ap.parse_args()
    packages = [(p.split(":")[0], p.endswith(":lib")) for p in args.package]

    jobs = []
    for target, exe, cwd in build(packages, args.test, not args.no_locked):
        jobs += [(target, exe, cwd, t) for t in list_tests(exe, cwd)]
    print(f"{len(jobs)} tests, {args.jobs} at a time, {args.timeout:.0f}s each", flush=True)

    failed = []
    times = []
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for target, test, status, secs, out in pool.map(
            lambda j: run_one(j, args.timeout), jobs
        ):
            times.append((secs, f"{target}::{test}"))
            if status != "ok":
                failed.append((target, test, status, out))
                print(f"{status}: {target}::{test} ({secs:.1f}s)", flush=True)
                print("\n".join(out.splitlines()[-60:]), flush=True)

    times.sort(reverse=True)
    print("\nslowest:")
    for secs, name in times[:10]:
        print(f"  {secs:6.1f}s {name}")
    print(f"\n{len(jobs) - len(failed)} passed, {len(failed)} failed")
    for target, test, status, _ in failed:
        print(f"  {status}: {target}::{test}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
