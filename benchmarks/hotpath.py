#!/usr/bin/env python3
"""Hot-path regression checks: a revision against its base.

    hotpath.py gate  [--base REV] [--head REV] [--threshold PCT] [--features F]
                     [--release REV] [--release-cache DIR]
    hotpath.py build-release --release REV --release-cache DIR [--features F]
    hotpath.py abba  [--base REV] [--head REV] [--reps N] [--cores LIST] [--features F]

Both build `benchmarks/hotpath/driver.rs` (the head's copy, in both trees, so
both sides run the same workload) as a release example of each revision.

`gate` counts instructions with callgrind, which is deterministic enough for
noisy CI runners. Each benchmark runs at two sizes; instructions per operation
is the difference divided by the extra operations, so process start, runtime
and actor setup cancel; the median over `--reps` run pairs (`--contended-reps` for the
two-thread benchmark). Every run is pinned to one CPU, so the host scheduler
cannot change how the threads interleave (see `instructions`). It fails when any benchmark grows by more than
`--threshold` percent (default 3). It is the CI check.

With `--release` (CI passes the last `v*` tag), `gate` also measures that
release and fails when any benchmark grows by more than `--release-threshold`
percent (default: `--threshold`) over it, so regressions that each pass the
per-PR check cannot add up across a release cycle. `--release-cache DIR` keeps
the release binary there, keyed by release commit, driver and toolchain, so
it is built once per release rather than per PR; `build-release` only fills
that cache (CI runs it on every push to main, where PRs can restore it from).

`abba` measures wall-clock throughput, alternating base and head on the same
cores (ABBA order), and reports the median and spread. Run it on a quiet
machine before a release; it is never run in CI.
"""
import argparse
import hashlib
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DRIVER = ROOT / "benchmarks" / "hotpath" / "driver.rs"


def sh(cmd, **kw):
    return subprocess.run(cmd, check=True, text=True, **kw)


def git(*args, cwd=ROOT):
    return sh(["git", *args], cwd=cwd, capture_output=True).stdout.strip()


def default_base(head):
    for ref in ("origin/main", "main"):
        try:
            return git("merge-base", head, ref)
        except subprocess.CalledProcessError:
            continue
    sys.exit("no --base given and no main/origin/main to take a merge base with")


def build(work, label, rev, features=None):
    """Check out `rev` in a worktree, add the driver, build it; return the binary."""
    tree = work / label
    work.mkdir(parents=True, exist_ok=True)
    sh(["git", "worktree", "add", "--detach", "--force", str(tree), rev], cwd=ROOT,
       capture_output=True)
    shutil.copy(DRIVER, tree / "concurrency" / "examples" / "hotpath.rs")
    target = work / f"target-{label}"  # one per side: identical crate hashes would collide
    env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_INCREMENTAL="0")
    extra = ["--features", features] if features else []
    sh(["cargo", "build", "--release", "--locked", "-q", "-p", "spawned-concurrency",
        "--example", "hotpath", *extra], cwd=tree, env=env)
    binary = work / f"hotpath-{label}"
    shutil.copy(target / "release" / "examples" / "hotpath", binary)
    return binary


def release_key(rev, features=None):
    """Cache key of a release binary: its commit, the driver and the compiler."""
    driver = hashlib.sha256(DRIVER.read_bytes()).hexdigest()[:16]
    rustc = sh(["rustc", "-V"], cwd=ROOT, capture_output=True).stdout
    toolchain = hashlib.sha256(rustc.encode()).hexdigest()[:16]
    commit = git("rev-parse", f"{rev}^{{commit}}")  # a tag's commit, not the tag object
    extra = "-" + re.sub(r"[^A-Za-z0-9]+", "_", features) if features else ""
    return f"hotpath-{commit[:16]}-{driver}-{toolchain}{extra}"


