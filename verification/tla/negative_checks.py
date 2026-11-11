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


def model_fault(module, fault, tlc, temporal=False, invariant="Safety", drop=()):
    """Run MODULE with Fault = FAULT and require a counterexample: a violated
    INVARIANT, or (temporal) a liveness violation. DROP lists invariants removed
    from the cfg first, so a fault that also breaks one of them still reaches
    the temporal check."""
    with tempfile.TemporaryDirectory(prefix="bad-model-") as directory:
        work = Path(directory)
        for source in HERE.glob("*.tla"):
            shutil.copyfile(source, work / source.name)  # writable copy (sources may be read-only)
        cfg = (HERE / f"{module}.cfg").read_text().replace('Fault = "None"', f'Fault = "{fault}"')
        for name in drop:
            cfg = cfg.replace(f"INVARIANT {name}\n", "")
        (work / f"{module}.cfg").write_text(cfg)
        result = subprocess.run(shlex.split(tlc) + ["-workers", "1", "-deadlock", "-cleanup", module],
                                cwd=work, capture_output=True, text=True)
        log = result.stdout + result.stderr
        expected = "Temporal properties were violated" if temporal else f"Invariant {invariant} is violated"
        if expected not in log:
            raise AssertionError(f"{module}/{fault}: no expected counterexample\n{log}")
    print(f"COUNTEREXAMPLE model: {module}/{fault}" + ("" if temporal else f" ({invariant})"))


def named_fault(module, fault, invariant, tlc):
    """Run MODULE (safety only) with Fault = FAULT, checking TypeOK and the
    single named INVARIANT, and require that invariant to be violated. This
    shows the mutation breaks the property it targets, not just Safety."""
    with tempfile.TemporaryDirectory(prefix="bad-model-") as directory:
        work = Path(directory)
        for source in HERE.glob("*.tla"):
            shutil.copyfile(source, work / source.name)  # writable copy (sources may be read-only)
        lines = []
        for line in (HERE / f"{module}.cfg").read_text().splitlines():
            if line.startswith("PROPERTY"):
                continue
            if line.startswith("SPECIFICATION"):
                line = "SPECIFICATION Spec"
            if line.startswith("INVARIANT"):
                line = f"INVARIANT TypeOK\nINVARIANT {invariant}"
            lines.append(line.replace('Fault = "None"', f'Fault = "{fault}"'))
        (work / f"{module}.cfg").write_text("\n".join(lines) + "\n")
        result = subprocess.run(shlex.split(tlc) + ["-workers", "1", "-deadlock", "-cleanup", module],
                                cwd=work, capture_output=True, text=True)
        log = result.stdout + result.stderr
        if f"Invariant {invariant} is violated" not in log:
            raise AssertionError(f"{module}/{fault}: {invariant} not violated\n{log}")
    print(f"COUNTEREXAMPLE model: {module}/{fault} ({invariant})")


def spec_mutation(module, spec, label, pattern, replacement, prop, tlc):
    """Run MODULE with one guard of SPEC rewritten (for models without a
    Fault constant), checking TypeOK and the single action property PROP,
    and require PROP to be violated."""
    import re
    with tempfile.TemporaryDirectory(prefix="bad-spec-") as directory:
        work = Path(directory)
        for source in HERE.glob("*.tla"):
            shutil.copyfile(source, work / source.name)  # writable copy (sources may be read-only)
        text = (work / spec).read_text()
        mutated, count = re.subn(pattern, replacement, text, flags=re.S)
        if count != 1:
            raise AssertionError(f"{spec}/{label}: mutation did not apply")
        (work / spec).write_text(mutated)
        lines = []
        # Keep SPECIFICATION, CONSTANTS and CONSTRAINT (which precede the
        # checked invariants and properties in the cfg).
        for line in (HERE / f"{module}.cfg").read_text().splitlines():
            if line.startswith(("INVARIANT", "PROPERT")):
                break
            lines.append(line)
        lines += ["INVARIANT TypeOK", f"PROPERTY {prop}"]
        (work / f"{module}.cfg").write_text("\n".join(lines) + "\n")
        result = subprocess.run(shlex.split(tlc) + ["-workers", "1", "-deadlock", "-cleanup", module],
                                cwd=work, capture_output=True, text=True)
        log = result.stdout + result.stderr
        if f"property {prop} is violated" not in log:
            raise AssertionError(f"{spec}/{label}: {prop} not violated\n{log}")
    print(f"COUNTEREXAMPLE model: {spec}/{label} ({prop})")


