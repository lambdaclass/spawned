#!/usr/bin/env python3
"""Hot-path regression checks: a revision against its base.

    hotpath.py gate  [--base REV] [--head REV] [--threshold PCT]
    hotpath.py abba  [--base REV] [--head REV] [--reps N] [--cores LIST]

Both build `benchmarks/hotpath/driver.rs` (the head's copy, in both trees, so
both sides run the same workload) as a release example of each revision.

`gate` counts instructions with callgrind, which is deterministic enough for
noisy CI runners. Each benchmark runs at two sizes; instructions per operation
is the difference divided by the extra operations, so process start, runtime
and actor setup cancel. It fails when any benchmark grows by more than
`--threshold` percent (default 3). It is the CI check.

`abba` measures wall-clock throughput, alternating base and head on the same
cores (ABBA order), and reports the median and spread. Run it on a quiet
machine before a release; it is never run in CI.
"""
import argparse
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


def build(work, label, rev):
    """Check out `rev` in a worktree, add the driver, build it; return the binary."""
    tree = work / label
    sh(["git", "worktree", "add", "--detach", "--force", str(tree), rev], cwd=ROOT,
       capture_output=True)
    shutil.copy(DRIVER, tree / "concurrency" / "examples" / "hotpath.rs")
    target = work / f"target-{label}"  # one per side: identical crate hashes would collide
    env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_INCREMENTAL="0")
    sh(["cargo", "build", "--release", "--locked", "-q", "-p", "spawned-concurrency",
        "--example", "hotpath"], cwd=tree, env=env)
    binary = work / f"hotpath-{label}"
    shutil.copy(target / "release" / "examples" / "hotpath", binary)
    return binary


def benchmarks(binary, only, gate=False):
    """The driver's benchmarks; the gate takes only the deterministic ones."""
    names = sh([str(binary), "list-gate" if gate else "list"], capture_output=True).stdout.split()
    return [n for n in names if not only or n in only]


IR = re.compile(r"Collected\s*:\s*(\d+)")


def instructions(binary, name, ops):
    out = subprocess.run(
        ["valgrind", "--tool=callgrind", "--fair-sched=yes",
         "--callgrind-out-file=/dev/null", str(binary), name, str(ops)],
        text=True, capture_output=True)
    if out.returncode != 0:
        sys.exit(f"{binary.name} {name} {ops} failed:\n{out.stderr[-3000:]}")
    return int(IR.search(out.stderr).group(1))


def per_op(binary, name, small, large, reps):
    samples = [
        (instructions(binary, name, large) - instructions(binary, name, small)) / (large - small)
        for _ in range(reps)
    ]
    return statistics.median(samples), samples


def report(lines):
    text = "\n".join(lines) + "\n"
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(text)


def gate(args, base_bin, head_bin, base, head):
    names = benchmarks(head_bin, args.only, gate=True)
    rows, failed = [], []
    for name in names:
        b, bs = per_op(base_bin, name, args.small, args.large, args.reps)
        h, hs = per_op(head_bin, name, args.small, args.large, args.reps)
        delta = 100 * (h / b - 1)
        bad = delta > args.threshold
        if bad:
            failed.append(name)
        spread = max(max(bs) - min(bs), max(hs) - min(hs))
        rows.append(f"| `{name}` | {b:,.0f} | {h:,.0f} | {delta:+.1f}% | ±{spread:,.0f} | "
                    f"{'FAIL' if bad else 'ok'} |")
        print(f"{name}: base {b:.0f} head {h:.0f} ({delta:+.1f}%)", file=sys.stderr)
    verdict = (f"**FAIL**: {', '.join(failed)} grew more than {args.threshold}%"
               if failed else f"**PASS**: no benchmark grew more than {args.threshold}%")
    report([
        "## Hot-path instruction counts",
        "",
        f"Base `{base[:10]}`, head `{head[:10]}`. Instructions per operation "
        f"(callgrind, sizes {args.small} and {args.large}, median of {args.reps}); "
        f"threshold +{args.threshold}%.",
        "",
        "| benchmark | base Ir/op | head Ir/op | change | max spread | result |",
        "|---|---:|---:|---:|---:|---|",
        *rows,
        "",
        verdict,
    ])
    return 1 if failed else 0


def wallclock(binary, name, ops, cores):
    cmd = [str(binary), name, str(ops)]
    if cores:
        cmd = ["taskset", "-c", cores, *cmd]
    out = sh(cmd, capture_output=True).stdout.strip().splitlines()[-1]
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
    p.add_argument("mode", choices=["gate", "abba"])
    p.add_argument("--head", default="HEAD")
    p.add_argument("--base", help="default: merge base of --head and main")
    p.add_argument("--only", nargs="*", help="benchmark names (default: all)")
    p.add_argument("--work", help="build directory (default: a temporary one, removed)")
    p.add_argument("--threshold", type=float,
                   default=float(os.environ.get("HOTPATH_THRESHOLD", "3")),
                   help="gate: max growth in percent (default 3, or $HOTPATH_THRESHOLD)")
    p.add_argument("--small", type=int, default=2000, help="gate: smaller size")
    p.add_argument("--large", type=int, default=6000, help="gate: larger size")
    p.add_argument("--reps", type=int, help="gate: runs per size (default 3); abba: repetitions (default 20)")
    p.add_argument("--ops", type=int, default=500_000, help="abba: operations per run")
    p.add_argument("--cores", help="abba: taskset core list, e.g. 12-15")
    p.add_argument("--csv", help="abba: raw samples")
    args = p.parse_args()
    args.reps = args.reps or (3 if args.mode == "gate" else 20)

    head = git("rev-parse", args.head)
    base = git("rev-parse", args.base) if args.base else default_base(head)
    work = Path(args.work or tempfile.mkdtemp(prefix="hotpath-")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    try:
        base_bin = build(work, "base", base)
        head_bin = build(work, "head", head)
        run = gate if args.mode == "gate" else abba
        sys.exit(run(args, base_bin, head_bin, base, head))
    finally:
        for label in ("base", "head"):
            subprocess.run(["git", "worktree", "remove", "--force", str(work / label)],
                           cwd=ROOT, capture_output=True)
        if not args.work:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