def build_release(work, rev, cache, features=None):
    """The release binary, from `cache` when an identical build is there."""
    if not cache:
        return build(work, "release", rev, features)
    cache = Path(cache).resolve()
    cached = cache / release_key(rev, features)
    if cached.exists():
        print(f"release binary from cache: {cached}", file=sys.stderr)
        return cached
    binary = build(work, "release", rev, features)
    cache.mkdir(parents=True, exist_ok=True)
    for stale in cache.glob("hotpath-*"):
        stale.unlink()
    shutil.copy(binary, cached)
    return cached


def benchmarks(binary, only, gate=False):
    """The driver's benchmarks; the gate takes only the deterministic ones."""
    names = sh([str(binary), "list-gate" if gate else "list"], capture_output=True).stdout.split()
    return [n for n in names if not only or n in only]


IR = re.compile(r"Collected\s*:\s*(\d+)")


def one_cpu():
    """Pin the measured process to one CPU (see `instructions`)."""
    if hasattr(os, "sched_setaffinity"):
        os.sched_setaffinity(0, {max(os.sched_getaffinity(0))})


def instructions(binary, name, ops):
    # Valgrind runs one guest thread at a time, but a thread blocked in a
    # syscall (futex wait, park) releases that lock, so on several CPUs the
    # host scheduler decides how the requester and the actor interleave and
    # how often they spin, park and wake: up to 3.7% instructions per
    # operation under load on `threads_request_seq`. On one CPU the
    # interleaving barely depends on the host (0.9%); the code paths measured
    # are the same.
    out = subprocess.run(
        ["valgrind", "--tool=callgrind", "--fair-sched=yes",
         "--callgrind-out-file=/dev/null", str(binary), name, str(ops)],
        text=True, capture_output=True, timeout=180, preexec_fn=one_cpu)
    if out.returncode != 0:
        sys.exit(f"{binary.name} {name} {ops} failed:\n{out.stderr[-3000:]}")
    return int(IR.search(out.stderr).group(1))


def contended(name):
    """Benchmarks whose count varies with thread interleaving (not `async_ct_*`)."""
    return not name.startswith("async_ct_")


def per_op(binary, name, small, large, reps, csv=None, label=""):
    """Instructions per operation: the median over run pairs of
    (instructions at `large` - at `small`) / extra operations, and the pairs.

    The two-thread benchmark gets more pairs (`--contended-reps`). The minimum
    is not a stable estimate here: the noise is in both sizes, so the
    difference of the minima moved 3.3% across ten loaded runs, against 1.4%
    for the median.
    """
    runs = {small: [], large: []}
    for rep in range(reps):
        for size in (small, large):
            ir = instructions(binary, name, size)
            runs[size].append(ir)
            if csv:
                csv.write(f"{label},{name},{size},{rep},{ir}\n")
    pairs = [(lg - sm) / (large - small) for sm, lg in zip(runs[small], runs[large])]
    return statistics.median(pairs), pairs


def report(lines):
    text = "\n".join(lines) + "\n"
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(text)