# S1 (otp-exit-signals.md): v0.7's startup behaviours against Supervision.tla,
# which has no Fault constant: (label, pattern, replacement).
SUPERVISION_S1_CONTROLS = [
    # v0.7 retried a failed first start of a Permanent/Transient child.
    ("FirstStartRetry",
     r'    init /\\ \(Restarts\(m, r\) \\/ \(~ready /\\ m # "Temporary"\)\)',
     r'    Restarts(m, r) \\/ (~ready /\\ init /\\ m # "Temporary")'),
    # v0.7 decided at once on a ready child's exit during the initial start.
    ("NoDeferral",
     r'(Decide\(c\) ==\s*/\\ pc\[c\] = "decide")\s*/\\ ~\(open /\\ ~sready /\\ readyGen\[c\]\)',
     r'\1'),
]

def source_fault(module, source, old, new, invariant, tlc):
    """Run MODULE (safety only: TypeOK and INVARIANT) with OLD replaced by NEW
    in SOURCE, for models without a Fault constant, and require INVARIANT to
    be violated."""
    with tempfile.TemporaryDirectory(prefix="bad-model-") as directory:
        work = Path(directory)
        for path in HERE.glob("*.tla"):
            shutil.copy(path, work)
        target = work / source
        text = target.read_text()
        if text.count(old) != 1:
            raise AssertionError(f"{source}: mutation anchor {old!r} not found exactly once")
        # Copies from the Nix store are read-only.
        target.chmod(0o644)
        target.write_text(text.replace(old, new))
        lines = []
        properties = False
        for line in (HERE / f"{module}.cfg").read_text().splitlines():
            if line.startswith("PROPERT"):
                properties = True
                continue
            if properties and line.startswith((" ", "\t")):
                continue
            properties = False
            if line.startswith("INVARIANT"):
                line = f"INVARIANT TypeOK\nINVARIANT {invariant}"
            lines.append(line)
        (work / f"{module}.cfg").write_text("\n".join(lines) + "\n")
        result = subprocess.run(shlex.split(tlc) + ["-workers", "1", "-deadlock", "-cleanup", module],
                                cwd=work, capture_output=True, text=True)
        log = result.stdout + result.stderr
        if f"Invariant {invariant} is violated" not in log:
            raise AssertionError(f"{module}/{source} mutation: {invariant} not violated\n{log}")
    print(f"COUNTEREXAMPLE model: {module}/{source} mutation ({invariant})")


# Rank 11 design models: (module, fault, invariant it must violate).
def named_temporal(module, fault, prop, tlc, constants=None):
    """Run MODULE with Fault = FAULT, TypeOK and the single temporal property
    PROP, and require a liveness counterexample for exactly that property.
    CONSTANTS overrides constant values (a smaller instance keeps the
    single-worker liveness check fast)."""
    constants = constants or {}
    with tempfile.TemporaryDirectory(prefix="bad-model-") as directory:
        work = Path(directory)
        for source in HERE.glob("*.tla"):
            shutil.copyfile(source, work / source.name)  # writable copy (sources may be read-only)
        lines = []
        for line in (HERE / f"{module}.cfg").read_text().splitlines():
            if line.startswith(("PROPERTY", "INVARIANT")):
                continue
            name = line.strip().split(" ")[0]
            if name in constants:
                line = f"    {name} = {constants[name]}"
            lines.append(line.replace('Fault = "None"', f'Fault = "{fault}"'))
        lines += ["INVARIANT TypeOK", f"PROPERTY {prop}"]
        (work / f"{module}.cfg").write_text("\n".join(lines) + "\n")
        result = subprocess.run(shlex.split(tlc) + ["-workers", "1", "-deadlock", "-cleanup", module],
                                cwd=work, capture_output=True, text=True)
        log = result.stdout + result.stderr
        if "Temporal properties were violated" not in log:
            raise AssertionError(f"{module}/{fault}: {prop} not violated\n{log}")
    print(f"COUNTEREXAMPLE model: {module}/{fault} ({prop})")


