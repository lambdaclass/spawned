#!/usr/bin/env python3
"""Measure which TLA+ model states and actions recorded lifecycle traces reach.

Trace validation (trace_check.py) shows that each recorded Rust execution is
a behavior of the model. This script asks the converse question: which parts
of the model's reachable behavior do the recorded executions never exercise?

Concrete model states depend on the constants (children, restart budget,
generation bound), which differ between the model-checking configurations
and the recorded traces, so both sides are compared on a projection defined
in TLA+ (SupervisionCoverage.tla, CohortsCoverage.tla, NestedCoverage.tla):

  A  action keys      action name plus outcome ("Decide/restart")
  S  state keys       one per child (or node): its local view of the state
  T  transition keys  an action key in the context of its pre-state
  C  coarse T keys    T without mode, budget bucket and similar detail

Model side: TLC explores each coverage configuration (the CI model-checking
constants, plus a Temporary-child instance of Supervision) with
Cov<Model>.tla, whose action constraint prints every key of every reachable
step. The explored state count must equal the plain model's (the coverage
module adds only an unfingerprinted label).

Trace side: every trace is replayed with CovTrace<Model>.tla, which adds a
history variable collecting the keys of each replayed step; the accepted
behavior's final value is the trace's coverage. When a log event admits
several model steps (an "unknown" exit reason, an unlogged window expiry),
the first accepted behavior TLC finds is used.

Usage:
  trace_coverage.py [--traces [LABEL=]DIR]... [--jobs N] [--workers N]
                    [--model-keys CACHE] [--json OUT] [--baseline FILE] [--write-baseline FILE]
                    [--top N]

Each --traces source is reported cumulatively in the given order (for example
the dedicated trace tests, then the whole test suite). --baseline FILE fails
(exit 1) if an action key listed there is no longer covered; coverage
percentages themselves never fail the run.
"""
import argparse
import concurrent.futures
import gzip
import hashlib
import io
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile

import trace_check

HERE = os.path.dirname(os.path.abspath(__file__))

# name -> (coverage module, MC module or None, cfg constants source or literal)
MODELS = {
    "Supervision": {
        "cov": "CovSupervision", "proj": "SupervisionCoverage",
        "configs": [
            ("MCSupervision", "MCSupervision.cfg", None),
            ("MCSupervision", None,
             "CONSTANTS\n    N = 2\n    MaxRestarts = 2\n    MaxGen = 3\n"
             "    Mode <- CovModeTemporary\nCONSTRAINT GenBound\n"),
        ],
        "schemas": {"one-for-one-v1"},
    },
    "Cohorts": {
        "cov": "CovCohorts", "proj": "CohortsCoverage",
        "configs": [
            ("MCCohortsAll", "MCCohortsAll.cfg", None),
            ("MCCohortsRest", "MCCohortsRest.cfg", None),
            ("MCCohortsAllSupersession", "MCCohortsAllSupersession.cfg", None),
            ("MCCohortsRestSupersession", "MCCohortsRestSupersession.cfg", None),
        ],
        "schemas": {"cohort-v1"},
    },
    "Nested": {
        "cov": "CovNested", "proj": "NestedCoverage",
        "configs": [("MCNested", "MCNested.cfg", None)],
        "schemas": {"tree-v1"},
    },
}

# Steps the model takes that no trace event can name (fairness devices).
UNOBSERVABLE = {"Nested": ("A|Deliver", "T|Deliver|")}

KEY = re.compile(r'"((?:[^"\\]|\\.)*)"')
TUPLE = re.compile(r"<<(.*?)>>", re.S)
VALID = re.compile(r"^[AST]\|.")


def render(fields):
    """<<kind, action, outcome, context...>> -> kind|action/outcome|context..."""
    head = fields[0] + "|" + fields[1] + ("/" + fields[2] if fields[2] else "")
    return head + "".join("|" + f for f in fields[3:] if f)


def parse_keys(text):
    """Keys printed by TLC: tuples of strings (rendered) or plain strings."""
    tuples = TUPLE.findall(text)
    keys = {render(KEY.findall(t)) for t in tuples} if tuples else set(KEY.findall(text))
    return {k for k in keys if VALID.match(k)}