def gate(args, base_bin, head_bin, base, head, release_bin=None, release=None):
    names = benchmarks(head_bin, args.only, gate=True)
    rows, failed, failed_release = [], [], []
    csv = open(args.csv, "w") if args.csv else None
    if csv:
        csv.write("side,benchmark,ops,rep,instructions\n")
    for name in names:
        reps = args.contended_reps if contended(name) else args.reps
        b, bs = per_op(base_bin, name, args.small, args.large, reps, csv, "base")
        h, hs = per_op(head_bin, name, args.small, args.large, reps, csv, "head")
        delta = 100 * (h / b - 1)
        bad = delta > args.threshold
        if bad:
            failed.append(name)
        spread = max(max(bs) - min(bs), max(hs) - min(hs))
        if release_bin is None:
            rows.append(f"| `{name}` | {b:,.0f} | {h:,.0f} | {delta:+.1f}% | {spread:,.0f} | "
                        f"{'FAIL' if bad else 'ok'} |")
            print(f"{name}: base {b:.0f} head {h:.0f} ({delta:+.1f}%)", file=sys.stderr)
            continue
        if release_bin == base_bin:
            r, rs = b, bs
        else:
            r, rs = per_op(release_bin, name, args.small, args.large, reps, csv, "release")
        rdelta = 100 * (h / r - 1)
        rbad = rdelta > args.release_threshold
        if rbad:
            failed_release.append(name)
        spread = max(spread, max(rs) - min(rs))
        result = ("FAIL" if bad else "ok") + " / " + ("FAIL" if rbad else "ok")
        rows.append(f"| `{name}` | {r:,.0f} | {b:,.0f} | {h:,.0f} | {delta:+.1f}% | "
                    f"{rdelta:+.1f}% | {spread:,.0f} | {result} |")
        print(f"{name}: release {r:.0f} base {b:.0f} head {h:.0f} "
              f"({delta:+.1f}% vs base, {rdelta:+.1f}% vs release)", file=sys.stderr)
    verdict = [f"**FAIL**: {', '.join(failed)} grew more than {args.threshold}% over the base"
               if failed else f"**PASS**: no benchmark grew more than {args.threshold}% over the base"]
    if release_bin is None:
        header = ["| benchmark | base Ir/op | head Ir/op | change | raw spread | result |",
                  "|---|---:|---:|---:|---:|---|"]
        revisions = f"Base `{base[:10]}`, head `{head[:10]}`."
        thresholds = f"threshold +{args.threshold}%"
    else:
        header = ["| benchmark | release Ir/op | base Ir/op | head Ir/op | Δ vs base | "
                  "Δ vs release | raw spread | result (base / release) |",
                  "|---|---:|---:|---:|---:|---:|---:|---|"]
        revisions = (f"Release `{args.release}` (`{release[:10]}`), base `{base[:10]}`, "
                     f"head `{head[:10]}`.")
        thresholds = (f"thresholds +{args.threshold}% over the base, "
                      f"+{args.release_threshold}% over the release")
        verdict.append(
            f"**FAIL**: {', '.join(failed_release)} grew more than {args.release_threshold}% "
            f"since `{args.release}` (cumulative budget; see docs/TESTING.md, "
            "\"Performance gate\")" if failed_release else
            f"**PASS**: no benchmark grew more than {args.release_threshold}% "
            f"since `{args.release}`")
    report([
        "## Hot-path instruction counts",
        "",
        f"{revisions} Instructions per operation "
        f"(callgrind, sizes {args.small} and {args.large}, median of {args.reps} run "
        f"pairs, {args.contended_reps} for the two-thread `threads_*`, one CPU); "
        f"{thresholds}.",
        "",
        *header,
        *rows,
        "",
        *(f"{v}  " for v in verdict),
    ])
    return 1 if failed or failed_release else 0


def wallclock(binary, name, ops, cores):
    cmd = [str(binary), name, str(ops)]
    if cores:
        cmd = ["taskset", "-c", cores, *cmd]
    out = sh(cmd, capture_output=True, timeout=120).stdout.strip().splitlines()[-1]
    return ops / float(out.split(",")[2])


