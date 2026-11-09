#!/usr/bin/env python3
"""Summarize cargo-nextest JUnit reports from repeated runs.

Usage: flaky-report.py JUNIT.xml [JUNIT.xml ...]

A test is FLAKY if any run recorded a <flakyFailure> (failed, then passed on
retry) or if it passed in one run and failed in another. It is FAILED if it
failed every attempt in some run. Prints a Markdown summary (append it to
$GITHUB_STEP_SUMMARY in CI) and exits 1 when anything is flaky or failed.
"""
import sys
import xml.etree.ElementTree as ET
from collections import defaultdict


def outcomes(path):
    for case in ET.parse(path).getroot().iter("testcase"):
        name = f"{case.get('classname')}::{case.get('name')}"
        if case.find("failure") is not None or case.find("error") is not None:
            yield name, "failed"
        elif case.find("flakyFailure") is not None or case.find("flakyError") is not None:
            yield name, "flaky"
        elif case.find("skipped") is None:
            yield name, "passed"


def main(paths):
    if not paths:
        print(__doc__, file=sys.stderr)
        return 2
    seen = defaultdict(lambda: defaultdict(int))
    for path in paths:
        for name, outcome in outcomes(path):
            seen[name][outcome] += 1
    flaky = sorted(
        n for n, o in seen.items() if o["flaky"] or (o["failed"] and o["passed"])
    )
    failed = sorted(n for n, o in seen.items() if o["failed"] and not o["passed"])
    print(f"## Flaky-test tracking ({len(paths)} runs, {len(seen)} tests)\n")
    if not flaky and not failed:
        print("No flaky or failing tests.")
        return 0
    print("| Test | Status | Passed | Flaky (passed on retry) | Failed |")
    print("|---|---|---|---|---|")
    for name in flaky + failed:
        o = seen[name]
        status = "FLAKY" if name in flaky else "FAILED"
        print(f"| `{name}` | {status} | {o['passed']} | {o['flaky']} | {o['failed']} |")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