# Base actor lifecycle (docs/design/otp-exit-signals.md): the v0.7 exit-signal
# behaviours and the PR #182/#185 bugs, each against the invariant it breaks.
LIFECYCLE_CONTROLS = [
    ("MCLifecycle", "DirectStop", "TrapRespected"),
    ("MCLifecycle", "DirectStop", "NormalSignalIgnored"),
    ("MCLifecycle", "KillAsKill", "KilledFromKill"),
    ("MCLifecycle", "KillNoSupersede", "KilledFromKill"),
    ("MCLifecycleFence", "NoUnlinkFence", "CancelFenced"),
    ("MCLifecycleFence", "NoDemonitorFence", "CancelFenced"),
    ("MCLifecycleFence", "NoDemonitorFlush", "CancelFenced"),
    ("MCLifecycle", "EarlyStopped", "StoppedAtDeath"),
    ("MCLifecycle", "PanicOverKill", "KilledFromKill"),
    ("MCLifecycle", "DropKeepsNormal", "RuntimeDropReason"),
    ("MCLifecycle", "AlwaysHook", "SkipHook"),
    ("MCLifecycle", "LateSignalIgnored", "LateSignalWins"),
    ("MCLifecycle", "LateRealReason", "NoProcIffLate"),
    ("MCLifecycle", "Retain182", "NoOrphanRequest"),
    ("MCLifecycle", "Dispatch185", "NoDispatchAfterStop"),
    ("MCLifecycle", "DropQueued", "NoOrphanRequest"),
    ("MCLifecycle", "EarlyNotify", "ExitAfterMessages"),
    ("MCLifecycle", "SplitDeathPoint", "NoEdgeToDead"),
    ("MCLifecycle", "PublishBeforeDischarge", "AnsweredBeforePublish"),
    ("MCLifecycle", "NoSignalFence", "SignalsBeforeDeath"),
]

# Liveness controls run on a smaller instance of MCLifecycleLive.
LIFECYCLE_TEMPORAL_CONTROLS = [
    ("ParentAsExit", "ShutdownEndsChild"),
    ("SplitDeathPoint", "DownDelivered"),
]
LIFECYCLE_SMALL = {"Reqs": "{}", "MaxSignals": "0", "DropEnabled": "FALSE"}

