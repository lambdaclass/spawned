#!/usr/bin/env python3
"""Check recorded lifecycle traces against Supervision.tla (and, by schema,
Cohorts.tla, NestedSupervision.tla, DeferredJobs.tla, DeferredReplies.tla and
Lifecycle.tla).

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


def tla_event(event, cohort=False):
    result = "[a |-> {}, c |-> {}, x |-> {}, n |-> {}".format(
        tla_string(event["a"]), int(event["c"]), tla_string(event["x"]), int(event["n"])
    )
    if cohort:
        result += ", r |-> {}".format(int(event.get("r", 0)))
    return result + "]"


def generate(trace, workdir):
    schema = trace.get("schema", "one-for-one-v1")
    if schema not in {"one-for-one-v1", "cohort-v1", "tree-v1", "jobs-v1", "replies-v1",
                      "lifecycle-v1"}:
        raise ValueError(f"unsupported trace schema: {schema}")
    if schema == "tree-v1":
        return generate_tree(trace, workdir)
    if schema == "jobs-v1":
        return generate_jobs(trace, workdir)
    if schema == "replies-v1":
        return generate_replies(trace, workdir)
    if schema == "lifecycle-v1":
        return generate_lifecycle(trace, workdir)
    cohort = schema == "cohort-v1"
    modules = ("Cohorts.tla", "TraceCohorts.tla") if cohort else MODELS
    for model in modules:
        shutil.copy(os.path.join(HERE, model), workdir)
    events = ",\n    ".join(tla_event(e, cohort) for e in trace["events"])
    modes = ", ".join(tla_string(m) for m in trace["modes"])
    with open(os.path.join(workdir, "TraceRun.tla"), "w") as out:
        out.write(
            "---- MODULE TraceRun ----\n" +
            ("EXTENDS TraceCohorts\n" if cohort else "EXTENDS TraceSupervision\n") +
            f"TraceMode == <<{modes}>>\n"
            f"TraceLog == <<\n    {events}\n>>\n"
            "====\n"
        )
    with open(os.path.join(workdir, "TraceRun.cfg"), "w") as out:
        out.write(
            "INIT TraceInit\n"
            "NEXT TraceNext\n"
            "CONSTANTS\n"
            f"    N = {int(trace['n'])}\n" +
            (f"    Strategy = {tla_string(trace['strategy'])}\n    MaxRounds = 1000\n    Fault = \"None\"\n" if cohort
             else f"    MaxRestarts = {int(trace['max_restarts'])}\n") +
            "    MaxGen = 1000\n"
            "    Mode <- TraceMode\n"
            "    Trace <- TraceLog\n"
            "INVARIANT Safety\n"
            "INVARIANT TraceIncomplete\n"
        )


def generate_tree(trace, workdir):
    for model in ("NestedSupervision.tla", "TraceNested.tla"):
        shutil.copy(os.path.join(HERE, model), workdir)
    # Normalize IDs without changing ordering, slot ownership or generations.
    nodes = {int(trace["root"]): 1}
    for event in trace["events"]:
        for key in ("node", "peer"):
            node = int(event[key])
            if node:
                nodes.setdefault(node, len(nodes) + 1)
    records = []
    for e in trace["events"]:
        records.append("[a |-> {}, node |-> {}, slot |-> {}, gen |-> {}, peer |-> {}, arity |-> {}, cancelled |-> {}]".format(
            tla_string(e["a"]), nodes[int(e["node"])], int(e["slot"]), int(e["gen"]),
            nodes.get(int(e["peer"]), 0), int(e["arity"]), "TRUE" if e["cancelled"] else "FALSE"))
    with open(os.path.join(workdir, "TraceRun.tla"), "w") as out:
        out.write("---- MODULE TraceRun ----\nEXTENDS TraceNested\nTraceLog == <<\n"
                  + ",\n".join(records) + "\n>>\n====\n")
    with open(os.path.join(workdir, "TraceRun.cfg"), "w") as out:
        out.write("INIT TraceInit\nNEXT TraceNext\nCONSTANTS\n"
                  f" NodeCount = {len(nodes)}\n Slots = {max(1,max(int(e['arity']) for e in trace['events']))}\n"
                  " MaxGen = 1000\n Fault = \"None\"\n Trace <- TraceLog\n"
                  "INVARIANT Safety\nINVARIANT TraceIncomplete\n")


def generate_replies(trace, workdir):
    """One owner instance's reply slots against DeferredReplies.tla."""
    for model in ("DeferredReplies.tla", "TraceReplies.tla"):
        shutil.copy(os.path.join(HERE, model), workdir)
    # A slot address can be reused once freed: each Send starts a request.
    current, n, records = {}, 0, []
    for e in trace["events"]:
        r = int(e["r"])
        if e["a"] == "Send":
            n += 1
            current[r] = n
        req = current.get(r, 0) if r else 0
        if r and not req:
            raise ValueError(f"event for an unsent request: {e}")
        records.append("[a |-> {}, r |-> {}, x |-> {}]".format(
            tla_string(e["a"]), req, tla_string(e["x"])))
    if not n:
        raise ValueError("a replies trace needs at least one request")
    # Deadlines are chosen by TLC; the clock only needs room for the trace.
    clock = 2 + sum(1 for e in trace["events"] if e.get("x") == "timeout")
    with open(os.path.join(workdir, "TraceRun.tla"), "w") as out:
        out.write("---- MODULE TraceRun ----\nEXTENDS TraceReplies\nTraceLog == <<\n    "
                  + ",\n    ".join(records) + "\n>>\n====\n")
    with open(os.path.join(workdir, "TraceRun.cfg"), "w") as out:
        out.write("INIT TraceInit\nNEXT TraceNext\nCONSTANTS\n"
                  f"    N = {n}\n    BoundA = {int(trace['bound'])}\n    BoundB = 1\n"
                  f"    MaxTime = {clock}\n    MaxWait = {clock}\n    Fault = \"None\"\n"
                  "    Trace <- TraceLog\n"
                  "INVARIANT Safety\nINVARIANT TraceIncomplete\n")