def tlc_env(workdir):
    env = dict(os.environ)
    env["JAVA_TOOL_OPTIONS"] = (
        env.get("JAVA_TOOL_OPTIONS", "") + f" -Djava.io.tmpdir={workdir}"
    ).strip()
    return env


def constants_of(cfg_text):
    """Keep the CONSTANTS and CONSTRAINT sections of an MC configuration."""
    keywords = {"CONSTANT", "CONSTANTS", "CONSTRAINT", "CONSTRAINTS", "SPECIFICATION",
                "INIT", "NEXT", "INVARIANT", "INVARIANTS", "PROPERTY", "PROPERTIES",
                "VIEW", "SYMMETRY", "ACTION_CONSTRAINT", "ACTION_CONSTRAINTS"}
    kept = {"CONSTANT", "CONSTANTS", "CONSTRAINT", "CONSTRAINTS"}
    out, section = [], None
    for line in cfg_text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("\\*"):
            continue
        head = stripped.split()[0]
        if head in keywords:
            section = head
        if section in kept:
            out.append(line)
    return "\n".join(out) + "\n"


def run_model_config(tlc, workers, model, mc_module, cfg_file, literal):
    workdir = tempfile.mkdtemp(prefix="cov-model-")
    try:
        for f in os.listdir(HERE):
            if f.endswith(".tla"):
                shutil.copy(os.path.join(HERE, f), workdir)
        if cfg_file:
            with open(os.path.join(HERE, cfg_file)) as f:
                constants = constants_of(f.read())
        else:
            constants = literal
        cov = MODELS[model]["cov"]
        with open(os.path.join(workdir, "CovRun.tla"), "w") as f:
            f.write(f"---- MODULE CovRun ----\nEXTENDS {cov}, {mc_module}\n====\n")
        with open(os.path.join(workdir, "CovRun.cfg"), "w") as f:
            f.write("INIT CovInit\nNEXT CovNext\nVIEW CovView\nACTION_CONSTRAINT Emit\n" + constants)
        with open(os.path.join(workdir, "Plain.tla"), "w") as f:
            f.write(f"---- MODULE Plain ----\nEXTENDS {MODELS[model]['proj']}, {mc_module}\n====\n")
        with open(os.path.join(workdir, "Plain.cfg"), "w") as f:
            f.write("INIT Init\nNEXT Next\n" + constants)
        base = shlex.split(tlc) + ["-deadlock", "-workers", str(workers), "-cleanup"]
        counts = {}
        keys = set()
        for spec in ("CovRun", "Plain"):
            run = subprocess.run(base + ["-metadir", os.path.join(workdir, spec + "-states"), spec],
                                 cwd=workdir, env=tlc_env(workdir), capture_output=True, text=True)
            log = run.stdout + run.stderr
            # The final count (progress lines report partial counts).
            m = re.search(r"([\d,]+) distinct states found, 0 states left on queue", log)
            # (TLC's exit status is not reliable here; the log is.)
            if "No error has been found" not in log or not m:
                raise RuntimeError(f"TLC failed on {model} {spec} ({cfg_file or 'literal'}):\n{log[-4000:]}")
            counts[spec] = int(m.group(1).replace(",", ""))
            if spec == "CovRun":
                # TLC wraps long tuples over several lines: parse the whole output.
                keys = parse_keys(run.stdout)
        if counts["CovRun"] != counts["Plain"]:
            raise RuntimeError(
                f"{cov} explores {counts['CovRun']} states but the model {counts['Plain']}: "
                "CovNext no longer matches Next")
        return keys, counts["Plain"]
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def open_cache(path, mode):
    """Text file, gzip-compressed if PATH ends in .gz (mtime 0, so an
    unchanged cache is rewritten byte for byte)."""
    if not path.endswith(".gz"):
        return open(path, mode)
    if "r" in mode:
        return gzip.open(path, "rt", encoding="utf-8")
    return io.TextIOWrapper(gzip.GzipFile(path, "wb", mtime=0), encoding="utf-8")