def abba(args, base_bin, head_bin, base, head):
    names = benchmarks(head_bin, args.only)
    data = {n: {"base": [], "head": []} for n in names}
    csv = open(args.csv, "w") if args.csv else None
    if csv:
        csv.write("benchmark,side,rep,ops_per_s\n")
    for rep in range(args.reps):
        order = [("base", base_bin), ("head", head_bin)]
        if rep % 2:
            order.reverse()
        for side, binary in order:
            for name in names:
                v = wallclock(binary, name, args.ops, args.cores)
                data[name][side].append(v)
                if csv:
                    csv.write(f"{name},{side},{rep},{v:.1f}\n")
    rows = []
    for name in names:
        b, h = data[name]["base"], data[name]["head"]
        paired = statistics.median(hv / bv - 1 for bv, hv in zip(b, h))
        rows.append(
            f"| `{name}` | {statistics.median(b)/1e3:,.0f}k ({min(b)/1e3:,.0f}k-{max(b)/1e3:,.0f}k) "
            f"| {statistics.median(h)/1e3:,.0f}k ({min(h)/1e3:,.0f}k-{max(h)/1e3:,.0f}k) "
            f"| {100*paired:+.1f}% |")
    report([
        "## Hot-path wall clock (ABBA)",
        "",
        f"Base `{base[:10]}`, head `{head[:10]}`; {args.reps} interleaved repetitions of "
        f"{args.ops} operations{', cores ' + args.cores if args.cores else ''}. "
        "Operations per second: median (min-max); change is the median paired ratio.",
        "",
        "| benchmark | base | head | change |",
        "|---|---:|---:|---:|",
        *rows,
    ])
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("mode", choices=["gate", "abba", "build-release"])
    p.add_argument("--head", default="HEAD")
    p.add_argument("--base", help="default: merge base of --head and main")
    p.add_argument("--only", nargs="*", help="benchmark names (default: all)")
    p.add_argument("--work", help="build directory (default: a temporary one, removed)")
    p.add_argument("--threshold", type=float,
                   default=float(os.environ.get("HOTPATH_THRESHOLD", "3")),
                   help="gate: max growth in percent (default 3, or $HOTPATH_THRESHOLD)")
    p.add_argument("--release", help="gate: also hold the head to this release (a tag)")
    p.add_argument("--release-threshold", type=float,
                   default=(float(os.environ["HOTPATH_RELEASE_THRESHOLD"])
                            if os.environ.get("HOTPATH_RELEASE_THRESHOLD") else None),
                   help="gate: max growth over --release in percent "
                        "(default --threshold, or $HOTPATH_RELEASE_THRESHOLD)")
    p.add_argument("--release-cache", help="gate: directory that keeps the release binary")
    p.add_argument("--small", type=int, default=2000, help="gate: smaller size")
    p.add_argument("--large", type=int, default=6000, help="gate: larger size")
    p.add_argument("--reps", type=int, help="gate: runs per size (default 3); abba: repetitions (default 20)")
    p.add_argument("--ops", type=int, default=500_000, help="abba: operations per run")
    p.add_argument("--cores", help="abba: taskset core list, e.g. 12-15")
    p.add_argument("--contended-reps", type=int, default=9,
                   help="gate: runs per size for benchmarks with more than one thread "
                        "(default 9)")
    p.add_argument("--csv", help="raw samples")
    args = p.parse_args()
    args.reps = args.reps or (3 if args.mode == "gate" else 20)
    if args.release_threshold is None:
        args.release_threshold = args.threshold
    if args.release and args.mode == "abba":
        p.error("--release is a gate option")
    if args.mode == "build-release" and not (args.release and args.release_cache):
        p.error("build-release needs --release and --release-cache")

    if args.mode == "build-release":
        work = Path(args.work or tempfile.mkdtemp(prefix="hotpath-")).resolve()
        try:
            print(build_release(work, args.release, args.release_cache, args.features))
        finally:
            subprocess.run(["git", "worktree", "remove", "--force", str(work / "release")],
                           cwd=ROOT, capture_output=True)
            if not args.work:
                shutil.rmtree(work, ignore_errors=True)
        return
    head = git("rev-parse", args.head)
    base = git("rev-parse", args.base) if args.base else default_base(head)
    release = git("rev-parse", f"{args.release}^{{commit}}") if args.release else None
    work = Path(args.work or tempfile.mkdtemp(prefix="hotpath-")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    try:
        base_bin = build(work, "base", base, args.features)
        head_bin = build(work, "head", head, args.features)
        if args.mode == "abba":
            sys.exit(abba(args, base_bin, head_bin, base, head))
        release_bin = None
        if release:
            try:
                release_bin = (base_bin if release == base
                               else build_release(work, release, args.release_cache, args.features))
            except subprocess.CalledProcessError:
                sys.exit(f"the driver does not build against {args.release}: "
                         "benchmarks/hotpath/driver.rs must only use APIs that the last "
                         "release also has")
        sys.exit(gate(args, base_bin, head_bin, base, head, release_bin, release))
    finally:
        for label in ("base", "head", "release"):
            subprocess.run(["git", "worktree", "remove", "--force", str(work / label)],
                           cwd=ROOT, capture_output=True)
        if not args.work:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