OWN_ACTIONS = {"Trap", "Stop", "Link", "Unlink", "Monitor", "Demonitor"}


def lifecycle_records(trace):
    """Normalize a lifecycle-v1 trace: actor and monitor numbers, Send split
    into Request and Enqueue, and request turns that panicked folded into
    their Dispatch (TraceLifecycle.tla). Returns (records, constants)."""
    events = trace["events"]
    actors, parents = {}, {}
    for e in events:
        if e["a"] == "Spawn":
            actors[int(e["t"])] = f"a{len(actors) + 1}"
            if int(e["s"]):
                parents[int(e["t"])] = int(e["s"])
    if not actors:
        raise ValueError("a lifecycle trace needs at least one spawned actor")
    for child, parent in parents.items():
        if parent not in actors:
            raise ValueError(f"start_linked parent {parent} is not recorded")

    def actor(raw):
        if int(raw) not in actors:
            raise ValueError(f"event for an unrecorded actor {raw}")
        return actors[int(raw)]

    # Request turns that panicked with nothing of the actor's own in between.
    folded = set()
    turn = {}
    for i, e in enumerate(events):
        t = int(e["t"])
        if e["a"] == "Dispatch":
            turn[t] = i if e["k"] == "req" else None
        elif e["a"] in OWN_ACTIONS:
            turn[t] = None
        elif e["a"] == "Panic" and e["x"] == "turn" and turn.get(t) is not None:
            folded |= {turn[t], i}
            turn[t] = None

    monitors, requests, children, sends = {}, 0, set(), 0
    signals = late = 0
    records = []

    def record(a, t="none", s="none", x="", m=0, q=0, k="", o="", p=False):
        records.append("[a |-> {}, t |-> {}, s |-> {}, x |-> {}, m |-> {}, q |-> {}, k |-> {}, o |-> {}, p |-> {}]".format(
            tla_string(a), tla_string(t), tla_string(s), tla_string(x), m, q,
            tla_string(k), tla_string(o), "TRUE" if p else "FALSE"))

    for i, e in enumerate(events):
        a = e["a"]
        if a == "Spawn":
            continue
        t = actor(e["t"])
        if a == "Send":
            # A plain message from a recorded actor: the model's Send(s, t).
            if int(e.get("s", 0)) and e["x"] == "queued":
                sends += 1
                record("Send", t=t, s=actor(e["s"]))
                continue
            requests += 1
            record("Request", t=t, q=requests)
            record("Enqueue", t=t, q=requests, x=e["x"])
            continue
        m = 0
        if int(e.get("m", 0)):
            m = monitors.setdefault(int(e["m"]), len(monitors) + 1)
        if a == "Signal":
            if e["o"] == "sup":
                source = "sup"
                children.add(t)
            else:
                source = actors.get(int(e["s"]), "ext")
                signals += 1
            record(a, t=t, s=source, x=e["x"])
            continue
        if a == "Stop":
            signals += 1
        if a in {"Link", "Monitor"} and e["x"] in {"noproc", "try", "late"}:
            late += 1
        source = actor(e["s"]) if a in {"Link", "Unlink", "Monitor", "Deliver"} else "none"
        record(a, t=t, s=source, x=e["x"], m=m, k=e["k"], o=e["o"], p=i in folded)
    constants = {
        "actors": [actors[k] for k in actors],
        "parents": {actors[c]: actors[p] for c, p in parents.items()},
        "children": sorted(children),
        "requests": requests,
        "monitors": len(monitors),
        "signals": signals,
        "late": late,
        "sends": sends,
    }
    return records, constants