DEFERRED_CONTROLS = [
    ("MCDeferredReplies", "NoAutoReply", "NoOrphan"),
    ("MCDeferredReplies", "NoAtomicExpiry", "AtMostOneDischarge"),
    ("MCDeferredReplies", "LateSuccess", "NoSuccessAfterDeadline"),
    ("MCDeferredReplies", "LoopDeadline", "TimelyExpiry"),
    ("MCDeferredReplies", "Lengthen", "DeadlineWithinCaller"),
    ("MCDeferredReplies", "Unbounded", "BoundedA"),
    ("MCDeferredReplies", "NoDestAdmission", "BoundedB"),
    ("MCDeferredReplies", "ExitStateOnly", "NoOrphan"),
    ("MCDeferredReplies", "UnwindDropped", "PanicStopped"),
    ("MCDeferredReplies", "ShareRunning", "BoundedA"),
    ("MCDeferredReplies", "AttachClosed", "BoundedA"),
    ("MCDeferredReplies", "LeakDownstream", "DownstreamOwned"),
    ("MCDeferredReplies", "EarlyClose", "NoEarlyClose"),
    ("MCDeferredReplies", "EarlyStopped", "StoppedAtDeath"),
    ("MCDeferredReplies", "NoReplyFence", "NoReplyAfterDeath"),
    ("MCDeferredReplies", "CallbackBeforeDischarge", "WokenAfterDischarge"),
    ("MCDeferredJobs", "NoInstanceCheck", "NoLateResult"),
    ("MCDeferredJobs", "NoCancelFence", "NoStaleAfterCancel"),
    ("MCDeferredJobs", "DoubleTerminal", "AtMostOneTerminal"),
    ("MCDeferredJobs", "ReleaseOnTimeout", "PermitsBounded"),
    ("MCDeferredJobs", "KeyNoCancel", "OneLivePerKey"),
    ("MCDeferredJobs", "ReplacePendingOnly", "NoStaleKeyed"),
    ("MCDeferredJobsEmits", "NoCancelFence", "NoStaleAfterCancel"),
    ("MCDeferredJobsEmits", "EmitAfterTerminal", "TerminalLast"),
    ("MCDeferredJobsEmits", "NoEmitCredit", "EmitCreditsBounded"),
    ("MCDeferredJobsHeld", "EarlyCredit", "HeldCounted"),
    ("MCDeferredJobsHeld", "HeldDeadlineLost", "DeadlineArmedWhileHeld"),
    ("MCDeferredJobsHeld", "ExitKeepsHeld", "ExitCancelsJobs"),
    ("MCDeferredJobs", "NoAdmissionFence", "ExitCancelsJobs"),
]


def reply_trace_controls(traces, tlc):
    """Edits of recorded reply-slot traces that DeferredReplies.tla must
    reject."""
    by_name = {t["name"]: t for t in traces if t.get("schema") == "replies-v1"}

    def overload_below_bound(events):
        next(e for e in events if e["a"] == "Discharge")["x"] = "overloaded"
    require_rejected(by_name["replies_in_turn"], overload_below_bound,
                     "Overloaded with a free credit", tlc)

    def reply_after_death(events):
        held = next(e for e in events if e["a"] == "Send")
        events.append({"a": "Discharge", "r": held["r"], "x": "ok"})
    require_rejected(by_name["replies_death"], reply_after_death,
                     "stored token answered after death", tlc)

    def discharge_twice(events):
        first = next(i for i, e in enumerate(events) if e["a"] == "Discharge")
        events.insert(first + 1, dict(events[first], x="ok"))
    require_rejected(by_name["replies_expired"], discharge_twice,
                     "second discharge of an expired request", tlc)

    def dropped_before_dispatch(events):
        dispatch = next(i for i, e in enumerate(events) if e["a"] == "Dispatch")
        drop = next(i for i, e in enumerate(events) if e.get("x") == "dropped")
        events.insert(dispatch, events.pop(drop))
    require_rejected(by_name["replies_unused"], dropped_before_dispatch,
                     "unused obligation dropped before its turn", tlc)

    def stopped_before_death(events):
        # Rank 11 before 5e: the token unwound by the panicking turn was
        # answered Stopped at once, before the death point fixed the reason.
        held = next(e for e in events if e["a"] == "Send")
        exit_at = next(i for i, e in enumerate(events) if e["a"] == "Exit")
        events.insert(exit_at, {"a": "Discharge", "r": held["r"], "x": "stopped"})
    require_rejected(by_name["replies_panic"], stopped_before_death,
                     "exit error answered Stopped before the death point", tlc)