def model_digest():
    """Hash of everything the model side depends on, to validate a cache."""
    h = hashlib.sha256(repr(sorted((m, repr(v)) for m, v in MODELS.items())).encode())
    for f in sorted(os.listdir(HERE)):
        if f.endswith((".tla", ".cfg")) and not f.startswith("Trace") and "Ind" not in f:
            with open(os.path.join(HERE, f), "rb") as fh:
                h.update(f.encode() + b"\0" + fh.read())
    return h.hexdigest()


def model_keys(tlc, workers, jobs):
    result = {m: set() for m in MODELS}
    states = {m: 0 for m in MODELS}
    tasks = [(m, *c) for m, spec in MODELS.items() for c in spec["configs"]]
    with concurrent.futures.ThreadPoolExecutor(max(1, jobs)) as pool:
        futures = {pool.submit(run_model_config, tlc, workers, *t): t for t in tasks}
        for future in concurrent.futures.as_completed(futures):
            model = futures[future][0]
            keys, count = future.result()
            result[model] |= keys
            states[model] += count
    return result, states


def model_of(trace):
    schema = trace.get("schema", "one-for-one-v1")
    for name, spec in MODELS.items():
        if schema in spec["schemas"]:
            return name
    raise ValueError(f"unsupported trace schema: {schema}")


COV_TRACE = {"Supervision": "CovTraceSupervision", "Cohorts": "CovTraceCohorts",
             "Nested": "CovTraceNested"}


def trace_keys(path, tlc):
    """Return (model, verdict, keys) for one recorded trace."""
    with open(path) as f:
        trace = json.load(f)
    model = model_of(trace)
    if not trace.get("events"):
        return model, "EMPTY", set()
    workdir = tempfile.mkdtemp(prefix="cov-trace-")
    try:
        trace_check.generate(trace, workdir)
        for f in os.listdir(HERE):
            if f.endswith(".tla") and ("Coverage" in f or f.startswith("Cov")):
                shutil.copy(os.path.join(HERE, f), workdir)
        with open(os.path.join(workdir, "CovRun.tla"), "w") as f:
            f.write(f"---- MODULE CovRun ----\nEXTENDS TraceRun, {COV_TRACE[model]}\n====\n")
        with open(os.path.join(workdir, "TraceRun.cfg")) as f:
            cfg = f.read()
        cfg = cfg.replace("INIT TraceInit", "INIT CovTraceInit").replace("NEXT TraceNext", "NEXT CovTraceNext")
        cfg = cfg.replace("INVARIANT Safety\n", "")
        with open(os.path.join(workdir, "CovRun.cfg"), "w") as f:
            f.write(cfg)
        cmd = shlex.split(tlc) + ["-deadlock", "-workers", "1", "-cleanup",
                                  "-metadir", os.path.join(workdir, "states"), "CovRun"]
        run = subprocess.run(cmd, cwd=workdir, env=tlc_env(workdir), capture_output=True, text=True)
        log = run.stdout + run.stderr
        if "Invariant TraceIncomplete is violated" not in log:
            return model, "REJECTED", set()
        # The last printed state of the error trace is the accepted one.
        last = log.rfind("/\\ cov = ")
        block = log[last + 1:]
        end = re.search(r"\n/\\ |\n\s*\n|\nState \d+|\n\d+ states generated", block)
        block = block[: end.start()] if end else block
        return model, "ACCEPTED", parse_keys(block)
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


# Trace schemas validated by trace_check.py but without a coverage model
# (the deferred-replies models and Lifecycle.tla): skipped here.
UNCOVERED_SCHEMAS = {"jobs-v1", "replies-v1", "lifecycle-v1"}


def covered(path):
    with open(path) as f:
        return json.load(f).get("schema") not in UNCOVERED_SCHEMAS


def collect(paths):
    files = []
    for p in paths:
        if os.path.isdir(p):
            files += sorted(os.path.join(p, f) for f in os.listdir(p) if f.endswith(".json"))
        elif os.path.exists(p):
            files.append(p)
    return [f for f in files if covered(f)]


