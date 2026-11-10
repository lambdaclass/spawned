#!/usr/bin/env python3
"""Require rejection of deliberately invalid real trace edits and model mutations.

Run after recording all trace suites. These checks fail if a checker/model fault
lets an invalid behavior pass. They do not manufacture accepted evidence.
"""
import argparse
import copy
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import trace_check

HERE = Path(__file__).resolve().parent


def require_rejected(trace, mutate, label, tlc):
    bad = copy.deepcopy(trace)
    mutate(bad["events"])
    with tempfile.TemporaryDirectory(prefix="bad-trace-") as directory:
        path = Path(directory) / "trace.json"
        path.write_text(json.dumps(bad))
        _, verdict, detail = trace_check.check(str(path), tlc, None)
        if verdict not in {"REJECTED", "SAFETY"}:
            raise AssertionError(f"{label}: expected rejection, got {verdict}: {detail}")
    print(f"REJECTED mutation: {label}")


def move_resume_early(events):
    resume = next(i for i, e in enumerate(events) if e["a"] == "CResume")
    stop = next(i for i, e in enumerate(events) if e["a"] == "CStop")
    events.insert(stop, events.pop(resume))


def wrong_selection(events):
    event = next(e for e in events if e["a"] == "CSelect")
    event["n"] = 2 if event["n"] == 1 else 1


def remove_descendant_done(events):
    attach = next(e for e in events if e["a"] == "TAttach")
    events.pop(next(i for i, e in enumerate(events) if e["a"] == "TDone" and e["node"] == attach["peer"]))


def false_propagation(events):
    next(e for e in events if e["a"] == "TPropagate")["cancelled"] = False


def unsafe_final_event(trace, tlc):
    # A deliberate model mutation admits a start after Close. Both Safety and
    # TraceIncomplete fail on the same final event: Safety must take precedence.
    original = trace_check.generate
    def mutated_generate(tape, work):
        original(tape, work)
        cfg = Path(work) / "TraceRun.cfg"
        cfg.write_text(cfg.read_text().replace('Fault = "None"', 'Fault = "Closed"'))
    bad = copy.deepcopy(trace)
    begin = next(e for e in bad["events"] if e["a"] == "CBegin")
    bad["events"] = [{"a":"CClose","c":0,"x":"","n":0,"r":0}, begin]
    trace_check.generate = mutated_generate
    try:
        with tempfile.TemporaryDirectory(prefix="unsafe-completion-") as directory:
            path = Path(directory) / "trace.json"
            path.write_text(json.dumps(bad))
            _, verdict, detail = trace_check.check(str(path), tlc, None)
            if verdict != "SAFETY":
                raise AssertionError(f"unsafe final event accepted/misclassified: {verdict}: {detail}")
    finally:
        trace_check.generate = original
    print("SAFETY before completion: invalid final event")


def model_fault(module, fault, tlc, temporal=False):
    with tempfile.TemporaryDirectory(prefix="bad-model-") as directory:
        work = Path(directory)
        for source in HERE.glob("*.tla"):
            shutil.copy(source, work)
        cfg = (HERE / f"{module}.cfg").read_text().replace('Fault = "None"', f'Fault = "{fault}"')
        (work / f"{module}.cfg").write_text(cfg)
        result = subprocess.run(shlex.split(tlc) + ["-workers", "1", "-deadlock", "-cleanup", module],
                                cwd=work, capture_output=True, text=True)
        log = result.stdout + result.stderr
        expected = "Temporal properties were violated" if temporal else "Invariant Safety is violated"
        if expected not in log:
            raise AssertionError(f"{module}/{fault}: no expected counterexample\n{log}")
    print(f"COUNTEREXAMPLE model: {module}/{fault}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("traces", type=Path)
    parser.add_argument("--tlc", default=os.environ.get("TLC", "tlc"))
    args = parser.parse_args()
    traces = [json.loads(path.read_text()) for path in args.traces.glob("*.json")]
    context = next(t for t in traces if t["name"] == "context_exit_reasons")
    def erase_abnormal_exit(events):
        for event in events:
            if event.get("x") == "Kill":
                event["x"] = "Normal"
    require_rejected(context, erase_abnormal_exit,
                     "transient restart after erased abnormal context exit", args.tlc)
    drain = next(t for t in traces if t["name"] == "queued_drop_panic")
    def erase_drain_failure(events):
        for event in events:
            if event.get("x") == "Panic":
                event["x"] = "Normal"
    require_rejected(drain, erase_drain_failure,
                     "transient restart after erased queued destructor panic", args.tlc)
    shutdown = next(t for t in traces if t["name"] == "queued_drop_preserves_shutdown")
    def escalate_shutdown(events):
        for event in events:
            if event.get("x") == "Shutdown":
                event["x"] = "Panic"
    require_rejected(shutdown, escalate_shutdown,
                     "transient retirement after incorrectly escalated shutdown", args.tlc)
    cohort = next(t for t in traces if t.get("schema") == "cohort-v1" and t["strategy"] == "OneForAll")
    tree = next(t for t in traces if t.get("schema") == "tree-v1" and any(e["a"] == "TPropagate" for e in t["events"]))
    require_rejected(cohort, move_resume_early, "resume before cohort cleanup", args.tlc)
    require_rejected(cohort, wrong_selection, "wrong strategy selection", args.tlc)
    require_rejected(tree, remove_descendant_done, "parent join before descendant completion", args.tlc)
    require_rejected(tree, false_propagation, "propagation without cancellation", args.tlc)
    unsafe_final_event(cohort, args.tlc)
    for fault in ("Selection", "Cleanup", "Retirement", "Closed"):
        model_fault("MCCohortsAll", fault, args.tlc)
    model_fault("MCCoordinator", "ReleaseEarly", args.tlc)
    model_fault("MCCoordinator", "ForgetFailure", args.tlc)
    model_fault("MCCoordinator", "ForgetSignal", args.tlc, temporal=True)
    model_fault("MCNested", "Join", args.tlc)
    model_fault("MCNested", "Propagation", args.tlc, temporal=True)


if __name__ == "__main__":
    main()