def lifecycle_trace_controls(traces, tlc):
    """Edits of recorded base-lifecycle traces that Lifecycle.tla must
    reject: each reintroduces a v0.7 behaviour or breaks a rank 5e/5g rule."""
    by_name = {t["name"]: t for t in traces if t.get("schema") == "lifecycle-v1"}

    def spawned(events, n):
        return [e["t"] for e in events if e["a"] == "Spawn"][n - 1]

    def index(events, action, x=None, after=0):
        return next(i for i, e in enumerate(events)
                    if i >= after and e["a"] == action and (x is None or e["x"] == x))

    def hook_after_kill(events):
        events[index(events, "Hook", "skip")]["x"] = "run"
    require_rejected(by_name["lifecycle_kill_queued_request"], hook_after_kill,
                     "stopped() runs after a kill", tlc)

    def killed_as_kill(events):
        events[index(events, "Die", "Killed")]["x"] = "Kill"
    require_rejected(by_name["lifecycle_kill_queued_request"], killed_as_kill,
                     "a kill ends Kill instead of Killed (v0.7)", tlc)

    def publish_before_death(events):
        events.insert(index(events, "Die"), events.pop(index(events, "Publish")))
    require_rejected(by_name["lifecycle_kill_queued_request"], publish_before_death,
                     "publication before the death point", tlc)

    def untrapped_runs_hook(events):
        events[index(events, "Hook", "skip")]["x"] = "run"
    require_rejected(by_name["lifecycle_untrapped_signal"], untrapped_runs_hook,
                     "stopped() runs after an untrapped exit signal (v0.7)", tlc)

    def normal_signal_stops(events):
        # v0.7: a Normal signal stopped a non-trapping actor.
        at = index(events, "Signal", "Normal")
        events[at + 1:at + 1] = [
            {"a": "Hook", "t": events[at]["t"], "s": 0, "x": "skip", "m": 0, "k": "", "o": ""}]
    require_rejected(by_name["lifecycle_untrapped_signal"], normal_signal_stops,
                     "a Normal exit signal stops a non-trapping actor (v0.7)", tlc)

    def down_with_real_reason(events):
        down = next(e for e in events if e["a"] == "Deliver" and e["k"] == "down")
        down["x"] = "Normal"
    require_rejected(by_name["lifecycle_late_registrations"], down_with_real_reason,
                     "late monitor's Down carries the real reason, not NoProc (v0.7)", tlc)

    def late_link_with_real_reason(events):
        notice = next(e for e in events if e["a"] == "Deliver" and e["k"] == "link")
        notice["x"] = "Normal"
    require_rejected(by_name["lifecycle_late_registrations"], late_link_with_real_reason,
                     "late link's notice carries the real reason, not NoProc (v0.7)", tlc)

    def notice_after_unlink(events):
        a1, a2 = spawned(events, 1), spawned(events, 2)
        at = next(i for i, e in enumerate(events) if e["a"] == "Publish" and e["t"] == a2)
        events.insert(at + 1, {"a": "Deliver", "t": a1, "s": a2, "x": "Shutdown",
                               "m": 0, "k": "link", "o": "death"})
    require_rejected(by_name["lifecycle_unlink_fence"], notice_after_unlink,
                     "death notice delivered after unlink (v0.7 fence)", tlc)

    def down_after_demonitor(events):
        a1, a2 = spawned(events, 1), spawned(events, 2)
        m = next(e["m"] for e in events if e["a"] == "Monitor")
        at = next(i for i, e in enumerate(events) if e["a"] == "Publish" and e["t"] == a2)
        events.insert(at + 1, {"a": "Deliver", "t": a1, "s": a2, "x": "Normal",
                               "m": m, "k": "down", "o": "death"})
    require_rejected(by_name["lifecycle_demonitor_fence"], down_after_demonitor,
                     "Down delivered after demonitor (v0.7 fence)", tlc)

    def parent_exit_as_exit(events):
        a2 = spawned(events, 2)
        next(e for e in events if e["a"] == "Dispatch" and e["t"] == a2
             and e["k"] == "pexit")["k"] = "exit"
    require_rejected(by_name["lifecycle_parent_exit"], parent_exit_as_exit,
                     "parent Shutdown reaches exit_received", tlc)

    def drop_keeps_normal(events):
        events[index(events, "Drop")]["x"] = "Normal"
    require_rejected(by_name["lifecycle_runtime_drop"], drop_keeps_normal,
                     "runtime drop ends Normal instead of Killed (v0.7)", tlc)