# ---------------------------------------------------------------------------
# Ranking of unreached keys: higher is more interesting. Keys are
# "KIND|action|context..." strings; the weights pick out shutdown racing
# in-flight work, exhausted budgets, deadlines and escalation.

WEIGHTS = {
    "Supervision": [
        (r"\|deadline", 4), (r"\|closed", 2), (r"\|failed", 2), (r"\|max", 2),
        (r"^[TC]\|[^|]+\|((Permanent|Transient|Temporary)\|)?(backoff|attempt|constructing|cleanup|decide|top)\|", 3),
        (r"^[TC]\|(Close|ShutdownDeadline|SupStopTurn|SupChildTimeout)\|", 2),
        (r"BackoffDone/exhausted|CleanupStuck|SupChildTimeout|Decide/fail|StartupFailed/timeout", 3),
        (r"RetryRelease|BecomeReady/closed|Attempt/closed|BackoffDone/closed|Decide/closed", 2),
        (r"\|reserved", 1), (r"Temporary", 1),
    ],
    "Cohorts": [
        (r"/closed", 3), (r"superseded|/stale", 3), (r"CRetire", 2), (r"\|P", 1),
        (r"\|(stopped|clean|resumed)", 2), (r"Temporary|Transient", 1), (r"/initial", 1),
    ],
    "Nested": [
        (r"cancelled|closed", 2), (r"TBegin/restart", 3), (r"\+sub", 2),
        (r"parent:(closed|cancelled)", 3), (r"TJoined/owner", 2), (r"\+prop", 1),
    ],
}


# Context the coarse transition keys (C) drop: restart mode, budget bucket,
# failure flag and position relative to the failing child. One coarse key
# stands for a situation ("Close while a child is in backoff during
# startup"); the fine keys are its variants.
DETAIL = {
    "Supervision": {"Permanent", "Transient", "Temporary", "failed", "0", "mid", "max", "reserved"},
    "Cohorts": {"Permanent", "Transient", "Temporary", "before", "after", "origin", "none"},
    "Nested": set(),
}


def coarse(model, key):
    parts = re.split(r"[|@]", key)
    return "C|" + "|".join(p for p in parts[1:] if p not in DETAIL[model])


def with_coarse(model, keys):
    return keys | {coarse(model, k) for k in keys if k.startswith("T|")}


def score(model, key):
    s = {"A": 10, "C": 1, "T": 1, "S": 0}[key[0]]
    for pattern, weight in WEIGHTS.get(model, []):
        if re.search(pattern, key):
            s += weight
    if "WindowExpire" in key:
        s -= 5  # timing only: an unlogged stutter in the traces
    return s


KINDS = "ASTC"


def kinds(keys):
    return {k: {x for x in keys if x.startswith(k + "|")} for k in KINDS}


