"""Negative controls for the narrowly declared feature-split exception."""
import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("check_api", Path(__file__).resolve().parents[1] / "check-api.py")
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)


def diagnostic(item):
    return "--- failure module_missing: module removed ---\n\nFailed in:\n  " + item + ", previously in file baseline.rs:1\n\n"


class FeatureSplitAudit(unittest.TestCase):
    expected = [["module_missing", "mod spawned_rt::tasks"]]

    def audit(self, code, output):
        result = subprocess.CompletedProcess([], code, stdout=output)
        with patch.object(api.subprocess, "run", return_value=result):
            api.declared_break(["unused"], self.expected, Path.cwd())

    def test_only_the_declared_removal_is_accepted(self):
        self.audit(100, diagnostic("mod spawned_rt::tasks"))

    def test_another_removed_module_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "unexpected"):
            self.audit(100, diagnostic("mod spawned_rt::tasks") + diagnostic("mod spawned_rt::threads"))

    def test_a_tool_failure_is_not_accepted(self):
        with self.assertRaises(RuntimeError):
            self.audit(1, "compiler failed")

    def test_an_unreported_break_is_not_accepted(self):
        with self.assertRaises(RuntimeError):
            self.audit(100, "unknown diagnostic format")

    def test_diagnostic_format_changes_fail_closed(self):
        with self.assertRaises(ValueError):
            self.audit(100, "--- failure module_missing: bad format\n")


if __name__ == "__main__":
    unittest.main()
