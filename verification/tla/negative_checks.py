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
            shutil.copy(source, work)
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


def named_fault(module, fault, invariant, tlc, constants=None):
    """Run MODULE (safety only) with Fault = FAULT, checking TypeOK and the
    single named INVARIANT, and require that invariant to be violated. This
    shows the mutation breaks the property it targets, not just Safety.
    CONSTANTS overrides constant values (an instance where the fault can
    show)."""
    constants = constants or {}
    with tempfile.TemporaryDirectory(prefix="bad-model-") as directory:
        work = Path(directory)
        for source in HERE.glob("*.tla"):
            shutil.copy(source, work)
        lines = []
        for line in (HERE / f"{module}.cfg").read_text().splitlines():
            if line.startswith("PROPERTY"):
                continue
            if line.startswith("SPECIFICATION"):
                line = "SPECIFICATION Spec"
            if line.startswith("INVARIANT"):
                line = f"INVARIANT TypeOK\nINVARIANT {invariant}"
            name = line.strip().split(" ")[0]
            if name in constants:
                line = f"    {name} = {constants[name]}"
            lines.append(line.replace('Fault = "None"', f'Fault = "{fault}"'))
        (work / f"{module}.cfg").write_text("\n".join(lines) + "\n")
        result = subprocess.run(shlex.split(tlc) + ["-workers", "1", "-deadlock", "-cleanup", module],
                                cwd=work, capture_output=True, text=True)
        log = result.stdout + result.stderr
        if f"Invariant {invariant} is violated" not in log:
            raise AssertionError(f"{module}/{fault}: {invariant} not violated\n{log}")
    print(f"COUNTEREXAMPLE model: {module}/{fault} ({invariant})")


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
            shutil.copy(source, work)
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
]

# Liveness controls run on a smaller instance of MCLifecycleLive.
LIFECYCLE_TEMPORAL_CONTROLS = [
    ("ParentAsExit", "ShutdownEndsChild"),
    ("SplitDeathPoint", "DownDelivered"),
]
LIFECYCLE_SMALL = {"Reqs": "{}", "MaxSignals": "0", "DropEnabled": "FALSE"}

