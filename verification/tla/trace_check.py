#!/usr/bin/env python3
"""Check recorded supervisor lifecycle traces against Supervision.tla.

Each JSON trace (written by concurrency/tests/supervision_trace.rs with the
`lifecycle-trace` feature) looks like:

    {"name": "...", "n": 2, "modes": ["Permanent", "Transient"],
     "max_restarts": 5,
     "events": [{"a": "SupLaunch", "c": 1, "x": "", "n": 0}, ...]}

For every trace this generates a TraceRun module that instantiates
TraceSupervision.tla with the trace as a constant, runs TLC and reports
ACCEPTED (TLC reached i = Len(Trace)) or REJECTED with the longest matched
prefix and the first event no model behavior could take.

Usage: trace_check.py [--tlc CMD] [--jobs N] [--keep DIR] PATH...
PATH is a trace file or a directory of *.json traces. Exits non-zero unless
every trace is accepted.
"""
import argparse
import concurrent.futures
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
MODELS = ("Supervision.tla", "TraceSupervision.tla")


def tla_string(value):
    return '"' + str(value).replace("\\", "\\\\").replace('"', '\\"') + '"'


def tla_event(event):
    return "[a |-> {}, c |-> {}, x |-> {}, n |-> {}]".format(
        tla_string(event["a"]), int(event["c"]), tla_string(event["x"]), int(event["n"])
    )


def generate(trace, workdir):
    for model in MODELS:
        shutil.copy(os.path.join(HERE, model), workdir)
    events = ",\n    ".join(tla_event(e) for e in trace["events"])
    modes = ", ".join(tla_string(m) for m in trace["modes"])
    with open(os.path.join(workdir, "TraceRun.tla"), "w") as out:
        out.write(
            "---- MODULE TraceRun ----\n"
            "EXTENDS TraceSupervision\n"
            f"TraceMode == <<{modes}>>\n"
            f"TraceLog == <<\n    {events}\n>>\n"
            "====\n"
        )
    with open(os.path.join(workdir, "TraceRun.cfg"), "w") as out:
        out.write(
            "INIT TraceInit\n"
            "NEXT TraceNext\n"
            "CONSTANTS\n"
            f"    N = {int(trace['n'])}\n"
            f"    MaxRestarts = {int(trace['max_restarts'])}\n"
            "    MaxGen = 1000\n"
            "    Mode <- TraceMode\n"
            "    Trace <- TraceLog\n"
            "INVARIANT TraceIncomplete\n"
            "INVARIANT Safety\n"
        )


def longest_prefix(dump_path):
    best = 0
    try:
        with open(dump_path) as dump:
            for line in dump:
                m = re.match(r"/\\ i = (\d+)", line.strip())
                if m:
                    best = max(best, int(m.group(1)))
    except OSError:
        pass
    return best


def check(path, tlc, keep):
    with open(path) as f:
        trace = json.load(f)
    events = trace["events"]
    name = trace.get("name") or os.path.basename(path)
    workdir = tempfile.mkdtemp(prefix="trace-")
    try:
        generate(trace, workdir)
        cmd = shlex.split(tlc) + [
            "-deadlock", "-workers", "1", "-cleanup",
            "-metadir", os.path.join(workdir, "states"),
            "-dump", os.path.join(workdir, "reached"),
            "TraceRun",
        ]
        # TLC extracts its standard modules into java.io.tmpdir; give each
        # concurrent run its own so they do not race.
        env = dict(os.environ)
        env["JAVA_TOOL_OPTIONS"] = (
            env.get("JAVA_TOOL_OPTIONS", "") + f" -Djava.io.tmpdir={workdir}"
        ).strip()
        run = subprocess.run(cmd, cwd=workdir, env=env, capture_output=True, text=True)
        log = run.stdout + run.stderr
        with open(os.path.join(workdir, "tlc.log"), "w") as f:
            f.write(log)
        if "Invariant TraceIncomplete is violated" in log:
            verdict, detail = "ACCEPTED", f"{len(events)} events"
        elif "Invariant Safety is violated" in log:
            verdict, detail = "SAFETY", "model invariant violated along the trace\n" + log
        elif "No error has been found" in log:
            matched = longest_prefix(os.path.join(workdir, "reached.dump"))
            nxt = events[matched] if matched < len(events) else None
            verdict = "REJECTED"
            detail = f"matched {matched}/{len(events)} events; no behavior takes event {matched + 1}: {json.dumps(nxt)}"
            context = events[max(0, matched - 8):matched]
            if context:
                detail += "\n      preceded by:\n" + "\n".join(
                    f"        {matched - len(context) + k + 1}: {json.dumps(e)}"
                    for k, e in enumerate(context)
                )
        else:
            verdict, detail = "ERROR", log
        if keep:
            dest = os.path.join(keep, os.path.splitext(os.path.basename(path))[0])
            shutil.rmtree(dest, ignore_errors=True)
            shutil.copytree(workdir, dest, ignore=shutil.ignore_patterns("states"))
        return name, verdict, detail
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("paths", nargs="+")
    parser.add_argument("--tlc", default=os.environ.get("TLC", "tlc"))
    parser.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 2) // 2))
    parser.add_argument("--keep", help="copy generated modules and TLC logs here")
    args = parser.parse_args()
    files = []
    for p in args.paths:
        if os.path.isdir(p):
            files += sorted(os.path.join(p, f) for f in os.listdir(p) if f.endswith(".json"))
        else:
            files.append(p)
    if not files:
        print("no traces found", file=sys.stderr)
        return 2
    if args.keep:
        os.makedirs(args.keep, exist_ok=True)
    failed = 0
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        for name, verdict, detail in pool.map(lambda f: check(f, args.tlc, args.keep), files):
            print(f"{verdict:9} {name}: {detail}")
            failed += verdict != "ACCEPTED"
    print(f"{len(files) - failed}/{len(files)} traces accepted")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