def generate_lifecycle(trace, workdir):
    """A group of actors' exit signals, links, monitors and requests against
    Lifecycle.tla."""
    for model in ("Lifecycle.tla", "TraceLifecycle.tla"):
        shutil.copy(os.path.join(HERE, model), workdir)
    records, c = lifecycle_records(trace)

    def tla_set(values):
        return "{" + ", ".join(tla_string(v) for v in values) + "}"
    if c["parents"]:
        parent = "CASE " + " [] ".join(f"a = {tla_string(ch)} -> {tla_string(p)}"
                                       for ch, p in c["parents"].items()) + ' [] OTHER -> "none"'
    else:
        parent = '"none"'
    with open(os.path.join(workdir, "TraceRun.tla"), "w") as out:
        out.write("---- MODULE TraceRun ----\nEXTENDS TraceLifecycle\n"
                  f"TraceActors == {tla_set(c['actors'])}\n"
                  f"TraceChildren == {tla_set(c['children'])}\n"
                  f"TraceLinkParent == [a \\in TraceActors |-> {parent}]\n"
                  f"TraceReqs == 1..{c['requests']}\n"
                  f"TraceMonIds == 1..{c['monitors']}\n"
                  "TraceLog == <<\n    " + ",\n    ".join(records) + "\n>>\n====\n")
    with open(os.path.join(workdir, "TraceRun.cfg"), "w") as out:
        out.write("INIT TraceInit\nNEXT TraceNext\nCONSTANTS\n"
                  "    Actors <- TraceActors\n    Children <- TraceChildren\n"
                  "    LinkParent <- TraceLinkParent\n    Reqs <- TraceReqs\n"
                  f"    MonIds <- TraceMonIds\n    MaxSends = {c['sends']}\n"
                  f"    MaxSignals = {c['signals']}\n    MaxLate = {c['late']}\n"
                  "    UnlinkEnabled = TRUE\n    TrapCanClear = TRUE\n"
                  "    SelfReasons <- Reasons\n    SignalReasons <- Reasons\n"
                  "    AdminReasons <- Reasons\n    DropEnabled = TRUE\n"
                  "    Fault = \"None\"\n    Trace <- TraceLog\n"
                  "INVARIANT Safety\nINVARIANT TraceIncomplete\n")


def generate_jobs(trace, workdir):
    """One owner instance's pipe-to-self jobs against DeferredJobs.tla."""
    for model in ("DeferredJobs.tla", "TraceJobs.tla"):
        shutil.copy(os.path.join(HERE, model), workdir)
    if int(trace.get("keys", 0)) > 1:
        raise ValueError("DeferredJobs.tla models one key per trace")
    # Number jobs 1..J in the order they appear; Exit carries job 0.
    jobs = {}
    for e in trace["events"]:
        if int(e["j"]):
            jobs.setdefault(int(e["j"]), len(jobs) + 1)
    if not jobs:
        raise ValueError("a jobs trace needs at least one job")
    keyed = sorted({jobs[int(e["j"])] for e in trace["events"] if e["a"] == "Start" and e["keyed"]})
    # Emits and delayed emits (Schedule) both count towards MaxEmit.
    emits = {}
    for e in trace["events"]:
        if e["a"] in {"Emit", "Schedule"}:
            emits[int(e["j"])] = emits.get(int(e["j"]), 0) + 1

    def flag(value):
        return "TRUE" if value else "FALSE"

    records = ",\n    ".join(
        "[a |-> {}, j |-> {}, x |-> {}, keyed |-> {}, dl |-> {}, fenced |-> {}, rel |-> {}]".format(
            tla_string(e["a"]), jobs.get(int(e["j"]), 0), tla_string(e["x"]),
            flag(e["keyed"]), flag(e["dl"]), flag(e["fenced"]), flag(e.get("rel", False)))
        for e in trace["events"])
    with open(os.path.join(workdir, "TraceRun.tla"), "w") as out:
        out.write("---- MODULE TraceRun ----\nEXTENDS TraceJobs\n"
                  "TraceKeyed == {" + ", ".join(map(str, keyed)) + "}\n"
                  f"TraceLog == <<\n    {records}\n>>\n====\n")
    with open(os.path.join(workdir, "TraceRun.cfg"), "w") as out:
        out.write("INIT TraceInit\nNEXT TraceNext\nCONSTANTS\n"
                  f"    J = {len(jobs)}\n    JobBound = {len(jobs)}\n    MaxGen = 1\n"
                  f"    MaxEmit = {max(emits.values(), default=0)}\n"
                  f"    Credits = {int(trace['credits'])}\n    Fault = \"None\"\n"
                  "    Keyed <- TraceKeyed\n    Trace <- TraceLog\n"
                  "INVARIANT Safety\nINVARIANT TraceIncomplete\n")


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
        if "Invariant Safety is violated" in log:
            verdict, detail = "SAFETY", "model invariant violated along the trace\n" + log
        elif "Invariant TraceIncomplete is violated" in log:
            verdict, detail = "ACCEPTED", f"{len(events)} events"
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