def job_trace_controls(traces, tlc):
    """Edits of recorded pipe-to-self job traces that DeferredJobs.tla must
    reject: each erases or reorders what the job core enforces."""
    by_name = {t["name"]: t for t in traces if t.get("schema") == "jobs-v1"}

    def deliver_replaced(events):
        next(e for e in events if e["a"] == "Dispatch" and e["fenced"])["fenced"] = False
    require_rejected(by_name["jobs_keyed_fenced"], deliver_replaced,
                     "replaced keyed job's terminal message delivered", tlc)

    def emit_after_terminal(events):
        finish = next(i for i, e in enumerate(events) if e["a"] == "Finish")
        last_emit = max(i for i, e in enumerate(events) if e["a"] == "Emit")
        events.insert(last_emit, events.pop(finish))
    require_rejected(by_name["jobs_emits"], emit_after_terminal,
                     "emit enqueued after the terminal message", tlc)

    def skip_credit_return(events):
        events.remove(next(e for e in events if e["a"] == "Dispatch"))
    require_rejected(by_name["jobs_emits"], skip_credit_return,
                     "second emit with its credit still in flight", tlc)

    def second_terminal(events):
        next(e for e in events if e["a"] == "Finish")["x"] = "notified"
    require_rejected(by_name["jobs_deadline"], second_terminal,
                     "terminal message after the job deadline's", tlc)

    def forget_cancel(events):
        events.remove(next(e for e in events if e["a"] == "Cancel"))
    require_rejected(by_name["jobs_cancelled"], forget_cancel,
                     "quiet finish of a job that was never cancelled", tlc)

    def dispatch_after_exit(events):
        exit_at = next(i for i, e in enumerate(events) if e["a"] == "Exit")
        dispatch = next(e for e in events if e["a"] == "Dispatch")
        events.remove(dispatch)
        events.insert(exit_at, dispatch)
    require_rejected(by_name["jobs_plain"], dispatch_after_exit,
                     "dispatch after the owner's exit", tlc)


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
    shutdown = next(t for t in traces if t["name"] == "queued_drop_shutdown_restarts")
    def drop_cleanup_failure(events):
        # v0.8: a cleanup failure after Shutdown projects to the abnormal
        # class; projecting the raw Shutdown instead must reject the restart.
        for event in events:
            if event.get("a") == "SelfExit" and event.get("x") == "Panic":
                event["x"] = "Shutdown"
    require_rejected(shutdown, drop_cleanup_failure,
                     "transient restart after a Shutdown without its cleanup failure", args.tlc)
    replacement = next(t for t in traces if t["name"] == "transient_replacement_normal_before_ready")
    def fail_instead_of_retry(events):
        failed = next(i for i, e in enumerate(events)
                      if e["a"] == "StartupFailed" and e["x"] == "Normal")
        decide = next(e for e in events[failed:] if e["a"] == "Decide")
        decide["x"] = "fail"
    require_rejected(replacement, fail_instead_of_retry,
                     "Transient replacement failing the tree instead of retrying", args.tlc)
    cohort = next(t for t in traces if t.get("schema") == "cohort-v1" and t["strategy"] == "OneForAll")
    tree = next(t for t in traces if t.get("schema") == "tree-v1" and any(e["a"] == "TPropagate" for e in t["events"]))
    require_rejected(cohort, move_resume_early, "resume before cohort cleanup", args.tlc)
    require_rejected(cohort, wrong_selection, "wrong strategy selection", args.tlc)
    require_rejected(tree, remove_descendant_done, "parent join before descendant completion", args.tlc)
    require_rejected(tree, false_propagation, "propagation without cancellation", args.tlc)
    unsafe_final_event(cohort, args.tlc)
    for fault in ("Selection", "Cleanup", "Retirement", "Closed"):
        model_fault("MCCohortsAll", fault, args.tlc)
    # Needs a second generation within one round (MaxGen 3).
    model_fault("MCCohortsAllSupersession", "InPlaceRetry", args.tlc)
    model_fault("MCCohortsAll", "FirstStartRetry", args.tlc, invariant="Safety")
    # Rank 5f: v0.7 charged every rebuilt child of a round.
    named_fault("MCCohortsAll", "PerChildCharge", "RoundCharged", args.tlc)
    for label, pattern, replacement in SUPERVISION_S1_CONTROLS:
        spec_mutation("MCSupervision", "Supervision.tla", label, pattern, replacement,
                      "RestartFollowsPolicy", args.tlc)
    deferred = next(t for t in traces if t["name"] == "ready_exit_deferred")
    def decide_at_once(events):
        # v0.7: no deferral; the restart decision is taken during startup,
        # where the deferral was recorded (before SupStartDone).
        at = next(i for i, e in enumerate(events) if e["a"] == "DeferExit")
        child = events[at]["c"]
        undefer = next(i for i, e in enumerate(events) if e["a"] == "Undefer")
        decide = next(i for i, e in enumerate(events)
                      if i > undefer and e["a"] == "Decide" and e["c"] == child)
        events[at] = events.pop(decide)
        events.pop(undefer)
    require_rejected(deferred, decide_at_once,
                     "ready child's exit decided during the initial start", args.tlc)
    model_fault("MCCoordinator", "ReleaseEarly", args.tlc)
    model_fault("MCCoordinator", "ForgetFailure", args.tlc)
    model_fault("MCCoordinator", "ForgetSignal", args.tlc, invariant="JoinAfterAllSignalled")
    model_fault("MCCoordinator", "ForgetSignal", args.tlc, temporal=True,
                drop=("JoinAfterAllSignalled",))
    model_fault("MCCoordinator", "RollbackReady", args.tlc, invariant="OrderedShutdown")
    # v0.8 grace expiry: no kill (a stuck trapping root holds the join), or a
    # kill in the ordered loop (stopped() skipped before the grace expired).
    model_fault("MCCoordinator", "NoKillAtGrace", args.tlc, invariant="GraceKillsRemaining")
    model_fault("MCCoordinator", "KillEarly", args.tlc, invariant="KillOnlyAtGrace")
    model_fault("MCNested", "Join", args.tlc)
    # A Temporary child restarted after a panic (MCSupervision has no
    # Temporary child, so only MCSupervisionTemporary can catch this).
    source_fault("MCSupervisionTemporary", "Supervision.tla",
                 '\\/ m = "Permanent"\n', '\\/ m \\in {"Permanent", "Temporary"}\n',
                 "TemporaryStartsOnce", args.tlc)
    model_fault("MCNested", "Propagation", args.tlc, temporal=True)
    for module, fault, invariant in DEFERRED_CONTROLS:
        named_fault(module, fault, invariant, args.tlc)
    job_trace_controls(traces, args.tlc)
    reply_trace_controls(traces, args.tlc)
    lifecycle_trace_controls(traces, args.tlc)
    for module, fault, invariant in LIFECYCLE_CONTROLS:
        named_fault(module, fault, invariant, args.tlc)
    for fault, prop in LIFECYCLE_TEMPORAL_CONTROLS:
        named_temporal("MCLifecycleLive", fault, prop, args.tlc, LIFECYCLE_SMALL)


if __name__ == "__main__":
    main()