def pct(a, b):
    return f"{100.0 * a / b:5.1f}%" if b else "  n/a"


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--traces", action="append", default=[],
                        help="[LABEL=]DIR or file of recorded traces (repeatable)")
    parser.add_argument("--tlc", default=os.environ.get("TLC", "tlc"))
    parser.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 2) // 2))
    parser.add_argument("--workers", default="4", help="TLC workers per model run")
    parser.add_argument("--model-keys", help="cache of the model side: used if it matches the "
                        "current models, else recomputed and rewritten")
    parser.add_argument("--json", help="write every key set and verdict here")
    parser.add_argument("--baseline", help="fail if an action key listed here is uncovered")
    parser.add_argument("--write-baseline", help="write the covered action keys here")
    parser.add_argument("--top", type=int, default=15, help="unreached keys to print per model")
    args = parser.parse_args()

    digest = model_digest()
    cached = None
    if args.model_keys and os.path.exists(args.model_keys):
        with open_cache(args.model_keys, "rt") as f:
            cached = json.load(f)
        if cached.get("digest") != digest:
            print(f"{args.model_keys} is stale (the models changed); recomputing the model side")
            cached = None
    if cached:
        model = {m: {k for k in v if VALID.match(k)} for m, v in cached["keys"].items()}
        state_counts = cached["states"]
    else:
        model, state_counts = model_keys(args.tlc, args.workers, min(args.jobs, 4))
        if args.model_keys:
            with open_cache(args.model_keys, "wt") as f:
                json.dump({"digest": digest, "states": state_counts,
                           "keys": {m: sorted(v) for m, v in model.items()}}, f, indent=0)
                f.write("\n")
            print(f"wrote {args.model_keys}")
    for name, prefixes in UNOBSERVABLE.items():
        model[name] = {k for k in model[name] if not k.startswith(prefixes)}
    model = {name: with_coarse(name, keys) for name, keys in model.items()}

    sources = []
    for spec in args.traces:
        label, _, path = spec.rpartition("=")
        sources.append((label or path, collect([path])))

    covered = {m: set() for m in MODELS}
    verdicts = []
    rows = []
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        for label, files in sources:
            counts = {m: [0, 0] for m in MODELS}
            for path, (name, verdict, keys) in zip(files, pool.map(lambda f: trace_keys(f, args.tlc), files)):
                verdicts.append({"source": label, "trace": os.path.basename(path),
                                 "model": name, "verdict": verdict, "keys": len(keys)})
                counts[name][1] += 1
                if verdict == "ACCEPTED":
                    counts[name][0] += 1
                    covered[name] |= with_coarse(name, keys)
                elif verdict == "REJECTED":
                    print(f"not counted (rejected by the model): {label}: {os.path.basename(path)}")
            for name in MODELS:
                mk, tk = kinds(model[name]), kinds(covered[name] & model[name])
                rows.append((label, name, counts[name], {k: (len(tk[k]), len(mk[k])) for k in KINDS}))

    print("\nTrace coverage of the TLA+ models (projected keys; see verification/tla/README.md)")
    print(f"{'source (cumulative)':24} {'model':12} {'traces':>9} {'actions (A)':>17} {'states (S)':>17} "
          f"{'transitions (T)':>17} {'coarse T (C)':>17}")
    for label, name, (ok, total), cov in rows:
        cells = [f"{a:4}/{b:<4} {pct(a, b)}" for a, b in (cov[k] for k in KINDS)]
        print(f"{label[:24]:24} {name:12} {ok:4}/{total:<4} " + " ".join(f"{c:>17}" for c in cells))
    for name in MODELS:
        beyond = covered[name] - model[name]
        print(f"{name}: model {state_counts[name]} states over {len(MODELS[name]['configs'])} configuration(s); "
              f"{len(beyond)} trace keys outside the configured bounds")

    for name in MODELS:
        # Rank unreached actions and coarse transitions; each coarse key
        # stands for its unreached fine variants (count shown).
        missing = model[name] - covered[name]
        variants = {}
        for k in missing:
            if k.startswith("T|"):
                variants[coarse(name, k)] = variants.get(coarse(name, k), 0) + 1
        ranked = sorted((k for k in missing if k[0] in "AC"), key=lambda k: (-score(name, k), k))
        print(f"\n{name}: {len(missing)} unreached keys; top {min(args.top, len(ranked))} "
              "unreached actions and coarse transitions:")
        for k in ranked[: args.top]:
            extra = f"  ({variants[k]} variants)" if k in variants else ""
            print(f"  [{score(name, k):2}] {k}{extra}")

    if args.json:
        with open(args.json, "w") as f:
            json.dump({
                "model": {m: sorted(v) for m, v in model.items()},
                "covered": {m: sorted(v) for m, v in covered.items()},
                "unreached": {m: sorted(model[m] - covered[m], key=lambda k: (-score(m, k), k)) for m in MODELS},
                "beyond": {m: sorted(covered[m] - model[m]) for m in MODELS},
                "traces": verdicts,
            }, f, indent=1)

    actions = {m: sorted(k for k in covered[m] & model[m] if k.startswith("A|")) for m in MODELS}
    if args.write_baseline:
        with open(args.write_baseline, "w") as f:
            json.dump(actions, f, indent=1)
            f.write("\n")
    if args.baseline:
        with open(args.baseline) as f:
            required = json.load(f)
        lost = [(m, k) for m, ks in required.items() for k in ks if k not in covered.get(m, set())]
        for m, k in lost:
            print(f"LOST coverage: {m} {k}")
        if lost:
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