# Rank 12a design model (docs/design/dynamic-supervisor.md, revision 3):
# each mutation against the property it must break. Every invariant and
# liveness property of DynamicSupervisor.tla has at least one. Run one at a
# time (parallel TLC runs race on the standard modules TLC extracts to /tmp).
DYNAMIC_CONTROLS = [
    ("MCDynamicSupervisor", "IgnoreMax", "MaxChildrenBound"),
    ("MCDynamicSupervisor", "UncountedRestart", "MaxChildrenBound"),
    ("MCDynamicSupervisor", "RestartTemporary", "TemporaryNeverRestarted"),
    ("MCDynamicSupervisor", "RestartTerminated", "TerminatedNeverRestarted"),
    ("MCDynamicSupervisor", "AcceptDuringShutdown", "NoStartAfterShutdown"),
    ("MCDynamicSupervisorBackoff", "RestartAfterClose", "NoGenerationAfterClose"),
    ("MCDynamicSupervisor", "NoEscalate", "ExhaustionCloses"),
    ("MCDynamicSupervisor", "EarlyDone", "NoChildOutlivesSupervisor"),
    ("MCDynamicSupervisor", "SkipSignal", "ConcurrentShutdown"),
    ("MCDynamicSupervisor", "ConcurrentStart", "SerialConstruction", {"MaxChildren": "2"}),
    ("MCDynamicSupervisor", "LaterOp", "FifoOrder"),
    ("MCDynamicSupervisor", "StuckFailsAll", "StuckIsolated"),
    ("MCDynamicSupervisor", "MisclassifyStuck", "StuckIsolated"),
    ("MCDynamicSupervisor", "PublishAfterClose", "NoPublishAfterStop"),
    ("MCDynamicSupervisor", "PublishAfterExit", "PublishedIsLive"),
    ("MCDynamicSupervisor", "OverlapGeneration", "AtMostOneGeneration"),
    ("MCDynamicSupervisor", "PublishBeforeReady", "PublishedIsLive"),
    ("MCDynamicSupervisor", "PrematureOk", "OkMeansReady"),
    ("MCDynamicSupervisor", "UnchargedRetry", "ChargedRestarts"),
    ("MCDynamicSupervisor", "ChargeFirstStart", "ChargedRestarts"),
    ("MCDynamicSupervisor", "TransientNormalRestart", "RestartFollowsPolicy"),
    ("MCDynamicSupervisor", "TransientNormalRestart", "RestartFollowsPolicy", {"MaxRestarts": "0", "MaxExpire": "0"}),
    ("MCDynamicSupervisor", "PermanentRetires", "RestartFollowsPolicy"),
    ("MCDynamicSupervisor", "RetryFirstStart", "FirstStartNeverRetried"),
    ("MCDynamicSupervisor", "EarlyFailedReply", "FailedAfterCleanup"),
    ("MCDynamicSupervisor", "EarlyRemove", "FailedAfterCleanup"),
    ("MCDynamicSupervisor", "LateFailedOverwrite", "FailureCauseKept"),
    ("MCDynamicSupervisorBackoff", "NoRefund", "ReservationsMatch"),
    ("MCDynamicSupervisorBackoff", "ExpireReserved", "ReservationsMatch"),
    ("MCDynamicSupervisor", "OverReserve", "BudgetBounded"),
    ("MCDynamicSupervisor", "TermReplyEarly", "TermResultsSound"),
    ("MCDynamicSupervisor", "SlotLookup", "TermResultsSound"),
    ("MCDynamicSupervisor", "QueuedTermOk", "TermResultsSound"),
    ("MCDynamicSupervisor", "StuckResultLost", "TermResultsSound"),
    ("MCDynamicSupervisor", "IgnoreExpect", "TermResultsSound"),
]
DYNAMIC_TEMPORAL_CONTROLS = [
    # Without startup deadlines, so the deadline cannot mask the missing abort.
    ("MCDynamicSupervisorCallers", "NoAbort", "StartAnswered", {"StartDeadlines": "FALSE"}),
    ("MCDynamicSupervisorCallers", "NoDeadline", "StartAnswered"),
    ("MCDynamicSupervisorCallers", "TermLost", "TerminateAnswered"),
    ("MCDynamicSupervisorCallers", "Sequential", "FanoutCompletes"),
    ("MCDynamicSupervisorLive", "StuckBackoff", "ClosedLeadsToDone"),
    ("MCDynamicSupervisorLive", "LostRestart", "RecoveryProgress"),
]

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
    ("MCDeferredJobs", "NoInstanceCheck", "NoLateResult"),
    ("MCDeferredJobs", "NoCancelFence", "NoStaleAfterCancel"),
    ("MCDeferredJobs", "DoubleTerminal", "AtMostOneTerminal"),
    ("MCDeferredJobs", "ReleaseOnTimeout", "PermitsBounded"),
    ("MCDeferredJobs", "KeyNoCancel", "OneLivePerKey"),
    ("MCDeferredJobs", "ReplacePendingOnly", "NoStaleKeyed"),
    ("MCDeferredJobsEmits", "NoCancelFence", "NoStaleAfterCancel"),
    ("MCDeferredJobsEmits", "EmitAfterTerminal", "TerminalLast"),
    ("MCDeferredJobsEmits", "NoEmitCredit", "EmitCreditsBounded"),
]


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
    model_fault("MCCoordinator", "ReleaseEarly", args.tlc)
    model_fault("MCCoordinator", "ForgetFailure", args.tlc)
    model_fault("MCCoordinator", "ForgetSignal", args.tlc, invariant="JoinAfterAllSignalled")
    model_fault("MCCoordinator", "ForgetSignal", args.tlc, temporal=True,
                drop=("JoinAfterAllSignalled",))
    model_fault("MCCoordinator", "RollbackReady", args.tlc, invariant="OrderedShutdown")
    model_fault("MCNested", "Join", args.tlc)
    # A Temporary child restarted after a panic (MCSupervision has no
    # Temporary child, so only MCSupervisionTemporary can catch this).
    source_fault("MCSupervisionTemporary", "Supervision.tla",
                 '\\/ m = "Permanent"\n', '\\/ m \\in {"Permanent", "Temporary"}\n',
                 "TemporaryStartsOnce", args.tlc)
    model_fault("MCNested", "Propagation", args.tlc, temporal=True)
    for module, fault, invariant in DEFERRED_CONTROLS:
        named_fault(module, fault, invariant, args.tlc)
    for module, fault, invariant in LIFECYCLE_CONTROLS:
        named_fault(module, fault, invariant, args.tlc)
    for fault, prop in LIFECYCLE_TEMPORAL_CONTROLS:
        named_temporal("MCLifecycleLive", fault, prop, args.tlc, LIFECYCLE_SMALL)
    for module, fault, invariant, *constants in DYNAMIC_CONTROLS:
        named_fault(module, fault, invariant, args.tlc, *constants)
    for module, fault, prop, *constants in DYNAMIC_TEMPORAL_CONTROLS:
        named_temporal(module, fault, prop, args.tlc, *constants)


if __name__ == "__main__":
    main()
